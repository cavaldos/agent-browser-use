//! What the page is doing behind the scenes: console messages, page errors and network
//! requests of every tab. Chrome reports them as events; they are kept in small ring
//! buffers so the agent (MCP tools browser_console_messages / browser_network_requests)
//! and the people watching can see why a page misbehaves.
//!
//! # Public API (what tools.rs / agent.rs call)
//!
//! Each tab owns one [`Observer`], created by [`Observe::tab_created`] and fed by
//! [`Observe::tab_event`]; [`Observe`] is a `BrowserHooks` (see `browser::HookSet` for combining it
//! with the other hook modules; main.rs installs it). Get a tab's observer with
//! [`observer_of`]`(&tab)`.
//!
//!   * `observer.console_report(level, limit, clear, history) -> String`: `consoleReport`
//!     (`level` = "all" | "debug" | "info" | "warning" | "error", "" = info; `limit` <= 0 = 100).
//!   * `observer.network_report(&NetFilter, clear) -> String`: `networkReport`
//!     (`NetFilter { match_, method, failed_only, static_, history, limit }`, limit <= 0 = 60).
//!   * `observer.find(id) -> Option<NetEntry>` and `NetEntry::detail -> String`: one request
//!     with its headers (browser_network_requests with `id`); `NetEntry::line` is the one-line form.
//!     The response body is not kept: ask Chrome with `Network.getResponseBody` using `NetEntry::req_id`
//!     (only when `has_body`).
//!   * `observer.alerts -> (String, usize)`: problems since the last time the agent was told
//!     (empty string and 0 when everything is fine); `observer.mark_seen` forgets them.
//!   * `human_size(bytes)`, `one_line(text, max)`, `short_url(url)`: the helpers of the same name.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{Map, Value};

use crate::browser::{BrowserHooks, Tab};
use crate::cdp::Event;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

const MAX_CONSOLE: usize = 500;
const MAX_NET: usize = 1000;

#[derive(Debug, Clone)]
pub struct ConsoleEntry {
    pub id: i64,
    pub time: SystemTime,
    /// error, warning, info, debug
    pub level: String,
    pub text: String,
    /// url:line, when known
    pub where_: String,
}

#[derive(Debug, Clone)]
pub struct NetEntry {
    pub id: i64,
    /// `reqID` (network.RequestID)
    pub req_id: String,
    pub time: SystemTime,
    started: Instant,
    pub method: String,
    pub url: String,
    /// Document, XHR, Fetch, Script, Image…
    pub type_: String,
    pub status: i64,
    pub status_text: String,
    pub mime: String,
    /// network error, e.g. net::ERR_CONNECTION_REFUSED
    pub failed: String,
    pub size: i64,
    pub duration: Duration,
    pub done: bool,
    pub req_headers: Map<String, Value>,
    pub res_headers: Map<String, Value>,
    pub has_body: bool,
    pub post_data: bool,
    pub from_cache: bool,
    /// the agent has been told this request failed
    told: bool,
}

impl NetEntry {
    pub fn is_error(&self) -> bool {
        !self.failed.is_empty() || self.status >= 400
    }
}

#[derive(Default)]
struct ObsState {
    console: Vec<ConsoleEntry>,
    net: Vec<NetEntry>,
    next_c: i64,
    next_n: i64,
    /// console messages up to this id were already reported (see alerts)
    told_console: i64,
    /// the tab's top frame: a document request for it starts a new page
    main_frame: String,
    /// entries up to these ids belong to earlier pages
    page_c: i64,
    page_n: i64,
}

/// observer holds what one tab reported. Safe for concurrent use; event handlers only take its lock.
pub struct Observer {
    st: Mutex<ObsState>,
}

pub fn new_observer(main_frame: &str) -> Observer {
    Observer {
        st: Mutex::new(ObsState {
            main_frame: main_frame.to_string(),
            ..Default::default()
        }),
    }
}

/// `t.obs`
pub fn observer_of(t: &Tab) -> Option<Arc<Observer>> {
    t.ext.get()?.clone().downcast::<Observer>().ok()
}

/// The `BrowserHooks` of this module: creates each tab's observer and feeds it.
#[derive(Default)]
pub struct Observe;

impl BrowserHooks for Observe {
    fn tab_created(&self, t: &Arc<Tab>) {
        let _ = t.ext.set(Arc::new(new_observer(&t.id)));
    }
    fn tab_event(&self, t: &Arc<Tab>, ev: &Event) {
        if let Some(o) = observer_of(t) {
            o.event(ev);
        }
    }
}

/// `[]rune(text)[:n]`
fn cut_runes(s: &str, n: usize) -> Option<&str> {
    s.char_indices().nth(n).map(|(i, _)| &s[..i])
}

impl ObsState {
    fn add_console(&mut self, level: &str, text: &str, where_: &str) {
        let text = match cut_runes(text, 2000) {
            Some(cut) => format!("{cut}…"),
            None => text.to_string(),
        };
        self.next_c += 1;
        self.console.push(ConsoleEntry {
            id: self.next_c,
            time: SystemTime::now(),
            level: level.to_string(),
            text,
            where_: where_.to_string(),
        });
        if self.console.len() > MAX_CONSOLE {
            let cut = self.console.len() - MAX_CONSOLE;
            self.console.drain(..cut);
        }
    }

    fn add_net(&mut self, mut e: NetEntry) {
        self.next_n += 1;
        e.id = self.next_n;
        self.net.push(e);
        if self.net.len() > MAX_NET {
            let cut = self.net.len() - MAX_NET;
            self.net.drain(..cut);
        }
    }

    /// `byReq[id]`: the newest entry of a request id (a redirect reuses the id).
    fn by_req(&mut self, id: &str) -> Option<&mut NetEntry> {
        self.net.iter_mut().rev().find(|n| n.req_id == id)
    }
}

/// remoteText turns a console argument into text.
fn remote_text(r: &Value) -> String {
    if r.is_null() {
        return String::new();
    }
    if let Some(v) = r.get("value") {
        // `json.Unmarshal(raw, &string)`: a string gives itself, null gives "", anything else its JSON
        return match v {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
    }
    if let Some(u) = r.get("unserializableValue").and_then(Value::as_str) {
        if !u.is_empty() {
            return u.to_string();
        }
    }
    if let Some(d) = r.get("description").and_then(Value::as_str) {
        if !d.is_empty() {
            return d.to_string();
        }
    }
    r.get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn console_level(t: &str) -> &'static str {
    match t {
        "error" | "assert" => "error",
        "warning" => "warning",
        "debug" | "trace" => "debug",
        _ => "info",
    }
}

fn frame_where(st: &Value) -> String {
    if let Some(f) = st
        .get("callFrames")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    {
        return format!(
            "{}:{}",
            f["url"].as_str().unwrap_or_default(),
            f["lineNumber"].as_i64().unwrap_or(0) + 1
        );
    }
    String::new()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

fn obj(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

fn num(v: &Value) -> i64 {
    v.as_f64().map_or(0, |f| f as i64)
}

impl Observer {
    fn st(&self) -> MutexGuard<'_, ObsState> {
        lock(&self.st)
    }

    /// event feeds one Chrome event of the tab into the buffers. Must return fast.
    pub fn event(&self, ev: &Event) {
        let p = &ev.params;
        match ev.method.as_str() {
            "Runtime.consoleAPICalled" => {
                let ty = s(&p["type"]);
                if ty == "clear" || ty == "endGroup" {
                    return;
                }
                let parts: Vec<String> = p["args"]
                    .as_array()
                    .map(|a| a.iter().map(remote_text).collect())
                    .unwrap_or_default();
                self.st().add_console(
                    console_level(ty),
                    &parts.join(" "),
                    &frame_where(&p["stackTrace"]),
                );
            }

            "Runtime.exceptionThrown" => {
                let d = &p["exceptionDetails"];
                if !d.is_object() {
                    return;
                }
                let mut text = s(&d["text"]);
                let desc = s(&d["exception"]["description"]);
                if !desc.is_empty() {
                    text = desc.split("\n    at ").next().unwrap_or(desc);
                }
                let url = s(&d["url"]);
                let mut where_ = url.to_string();
                if !url.is_empty() {
                    where_ = format!("{url}:{}", num(&d["lineNumber"]) + 1);
                }
                self.st()
                    .add_console("error", &format!("Uncaught {text}"), &where_);
            }

            "Log.entryAdded" => {
                let en = &p["entry"];
                if !en.is_object() || s(&en["source"]) == "network" {
                    return; // network errors are shown as requests
                }
                let mut level = s(&en["level"]);
                if level == "verbose" {
                    level = "debug";
                }
                let url = s(&en["url"]);
                let mut where_ = url.to_string();
                if !url.is_empty() && num(&en["lineNumber"]) > 0 {
                    where_ = format!("{url}:{}", num(&en["lineNumber"]));
                }
                self.st().add_console(
                    level,
                    &format!("[{}] {}", s(&en["source"]), s(&en["text"])),
                    &where_,
                );
            }

            "Network.requestWillBeSent" => {
                let req = &p["request"];
                if !req.is_object() {
                    return;
                }
                let id = s(&p["requestId"]);
                let mut o = self.st();
                let redirect = p.get("redirectResponse").filter(|r| r.is_object());
                if let Some(rr) = redirect {
                    // a redirect reuses the request id
                    if let Some(prev) = o.by_req(id) {
                        prev.status = num(&rr["status"]);
                        prev.status_text = s(&rr["statusText"]).to_string();
                        prev.res_headers = obj(&rr["headers"]);
                        prev.done = true;
                        prev.duration = prev.started.elapsed();
                    }
                }
                if s(&p["type"]) == "Document"
                    && s(&p["frameId"]) == o.main_frame
                    && redirect.is_none()
                {
                    // a new page starts: earlier entries are history
                    o.page_c = o.next_c;
                    o.page_n = o.next_n;
                }
                o.add_net(NetEntry {
                    id: 0,
                    req_id: id.to_string(),
                    time: SystemTime::now(),
                    started: Instant::now(),
                    method: s(&req["method"]).to_string(),
                    url: s(&req["url"]).to_string(),
                    type_: s(&p["type"]).to_string(),
                    status: 0,
                    status_text: String::new(),
                    mime: String::new(),
                    failed: String::new(),
                    size: 0,
                    duration: Duration::ZERO,
                    done: false,
                    req_headers: obj(&req["headers"]),
                    res_headers: Map::new(),
                    has_body: false,
                    post_data: req["hasPostData"].as_bool().unwrap_or(false),
                    from_cache: false,
                    told: false,
                });
            }

            "Network.responseReceived" => {
                let r = &p["response"];
                if !r.is_object() {
                    return;
                }
                if let Some(n) = self.st().by_req(s(&p["requestId"])) {
                    n.status = num(&r["status"]);
                    n.status_text = s(&r["statusText"]).to_string();
                    n.mime = s(&r["mimeType"]).to_string();
                    n.res_headers = obj(&r["headers"]);
                    n.from_cache = r["fromDiskCache"].as_bool().unwrap_or(false);
                    if !s(&p["type"]).is_empty() {
                        n.type_ = s(&p["type"]).to_string();
                    }
                }
            }

            "Network.loadingFinished" => {
                if let Some(n) = self.st().by_req(s(&p["requestId"])) {
                    n.done = true;
                    n.has_body = true;
                    n.size = num(&p["encodedDataLength"]);
                    n.duration = n.started.elapsed();
                }
            }

            "Network.loadingFailed" => {
                if let Some(n) = self.st().by_req(s(&p["requestId"])) {
                    n.done = true;
                    n.duration = n.started.elapsed();
                    n.failed = s(&p["errorText"]).to_string();
                    if p["canceled"].as_bool().unwrap_or(false) && n.failed.is_empty() {
                        n.failed = "canceled".into();
                    }
                    if !s(&p["blockedReason"]).is_empty() {
                        n.failed += &format!(" (blocked: {})", s(&p["blockedReason"]));
                    }
                    if p["corsErrorStatus"].is_object() {
                        n.failed += &format!(" (CORS: {})", s(&p["corsErrorStatus"]["corsError"]));
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------- Reading ----------

fn level_rank(l: &str) -> i64 {
    match l {
        "debug" => 0,
        "info" => 1,
        "warning" => 2,
        "error" => 3,
        _ => 0,
    }
}

/// `s[:n]` for ASCII-ish cuts, never splitting a character.
fn cut_bytes(s: &str, n: usize) -> &str {
    let mut n = n.min(s.len());
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

pub fn short_url(u: &str) -> String {
    if u.starts_with("data:") && u.len() > 60 {
        return format!("{}…", cut_bytes(u, 60));
    }
    if u.len() > 200 {
        return format!("{}…", cut_bytes(u, 200));
    }
    u.to_string()
}

/// `time.Format("15:04:05")` in the local zone.
fn hms(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    #[cfg(unix)]
    // SAFETY: localtime_r only writes into the zeroed tm we pass.
    unsafe {
        let ts = secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&ts, &mut tm);
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
    #[cfg(not(unix))]
    {
        // UTC fallback
        let r = secs % 86400;
        format!("{:02}:{:02}:{:02}", r / 3600, r % 3600 / 60, r % 60)
    }
}

impl Observer {
    /// consoleReport lists messages of at least the given level ("all" = everything, default "info").
    pub fn console_report(&self, level: &str, limit: i64, clear: bool, history: bool) -> String {
        let mut min = level_rank("info");
        if let l @ ("debug" | "info" | "warning" | "error") = level.to_lowercase().as_str() {
            min = level_rank(l);
        }
        if level.eq_ignore_ascii_case("all") {
            min = 0;
        }
        let (mut rows, total) = {
            let mut o = self.st();
            let rows: Vec<ConsoleEntry> = o
                .console
                .iter()
                .filter(|c| level_rank(&c.level) >= min && (history || c.id > o.page_c))
                .cloned()
                .collect();
            let total = o.console.len();
            if clear {
                o.console.clear();
                o.told_console = o.next_c;
            }
            (rows, total)
        };

        let limit = if limit <= 0 { 100 } else { limit as usize };
        let mut cut = 0;
        if rows.len() > limit {
            cut = rows.len() - limit;
            rows.drain(..cut);
        }
        let mut out = format!(
            "### Console messages ({} shown, {} kept in total)\n",
            rows.len(),
            total
        );
        if !history {
            out.push_str("(current page only; history: true includes earlier pages)\n");
        }
        if cut > 0 {
            out.push_str(&format!(
                "({cut} older ones left out; use limit to see more)\n"
            ));
        }
        if rows.is_empty() {
            out.push_str("No messages.\n");
        }
        for c in &rows {
            out.push_str(&format!(
                "- [{}] {} {}",
                c.level.to_uppercase(),
                hms(c.time),
                c.text
            ));
            if !c.where_.is_empty() {
                out.push_str(&format!("  ({})", c.where_));
            }
            out.push('\n');
        }
        if clear {
            out.push_str("(cleared)\n");
        }
        out
    }
}

fn is_static_type(t: &str) -> bool {
    matches!(
        t,
        "Image" | "Font" | "Stylesheet" | "Media" | "Manifest" | "Ping"
    )
}

#[derive(Debug, Clone, Default)]
pub struct NetFilter {
    /// regular expression (or plain text) on the URL
    pub match_: String,
    pub method: String,
    pub failed_only: bool,
    pub static_: bool,
    /// include requests of earlier pages
    pub history: bool,
    pub limit: i64,
}

impl Observer {
    pub fn network_report(&self, f: &NetFilter, clear: bool) -> String {
        let mut re: Option<Regex> = None;
        if !f.match_.is_empty() {
            re = Some(
                Regex::new(&format!("(?i){}", f.match_)).unwrap_or_else(|_| {
                    Regex::new(&format!("(?i){}", regex::escape(&f.match_))).expect("escaped")
                }),
            );
        }
        let (mut rows, total) = {
            let mut o = self.st();
            let mut rows: Vec<NetEntry> = Vec::new();
            for n in &o.net {
                if !f.history && n.id <= o.page_n {
                    continue;
                }
                if !f.static_ && is_static_type(&n.type_) && !n.is_error() {
                    continue;
                }
                if f.failed_only && !n.is_error() {
                    continue;
                }
                if !f.method.is_empty() && !n.method.eq_ignore_ascii_case(&f.method) {
                    continue;
                }
                if re.as_ref().is_some_and(|re| !re.is_match(&n.url)) {
                    continue;
                }
                rows.push(n.clone());
            }
            let total = o.net.len();
            if clear {
                o.net.clear();
            }
            (rows, total)
        };

        let limit = if f.limit <= 0 { 60 } else { f.limit as usize };
        let mut cut = 0;
        if rows.len() > limit {
            cut = rows.len() - limit;
            rows.drain(..cut);
        }
        let mut out = format!(
            "### Network requests ({} shown, {} kept in total)\n",
            rows.len(),
            total
        );
        if cut > 0 {
            out.push_str(&format!(
                "({cut} older ones left out; use limit to see more)\n"
            ));
        }
        if rows.is_empty() {
            out.push_str("No requests");
            if !f.static_ {
                out.push_str(
                    " (images, fonts, stylesheets and media are hidden unless includeStatic is true)",
                );
            }
            out.push_str(".\n");
        }
        for r in &rows {
            out.push_str(&format!("- {}\n", r.line()));
        }
        if !rows.is_empty() {
            out.push_str("\nPass id to see headers (and body with body: true) of one request.\n");
        }
        if clear {
            out.push_str("(cleared)\n");
        }
        out
    }
}

impl NetEntry {
    pub fn line(&self) -> String {
        let mut st = "pending".to_string();
        if !self.failed.is_empty() {
            st = format!("FAILED {}", self.failed);
        } else if self.status > 0 {
            st = format!("{} {}", self.status, self.status_text);
            if self.from_cache {
                st += " (cache)";
            }
        }
        let mut extra = String::new();
        if self.done && !self.duration.is_zero() {
            extra += &format!(" {}ms", self.duration.as_millis());
        }
        if self.size > 0 {
            extra += &format!(" {}", human_size(self.size));
        }
        let t = if self.type_.is_empty() {
            "-"
        } else {
            &self.type_
        };
        format!(
            "#{} {} {} → {} [{}]{}",
            self.id,
            self.method,
            short_url(&self.url),
            st.trim(),
            t,
            extra
        )
    }
}

pub fn human_size(b: i64) -> String {
    if b >= 1 << 20 {
        format!("{:.1}MB", b as f64 / (1 << 20) as f64)
    } else if b >= 1 << 10 {
        format!("{:.1}KB", b as f64 / (1 << 10) as f64)
    } else {
        format!("{b}B")
    }
}

impl Observer {
    /// find returns a copy of the request with this id.
    pub fn find(&self, id: i64) -> Option<NetEntry> {
        self.st().net.iter().find(|n| n.id == id).cloned()
    }
}

fn is_secret_header(k: &str) -> bool {
    matches!(
        k.to_lowercase().as_str(),
        "authorization" | "cookie" | "set-cookie" | "proxy-authorization" | "x-api-key"
    )
}

/// `fmt.Sprint(v)` for a JSON value.
fn sprint(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "<nil>".into(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 1e21 => format!("{f}"),
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

fn write_headers(s: &mut String, title: &str, h: &Map<String, Value>) {
    if h.is_empty() {
        return;
    }
    s.push_str(&format!("{title}\n"));
    let mut keys: Vec<&String> = h.keys().collect();
    keys.sort();
    for k in keys {
        let v = if is_secret_header(k) {
            "(hidden)".to_string()
        } else {
            sprint(&h[k])
        };
        s.push_str(&format!("  {k}: {v}\n"));
    }
}

fn cmp_or<'a>(a: &'a str, b: &'a str) -> &'a str {
    if a.is_empty() {
        b
    } else {
        a
    }
}

impl NetEntry {
    pub fn detail(&self) -> String {
        let mut s = format!(
            "### Request #{}\n- {}\n- Type: {}, MIME: {}\n",
            self.id,
            self.line(),
            cmp_or(&self.type_, "-"),
            cmp_or(&self.mime, "-")
        );
        if self.post_data {
            s.push_str("- Has a request body\n");
        }
        write_headers(&mut s, "Request headers:", &self.req_headers);
        write_headers(&mut s, "Response headers:", &self.res_headers);
        s
    }
}

impl Observer {
    /// alerts lists what went wrong since the last time the agent was told: console errors,
    /// page exceptions and failed requests. Empty when everything is fine.
    pub fn alerts(&self) -> (String, usize) {
        let mut o = self.st();
        let mut lines: Vec<String> = Vec::new();
        for c in &o.console {
            if c.id > o.told_console && level_rank(&c.level) >= level_rank("warning") {
                lines.push(format!("- console {}: {}", c.level, one_line(&c.text, 240)));
            }
        }
        for n in o.net.iter_mut() {
            let base = n.url.split('?').next().unwrap_or_default();
            if !n.told && n.is_error() && n.done && !base.ends_with("/favicon.ico") {
                n.told = true;
                lines.push(format!("- request {}", n.line()));
            }
        }
        o.told_console = o.next_c;
        if lines.is_empty() {
            return (String::new(), 0);
        }
        let count = lines.len();
        if lines.len() > 8 {
            let more = lines.len() - 8;
            lines.truncate(8);
            lines.push(format!(
                "- … and {more} more (see browser_console_messages / browser_network_requests)"
            ));
        }
        (
            format!(
                "### Page problems since the last step\n{}\n",
                lines.join("\n")
            ),
            count,
        )
    }

    /// markSeen forgets problems so far, so the next report only covers what comes after
    /// (used when the agent leaves for another page).
    pub fn mark_seen(&self) {
        let mut o = self.st();
        o.told_console = o.next_c;
        for n in o.net.iter_mut() {
            n.told = true;
        }
    }
}

pub fn one_line(s: &str, max: usize) -> String {
    let mut s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(cut) = cut_runes(&s, max) {
        // r[:max-1] + "…"
        let keep = cut_runes(&s, max - 1).unwrap_or(cut).to_string();
        s = format!("{keep}…");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(method: &str, params: Value) -> Event {
        Event {
            method: method.into(),
            params,
            session: None,
        }
    }

    #[test]
    fn console_and_network_buffers() {
        let o = new_observer("main");
        o.event(&ev(
            "Runtime.consoleAPICalled",
            json!({"type":"warning","args":[{"type":"string","value":"hi"},{"type":"number","value":3}],
                   "stackTrace":{"callFrames":[{"url":"http://x/a.js","lineNumber":4}]}}),
        ));
        o.event(&ev(
            "Network.requestWillBeSent",
            json!({"requestId":"1","type":"Document","frameId":"main","request":{"method":"GET","url":"http://x/","headers":{"Cookie":"a"}}}),
        ));
        o.event(&ev(
            "Network.responseReceived",
            json!({"requestId":"1","type":"Document","response":{"status":404,"statusText":"Not Found","mimeType":"text/html","headers":{}}}),
        ));
        o.event(&ev(
            "Network.loadingFinished",
            json!({"requestId":"1","encodedDataLength":2048}),
        ));
        // the Document request below starts a new page, so this message is history
        let c = o.console_report("", 0, false, true);
        assert!(c.contains("- [WARNING] "), "{c}");
        assert!(c.contains("hi 3  (http://x/a.js:5)"), "{c}");
        let n = o.network_report(&NetFilter::default(), false);
        assert!(
            n.contains("#1 GET http://x/ → 404 Not Found [Document] "),
            "{n}"
        );
        assert!(n.contains("2.0KB"), "{n}");
        let (a, count) = o.alerts();
        assert_eq!(count, 2, "{a}");
        assert_eq!(o.alerts().1, 0);
        assert!(o.find(1).unwrap().detail().contains("Cookie: (hidden)"));
    }

    #[test]
    fn ring_buffer_and_pages() {
        let o = new_observer("m");
        for i in 0..(MAX_CONSOLE + 5) {
            o.st().add_console("info", &format!("m{i}"), "");
        }
        assert_eq!(o.st().console.len(), MAX_CONSOLE);
        assert_eq!(o.st().console[0].text, "m5");
        o.event(&ev(
            "Network.requestWillBeSent",
            json!({"requestId":"2","type":"Document","frameId":"m","request":{"method":"GET","url":"u"}}),
        ));
        assert!(o
            .console_report("all", 0, false, false)
            .contains("No messages."));
        assert!(!o
            .console_report("all", 0, false, true)
            .contains("No messages."));
        assert_eq!(one_line("a   b\n c", 240), "a b c");
        assert_eq!(
            one_line(&"x".repeat(300), 10),
            format!("{}…", "x".repeat(9))
        );
    }
}
