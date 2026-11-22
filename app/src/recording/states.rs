//! Saved sessions (logins).
//!
//! A session is every cookie of the browser plus the localStorage/sessionStorage of each site open
//! in a tab. Saved under a name it can be loaded again later; exported to a JSON file it can be
//! imported into the browser on another machine, which is then signed in to the same sites
//! (Facebook, Gmail, GitHub…) without logging in again.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;

use crate::agent::{cmp_or, quote, Agent};
use crate::browser::{BResult, Browser, Tab, CMD_TIMEOUT};

// ---------- Saved sessions (logins) ----------
//
// A session is every cookie of the browser plus the localStorage/sessionStorage of each site open
// in a tab. Saved under a name it can be loaded again later; exported to a JSON file it can be
// imported into the browser on another machine, which is then signed in to the same sites
// (Facebook, Gmail, GitHub…) without logging in again.

/// `null` for any field reads as the zero value (encoding/json does the same).
fn nullable<'de, T, D>(d: D) -> Result<T, D::Error>
where
    T: Deserialize<'de> + Default,
    D: Deserializer<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// network.CookiePartitionKey
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct PartitionKey {
    pub top_level_site: String,
    pub has_cross_site_ancestor: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct StCookie {
    #[serde(deserialize_with = "nullable")]
    pub name: String,
    #[serde(deserialize_with = "nullable")]
    pub value: String,
    /// a leading dot = also its subdomains; none = that host only
    #[serde(deserialize_with = "nullable")]
    pub domain: String,
    #[serde(deserialize_with = "nullable")]
    pub path: String,
    #[serde(deserialize_with = "nullable")]
    pub expires: f64,
    #[serde(deserialize_with = "nullable")]
    pub session: bool,
    #[serde(deserialize_with = "nullable")]
    pub http_only: bool,
    #[serde(deserialize_with = "nullable")]
    pub secure: bool,
    #[serde(
        deserialize_with = "nullable",
        skip_serializing_if = "String::is_empty"
    )]
    pub same_site: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition_key: Option<PartitionKey>,
}

/// One storage area. It reads both {"key": "value"} and Playwright's [{"name", "value"}].
pub type KvMap = BTreeMap<String, String>;

fn kv<'de, D>(d: D) -> Result<KvMap, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct NameValue {
        #[serde(default)]
        name: String,
        #[serde(default)]
        value: String,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Obj(KvMap),
        Arr(Vec<NameValue>),
    }
    Ok(match Option::<Either>::deserialize(d)? {
        None => KvMap::new(),
        Some(Either::Obj(m)) => m,
        Some(Either::Arr(v)) => v.into_iter().map(|kv| (kv.name, kv.value)).collect(),
    })
}

/// The localStorage and sessionStorage of one site.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct SiteStorage {
    #[serde(deserialize_with = "nullable")]
    pub origin: String,
    #[serde(
        rename = "localStorage",
        deserialize_with = "kv",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub local: KvMap,
    #[serde(
        rename = "sessionStorage",
        deserialize_with = "kv",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub session: KvMap,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct SavedState {
    /// time.Time as RFC 3339 text, kept verbatim
    #[serde(deserialize_with = "nullable")]
    pub saved: String,
    #[serde(deserialize_with = "nullable")]
    pub url: String,
    #[serde(deserialize_with = "nullable")]
    pub cookies: Vec<StCookie>,
    // Storage of the page that was open when saving. Kept so older versions can still read the file.
    #[serde(deserialize_with = "nullable")]
    pub origin: String,
    #[serde(rename = "localStorage", deserialize_with = "kv")]
    pub local: KvMap,
    #[serde(rename = "sessionStorage", deserialize_with = "kv")]
    pub session: KvMap,
    // Storage of every site that was open in a tab.
    #[serde(deserialize_with = "nullable", skip_serializing_if = "Vec::is_empty")]
    pub origins: Vec<SiteStorage>,
}

impl SavedState {
    /// sites lists the storage to restore, one entry per site.
    pub fn sites(&self) -> Vec<SiteStorage> {
        let mut out: Vec<SiteStorage> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut add = |s: SiteStorage| {
            if !s.origin.starts_with("http")
                || seen.contains(&s.origin)
                || s.local.len() + s.session.len() == 0
            {
                return;
            }
            seen.insert(s.origin.clone());
            out.push(s);
        };
        for s in &self.origins {
            add(s.clone());
        }
        add(SiteStorage {
            origin: self.origin.clone(),
            local: self.local.clone(),
            session: self.session.clone(),
        });
        out
    }
}

/// The portable file produced by export and read by import: one or more saved states, so a whole
/// set of logins can be carried to another machine in a single JSON file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StateBundle {
    pub version: i64,
    pub app: String,
    pub exported: String,
    #[serde(deserialize_with = "nullable")]
    pub states: BTreeMap<String, SavedState>,
}

/// 2: every open site's storage (origins)
const BUNDLE_VERSION: i64 = 2;

/// nameRe: `^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$`
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=64).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

/// statePath: the file a saved state lives in.
fn state_path(dir: &str, name: &str) -> BResult<PathBuf> {
    if !valid_name(name) {
        return Err("name is required: letters, digits, - _ . (up to 64)".into());
    }
    Ok(Path::new(dir).join(format!("{name}.json")))
}

// ---------- Capture ----------

const DUMP_STORAGE_JS: &str = r#"JSON.stringify({origin: location.origin,
  local: Object.fromEntries(Object.keys(localStorage).map(k => [k, localStorage.getItem(k)])),
  session: Object.fromEntries(Object.keys(sessionStorage).map(k => [k, sessionStorage.getItem(k)]))})"#;

/// readStorage returns the storage of the page open in t.
async fn read_storage(t: &Tab) -> BResult<SiteStorage> {
    let raw = t
        .evaluate(DUMP_STORAGE_JS, Some(Duration::from_secs(5)))
        .await?;
    let raw = raw.as_str().unwrap_or_default();
    // The page script answers with its own keys (local, session), not the file's (localStorage, sessionStorage).
    #[derive(Deserialize)]
    struct Dump {
        #[serde(default)]
        origin: String,
        #[serde(default, deserialize_with = "kv")]
        local: KvMap,
        #[serde(default, deserialize_with = "kv")]
        session: KvMap,
    }
    let d: Dump = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    Ok(SiteStorage {
        origin: d.origin,
        local: d.local,
        session: d.session,
    })
}

/// openTabs returns the tabs with the URL each one shows.
fn open_tabs(b: &Browser) -> Vec<(Arc<Tab>, String)> {
    let tabs = b.st().tabs.clone();
    tabs.into_iter()
        .map(|t| {
            let u = t.st().url.clone();
            (t, u)
        })
        .collect()
}

/// originOf: scheme://host of an http(s) address, "" for anything else.
pub fn origin_of(raw: &str) -> String {
    let Ok(u) = url::Url::parse(raw) else {
        return String::new();
    };
    if u.scheme() != "http" && u.scheme() != "https" {
        return String::new();
    }
    let Some(host) = u.host_str().filter(|h| !h.is_empty()) else {
        return String::new();
    };
    match u.port() {
        Some(p) => format!("{}://{host}:{p}", u.scheme()),
        None => format!("{}://{host}", u.scheme()),
    }
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

// ---------- Restore ----------

/// sameSiteOf: the CDP SameSite value for the saved text ("" when unknown).
fn same_site_of(s: &str) -> &'static str {
    match s.to_lowercase().as_str() {
        "strict" => "Strict",
        "lax" => "Lax",
        "none" | "no_restriction" => "None",
        _ => "",
    }
}

/// network.CookieParam, the parts Storage.setCookies reads.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CookieParam {
    pub name: String,
    pub value: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub domain: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(skip_serializing_if = "is_false")]
    pub secure: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub http_only: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub same_site: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition_key: Option<PartitionKey>,
}

fn cookie_param(c: &StCookie) -> CookieParam {
    let path = cmp_or(&c.path, "/");
    let mut p = CookieParam {
        name: c.name.clone(),
        value: c.value.clone(),
        path: path.clone(),
        secure: c.secure,
        http_only: c.http_only,
        same_site: same_site_of(&c.same_site).to_string(),
        partition_key: c.partition_key.clone(),
        ..Default::default()
    };
    if c.domain.starts_with('.') {
        p.domain = c.domain.clone();
    } else {
        // host-only: set it through its URL, or Chrome would widen it to the subdomains (and refuse __Host- cookies)
        let scheme = if c.secure { "https" } else { "http" };
        p.url = format!("{scheme}://{}{path}", c.domain);
    }
    if !c.session && c.expires > 0.0 {
        p.expires = Some(c.expires);
    }
    p
}

/// Storage.setCookies for one batch.
async fn set_cookie_batch(b: &Browser, ps: &[CookieParam]) -> BResult<()> {
    b.root_call(
        "Storage.setCookies",
        json!({ "cookies": ps }),
        Some(CMD_TIMEOUT),
    )
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// writeStorage fills the storage of the page open in t, which must be on site s.
async fn write_storage(t: &Tab, s: &SiteStorage, with_session: bool) -> BResult<()> {
    let l = serde_json::to_string(&s.local).unwrap_or_else(|_| "{}".into());
    let ss = if with_session {
        serde_json::to_string(&s.session).unwrap_or_else(|_| "{}".into())
    } else {
        "null".to_string()
    };
    let o = serde_json::to_string(&s.origin).unwrap_or_default();
    let expr = format!(
        r#"(() => {{
  if (location.origin !== {o}) throw new Error("wrong site: " + location.origin);
  for (const [k, v] of Object.entries({l} || {{}})) localStorage.setItem(k, v);
  for (const [k, v] of Object.entries({ss} || {{}})) sessionStorage.setItem(k, v);
  return true;
}})()"#
    );
    t.evaluate(&expr, Some(Duration::from_secs(10)))
        .await
        .map(|_| ())
}

/// openOrigin loads a light page of the site in t (its robots.txt), just so its storage can be written.
async fn open_origin(t: &Tab, origin: &str) -> BResult<()> {
    for p in ["/robots.txt", "/"] {
        let target = format!("{origin}{p}");
        let nav = t
            .call(
                "Page.navigate",
                json!({ "url": target }),
                Some(Duration::from_secs(20)),
            )
            .await;
        let navigated = matches!(&nav, Ok(v) if v["errorText"].as_str().unwrap_or("").is_empty());
        if navigated {
            // Wait for the page to load, then ask where it is.
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let got = t
                    .evaluate(
                        "document.readyState === 'complete' ? location.origin : ''",
                        Some(Duration::from_secs(5)),
                    )
                    .await
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                if got == origin {
                    return Ok(());
                }
                if got.is_empty() && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                break;
            }
        }
    }
    Err(format!("could not open {origin}"))
}

/// site_names lists the sites the cookies belong to (google.com for mail.google.com), sorted.
/// Nil (None) when there are none, like `slices.Sorted` of an empty set.
pub fn site_names(cookies: &[StCookie]) -> Option<Vec<String>> {
    let mut set = BTreeSet::new();
    for c in cookies {
        let domain = c.domain.strip_prefix('.').unwrap_or(&c.domain);
        let parts: Vec<&str> = domain.split('.').collect();
        let mut n = 2;
        if parts.len() >= 3 && parts[parts.len() - 2].len() <= 3 {
            // facebook.com.vn, bbc.co.uk
            n = 3;
        }
        let parts = if parts.len() > n {
            &parts[parts.len() - n..]
        } else {
            &parts[..]
        };
        let h = parts.join(".");
        if !h.is_empty() {
            set.insert(h);
        }
    }
    if set.is_empty() {
        None
    } else {
        Some(set.into_iter().collect())
    }
}

/// mergeStates folds several sessions into one, later ones winning.
pub fn merge_states(states: &BTreeMap<String, SavedState>) -> SavedState {
    let mut out = SavedState::default();
    for st in states.values() {
        out.cookies.extend(st.cookies.iter().cloned());
        out.origins.extend(st.sites());
    }
    out
}

// ---------- Files ----------

/// stateInfo: one row of the list (API and the list action).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StateInfo {
    pub name: String,
    pub cookies: usize,
    pub sites: Option<Vec<String>>,
    pub origin: String,
    pub saved: String,
}

/// stateNames: the saved states on disk, sorted.
fn state_names(dir: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(ents) = std::fs::read_dir(dir) {
        for e in ents.flatten() {
            let file = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if let Some(n) = file.strip_suffix(".json") {
                if !is_dir && valid_name(n) {
                    out.push(n.to_string());
                }
            }
        }
    }
    out.sort();
    out
}

/// listStates returns metadata for every saved state, for the settings UI.
pub fn list_states(dir: &str) -> Vec<StateInfo> {
    let mut out = Vec::new();
    for n in state_names(dir) {
        if let Ok(st) = read_state(dir, &n) {
            out.push(StateInfo {
                name: n,
                cookies: st.cookies.len(),
                sites: site_names(&st.cookies),
                origin: st.origin.clone(),
                saved: st.saved.clone(),
            });
        }
    }
    out
}

/// readState reads and parses one saved state file by name.
pub fn read_state(dir: &str, name: &str) -> BResult<SavedState> {
    let p = state_path(dir, name)?;
    let data = std::fs::read(&p).map_err(|_| {
        format!(
            "no saved state called {} (use action list)",
            quote_raw(name)
        )
    })?;
    serde_json::from_slice(&data).map_err(|e| format!("state {} is damaged: {e}", quote_raw(name)))
}

/// writeState writes one saved state file (0600: it holds logins).
pub fn write_state(dir: &str, name: &str, st: &SavedState) -> BResult<()> {
    let p = state_path(dir, name)?;
    let data = serde_json::to_vec_pretty(st).map_err(|e| e.to_string())?;
    write_private(&p, &data).map_err(|e| e.to_string())
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)?.write_all(data)
}

/// `%q` for a name that already passed the name check (no escapes needed).
fn quote_raw(s: &str) -> String {
    format!("\"{s}\"")
}

fn new_bundle() -> StateBundle {
    StateBundle {
        version: BUNDLE_VERSION,
        app: crate::APP_NAME.to_string(),
        exported: rfc3339(SystemTime::now()),
        states: BTreeMap::new(),
    }
}

/// buildBundle gathers one saved state (name given) or every saved state (name empty) into a bundle.
pub fn build_bundle(dir: &str, name: &str) -> BResult<StateBundle> {
    let mut bundle = new_bundle();
    if !name.is_empty() {
        let st = read_state(dir, name)?;
        bundle.states.insert(name.to_string(), st);
        return Ok(bundle);
    }
    for n in state_names(dir) {
        if let Ok(st) = read_state(dir, &n) {
            bundle.states.insert(n, st);
        }
    }
    if bundle.states.is_empty() {
        return Err("no saved states to export (save one first)".into());
    }
    Ok(bundle)
}

/// extCookie is one cookie as browser extensions export them (Cookie-Editor, EditThisCookie).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ExtCookie {
    #[serde(deserialize_with = "nullable")]
    name: String,
    #[serde(deserialize_with = "nullable")]
    value: String,
    #[serde(deserialize_with = "nullable")]
    domain: String,
    #[serde(deserialize_with = "nullable")]
    path: String,
    #[serde(deserialize_with = "nullable")]
    expiration_date: f64,
    #[serde(deserialize_with = "nullable")]
    expires: f64,
    #[serde(deserialize_with = "nullable")]
    host_only: bool,
    #[serde(deserialize_with = "nullable")]
    http_only: bool,
    #[serde(deserialize_with = "nullable")]
    secure: bool,
    #[serde(deserialize_with = "nullable")]
    session: bool,
    #[serde(deserialize_with = "nullable")]
    same_site: String,
}

impl ExtCookie {
    fn state(&self) -> StCookie {
        let domain = if self.host_only {
            self.domain
                .strip_prefix('.')
                .unwrap_or(&self.domain)
                .to_string()
        } else {
            self.domain.clone()
        };
        let exp = if self.expiration_date == 0.0 {
            self.expires
        } else {
            self.expiration_date
        };
        StCookie {
            name: self.name.clone(),
            value: self.value.clone(),
            domain,
            path: cmp_or(&self.path, "/"),
            expires: exp,
            session: self.session || exp <= 0.0,
            http_only: self.http_only,
            secure: self.secure,
            same_site: self.same_site.clone(),
            partition_key: None,
        }
    }
}

/// parseSessions reads an import file: a bundle exported here (or a single saved state), the cookie
/// list of the Cookie-Editor / EditThisCookie extensions, or a Playwright storageState, so logins can
/// also come from an ordinary Chrome, Edge or Firefox.
pub fn parse_sessions(data: &[u8], fallback_name: &str) -> BResult<BTreeMap<String, SavedState>> {
    let name = if valid_name(fallback_name) {
        fallback_name
    } else {
        "imported"
    };
    let mut data = data.strip_prefix("\u{feff}".as_bytes()).unwrap_or(data);
    while let Some((first, rest)) = data.split_first() {
        if !first.is_ascii_whitespace() {
            break;
        }
        data = rest;
    }
    while let Some((last, rest)) = data.split_last() {
        if !last.is_ascii_whitespace() {
            break;
        }
        data = rest;
    }
    let bad = "not a sessions file: expected a file exported here, a Cookie-Editor cookie list or a Playwright storageState";
    if data.first() == Some(&b'[') {
        let list: Vec<ExtCookie> = serde_json::from_slice(data).map_err(|_| bad.to_string())?;
        let cookies: Vec<StCookie> = list
            .iter()
            .filter(|c| !c.name.is_empty() && !c.domain.is_empty())
            .map(ExtCookie::state)
            .collect();
        if cookies.is_empty() {
            return Err("the file holds no cookies".into());
        }
        let st = SavedState {
            saved: rfc3339(SystemTime::now()),
            cookies,
            ..Default::default()
        };
        return Ok(BTreeMap::from([(name.to_string(), st)]));
    }
    if let Ok(bundle) = serde_json::from_slice::<StateBundle>(data) {
        if !bundle.states.is_empty() {
            return Ok(bundle.states);
        }
    }
    if let Ok(one) = serde_json::from_slice::<SavedState>(data) {
        if !one.cookies.is_empty() || !one.sites().is_empty() {
            return Ok(BTreeMap::from([(name.to_string(), one)]));
        }
    }
    Err(bad.into())
}

/// saveBundle parses an import file and writes every usable state into the states folder,
/// returning the names it saved.
pub fn save_bundle(dir: &str, data: &[u8], fallback_name: &str) -> BResult<Vec<String>> {
    let states = parse_sessions(data, fallback_name)?;
    let mut names = Vec::new();
    for (name, st) in &states {
        if valid_name(name) && write_state(dir, name, st).is_ok() {
            names.push(name.clone());
        }
    }
    if names.is_empty() {
        return Err("nothing imported: the file held no usable states".into());
    }
    names.sort();
    Ok(names)
}

// ---------- Time (time.Now formats) ----------

/// days_from_civil: days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// RFC 3339 with nanoseconds and the local offset, as time.Time marshals it.
fn rfc3339(now: SystemTime) -> String {
    let d = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (y, mo, da, h, mi, s) = crate::chat::local_tm(secs);
    let local = days_from_civil(y as i64, mo as i64, da as i64) * 86400
        + h as i64 * 3600
        + mi as i64 * 60
        + s as i64;
    let off = local - secs;
    let mut out = format!("{y:04}-{mo:02}-{da:02}T{h:02}:{mi:02}:{s:02}");
    if d.subsec_nanos() > 0 {
        out.push('.');
        out.push_str(format!("{:09}", d.subsec_nanos()).trim_end_matches('0'));
    }
    if off == 0 {
        out.push('Z');
    } else {
        let a = off.abs();
        out.push_str(&format!(
            "{}{:02}:{:02}",
            if off < 0 { '-' } else { '+' },
            a / 3600,
            (a % 3600) / 60
        ));
    }
    out
}

/// Local time as Format("20060102-150405") (or "20060102-1504" without seconds).
fn stamp(seconds: bool) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (y, mo, d, h, mi, s) = crate::chat::local_tm(secs);
    if seconds {
        format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
    } else {
        format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}")
    }
}

/// The saved time as the list shows it, "2006-01-02 15:04" (prints in the time's own zone).
fn saved_label(saved: &str) -> String {
    if saved.is_empty() {
        return "0001-01-01 00:00".into();
    }
    match saved.get(..16) {
        Some(p) => p.replace('T', " "),
        None => saved.to_string(),
    }
}

// ---------- Agent: capture, restore, the browser_state tool ----------

impl Agent {
    /// The shared browser's cookies (all sites).
    async fn login_cookies(&self) -> BResult<Vec<StCookie>> {
        let r = self
            .b
            .root_call("Storage.getCookies", json!({}), None)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::from_value(r["cookies"].clone()).unwrap_or_default())
    }

    /// captureState reads the whole login state of the running browser: every cookie (all sites) and
    /// the storage of each site open in a tab.
    pub async fn capture_state(&self) -> BResult<SavedState> {
        let cookies = self.login_cookies().await?;
        let mut st = SavedState {
            saved: rfc3339(SystemTime::now()),
            url: self.page_url(),
            ..Default::default()
        };
        let now = unix_now();
        for c in cookies {
            if !c.session && c.expires > 0.0 && c.expires < now {
                continue;
            }
            st.cookies.push(c);
        }
        let mut seen: HashMap<String, usize> = HashMap::new();
        for (t, u) in open_tabs(&self.b) {
            if origin_of(&u).is_empty() {
                continue;
            }
            let Ok(s) = read_storage(&t).await else {
                continue;
            };
            if origin_of(&s.origin).is_empty() {
                continue;
            }
            if let Some(&j) = seen.get(&s.origin) {
                // the same site in two tabs: keep both
                let prev = &mut st.origins[j];
                prev.local.extend(s.local);
                prev.session.extend(s.session);
                continue;
            }
            seen.insert(s.origin.clone(), st.origins.len());
            st.origins.push(s);
        }
        if let Some(&j) = seen.get(&origin_of(&st.url)) {
            let cur = st.origins[j].clone();
            st.origin = cur.origin;
            st.local = cur.local;
            st.session = cur.session;
        }
        Ok(st)
    }

    /// setCookies puts the cookies into the browser. It returns how many were set and how many Chrome refused.
    pub async fn set_cookies(&self, cookies: &[StCookie]) -> (usize, usize) {
        let now = unix_now();
        let mut params: Vec<CookieParam> = Vec::new();
        for c in cookies {
            if c.name.is_empty()
                || c.domain.is_empty()
                || (!c.session && c.expires > 0.0 && c.expires < now)
            {
                continue;
            }
            params.push(cookie_param(c));
        }
        if params.is_empty() {
            return (0, 0);
        }
        if set_cookie_batch(&self.b, &params).await.is_err() {
            // Chrome refuses the whole batch when one cookie is malformed: set them one by one instead.
            for p in &params {
                let _ = set_cookie_batch(&self.b, std::slice::from_ref(p)).await;
            }
        }
        // Chrome also drops some cookies silently (SameSite=None without Secure…): count what it kept.
        let Ok(have) = self.login_cookies().await else {
            return (params.len(), 0);
        };
        let kept: HashSet<String> = have
            .iter()
            .map(|c| format!("{}|{}|{}", c.name, c.domain, c.path))
            .collect();
        let mut ok = 0;
        for p in &params {
            let domain = if p.domain.is_empty() {
                url::Url::parse(&p.url)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_string))
                    .unwrap_or_default()
            } else {
                p.domain.clone()
            };
            if kept.contains(&format!("{}|{}|{}", p.name, domain, p.path)) {
                ok += 1;
            }
        }
        (ok, params.len() - ok)
    }

    /// writeSites restores the storage of each site. A site open in a tab is written in place; the
    /// others are opened one after another in a temporary background tab (sessionStorage belongs to a
    /// tab, so only localStorage survives for those).
    async fn write_sites(&self, sites: &[SiteStorage]) -> (Vec<String>, Vec<String>) {
        let mut done = Vec::new();
        let mut missed = Vec::new();
        if sites.is_empty() {
            return (done, missed);
        }
        let mut open: HashMap<String, Arc<Tab>> = HashMap::new();
        for (t, u) in open_tabs(&self.b) {
            let o = origin_of(&u);
            if !o.is_empty() && !open.contains_key(&o) {
                open.insert(o, t);
            }
        }
        let mut rest: Vec<SiteStorage> = Vec::new();
        for s in sites {
            let written = match open.get(&s.origin) {
                Some(t) => write_storage(t, s, true).await.is_ok(),
                None => false,
            };
            if written {
                done.push(s.origin.clone());
            } else if !s.local.is_empty() {
                rest.push(s.clone());
            }
        }
        if rest.is_empty() {
            return (done, missed);
        }
        let tmp = self
            .b
            .call(|b| async move {
                let full = b.st().tabs.len() as i64 >= b.opt.max_tabs;
                if full {
                    return Err(format!("tab limit ({}) reached", b.opt.max_tabs));
                }
                b.create_tab().await
            })
            .await;
        let tmp = match tmp {
            Ok(t) => t,
            Err(_) => {
                missed.extend(rest.iter().map(|s| s.origin.clone()));
                return (done, missed);
            }
        };
        for s in &rest {
            if open_origin(&tmp, &s.origin).await.is_ok()
                && write_storage(&tmp, s, false).await.is_ok()
            {
                done.push(s.origin.clone());
            } else {
                missed.push(s.origin.clone());
            }
        }
        let id = tmp.id.clone();
        self.b.close_tab(&id).await;
        (done, missed)
    }

    /// applyState puts a saved session into the running browser: its cookies, then each site's storage.
    /// The caller reloads the page so it picks up the login.
    pub async fn apply_state(&self, st: &SavedState) -> BResult<String> {
        let (set, refused) = self.set_cookies(&st.cookies).await;
        let sites = st.sites();
        if set == 0 && sites.is_empty() {
            if refused > 0 {
                return Err(format!("the browser refused all {refused} cookies"));
            }
            return Err("the session holds no cookies or storage".into());
        }
        let (done, missed) = self.write_sites(&sites).await;
        let names = site_names(&st.cookies).map_or(0, |v| v.len());
        let mut msg = format!(
            "{set} cookies on {names} sites, storage of {} sites",
            done.len()
        );
        if refused > 0 {
            msg.push_str(&format!(" ({refused} cookies refused by the browser)"));
        }
        if !missed.is_empty() {
            msg.push_str(&format!(
                " (could not restore the storage of {})",
                missed.join(", ")
            ));
        }
        Ok(msg)
    }

    /// reloadVisible reloads the tab people are looking at, so a just-imported login shows up.
    fn reload_visible(&self) {
        if !self.page_url().starts_with("http") {
            return;
        }
        self.b.enqueue(|b| async move {
            let t = b.st().active.clone();
            let Some(t) = t else { return };
            let _ = t.call("Page.reload", json!({}), Some(CMD_TIMEOUT)).await;
        });
    }

    /// liveBundle captures the logins of the running browser right now into a bundle.
    pub async fn live_bundle(&self) -> BResult<StateBundle> {
        let st = self.capture_state().await?;
        if st.cookies.is_empty() && st.origins.is_empty() {
            return Err("the browser has no logins to export yet: sign in to a site first".into());
        }
        let mut bundle = new_bundle();
        bundle
            .states
            .insert(format!("browser-{}", stamp(false)), st);
        Ok(bundle)
    }

    /// importAndApply saves every session of an import file and loads them all into the running
    /// browser, which is then signed in without logging in again. The outer error means nothing was
    /// saved; the inner one means it was saved but the browser did not take it.
    pub async fn import_and_apply(
        &self,
        data: &[u8],
        fallback: &str,
    ) -> BResult<(Vec<String>, BResult<String>)> {
        let dir = self.state_dir();
        let names = save_bundle(&dir, data, fallback)?;
        let mut states = BTreeMap::new();
        for n in &names {
            if let Ok(st) = read_state(&dir, n) {
                states.insert(n.clone(), st);
            }
        }
        match self.apply_state(&merge_states(&states)).await {
            Err(e) => Ok((names, Err(e))),
            Ok(msg) => {
                self.reload_visible();
                Ok((names, Ok(msg)))
            }
        }
    }

    fn state_dir(&self) -> String {
        self.b.opt.state_dir.clone()
    }

    /// exportStates bundles one saved state (name given) or every saved state (name empty) into a single
    /// JSON file that can be copied to another machine and read back with import. If path is empty the
    /// file is written into the downloads folder.
    pub fn export_states(&self, name: &str, path: &str) -> BResult<String> {
        let bundle = build_bundle(&self.state_dir(), name)?;
        let path = if path.is_empty() {
            let fname = format!("states-export-{}.json", stamp(true));
            Path::new(&cmp_or(&self.b.opt.download_dir, &self.state_dir())).join(fname)
        } else if !path.ends_with(".json") {
            PathBuf::from(format!("{path}.json"))
        } else {
            PathBuf::from(path)
        };
        let data = serde_json::to_vec_pretty(&bundle).map_err(|e| e.to_string())?;
        write_private(&path, &data).map_err(|e| e.to_string())?;
        let abs = std::path::absolute(&path).unwrap_or(path);
        self.announce(
            "📤",
            &format!("Export {} session(s)", bundle.states.len()),
            None,
        );
        Ok(format!(
            "Exported {} session(s) to {}. Copy this file to the other machine and run action import with its path there; the secrets inside let anyone log in as you, so keep it safe and delete it when done.",
            bundle.states.len(),
            abs.display()
        ))
    }

    /// importStates reads a bundle written by export and saves every state inside it into the states
    /// folder, ready to be restored with action load.
    pub fn import_states(&self, path: &str) -> BResult<String> {
        if path.is_empty() {
            return Err("path is required: the JSON file produced by action export".into());
        }
        let data = std::fs::read(path).map_err(|e| format!("cannot read {path:?}: {e}"))?;
        let base = Path::new(path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let fallback = base.strip_suffix(".json").unwrap_or(&base).to_string();
        let names = save_bundle(&self.state_dir(), &data, &fallback)?;
        self.announce("📥", &format!("Import {} session(s)", names.len()), None);
        Ok(format!(
            "Imported {} session(s): {}. Load one with action load and its name to sign in without logging in again.",
            names.len(),
            names.join(", ")
        ))
    }

    /// State saves or restores the whole login state (cookies + storage of the open sites) under a name.
    /// The browser_state tool.
    pub async fn state_tool(&self, action: &str, name: &str, path: &str) -> BResult<String> {
        match action {
            "" | "list" => {
                let infos = list_states(&self.state_dir());
                let mut s = String::from("### Saved states\n");
                for st in &infos {
                    s.push_str(&format!(
                        "- {}: {} cookies on {} sites, saved {}\n",
                        st.name,
                        st.cookies,
                        st.sites.as_ref().map_or(0, Vec::len),
                        saved_label(&st.saved)
                    ));
                }
                if infos.is_empty() {
                    s.push_str("None. Save one with action save and a name.\n");
                }
                return Ok(s);
            }
            "export" => return self.export_states(name, path),
            "import" => return self.import_states(path),
            _ => {}
        }
        let dir = self.state_dir();
        // checks the name before the action: a bad name is reported first.
        state_path(&dir, name)?;
        match action {
            "save" => {
                let st = self.capture_state().await?;
                write_state(&dir, name, &st)?;
                self.announce("💾", &format!("Save session {}", quote(name)), None);
                Ok(format!(
                    "Saved state {}: {} cookies on {} sites, storage of {} open sites.",
                    quote_raw(name),
                    st.cookies.len(),
                    site_names(&st.cookies).map_or(0, |v| v.len()),
                    st.origins.len()
                ))
            }
            "load" => {
                let st = read_state(&dir, name)?;
                self.hub.agent_take()?;
                self.announce("💾", &format!("Load session {}", quote(name)), None);
                let msg = self.apply_state(&st).await?;
                if self.page_url().starts_with("http") {
                    self.reload().await?;
                }
                Ok(format!("Loaded state {}: {msg}.", quote_raw(name)))
            }
            "delete" => {
                let p = state_path(&dir, name)?;
                std::fs::remove_file(p)
                    .map_err(|_| format!("no saved state called {}", quote_raw(name)))?;
                Ok(format!("Deleted state {name}."))
            }
            _ => Err(format!(
                "action must be save, load, list, delete, export or import, not {}",
                quote_raw(action)
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    //! The file helpers, no browser needed.
    use super::*;

    /// A fresh folder for one test (t.TempDir).
    fn temp_dir(tag: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("abu-states-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    fn cookie(name: &str, value: &str, domain: &str) -> StCookie {
        StCookie {
            name: name.into(),
            value: value.into(),
            domain: domain.into(),
            path: "/".into(),
            ..Default::default()
        }
    }

    #[test]
    fn export_import_round_trip() {
        let src = temp_dir("roundtrip-src");
        write_state(
            &src,
            "github",
            &SavedState {
                origin: "https://github.com".into(),
                cookies: vec![cookie("user_session", "abc", "github.com")],
                local: KvMap::from([("theme".into(), "dark".into())]),
                ..Default::default()
            },
        )
        .unwrap();
        write_state(
            &src,
            "gmail",
            &SavedState {
                origin: "https://mail.google.com".into(),
                cookies: vec![cookie("SID", "xyz", ".google.com")],
                ..Default::default()
            },
        )
        .unwrap();

        // Build a bundle of everything (what the export endpoint streams).
        let b = build_bundle(&src, "").unwrap();
        assert_eq!(b.states.len(), 2, "want 2 states in bundle");
        let raw = serde_json::to_vec_pretty(&b).unwrap();

        // Import the bundle into a fresh (empty) folder: simulating another machine.
        let dst = temp_dir("roundtrip-dst");
        let names = save_bundle(&dst, &raw, "fallback").unwrap();
        assert_eq!(names.len(), 2, "want 2 imported: {names:?}");
        let got = read_state(&dst, "github").unwrap();
        assert_eq!(got.origin, "https://github.com");
        assert_eq!(got.cookies.len(), 1);
        assert_eq!(got.cookies[0].value, "abc");
        assert_eq!(got.local.get("theme").map(String::as_str), Some("dark"));
        assert_eq!(list_states(&dst).len(), 2);
    }

    #[test]
    fn export_single_and_bare_import() {
        let dir = temp_dir("single");
        write_state(
            &dir,
            "gmail",
            &SavedState {
                origin: "https://mail.google.com".into(),
                cookies: vec![cookie("SID", "x", "")],
                ..Default::default()
            },
        )
        .unwrap();

        let one = build_bundle(&dir, "gmail").unwrap();
        assert_eq!(one.states.len(), 1, "single export should hold 1 state");

        // A bare single SavedState file (not a bundle) imports under its fallback name.
        let data = serde_json::to_vec(&SavedState {
            cookies: vec![cookie("k", "v", "")],
            ..Default::default()
        })
        .unwrap();
        let names = save_bundle(&dir, &data, "legacy").unwrap();
        assert_eq!(names, vec!["legacy".to_string()]);
    }

    #[test]
    fn import_rejects_junk() {
        let dir = temp_dir("junk");
        assert!(save_bundle(&dir, b"not json", "x").is_err());
    }

    #[test]
    fn import_cookie_editor_list() {
        let dir = temp_dir("cookie-editor");
        let data = br#"[
	  {"domain": ".facebook.com", "expirationDate": 4102444800, "hostOnly": false, "httpOnly": true, "name": "xs", "path": "/", "sameSite": "no_restriction", "secure": true, "session": false, "value": "abc"},
	  {"domain": "mail.google.com", "hostOnly": true, "httpOnly": false, "name": "GMAIL_AT", "path": "/mail", "sameSite": null, "secure": true, "session": true, "value": "q"}
	]"#;
        let names = save_bundle(&dir, data, "cookies").unwrap();
        assert_eq!(names, vec!["cookies".to_string()]);
        let st = read_state(&dir, "cookies").unwrap();
        assert_eq!(st.cookies.len(), 2);
        assert_eq!(st.cookies[0].expires, 4102444800.0);
        assert!(st.cookies[1].session);

        let p = cookie_param(&st.cookies[0]);
        assert_eq!(p.domain, ".facebook.com");
        assert_eq!(p.same_site, "None");
        assert!(p.url.is_empty());

        // host-only cookies must be set by URL
        let p = cookie_param(&st.cookies[1]);
        assert!(p.domain.is_empty());
        assert_eq!(p.url, "https://mail.google.com/mail");

        assert_eq!(
            site_names(&st.cookies),
            Some(vec!["facebook.com".to_string(), "google.com".to_string()])
        );
    }

    #[test]
    fn import_playwright_storage_state() {
        let dir = temp_dir("playwright");
        let data = br#"{"cookies": [{"name": "sid", "value": "1", "domain": "example.com", "path": "/", "expires": -1, "httpOnly": true, "secure": true, "sameSite": "Lax"}],
	  "origins": [{"origin": "https://example.com", "localStorage": [{"name": "token", "value": "t1"}]}]}"#;
        save_bundle(&dir, data, "pw").unwrap();
        let st = read_state(&dir, "pw").unwrap();
        let sites = st.sites();
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].origin, "https://example.com");
        assert_eq!(sites[0].local.get("token").map(String::as_str), Some("t1"));
        // expires -1 must be a session cookie
        assert_eq!(cookie_param(&st.cookies[0]).expires, None);
    }

    #[test]
    fn sites_merges_legacy_and_origins() {
        let st = SavedState {
            origin: "https://a.com".into(),
            local: KvMap::from([("k".into(), "v".into())]),
            origins: vec![
                SiteStorage {
                    origin: "https://a.com".into(),
                    local: KvMap::from([("k".into(), "v".into())]),
                    ..Default::default()
                },
                SiteStorage {
                    origin: "https://b.com".into(),
                    local: KvMap::from([("x".into(), "y".into())]),
                    ..Default::default()
                },
                SiteStorage {
                    origin: "https://empty.com".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(st.sites().len(), 2, "deduped, empty dropped");
        let merged = merge_states(&BTreeMap::from([
            ("one".to_string(), st),
            (
                "two".to_string(),
                SavedState {
                    cookies: vec![cookie("c", "", "b.com")],
                    ..Default::default()
                },
            ),
        ]));
        assert_eq!(merged.cookies.len(), 1);
        assert_eq!(merged.sites().len(), 2);
    }

    #[test]
    fn state_names_follow_the_rule() {
        assert!(valid_name("github"));
        assert!(valid_name("a.b-c_9"));
        assert!(!valid_name(""));
        assert!(!valid_name("-x"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name(&"a".repeat(65)));
    }
}
