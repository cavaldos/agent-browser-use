//! The viewer's Assets tab: everything the browser produced while people and agents worked, kept
//! in one place so it can be looked at afterwards.
//!   - screenshot: a picture taken by an agent (browser_take_screenshot) or by the Capture button
//!   - copy:       text someone copied (Ctrl/Cmd+C or X) inside the shared browser
//!   - element:    an element somebody picked with the Pick tool
//!   - download:   a file the browser downloaded (stays in the download folder; only listed here)
//!
//! The list survives a restart (index.json next to the picture files). Every change is announced to
//! all viewers with an "assets" message, so their lists stay current.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::browser::Browser;
use crate::files::{safe_name, Download};
use crate::{go_json, golog, Hub, Msg};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// oldest ones are dropped beyond this
const MAX_ASSETS: usize = 200;
/// characters kept for one copied text or element
const MAX_ASSET_TEXT: usize = 100000;

fn is_empty_str(s: &str) -> bool {
    s.is_empty()
}

/// assets.rs: `type Asset`. `time` is `time.Time` (RFC 3339 with nanoseconds, local zone).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Asset {
    pub id: String,
    /// screenshot, copy, element, download
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub mime: String,
    #[serde(default)]
    pub size: i64,
    /// copy and element: the content
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub text: String,
    /// page it came from
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub url: String,
    /// title of that page
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub title: String,
    /// who made it: a person's name or "agent"
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub by: String,
    #[serde(default)]
    pub time: String,

    /// file on disk (screenshot, download); never sent to viewers
    #[serde(skip)]
    pub path: String,
    /// the file lives in the assets folder and goes away with the asset
    #[serde(skip)]
    pub owned: bool,
}

/// assetFile is an Asset as saved in index.json (it also needs the private fields).
#[derive(Serialize, Deserialize)]
struct AssetFile {
    #[serde(flatten)]
    asset: Asset,
    #[serde(default, rename = "path", skip_serializing_if = "is_empty_str")]
    path: String,
    #[serde(default, rename = "owned", skip_serializing_if = "crate::is_false")]
    owned: bool,
}

pub struct Assets {
    dir: String,
    /// oldest first
    list: Mutex<Vec<Asset>>,
    hub: Arc<Hub>,
}

/// keeps the Assets on the hub (`h.assets`); the hub state is main.rs's, so the one instance lives
/// here and `Hooks`/the routes get it from `global`.
static ASSETS: OnceLock<Arc<Assets>> = OnceLock::new();

/// The Assets of this process (None before `new_assets`).
pub fn global() -> Option<Arc<Assets>> {
    ASSETS.get().cloned()
}

/// cut: the first n characters of s.
fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// UTC offset of the local zone at `secs`, in seconds.
fn local_offset(secs: i64) -> i64 {
    #[cfg(unix)]
    // SAFETY: localtime_r only writes into the zeroed tm we pass.
    unsafe {
        let t = secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        {
            #[allow(clippy::unnecessary_cast)]
            let off = tm.tm_gmtoff as i64; // c_long
            off
        }
    }
    #[cfg(not(unix))]
    {
        let _ = secs;
        0
    }
}

/// `time.Now` marshalled by encoding/json: RFC 3339 with the fraction trimmed of trailing zeros.
pub(crate) fn now_rfc3339() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let (secs, nanos) = (d.as_secs() as i64, d.subsec_nanos());
    let (y, mo, da, h, mi, se) = crate::chat::local_tm(secs);
    let mut s = format!("{y:04}-{mo:02}-{da:02}T{h:02}:{mi:02}:{se:02}");
    if nanos > 0 {
        let frac = format!("{nanos:09}");
        s.push('.');
        s.push_str(frac.trim_end_matches('0'));
    }
    let off = local_offset(secs);
    if off == 0 {
        s.push('Z');
    } else {
        let (sign, a) = if off < 0 { ('-', -off) } else { ('+', off) };
        s.push_str(&format!("{sign}{:02}:{:02}", a / 3600, a % 3600 / 60));
    }
    s
}

/// mime.ExtensionsByType (the first extension) for the types an asset can have.
fn extension_by_type(m: &str) -> Option<&'static str> {
    Some(match m {
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/svg+xml" => ".svg",
        "image/avif" => ".avif",
        "application/pdf" => ".pdf",
        "application/json" => ".json",
        "text/plain" => ".txt",
        "text/html" => ".html",
        "text/css" => ".css",
        "text/javascript" => ".js",
        "text/xml" => ".xml",
        "application/wasm" => ".wasm",
        _ => return None,
    })
}

/// mime.TypeByExtension with built-in table (plus the most common system entries).
fn type_by_extension(ext: &str) -> String {
    match ext {
        ".avif" => "image/avif",
        ".css" => "text/css; charset=utf-8",
        ".gif" => "image/gif",
        ".htm" | ".html" => "text/html; charset=utf-8",
        ".jpeg" | ".jpg" => "image/jpeg",
        ".js" | ".mjs" => "text/javascript; charset=utf-8",
        ".json" => "application/json",
        ".pdf" => "application/pdf",
        ".png" => "image/png",
        ".svg" => "image/svg+xml",
        ".wasm" => "application/wasm",
        ".webp" => "image/webp",
        ".xml" => "text/xml; charset=utf-8",
        ".txt" => "text/plain; charset=utf-8",
        ".csv" => "text/csv; charset=utf-8",
        ".zip" => "application/zip",
        _ => "",
    }
    .to_string()
}

/// mime.FormatMediaType("attachment", {"filename": name})
fn attachment(name: &str) -> String {
    let token = |c: char| c.is_ascii_graphic() && !"()<>@,;:\\\"/[]?=".contains(c);
    if !name.is_empty() && name.chars().all(token) {
        return format!("attachment; filename={name}");
    }
    if name.is_ascii() {
        let mut q = String::new();
        for c in name.chars() {
            if c == '"' || c == '\\' {
                q.push('\\');
            }
            q.push(c);
        }
        return format!("attachment; filename=\"{q}\"");
    }
    let mut e = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            e.push(b as char);
        } else {
            e.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename*=utf-8''{e}")
}

impl Assets {
    /// newAssets
    pub fn new(dir: &str, hub: Arc<Hub>) -> Arc<Assets> {
        let a = Assets {
            dir: dir.to_string(),
            list: Mutex::new(Vec::new()),
            hub,
        };
        mkdir_0700(dir);
        if let Ok(raw) = std::fs::read(crate::go_join(dir, "index.json")) {
            if let Ok(saved) = serde_json::from_slice::<Vec<AssetFile>>(&raw) {
                let mut l = lock(&a.list);
                for s in saved {
                    let mut asset = s.asset;
                    asset.path = s.path;
                    asset.owned = s.owned;
                    l.push(asset);
                }
            }
        }
        let a = Arc::new(a);
        let _ = ASSETS.set(a.clone());
        a
    }

    /// save writes the index; the caller holds the agent lock.
    fn save(&self, list: &[Asset]) {
        let out: Vec<AssetFile> = list
            .iter()
            .map(|a| AssetFile {
                asset: a.clone(),
                path: a.path.clone(),
                owned: a.owned,
            })
            .collect();
        let Ok(raw) = serde_json::to_string(&out) else {
            return;
        };
        let tmp = crate::go_join(&self.dir, "index.json.tmp");
        if write_0600(&tmp, crate::go_escape(raw).as_bytes()).is_ok() {
            let _ = std::fs::rename(&tmp, crate::go_join(&self.dir, "index.json"));
        }
    }

    /// add records an asset. data is the content of a file the assets folder should keep (a picture);
    /// without it, as.path may name an existing file (a download) or the asset is only text.
    pub fn add(&self, mut asset: Asset, data: Option<&[u8]>) -> Option<Asset> {
        let id = crate::new_id();
        asset.id = format!("as-{}", &id[..id.len().min(6)]);
        asset.time = now_rfc3339();
        asset.text = cut(&asset.text, MAX_ASSET_TEXT);
        if let Some(data) = data {
            let mut ext = extension_by_type(&asset.mime).unwrap_or(".bin");
            if asset.mime == "image/jpeg" {
                ext = ".jpg";
            }
            let file = crate::go_join(&self.dir, &format!("{}{ext}", asset.id));
            if write_0600(&file, data).is_err() {
                return None;
            }
            asset.path = file;
            asset.owned = true;
            asset.size = data.len() as i64;
        }
        {
            let mut list = lock(&self.list);
            // Copying the same text twice in a row is one asset, not two.
            if asset.kind == "copy" {
                if let Some(last) = list.last() {
                    if last.kind == "copy" && last.text == asset.text {
                        return Some(last.clone());
                    }
                }
            }
            list.push(asset.clone());
            while list.len() > MAX_ASSETS {
                Self::drop_file(&list[0]);
                list.remove(0);
            }
            self.save(&list);
        }
        self.hub.assets_changed(Some(&asset));
        Some(asset)
    }

    /// drop removes the file of an asset the assets folder owns.
    fn drop_file(a: &Asset) {
        if a.owned && !a.path.is_empty() {
            let _ = std::fs::remove_file(&a.path);
        }
    }

    pub fn get(&self, id: &str) -> Option<Asset> {
        lock(&self.list).iter().find(|a| a.id == id).cloned()
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut list = lock(&self.list);
        if let Some(i) = list.iter().position(|a| a.id == id) {
            Self::drop_file(&list[i]);
            list.remove(i);
            self.save(&list);
            return true;
        }
        false
    }

    pub fn clear(&self) {
        let mut list = lock(&self.list);
        for a in list.iter() {
            Self::drop_file(a);
        }
        list.clear();
        self.save(&list);
    }

    /// snapshot is the list, newest first.
    pub fn snapshot(&self) -> Vec<Asset> {
        lock(&self.list).iter().rev().cloned().collect()
    }

    /// addDownload lists a finished download. The file stays where Chrome saved it.
    /// (`BrowserHooks::download_done` calls this through `add_download`.)
    pub fn add_download(&self, dl: &Download) {
        let size = std::fs::metadata(&dl.path).map_or(0, |m| m.len() as i64);
        let ext = match dl.name.rfind('.') {
            Some(i) => dl.name[i..].to_lowercase(),
            None => String::new(),
        };
        self.add(
            Asset {
                kind: "download".into(),
                name: dl.name.clone(),
                mime: type_by_extension(&ext),
                size,
                url: dl.url.clone(),
                by: "browser".into(),
                text: dl.path.clone(),
                path: dl.path.clone(),
                ..Default::default()
            },
            None,
        );
    }
}

/// `h.assets.addDownload(d)` for whoever implements `BrowserHooks::download_done`.
pub fn add_download(d: &Download) {
    if let Some(a) = global() {
        a.add_download(d);
    }
}

fn mkdir_0700(dir: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::create_dir_all(dir);
    }
}

fn write_0600(path: &str, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)?.write_all(data)
}

impl Hub {
    /// assetsChanged tells every viewer to reload the Assets tab. as is the new asset, if there is one.
    pub fn assets_changed(&self, a: Option<&Asset>) {
        let st = self.lock();
        let mut m = Msg {
            type_: "assets".into(),
            ..Default::default()
        };
        if let Some(a) = a {
            m.text = a.kind.clone();
            m.id = a.id.clone();
        }
        st.broadcast(&m, None);
    }
}

/// Capture takes a picture of what is on screen now (the viewer's Capture button).
pub async fn capture(b: &Arc<Browser>) -> Result<Vec<u8>, String> {
    let t = b.st().active.clone();
    let Some(t) = t else {
        return Err("no tab is open".into());
    };
    b.shot(&t).await
}

/// `shotName`
pub fn shot_name() -> String {
    let (_, _, _, h, m, s) = crate::chat::local_tm(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64),
    );
    format!("screenshot-{h:02}{m:02}{s:02}.jpg")
}

// ---------- /api/assets* ----------

#[derive(Clone)]
struct ApiState {
    hub: Arc<Hub>,
    assets: Arc<Assets>,
    browser: Option<Arc<Browser>>,
}

fn http_error(status: StatusCode, msg: &str) -> Response {
    // http.Error
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header("x-content-type-options", "nosniff")
        .body(Body::from(format!("{msg}\n")))
        .unwrap()
}

fn json_response(body: String) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!("{body}\n")))
        .unwrap()
}

fn post_only(r: &Request) -> Option<Response> {
    if r.method() != Method::POST {
        return Some(http_error(StatusCode::METHOD_NOT_ALLOWED, "POST only"));
    }
    None
}

/// registerAssetAPI
pub fn routes(hub: Arc<Hub>, assets: Arc<Assets>, browser: Option<Arc<Browser>>) -> Router {
    let st = ApiState {
        hub,
        assets,
        browser,
    };
    Router::new()
        .route("/api/assets", any(api_list))
        .route("/api/assets/file", any(api_file))
        .route("/api/assets/delete", any(api_delete))
        .route("/api/assets/clear", any(api_clear))
        .route("/api/assets/capture", any(api_capture))
        .with_state(st)
}

/// GET /api/assets -> { assets: [...] } newest first
async fn api_list(State(st): State<ApiState>) -> Response {
    #[derive(Serialize)]
    struct List {
        assets: Vec<Asset>,
    }
    json_response(go_json(&List {
        assets: st.assets.snapshot(),
    }))
}

/// GET /api/assets/file?id=…[&download=1] -> the picture, the file, or the text of the asset
async fn api_file(
    State(st): State<ApiState>,
    Query(q): Query<HashMap<String, String>>,
    req: Request,
) -> Response {
    let Some(a) = st.assets.get(q.get("id").map_or("", |s| s.as_str())) else {
        return http_error(StatusCode::NOT_FOUND, "no such asset");
    };
    let mut name = safe_name(&a.name);
    if name.is_empty() {
        name = a.id.clone();
    }
    let download = q.get("download").is_some_and(|d| !d.is_empty());
    let mut b = Response::builder().header("x-content-type-options", "nosniff");
    let ext_of = |n: &str| -> String {
        // filepath.Ext
        match n.rfind('.') {
            Some(i) if !n[i..].contains('/') => n[i..].to_string(),
            _ => String::new(),
        }
    };
    if download {
        b = b.header(header::CONTENT_DISPOSITION, attachment(&name));
    }
    if a.path.is_empty() {
        // text
        if ext_of(&name).is_empty() {
            if a.kind == "element" {
                name.push_str(".html");
            }
            if ext_of(&name).is_empty() {
                name.push_str(".txt");
            }
        }
        let mut r = b
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(a.text.clone()))
            .unwrap();
        if download {
            r.headers_mut().insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&attachment(&name)).unwrap(),
            );
        }
        return r;
    }
    // Only pictures are shown inline: a downloaded page must never run on our origin.
    if a.kind != "screenshot" {
        b = b
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_DISPOSITION, attachment(&name));
    } else {
        b = b.header(
            header::CONTENT_TYPE,
            HeaderValue::from_str(&a.mime).unwrap_or(HeaderValue::from_static("image/jpeg")),
        );
    }
    let data = match std::fs::read(&a.path) {
        Ok(d) => d,
        Err(_) => return http_error(StatusCode::GONE, "the file is gone"),
    };
    let size = data.len();
    let mtime = std::fs::metadata(&a.path).and_then(|m| m.modified()).ok();
    let mut b = b.header(header::ACCEPT_RANGES, "bytes");
    if let Some(m) = mtime {
        b = b.header(header::LAST_MODIFIED, http_date(m));
    }
    // a single "bytes=a-b" range, like http.ServeContent
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.split_once('-'))
        .and_then(|(s, e)| {
            if s.is_empty() {
                let n: usize = e.parse().ok()?;
                Some((size.saturating_sub(n), size))
            } else {
                let s: usize = s.parse().ok()?;
                let e = if e.is_empty() {
                    size
                } else {
                    (e.parse::<usize>().ok()? + 1).min(size)
                };
                (s < e).then_some((s, e))
            }
        });
    let head = req.method() == Method::HEAD;
    let (status, body) = match range {
        Some((s, e)) => {
            b = b.header(header::CONTENT_RANGE, format!("bytes {s}-{}/{size}", e - 1));
            (StatusCode::PARTIAL_CONTENT, data[s..e].to_vec())
        }
        None => (StatusCode::OK, data),
    };
    b = b.header(header::CONTENT_LENGTH, body.len());
    b.status(status)
        .body(if head {
            Body::empty()
        } else {
            Body::from(body)
        })
        .unwrap()
}

/// RFC 7231 date (http.TimeFormat), UTC.
fn http_date(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MO: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{}, {d:02} {} {y:04} {:02}:{:02}:{:02} GMT",
        WD[days.rem_euclid(7) as usize],
        MO[(m - 1) as usize],
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// POST /api/assets/delete?id=…
async fn api_delete(
    State(st): State<ApiState>,
    Query(q): Query<HashMap<String, String>>,
    req: Request,
) -> Response {
    if let Some(r) = post_only(&req) {
        return r;
    }
    if !st.assets.remove(q.get("id").map_or("", |s| s.as_str())) {
        return http_error(StatusCode::NOT_FOUND, "no such asset");
    }
    st.hub.assets_changed(None);
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap()
}

/// POST /api/assets/clear
async fn api_clear(State(st): State<ApiState>, req: Request) -> Response {
    if let Some(r) = post_only(&req) {
        return r;
    }
    st.assets.clear();
    st.hub.assets_changed(None);
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap()
}

/// POST /api/assets/capture?by=name -> takes a screenshot of the shared screen now
async fn api_capture(
    State(st): State<ApiState>,
    Query(q): Query<HashMap<String, String>>,
    req: Request,
) -> Response {
    if let Some(r) = post_only(&req) {
        return r;
    }
    let Some(b) = &st.browser else {
        return http_error(StatusCode::SERVICE_UNAVAILABLE, "no browser is running");
    };
    let data = match capture(b).await {
        Ok(d) => d,
        Err(e) => return http_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let (u, title) = crate::browser::BrowserApi::page_of(&**b);
    let by = cut(q.get("by").map_or("", |s| s.as_str()), 40)
        .trim()
        .to_string();
    let a = st.assets.add(
        Asset {
            kind: "screenshot".into(),
            name: shot_name(),
            mime: "image/jpeg".into(),
            url: u,
            title,
            by,
            ..Default::default()
        },
        Some(&data),
    );
    json_response(match a {
        Some(a) => go_json(&a),
        None => "null".into(),
    })
}

/// A copy event from the page (`HubHooks::asset_copy`): adds it right in `Hub.handle`.
pub fn add_copy(text: String, url: String, title: String, by: String) {
    if let Some(a) = global() {
        let size = text.len() as i64;
        a.add(
            Asset {
                kind: "copy".into(),
                name: "copied-text.txt".into(),
                mime: "text/plain".into(),
                size,
                text,
                url,
                title,
                by,
                ..Default::default()
            },
            None,
        );
    } else {
        golog("assets: not initialised");
    }
}

/// The `BrowserHooks` of this module: `h.assets.addDownload(d)` when a download finishes.
pub struct DownloadHooks;

impl crate::browser::BrowserHooks for DownloadHooks {
    fn download_done(&self, d: &Download) {
        add_download(d);
    }
}
