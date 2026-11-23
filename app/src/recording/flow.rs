//! Recorded flows: a person presses Record in the viewer and uses the browser as usual. Every
//! click, piece of typed text, choice in a list, key and page change becomes a step. Stopping saves
//! the flow on disk under a short id (e.g. flow-3fa9c1). The person gives that id to a coding agent,
//! which reads the steps with browser_get_flow and replays them with browser_run_flow, or follows
//! them itself with changes (other data, a loop over items…).
//!
//! What a person does is caught inside the page by agent.js (recStart), which reports each action
//! through the __abuRec binding while a recording is on. Navigation from the viewer's toolbar
//! (address bar, back, tabs…) is recorded by the hub, where those commands arrive.
//!
//! The live recording is kept in the `REC` static, always
//! locked *after* the hub state (every function that touches it is called with the hub state held,
//! or takes it first), the lock order of main.rs. The hub and the browser are reached through
//! `attach`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::json;

use crate::agent::{cmp_or, eval_js_as, go_quote, parse_key, quote, Agent};
use crate::browser::{normalize_url, BResult, Browser, BrowserHooks, Tab, TabInfo, View};
use crate::{go_json, Client, Hub, HubState, Msg, OutMsg};

const REC_BINDING: &str = "__abuRec";
pub const MAX_FLOW_STEPS: usize = 500;
const MAX_REPEAT: i64 = 50;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `json.Unmarshal` leaves a field alone for `null`: null becomes the default here too.
fn null_default<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(
    d: D,
) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// A float the way encoding/json prints it: `100`, not `100.0`.
fn ser_num<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        s.serialize_i64(*v as i64)
    } else {
        s.serialize_f64(*v)
    }
}

fn is_zero_f64(v: &f64) -> bool {
    *v == 0.0
}

fn is_zero_usize(v: &i64) -> bool {
    *v == 0
}

/// FlowTarget describes an element so it can be found again: role + name (as in browser_snapshot),
/// then test id, id, CSS path, text, and the point on the screen as a last resort.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FlowTarget {
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub tag: String,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub role: String,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub name: String,
    /// which of the elements with the same role and name (0 = first)
    #[serde(
        default,
        skip_serializing_if = "is_zero_usize",
        deserialize_with = "null_default"
    )]
    pub nth: i64,
    /// how many had that role and name
    #[serde(
        default,
        skip_serializing_if = "is_zero_usize",
        deserialize_with = "null_default"
    )]
    pub count: i64,
    #[serde(
        default,
        rename = "testAttr",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub test_attr: String,
    #[serde(
        default,
        rename = "testId",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub test_id: String,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub id: String,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub sel: String,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub text: String,
    #[serde(
        default,
        rename = "type",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub type_: String,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub placeholder: String,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub field: String,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub href: String,
    #[serde(default, serialize_with = "ser_num", deserialize_with = "null_default")]
    pub x: f64,
    #[serde(default, serialize_with = "ser_num", deserialize_with = "null_default")]
    pub y: f64,
    #[serde(
        default,
        skip_serializing_if = "is_zero_f64",
        serialize_with = "ser_num",
        deserialize_with = "null_default"
    )]
    pub vw: f64,
    #[serde(
        default,
        skip_serializing_if = "is_zero_f64",
        serialize_with = "ser_num",
        deserialize_with = "null_default"
    )]
    pub vh: f64,
}

/// FlowStep is one thing the person did. Kinds: goto, click, dblclick, rclick, fill, select, key,
/// drag, back, forward, reload, tab_new, tab_switch, tab_close, and page (the page address changed:
/// not an action, the result of the step before it).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FlowStep {
    #[serde(default, deserialize_with = "null_default")]
    pub kind: String,
    /// ms since the recording started
    #[serde(default, deserialize_with = "null_default")]
    pub at: i64,
    /// goto/page/tab_new: the address; others: the page it happened on
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<FlowTarget>,
    /// drag: where it was dropped
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<FlowTarget>,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub value: String,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_default"
    )]
    pub values: Vec<String>,
    /// a password: what was typed is not stored
    #[serde(
        default,
        skip_serializing_if = "crate::is_false",
        deserialize_with = "null_default"
    )]
    pub secret: bool,
    /// tab_switch / tab_close: tab position
    #[serde(
        default,
        skip_serializing_if = "is_zero_usize",
        deserialize_with = "null_default"
    )]
    pub index: i64,
}

/// zero `time.Time` as encoding/json writes it.
const ZERO_TIME: &str = "0001-01-01T00:00:00Z";

fn zero_time() -> String {
    ZERO_TIME.to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Flow {
    #[serde(default, deserialize_with = "null_default")]
    pub id: String,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    /// RFC 3339, as `time.Time` (kept as the string it was read as)
    #[serde(default = "zero_time", deserialize_with = "null_created")]
    pub created: String,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub by: String,
    #[serde(
        default,
        rename = "startUrl",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub start_url: String,
    #[serde(
        default,
        rename = "startTitle",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub start_title: String,
    /// ms
    #[serde(default, deserialize_with = "null_default")]
    pub duration: i64,
    #[serde(default, deserialize_with = "null_default")]
    pub view: View,
    #[serde(default, deserialize_with = "null_default")]
    pub steps: Vec<FlowStep>,
}

fn null_created<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_else(zero_time))
}

/// FlowInfo is what the viewer gets: the live recording, a saved flow, or a row of the list.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FlowInfo {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub on: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<crate::User>,
    /// no omitempty
    #[serde(default)]
    pub count: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last: String,
    /// recording started (unix ms)
    #[serde(default, skip_serializing_if = "crate::is_zero_i64")]
    pub since: i64,
    /// `time.Time` with `omitzero` (RFC 3339 string; None = the zero time)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, rename = "startUrl", skip_serializing_if = "String::is_empty")]
    pub start_url: String,
    /// ms
    #[serde(default, skip_serializing_if = "crate::is_zero_i64")]
    pub duration: i64,
    /// password steps: a replay needs them typed in
    #[serde(default, skip_serializing_if = "crate::is_zero_i64")]
    pub secrets: i64,
}

struct Recording {
    flow: Flow,
    by: Arc<Client>,
    start: Instant,
    /// recording started (unix ms)
    start_ms: i64,
    stopping: bool,
}

/// `Hub.rec`: the live recording (locked after the hub state).
static REC: Mutex<Option<Recording>> = Mutex::new(None);
/// `Browser.recOn`
static REC_ON: AtomicBool = AtomicBool::new(false);
/// The hub and the browser (`h.browser`, `b.hub`).
static CTX: OnceLock<(Weak<Hub>, Weak<Browser>)> = OnceLock::new();

/// Gives the module the hub and the browser (once, when the browser has started).
pub fn attach(hub: &Arc<Hub>, b: &Arc<Browser>) {
    let _ = CTX.set((Arc::downgrade(hub), Arc::downgrade(b)));
}

fn ctx() -> Option<(Arc<Hub>, Arc<Browser>)> {
    let (h, b) = CTX.get()?;
    Some((h.upgrade()?, b.upgrade()?))
}

// ---------- Time (time.Time as encoding/json writes it) ----------

/// days since 1970-01-01 of a civil date (Howard Hinnant)
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// An RFC 3339 time: unix seconds, nanoseconds, and the UTC offset it was written with.
fn parse_time(s: &str) -> Option<(i64, u32, i64)> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let n = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (
        n(0, 4)?,
        n(5, 7)?,
        n(8, 10)?,
        n(11, 13)?,
        n(14, 16)?,
        n(17, 19)?,
    );
    let mut i = 19;
    let mut nanos = 0u32;
    if b[i] == b'.' {
        let st = i + 1;
        i = st;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let frac = &s[st..i];
        if frac.is_empty() {
            return None;
        }
        let mut f = frac.chars().take(9).collect::<String>();
        while f.len() < 9 {
            f.push('0');
        }
        nanos = f.parse().ok()?;
    }
    let off = match b.get(i)? {
        b'Z' | b'z' => 0,
        sign @ (b'+' | b'-') => {
            let (oh, om) = (n(i + 1, i + 3)?, n(i + 4, i + 6)?);
            let v = oh * 3600 + om * 60;
            if *sign == b'-' {
                -v
            } else {
                v
            }
        }
        _ => return None,
    };
    let secs = days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se - off;
    Some((secs, nanos, off))
}

pub fn is_zero_time(s: &str) -> bool {
    parse_time(s).is_none_or(|(secs, n, _)| secs == -62135596800 && n == 0)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// (year, month, day, hour, minute) in the time's own zone
fn civil_of(s: &str) -> (i64, i64, i64, i64, i64) {
    let (secs, _, off) = parse_time(s).unwrap_or((-62135596800, 0, 0));
    let l = secs + off;
    let (y, m, d) = civil_from_days(l.div_euclid(86400));
    let r = l.rem_euclid(86400);
    (y, m, d, r / 3600, r % 3600 / 60)
}

/// layout "2006-01-02 15:04"
fn fmt_long(s: &str) -> String {
    let (y, m, d, h, mi) = civil_of(s);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}")
}

/// layout "Jan 2 15:04"
fn fmt_short(s: &str) -> String {
    let (_, m, d, h, mi) = civil_of(s);
    format!("{} {d} {h:02}:{mi:02}", MONTHS[(m - 1) as usize])
}

fn time_key(s: &str) -> (i64, u32) {
    parse_time(s).map_or((-62135596800, 0), |(a, b, _)| (a, b))
}

/// `time.Duration(ms).Round(time.Second).String`
fn go_dur_rounded(ms: i64) -> String {
    let secs = if ms >= 0 {
        (ms + 500) / 1000
    } else {
        -((-ms + 500) / 1000)
    };
    let (neg, s) = (secs < 0, secs.abs());
    let (h, m, sec) = (s / 3600, s % 3600 / 60, s % 60);
    let body = match (h, m) {
        (0, 0) => format!("{sec}s"),
        (0, _) => format!("{m}m{sec}s"),
        _ => format!("{h}h{m}m{sec}s"),
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

// ---------- Storage ----------

/// `^flow-[0-9a-f]{6}$`
fn flow_id_ok(id: &str) -> bool {
    id.len() == 11
        && id.starts_with("flow-")
        && id[5..]
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn norm_flow_id(id: &str) -> String {
    crate::tools::norm_flow_id(id)
}

pub fn flow_path(dir: &str, id: &str) -> Result<std::path::PathBuf, String> {
    let id = norm_flow_id(id);
    if !flow_id_ok(&id) {
        return Err(format!(
            "{} is not a flow id (they look like flow-3fa9c1)",
            go_quote(&id)
        ));
    }
    Ok(Path::new(dir).join(format!("{id}.json")))
}

pub fn new_flow_id(dir: &str) -> String {
    loop {
        let id = format!("flow-{:06x}", rand::random::<u32>() & 0xff_ffff);
        if !Path::new(dir).join(format!("{id}.json")).exists() {
            return id;
        }
    }
}

/// json.MarshalIndent(f, "", "  ")
pub fn flow_json(f: &Flow) -> String {
    crate::go_escape(serde_json::to_string_pretty(f).unwrap_or_default())
}

pub fn save_flow(dir: &str, f: &Flow) -> Result<(), String> {
    let p = flow_path(dir, &f.id)?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let data = flow_json(f);
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true).mode(0o600);
        o.open(&p)
            .and_then(|mut fh| fh.write_all(data.as_bytes()))
            .map_err(|e| e.to_string())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&p, data).map_err(|e| e.to_string())
    }
}

pub fn load_flow(dir: &str, id: &str) -> Result<Flow, String> {
    let p = flow_path(dir, id)?;
    let data = std::fs::read(&p).map_err(|_| {
        format!(
            "no flow called {} (call browser_get_flow without an id to list them)",
            norm_flow_id(id)
        )
    })?;
    serde_json::from_slice(&data).map_err(|e| format!("flow {} is damaged: {e}", norm_flow_id(id)))
}

impl Flow {
    pub fn info(&self) -> FlowInfo {
        let n = self.steps.iter().filter(|s| s.secret).count() as i64;
        FlowInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            count: self.steps.len() as i64,
            created: (!is_zero_time(&self.created)).then(|| self.created.clone()),
            start_url: self.start_url.clone(),
            duration: self.duration,
            secrets: n,
            ..Default::default()
        }
    }
}

pub fn list_flows(dir: &str) -> Vec<FlowInfo> {
    let mut out: Vec<(FlowInfo, String)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".json").unwrap_or(&name).to_string();
            if e.file_type().is_ok_and(|t| t.is_dir()) || !flow_id_ok(&id) {
                continue;
            }
            if let Ok(f) = load_flow(dir, &id) {
                out.push((f.info(), f.created.clone()));
            }
        }
    }
    out.sort_by_key(|a| std::cmp::Reverse(time_key(&a.1)));
    out.into_iter().map(|(i, _)| i).collect()
}

// ---------- Recording in the tabs ----------

/// recScript runs in every new document of a tab while a recording is on.
fn rec_script() -> String {
    format!("{}\n;__abu.recStart();", crate::agent::agent_js())
}

/// SetRecording turns the recorder on or off in every tab (tabs opened later get it in attach).
async fn set_recording(b: &Arc<Browser>, on: bool) {
    let tabs = {
        let mut st = b.st();
        st.rec_on = on;
        REC_ON.store(on, Ordering::SeqCst);
        st.tabs.clone()
    };
    for t in tabs {
        if on {
            rec_install(&t).await;
        } else {
            rec_uninstall(&t).await;
        }
    }
}

async fn rec_install(t: &Arc<Tab>) {
    if t.call_cmd("Runtime.addBinding", json!({"name": REC_BINDING}))
        .await
        .is_err()
    {
        return;
    }
    let src = rec_script();
    let Ok(res) = t
        .call_cmd(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": src}),
        )
        .await
    else {
        return;
    };
    t.st().rec_script = res["identifier"].as_str().map(str::to_string);
    let _ = t
        .call_cmd("Runtime.evaluate", json!({"expression": src}))
        .await; // the page that is already open
}

async fn rec_uninstall(t: &Arc<Tab>) {
    let id = t.st().rec_script.take();
    // recStop also reports the text still being typed, so nothing is lost when Stop is pressed mid-word.
    let _ = t
        .call_cmd(
            "Runtime.evaluate",
            json!({"expression": "window.__abu && window.__abu.recStop && window.__abu.recStop()"}),
        )
        .await;
    if let Some(id) = id {
        let _ = t
            .call_cmd(
                "Page.removeScriptToEvaluateOnNewDocument",
                json!({"identifier": id}),
            )
            .await;
    }
    let _ = t
        .call_cmd("Runtime.removeBinding", json!({"name": REC_BINDING}))
        .await;
}

/// The recorder's browser hooks (the methods `recording`, `recInstall`, `onRec`, `onPageChange`).
pub struct FlowHooks;

impl BrowserHooks for FlowHooks {
    fn recording(&self) -> bool {
        REC_ON.load(Ordering::SeqCst)
    }

    fn rec_install(&self, t: &Arc<Tab>) {
        let t = t.clone();
        tokio::spawn(async move { rec_install(&t).await });
    }

    /// onRec gets an action from the recorder in a page. Only the tab people see counts.
    fn binding(&self, t: &Arc<Tab>, name: &str, payload: &str) {
        if name != REC_BINDING {
            return;
        }
        let Some((hub, b)) = ctx() else { return };
        let active = b.st().active.as_ref().is_some_and(|a| Arc::ptr_eq(a, t));
        let u = t.st().url.clone();
        if active {
            rec_event(&hub, &u, payload);
        }
    }

    /// onPageChange notes that the tab people see is on a new address (a link, a form, a redirect…).
    fn page_changed(&self, t: &Arc<Tab>, u: &str) {
        let Some((hub, b)) = ctx() else { return };
        let (active, on) = {
            let st = b.st();
            (
                st.active.as_ref().is_some_and(|a| Arc::ptr_eq(a, t)),
                st.rec_on,
            )
        };
        if active && on && u.starts_with("http") {
            rec_page(&hub, u);
        }
    }
}

// ---------- Hub: the recording ----------

/// recLive says whether actions count now: a recording is on and a person (not an agent) drives.
/// caller holds h.mu
fn rec_live(st: &HubState, g: &Option<Recording>) -> bool {
    g.is_some() && st.ctrl.as_ref().is_some_and(|c| !Arc::ptr_eq(c, &st.agent))
}

fn info_of(r: &Recording) -> FlowInfo {
    let mut info = FlowInfo {
        on: true,
        by: Some(r.by.user()),
        count: r.flow.steps.len() as i64,
        since: r.start_ms,
        ..Default::default()
    };
    if let Some(s) = r.flow.steps.last() {
        info.last = s.label();
    }
    info
}

/// `h.rec != nil` -> `recInfo`
pub fn rec_info() -> Option<FlowInfo> {
    lock(&REC).as_ref().map(info_of)
}

/// `h.rec != nil && h.rec.by == c`
pub fn rec_by(c: &Arc<Client>) -> bool {
    lock(&REC).as_ref().is_some_and(|r| Arc::ptr_eq(&r.by, c))
}

fn rec_msg(info: FlowInfo, type_: &str) -> Msg {
    Msg {
        type_: type_.into(),
        rec: Some(info),
        ..Default::default()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// caller holds h.mu
pub fn rec_start(st: &mut HubState, c: &Arc<Client>) {
    let Some((_, b)) = ctx() else { return };
    let mut g = lock(&REC);
    if g.is_some() {
        return;
    }
    let (tabs, active, _, v) = b.state();
    let mut f = Flow {
        created: crate::assets::now_rfc3339(),
        by: c.user().name,
        view: v,
        steps: Vec::new(),
        ..Default::default()
    };
    for t in &tabs {
        if t.id == active && t.url.starts_with("http") {
            f.start_url = t.url.clone();
            f.start_title = t.title.clone();
        }
    }
    let r = Recording {
        flow: f,
        by: c.clone(),
        start: Instant::now(),
        start_ms: now_ms(),
        stopping: false,
    };
    let info = info_of(&r);
    *g = Some(r);
    drop(g);
    tokio::spawn(async move { set_recording(&b, true).await });
    st.broadcast(&rec_msg(info, "rec"), None);
    st.add_system_chat(&format!("⏺ {} started recording a flow", c.user().name));
}

/// recStop ends the recording and saves it (unless discard). c gets the id; nil when the person left.
/// caller holds h.mu
pub fn rec_stop(_st: &mut HubState, c: Option<&Arc<Client>>, discard: bool) {
    let Some((hub, b)) = ctx() else { return };
    {
        let mut g = lock(&REC);
        match g.as_mut() {
            Some(r) if !r.stopping => r.stopping = true,
            _ => return,
        }
    }
    let c = c.cloned();
    tokio::spawn(async move {
        set_recording(&b, false).await; // reports text still being typed…
        tokio::time::sleep(Duration::from_millis(150)).await; // …which arrives as an event just after
        let mut st = hub.lock();
        let r = lock(&REC).take();
        st.broadcast(&rec_msg(FlowInfo::default(), "rec"), None);
        if let Some(r) = r {
            finish_rec(&mut st, &b, r, c, discard);
        }
    });
}

/// caller holds h.mu
fn finish_rec(
    st: &mut HubState,
    b: &Arc<Browser>,
    r: Recording,
    c: Option<Arc<Client>>,
    discard: bool,
) {
    let mut f = r.flow;
    if !f.steps.iter().any(|s| s.kind != "page") {
        f.steps.clear(); // only page changes, no action: nothing to replay
    }
    let reply = |st: &HubState, info: FlowInfo| {
        if let Some(c) = &c {
            if st.clients.iter().any(|x| Arc::ptr_eq(x, c)) {
                c.try_send_best_effort(OutMsg {
                    bin: false,
                    data: Bytes::from(go_json(&rec_msg(info, "rec_done"))),
                });
            }
        }
    };
    if discard {
        st.add_system_chat("⏹ The recording was discarded");
        return;
    }
    if f.steps.is_empty() {
        reply(st, FlowInfo::default());
        st.add_system_chat("⏹ Recording stopped: no actions were recorded");
        return;
    }
    let dir = b.opt.flow_dir.clone();
    f.id = new_flow_id(&dir);
    f.duration = r.start.elapsed().as_millis() as i64;
    f.name = default_flow_name(&f);
    if let Err(e) = save_flow(&dir, &f) {
        st.add_system_chat(&format!("⚠️ Could not save the flow: {e}"));
        return;
    }
    reply(st, f.info());
    st.broadcast(
        &Msg {
            type_: "flows".into(),
            ..Default::default()
        },
        None,
    ); // every viewer's flow list
    st.add_system_chat(&format!(
        "🎬 Flow {} saved ({} steps): give this id to an agent, it reads it with browser_get_flow and replays it with browser_run_flow.",
        f.id,
        f.steps.len()
    ));
}

pub fn default_flow_name(f: &Flow) -> String {
    let mut wh = f.start_title.clone();
    if wh.is_empty() {
        for s in &f.steps {
            if !s.url.is_empty() {
                if let Ok(u) = url::Url::parse(&s.url) {
                    wh = u.host_str().unwrap_or_default().to_string();
                }
                break;
            }
        }
    }
    let r: Vec<char> = wh.chars().collect();
    if r.len() > 40 {
        wh = r[..39].iter().collect::<String>() + "…";
    }
    let s = format!("{wh} · {}", fmt_short(&f.created));
    s.strip_prefix(" · ").map(str::to_string).unwrap_or(s)
}

/// recEvent takes an action reported by the recorder in the page (the page could call the binding itself, so validate).
fn rec_event(hub: &Arc<Hub>, page_url: &str, payload: &str) {
    if payload.len() > 64 * 1024 {
        return;
    }
    #[derive(Deserialize, Default)]
    struct Ev {
        #[serde(default, deserialize_with = "null_default")]
        k: String,
        #[serde(default)]
        t: Option<FlowTarget>,
        #[serde(default)]
        to: Option<FlowTarget>,
        #[serde(default, deserialize_with = "null_default")]
        v: String,
        #[serde(default, deserialize_with = "null_default")]
        vs: Vec<String>,
        #[serde(default, deserialize_with = "null_default")]
        secret: bool,
    }
    let Ok(e) = serde_json::from_str::<Ev>(payload) else {
        return;
    };
    let st = hub.lock();
    let mut g = lock(&REC);
    if !rec_live(&st, &g) {
        return;
    }
    let mut s = FlowStep {
        kind: e.k.clone(),
        url: page_url.to_string(),
        target: tidy(e.t),
        to: tidy(e.to),
        ..Default::default()
    };
    match e.k.as_str() {
        "click" | "rclick" => {
            if s.target.is_none() {
                return;
            }
            s.to = None;
        }
        "drag" => {
            if s.target.is_none() || s.to.is_none() {
                return;
            }
        }
        "dblclick" => {
            if let Some(r) = g.as_mut() {
                rec_double(&st, r);
            }
            return;
        }
        "fill" => {
            if s.target.is_none() {
                return;
            }
            s.to = None;
            s.secret = e.secret;
            if !s.secret {
                s.value = cut(&e.v, 5000);
            }
        }
        "select" => {
            if s.target.is_none() || e.vs.is_empty() {
                return;
            }
            s.to = None;
            s.values = e.vs.iter().take(20).map(|v| cut(v, 200)).collect();
        }
        "key" => {
            if parse_key(&e.v).is_err() || e.v.len() > 40 {
                return;
            }
            s.target = None;
            s.to = None;
            s.value = e.v;
        }
        _ => return,
    }
    if let Some(r) = g.as_mut() {
        rec_add(&st, r, s);
    }
}

/// recPage notes a new page address after an action.
fn rec_page(hub: &Arc<Hub>, u: &str) {
    let st = hub.lock();
    let mut g = lock(&REC);
    if !rec_live(&st, &g) {
        return;
    }
    if let Some(r) = g.as_mut() {
        rec_add(
            &st,
            r,
            FlowStep {
                kind: "page".into(),
                url: u.to_string(),
                ..Default::default()
            },
        );
    }
}

/// recInput records a viewer toolbar command (address bar, back, tabs…) from the person in control.
/// caller holds h.mu
pub fn rec_input(st: &mut HubState, m: &Msg) {
    let mut g = lock(&REC);
    let Some(r) = g.as_mut() else { return };
    if st.ctrl.as_ref().is_some_and(|c| Arc::ptr_eq(c, &st.agent)) {
        return;
    }
    let Some((_, b)) = ctx() else { return };
    let tab_index = |id: &str| -> Option<i64> {
        let (tabs, _, _, _): (Vec<TabInfo>, _, _, _) = b.state();
        tabs.iter().position(|t| t.id == id).map(|i| i as i64)
    };
    let step = |kind: &str| FlowStep {
        kind: kind.into(),
        ..Default::default()
    };
    match m.type_.as_str() {
        "nav" => {
            let u = normalize_url(&m.url);
            if !u.is_empty() {
                rec_add(
                    st,
                    r,
                    FlowStep {
                        url: u,
                        ..step("goto")
                    },
                );
            }
        }
        "back" | "forward" | "reload" => rec_add(st, r, step(&m.type_)),
        "tab_new" => rec_add(
            st,
            r,
            FlowStep {
                url: normalize_url(&m.url),
                ..step("tab_new")
            },
        ),
        "tab_activate" => {
            if let Some(i) = tab_index(&m.id) {
                rec_add(
                    st,
                    r,
                    FlowStep {
                        index: i,
                        ..step("tab_switch")
                    },
                );
            }
        }
        "tab_close" => {
            if let Some(i) = tab_index(&m.id) {
                rec_add(
                    st,
                    r,
                    FlowStep {
                        index: i,
                        ..step("tab_close")
                    },
                );
            }
        }
        _ => {}
    }
}

/// recDouble turns the two clicks before a double-click into one double-click step.
/// caller holds h.mu
fn rec_double(st: &HubState, r: &mut Recording) {
    let f = &mut r.flow;
    let mut n = f.steps.len();
    if n == 0 || f.steps[n - 1].kind != "click" {
        return;
    }
    if n >= 2
        && f.steps[n - 2].kind == "click"
        && same(&f.steps[n - 2].target, &f.steps[n - 1].target)
    {
        f.steps.truncate(n - 1);
        n -= 1;
    }
    f.steps[n - 1].kind = "dblclick".into();
    rec_changed(st, r);
}

/// recAdd appends a step, merging it with the one before when they are really one action.
/// caller holds h.mu
fn rec_add(st: &HubState, r: &mut Recording, mut s: FlowStep) {
    s.at = r.start.elapsed().as_millis() as i64;
    let f = &mut r.flow;
    let n = f.steps.len();
    if n == 0 {
        if s.kind == "page" {
            return; // where the recording started
        }
        if n >= MAX_FLOW_STEPS {
            return;
        }
        f.steps.push(s);
        rec_changed(st, r);
        return;
    }
    let last = &mut f.steps[n - 1];
    if s.kind == "fill" && last.kind == "fill" && same(&last.target, &s.target) {
        // more typing in the same field
        last.value = s.value;
        last.secret = s.secret;
        last.at = s.at;
    } else if (s.kind == "fill" || s.kind == "select")
        && last.kind == "click"
        && same(&last.target, &s.target)
    {
        *last = s; // the click only put the focus there; typing into a field clicks it anyway
    } else if s.kind == "page" && last.kind == "page" {
        last.url = s.url; // a redirect
        last.at = s.at;
    } else if s.kind == "page"
        && ["goto", "back", "forward", "reload", "tab_new", "tab_switch"]
            .contains(&last.kind.as_str())
    {
        return; // the obvious result of that step (or where the recording started)
    } else if s.kind == "page" && same_url(&last.url, &s.url) {
        return; // still the same page
    } else {
        if n >= MAX_FLOW_STEPS {
            return;
        }
        f.steps.push(s);
    }
    rec_changed(st, r);
}

/// caller holds h.mu
fn rec_changed(st: &HubState, r: &Recording) {
    st.broadcast(&rec_msg(info_of(r), "rec"), None);
}

pub fn same_url(a: &str, b: &str) -> bool {
    let trim = |s: &str| -> String {
        let s = s.split('#').next().unwrap_or_default();
        s.strip_suffix('/').unwrap_or(s).to_string()
    };
    trim(a) == trim(b)
}

pub fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// tidy bounds what came from the page.
pub fn tidy(t: Option<FlowTarget>) -> Option<FlowTarget> {
    let mut t = t?;
    for p in [&mut t.tag, &mut t.role, &mut t.test_attr, &mut t.type_] {
        *p = cut(p, 40);
    }
    for p in [
        &mut t.name,
        &mut t.test_id,
        &mut t.id,
        &mut t.text,
        &mut t.placeholder,
        &mut t.field,
    ] {
        *p = cut(p, 200);
    }
    t.sel = cut(&t.sel, 500);
    t.href = cut(&t.href, 300);
    if !t.test_attr.is_empty()
        && ![
            "data-testid",
            "data-test-id",
            "data-test",
            "data-qa",
            "data-cy",
        ]
        .contains(&t.test_attr.as_str())
    {
        t.test_attr.clear();
        t.test_id.clear();
    }
    Some(t)
}

fn same(a: &Option<FlowTarget>, o: &Option<FlowTarget>) -> bool {
    let (Some(t), Some(o)) = (a, o) else {
        return false;
    };
    (!t.sel.is_empty() && t.sel == o.sel)
        || (!t.name.is_empty() && t.role == o.role && t.name == o.name)
}

impl FlowTarget {
    /// positional: nothing to recognize the element by but where it was (a canvas, an empty box…).
    pub fn positional(&self) -> bool {
        self.tag == "canvas"
            || (self.role.is_empty()
                && self.name.is_empty()
                && self.text.is_empty()
                && self.test_id.is_empty()
                && self.id.is_empty()
                && self.placeholder.is_empty())
    }
}

// ---------- Describing a flow ----------

/// what names the element for people and agents: role "name", else tag "text", else a point.
fn what(t: &Option<FlowTarget>) -> String {
    let Some(t) = t else { return "?".into() };
    if !t.role.is_empty() && !t.name.is_empty() {
        format!("{} {}", t.role, quote(&t.name))
    } else if !t.text.is_empty() {
        format!(
            "{} {}",
            cmp_or(&t.role, &cmp_or(&t.tag, "element")),
            quote(&t.text)
        )
    } else if !t.placeholder.is_empty() {
        format!(
            "{} with placeholder {}",
            cmp_or(&t.role, "textbox"),
            quote(&t.placeholder)
        )
    } else if !t.role.is_empty() {
        t.role.clone()
    } else if !t.tag.is_empty() {
        format!("{} at ({:.0}, {:.0})", t.tag, t.x, t.y)
    } else {
        format!("point ({:.0}, {:.0})", t.x, t.y)
    }
}

/// hints are the other ways to find the element, for an agent that follows the flow itself.
pub fn hints(t: &Option<FlowTarget>) -> String {
    let Some(t) = t else { return String::new() };
    let mut h: Vec<String> = Vec::new();
    if t.count > 1 {
        h.push(format!("#{} of {} with this name", t.nth + 1, t.count));
    }
    if !t.test_id.is_empty() {
        h.push(format!("{}={}", t.test_attr, go_quote(&t.test_id)));
    }
    if !t.id.is_empty() {
        h.push(format!("id={}", t.id));
    }
    if !t.href.is_empty() {
        h.push(format!("href={}", t.href));
    }
    if !t.sel.is_empty() {
        h.push(format!("css: {}", t.sel));
    }
    if t.positional() {
        h.push(format!(
            "point {:.0},{:.0} of a {:.0}×{:.0} page",
            t.x, t.y, t.vw, t.vh
        ));
    }
    if h.is_empty() {
        return String::new();
    }
    format!("  [{}]", h.join("; "))
}

impl FlowStep {
    /// label is one short line for the step.
    pub fn label(&self) -> String {
        match self.kind.as_str() {
            "goto" => format!("Open {}", self.url),
            "click" => format!("Click {}", what(&self.target)),
            "dblclick" => format!("Double-click {}", what(&self.target)),
            "rclick" => format!("Right-click {}", what(&self.target)),
            "fill" => {
                if self.secret {
                    format!(
                        "Type the password (not recorded) into {}",
                        what(&self.target)
                    )
                } else if self.value.is_empty() {
                    format!("Clear {}", what(&self.target))
                } else {
                    format!("Type {} into {}", quote(&self.value), what(&self.target))
                }
            }
            "select" => format!(
                "Choose {} in {}",
                quote(&self.values.join(", ")),
                what(&self.target)
            ),
            "key" => format!("Press {}", self.value),
            "drag" => format!("Drag {} onto {}", what(&self.target), what(&self.to)),
            "back" => "Go back".into(),
            "forward" => "Go forward".into(),
            "reload" => "Reload the page".into(),
            "tab_new" => format!(
                "Open a new tab{}",
                if self.url.is_empty() {
                    String::new()
                } else {
                    format!(": {}", self.url)
                }
            ),
            "tab_switch" => format!("Switch to tab {}", self.index),
            "tab_close" => format!("Close tab {}", self.index),
            "page" => format!("→ the page becomes {}", self.url),
            k => k.to_string(),
        }
    }
}

impl Flow {
    pub fn describe(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("### Flow {} {}\n", self.id, quote(&self.name)));
        s.push_str(&format!(
            "Recorded by {} on {}: {} steps in {}.\n",
            cmp_or(&self.by, "someone"),
            fmt_long(&self.created),
            self.steps.len(),
            go_dur_rounded(self.duration)
        ));
        if !self.start_url.is_empty() {
            s.push_str(&format!(
                "Start page: {} ({})\n",
                self.start_url, self.start_title
            ));
        }
        s.push_str("\nSteps:\n");
        let (mut secret, mut typed): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
        for (i, st) in self.steps.iter().enumerate() {
            let n = (i + 1).to_string();
            let mut line = format!("{n}. {}", st.label());
            if st.target.is_some() && st.kind != "page" {
                line.push_str(&hints(&st.target));
            }
            if st.kind == "drag" {
                line.push_str(&hints(&st.to));
            }
            s.push_str(&line);
            s.push('\n');
            if st.kind == "fill" || st.kind == "select" {
                typed.push(n.clone());
                if st.secret {
                    secret.push(n);
                }
            }
        }
        s.push_str("\nHow to use it:\n");
        s.push_str(&format!(
            "- Replay it exactly: browser_run_flow with flowId {} (repeat: N runs it N times, each from the start page).\n",
            go_quote(&self.id)
        ));
        if !typed.is_empty() {
            s.push_str(&format!(
                "- Change what is typed or chosen with inputs, by step number: e.g. {{{}: \"other text\"}} (steps with input: {}).\n",
                go_quote(&typed[0]),
                typed.join(", ")
            ));
        }
        if !secret.is_empty() {
            s.push_str(&format!(
                "- Passwords are not recorded: step(s) {} need inputs. Ask the person (browser_say) or reuse a saved login (browser_state).\n",
                secret.join(", ")
            ));
        }
        s.push_str("- Or follow it yourself to adapt it (other data, a loop, a different order): find each element in browser_snapshot by its role and name; the hints in [ ] are fallbacks. Steps with → show where the page should be after the step before.\n");
        s
    }
}

// ---------- Agent: reading and replaying ----------

#[derive(Debug, Default, Deserialize)]
struct FlowFound {
    #[serde(default, rename = "ref", deserialize_with = "null_default")]
    ref_: String,
    #[serde(default, deserialize_with = "null_default")]
    how: String,
    #[allow(dead_code)]
    #[serde(default, deserialize_with = "null_default")]
    checked: bool,
}

/// Clears `flowRun` when a replay ends, however it ends.
struct RunGuard(Arc<Hub>, String);

impl Drop for RunGuard {
    fn drop(&mut self) {
        crate::flow_http::flow_run(&self.0, &self.1, false);
    }
}

impl Agent {
    /// flow.rs `GetFlow`
    pub async fn get_flow(&self, id: &str) -> BResult<String> {
        let dir = self.b.opt.flow_dir.clone();
        if id.trim().is_empty() {
            let flows = list_flows(&dir);
            let mut s = String::from("### Recorded flows\n");
            if flows.is_empty() {
                s.push_str("None yet. A person records one with the Record button in the viewer and gives you its id.\n");
            }
            for f in flows {
                let created = f
                    .created
                    .as_deref()
                    .map(fmt_long)
                    .unwrap_or_else(|| fmt_long(ZERO_TIME));
                s.push_str(&format!(
                    "- {} {}: {} steps, recorded {}{}\n",
                    f.id,
                    quote(&f.name),
                    f.count,
                    created,
                    if f.start_url.is_empty() {
                        String::new()
                    } else {
                        format!(", starts at {}", f.start_url)
                    }
                ));
            }
            return Ok(s);
        }
        let f = load_flow(&dir, id)?;
        self.announce(
            "🎬",
            &format!("Read flow {} {}", f.id, quote(&f.name)),
            None,
        );
        Ok(f.describe())
    }

    /// findTarget looks for a recorded element, waiting for it to show up (the page may still be loading).
    async fn find_target(&self, t: &Option<FlowTarget>, wait: Duration) -> BResult<FlowFound> {
        let js = serde_json::to_string(t).unwrap_or_else(|_| "null".into());
        let end = Instant::now() + wait;
        loop {
            let res = self
                .eval_retry(|tab| {
                    let expr = format!("__abu.find({js})");
                    async move { eval_js_as::<FlowFound>(&tab, &expr).await }
                })
                .await;
            if let Ok(r) = &res {
                if !r.ref_.is_empty() {
                    return res;
                }
            }
            if Instant::now() > end {
                return match res {
                    Err(e) => Err(e),
                    Ok(_) => Err(format!(
                        "could not find {} on the page ({})",
                        what(t),
                        self.page_url()
                    )),
                };
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    /// point turns a recorded screen point into a point on the current screen (its size may differ).
    fn point(&self, t: &FlowTarget) -> (f64, f64) {
        let v = self.b.view_size();
        let (mut x, mut y) = (t.x, t.y);
        if t.vw > 0.0 && t.vh > 0.0 {
            x = x * v.width as f64 / t.vw;
            y = y * v.height as f64 / t.vh;
        }
        (x, y)
    }

    /// RunFlow replays a recorded flow, repeat times. inputs replaces what steps type or choose, by step number.
    pub async fn run_flow(
        &self,
        id: &str,
        repeat: i64,
        inputs: &HashMap<String, String>,
        skip_start: bool,
    ) -> BResult<String> {
        let f = load_flow(&self.b.opt.flow_dir, id)?;
        let repeat = repeat.clamp(1, MAX_REPEAT);
        let mut keys: Vec<&String> = inputs.keys().collect();
        keys.sort();
        for k in keys {
            let ok = k.parse::<usize>().is_ok_and(|n| {
                n >= 1
                    && n <= f.steps.len()
                    && matches!(f.steps[n - 1].kind.as_str(), "fill" | "select")
            });
            if !ok {
                return Err(format!(
                    "inputs: {} is not the number of a step that types or chooses something (see browser_get_flow)",
                    go_quote(k)
                ));
            }
        }
        let missing: Vec<String> = f
            .steps
            .iter()
            .enumerate()
            .filter(|(i, s)| s.secret && !inputs.contains_key(&(i + 1).to_string()))
            .map(|(i, _)| (i + 1).to_string())
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "step(s) {} type a password, which is not recorded: pass it in inputs, e.g. {{{}: \"…\"}}. Ask the person for it with browser_say, or load a saved login with browser_state and start the flow after the login steps",
                missing.join(", "),
                go_quote(&missing[0])
            ));
        }

        crate::flow_http::FLOW_STOP.store(false, Ordering::SeqCst);
        crate::flow_http::flow_run(&self.hub, &f.id, true);
        let _run = RunGuard(self.hub.clone(), f.id.clone());
        self.announce(
            "🎬",
            &format!(
                "Run flow {} {} ({} steps{})",
                f.id,
                quote(&f.name),
                f.steps.len(),
                if repeat > 1 {
                    format!(" × {repeat}")
                } else {
                    String::new()
                }
            ),
            None,
        );
        let mut log = String::new();
        for run in 1..=repeat {
            if repeat > 1 {
                log.push_str(&format!("Run {run} of {repeat}:\n"));
            }
            if !skip_start
                && !f.start_url.is_empty()
                && (run > 1 || !same_url(&self.page_url(), &f.start_url))
            {
                if let Err(e) = self.navigate(&f.start_url).await {
                    return Err(format!(
                        "{log}could not open the start page {}: {e}",
                        f.start_url
                    ));
                }
                log.push_str(&format!("- opened the start page {}\n", f.start_url));
            }
            for (i, s) in f.steps.iter().enumerate() {
                let mut s = s.clone();
                if crate::flow_http::FLOW_STOP.load(Ordering::SeqCst) {
                    return Err(format!(
                        "{log}⏹ stopped by a person before step {} ({})",
                        i + 1,
                        s.label()
                    ));
                }
                let input = inputs.get(&(i + 1).to_string());
                let res = self.run_step(&s, input).await;
                if let Some(inp) = input {
                    if !s.secret {
                        // the log shows what was really typed or chosen
                        s.value = inp.clone();
                        s.values = vec![inp.clone()];
                    }
                }
                match res {
                    Err(e) => {
                        return Err(format!(
                            "{log}✗ step {} ({}) failed: {e}\nThe steps before it were done. Take a snapshot and finish the flow yourself from step {} (browser_get_flow shows the rest), or ask the person",
                            i + 1,
                            s.label(),
                            i + 1
                        ))
                    }
                    Ok(note) => log.push_str(&format!("✓ {}. {}{note}\n", i + 1, s.label())),
                }
            }
        }
        Ok(log)
    }

    async fn run_step(&self, s: &FlowStep, input: Option<&String>) -> BResult<String> {
        const WAIT: Duration = Duration::from_secs(15);
        let target = || s.target.clone().unwrap_or_default();
        match s.kind.as_str() {
            "goto" => self.navigate(&s.url).await.map(|_| String::new()),
            "back" => self.history(-1).await.map(|_| String::new()),
            "forward" => self.history(1).await.map(|_| String::new()),
            "reload" => self.reload().await.map(|_| String::new()),
            "tab_new" => self
                .tabs("new", -1, &s.url, "", false)
                .await
                .map(|_| String::new()),
            "tab_switch" => self
                .tabs("select", s.index, "", "", false)
                .await
                .map(|_| String::new()),
            "tab_close" => self
                .tabs("close", s.index, "", "", false)
                .await
                .map(|_| String::new()),
            "key" => self.press(&s.value).await.map(|_| String::new()),
            "page" => {
                // wait for the step before to land on that page; a different page is only a warning
                let end = Instant::now() + Duration::from_secs(10);
                while !same_url(&self.page_url(), &s.url) {
                    if Instant::now() > end {
                        return Ok(format!(" (warning: the page is {})", self.page_url()));
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                self.settle(Duration::from_secs(5)).await;
                Ok(String::new())
            }
            "click" | "dblclick" | "rclick" => {
                let button = if s.kind == "rclick" { "right" } else { "" };
                let t = target();
                if t.positional() {
                    let (x, y) = self.point(&t);
                    self.click(
                        "",
                        &what(&s.target),
                        button,
                        s.kind == "dblclick",
                        Some(x),
                        Some(y),
                    )
                    .await?;
                    return Ok(String::new());
                }
                let el = self.find_target(&s.target, WAIT).await?;
                self.click(
                    &el.ref_,
                    &what(&s.target),
                    button,
                    s.kind == "dblclick",
                    None,
                    None,
                )
                .await?;
                Ok(found(&el))
            }
            "fill" => {
                let text = input.unwrap_or(&s.value);
                let el = self.find_target(&s.target, WAIT).await?;
                self.type_(&el.ref_, &what(&s.target), text, false, true)
                    .await?;
                Ok(found(&el))
            }
            "select" => {
                let values = match input {
                    Some(i) if !i.is_empty() => vec![i.clone()],
                    _ => s.values.clone(),
                };
                let el = self.find_target(&s.target, WAIT).await?;
                self.select_option(&el.ref_, &what(&s.target), &values)
                    .await?;
                Ok(found(&el))
            }
            "drag" => {
                let (mut from_ref, mut to_ref) = (String::new(), String::new());
                let (mut sx, mut sy, mut ex, mut ey) = (None, None, None, None);
                let (t, to) = (target(), s.to.clone().unwrap_or_default());
                if t.positional() {
                    let (x, y) = self.point(&t);
                    (sx, sy) = (Some(x), Some(y));
                } else {
                    from_ref = self.find_target(&s.target, WAIT).await?.ref_;
                }
                if to.positional() {
                    let (x, y) = self.point(&to);
                    (ex, ey) = (Some(x), Some(y));
                } else {
                    to_ref = self.find_target(&s.to, WAIT).await?.ref_;
                }
                self.drag(
                    &from_ref,
                    &what(&s.target),
                    &to_ref,
                    &what(&s.to),
                    sx,
                    sy,
                    ex,
                    ey,
                )
                .await?;
                Ok(String::new())
            }
            k => Err(format!("unknown step kind {}", go_quote(k))),
        }
    }
}

fn found(el: &FlowFound) -> String {
    if el.how.is_empty() || el.how == "role and name" {
        return String::new();
    }
    format!(" (found by {})", el.how)
}
