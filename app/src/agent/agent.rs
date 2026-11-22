//! The AI agent: drives the shared browser for an MCP client (see mcp.rs).
//!
//! It acts like a person would, so whoever watches the web UI can follow along:
//! its own cursor glides to each element, the element is outlined with a caption,
//! clicks show a ripple, text is typed key by key, and every step is logged in
//! the chat as "AI Agent". A person can click "Take over" at any time; the agent
//! then stops and its next step reports that a human is in control.
//!
//! # Rust port: concurrency
//!   * `Agent` is one worker: the root agent plus one per (sub-agent name, tab). `Agent.mu` is
//!     a `tokio::sync::Mutex<>` taken as an owned guard by the MCP layer for a whole tool call (it
//!     serialises that worker's calls); `browser_read_chat` drops the guard while it waits
//!     and takes it again afterwards (it unlocks and relocks it). The mutable fields of the
//!     `Agent` (cursor, last outline, baselines, downloads seen) live in `Agent.st`, a std mutex held only for
//!     short reads and writes, never across an `.await`.
//!   * Every browser step goes through `act` (takes control, then runs on the browser's command queue like
//!     `a.b.run`, or on the tab's lane with `run_tab` for a worker bound to a tab) or `look` (reads only,
//!     skips the input queue). Different workers on different tabs run concurrently; the hub lock is only held
//!     for short in-memory sections.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::browser::{
    fix_mods, go_history, normalize_url, BResult, Browser, BrowserApi, Tab, INPUT_TIMEOUT,
    MOD_CTRL, MOD_META, MOD_SHIFT,
};
use crate::frames::{self, js_error, jsq, parse_frame_ref};
use crate::{Client, Hub, HubHooks, HubState, Msg, User};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The page helper injected into every tab (agent.js at the crate root).
pub fn agent_js() -> &'static str {
    include_str!("../../agent.js")
}

/// agent.rs: `type Rect` (normalised element outline, no omitempty).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// agent.rs: `var agentUser`.
pub fn agent_user() -> User {
    User {
        id: "agent".into(),
        name: "AI Agent".into(),
        color: "#a78bfa".into(),
        agent: true,
    }
}

pub struct AgentOptions {
    /// 0 = no animation (fastest), 1 = normal pace, 2 = twice as slow
    pub speed: f64,
    /// open the web UI when the agent starts and nobody watches
    pub open_viewer: bool,
    pub viewer_url: String,
    /// give control back after this long without agent actions
    pub idle: Duration,
}

#[derive(Clone)]
pub struct Mark {
    pub snap: String,
    pub url: String,
}

/// The fields of `Agent` that its calls change (see the module docs).
#[derive(Default)]
pub struct AgentSt {
    /// cursor position, 0..1 of the screen
    pub x: f64,
    pub y: f64,
    /// viewer already opened once
    pub opened: bool,
    /// the outline the model saw last, and which tab/URL it was for
    pub last: String,
    pub last_key: String,
    /// named baselines for browser_snapshot_diff
    pub marks: HashMap<String, Mark>,
    /// downloads the agent has been told about
    pub dl_seen: usize,
}

pub struct Agent {
    pub hub: Arc<Hub>,
    pub b: Arc<Browser>,
    opt: Arc<AgentOptions>,

    /// serializes this worker's calls
    pub mu: Arc<tokio::sync::Mutex<()>>,
    /// empty means the active tab (legacy behavior)
    pub tab_id: String,
    /// the agent's display name (empty: the default agent)
    worker_id: String,
    /// id of this agent's cursor on everyone's screen
    cursor_id: String,
    st: Mutex<AgentSt>,
    workers: Arc<Mutex<HashMap<String, Arc<Agent>>>>,
}

impl Agent {
    /// The mutable per-worker state (cursor, last outline, baselines…).
    pub fn st(&self) -> MutexGuard<'_, AgentSt> {
        lock(&self.st)
    }

    /// worker returns the agent identity for (name, tab): every sub-agent gets its own cursor,
    /// refs and snapshots, even when several of them use the same tab.
    pub fn worker(&self, name: &str, tab_id: &str) -> Arc<Agent> {
        let key = format!("{name}@{tab_id}");
        let mut workers = lock(&self.workers);
        if let Some(w) = workers.get(&key) {
            return w.clone();
        }
        let w = Arc::new(Agent {
            hub: self.hub.clone(),
            b: self.b.clone(),
            opt: self.opt.clone(),
            mu: Arc::new(tokio::sync::Mutex::new(())),
            tab_id: tab_id.to_string(),
            worker_id: cmp_or(name, &format!("agent:{tab_id}")),
            cursor_id: format!("agent:{key}"),
            st: Mutex::new(AgentSt {
                x: 0.5,
                y: 0.5,
                ..Default::default()
            }),
            workers: self.workers.clone(),
        });
        workers.insert(key, w.clone());
        w
    }

    /// The agent for (name, tab): the root agent itself when both are empty.
    pub fn worker_or_self(self: &Arc<Self>, name: &str, tab_id: &str) -> Arc<Agent> {
        if name.is_empty() && tab_id.is_empty() {
            return self.clone();
        }
        self.worker(name, tab_id)
    }

    pub fn new(hub: Arc<Hub>, b: Arc<Browser>, opt: AgentOptions) -> Arc<Agent> {
        let idle = opt.idle;
        let a = Arc::new(Agent {
            hub: hub.clone(),
            b,
            opt: Arc::new(opt),
            mu: Arc::new(tokio::sync::Mutex::new(())),
            tab_id: String::new(),
            worker_id: String::new(),
            cursor_id: String::new(),
            st: Mutex::new(AgentSt {
                x: 0.5,
                y: 0.5,
                ..Default::default()
            }),
            workers: Arc::new(Mutex::new(HashMap::new())),
        });
        tokio::spawn(hub.watch_agent_idle(idle));
        // (also starts `h.watchSeats` here; main.rs starts it for every run.)
        a
    }
}

// ---------- Hub side: presence, control, what viewers see ----------

/// humanGrace: the agent won't take control from a person who used the browser this recently.
const HUMAN_GRACE: Duration = Duration::from_secs(8);

impl Hub {
    /// agentJoin shows the agent in the room and returns how many people are watching.
    pub fn agent_join(&self) -> usize {
        let mut st = self.lock();
        if !st.agent_on {
            st.agent_on = true;
        }
        st.agent_last = Some(Instant::now());
        st.agent_busy = true;
        st.clients.len()
    }

    /// agentDone marks the end of a tool call: the idle timer starts from here.
    pub fn agent_done(&self) {
        let mut st = self.lock();
        st.agent_last = Some(Instant::now());
        st.agent_busy = false;
    }

    /// agentTake gives control to the agent, taking it from a person if needed, unless someone blocked the agent.
    /// (wraps `errBlocked`: its text is "agent blocked: …".)
    pub fn agent_take(&self) -> BResult<()> {
        let mut st = self.lock();
        if let Some(b) = &st.blocked {
            return Err(format!("agent blocked: {} blocked the agent from using the browser. Wait until they click “Unblock”, or ask them to (browser_say), then try again", b.name));
        }
        let other = st
            .ctrl
            .as_ref()
            .filter(|c| !Arc::ptr_eq(c, &st.agent))
            .cloned();
        if let Some(c) = &other {
            if st.human_last.is_some_and(|t| t.elapsed() < HUMAN_GRACE) {
                return Err(format!("agent blocked: {} is using the browser right now. Wait a few seconds and try again, or ask them (browser_say)", c.user().name));
            }
        }
        if !st.ctrl_is_agent() {
            let prev = st.ctrl.clone();
            st.agent_from = prev.clone(); // None when nobody was driving
            if let Some(c) = &prev {
                let text = format!(
                    "🤖 {} took over from {}",
                    st.agent.user().name,
                    c.user().name
                );
                st.add_system_chat(&text);
            }
            let a = st.agent.clone();
            st.set_ctrl(Some(&a));
        }
        st.agent_last = Some(Instant::now());
        Ok(())
    }

    /// watchAgentIdle gives control back after the agent has been idle for `idle`.
    pub async fn watch_agent_idle(self: Arc<Self>, idle: Duration) {
        if idle.is_zero() {
            return;
        }
        let mut t = tokio::time::interval(Duration::from_secs(1));
        t.tick().await; // time.Tick first fires after 1s
        loop {
            t.tick().await;
            let mut st = self.lock();
            if st.ctrl_is_agent()
                && !st.agent_busy
                && st.agent_last.is_some_and(|t| t.elapsed() > idle)
            {
                st.agent_release();
            }
        }
    }

    /// agentSend broadcasts one message from the agent to every viewer.
    pub fn agent_send(&self, m: Msg) {
        self.lock().broadcast(&m, None);
    }

    /// agentLog adds a line from the agent to the chat (not saved to disk on its own).
    pub fn agent_log(&self, text: &str) {
        self.agent_chat("", text);
    }

    /// agentChat adds a line to the chat from a coding agent (who = its @handle; empty = the generic "AI Agent").
    pub fn agent_chat(&self, who: &str, text: &str) {
        let mut st = self.lock();
        let u = st.agent.user();
        let mut cm = crate::ChatMsg {
            user: u.name,
            color: u.color,
            text: text.to_string(),
            time: crate::now_ms(),
            agent: true,
            ..Default::default()
        };
        if !who.is_empty() {
            cm.user = who.to_string();
        }
        st.push_chat(cm.clone());
        st.broadcast(
            &Msg {
                type_: "chat".into(),
                msg: Some(cm),
                ..Default::default()
            },
            None,
        );
    }
}

impl HubState {
    /// agentRelease gives control back to the person the agent took it from (if still here), or frees it.
    /// caller holds h.mu
    pub fn agent_release(&mut self) {
        let mut back = self.agent_from.take();
        if let Some(b) = &back {
            if !self.clients.iter().any(|c| Arc::ptr_eq(c, b)) {
                back = None;
            }
        }
        self.set_ctrl(back.as_ref());
    }
}

/// The hooks the hub calls into this port's modules (agent.rs `agent_release`, pick.rs `pick`, assets.rs `add`).
pub struct AppHooks {
    pub browser: Option<Arc<Browser>>,
}

impl HubHooks for AppHooks {
    fn agent_release(&self, st: &mut HubState) {
        st.agent_release();
    }

    fn rec_start(&self, st: &mut HubState, c: &Arc<Client>) {
        crate::flow::rec_start(st, c);
    }

    fn rec_stop(&self, st: &mut HubState, c: Option<&Arc<Client>>, discard: bool) {
        crate::flow::rec_stop(st, c, discard);
    }

    fn rec_input(&self, st: &mut HubState, m: &Msg) {
        crate::flow::rec_input(st, m);
    }

    fn rec_info(&self, _st: &HubState) -> Option<crate::flow::FlowInfo> {
        crate::flow::rec_info()
    }

    fn rec_by(&self, _st: &HubState, c: &Arc<Client>) -> bool {
        crate::flow::rec_by(c)
    }

    fn pick(&self, hub: &Arc<Hub>, c: &Arc<Client>, m: Msg) {
        if let Some(b) = self.browser.clone() {
            let (hub, c) = (hub.clone(), c.clone());
            tokio::spawn(async move { crate::pick::pick(&hub, &b, &c, m).await });
        }
    }

    fn asset_copy(&self, _hub: &Arc<Hub>, text: String, url: String, title: String, by: String) {
        // adds the asset in Hub.handle's copy callback; spawn so the hub lock is free for assetsChanged
        tokio::task::spawn_blocking(move || crate::assets::add_copy(text, url, title, by));
    }
}

// ---------- Page helpers: evaluating agent.js ----------

const AGENT_MISSING: &str = "__abu_missing__";

/// agentReady throws unless the current version of agent.js is loaded in the page.
fn agent_ready() -> &'static str {
    static READY: LazyLock<String> = LazyLock::new(|| {
        let js = agent_js();
        let ver = js.find("const V = ").and_then(|i| {
            let rest = &js[i + "const V = ".len()..];
            let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            (!n.is_empty() && rest[n.len()..].starts_with(';')).then_some(n)
        });
        match ver {
            None => format!("throw new Error('{AGENT_MISSING}');\n"),
            Some(v) => format!(
                "if (!(window.__abu && window.__abu.v === {v})) throw new Error('{AGENT_MISSING}');\n"
            ),
        }
    });
    &READY
}

/// What evaluating a script does with its result: an exception becomes the error `jsError` then reads.
async fn evaluate_raw(t: &Tab, expr: String) -> BResult<Value> {
    let res = t
        .call(
            "Runtime.evaluate",
            json!({"expression": expr, "returnByValue": true, "awaitPromise": true}),
            Some(Duration::from_secs(30)), // a hung page must not hang the agent
        )
        .await
        .map_err(|e| e.to_string())?;
    if let Some(d) = res.get("exceptionDetails") {
        return Err(js_error(d));
    }
    Ok(res.pointer("/result/value").cloned().unwrap_or(Value::Null))
}

/// evalJS runs expr in the page after installing the helpers from agent.js.
pub async fn eval_js(t: &Tab, expr: &str) -> BResult<Value> {
    // Usually the helper is already in the page: skip sending its 20KB source each time.
    match evaluate_raw(t, format!("{}{expr}", agent_ready())).await {
        Err(e) if e.contains(AGENT_MISSING) => {
            evaluate_raw(t, format!("{}\n;{expr}", agent_js())).await
        }
        r => r,
    }
}

/// `json.Unmarshal` of the result into T: null/undefined leave the zero value.
fn decode<T: DeserializeOwned + Default>(v: Value) -> BResult<T> {
    if v.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(v).map_err(|e| e.to_string())
}

/// evalJS[T]
pub async fn eval_js_as<T: DeserializeOwned + Default>(t: &Tab, expr: &str) -> BResult<T> {
    decode(eval_js(t, expr).await?)
}

/// evalRef evaluates a page-helper expression on the tab, routing refs like
/// "f1e12" into that cross-origin iframe. For the main frame it is plain evalJS.
pub async fn eval_ref<T: DeserializeOwned + Default>(t: &Tab, r: &str, expr: &str) -> BResult<T> {
    let fr = parse_frame_ref(r);
    if fr.frame == 0 {
        return eval_js_as(t, expr).await;
    }
    // Inside the iframe: same helper, but evaluated in that frame's own world.
    frames::eval_in_frame_as(t, fr.frame, expr).await
}

/// `exceptionError`: a Runtime.evaluate result's exception as a plain message.
pub fn exception_error(d: &Value) -> String {
    js_error(d)
}

pub fn cmp_or(s: &str, def: &str) -> String {
    if s.is_empty() {
        def.to_string()
    } else {
        s.to_string()
    }
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

fn open_in_browser(u: &str) {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(u);
        c
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", u]);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(u);
        c
    };
    if let Ok(mut child) = cmd.spawn() {
        let _ = child.wait();
    }
}

// ---------- Basics ----------

impl Agent {
    /// begin runs at the start of every tool call.
    pub fn begin(&self) {
        let viewers = self.hub.agent_join();
        let open = {
            let mut s = self.st();
            if self.opt.open_viewer && !s.opened && viewers == 0 {
                s.opened = true;
                true
            } else {
                false
            }
        };
        if open {
            let u = self.opt.viewer_url.clone();
            std::thread::spawn(move || open_in_browser(&u));
        }
    }

    /// pause waits d scaled by the speed setting, so people can follow.
    async fn pause(&self, d: Duration) {
        if self.opt.speed > 0.0 {
            tokio::time::sleep(d.mul_f64(self.opt.speed)).await;
        }
    }

    /// act runs f on the active tab, as long as the agent may drive.
    pub async fn act<T, F, Fut>(&self, f: F) -> BResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Tab>) -> Fut + Send + 'static,
        Fut: Future<Output = BResult<T>> + Send + 'static,
    {
        self.hub.agent_take()?;
        if !self.tab_id.is_empty() {
            return self.b.run_tab(&self.tab_id, f).await;
        }
        self.b.run(f).await
    }

    /// look runs f on the active tab without needing control (reading only).
    /// Reads skip the input queue, so the agent watching or snapshotting the page
    /// never makes a person's clicks and keys wait.
    pub async fn look<T, F, Fut>(&self, f: F) -> BResult<T>
    where
        F: FnOnce(Arc<Tab>) -> Fut,
        Fut: Future<Output = BResult<T>>,
    {
        if !self.tab_id.is_empty() {
            return self.b.run_tab(&self.tab_id, f).await;
        }
        let t = self.b.st().active.clone();
        match t {
            Some(t) => f(t).await,
            None => Err("no tab is open".into()),
        }
    }

    /// evalRetry retries while a page is between two documents ("context destroyed").
    pub(crate) async fn eval_retry<T, F, Fut>(&self, mut f: F) -> BResult<T>
    where
        F: FnMut(Arc<Tab>) -> Fut,
        Fut: Future<Output = BResult<T>>,
    {
        let mut err = String::new();
        for _ in 0..4 {
            match self.look(&mut f).await {
                Ok(v) => return Ok(v),
                Err(e) if !e.contains("context") => return Err(e),
                Err(e) => err = e,
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        Err(err)
    }

    /// idleFor says how long the page has gone without changing (clocks and timers ticking don't count).
    async fn idle_for(&self) -> BResult<Duration> {
        let ms: f64 = self
            .look(|t| async move { eval_js_as::<f64>(&t, "__abu.idleFor()").await })
            .await?;
        Ok(Duration::from_millis(ms.max(0.0) as u64))
    }

    /// settle waits until the page has reacted to an action: done loading, and still for a moment.
    /// It returns as soon as that happens, so a quick page costs ~200ms instead of a fixed wait.
    fn tab_busy(&self) -> bool {
        if !self.tab_id.is_empty() {
            return self.b.busy_tab(&self.tab_id);
        }
        self.b.busy()
    }

    pub async fn settle(&self, max: Duration) {
        const STILL: Duration = Duration::from_millis(150);
        tokio::time::sleep(Duration::from_millis(50)).await; // let a navigation or a new tab start
        let end = Instant::now() + max;
        let mut give_up: Option<Instant> = None; // some pages never stop animating: don't wait for them forever
        while Instant::now() < end {
            if self.tab_busy() {
                give_up = None;
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            match give_up {
                None => give_up = Some(Instant::now() + Duration::from_millis(1500)),
                Some(g) if Instant::now() > g => return,
                _ => {}
            }
            if let Ok(d) = self.idle_for().await {
                if d >= STILL {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    /// WaitForChange returns as soon as the page changes on its own (or the element under
    /// ref / selector does): the other player moved, a reply arrived… Clocks ticking don't count.
    /// It compares with the outline the model saw last, so a change that came just before the call counts too.
    pub async fn wait_for_change(
        &self,
        mut seconds: f64,
        r: &str,
        selector: &str,
    ) -> BResult<bool> {
        if seconds <= 0.0 {
            seconds = 60.0;
        }
        let limit = Duration::from_secs_f64(seconds.min(300.0));
        if self.st().last.is_empty() {
            // nothing to compare with yet
            self.snapshot(false).await?;
        }
        self.announce("👀", "Wait for the page to change", None);
        let start = format!(
            "__abu.watchStart({}, {}, {})",
            jsq(&self.st().last),
            jsq(r),
            jsq(selector)
        );
        let mut hit: String = self
            .eval_retry(|t| {
                let start = start.clone();
                async move { eval_js_as::<String>(&t, &start).await }
            })
            .await?;
        let end = Instant::now() + limit;
        while hit.is_empty() {
            if Instant::now() > end {
                let _ = self
                    .look(|t| async move { eval_js_as::<bool>(&t, "__abu.watchStop()").await })
                    .await;
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
            if self.tab_busy() {
                break; // a new page is loading
            }
            match self
                .look(|t| async move { eval_js_as::<String>(&t, "__abu.watchState()").await })
                .await
            {
                Ok(h) => hit = h,
                Err(e) => {
                    if e.contains("context") {
                        break; // the page is being replaced
                    }
                    return Err(e);
                }
            }
        }
        self.settle(Duration::from_secs(5)).await; // let the change finish (a piece sliding, a list filling…)
        Ok(true)
    }
}

// ---------- Showing what the agent does ----------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Bx {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub cx: f64,
    pub cy: f64,
    pub vw: f64,
    pub vh: f64,
    pub cover: String,
    pub label: String,
}

impl Bx {
    fn rect(&self) -> Option<Rect> {
        if self.vw <= 0.0 || self.vh <= 0.0 {
            return None;
        }
        Some(Rect {
            x: self.x / self.vw,
            y: self.y / self.vh,
            w: self.w / self.vw,
            h: self.h / self.vh,
        })
    }

    fn center(&self) -> (f64, f64) {
        if self.vw <= 0.0 || self.vh <= 0.0 {
            return (0.5, 0.5);
        }
        (self.cx / self.vw, self.cy / self.vh)
    }
}

/// quote
pub fn quote(s: &str) -> String {
    let mut s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    s = s.trim().to_string();
    let r: Vec<char> = s.chars().collect();
    if r.len() > 80 {
        s = r[..79].iter().collect::<String>() + "…";
    }
    format!("“{s}”")
}

/// targetName names the element for captions: the agent's own description if it gave one.
fn target_name(element: &str, bx: &Bx) -> String {
    if !element.trim().is_empty() {
        return quote(element);
    }
    bx.label.clone()
}

impl Agent {
    fn agent_msg(&self, act: &str) -> Msg {
        Msg {
            type_: "agent".into(),
            act: act.into(),
            tab_id: self.tab_id.clone(),
            worker_id: self.worker_id.clone(),
            ..Default::default()
        }
    }

    /// announce puts a caption (and an outline, if bx is given) on everyone's screen and logs the step in the chat.
    pub fn announce(&self, icon: &str, text: &str, bx: Option<&Bx>) {
        let mut m = self.agent_msg("step");
        m.text = text.to_string();
        if let Some(bx) = bx {
            m.rect = bx.rect();
        }
        self.hub.agent_send(m);
        self.hub.agent_log(&format!("{icon} {text}"));
    }

    /// locate scrolls the element into view and returns where it is.
    pub async fn locate(&self, r: &str) -> BResult<Bx> {
        if r.is_empty() {
            return Err("ref is required: take a snapshot (browser_snapshot) and pass the ref of the element, e.g. \"e12\"".into());
        }
        let fr = parse_frame_ref(r);
        let (b, rr) = (self.b.clone(), r.to_string());
        let bx = self
            .act(move |t| async move {
                let expr = format!("__abu.locate({})", jsq(&fr.rest));
                let mut bx: Bx = eval_ref(&t, &rr, &expr).await?;
                if fr.frame == 0 {
                    return Ok(bx);
                }
                // Coordinates came from inside the iframe: add where the iframe sits on
                // the page, and measure against the page's own viewport instead.
                let off = frames::frame_origin(&t, fr.frame).await?;
                bx.x += off.x;
                bx.y += off.y;
                bx.cx += off.x;
                bx.cy += off.y;
                let v = b.view_size();
                bx.vw = v.width as f64;
                bx.vh = v.height as f64;
                bx.cx = bx.cx.max(1.0).min(bx.vw - 1.0);
                bx.cy = bx.cy.max(1.0).min(bx.vh - 1.0);
                Ok(bx)
            })
            .await?;
        self.pause(Duration::from_millis(80)).await; // let the scroll reach the screen
        Ok(bx)
    }
}

// ---------- Mouse and keyboard ----------

/// One input command, bounded by `INPUT_TIMEOUT`.
async fn input_call(t: &Tab, method: &str, params: Value) -> BResult<()> {
    t.call(method, params, Some(INPUT_TIMEOUT))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// `input.MouseButton` and its `buttons` mask.
#[derive(Clone, Copy)]
struct MouseBtn {
    name: &'static str,
    mask: i64,
}

fn mouse_button(name: &str) -> MouseBtn {
    match name {
        "right" => MouseBtn {
            name: "right",
            mask: 2,
        },
        "middle" => MouseBtn {
            name: "middle",
            mask: 4,
        },
        _ => MouseBtn {
            name: "left",
            mask: 1,
        },
    }
}

impl Agent {
    /// mouse: `typ` is mousePressed, mouseReleased or mouseMoved; `btn` "" = none.
    async fn mouse(
        &self,
        typ: &str,
        x: f64,
        y: f64,
        btn: &str,
        buttons: i64,
        clicks: i64,
    ) -> BResult<()> {
        let v = self.b.view_size();
        let mut p = json!({
            "type": typ,
            "x": x * v.width as f64,
            "y": y * v.height as f64,
            "button": if btn.is_empty() { "none" } else { btn },
        });
        if buttons != 0 {
            p["buttons"] = json!(buttons);
        }
        if clicks != 0 {
            p["clickCount"] = json!(clicks);
        }
        self.act(move |t| async move { input_call(&t, "Input.dispatchMouseEvent", p).await })
            .await
    }

    /// moveTo glides the cursor to (x, y), sending real mouse moves (hover effects show) and the cursor to viewers.
    pub async fn move_to(&self, x: f64, y: f64, held: &str, buttons: i64) -> BResult<()> {
        let (x, y) = (clamp01(x), clamp01(y));
        let v = self.b.view_size();
        let (sx, sy) = {
            let s = self.st();
            (s.x, s.y)
        };
        let mut steps = 1i64;
        if self.opt.speed > 0.0 {
            let d = ((x - sx) * v.width as f64).hypot((y - sy) * v.height as f64);
            steps = (d / 35.0).clamp(6.0, 26.0) as i64;
        }
        if !held.is_empty() {
            steps = steps.max(5); // drag-and-drop code needs a few moves to notice the drag
        }
        for i in 1..=steps {
            let mut k = i as f64 / steps as f64;
            k = k * k * (3.0 - 2.0 * k); // ease in and out
            let (cx, cy) = (sx + (x - sx) * k, sy + (y - sy) * k);
            self.mouse("mouseMoved", cx, cy, held, buttons, 0).await?;
            {
                let mut s = self.st();
                s.x = cx;
                s.y = cy;
            }
            let mut m = self.agent_msg("");
            m.type_ = "cursor".into();
            m.id = cmp_or(&self.cursor_id, &agent_user().id);
            m.x = cx;
            m.y = cy;
            m.area = "b".into();
            self.hub.agent_send(m);
            if i < steps {
                self.pause(Duration::from_millis(18)).await;
            }
        }
        Ok(())
    }

    /// clickAt presses and releases a button where the cursor is.
    pub async fn click_at(&self, button: &str, count: i64) -> BResult<()> {
        let mb = mouse_button(button);
        let (x, y) = {
            let s = self.st();
            (s.x, s.y)
        };
        let mut m = self.agent_msg("click");
        m.x = x;
        m.y = y;
        self.hub.agent_send(m);
        for c in 1..=count {
            self.mouse("mousePressed", x, y, mb.name, mb.mask, c)
                .await?;
            self.mouse("mouseReleased", x, y, mb.name, 0, c).await?;
        }
        Ok(())
    }
}

/// One entry of the key table.
#[derive(Debug)]
pub struct KbKey {
    pub code: &'static str,
    pub key: &'static str,
    pub text: &'static str,
    pub unmodified: &'static str,
    pub native: i64,
    pub windows: i64,
    pub shift: bool,
    pub print: bool,
}

/// rune, code, key, text, unmodified, native, windows, shift, print
type KbRow = (
    u32,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    i64,
    i64,
    bool,
    bool,
);

/// Key table: the names and codes of the keys, with their key and char codes.
#[rustfmt::skip]
const KB_TABLE: &[KbRow] = &[
    (8, "Backspace", "Backspace", "", "", 8, 8, false, false),
    (9, "Tab", "Tab", "", "", 9, 9, false, false),
    (13, "Enter", "Enter", "\u{d}", "\u{d}", 13, 13, false, true),
    (27, "Escape", "Escape", "", "", 27, 27, false, false),
    (32, "Space", " ", " ", " ", 32, 32, false, true),
    (33, "Digit1", "!", "!", "1", 49, 49, true, true),
    (34, "Quote", "\"", "\"", "'", 222, 222, true, true),
    (35, "Digit3", "#", "#", "3", 51, 51, true, true),
    (36, "Digit4", "$", "$", "4", 52, 52, true, true),
    (37, "Digit5", "%", "%", "5", 53, 53, true, true),
    (38, "Digit7", "&", "&", "7", 55, 55, true, true),
    (39, "Quote", "'", "'", "'", 222, 222, false, true),
    (40, "Digit9", "(", "(", "9", 57, 57, true, true),
    (41, "Digit0", ")", ")", "0", 48, 48, true, true),
    (42, "Digit8", "*", "*", "8", 56, 56, true, true),
    (43, "Equal", "+", "+", "=", 187, 187, true, true),
    (44, "Comma", ",", ",", ",", 188, 188, false, true),
    (45, "Minus", "-", "-", "-", 189, 189, false, true),
    (46, "Period", ".", ".", ".", 190, 190, false, true),
    (47, "Slash", "/", "/", "/", 191, 191, false, true),
    (48, "Digit0", "0", "0", "0", 48, 48, false, true),
    (49, "Digit1", "1", "1", "1", 49, 49, false, true),
    (50, "Digit2", "2", "2", "2", 50, 50, false, true),
    (51, "Digit3", "3", "3", "3", 51, 51, false, true),
    (52, "Digit4", "4", "4", "4", 52, 52, false, true),
    (53, "Digit5", "5", "5", "5", 53, 53, false, true),
    (54, "Digit6", "6", "6", "6", 54, 54, false, true),
    (55, "Digit7", "7", "7", "7", 55, 55, false, true),
    (56, "Digit8", "8", "8", "8", 56, 56, false, true),
    (57, "Digit9", "9", "9", "9", 57, 57, false, true),
    (58, "Semicolon", ":", ":", ";", 186, 186, true, true),
    (59, "Semicolon", ";", ";", ";", 186, 186, false, true),
    (60, "Comma", "<", "<", ",", 188, 188, true, true),
    (61, "Equal", "=", "=", "=", 187, 187, false, true),
    (62, "Period", ">", ">", ".", 190, 190, true, true),
    (63, "Slash", "?", "?", "/", 191, 191, true, true),
    (64, "Digit2", "@", "@", "2", 50, 50, true, true),
    (65, "KeyA", "A", "A", "a", 65, 65, true, true),
    (66, "KeyB", "B", "B", "b", 66, 66, true, true),
    (67, "KeyC", "C", "C", "c", 67, 67, true, true),
    (68, "KeyD", "D", "D", "d", 68, 68, true, true),
    (69, "KeyE", "E", "E", "e", 69, 69, true, true),
    (70, "KeyF", "F", "F", "f", 70, 70, true, true),
    (71, "KeyG", "G", "G", "g", 71, 71, true, true),
    (72, "KeyH", "H", "H", "h", 72, 72, true, true),
    (73, "KeyI", "I", "I", "i", 73, 73, true, true),
    (74, "KeyJ", "J", "J", "j", 74, 74, true, true),
    (75, "KeyK", "K", "K", "k", 75, 75, true, true),
    (76, "KeyL", "L", "L", "l", 76, 76, true, true),
    (77, "KeyM", "M", "M", "m", 77, 77, true, true),
    (78, "KeyN", "N", "N", "n", 78, 78, true, true),
    (79, "KeyO", "O", "O", "o", 79, 79, true, true),
    (80, "KeyP", "P", "P", "p", 80, 80, true, true),
    (81, "KeyQ", "Q", "Q", "q", 81, 81, true, true),
    (82, "KeyR", "R", "R", "r", 82, 82, true, true),
    (83, "KeyS", "S", "S", "s", 83, 83, true, true),
    (84, "KeyT", "T", "T", "t", 84, 84, true, true),
    (85, "KeyU", "U", "U", "u", 85, 85, true, true),
    (86, "KeyV", "V", "V", "v", 86, 86, true, true),
    (87, "KeyW", "W", "W", "w", 87, 87, true, true),
    (88, "KeyX", "X", "X", "x", 88, 88, true, true),
    (89, "KeyY", "Y", "Y", "y", 89, 89, true, true),
    (90, "KeyZ", "Z", "Z", "z", 90, 90, true, true),
    (91, "BracketLeft", "[", "[", "[", 219, 219, false, true),
    (92, "Backslash", "\\", "\\", "\\", 220, 220, false, true),
    (93, "BracketRight", "]", "]", "]", 221, 221, false, true),
    (94, "Digit6", "^", "^", "6", 54, 54, true, true),
    (95, "Minus", "_", "_", "-", 189, 189, true, true),
    (96, "Backquote", "`", "`", "`", 192, 192, false, true),
    (97, "KeyA", "a", "a", "a", 65, 65, false, true),
    (98, "KeyB", "b", "b", "b", 66, 66, false, true),
    (99, "KeyC", "c", "c", "c", 67, 67, false, true),
    (100, "KeyD", "d", "d", "d", 68, 68, false, true),
    (101, "KeyE", "e", "e", "e", 69, 69, false, true),
    (102, "KeyF", "f", "f", "f", 70, 70, false, true),
    (103, "KeyG", "g", "g", "g", 71, 71, false, true),
    (104, "KeyH", "h", "h", "h", 72, 72, false, true),
    (105, "KeyI", "i", "i", "i", 73, 73, false, true),
    (106, "KeyJ", "j", "j", "j", 74, 74, false, true),
    (107, "KeyK", "k", "k", "k", 75, 75, false, true),
    (108, "KeyL", "l", "l", "l", 76, 76, false, true),
    (109, "KeyM", "m", "m", "m", 77, 77, false, true),
    (110, "KeyN", "n", "n", "n", 78, 78, false, true),
    (111, "KeyO", "o", "o", "o", 79, 79, false, true),
    (112, "KeyP", "p", "p", "p", 80, 80, false, true),
    (113, "KeyQ", "q", "q", "q", 81, 81, false, true),
    (114, "KeyR", "r", "r", "r", 82, 82, false, true),
    (115, "KeyS", "s", "s", "s", 83, 83, false, true),
    (116, "KeyT", "t", "t", "t", 84, 84, false, true),
    (117, "KeyU", "u", "u", "u", 85, 85, false, true),
    (118, "KeyV", "v", "v", "v", 86, 86, false, true),
    (119, "KeyW", "w", "w", "w", 87, 87, false, true),
    (120, "KeyX", "x", "x", "x", 88, 88, false, true),
    (121, "KeyY", "y", "y", "y", 89, 89, false, true),
    (122, "KeyZ", "z", "z", "z", 90, 90, false, true),
    (123, "BracketLeft", "{", "{", "[", 219, 219, true, true),
    (124, "Backslash", "|", "|", "\\", 220, 220, true, true),
    (125, "BracketRight", "}", "}", "]", 221, 221, true, true),
    (126, "Backquote", "~", "~", "`", 192, 192, true, true),
    (127, "Delete", "Delete", "", "", 46, 46, false, false),
    (165, "IntlYen", "\u{a5}", "\u{a5}", "\u{a5}", 220, 220, false, true),
    (258, "AltLeft", "Alt", "", "", 164, 164, false, false),
    (260, "CapsLock", "CapsLock", "", "", 20, 20, false, false),
    (261, "ControlLeft", "Control", "", "", 162, 162, false, false),
    (262, "Fn", "Fn", "", "", 255, 0, false, false),
    (263, "FnLock", "FnLock", "", "", 0, 0, false, false),
    (264, "Hyper", "Hyper", "", "", 0, 0, false, false),
    (265, "MetaLeft", "Meta", "", "", 91, 91, false, false),
    (266, "NumLock", "NumLock", "", "", 144, 144, false, false),
    (268, "ScrollLock", "ScrollLock", "", "", 145, 145, false, false),
    (269, "ShiftLeft", "Shift", "", "", 160, 160, false, false),
    (270, "Super", "Super", "", "", 0, 0, false, false),
    (769, "ArrowDown", "ArrowDown", "", "", 40, 40, false, false),
    (770, "ArrowLeft", "ArrowLeft", "", "", 37, 37, false, false),
    (771, "ArrowRight", "ArrowRight", "", "", 39, 39, false, false),
    (772, "ArrowUp", "ArrowUp", "", "", 38, 38, false, false),
    (773, "End", "End", "", "", 35, 35, false, false),
    (774, "Home", "Home", "", "", 36, 36, false, false),
    (775, "PageDown", "PageDown", "", "", 34, 34, false, false),
    (776, "PageUp", "PageUp", "", "", 33, 33, false, false),
    (1025, "NumpadClear", "Clear", "", "", 12, 12, false, false),
    (1026, "Copy", "Copy", "", "", 0, 0, false, false),
    (1028, "Cut", "Cut", "", "", 0, 0, false, false),
    (1031, "Insert", "Insert", "", "", 45, 45, false, false),
    (1032, "Paste", "Paste", "", "", 0, 0, false, false),
    (1033, "Redo", "Redo", "", "", 0, 0, false, false),
    (1034, "Undo", "Undo", "", "", 0, 0, false, false),
    (1282, "Again", "Again", "", "", 0, 0, false, false),
    (1284, "Abort", "Cancel", "", "", 3, 3, false, false),
    (1285, "ContextMenu", "ContextMenu", "", "", 93, 93, false, false),
    (1287, "Find", "Find", "", "", 0, 0, false, false),
    (1288, "Help", "Help", "", "", 47, 47, false, false),
    (1289, "Pause", "Pause", "", "", 19, 19, false, false),
    (1291, "Props", "Props", "", "", 0, 0, false, false),
    (1292, "Select", "Select", "", "", 41, 41, false, false),
    (1293, "ZoomIn", "ZoomIn", "", "", 0, 0, false, false),
    (1294, "ZoomOut", "ZoomOut", "", "", 0, 0, false, false),
    (1537, "BrightnessDown", "BrightnessDown", "", "", 216, 0, false, false),
    (1538, "BrightnessUp", "BrightnessUp", "", "", 217, 0, false, false),
    (1540, "Eject", "Eject", "", "", 0, 0, false, false),
    (1541, "LogOff", "LogOff", "", "", 0, 0, false, false),
    (1542, "Power", "Power", "", "", 152, 0, false, false),
    (1544, "PrintScreen", "PrintScreen", "", "", 44, 44, false, false),
    (1547, "WakeUp", "WakeUp", "", "", 0, 0, false, false),
    (1797, "Convert", "Convert", "", "", 28, 28, false, false),
    (1803, "KeyboardLayoutSelect", "ModeChange", "", "", 0, 0, false, false),
    (1805, "NonConvert", "NonConvert", "", "", 29, 29, false, false),
    (1809, "Lang1", "HangulMode", "", "", 21, 21, false, false),
    (1810, "Lang2", "HanjaMode", "", "", 25, 25, false, false),
    (1814, "Lang4", "Hiragana", "", "", 0, 0, false, false),
    (1816, "KanaMode", "KanaMode", "", "", 21, 21, false, false),
    (1818, "Lang3", "Katakana", "", "", 0, 0, false, false),
    (1821, "Lang5", "ZenkakuHankaku", "", "", 0, 0, false, false),
    (2049, "F1", "F1", "", "", 112, 112, false, false),
    (2050, "F2", "F2", "", "", 113, 113, false, false),
    (2051, "F3", "F3", "", "", 114, 114, false, false),
    (2052, "F4", "F4", "", "", 115, 115, false, false),
    (2053, "F5", "F5", "", "", 116, 116, false, false),
    (2054, "F6", "F6", "", "", 117, 117, false, false),
    (2055, "F7", "F7", "", "", 118, 118, false, false),
    (2056, "F8", "F8", "", "", 119, 119, false, false),
    (2057, "F9", "F9", "", "", 120, 120, false, false),
    (2058, "F10", "F10", "", "", 121, 121, false, false),
    (2059, "F11", "F11", "", "", 122, 122, false, false),
    (2060, "F12", "F12", "", "", 123, 123, false, false),
    (2061, "F13", "F13", "", "", 124, 124, false, false),
    (2062, "F14", "F14", "", "", 125, 125, false, false),
    (2063, "F15", "F15", "", "", 126, 126, false, false),
    (2064, "F16", "F16", "", "", 127, 127, false, false),
    (2065, "F17", "F17", "", "", 128, 128, false, false),
    (2066, "F18", "F18", "", "", 129, 129, false, false),
    (2067, "F19", "F19", "", "", 130, 130, false, false),
    (2068, "F20", "F20", "", "", 131, 131, false, false),
    (2069, "F21", "F21", "", "", 132, 132, false, false),
    (2070, "F22", "F22", "", "", 133, 133, false, false),
    (2071, "F23", "F23", "", "", 134, 134, false, false),
    (2072, "F24", "F24", "", "", 135, 135, false, false),
    (2561, "Close", "Close", "", "", 0, 0, false, false),
    (2562, "MailForward", "MailForward", "", "", 0, 0, false, false),
    (2563, "MailReply", "MailReply", "", "", 0, 0, false, false),
    (2564, "MailSend", "MailSend", "", "", 0, 0, false, false),
    (2565, "MediaPlayPause", "MediaPlayPause", "", "", 179, 179, false, false),
    (2567, "MediaStop", "MediaStop", "", "", 178, 178, false, false),
    (2568, "MediaTrackNext", "MediaTrackNext", "", "", 176, 176, false, false),
    (2569, "MediaTrackPrevious", "MediaTrackPrevious", "", "", 177, 177, false, false),
    (2570, "New", "New", "", "", 0, 0, false, false),
    (2571, "Open", "Open", "", "", 43, 43, false, false),
    (2572, "Print", "Print", "", "", 0, 0, false, false),
    (2573, "Save", "Save", "", "", 0, 0, false, false),
    (2574, "SpellCheck", "SpellCheck", "", "", 0, 0, false, false),
    (2575, "AudioVolumeDown", "AudioVolumeDown", "", "", 174, 174, false, false),
    (2576, "AudioVolumeUp", "AudioVolumeUp", "", "", 175, 175, false, false),
    (2577, "AudioVolumeMute", "AudioVolumeMute", "", "", 173, 173, false, false),
    (2817, "LaunchApp2", "LaunchApplication2", "", "", 183, 183, false, false),
    (2818, "LaunchCalendar", "LaunchCalendar", "", "", 0, 0, false, false),
    (2819, "LaunchMail", "LaunchMail", "", "", 180, 180, false, false),
    (2820, "MediaSelect", "LaunchMediaPlayer", "", "", 181, 181, false, false),
    (2821, "LaunchMusicPlayer", "LaunchMusicPlayer", "", "", 0, 0, false, false),
    (2822, "LaunchApp1", "LaunchApplication1", "", "", 182, 182, false, false),
    (2823, "LaunchScreenSaver", "LaunchScreenSaver", "", "", 0, 0, false, false),
    (2824, "LaunchSpreadsheet", "LaunchSpreadsheet", "", "", 0, 0, false, false),
    (2825, "LaunchWebBrowser", "LaunchWebBrowser", "", "", 0, 0, false, false),
    (2828, "LaunchContacts", "LaunchContacts", "", "", 0, 0, false, false),
    (2829, "LaunchPhone", "LaunchPhone", "", "", 0, 0, false, false),
    (2830, "LaunchAssistant", "LaunchAssistant", "", "", 153, 0, false, false),
    (3073, "BrowserBack", "BrowserBack", "", "", 166, 166, false, false),
    (3074, "BrowserFavorites", "BrowserFavorites", "", "", 171, 171, false, false),
    (3075, "BrowserForward", "BrowserForward", "", "", 167, 167, false, false),
    (3076, "BrowserHome", "BrowserHome", "", "", 172, 172, false, false),
    (3077, "BrowserRefresh", "BrowserRefresh", "", "", 168, 168, false, false),
    (3078, "BrowserSearch", "BrowserSearch", "", "", 170, 170, false, false),
    (3079, "BrowserStop", "BrowserStop", "", "", 169, 169, false, false),
    (3338, "ChannelDown", "ChannelDown", "", "", 0, 0, false, false),
    (3339, "ChannelUp", "ChannelUp", "", "", 0, 0, false, false),
    (3346, "ClosedCaptionToggle", "ClosedCaptionToggle", "", "", 0, 0, false, false),
    (3349, "Exit", "Exit", "", "", 0, 0, false, false),
    (3362, "Guide", "Guide", "", "", 0, 0, false, false),
    (3365, "Info", "Info", "", "", 0, 0, false, false),
    (3372, "MediaFastForward", "MediaFastForward", "", "", 0, 0, false, false),
    (3373, "MediaLast", "MediaLast", "", "", 0, 0, false, false),
    (3374, "MediaPause", "MediaPause", "", "", 0, 0, false, false),
    (3375, "MediaPlay", "MediaPlay", "", "", 0, 0, false, false),
    (3376, "MediaRecord", "MediaRecord", "", "", 0, 0, false, false),
    (3377, "MediaRewind", "MediaRewind", "", "", 0, 0, false, false),
    (3395, "LaunchControlPanel", "Settings", "", "", 154, 0, false, false),
    (3406, "ZoomToggle", "ZoomToggle", "", "", 251, 251, false, false),
    (3586, "AudioBassBoostToggle", "AudioBassBoostToggle", "", "", 0, 0, false, false),
    (3842, "SpeechInputToggle", "SpeechInputToggle", "", "", 0, 0, false, false),
    (4097, "SelectTask", "AppSwitch", "", "", 0, 0, false, false),
];

static KEYS: LazyLock<HashMap<u32, KbKey>> = LazyLock::new(|| {
    KB_TABLE
        .iter()
        .map(
            |&(r, code, key, text, unmodified, native, windows, shift, print)| {
                (
                    r,
                    KbKey {
                        code,
                        key,
                        text,
                        unmodified,
                        native,
                        windows,
                        shift,
                        print,
                    },
                )
            },
        )
        .collect()
});

/// `kb.Keys[r]`
pub fn kb_key(r: char) -> Option<&'static KbKey> {
    KEYS.get(&(r as u32))
}

/// Keys by name ("Enter", "ArrowDown", "F5"…), built from the key table.
static KEY_NAMES: LazyLock<HashMap<String, &'static KbKey>> = LazyLock::new(|| {
    let mut m: HashMap<String, &'static KbKey> = HashMap::new();
    let mut runes: Vec<&u32> = KEYS.keys().collect();
    runes.sort(); // map iteration order is random; the table has no duplicate names
    for r in runes {
        let k = &KEYS[r];
        if k.key.chars().count() > 1 {
            m.insert(k.key.to_lowercase(), k);
        }
    }
    for (a, k) in [
        ("esc", "escape"),
        ("return", "enter"),
        ("del", "delete"),
        ("up", "arrowup"),
        ("down", "arrowdown"),
        ("left", "arrowleft"),
        ("right", "arrowright"),
        ("pgup", "pageup"),
        ("pgdn", "pagedown"),
        ("ins", "insert"),
    ] {
        if let Some(v) = m.get(k).copied() {
            m.insert(a.to_string(), v);
        }
    }
    if let Some(sp) = kb_key(' ') {
        m.insert("space".into(), sp);
    }
    m
});

/// `keyNames[name]`
pub fn key_by_name(name: &str) -> Option<&'static KbKey> {
    KEY_NAMES.get(name).copied()
}

const MOD_ALT: i64 = 1;

/// The editing command Chrome on macOS needs for Cmd+A/C/X/Z.
fn mac_command(key: &str) -> Option<&'static str> {
    Some(match key {
        "a" => "selectAll",
        "c" => "copy",
        "x" => "cut",
        "z" => "undo",
        _ => return None,
    })
}

/// parseKey reads "Enter", "a", "Shift+Tab", "Control+A", "Meta+Enter"…
pub fn parse_key(combo: &str) -> BResult<(&'static KbKey, i64)> {
    let mut parts: Vec<&str> = combo.split('+').collect();
    if combo.ends_with("++") || combo == "+" {
        // the "+" key itself
        let n = parts.len();
        parts.truncate(n - 2);
        parts.push("+");
    }
    let mut mods = 0i64;
    for p in &parts[..parts.len() - 1] {
        match p.trim().to_lowercase().as_str() {
            "control" | "ctrl" => mods |= MOD_CTRL,
            "alt" | "option" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "meta" | "cmd" | "command" | "super" | "win" => mods |= MOD_META,
            "controlormeta" | "mod" => {
                mods |= if cfg!(target_os = "macos") {
                    MOD_META
                } else {
                    MOD_CTRL
                }
            }
            _ => {
                return Err(format!(
                    "unknown modifier {p:?} in {combo:?} (use Control, Alt, Shift or Meta)"
                ))
            }
        }
    }
    let name = parts[parts.len() - 1];
    let k = if name.chars().count() == 1 {
        let mut r = name.chars().next().unwrap_or('\0');
        if mods & (MOD_CTRL | MOD_META | MOD_ALT) != 0 {
            r = name.to_lowercase().chars().next().unwrap_or(r); // shortcuts use the plain key
        }
        kb_key(r)
    } else {
        key_by_name(&name.trim().to_lowercase())
    };
    let Some(k) = k else {
        return Err(format!("unknown key {name:?} (examples: Enter, Tab, Escape, ArrowDown, Backspace, a, Control+A, Shift+Tab)"));
    };
    if k.shift {
        mods |= MOD_SHIFT;
    }
    Ok((k, fix_mods(mods)))
}

impl Agent {
    pub async fn press_key(&self, k: &'static KbKey, mods: i64) -> BResult<()> {
        let mut down = json!({"modifiers": mods, "key": k.key, "code": k.code,
            "windowsVirtualKeyCode": k.windows, "nativeVirtualKeyCode": k.native});
        if k.text.is_empty() || mods & (MOD_CTRL | MOD_META | MOD_ALT) != 0 {
            down["type"] = json!("rawKeyDown");
        } else {
            down["type"] = json!("keyDown");
            down["text"] = json!(k.text);
            down["unmodifiedText"] = json!(k.unmodified);
        }
        // Chrome on macOS needs the editing command for Cmd+A/C/X/Z (and Ctrl, which people expect to work the same).
        if cfg!(target_os = "macos") && mods & (MOD_CTRL | MOD_META) != 0 {
            if let Some(mut cmd) = mac_command(&k.key.to_lowercase()) {
                if cmd == "undo" && mods & MOD_SHIFT != 0 {
                    cmd = "redo";
                }
                down["commands"] = json!([cmd]);
            }
        }
        let mut up = json!({"type": "keyUp", "modifiers": mods, "key": k.key, "code": k.code,
            "windowsVirtualKeyCode": k.windows, "nativeVirtualKeyCode": k.native});
        // cdproto omits empty fields (omitempty)
        for p in [&mut down, &mut up] {
            let o = p.as_object_mut().unwrap();
            if mods == 0 {
                o.remove("modifiers");
            }
            for f in ["key", "code"] {
                if o[f] == json!("") {
                    o.remove(f);
                }
            }
            for f in ["windowsVirtualKeyCode", "nativeVirtualKeyCode"] {
                if o[f] == json!(0) {
                    o.remove(f);
                }
            }
        }
        self.act(move |t| async move {
            input_call(&t, "Input.dispatchKeyEvent", down).await?;
            input_call(&t, "Input.dispatchKeyEvent", up).await
        })
        .await
    }

    async fn insert_text(&self, s: String) -> BResult<()> {
        self.act(
            move |t| async move { input_call(&t, "Input.insertText", json!({"text": s})).await },
        )
        .await
    }

    /// typeText types key by key so viewers see it appear. Characters without a key (é, ộ, emoji…) are inserted as text.
    pub async fn type_text(&self, s: &str) -> BResult<()> {
        let runes: Vec<char> = s.chars().collect();
        if self.opt.speed == 0.0 || runes.len() > 400 {
            return self.insert_text(s.to_string()).await;
        }
        for r in runes {
            let k = kb_key(r).filter(|k| (r as u32) < 128 && (k.print || r == '\n'));
            match k {
                Some(k) => {
                    let mods = if k.shift { MOD_SHIFT } else { 0 };
                    self.press_key(k, mods).await?;
                }
                None => self.insert_text(r.to_string()).await?,
            }
            self.pause(Duration::from_millis(28)).await;
        }
        Ok(())
    }
}

// ---------- Actions (one per MCP tool) ----------

impl Agent {
    pub async fn navigate(&self, raw: &str) -> BResult<()> {
        let u = normalize_url(raw);
        if u.is_empty() {
            return Err(format!(
                "cannot open {raw:?}: only http(s) addresses are allowed"
            ));
        }
        self.announce("🌐", &format!("Open {u}"), None);
        let uu = u.clone();
        self.act(move |t| async move {
            let res = t
                .call("Page.navigate", json!({"url": uu}), None)
                .await
                .map_err(|e| e.to_string())?;
            match res["errorText"].as_str() {
                Some(e) if !e.is_empty() => Err(format!("could not open {uu}: {e}")),
                _ => Ok(()),
            }
        })
        .await?;
        self.settle(Duration::from_secs(20)).await;
        Ok(())
    }

    pub async fn history(&self, delta: i64) -> BResult<()> {
        let word = if delta > 0 { "forward" } else { "back" };
        self.announce("↩️", &format!("Go {word}"), None);
        let moved = self
            .act(move |t| async move { go_history(&t, delta).await })
            .await?;
        if !moved {
            return Err(format!("there is no page to go {word} to"));
        }
        self.settle(Duration::from_secs(15)).await;
        Ok(())
    }

    pub async fn reload(&self) -> BResult<()> {
        self.announce("🔄", "Reload the page", None);
        self.act(|t| async move {
            t.call("Page.reload", json!({}), None)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .await?;
        self.settle(Duration::from_secs(15)).await;
        Ok(())
    }

    /// Click returns a note when another element sits on top of the target.
    pub async fn click(
        &self,
        r: &str,
        element: &str,
        button: &str,
        double: bool,
        px: Option<f64>,
        py: Option<f64>,
    ) -> BResult<String> {
        let bx = self.locate_or_point(r, px, py).await?;
        let verb = if double {
            "Double-click"
        } else if button == "right" {
            "Right-click"
        } else {
            "Click"
        };
        self.announce(
            "🖱️",
            &format!("{verb} {}", target_name(element, &bx)),
            Some(&bx),
        );
        let (x, y) = bx.center();
        self.move_to(x, y, "", 0).await?;
        self.pause(Duration::from_millis(220)).await;
        self.click_at(button, if double { 2 } else { 1 }).await?;
        self.settle(Duration::from_secs(8)).await;
        if !bx.cover.is_empty() {
            return Ok(format!("Note: {} was covering the element at the click point, so the click may have hit it instead.", bx.cover));
        }
        Ok(String::new())
    }

    pub async fn hover(&self, r: &str, element: &str) -> BResult<()> {
        let bx = self.locate(r).await?;
        self.announce(
            "👆",
            &format!("Hover {}", target_name(element, &bx)),
            Some(&bx),
        );
        let (x, y) = bx.center();
        self.move_to(x, y, "", 0).await?;
        self.settle(Duration::from_secs(1)).await;
        Ok(())
    }

    pub async fn type_(
        &self,
        r: &str,
        element: &str,
        text: &str,
        submit: bool,
        clear: bool,
    ) -> BResult<()> {
        if r.is_empty() {
            return self.type_focused(element, text, submit, clear).await;
        }
        let bx = self.locate(r).await?;
        let mut shown = text.to_string();
        if has_password(&[element, &bx.label]) {
            shown = "•".repeat(text.chars().count().min(12));
        }
        self.announce(
            "⌨️",
            &format!("Type {} into {}", quote(&shown), target_name(element, &bx)),
            Some(&bx),
        );
        let (x, y) = bx.center();
        self.move_to(x, y, "", 0).await?;
        self.pause(Duration::from_millis(150)).await;
        self.click_at("left", 1).await?;
        let fr = parse_frame_ref(r);
        let rr = r.to_string();
        let had_text: bool = self
            .act(move |t| async move {
                let expr = format!("__abu.prepareInput({}, {clear})", jsq(&fr.rest));
                eval_ref::<bool>(&t, &rr, &expr).await
            })
            .await?;
        if clear && had_text {
            self.pause(Duration::from_millis(120)).await;
            self.press_key(key_by_name("backspace").expect("backspace"), 0)
                .await?;
        }
        self.type_text(text).await?;
        if submit {
            self.pause(Duration::from_millis(200)).await;
            self.press_key(key_by_name("enter").expect("enter"), 0)
                .await?;
        }
        self.settle(Duration::from_secs(8)).await;
        Ok(())
    }

    /// typeFocused types into whatever has focus (after Tab, or a field the snapshot has no ref for).
    async fn type_focused(
        &self,
        element: &str,
        text: &str,
        submit: bool,
        clear: bool,
    ) -> BResult<()> {
        let mut shown = text.to_string();
        if has_password(&[element]) {
            shown = "•".repeat(text.chars().count().min(12));
        }
        self.announce(
            "⌨️",
            &format!(
                "Type {} into {}",
                quote(&shown),
                cmp_or(element, "the focused field")
            ),
            None,
        );
        let mut had_text = false;
        if clear {
            had_text = self
                .act(|t| async move { eval_js_as::<bool>(&t, SELECT_FOCUSED_JS).await })
                .await?;
        }
        if had_text {
            self.press_key(key_by_name("backspace").expect("backspace"), 0)
                .await?;
        }
        self.type_text(text).await?;
        if submit {
            self.pause(Duration::from_millis(200)).await;
            self.press_key(key_by_name("enter").expect("enter"), 0)
                .await?;
        }
        self.settle(Duration::from_secs(8)).await;
        Ok(())
    }
}

/// selectFocusedJS selects the content of the focused field so typing replaces it; says whether it had any.
const SELECT_FOCUSED_JS: &str = r#"(() => {
  const el = document.activeElement;
  if (!el || el === document.body) return false;
  if (el.tagName === "INPUT" || el.tagName === "TEXTAREA") {
    try { el.select(); } catch (e) { return false; }
    return el.value !== "";
  }
  if (el.isContentEditable) {
    const r = document.createRange(); r.selectNodeContents(el);
    const s = getSelection(); s.removeAllRanges(); s.addRange(r);
    return el.textContent !== "";
  }
  return false;
})()"#;

fn has_password(s: &[&str]) -> bool {
    s.iter().any(|x| {
        let x = x.to_lowercase();
        x.contains("password") || x.contains("mật khẩu") || x.contains("passcode")
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PickerOut {
    shown: i64,
    total: i64,
    selected: String,
}

/// `%q` for a string.
pub(crate) fn go_quote(s: &str) -> String {
    format!("{s:?}")
}

impl Agent {
    pub async fn press(&self, combo: &str) -> BResult<()> {
        let (k, mods) = parse_key(combo)?;
        self.announce("⌨️", &format!("Press {combo}"), None);
        let mut m = self.agent_msg("key");
        m.text = combo.to_string();
        self.hub.agent_send(m);
        self.press_key(k, mods).await?;
        self.settle(Duration::from_secs(8)).await;
        Ok(())
    }

    pub async fn select_option(
        &self,
        r: &str,
        element: &str,
        values: &[String],
    ) -> BResult<Vec<String>> {
        if values.is_empty() {
            return Err("values is required: the option values or labels to select".into());
        }
        let bx = self.locate(r).await?;
        self.announce(
            "📋",
            &format!(
                "Choose {} in {}",
                quote(&values.join(", ")),
                target_name(element, &bx)
            ),
            Some(&bx),
        );
        let (x, y) = bx.center();
        self.move_to(x, y, "", 0).await?;
        self.pause(Duration::from_millis(200)).await;
        let (cx, cy) = {
            let s = self.st();
            (s.x, s.y)
        };
        let mut m = self.agent_msg("click");
        m.x = cx;
        m.y = cy;
        self.hub.agent_send(m);
        let fr = parse_frame_ref(r);
        let (rr, vals) = (r.to_string(), values.to_vec());
        let picked = self
            .act(move |t| async move {
                if fr.frame == 0 {
                    let v = serde_json::to_string(&vals).unwrap_or_default();
                    return eval_js_as::<Vec<String>>(
                        &t,
                        &format!("__abu.selectOption({}, {v})", jsq(&rr)),
                    )
                    .await;
                }
                // Inside an iframe: a real <select> is set directly; a custom combobox
                // (Stripe's country picker, most design systems) is opened and clicked
                // with its options, because no native popup is painted in headless.
                frames::eval_in_frame_as::<Vec<String>>(
                    &t,
                    fr.frame,
                    &frames::pick_option_expr(&fr.rest, &vals),
                )
                .await
            })
            .await?;
        self.settle(Duration::from_secs(5)).await;
        Ok(picked)
    }

    /// OpenPicker shows a <select>'s options inline, so the list is visible in the
    /// stream and in screenshots. A native dropdown is a browser popup, which headless
    /// Chrome never paints: without this, clicking one looks like nothing happened.
    pub async fn open_picker(&self, r: &str, element: &str) -> BResult<String> {
        if r.is_empty() {
            return Err("ref is required: take a snapshot (browser_snapshot) and pass the ref of the <select>".into());
        }
        self.announce(
            "📋",
            &format!(
                "Show the options of {}",
                target_name(element, &Bx::default())
            ),
            None,
        );
        let fr = parse_frame_ref(r);
        let out: PickerOut = self
            .act(move |t| async move {
                let expr = format!("__abu.openPicker({})", jsq(&fr.rest));
                if fr.frame == 0 {
                    return eval_js_as::<PickerOut>(&t, &expr).await;
                }
                frames::eval_in_frame_as::<PickerOut>(&t, fr.frame, &expr).await
            })
            .await?;
        self.settle(Duration::from_secs(1)).await;
        Ok(format!("Showing {} of {} options inline (selected: {}). Use browser_select_option, or click an option, then browser_close_picker.", out.shown, out.total, go_quote(&out.selected)))
    }

    /// ClosePicker puts a <select> back the way it was before OpenPicker.
    pub async fn close_picker(&self, r: &str, _element: &str) -> BResult<String> {
        if r.is_empty() {
            return Err("ref is required".into());
        }
        let fr = parse_frame_ref(r);
        let closed: bool = self
            .act(move |t| async move {
                let expr = format!("__abu.closePicker({})", jsq(&fr.rest));
                if fr.frame == 0 {
                    return eval_js_as::<bool>(&t, &expr).await;
                }
                frames::eval_in_frame_as::<bool>(&t, fr.frame, &expr).await
            })
            .await?;
        if !closed {
            return Ok(
                "Nothing to close: this dropdown was not opened with browser_open_picker.".into(),
            );
        }
        Ok("Closed the dropdown.".into())
    }

    pub async fn scroll(
        &self,
        direction: &str,
        mut amount: f64,
        r: &str,
        element: &str,
    ) -> BResult<()> {
        let v = self.b.view_size();
        let (mut x, mut y) = (0.5, 0.5);
        let mut wher = "the page".to_string();
        if !r.is_empty() {
            let bx = self.locate(r).await?;
            (x, y) = bx.center();
            wher = target_name(element, &bx);
        }
        let (mut dx, mut dy) = (0.0, 0.0);
        match direction.to_lowercase().as_str() {
            "" | "down" => dy = 1.0,
            "up" => dy = -1.0,
            "right" => dx = 1.0,
            "left" => dx = -1.0,
            _ => {
                return Err(format!(
                    "direction must be up, down, left or right, not {}",
                    go_quote(direction)
                ))
            }
        }
        if amount <= 0.0 {
            amount = 0.7 * v.height as f64;
            if dx != 0.0 {
                amount = 0.7 * v.width as f64;
            }
        }
        self.announce(
            "🖱️",
            &format!(
                "Scroll {wher} {} by {amount:.0}px",
                cmp_or(direction, "down").to_lowercase()
            ),
            None,
        );
        self.move_to(x, y, "", 0).await?;
        let steps = if self.opt.speed > 0.0 { 10 } else { 1 };
        let (cx, cy) = {
            let s = self.st();
            (s.x, s.y)
        };
        let (px, py) = (cx * v.width as f64, cy * v.height as f64);
        for _ in 0..steps {
            let p = json!({"type": "mouseWheel", "x": px, "y": py,
                "deltaX": dx * amount / steps as f64, "deltaY": dy * amount / steps as f64, "modifiers": 0});
            self.act(move |t| async move { input_call(&t, "Input.dispatchMouseEvent", p).await })
                .await?;
            self.pause(Duration::from_millis(30)).await;
        }
        self.settle(Duration::from_secs(2)).await;
        Ok(())
    }

    /// locateOrPoint finds an element by ref, or a bare point given in page pixels (for canvases).
    pub async fn locate_or_point(&self, r: &str, px: Option<f64>, py: Option<f64>) -> BResult<Bx> {
        if let (true, Some(px), Some(py)) = (r.is_empty(), px, py) {
            let v = self.b.view_size();
            if v.width <= 0 || v.height <= 0 {
                return Err("the page has no size yet".into());
            }
            return Ok(Bx {
                x: px,
                y: py,
                cx: px,
                cy: py,
                vw: v.width as f64,
                vh: v.height as f64,
                label: format!("point ({px:.0}, {py:.0})"),
                ..Default::default()
            });
        }
        self.locate(r).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn drag(
        &self,
        start_ref: &str,
        start_el: &str,
        end_ref: &str,
        end_el: &str,
        sx: Option<f64>,
        sy: Option<f64>,
        ex: Option<f64>,
        ey: Option<f64>,
    ) -> BResult<()> {
        let from = self.locate_or_point(start_ref, sx, sy).await?;
        self.announce(
            "✋",
            &format!(
                "Drag {} to {}",
                target_name(start_el, &from),
                if end_el.is_empty() {
                    end_ref.to_string()
                } else {
                    quote(end_el)
                }
            ),
            Some(&from),
        );
        let (x, y) = from.center();
        self.move_to(x, y, "", 0).await?;
        self.pause(Duration::from_millis(150)).await;
        let (cx, cy) = {
            let s = self.st();
            (s.x, s.y)
        };
        self.mouse("mousePressed", cx, cy, "left", 1, 1).await?;
        let to = match self.locate_or_point(end_ref, ex, ey).await {
            Ok(to) => to,
            Err(e) => {
                let (cx, cy) = {
                    let s = self.st();
                    (s.x, s.y)
                };
                let _ = self.mouse("mouseReleased", cx, cy, "left", 0, 1).await;
                return Err(e);
            }
        };
        let mut m = Msg {
            type_: "agent".into(),
            act: "step".into(),
            text: format!("Drop on {}", target_name(end_el, &to)),
            ..Default::default()
        };
        m.rect = to.rect();
        self.hub.agent_send(m);
        let (x, y) = to.center();
        self.move_to(x, y, "left", 1).await?;
        self.pause(Duration::from_millis(150)).await;
        let (cx, cy) = {
            let s = self.st();
            (s.x, s.y)
        };
        self.mouse("mouseReleased", cx, cy, "left", 0, 1).await?;
        self.settle(Duration::from_secs(3)).await;
        Ok(())
    }

    /// WaitFor waits for a time, for text to appear, or for text to go away.
    pub async fn wait_for(&self, mut seconds: f64, text: &str, gone: &str) -> BResult<()> {
        if seconds <= 0.0 && text.is_empty() && gone.is_empty() {
            return Err("give time (seconds), text or textGone".into());
        }
        if text.is_empty() && gone.is_empty() {
            seconds = seconds.min(60.0);
            self.announce("⏳", &format!("Wait {seconds:.1}s"), None);
            tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
            return Ok(());
        }
        let mut limit = Duration::from_secs(30);
        if seconds > 0.0 {
            limit = Duration::from_secs_f64(seconds.min(120.0));
        }
        let (what, want) = if text.is_empty() {
            (gone, false)
        } else {
            (text, true)
        };
        if want {
            self.announce("⏳", &format!("Wait for {}", quote(what)), None);
        } else {
            self.announce("⏳", &format!("Wait until {} is gone", quote(what)), None);
        }
        let end = Instant::now() + limit;
        loop {
            let has = self
                .look(|t| async move {
                    eval_js_as::<bool>(&t, &format!("__abu.hasText({})", jsq(what))).await
                })
                .await;
            if let Ok(has) = has {
                if has == want {
                    return Ok(());
                }
            }
            if Instant::now() > end {
                let lim = go_duration(limit);
                if want {
                    return Err(format!("text {} did not appear within {lim}", quote(what)));
                }
                return Err(format!("text {} was still there after {lim}", quote(what)));
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    pub async fn screenshot(&self) -> BResult<String> {
        let b = self.b.clone();
        let data = self.look(move |t| async move { b.shot(&t).await }).await?;
        if let Some(a) = crate::assets::global() {
            let (u, title) = BrowserApi::page_of(&*self.b);
            let data2 = data.clone();
            let _ = tokio::task::spawn_blocking(move || {
                a.add(
                    crate::assets::Asset {
                        kind: "screenshot".into(),
                        name: crate::assets::shot_name(),
                        mime: "image/jpeg".into(),
                        url: u,
                        title,
                        by: "agent".into(),
                        ..Default::default()
                    },
                    Some(&data2),
                );
            })
            .await;
        }
        Ok(base64::engine::general_purpose::STANDARD.encode(data))
    }

    pub async fn evaluate(&self, func: &str, r: &str) -> BResult<String> {
        if func.trim().is_empty() {
            return Err("function is required, e.g. \"() => document.title\"".into());
        }
        self.announce("⚙️", "Run JavaScript on the page", None);
        let arg = if r.is_empty() {
            "undefined".to_string()
        } else {
            format!("__abu.get({})", jsq(r))
        };
        let expr = format!(
            r#"(async () => {{
  const __f = ({func});
  const r = await (typeof __f === "function" ? __f({arg}) : __f);
  if (r === undefined) return "undefined";
  if (r instanceof Node) return (r.outerHTML || r.textContent || "").slice(0, 20000);
  try {{ return JSON.stringify(r, null, 2) ?? String(r); }} catch (e) {{ return String(r); }}
}})()"#
        );
        let fr = parse_frame_ref(r);
        let rr = r.to_string();
        let res = self
            .act(move |t| async move {
                if fr.frame == 0 {
                    return eval_js_as::<String>(&t, &expr).await;
                }
                // A ref from inside an iframe (f1e12): run in that frame's own world, with
                // that frame's ref.
                let inner = expr.replace(
                    &format!("__abu.get({})", jsq(&rr)),
                    &format!("__abu.get({})", jsq(&fr.rest)),
                );
                frames::eval_in_frame_as::<String>(&t, fr.frame, &inner).await
            })
            .await;
        // keeps whatever came back; a failed call has no output
        let mut out = res?;
        if out.len() > 50000 {
            let mut n = 50000;
            while !out.is_char_boundary(n) {
                n -= 1;
            }
            out.truncate(n);
            out.push_str("\n… (cut)");
        }
        Ok(out)
    }

    pub fn say(&self, text: &str, who: &str) {
        let mut m = self.agent_msg("say");
        m.text = text.to_string();
        self.hub.agent_send(m);
        self.hub.agent_chat(who, text);
    }

    /// Tabs lists, opens, selects or closes tabs.
    pub async fn tabs(
        &self,
        action: &str,
        mut index: i64,
        raw_url: &str,
        tab_id: &str,
        background: bool,
    ) -> BResult<String> {
        let (tabs, _, _, _) = self.b.state();
        let pick = |index: i64| -> BResult<crate::browser::TabInfo> {
            if index < 0 || index >= tabs.len() as i64 {
                return Err(format!(
                    "no tab with index {index} (there are {} tabs, numbered from 0)",
                    tabs.len()
                ));
            }
            Ok(tabs[index as usize].clone())
        };
        match action {
            "" | "list" => return Ok(self.tab_list()),
            "new" => {
                let u = cmp_or(raw_url, &self.b.opt.start_url);
                let (_, prior, _, _) = self.b.state();
                self.announce("➕", &format!("Open a new tab: {u}"), None);
                self.hub.agent_take()?;
                let uu = u.clone();
                let new_id = self
                    .b
                    .call(move |b| async move {
                        let full = b.st().tabs.len() as i64 >= b.opt.max_tabs;
                        if full {
                            return Err(format!(
                                "tab limit ({}) reached, close a tab first",
                                b.opt.max_tabs
                            ));
                        }
                        let (t, err) = b.open_tab(&uu).await;
                        let id = t.map(|t| t.id.clone()).unwrap_or_default();
                        if background && !prior.is_empty() {
                            b.activate_tab_cmd(&prior); // sub-agents work in their own tab; the viewer keeps its tab
                        }
                        match err {
                            Some(e) => Err(e),
                            None => Ok(id),
                        }
                    })
                    .await?;
                if !new_id.is_empty() {
                    self.worker("", &new_id)
                        .settle(Duration::from_secs(20))
                        .await;
                } else {
                    self.settle(Duration::from_secs(20)).await;
                }
                if background {
                    return Ok(format!("Opened tab tabId={new_id}. Pass this tabId to every tool call of the sub-agent that owns it.\n\n{}", self.tab_list()));
                }
            }
            "select" => {
                if !tab_id.is_empty() {
                    if let Some(i) = tabs.iter().position(|c| c.id == tab_id) {
                        index = i as i64;
                    }
                }
                let t = pick(index)?;
                self.announce("🗂️", &format!("Switch to tab {}", quote(&t.title)), None);
                self.hub.agent_take()?;
                self.b.activate_tab_cmd(&t.id);
                self.settle(Duration::from_secs(2)).await;
            }
            "close" => {
                if !tab_id.is_empty() {
                    if let Some(i) = tabs.iter().position(|c| c.id == tab_id) {
                        index = i as i64;
                    }
                }
                if index < 0 {
                    // the current one
                    let (_, active, _, _) = self.b.state();
                    for (i, t) in tabs.iter().enumerate() {
                        if t.id == active {
                            index = i as i64;
                        }
                    }
                }
                let t = pick(index)?;
                self.announce("✖️", &format!("Close tab {}", quote(&t.title)), None);
                self.hub.agent_take()?;
                let id = t.id.clone();
                let _ = self
                    .b
                    .call(move |b| async move {
                        b.close_tab(&id).await;
                        Ok(())
                    })
                    .await;
                lock(&self.workers).retain(|k, _| !k.ends_with(&format!("@{}", t.id)));
                self.settle(Duration::from_secs(2)).await;
            }
            _ => {
                return Err(format!(
                    "action must be list, new, select or close, not {}",
                    go_quote(action)
                ))
            }
        }
        Ok(String::new())
    }

    fn tab_list(&self) -> String {
        let (tabs, active, _, _) = self.b.state();
        let mut s = String::from("### Open tabs\n");
        for (i, t) in tabs.iter().enumerate() {
            let cur = if t.id == active { " (current)" } else { "" };
            s.push_str(&format!(
                "- {i}:{cur} [{}]({}) tabId={}\n",
                t.title, t.url, t.id
            ));
        }
        s
    }

    /// Snapshot describes the current page for the model: address, title, tabs, and the outline with refs.
    pub async fn snapshot_text(&self) -> BResult<String> {
        self.snapshot(false).await
    }

    /// Changes describes the page after an action. On the same page it lists only what changed
    /// since the last snapshot, so the model has much less to read; a new page gets the full outline.
    pub async fn changes(&self) -> BResult<String> {
        self.snapshot(true).await
    }

    async fn snapshot(&self, diff: bool) -> BResult<String> {
        let (mut snap, docs) = self
            .eval_retry(|t| async move {
                let snap = eval_js_as::<String>(&t, "__abu.snapshot()").await?;
                // Cross-origin iframes are separate targets: their contents are read in
                // their own context and appended with f<N> refs, so the model can act on
                // them the same way as the page itself.
                let docs = frames::frame_docs(&t, &snap).await;
                Ok((snap, docs))
            })
            .await?;
        if docs.len() > 1 {
            snap = frames::join_frame_outlines(&docs);
        }
        let (tabs, mut active, _, _) = self.b.state();
        if !self.tab_id.is_empty() {
            active = self.tab_id.clone();
        }
        let mut key = String::new();
        let mut s = String::from("### Page state\n");
        for t in &tabs {
            if t.id == active {
                s.push_str(&format!(
                    "- Page URL: {}\n- Page Title: {}\n",
                    t.url, t.title
                ));
                key = format!("{} {}", t.id, t.url);
            }
        }
        if tabs.len() > 1 {
            s.push_str(&self.tab_list());
        }
        let (prev, prev_key) = {
            let mut st = self.st();
            (
                std::mem::replace(&mut st.last, snap.clone()),
                std::mem::replace(&mut st.last_key, key.clone()),
            )
        };
        if diff && !prev.is_empty() && key == prev_key {
            if let Some(d) = diff_outline(&prev, &snap, false) {
                s.push_str(&d);
                return Ok(s);
            }
        }
        s.push_str("### Page snapshot\n```yaml\n");
        s.push_str(&snap);
        s.push_str("\n```\n");
        Ok(s)
    }
}

/// `time.Duration.String` for the whole-second values used in messages ("30s", "2m0s").
fn go_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        if secs.fract() == 0.0 {
            format!("{}s", secs as u64)
        } else {
            format!(
                "{}s",
                format!("{secs:.9}")
                    .trim_end_matches('0')
                    .trim_end_matches('.')
            )
        }
    } else {
        let m = (secs / 60.0).floor() as u64;
        let rest = secs - m as f64 * 60.0;
        format!("{m}m{}s", rest as u64)
    }
}

/// diffOutline lists what changed between two page outlines, with a little context around each change.
/// `None` when so much changed that the full outline is clearer (unless force: an explicit diff is always shown).
pub fn diff_outline(prev: &str, cur: &str, force: bool) -> Option<String> {
    let a: Vec<&str> = prev.split('\n').collect();
    let b: Vec<&str> = cur.split('\n').collect();
    let ops = diff_lines(&a, &b)?;
    const CTX: usize = 2;
    let mut keep = vec![false; ops.len()];
    let mut changed = 0;
    for (i, o) in ops.iter().enumerate() {
        if o.0 != ' ' {
            changed += 1;
            for k in keep
                .iter_mut()
                .take((i + CTX).min(ops.len() - 1) + 1)
                .skip(i.saturating_sub(CTX))
            {
                *k = true;
            }
        }
    }
    if changed == 0 {
        return Some(
            "### Page snapshot\nNothing changed on the page since the last snapshot.\n".into(),
        );
    }
    let mut s = String::from("### Page changes\nOnly what changed since the earlier snapshot (+ added, - removed, the rest is context). Refs from earlier snapshots still work; call browser_snapshot for the whole page.\n```diff\n");
    let mut gap = false;
    for (i, o) in ops.iter().enumerate() {
        if !keep[i] {
            gap = true;
            continue;
        }
        if gap {
            s.push_str("  …\n");
            gap = false;
        }
        s.push(o.0);
        s.push(' ');
        s.push_str(&o.1);
        s.push('\n');
    }
    if gap {
        s.push_str("  …\n");
    }
    s.push_str("```\n");
    if !force && s.len() > cur.len() * 2 / 3 {
        return None;
    }
    Some(s)
}

/// diffLines is a line diff (longest common subsequence). `None` when the changed part is too big to compare.
/// Each op is (kind, text): ' ' same, '-' removed, '+' added.
fn diff_lines(a: &[&str], b: &[&str]) -> Option<Vec<(char, String)>> {
    let mut p = 0;
    while p < a.len() && p < b.len() && a[p] == b[p] {
        p += 1;
    }
    let mut q = 0;
    while q < a.len() - p && q < b.len() - p && a[a.len() - 1 - q] == b[b.len() - 1 - q] {
        q += 1;
    }
    let am = &a[p..a.len() - q];
    let bm = &b[p..b.len() - q];
    let (n, m) = (am.len(), bm.len());
    if n * m > 1_000_000 {
        return None;
    }
    let mut ops: Vec<(char, String)> = Vec::with_capacity(a.len() + b.len());
    for l in &a[..p] {
        ops.push((' ', l.to_string()));
    }
    let w = m + 1;
    let mut lcs = vec![0i32; (n + 1) * w]; // lcs[i*w+j]: common lines of am[i:] and bm[j:]
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * w + j] = if am[i] == bm[j] {
                lcs[(i + 1) * w + j + 1] + 1
            } else {
                lcs[(i + 1) * w + j].max(lcs[i * w + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && am[i] == bm[j] {
            ops.push((' ', am[i].to_string()));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[(i + 1) * w + j] >= lcs[i * w + j + 1]) {
            ops.push(('-', am[i].to_string()));
            i += 1;
        } else {
            ops.push(('+', bm[j].to_string()));
            j += 1;
        }
    }
    for l in &a[a.len() - q..] {
        ops.push((' ', l.to_string()));
    }
    Some(ops)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_key_names_and_combos() {
        let (k, m) = parse_key("Enter").unwrap();
        assert_eq!((k.key, m), ("Enter", 0));
        assert_eq!(parse_key("return").unwrap().0.key, "Enter");
        assert_eq!(parse_key("Esc").unwrap().0.key, "Escape");
        let (k, m) = parse_key("Shift+Tab").unwrap();
        assert_eq!((k.key, m), ("Tab", MOD_SHIFT));
        let (k, m) = parse_key("Control+A").unwrap();
        assert_eq!(k.key, "a"); // shortcuts use the plain key
        assert_eq!(m, MOD_CTRL);
        let (k, m) = parse_key("A").unwrap();
        assert_eq!((k.key, m), ("A", MOD_SHIFT)); // an upper-case letter carries Shift
        assert_eq!(parse_key("+").unwrap().0.key, "+");
        assert_eq!(parse_key("space").unwrap().0.key, " ");
        assert!(parse_key("Bogus").is_err());
        assert!(parse_key("Hyper+a")
            .unwrap_err()
            .contains("unknown modifier"));
    }

    #[test]
    fn outline_diff_lists_changes_with_context() {
        let prev = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj";
        let cur = "a\nb\nc\nd\nX\nf\ng\nh\ni\nj";
        let d = diff_outline(prev, cur, true).unwrap();
        assert!(d.contains("- e\n+ X\n"));
        assert!(d.starts_with("### Page changes\n"));
        assert_eq!(
            diff_outline(prev, prev, false).unwrap(),
            "### Page snapshot\nNothing changed on the page since the last snapshot.\n"
        );
        // so much changed that the whole outline is clearer
        assert!(diff_outline("a\nb", "x\ny\nz", false).is_none());
    }

    #[test]
    fn quote_shortens_and_collapses_whitespace() {
        assert_eq!(quote("  Sign   in \n"), "“Sign in”");
        let long = "x".repeat(100);
        assert_eq!(quote(&long).chars().count(), 79 + 1 + 2);
    }

    #[test]
    fn agent_js_embedded_with_version() {
        assert!(agent_ready().contains("window.__abu.v === "));
    }
}
