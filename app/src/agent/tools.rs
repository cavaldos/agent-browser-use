//! Agent actions for the extra MCP tools: console and network inspection, downloads and
//! uploads, cookies and storage, saved sessions, snapshot diffs and device emulation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::{cmp_or, diff_outline, eval_js, quote, Agent};
use crate::browser::BResult;
use crate::emulate::{preset_names, EmulateOpts};
use crate::files::{safe_name, upload_paths};
use crate::frames::jsq;
use crate::observe::{human_size, observer_of, one_line, NetFilter, Observer};

// ---------- Console and network ----------

impl Agent {
    /// activeObs is the observer of the tab the agent is looking at.
    pub fn active_obs(&self) -> Option<Arc<Observer>> {
        let st = self.b.st();
        let t = if !self.tab_id.is_empty() {
            st.tabs.iter().find(|t| t.id == self.tab_id)?.clone()
        } else {
            st.active.clone()?
        };
        observer_of(&t)
    }

    pub async fn console(
        &self,
        level: &str,
        limit: i64,
        clear: bool,
        history: bool,
    ) -> BResult<String> {
        let Some(o) = self.active_obs() else {
            return Err("no tab is open".into());
        };
        self.announce("🪵", "Read the console", None);
        Ok(o.console_report(level, limit, clear, history))
    }

    /// Network lists requests, or one request in detail (with its body) when id > 0.
    pub async fn network(&self, f: NetFilter, id: i64, body: bool, clear: bool) -> BResult<String> {
        let Some(o) = self.active_obs() else {
            return Err("no tab is open".into());
        };
        if id <= 0 {
            self.announce("📡", "Check network requests", None);
            return Ok(o.network_report(&f, clear));
        }
        let Some(n) = o.find(id) else {
            return Err(format!(
                "no request with id {id} (it may have been cleared; list requests first)"
            ));
        };
        self.announce("📡", &format!("Inspect request #{id}"), None);
        let out = n.detail();
        if !body {
            return Ok(out);
        }
        if !n.has_body {
            return Ok(format!(
                "{out}\nNo response body (the request is not finished, failed, or is a redirect).\n"
            ));
        }
        let req_id = n.req_id.clone();
        let res = self
            .look(move |t| async move {
                let r = t
                    .call(
                        "Network.getResponseBody",
                        json!({"requestId": req_id}),
                        None,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                let b = r["body"].as_str().unwrap_or_default();
                if r["base64Encoded"].as_bool().unwrap_or(false) {
                    base64::engine::general_purpose::STANDARD
                        .decode(b)
                        .map_err(|e| e.to_string())
                } else {
                    Ok(b.as_bytes().to_vec())
                }
            })
            .await;
        let data = match res {
            Ok(d) => d,
            Err(e) => return Ok(format!("{out}\nResponse body not available: {e}\n")),
        };
        let Ok(mut text) = String::from_utf8(data.clone()) else {
            return Ok(format!(
                "{out}\nResponse body is binary ({}).\n",
                human_size(data.len() as i64)
            ));
        };
        if text.len() > 20000 {
            let mut n = 20000;
            while !text.is_char_boundary(n) {
                n -= 1;
            }
            text.truncate(n);
            text.push_str("\n… (cut)");
        }
        Ok(format!("{out}\nResponse body:\n```\n{text}\n```\n"))
    }

    /// Alerts describes new console errors and failed requests, to append to a tool result.
    pub fn alerts(&self) -> String {
        let Some(o) = self.active_obs() else {
            return String::new();
        };
        let (s, n) = o.alerts();
        if n > 0 {
            self.hub.agent_log(&format!(
                "⚠️ {n} problem(s) on the page (console errors or failed requests)"
            ));
        }
        s
    }
}

// ---------- Downloads and uploads ----------

impl Agent {
    fn download_list(&self) -> String {
        let dls = self.b.dl.snapshot();
        self.st().dl_seen = dls.len();
        let mut s = format!("### Downloads (folder: {})\n", self.b.opt.download_dir);
        if dls.is_empty() {
            s.push_str("No downloads yet.\n");
        }
        for (i, d) in dls.iter().enumerate() {
            s.push_str(&d.line(i + 1));
            s.push('\n');
        }
        s
    }

    pub async fn downloads(&self, action: &str, name: &str, mut seconds: f64) -> BResult<String> {
        match action {
            "" | "list" => Ok(self.download_list()),
            "wait" => {
                if seconds <= 0.0 {
                    seconds = 30.0;
                }
                seconds = seconds.min(120.0);
                self.announce("⏳", "Wait for a download", None);
                let end = Instant::now() + Duration::from_secs_f64(seconds);
                while Instant::now() < end {
                    let dls = self.b.dl.snapshot();
                    let active = dls.iter().any(|d| d.state == "inProgress");
                    if !active && dls.len() > self.st().dl_seen {
                        // something new finished (maybe already, while the click was settling)
                        return Ok(self.download_list());
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(format!("no download finished within {seconds:.0}s. If the page should have started one, check the click; current state:\n{}", self.download_list()))
            }
            "read" => {
                let path = self.download_file(name)?;
                let data = std::fs::read(&path).map_err(|e| e.to_string())?;
                let base = path.rsplit('/').next().unwrap_or(&path).to_string();
                self.announce(
                    "📄",
                    &format!("Read downloaded file {}", quote(&base)),
                    None,
                );
                match String::from_utf8(data) {
                    Ok(t) if t.len() <= 60000 => Ok(t),
                    Ok(t) => Ok(format!("{base} is {} and not plain text (or too big to show). It is saved at {path}", human_size(t.len() as i64))),
                    Err(e) => Ok(format!("{base} is {} and not plain text (or too big to show). It is saved at {path}", human_size(e.as_bytes().len() as i64))),
                }
            }
            _ => Err(format!("action must be list, wait or read, not {action:?}")),
        }
    }

    /// downloadFile finds a downloaded file by name (the newest one when name is empty).
    fn download_file(&self, name: &str) -> BResult<String> {
        let dir = &self.b.opt.download_dir;
        if name.is_empty() {
            let dls = self.b.dl.snapshot();
            for d in dls.iter().rev() {
                if d.state == "completed" {
                    return Ok(d.path.clone());
                }
            }
            return Err("no finished download yet".into());
        }
        let n = safe_name(name);
        if n.is_empty() {
            return Err(format!("invalid file name {name:?}"));
        }
        let p = crate::go_join(dir, &n);
        if std::fs::metadata(&p).is_err() {
            return Err(format!("{n:?} is not in the download folder {dir}"));
        }
        Ok(p)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UploadFile {
    #[serde(default)]
    pub name: String,
    /// base64
    #[serde(default)]
    pub data: String,
}

impl Agent {
    /// Upload gives files to a <input type=file>. ref can be the input or something that belongs to it (button, label).
    pub async fn upload(
        &self,
        r: &str,
        element: &str,
        mut paths: Vec<String>,
        files: &[UploadFile],
    ) -> BResult<String> {
        if r.is_empty() {
            return Err(
                "ref is required: the file input (or its button/label) from a snapshot".into(),
            );
        }
        for f in files {
            // files sent inline are stored in the upload folder first
            let n = safe_name(&f.name);
            let data = base64::engine::general_purpose::STANDARD.decode(&f.data);
            let (true, Ok(data)) = (!n.is_empty(), data) else {
                return Err(format!(
                    "files: {:?} needs a plain file name and base64 data",
                    f.name
                ));
            };
            if data.len() > 20 << 20 {
                return Err(format!("files: {n:?} is bigger than 20 MB"));
            }
            write_0600(&crate::go_join(&self.b.opt.upload_dir, &n), &data)
                .map_err(|e| e.to_string())?;
            paths.push(n);
        }
        let abs = upload_paths(&self.b, &paths)?;
        let names: Vec<String> = abs
            .iter()
            .map(|p| p.rsplit('/').next().unwrap_or(p).to_string())
            .collect();
        self.announce(
            "📎",
            &format!(
                "Upload {} to {}",
                quote(&names.join(", ")),
                if element.is_empty() {
                    r.to_string()
                } else {
                    quote(element)
                }
            ),
            None,
        );
        let rr = r.to_string();
        self.act(move |t| async move {
            let expr = format!(
                "{}\n;__abu.fileInput({})",
                crate::agent::agent_js(),
                jsq(&rr)
            );
            let res = t
                .call("Runtime.evaluate", json!({"expression": expr}), None)
                .await
                .map_err(|e| e.to_string())?;
            if let Some(d) = res.get("exceptionDetails") {
                return Err(crate::agent::exception_error(d));
            }
            let oid = res
                .pointer("/result/objectId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if oid.is_empty() {
                return Err("could not find the file input".into());
            }
            t.call(
                "DOM.setFileInputFiles",
                json!({"files": abs, "objectId": oid}),
                None,
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
        .await?;
        self.settle(Duration::from_secs(3)).await;
        Ok(format!("Uploaded {}", names.join(", ")))
    }

    /// Add files/images to the Assets tab (and the upload folder). Useful for sending an image from a
    /// coding agent to the shared browser so people can see it or drag it into the page.
    pub async fn add_asset(&self, mut paths: Vec<String>, files: &[UploadFile]) -> BResult<String> {
        for f in files {
            let n = safe_name(&f.name);
            let data = base64::engine::general_purpose::STANDARD.decode(&f.data);
            let (true, Ok(data)) = (!n.is_empty(), data) else {
                return Err(format!(
                    "files: {:?} needs a plain file name and base64 data",
                    f.name
                ));
            };
            if data.len() > 20 << 20 {
                return Err(format!("files: {n:?} is bigger than 20 MB"));
            }
            write_0600(&crate::go_join(&self.b.opt.upload_dir, &n), &data)
                .map_err(|e| e.to_string())?;
            paths.push(n);
        }
        let abs = upload_paths(&self.b, &paths)?;
        let mut out: Vec<String> = Vec::new();
        if let Some(a) = crate::assets::global() {
            for p in &abs {
                let data = std::fs::read(p).map_err(|e| e.to_string())?;
                let name = p.rsplit('/').next().unwrap_or(p).to_string();
                let ext = match name.rfind('.') {
                    Some(i) => &name[i..],
                    None => "",
                };
                let mime = crate::assets::mime_by_extension(ext);
                let mime = if mime.is_empty() {
                    "application/octet-stream".to_string()
                } else {
                    mime
                };
                let kind = if mime.starts_with("image/") {
                    "image"
                } else {
                    "file"
                };
                let asset = a.add(
                    crate::assets::Asset {
                        kind: kind.into(),
                        name: name.clone(),
                        mime,
                        by: "agent".into(),
                        ..Default::default()
                    },
                    Some(&data),
                );
                if let Some(asset) = asset {
                    out.push(format!("{} ({})", asset.id, name));
                }
            }
        } else {
            return Err("assets are not initialised".into());
        }
        if out.is_empty() {
            return Err("no assets added".into());
        }
        Ok(format!("Added to Assets: {}", out.join(", ")))
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

// ---------- Cookies and storage ----------

impl Agent {
    /// pageURL is the address of the tab the agent is on.
    pub fn page_url(&self) -> String {
        let (tabs, mut active, _, _) = self.b.state();
        if !self.tab_id.is_empty() {
            active = self.tab_id.clone();
        }
        tabs.iter()
            .find(|t| t.id == active)
            .map(|t| t.url.clone())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default)]
pub struct CookieArgs {
    pub action: String,
    pub url: String,
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub same_site: String,
    pub secure: Option<bool>,
    pub http_only: Option<bool>,
    pub expires: f64,
    pub all: bool,
    pub full: bool,
}

/// `network.Cookie`, the parts the tools read.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Cookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    expires: f64,
    #[serde(rename = "httpOnly")]
    http_only: bool,
    secure: bool,
    session: bool,
    #[serde(rename = "sameSite")]
    same_site: String,
}

fn cookie_line(c: &Cookie, full: bool) -> String {
    let mut v = c.value.clone();
    if !full && v.chars().count() > 60 {
        v = v.chars().take(60).collect::<String>() + "…";
    }
    let mut flags = Vec::new();
    if c.secure {
        flags.push("Secure".to_string());
    }
    if c.http_only {
        flags.push("HttpOnly".to_string());
    }
    if !c.same_site.is_empty() {
        flags.push(format!("SameSite={}", c.same_site));
    }
    let mut exp = "session".to_string();
    if !c.session && c.expires > 0.0 {
        let (y, mo, d, h, mi, _) = crate::chat::local_tm(c.expires as i64);
        exp = format!("expires {y:04}-{mo:02}-{d:02} {h:02}:{mi:02}");
    }
    let mut info = vec![format!("{}{}", c.domain, c.path)];
    info.extend(flags);
    format!("- {}={}  ({}; {})", c.name, v, info.join(" "), exp)
}

fn cookies_of(v: &Value) -> Vec<Cookie> {
    serde_json::from_value(v["cookies"].clone()).unwrap_or_default()
}

impl Agent {
    async fn all_cookies(&self) -> BResult<Vec<Cookie>> {
        let r = self
            .b
            .root_call("Storage.getCookies", json!({}), None)
            .await
            .map_err(|e| e.to_string())?;
        Ok(cookies_of(&r))
    }

    pub async fn cookies(&self, c: CookieArgs) -> BResult<String> {
        let cur = self.page_url();
        match c.action.as_str() {
            "" | "list" => {
                self.announce("🍪", "Read cookies", None);
                let mut cookies;
                if c.all {
                    cookies = self.all_cookies().await?;
                } else {
                    let u = cmp_or(&c.url, &cur);
                    if !u.starts_with("http") {
                        return Err("open a web page first, or pass url / all: true".into());
                    }
                    cookies = self
                        .look(move |t| async move {
                            let r = t
                                .call("Network.getCookies", json!({"urls": [u]}), None)
                                .await
                                .map_err(|e| e.to_string())?;
                            Ok(cookies_of(&r))
                        })
                        .await?;
                }
                if !c.name.is_empty() {
                    cookies.retain(|x| x.name == c.name);
                }
                cookies.sort_by(|x, y| {
                    format!("{}{}", x.domain, x.name).cmp(&format!("{}{}", y.domain, y.name))
                });
                let mut s = format!("### Cookies ({})\n", cookies.len());
                for k in &cookies {
                    s.push_str(&cookie_line(k, c.full));
                    s.push('\n');
                }
                Ok(s)
            }
            "set" => {
                if c.name.is_empty() {
                    return Err("name is required".into());
                }
                let mut p = json!({"name": c.name, "value": c.value});
                if !c.domain.is_empty() {
                    p["domain"] = json!(c.domain);
                }
                if !c.path.is_empty() {
                    p["path"] = json!(c.path);
                }
                if let Some(b) = c.secure {
                    p["secure"] = json!(b);
                }
                if let Some(b) = c.http_only {
                    p["httpOnly"] = json!(b);
                }
                if c.domain.is_empty() {
                    let u = cmp_or(&c.url, &cur);
                    if !u.starts_with("http") {
                        return Err("open a web page first, or pass url or domain".into());
                    }
                    p["url"] = json!(u);
                }
                match c.same_site.to_lowercase().as_str() {
                    "" => {}
                    "strict" => p["sameSite"] = json!("Strict"),
                    "lax" => p["sameSite"] = json!("Lax"),
                    "none" => p["sameSite"] = json!("None"),
                    _ => {
                        return Err(format!(
                            "sameSite must be Strict, Lax or None, not {:?}",
                            c.same_site
                        ))
                    }
                }
                if c.expires > 0.0 {
                    p["expires"] = json!(c.expires);
                }
                self.announce("🍪", &format!("Set cookie {}", quote(&c.name)), None);
                self.act(move |t| async move {
                    t.call("Network.setCookie", p, None)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                })
                .await?;
                Ok(format!("Cookie {} set.", c.name))
            }
            "delete" => {
                if c.name.is_empty() {
                    return Err("name is required (use action clear to remove many)".into());
                }
                let mut p = json!({"name": c.name});
                if !c.domain.is_empty() {
                    p["domain"] = json!(c.domain);
                }
                if !c.path.is_empty() {
                    p["path"] = json!(c.path);
                }
                if c.domain.is_empty() {
                    let u = cmp_or(&c.url, &cur);
                    if !u.is_empty() {
                        p["url"] = json!(u);
                    }
                }
                self.announce("🍪", &format!("Delete cookie {}", quote(&c.name)), None);
                self.act(move |t| async move {
                    t.call("Network.deleteCookies", p, None)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                })
                .await?;
                Ok(format!("Cookie {} deleted.", c.name))
            }
            "clear" => {
                if !c.domain.is_empty() {
                    self.announce("🍪", &format!("Clear cookies of {}", c.domain), None);
                } else {
                    self.announce("🍪", "Clear all cookies", None);
                }
                if c.domain.is_empty() {
                    self.act(|t| async move {
                        t.call("Network.clearBrowserCookies", json!({}), None)
                            .await
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    })
                    .await?;
                    return Ok("All cookies cleared. You are signed out everywhere.".into());
                }
                let cookies = self.all_cookies().await?;
                let want = c.domain.to_lowercase();
                let want = want.trim_start_matches('.').to_string();
                let res = self
                    .act(move |t| async move {
                        let mut n = 0;
                        for k in &cookies {
                            let d = k.domain.to_lowercase();
                            let d = d.trim_start_matches('.');
                            if d == want || d.ends_with(&format!(".{want}")) {
                                t.call(
                                    "Network.deleteCookies",
                                    json!({"name": k.name, "domain": k.domain, "path": k.path}),
                                    None,
                                )
                                .await
                                .map_err(|e| e.to_string())?;
                                n += 1;
                            }
                        }
                        Ok(n)
                    })
                    .await?;
                Ok(format!("{res} cookie(s) of {} cleared.", c.domain))
            }
            _ => Err(format!(
                "action must be list, set, delete or clear, not {:?}",
                c.action
            )),
        }
    }

    /// Storage reads and writes localStorage / sessionStorage of the current page.
    pub async fn storage(
        &self,
        action: &str,
        kind: &str,
        key: &str,
        value: &str,
    ) -> BResult<String> {
        let area = match kind.to_lowercase().as_str() {
            "" | "local" => "localStorage",
            "session" => "sessionStorage",
            _ => return Err(format!("type must be local or session, not {kind:?}")),
        };
        let expr = match action {
            "" | "list" => format!(
                "JSON.stringify(Object.fromEntries(Object.keys({area}).map(k => [k, {area}.getItem(k)])))"
            ),
            "get" => format!("{area}.getItem({})", jsq(key)),
            "set" => {
                if key.is_empty() {
                    return Err("key is required".into());
                }
                format!("({area}.setItem({}, {}), 'ok')", jsq(key), jsq(value))
            }
            "remove" => format!("({area}.removeItem({}), 'ok')", jsq(key)),
            "clear" => format!("({area}.clear(), 'ok')"),
            _ => {
                return Err(format!(
                    "action must be list, get, set, remove or clear, not {action:?}"
                ))
            }
        };
        if (action == "get" || action == "remove") && key.is_empty() {
            return Err("key is required".into());
        }
        self.announce("🗄️", &format!("{} {area}", cmp_or(action, "list")), None);
        let out: Value = self
            .act(move |t| async move { eval_js(&t, &expr).await })
            .await?;
        // fmt.Sprint of the value
        let sprint = |v: &Value| match v {
            Value::String(s) => s.clone(),
            Value::Null => "<nil>".to_string(),
            v => v.to_string(),
        };
        match action {
            "" | "list" => {
                let m: HashMap<String, String> =
                    serde_json::from_str(&sprint(&out)).unwrap_or_default();
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut s = format!("### {area} of {} ({})\n", self.page_url(), keys.len());
                for k in keys {
                    s.push_str(&format!("- {k} = {}\n", one_line(&m[k], 200)));
                }
                Ok(s)
            }
            "get" => {
                if out.is_null() {
                    return Ok("(no such key)".into());
                }
                Ok(sprint(&out))
            }
            _ => Ok(sprint(&out)),
        }
    }
}

// ---------- Snapshot diff ----------

/// nameRe: `^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$`
fn name_ok(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

impl Agent {
    /// SnapshotDiff shows what changed on the page compared with an earlier snapshot: the last one the
    /// agent saw (baseline "last"), or one saved under a name with save. save keeps the current page as a new baseline.
    pub async fn snapshot_diff(&self, baseline: &str, save: &str) -> BResult<String> {
        let cur: String = self
            .look(
                |t| async move { crate::agent::eval_js_as::<String>(&t, "__abu.snapshot()").await },
            )
            .await?;
        let url = self.page_url();
        let (prev, prev_url, label): (String, String, String);
        {
            let st = self.st();
            if baseline.is_empty() || baseline == "last" {
                if st.last.is_empty() {
                    return Err("there is no earlier snapshot to compare with yet: call browser_snapshot first, or use save to name a baseline".into());
                }
                prev = st.last.clone();
                prev_url = st
                    .last_key
                    .split_once(' ')
                    .map(|(_, u)| u.to_string())
                    .unwrap_or_default();
                label = "the last snapshot".to_string();
            } else {
                let Some(m) = st.marks.get(baseline) else {
                    let mut names: Vec<&String> = st.marks.keys().collect();
                    names.sort();
                    let names: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
                    return Err(format!(
                        "no baseline called {baseline:?}. Saved baselines: {}",
                        cmp_or(&names.join(", "), "none (create one with save)")
                    ));
                };
                prev = m.snap.clone();
                prev_url = m.url.clone();
                label = format!("baseline {}", quote(baseline));
            }
        }

        let mut s = format!("### Snapshot diff against {label}\n");
        if !prev_url.is_empty() && prev_url != url {
            s.push_str(&format!("- URL changed: {prev_url} → {url}\n"));
        }
        self.announce("🔍", &format!("Compare the page with {label}"), None);
        match diff_outline(&prev, &cur, true) {
            Some(d) => s.push_str(&d),
            None => s.push_str(&format!(
                "Too much changed to list ({} lines before, {} now). Call browser_snapshot for the whole page.\n",
                prev.matches('\n').count() + 1,
                cur.matches('\n').count() + 1
            )),
        }
        let (tabs, active, _, _) = self.b.state();
        let mut key = String::new();
        for t in &tabs {
            if t.id == active {
                key = format!("{} {}", t.id, t.url);
            }
        }
        let mut st = self.st();
        st.last = cur.clone();
        st.last_key = key; // the agent has now seen this page
        if !save.is_empty() {
            if !name_ok(save) {
                return Err("save: use letters, digits, - _ . only".into());
            }
            if !st.marks.contains_key(save) && st.marks.len() >= 20 {
                return Err("too many saved baselines (20): reuse a name".into());
            }
            st.marks
                .insert(save.to_string(), crate::agent::Mark { snap: cur, url });
            s.push_str(&format!(
                "\nSaved the current page as baseline {}.\n",
                quote(save)
            ));
        }
        Ok(s)
    }
}

// ---------- Device emulation ----------

impl Agent {
    pub async fn emulate(&self, o: EmulateOpts) -> BResult<String> {
        if o.is_empty() || o.device.eq_ignore_ascii_case("list") {
            return Ok(format!(
                "{}\nDevices: {}\n",
                self.b.emulation(),
                preset_names()
            ));
        }
        self.hub.agent_take()?;
        let mut label = cmp_or(&o.device, "custom settings");
        if o.device.eq_ignore_ascii_case("reset") {
            label = "default desktop".into();
        }
        self.announce("📱", &format!("Emulate {label}"), None);
        let prev = self.b.emulation();
        let e = self.b.set_emulation(o).await?;
        let mut note = "";
        if e.width != prev.width
            || e.height != prev.height
            || e.mobile != prev.mobile
            || e.user_agent != prev.user_agent
            || e.scale != prev.scale
            || e.locale != prev.locale
        {
            let u = self.page_url();
            if u.starts_with("http") {
                let r = self
                    .look(|t| async move {
                        t.call("Page.reload", json!({}), None)
                            .await
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    })
                    .await;
                if r.is_ok() {
                    self.settle(Duration::from_secs(15)).await;
                    note = "The page was reloaded so the new device takes effect.\n";
                }
            }
        }
        Ok(format!("{e}\n{note}"))
    }
}

// ---------- Phase 4: flows and saved sessions ----------

/// normFlowID (flow.rs): the id as people write it ("@flow-3fa9c1 " -> "flow-3fa9c1").
pub fn norm_flow_id(id: &str) -> String {
    id.trim().trim_start_matches('@').trim().to_lowercase()
}
