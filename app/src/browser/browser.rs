//! Shared browser: one headless Chrome on the server, with real tabs.
//!   - Video: the active tab streams JPEG frames (screencast) whenever it changes.
//!   - Audio: a script injected into every tab taps <audio>/<video>/Web Audio and
//!     sends PCM back through a DevTools binding. Background tabs keep playing, like Chrome.
//!   - Input: mouse/keyboard from whoever is in control goes to the active tab.
//!   - Tabs: opened/closed/switched from the web UI. Tabs that a page opens
//!     (target=_blank links, window.open) are picked up automatically.
//!   - Agent: an AI agent (over MCP, see mcp.rs and agent.rs) drives the same tabs,
//!     so people watching the web UI see every step it takes.
//! No Docker, X server or video encoder, so it stays far lighter than neko.
//!
//! # Rust port: concurrency and the API the later modules plug into
//!
//! tasks and mutexes map like this (the lock order is the one in main.rs and the
//! comments below: hub state -> `Browser.mu` -> a tab's own state; the browser never calls the hub while
//! holding `Browser.mu`, and `BrowserApi` methods, called by the hub under its lock, never block):
//!   * `Browser.mu` (`st`): std mutex, the same fields as `mu`; never held across an `.await`
//!     (the guard is not `Send`, the compiler enforces it). A tab's fields "guarded by Browser.mu" in
//!     live in `Tab.st`, a second std mutex always taken *after* `Browser.mu`.
//!   * The command queue (`push`/`enqueue`/`call`/`run`): a `VecDeque` plus one `Notify`; one loop task
//!     drains it and runs every job in its own spawned task bounded by `JOB_LIMIT` (the job keeps
//!     running if it overruns, exactly like the task). Queued moves and wheel turns are merged.
//!   * Per-tab lanes: `Tab.lane` is a `tokio::sync::Mutex<>` held by `run_tab` for the whole closure,
//!     so single agent operations on one tab are serialised while other tabs run in parallel.
//!     `Tab.gesture` (an `Arc<tokio::sync::Mutex<>>`, handed out as an `OwnedMutexGuard`) is the gesture
//!     lock `lock_gesture` returns, held for a whole input tool call. `Tab.sess` (std mutex: owner and
//!     expiry) is the `browser_lock_tab` session. Lock order for those four: lane (outermost, only
//!     `run_tab`) -> gesture -> sess; `claim_tab`/`lock_gesture` take sess only briefly, never while
//!     waiting for gesture, and `lock_gesture` re-checks the session after it got the gesture lock.
//!   * A tab's "context" is `Tab::cancel` + `Tab::canceled`: every `Tab::call` ends with
//!     `CdpError::Canceled` when the tab is cancelled, and cancelling closes the target.
//!   * Chrome and its connection come from cdp.rs (a small CDP client over
//!     `--remote-debugging-pipe`, see there for why chromiumoxide's launcher is not used).
//!
//! ## Hooks for observe / frames / devtools / keepalive / share (Phase 2b) and the agent (Phase 3)
//!   * Event fan-out per tab: `BrowserHooks::tab_event(tab, &Event)` is called for every CDP event of
//!     every tab (the observer's `t.obs.event`; must return fast), and `Tab::subscribe` gives any other
//!     module its own ordered stream of that tab's events. `BrowserHooks::tab_created` is where
//!     the observer is created by `newObserver(id)`; keep its state in `Tab::ext` (`OnceLock<Arc<dyn Any>>`).
//!   * Run CDP on a tab: `Tab::call(method, params, timeout)` / `call_cmd` (15 s) and
//!     `Tab::evaluate(expr)`; on the browser itself: `Browser::root_call`. `Tab::session` is the flat
//!     session id (devtools/frames need it). `Browser::find_tab`, `run`, `run_tab`, `call`, `enqueue`,
//!     `claim_tab`, `release_tab`, `lock_gesture`, `busy`, `busy_tab`, `view_size`, `screen`, `shot`,
//!     `go_history` are the methods of the same name for agent.rs/tools.rs.
//!   * The frame stream for share.rs: `BrowserHooks::frame(jpeg)` (called with
//!     `Browser.mu` held for the active tab's every frame) and `BrowserHooks::audio(tab, rate, b64)`
//!     (`shareAudio`). Page bindings of other modules (`__tbShareOut`, `__tbRecOut`) are added per tab
//!     with `BrowserHooks::tab_scripts` (binding + new-document script, installed in `attach` in this
//!     order: stealth, focus, observe domains, device, meta, those, audio) and their calls arrive in
//!     `BrowserHooks::binding`. The flow recorder asks `recording`/`rec_install`/`page_changed`.
//!   * `BrowserHooks::download_done` is `h.assets.addDownload`.

use std::any::Any;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use base64::Engine as _;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{oneshot, watch, Mutex as AsyncMutex, Notify, OwnedMutexGuard};

use crate::cdp::{self, CdpError, CdpResult, Chrome, Event, LaunchSpec};
use crate::emulate::{metrics, Emulation};
use crate::engine::Engine;
use crate::files::{Download, DownloadEvent, Downloads};
use crate::stealth::{
    ua_metadata, UserAgentBrandVersion, UserAgentMetadata, UserAgentOverrideParams,
    SET_USER_AGENT_OVERRIDE, STEALTH_JS,
};
use crate::{golog, Hub, Msg};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// `error` values are strings here.
pub type BResult<T> = Result<T, String>;

#[cfg(target_os = "macos")]
#[path = "store_install.rs"]
pub mod store_install;

#[derive(Default)]
pub struct BrowserOptions {
    pub width: i64,
    pub height: i64,
    pub quality: i64,
    pub start_url: String,
    /// Chrome user data dir (cookies, logins…)
    pub profile: String,
    /// empty = auto-detect
    pub chrome_path: String,
    /// resolved from -engine / -chrome
    pub engine: Engine,
    /// local SOCKS5 switch the browser connects through (empty = direct)
    pub proxy: String,
    pub audio: bool,
    pub max_tabs: i64,
    /// where Chrome saves downloads (also readable by browser_file_upload)
    pub download_dir: String,
    /// files the agent may upload
    pub upload_dir: String,
    /// saved cookie/storage states (browser_state)
    pub state_dir: String,
    /// recorded flows (browser_get_flow / browser_run_flow)
    pub flow_dir: String,
    /// do not open Chrome's local debugging port: no Inspect in the viewer
    pub no_dev_tools: bool,
    pub enable_extensions: bool,
    /// the hooks of observe/share/flow, installed before Chrome starts (see `HookSet`)
    pub hooks: Option<Arc<dyn BrowserHooks>>,
}

/// browser.rs: `type TabInfo`: what the web UI gets for each tab.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TabInfo {
    pub id: String,
    pub title: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub favicon: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub loading: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub audible: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub muted: bool,
    #[serde(default, rename = "canBack", skip_serializing_if = "is_false")]
    pub can_back: bool,
    #[serde(default, rename = "canFwd", skip_serializing_if = "is_false")]
    pub can_fwd: bool,
}

/// browser.rs: `type View` (no omitempty): the size and image quality of the shared screen.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct View {
    #[serde(default)]
    pub width: i64,
    #[serde(default)]
    pub height: i64,
    #[serde(default)]
    pub quality: i64,
}

/// the fields of `Tab` that are "guarded by Browser.mu" (here: by `Tab.st`, taken after `Browser.mu`).
#[derive(Debug, Default)]
pub struct TabState {
    /// from Chrome's target info
    pub title: String,
    pub url: String,
    /// reported by the page itself (metaJS)
    pub doc_title: String,
    pub favicon: String,
    pub loading: bool,
    pub audible: bool,
    pub muted: bool,
    pub can_back: bool,
    pub can_fwd: bool,
    /// first page of a new tab: drop the about:blank history entry once it commits
    pub fresh: bool,
    /// newest refreshNav wins
    pub nav_seq: u64,
    pub last_audio: Option<Instant>,
    /// the flow recorder, while a recording is on (Page.ScriptIdentifier)
    pub rec_script: Option<String>,
    /// last time refit restored this tab's screen size
    pub refit_at: Option<Instant>,
}

/// `Tab.owner` / `Tab.ownerUntil` (guarded by `Tab.sess`).
#[derive(Debug, Default)]
pub struct Session {
    /// agent holding an exclusive session on this tab (browser_lock_tab)
    pub owner: String,
    pub until: Option<Instant>,
}

pub struct Tab {
    /// serializes single agent operations targeting this tab
    pub lane: AsyncMutex<()>,
    /// guards owner and ownerUntil
    pub sess: Mutex<Session>,
    /// held for a whole input tool call, so agents sharing a tab never interleave a click, drag or typing
    pub gesture: Arc<AsyncMutex<()>>,
    pub id: String,
    /// tab that opened this one, if any
    pub opener: String,
    /// flat CDP session of this tab ("" when the tab has no connection, in tests)
    pub session: String,
    conn: Option<Arc<cdp::Conn>>,
    /// `t.ctx` / `t.cancel`
    cancel: watch::Sender<bool>,
    pub st: Mutex<TabState>,
    /// per-tab state of the Phase 2b modules (the observer's `obs`, the share module's `share`)
    pub ext: OnceLock<Arc<dyn Any + Send + Sync>>,
}

impl Tab {
    fn build(id: &str, opener: &str, session: &str, conn: Option<Arc<cdp::Conn>>) -> Tab {
        Tab {
            lane: AsyncMutex::new(()),
            sess: Mutex::new(Session::default()),
            gesture: Arc::new(AsyncMutex::new(())),
            id: id.to_string(),
            opener: opener.to_string(),
            session: session.to_string(),
            conn,
            cancel: watch::channel(false).0,
            st: Mutex::new(TabState {
                title: "New Tab".into(),
                url: "about:blank".into(),
                ..Default::default()
            }),
            ext: OnceLock::new(),
        }
    }

    /// A tab with no Chrome behind it (tests).
    pub fn detached(id: &str) -> Arc<Tab> {
        Arc::new(Tab::build(id, "", "", None))
    }

    /// `t.ctx.Err != nil`
    pub fn canceled(&self) -> bool {
        *self.cancel.borrow()
    }

    /// `t.cancel`: ends every command still waiting on the tab, and closes the target.
    pub fn cancel(&self) {
        if self.cancel.send_replace(true) {
            return;
        }
        if let (Some(conn), Ok(_)) = (self.conn.clone(), tokio::runtime::Handle::try_current()) {
            let id = self.id.clone();
            tokio::spawn(async move {
                let _ = conn
                    .call(None, "Target.closeTarget", json!({"targetId": id}))
                    .await;
            });
        }
    }

    /// An ordered stream of this tab's CDP events (besides `BrowserHooks::tab_event`).
    pub fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<Arc<Event>> {
        match &self.conn {
            Some(c) => c.subscribe(Some(&self.session)),
            None => tokio::sync::mpsc::unbounded_channel().1,
        }
    }

    /// `Tab::call`-style command on this tab: one command on the tab's session, ended by `timeout` (None = only
    /// the tab's cancellation).
    pub async fn call(&self, method: &str, params: Value, timeout: Option<Duration>) -> CdpResult {
        let Some(conn) = &self.conn else {
            return Err(CdpError::Closed);
        };
        conn.call_with(
            Some(&self.session),
            method,
            params,
            timeout,
            Some(self.cancel.subscribe()),
        )
        .await
    }

    /// `cmdCtx(t.ctx)`: bounded by `CMD_TIMEOUT`.
    pub async fn call_cmd(&self, method: &str, params: Value) -> CdpResult {
        self.call(method, params, Some(CMD_TIMEOUT)).await
    }

    /// `evaluate`: Runtime.evaluate by value in the page's main world; a script exception is an error.
    pub async fn evaluate(&self, expr: &str, timeout: Option<Duration>) -> BResult<Value> {
        let res = self
            .call(
                "Runtime.evaluate",
                json!({"expression": expr, "returnByValue": true}),
                timeout,
            )
            .await
            .map_err(|e| e.to_string())?;
        if let Some(d) = res.get("exceptionDetails") {
            let msg = d
                .pointer("/exception/description")
                .and_then(Value::as_str)
                .or_else(|| d.get("text").and_then(Value::as_str))
                .unwrap_or("exception");
            return Err(msg.to_string());
        }
        Ok(res.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }

    pub fn st(&self) -> MutexGuard<'_, TabState> {
        lock(&self.st)
    }

    /// sessionFree reports whether who may act on the tab now (nobody holds a session, it expired, or who holds it).
    /// Caller holds t.sessMu.
    fn session_free(s: &Session, who: &str) -> bool {
        s.owner.is_empty() || s.owner == who || s.until.is_none_or(|u| Instant::now() > u)
    }
}

/// What the Phase 2b modules and the flow recorder add to the browser (see the module docs).
/// Every method has an empty default; hooks run on browser tasks and must return fast.
pub trait BrowserHooks: Send + Sync {
    /// A tab was attached (before its first event): create its per-tab state (`newObserver`).
    fn tab_created(&self, _t: &Arc<Tab>) {}
    /// Every CDP event of every tab (`t.obs.event(ev)`).
    fn tab_event(&self, _t: &Arc<Tab>, _ev: &Event) {}
    /// A frame of the active tab (`shareFrameLocked`); called with `Browser.mu` held.
    fn frame(&self, _jpeg: &[u8]) {}
    /// Page audio of a tab that is not muted (`shareAudio`): sample rate and base64 int16 stereo PCM.
    fn audio(&self, _t: &Arc<Tab>, _rate: i64, _b64: &str) {}
    /// A call of a page binding the browser does not know itself (`onShare`, `onRec`).
    fn binding(&self, _t: &Arc<Tab>, _name: &str, _payload: &str) {}
    /// Bindings and new-document scripts to install in every tab (share.rs: `shareBinding`/`shareJS`).
    fn tab_scripts(&self) -> Vec<TabScript> {
        Vec::new()
    }
    /// `b.recording`
    fn recording(&self) -> bool {
        false
    }
    /// `b.recInstall(t)`
    fn rec_install(&self, _t: &Arc<Tab>) {}
    /// `b.onPageChange(t, url)`
    fn page_changed(&self, _t: &Arc<Tab>, _url: &str) {}
    /// `h.assets.addDownload(d)`
    fn download_done(&self, _d: &Download) {}
}

pub struct TabScript {
    pub binding: &'static str,
    pub source: String,
}

/// Several hook modules behind the browser's single hook slot: every method goes to each module in
/// order (`tab_scripts` are concatenated, `recording` is true when any module is recording).
pub struct HookSet(pub Vec<Arc<dyn BrowserHooks>>);

impl BrowserHooks for HookSet {
    fn tab_created(&self, t: &Arc<Tab>) {
        self.0.iter().for_each(|h| h.tab_created(t));
    }
    fn tab_event(&self, t: &Arc<Tab>, ev: &Event) {
        self.0.iter().for_each(|h| h.tab_event(t, ev));
    }
    fn frame(&self, jpeg: &[u8]) {
        self.0.iter().for_each(|h| h.frame(jpeg));
    }
    fn audio(&self, t: &Arc<Tab>, rate: i64, b64: &str) {
        self.0.iter().for_each(|h| h.audio(t, rate, b64));
    }
    fn binding(&self, t: &Arc<Tab>, name: &str, payload: &str) {
        self.0.iter().for_each(|h| h.binding(t, name, payload));
    }
    fn tab_scripts(&self) -> Vec<TabScript> {
        self.0.iter().flat_map(|h| h.tab_scripts()).collect()
    }
    fn recording(&self) -> bool {
        self.0.iter().any(|h| h.recording())
    }
    fn rec_install(&self, t: &Arc<Tab>) {
        self.0.iter().for_each(|h| h.rec_install(t));
    }
    fn page_changed(&self, t: &Arc<Tab>, url: &str) {
        self.0.iter().for_each(|h| h.page_changed(t, url));
    }
    fn download_done(&self, d: &Download) {
        self.0.iter().for_each(|h| h.download_done(d));
    }
}

/// Binary packet types sent to the web clients (first byte).
/// `[1][jpeg…]`
pub const PKT_FRAME: u8 = 1;
/// `[2][stream id][sample rate uint32 LE][int16 stereo PCM…]`
pub const PKT_AUDIO: u8 = 2;

pub const AUDIO_BINDING: &str = "__tbAudioOut";

pub static AUDIO_JS: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let a = r####"(() => {
  if (window.__tbAudio) return;
  Object.defineProperty(window, '__tbAudio', { value: true });
  const Ctx = window.AudioContext || window.webkitAudioContext;
  if (!Ctx) return;
  const out = (s) => { const f = window."####;
    let b = r####"; if (typeof f === 'function') f(s); };
  const connect = AudioNode.prototype.connect;
  const taps = new WeakMap();

  // Float samples -> base64 int16 stereo. step=2 halves 44.1/48 kHz to save bandwidth.
  function encode(L, R, step) {
    const n = Math.floor(L.length / step), pcm = new Int16Array(n * 2);
    let peak = 0;
    for (let i = 0; i < n; i++) {
      let l = 0, r = 0;
      for (let k = 0; k < step; k++) { l += L[i * step + k]; r += R[i * step + k]; }
      l = Math.max(-1, Math.min(1, l / step)); r = Math.max(-1, Math.min(1, r / step));
      peak = Math.max(peak, Math.abs(l), Math.abs(r));
      pcm[2 * i] = l * 32767; pcm[2 * i + 1] = r * 32767;
    }
    if (peak < 0.0005) return null; // silence: send nothing
    const bytes = new Uint8Array(pcm.buffer);
    let bin = '';
    for (let i = 0; i < bytes.length; i += 0x8000) bin += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    return btoa(bin);
  }

  function tap(ctx) {
    let t = taps.get(ctx);
    if (t) return t;
    const id = 1 + Math.floor(Math.random() * 254);
    const step = ctx.sampleRate >= 44100 ? 2 : 1;
    const rate = Math.round(ctx.sampleRate / step);
    t = ctx.createScriptProcessor(2048, 2, 2);
    t.onaudioprocess = (e) => {
      const b = e.inputBuffer;
      const L = b.getChannelData(0), R = b.numberOfChannels > 1 ? b.getChannelData(1) : L;
      const data = encode(L, R, step);
      if (data) out(id + '|' + rate + '|' + data);
    };
    const silent = ctx.createGain();
    silent.gain.value = 0;
    connect.call(t, silent);
    connect.call(silent, ctx.destination); // keeps the tap running, outputs nothing
    taps.set(ctx, t);
    return t;
  }

  // Web Audio: whatever the page sends to the speakers also goes to the tap.
  const native = window.__abuNative || ((f) => f); // prints as the built-in (stealth.rs)
  AudioNode.prototype.connect = native({ connect(dest) {
    const r = connect.apply(this, arguments);
    try { if (dest instanceof AudioDestinationNode && this.context) connect.call(this, tap(this.context)); } catch (e) {}
    return r;
  } }.connect, 'connect');

  // <audio> / <video>: route the element into our own context.
  let mctx = null;
  const routed = new WeakSet();
  function route(el) {
    if (!(el instanceof HTMLMediaElement) || routed.has(el)) return;
    routed.add(el);
    try {
      if (!mctx) mctx = new Ctx({ sampleRate: 32000 });
      if (mctx.state !== 'running') mctx.resume();
      connect.call(mctx.createMediaElementSource(el), tap(mctx));
    } catch (e) {}
  }
  const play = HTMLMediaElement.prototype.play;
  HTMLMediaElement.prototype.play = native({ play() { route(this); return play.apply(this, arguments); } }.play, 'play');
  document.addEventListener('play', (e) => route(e.target), true);
  document.querySelectorAll('audio,video').forEach((el) => { if (!el.paused) route(el); });
})();"####;
    format!("{a}{AUDIO_BINDING}{b}")
});

pub const META_BINDING: &str = "__tbMetaOut";

/// Reports the page title and favicon whenever they change (Chrome sends no event for that).
pub static META_JS: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let a = r####"(() => {
  if (window.top !== window || window.__tbMeta) return;
  Object.defineProperty(window, '__tbMeta', { value: true });
  let last = '', timer = 0, obs = null;
  const icon = () => {
    const l = document.querySelector('link[rel~="icon"]');
    if (l && l.href) return l.href;
    return /^https?:$/.test(location.protocol) ? location.origin + '/favicon.ico' : '';
  };
  const send = () => {
    timer = 0;
    const s = JSON.stringify({ t: document.title, i: icon() });
    if (s === last) return;
    last = s;
    const f = window."####;
    let b = r####";
    if (typeof f === 'function') f(s);
  };
  const soon = () => { if (!timer) timer = setTimeout(send, 50); };
  const watch = () => {
    if (obs || !document.head) return;
    obs = new MutationObserver(soon);
    obs.observe(document.head, { subtree: true, childList: true, characterData: true, attributes: true, attributeFilter: ['href', 'rel'] });
    soon();
  };
  document.addEventListener('DOMContentLoaded', watch);
  window.addEventListener('load', soon);
  watch();
})();"####;
    format!("{a}{META_BINDING}{b}")
});

/// The Chrome launch flags: the defaults, with this program's changes, in that order
/// would pass them (a flag set again keeps its first place).
pub fn chrome_flags(opt: &BrowserOptions) -> cdp::Flags {
    let mut f = cdp::Flags::default();
    // default launch flags
    f.bool("no-first-run", true);
    f.bool("no-default-browser-check", true);
    f.bool("headless", true);
    f.bool("hide-scrollbars", true);
    f.bool("mute-audio", true);
    f.bool("disable-background-networking", true);
    f.str("enable-features", "NetworkService,NetworkServiceInProcess");
    f.bool("disable-background-timer-throttling", true);
    f.bool("disable-backgrounding-occluded-windows", true);
    f.bool("disable-breakpad", true);
    f.bool("disable-client-side-phishing-detection", true);
    f.bool("disable-default-apps", true);
    f.bool("disable-dev-shm-usage", true);
    let enabled = opt.enable_extensions
        || (opt.engine.name == "chrome"
            && !opt.profile.is_empty()
            && std::path::Path::new(&opt.profile)
                .join(".abu-extensions-enabled")
                .is_file());
    f.bool("disable-extensions", !enabled);
    f.str("disable-features", "Translate,BlinkGenPropertyTrees");
    f.bool("disable-hang-monitor", true);
    f.bool("disable-ipc-flooding-protection", true);
    f.bool("disable-popup-blocking", true);
    f.bool("disable-prompt-on-repost", true);
    f.bool("disable-renderer-backgrounding", true);
    f.bool("disable-sync", true);
    f.str("force-color-profile", "srgb");
    f.bool("metrics-recording-only", true);
    f.bool("safebrowsing-disable-auto-update", true);
    f.bool("enable-automation", true);
    f.str("password-store", "basic");
    f.bool("use-mock-keychain", true);
    // chromeFlags
    f.bool("enable-automation", false);
    f.str("disable-blink-features", "AutomationControlled");
    f.bool("mute-audio", true); // never play sound on the server itself
    f.str("autoplay-policy", "no-user-gesture-required");
    // Fewer processes = less RAM. Keeping iframes in the page process also lets
    // the audio tap reach embedded players.
    // keep GPU on so WebGL exists (a missing WebGL context is a headless tell)
    f.str("renderer-process-limit", "4");
    f.bool("disable-site-isolation-trials", true);
    f.str("disable-features", "Translate,BlinkGenPropertyTrees,SpareRendererForSitePerProcess,OptimizationHints,MediaRouter,DialMediaRouteProvider,IsolateOrigins,site-per-process,ChromeWhatsNewUI");
    f.bool("disable-component-update", true);
    // Stop Chrome from phoning home: its update check opens an "About Chrome"
    // (chrome://settings/help) tab on its own. These also silence the default-browser
    // and first-run nags, and the "restore last session" bubble.
    f.bool("disable-background-networking", true);
    f.bool("no-default-browser-check", true);
    f.bool("disable-session-crashed-bubble", true);
    f.bool("hide-crash-restore-bubble", true);
    f.str("js-flags", "--max-old-space-size=512");
    f.str("window-size", &format!("{},{}", opt.width, opt.height));
    if !opt.engine.shell {
        // chrome-headless-shell is always headless and has no "new" mode
        f.str("headless", "new");
    }
    if !opt.proxy.is_empty() {
        // Only this browser goes through the proxy; the rest of the machine is untouched.
        // socks5:// also sends DNS lookups through the proxy. WebRTC could otherwise
        // reveal the real IP over UDP, so it is limited to proxied traffic.
        f.str("proxy-server", &opt.proxy);
        f.str("force-webrtc-ip-handling-policy", "disable_non_proxied_udp");
        f.str("webrtc-ip-handling-policy", "disable_non_proxied_udp");
    }
    f
}

/// cmdTimeout bounds one Chrome command, so a hung page or browser can never stall
/// the command queue (and with it every later click, key and agent step).
pub const CMD_TIMEOUT: Duration = Duration::from_secs(15);

/// jobLimit is the longest one queued command may hold up the ones behind it.
const JOB_LIMIT: Duration = Duration::from_secs(30);

/// Headless Chrome says "HeadlessChrome" in its user agent, and some sites block
/// that. Start Chrome once to read the real version, so the shared browser can
/// run with a normal user agent everywhere (tabs, popups, workers), with the
/// client hints that go with it (see ua_metadata).
pub async fn detect_user_agent(
    opt: &BrowserOptions,
) -> BResult<(String, Option<UserAgentMetadata>)> {
    if opt.engine.is_obscura() {
        // no Chrome user agent or client hints to read; Obscura keeps its own
        return Ok((String::new(), None));
    }
    // only reads the version: no network, so no need to wait on the proxy/WARP
    let flags = chrome_flags(&BrowserOptions {
        proxy: String::new(),
        engine: opt.engine.clone(),
        width: opt.width,
        height: opt.height,
        ..Default::default()
    });
    let chrome = cdp::launch(LaunchSpec {
        exec_path: opt.engine.path.clone(),
        flags,
        obscura: false,
        cdp_url: String::new(),
        debug_port: false,
    })
    .await?;
    // a browser that never comes up must not hang startup
    let r = tokio::time::timeout(Duration::from_secs(30), detect_in(&chrome)).await;
    chrome.kill();
    let _ = tokio::time::timeout(Duration::from_secs(3), chrome.wait_exit()).await;
    match r {
        Ok(r) => r,
        Err(_) => Err(CdpError::Timeout.to_string()),
    }
}

async fn detect_in(chrome: &Chrome) -> BResult<(String, Option<UserAgentMetadata>)> {
    let v = chrome
        .conn
        .call(None, "Browser.getVersion", json!({}))
        .await
        .map_err(|e| e.to_string())?;
    let user_agent = v["userAgent"].as_str().unwrap_or_default();
    let product = v["product"].as_str().unwrap_or_default();
    let ua = user_agent.replacen("HeadlessChrome", "Chrome", 1);
    // The brands (with Chrome's own GREASE entry) and platform, as a page sees them.
    // userAgentData only exists in secure contexts, which about:blank is not.
    let (target_id, session) = first_page_session(chrome).await?;
    let _ = target_id;
    let call = |m: &'static str, p: Value| chrome.conn.call(Some(&session), m, p);
    if let Err(e) = call("Page.navigate", json!({"url": "chrome://version/"})).await {
        golog(&format!("chrome: client hints: {e}"));
    }
    // Evaluate runs in the new document's context; wait for the navigation to commit
    for _ in 0..100 {
        let href = call(
            "Runtime.evaluate",
            json!({"expression": "location.href", "returnByValue": true}),
        )
        .await;
        if href.is_ok_and(|h| {
            h.pointer("/result/value")
                .and_then(Value::as_str)
                .is_some_and(|u| u.starts_with("chrome://version"))
        }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    #[derive(Deserialize)]
    struct UaData {
        brands: Vec<UserAgentBrandVersion>,
        platform: String,
    }
    let r = call(
        "Runtime.evaluate",
        json!({"expression": "navigator.userAgentData ? {brands: navigator.userAgentData.brands, platform: navigator.userAgentData.platform} : null", "returnByValue": true}),
    )
    .await;
    let d = match r {
        Ok(r) if r.get("exceptionDetails").is_none() => r
            .pointer("/result/value")
            .cloned()
            .and_then(|v| serde_json::from_value::<Option<UaData>>(v).ok().flatten()),
        _ => None,
    };
    let Some(d) = d else {
        golog("chrome: no client hints to complete: <nil>");
        return Ok((ua, None));
    };
    Ok((ua, ua_metadata(product, &d.brands, &d.platform)))
}

/// First run of the browser: wait for a page target of the new browser and attach to it.
async fn first_page_session(chrome: &Chrome) -> BResult<(String, String)> {
    let id = first_page_target(chrome).await?;
    let res = chrome
        .conn
        .call(
            None,
            "Target.attachToTarget",
            json!({"targetId": id, "flatten": true}),
        )
        .await
        .map_err(|e| e.to_string())?;
    let session = res["sessionId"].as_str().unwrap_or_default().to_string();
    Ok((id, session))
}

async fn first_page_target(chrome: &Chrome) -> BResult<String> {
    let mut created = false;
    loop {
        let r = chrome
            .conn
            .call(None, "Target.getTargets", json!({}))
            .await
            .map_err(|e| e.to_string())?;
        if let Some(t) = r["targetInfos"]
            .as_array()
            .and_then(|l| l.iter().find(|t| t["type"] == "page"))
        {
            return Ok(t["targetId"].as_str().unwrap_or_default().to_string());
        }
        if chrome.obscura && !created {
            // Obscura starts with no tab at all
            created = true;
            let r = chrome
                .conn
                .call(None, "Target.createTarget", json!({"url": "about:blank"}))
                .await
                .map_err(|e| e.to_string())?;
            if let Some(id) = r["targetId"].as_str() {
                return Ok(id.to_string());
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------- The browser ----------

type JobFn = Box<dyn FnOnce(Arc<Browser>) -> BoxFuture<'static, ()> + Send>;

/// Each Chrome call waits for Chrome, and mouse moves and wheel turns can come in
/// faster than a busy page handles them. Queued moves and wheel turns are merged,
/// so the screen follows the hand instead of replaying a backlog for seconds.
struct Job {
    /// Func runs f, the others send mouse to the active tab
    kind: JobKind,
    f: Option<JobFn>,
    mouse: Msg,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum JobKind {
    Func,
    Mouse,
    Move,
    Wheel,
}

/// Browser.mu guards these (`Browser` struct, "guarded by mu").
pub struct State {
    /// the running Chrome; replaced by restart
    pub chrome: Option<Arc<Chrome>>,
    /// Close was called: never restart again
    pub closed: bool,
    /// URL -> when a hung tab on it was last reopened
    pub recovered: HashMap<String, Instant>,
    pub view: View,
    pub tabs: Vec<Arc<Tab>>,
    pub active: Option<Arc<Tab>>,
    /// latest frame packet of the active tab
    pub frame: Option<Bytes>,
    /// the active tab sent a screencast frame since it became active
    pub got_frame: bool,
    pub streams: HashMap<String, u8>,
    pub next_stream: u32,
    /// a tab opened before this time stays in the background (middle/ctrl-click)
    pub bg_until: Option<Instant>,
    /// device emulation
    pub emu: Emulation,
    /// a flow is being recorded: every tab runs the recorder
    pub rec_on: bool,
}

pub struct Browser {
    me: Weak<Browser>,
    pub(crate) hub: Arc<Hub>,
    pub(crate) opt: BrowserOptions,
    hooks: OnceLock<Arc<dyn BrowserHooks>>,
    /// held while Chrome restarts
    rmu: AsyncMutex<()>,

    /// Tab and input commands, run one at a time and in order (see push).
    jobs: Mutex<VecDeque<Job>>,
    wake: Notify,

    /// createMu is held while we create a tab ourselves, so that the handler for
    /// new targets can tell our tabs apart from tabs that a page opened.
    create_mu: AsyncMutex<HashSet<String>>,

    mu: Mutex<State>,

    /// batches tab-list notifications
    notifying: Mutex<bool>,

    /// Chrome's normal user agent (what "no emulation" means)
    pub(crate) ua: String,
    /// its client hints, complete (stealth.rs); None = Chrome's own
    pub(crate) ua_meta: Option<UserAgentMetadata>,
    pub dl: Downloads,
}

impl Browser {
    fn new(
        hub: Arc<Hub>,
        opt: BrowserOptions,
        ua: String,
        ua_meta: Option<UserAgentMetadata>,
    ) -> Arc<Browser> {
        let view = View {
            width: opt.width,
            height: opt.height,
            quality: opt.quality,
        };
        Arc::new_cyclic(|me| Browser {
            me: me.clone(),
            hub,
            opt,
            hooks: OnceLock::new(),
            rmu: AsyncMutex::new(()),
            jobs: Mutex::new(VecDeque::new()),
            wake: Notify::new(),
            create_mu: AsyncMutex::new(HashSet::new()),
            mu: Mutex::new(State {
                chrome: None,
                closed: false,
                recovered: HashMap::new(),
                view,
                tabs: Vec::new(),
                active: None,
                frame: None,
                got_frame: false,
                streams: HashMap::new(),
                next_stream: 0,
                bg_until: None,
                emu: Emulation::default(),
                rec_on: false,
            }),
            notifying: Mutex::new(false),
            ua,
            ua_meta,
            dl: Downloads::new(),
        })
    }

    /// A browser with the given tabs and no Chrome (tests: builds `&Browser{tabs: …}`).
    #[cfg(test)]
    pub fn for_test(tabs: Vec<Arc<Tab>>) -> Arc<Browser> {
        let b = Browser::new(Hub::new(""), BrowserOptions::default(), String::new(), None);
        b.st().tabs = tabs;
        b
    }

    fn arc(&self) -> Arc<Browser> {
        self.me.upgrade().expect("browser dropped")
    }

    /// Locks `Browser.mu` (never hold the guard across an `.await`).
    pub fn st(&self) -> MutexGuard<'_, State> {
        lock(&self.mu)
    }

    /// Installs the hooks of observe/share/flow (once, before tabs exist).
    pub fn set_hooks(&self, h: Arc<dyn BrowserHooks>) {
        let _ = self.hooks.set(h);
    }

    fn hooks(&self) -> Option<&Arc<dyn BrowserHooks>> {
        self.hooks.get()
    }

    /// `go b.hub.systemChat(text)`
    fn chat_later(&self, text: String) {
        let hub = self.hub.clone();
        tokio::spawn(async move { hub.system_chat(&text) });
    }

    /// rootCtx is the running Chrome (it changes when Chrome restarts).
    pub fn root(&self) -> Option<Arc<Chrome>> {
        self.st().chrome.clone()
    }

    /// A browser-level command.
    pub async fn root_call(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> CdpResult {
        let chrome = self.root().ok_or(CdpError::Closed)?;
        chrome
            .conn
            .call_with(None, method, params, timeout, None)
            .await
    }
}

/// startBrowser starts Chrome and everything around it.
pub async fn start_browser(hub: Arc<Hub>, mut opt: BrowserOptions) -> BResult<Arc<Browser>> {
    let (ua, ua_meta) = detect_user_agent(&opt).await?;
    let hooks = opt.hooks.take();
    let b = Browser::new(hub, opt, ua, ua_meta);
    if let Some(h) = hooks {
        b.set_hooks(h); // before launch: the first tab already runs through them
    }
    for d in [
        &b.opt.download_dir,
        &b.opt.upload_dir,
        &b.opt.state_dir,
        &b.opt.flow_dir,
    ] {
        if !d.is_empty() {
            mkdir_all_0700(d);
        }
    }
    // before launch: launch may already queue work (tab events)
    tokio::spawn(b.clone().run_queue());
    b.launch().await?;
    tokio::spawn(b.clone().watch_audio());
    tokio::spawn(b.clone().watch_health());
    Ok(b)
}

fn mkdir_all_0700(d: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(d);
    }
    #[cfg(not(unix))]
    let _ = std::fs::create_dir_all(d);
}

impl Browser {
    /// launch starts Chrome and opens the start page. Used at startup and by restart.
    async fn launch(self: &Arc<Self>) -> BResult<()> {
        let mut flags = chrome_flags(&self.opt);
        flags.str("user-agent", &self.ua);
        flags.str("user-data-dir", &self.opt.profile);
        // Chrome talks to us over a pipe and so opens no port. The viewer's Inspect needs one
        // (it is only reachable from this machine, and the viewer goes through devtools.rs).
        // It cannot be a Flag: that would switch the pipe off. Both can run together.
        let chrome = cdp::launch(LaunchSpec {
            exec_path: self.opt.engine.path.clone(),
            flags,
            obscura: self.opt.engine.is_obscura(),
            cdp_url: self.opt.engine.cdp_url.clone(),
            debug_port: !self.opt.no_dev_tools && !self.opt.engine.is_obscura(),
        })
        .await?;
        // Starts Chrome and attaches to its first blank tab; bounded by a timer.
        let first =
            match tokio::time::timeout(Duration::from_secs(30), first_page_target(&chrome)).await {
                Ok(Ok(id)) => id,
                Ok(Err(e)) => {
                    chrome.kill();
                    return Err(e);
                }
                Err(_) => {
                    chrome.kill();
                    return Err("the browser took too long to start".into());
                }
            };

        self.st().chrome = Some(chrome.clone());
        let first_blank = first;
        self.create_mu.lock().await.clear();
        self.create_mu.lock().await.insert(first_blank.clone());

        let rx = chrome.conn.subscribe(None);
        tokio::spawn(self.clone().browser_events(rx));
        if let Err(e) = chrome
            .conn
            .call_with(
                None,
                "Target.setDiscoverTargets",
                json!({"discover": true}),
                Some(CMD_TIMEOUT),
                None,
            )
            .await
        {
            if chrome.obscura {
                golog(&format!("obscura: target discovery: {e}"));
            } else {
                chrome.kill();
                return Err(e.to_string());
            }
        }
        if !self.opt.download_dir.is_empty() && !chrome.obscura {
            if let Err(e) = self
                .root_call(
                    "Browser.setDownloadBehavior",
                    json!({"behavior": "allow", "downloadPath": self.opt.download_dir, "eventsEnabled": true}),
                    Some(CMD_TIMEOUT),
                )
                .await
            {
                golog(&format!("chrome: downloads: {e}"));
            }
        }
        let (t, err) = self.open_tab(&self.opt.start_url).await;
        if let Some(err) = err {
            if t.is_none() {
                // no tab at all: Chrome is not usable
                chrome.kill();
                return Err(err);
            }
            // The tab exists but the start page was slow (e.g. WARP still connecting): it keeps loading.
            golog(&format!("chrome: start page: {err}"));
        }
        // Our tabs live in the same window, so closing the startup tab keeps Chrome running.
        let _ = chrome
            .conn
            .call_with(
                None,
                "Target.closeTarget",
                json!({"targetId": first_blank}),
                Some(CMD_TIMEOUT),
                None,
            )
            .await;
        Ok(())
    }

    // ---------- Health: recover from a frozen tab or browser ----------

    /// watchHealth pings Chrome and the active tab. A tab whose page stopped
    /// responding is reopened; a browser that died or froze is restarted, so nobody
    /// has to restart the server.
    async fn watch_health(self: Arc<Self>) {
        const EVERY: Duration = Duration::from_secs(5);
        const LIMIT: Duration = Duration::from_secs(8);
        let (mut browser_fails, mut tab_fails) = (0, 0);
        let mut last_tab: Option<Arc<Tab>> = None;
        let mut tick = tokio::time::interval(EVERY);
        tick.tick().await;
        loop {
            tick.tick().await;
            let (closed, root, t, loading) = {
                let st = self.st();
                let loading = st.active.as_ref().is_some_and(|a| a.st().loading);
                (st.closed, st.chrome.clone(), st.active.clone(), loading)
            };
            if closed {
                return;
            }
            let Some(root) = root else { continue };
            if root.is_dead() {
                // Chrome exited or the connection dropped
                self.restart("the browser closed unexpectedly").await;
                (browser_fails, tab_fails) = (0, 0);
                continue;
            }
            // Obscura has no Browser.getVersion to ping: a dead process or connection is caught above
            let r = if root.obscura {
                Ok(Value::Null)
            } else {
                root.conn
                    .call_with(None, "Browser.getVersion", json!({}), Some(LIMIT), None)
                    .await
            };
            if r.is_err() {
                browser_fails += 1;
                if browser_fails >= 2 {
                    self.restart("the browser stopped responding").await;
                    (browser_fails, tab_fails) = (0, 0);
                }
                continue;
            }
            browser_fails = 0;

            let same = match (&t, &last_tab) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            };
            if t.is_none() || !same {
                last_tab = t.clone();
                tab_fails = 0;
            }
            let Some(t) = t else { continue };
            if t.canceled() {
                continue;
            }
            let r = t
                .call("Runtime.evaluate", json!({"expression": "1"}), Some(LIMIT))
                .await;
            // Only a timeout means "hung": errors like "context destroyed" come during normal navigation.
            if r == Err(CdpError::Timeout) && !t.canceled() {
                tab_fails += 1;
                if loading {
                    // a slow page load gets more time
                    tab_fails = tab_fails.min(2);
                }
                if tab_fails >= 3 || (tab_fails >= 2 && !loading) {
                    self.recover_tab(&t).await;
                    last_tab = None;
                    tab_fails = 0;
                }
                continue;
            }
            tab_fails = 0;
        }
    }

    /// recoverTab replaces a tab whose page froze (endless script, runaway memory…).
    async fn recover_tab(self: &Arc<Self>, t: &Arc<Tab>) {
        let (u, again) = {
            let mut st = self.st();
            let u = t.st().url.clone();
            let again = st
                .recovered
                .get(&u)
                .is_some_and(|w| w.elapsed() < Duration::from_secs(120));
            st.recovered.insert(u.clone(), Instant::now());
            (u, again)
        };
        golog(&format!("chrome: tab not responding: {u}"));
        let reopen = !again && (u.starts_with("http://") || u.starts_with("https://"));
        // Open the replacement first: a window whose last tab closes is gone, and
        // headless Chrome then can't open tabs. Tabs are made by the browser process,
        // which still answers while a page is frozen.
        let last = self.st().tabs.len() == 1;
        let mut repl: Option<Arc<Tab>> = None;
        if reopen || last {
            match self.create_tab().await {
                Ok(r) => repl = Some(r),
                Err(e) => golog(&format!("chrome: replacement tab: {e}")),
            }
        }
        t.cancel(); // ends every command still waiting on it, and closes the target
        if reopen {
            self.chat_later(format!("The page stopped responding and was reloaded: {u}"));
        } else {
            self.chat_later(format!("The page stopped responding and was closed: {u}"));
        }
        let t = t.clone();
        self.enqueue(move |b| async move {
            let Some(repl) = repl else {
                b.close_tab(&t.id).await;
                return;
            };
            b.activate(&repl).await;
            b.close_tab(&t.id).await;
            let dest = if reopen { u } else { b.opt.start_url.clone() };
            let nu = normalize_url(&dest);
            if !nu.is_empty() {
                repl.st().fresh = true;
                if let Err(e) = repl.call_cmd("Page.navigate", json!({"url": nu})).await {
                    golog(&format!("chrome: reopen tab: {e}"));
                }
            }
        });
    }

    /// restart kills Chrome and starts a fresh one with the same profile (cookies and logins stay).
    async fn restart(self: &Arc<Self>, reason: &str) {
        let Ok(_guard) = self.rmu.try_lock() else {
            return;
        };
        golog(&format!("chrome: restarting: {reason}"));
        self.chat_later(format!("Restarting the browser: {reason}"));
        if self.stop_for_restart().await {
            self.relaunch().await;
        }
    }

    async fn stop_for_restart(&self) -> bool {
        let (tabs, root) = {
            let mut st = self.st();
            if st.closed {
                return false;
            }
            let tabs = std::mem::take(&mut st.tabs);
            let root = st.chrome.take();
            st.active = None;
            st.frame = None;
            st.got_frame = false;
            st.streams.clear();
            (tabs, root)
        };
        self.notify();

        for t in &tabs {
            t.cancel(); // unblocks anything still waiting on the old tabs
        }
        if let Some(root) = root {
            let _ = tokio::time::timeout(Duration::from_secs(3), root.close_gracefully()).await;
            root.kill(); // kills the process if it is still there
            let _ = tokio::time::timeout(Duration::from_secs(3), root.wait_exit()).await;
        }
        true
    }

    async fn relaunch(self: &Arc<Self>) {
        let mut try_ = 1u64;
        while !self.st().closed {
            match self.launch().await {
                Ok(()) => {
                    self.hub.system_chat("The browser is back");
                    return;
                }
                Err(err) => {
                    golog(&format!("chrome: restart failed (try {try_}): {err}"));
                    if self.st().closed {
                        return;
                    }
                    if try_ == 3 {
                        self.hub
                            .system_chat(&format!("Could not restart the browser: {err}"));
                    }
                    tokio::time::sleep(Duration::from_secs((try_ * 5).min(60))).await;
                }
            }
            try_ += 1;
        }
    }

    // ---------- Command queue ----------

    async fn run_queue(self: Arc<Self>) {
        loop {
            self.wake.notified().await;
            loop {
                let j = lock(&self.jobs).pop_front();
                let Some(j) = j else { break };
                self.run_job(j).await;
            }
        }
    }

    /// run_job runs one command. Every Chrome call has its own timeout, but if a
    /// command still hangs, the queue moves on instead of freezing for everyone
    /// (the hung command finishes or fails on its own later).
    async fn run_job(self: &Arc<Self>, j: Job) {
        let b = self.clone();
        let h = tokio::spawn(async move {
            match j.kind {
                JobKind::Func => {
                    if let Some(f) = j.f {
                        f(b).await;
                    }
                }
                _ => b.dispatch_mouse(j.mouse).await,
            }
        });
        if tokio::time::timeout(JOB_LIMIT, h).await.is_err() {
            golog("chrome: a command took too long; moving on");
        }
    }

    fn push(&self, j: Job) {
        {
            let mut q = lock(&self.jobs);
            // queue full: drop
            if !merge_locked(&mut q, &j) && q.len() < 1024 {
                q.push_back(j);
            }
        }
        self.wake.notify_one();
    }

    pub fn enqueue<F, Fut>(&self, f: F)
    where
        F: FnOnce(Arc<Browser>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.push(Job {
            kind: JobKind::Func,
            f: Some(Box::new(move |b| Box::pin(f(b)))),
            mouse: Msg::default(),
        });
    }

    /// call runs f from the queue and waits for it (used by the AI agent, which needs results).
    pub async fn call<T, F, Fut>(&self, f: F) -> BResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Browser>) -> Fut + Send + 'static,
        Fut: Future<Output = BResult<T>> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.enqueue(move |b| async move {
            let _ = tx.send(f(b).await);
        });
        match tokio::time::timeout(Duration::from_secs(45), rx).await {
            Ok(Ok(r)) => r,
            _ => Err("the browser did not respond in time".into()),
        }
    }

    /// run runs f on the active tab from the queue and waits for it.
    pub async fn run<T, F, Fut>(&self, f: F) -> BResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Tab>) -> Fut + Send + 'static,
        Fut: Future<Output = BResult<T>> + Send + 'static,
    {
        self.call(move |b| async move {
            let t = b.st().active.clone();
            match t {
                Some(t) => f(t).await,
                None => Err("no tab is open".into()),
            }
        })
        .await
    }

    /// runTab serializes work for one stable tab without blocking work on other tabs.
    pub async fn run_tab<T, F, Fut>(&self, tab_id: &str, f: F) -> BResult<T>
    where
        F: FnOnce(Arc<Tab>) -> Fut,
        Fut: Future<Output = BResult<T>>,
    {
        let t = self.st().tabs.iter().find(|c| c.id == tab_id).cloned();
        let Some(t) = t else {
            return Err(format!("tab not found: {tab_id}"));
        };
        let _lane = t.lane.lock().await;
        f(t.clone()).await
    }

    /// findTab returns the tab with that id, or the active one when tabID is empty.
    pub fn find_tab(&self, tab_id: &str) -> Option<Arc<Tab>> {
        let st = self.st();
        if tab_id.is_empty() {
            return st.active.clone();
        }
        st.tabs.iter().find(|c| c.id == tab_id).cloned()
    }

    /// claimTab gives who exclusive use of the tab for ttl (a "session": pick a tool, then draw, without another
    /// agent changing the page state in between). Others' input calls wait until release or expiry.
    pub async fn claim_tab(&self, tab_id: &str, who: &str, ttl: Duration) -> BResult<()> {
        let Some(t) = self.find_tab(tab_id) else {
            return Err("tab not found".into());
        };
        let end = Instant::now() + Duration::from_secs(60);
        loop {
            let holder = {
                let mut s = lock(&t.sess);
                if Tab::session_free(&s, who) {
                    s.owner = who.to_string();
                    s.until = Some(Instant::now() + ttl);
                    return Ok(());
                }
                s.owner.clone()
            };
            if Instant::now() > end {
                return Err(format!("the tab is in use by {holder:?}; try again later"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub fn release_tab(&self, tab_id: &str, who: &str) {
        if let Some(t) = self.find_tab(tab_id) {
            let mut s = lock(&t.sess);
            if s.owner == who {
                s.owner.clear();
            }
        }
    }

    /// releaseOwner frees every tab locked by this agent or one of its sub-agents (who/…).
    pub fn release_owner(&self, who: &str) {
        let tabs = self.st().tabs.clone();
        let sub = format!("{who}/");
        for t in tabs {
            let mut s = lock(&t.sess);
            if s.owner == who || s.owner.starts_with(&sub) {
                s.owner.clear();
            }
        }
    }

    /// lockGesture waits for any other agent's session on the tab, then takes the tab's gesture lock;
    /// it returns the guard (dropping it unlocks; None when the tab does not exist). Input tools hold it for the whole call.
    pub async fn lock_gesture(
        &self,
        tab_id: &str,
        who: &str,
    ) -> BResult<Option<OwnedMutexGuard<()>>> {
        let Some(t) = self.find_tab(tab_id) else {
            return Ok(None);
        };
        let end = Instant::now() + Duration::from_secs(60);
        loop {
            let (free, holder) = {
                let s = lock(&t.sess);
                (Tab::session_free(&s, who), s.owner.clone())
            };
            if free {
                let g = t.gesture.clone().lock_owned().await;
                let still = Tab::session_free(&lock(&t.sess), who);
                if still {
                    return Ok(Some(g));
                }
                drop(g); // someone claimed the tab while we waited for the lock
            }
            if Instant::now() > end {
                return Err(format!(
                    "the tab is in use by {holder:?} (browser_lock_tab); try again later"
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// busy reports whether the active tab is still loading.
    pub fn busy(&self) -> bool {
        self.st().active.as_ref().is_some_and(|a| a.st().loading)
    }

    /// busyTab reports whether a specific tab is still loading without changing the active tab.
    pub fn busy_tab(&self, tab_id: &str) -> bool {
        self.st()
            .tabs
            .iter()
            .find(|t| t.id == tab_id)
            .is_some_and(|t| t.st().loading)
    }

    pub fn view_size(&self) -> View {
        self.st().view
    }

    /// screen is the configured screen size together with the device emulation in force,
    /// which together decide the real viewport a capture or a frame must cover.
    pub fn screen(&self) -> (View, Emulation) {
        let st = self.st();
        (st.view, st.emu.clone())
    }

    /// screenEmu is the device emulation in force, on its own.
    pub fn screen_emu(&self) -> Emulation {
        self.st().emu.clone()
    }

    /// onTab runs f on the active tab from the queue.
    fn on_tab<F, Fut>(&self, f: F)
    where
        F: FnOnce(Arc<Tab>) -> Fut + Send + 'static,
        Fut: Future<Output = BResult<()>> + Send + 'static,
    {
        self.enqueue(move |b| async move {
            let t = b.st().active.clone();
            let Some(t) = t else { return };
            if let Err(e) = f(t.clone()).await {
                if !t.canceled() {
                    golog(&format!("chrome: {e}"));
                }
            }
        });
    }

    /// Close shuts Chrome down, so no process is left behind when we exit. Blocks up to 3 s (the hub calls
    /// it on the way out): Browser.close is sent without waiting, then the process gets the same grace
    /// period before it is killed.
    pub fn close_now(&self) {
        let root = {
            let mut st = self.st();
            st.closed = true;
            st.chrome.clone()
        };
        let Some(root) = root else { return };
        root.conn.send_nowait("Browser.close", json!({}));
        let end = Instant::now() + Duration::from_secs(3);
        while !root.is_dead() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        root.kill();
        // let the supervisor finish the kill before the process exits
        let end = Instant::now() + Duration::from_millis(500);
        while !root.exited() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// merge_locked folds a move or wheel turn into a queued one of the same kind.
/// Only the moves and wheel turns after the last other command are looked at,
/// so clicks and keys keep their order.
fn merge_locked(jobs: &mut VecDeque<Job>, j: &Job) -> bool {
    if j.kind != JobKind::Move && j.kind != JobKind::Wheel {
        return false;
    }
    for q in jobs.iter_mut().rev() {
        if q.kind != JobKind::Move && q.kind != JobKind::Wheel {
            return false;
        }
        if q.kind != j.kind || q.mouse.mods != j.mouse.mods || q.mouse.buttons != j.mouse.buttons {
            continue;
        }
        let (dx, dy) = (q.mouse.dx, q.mouse.dy);
        q.mouse = j.mouse.clone(); // newest position
        if j.kind == JobKind::Wheel {
            q.mouse.dx += dx;
            q.mouse.dy += dy;
        }
        return true;
    }
    false
}

// ---------- Tabs ----------

impl Browser {
    /// createTab opens a blank tab in the shared window.
    pub(crate) async fn create_tab(self: &Arc<Self>) -> BResult<Arc<Tab>> {
        let mut known = self.create_mu.lock().await;
        let mut nw = false;
        let mut res = self
            .root_call(
                "Target.createTarget",
                json!({"url": "about:blank", "newWindow": nw}),
                Some(CMD_TIMEOUT),
            )
            .await;
        if res
            .as_ref()
            .is_err_and(|e| e.to_string().contains("no browser is open"))
        {
            nw = true; // the window closed with its last tab: open a new one
            res = self
                .root_call(
                    "Target.createTarget",
                    json!({"url": "about:blank", "newWindow": nw}),
                    Some(CMD_TIMEOUT),
                )
                .await;
        }
        let res = match res {
            Ok(r) => r,
            Err(e) => return Err(e.to_string()),
        };
        let id = res["targetId"].as_str().unwrap_or_default().to_string();
        known.insert(id.clone());
        drop(known);
        self.attach(&id, "", false).await
    }

    /// openTab opens a tab, shows it and then loads the page, like Chrome's new tab button.
    /// Like it can return both a tab and an error (the tab exists, the page was slow).
    pub async fn open_tab(self: &Arc<Self>, raw_url: &str) -> (Option<Arc<Tab>>, Option<String>) {
        let t = match self.create_tab().await {
            Ok(t) => t,
            Err(e) => return (None, Some(e)),
        };
        self.activate(&t).await;
        let mut err = None;
        let u = normalize_url(raw_url);
        if !u.is_empty() {
            t.st().fresh = true;
            if let Err(e) = t.call_cmd("Page.navigate", json!({"url": u})).await {
                err = Some(e.to_string());
            }
        }
        (Some(t), err)
    }

    /// attach connects to a tab, prepares it (screen size, audio tap) and adds it to the tab strip.
    async fn attach(
        self: &Arc<Self>,
        id: &str,
        opener_id: &str,
        opened: bool,
    ) -> BResult<Arc<Tab>> {
        let chrome = self.root().ok_or("connection closed")?;
        // Do binds the tab to its context, so bound it with a timer instead of a timeout context.
        let attached = chrome
            .conn
            .call_with(
                None,
                "Target.attachToTarget",
                json!({"targetId": id, "flatten": true}),
                Some(CMD_TIMEOUT),
                None,
            )
            .await;
        let session = match attached {
            Ok(r) => r["sessionId"].as_str().unwrap_or_default().to_string(),
            Err(CdpError::Timeout) => return Err("the tab did not respond".into()),
            Err(e) => return Err(e.to_string()),
        };
        let t = Arc::new(Tab::build(
            id,
            opener_id,
            &session,
            Some(chrome.conn.clone()),
        ));
        if let Some(h) = self.hooks() {
            h.tab_created(&t);
        }
        // the listener sees every event of the tab: subscribe before the first command
        let rx = t.subscribe();
        tokio::spawn(self.clone().tab_events(t.clone(), rx));
        // the domains enabled on every target it attaches to
        let obscura = self.opt.engine.is_obscura();
        let setup = async {
            for (m, p) in [
                ("Runtime.enable", json!({})),
                ("Log.enable", json!({})),
                ("Network.enable", json!({})),
                ("Page.enable", json!({})),
                ("DOM.enable", json!({})),
            ] {
                if let Err(e) = t.call_cmd(m, p).await {
                    if !obscura {
                        return Err(e);
                    }
                    golog(&format!("obscura: {m}: {e}"));
                }
            }
            Ok::<(), CdpError>(())
        };
        if let Err(e) = setup.await {
            t.cancel();
            return Err(e.to_string());
        }
        // all setup steps together
        match tokio::time::timeout(CMD_TIMEOUT, self.attach_steps(&t)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                t.cancel();
                return Err(e);
            }
            Err(_) => {
                t.cancel();
                return Err(CdpError::Timeout.to_string());
            }
        }
        if self.hooks().is_some_and(|h| h.recording()) {
            // a tab opened during a recording is recorded too
            if let Some(h) = self.hooks() {
                h.rec_install(&t);
            }
        }

        {
            let mut st = self.st();
            let mut i = st.tabs.len();
            if opened {
                // like Chrome: right after the page that opened it (and its earlier tabs)
                if let Some(j) = st.tabs.iter().position(|x| x.id == opener_id) {
                    i = j + 1;
                    while i < st.tabs.len() && st.tabs[i].opener == opener_id {
                        i += 1;
                    }
                }
            }
            st.tabs.insert(i, t.clone());
        }
        self.notify();
        let (b, t2) = (self.clone(), t.clone());
        tokio::spawn(async move { b.refresh_nav(&t2).await });
        Ok(t)
    }

    async fn attach_steps(&self, t: &Arc<Tab>) -> BResult<()> {
        let (v, emu) = {
            let st = self.st();
            (st.view, st.emu.clone())
        };
        // Obscura lacks some of these (see engine::OBSCURA_NOTICE): a refused step is skipped there
        let obscura = self.opt.engine.is_obscura();
        let e = |r: CdpResult| match r {
            Err(err) if obscura => {
                golog(&format!("obscura: skipped: {err}"));
                Ok(())
            }
            r => r.map(|_| ()).map_err(|e| e.to_string()),
        };
        // first, so it runs before every other script in the page (stealth.rs)
        e(t.call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": &*STEALTH_JS, "runImmediately": true}),
            None,
        )
        .await)?;
        // a background tab must still behave as focused, so parallel sub-agents can use it
        e(t.call(
            "Emulation.setFocusEmulationEnabled",
            json!({"enabled": true}),
            None,
        )
        .await)?;
        // console and network, so the agent can see what the page does
        e(t.call(
            "Network.enable",
            json!({"maxTotalBufferSize": 10i64 << 20, "maxResourceBufferSize": 2i64 << 20}),
            None,
        )
        .await)?;
        e(t.call("Runtime.enable", json!({}), None).await)?;
        e(t.call("Log.enable", json!({}), None).await)?;
        if emu != Emulation::default() {
            // tabs opened later get the same device
            self.apply_emulation(t, v, &emu).await?;
        } else {
            if let Some(meta) = &self.ua_meta {
                // the full client hints of the normal user agent
                let up = UserAgentOverrideParams {
                    user_agent: self.ua.clone(),
                    user_agent_metadata: Some(meta.clone()),
                    ..Default::default()
                };
                e(t.call(
                    SET_USER_AGENT_OVERRIDE,
                    serde_json::to_value(&up).unwrap_or(Value::Null),
                    None,
                )
                .await)?;
            }
            e(t.call(
                "Emulation.setDeviceMetricsOverride",
                metrics(v, &emu).to_json(),
                None,
            )
            .await)?;
        }
        e(
            t.call("Runtime.addBinding", json!({"name": META_BINDING}), None)
                .await,
        )?;
        e(t.call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": &*META_JS, "runImmediately": true}),
            None,
        )
        .await)?;
        // getDisplayMedia gets the shared screen instead of a picker (share.rs)
        for s in self.hooks().map(|h| h.tab_scripts()).unwrap_or_default() {
            e(
                t.call("Runtime.addBinding", json!({"name": s.binding}), None)
                    .await,
            )?;
            e(t.call(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": s.source, "runImmediately": true}),
                None,
            )
            .await)?;
        }
        if self.opt.audio {
            e(
                t.call("Runtime.addBinding", json!({"name": AUDIO_BINDING}), None)
                    .await,
            )?;
            // A tab that a page opened is already loading, so also run the tap in it now.
            e(t.call(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": &*AUDIO_JS, "runImmediately": true}),
                None,
            )
            .await)?;
        }
        Ok(())
    }

    /// activate shows tab t to everyone: only the active tab streams frames.
    pub async fn activate(self: &Arc<Self>, t: &Arc<Tab>) {
        let (old, v) = {
            let mut st = self.st();
            let old = st.active.clone();
            if old.as_ref().is_some_and(|o| Arc::ptr_eq(o, t))
                || !st.tabs.iter().any(|x| Arc::ptr_eq(x, t))
            {
                return;
            }
            st.active = Some(t.clone());
            st.got_frame = false;
            (old, st.view)
        };
        self.notify();

        if let Some(old) = old {
            let _ = old
                .call(
                    "Page.stopScreencast",
                    json!({}),
                    Some(Duration::from_secs(3)),
                )
                .await;
        }
        let _ = self
            .root_call(
                "Target.activateTarget",
                json!({"targetId": t.id}),
                Some(CMD_TIMEOUT),
            )
            .await;
        // a popup tab owns a real OS window; activating it would pop that window
        // back over the desktop, so keep it minimized (the viewer tab is the UI)
        if !t.opener.is_empty() {
            self.minimize_window(&t.id).await;
        }
        self.start_screencast(t, v).await;
    }

    /// minimize_window hides the OS window of a popup that a page opened: the page
    /// keeps running and streaming (no backgrounding throttling), only the desktop
    /// window goes away. Errors are ignored: Obscura and tests have no window.
    async fn minimize_window(&self, target_id: &str) {
        if let Ok(res) = self
            .root_call(
                "Browser.getWindowForTarget",
                json!({"targetId": target_id}),
                Some(CMD_TIMEOUT),
            )
            .await
        {
            if let Some(wid) = res["windowId"].as_i64() {
                let _ = self
                    .root_call(
                        "Browser.setWindowBounds",
                        json!({"windowId": wid, "bounds": {"windowState": "minimized"}}),
                        Some(CMD_TIMEOUT),
                    )
                    .await;
            }
        }
    }

    pub async fn start_screencast(self: &Arc<Self>, t: &Arc<Tab>, v: View) {
        let q = v.quality;
        // Allow full device-pixel frames so text stays sharp. The factor has to be the
        // one Chrome actually renders at: asking for more than that made Chrome compose
        // an oversized surface and hand back a tiled 2x2 frame instead of the viewport.
        let mut scale = 1.0;
        let m = metrics(v, &self.screen_emu());
        if m.device_scale_factor > 1.0 {
            scale = m.device_scale_factor;
        }
        let (max_w, max_h) = (
            (v.width as f64 * scale) as i64,
            (v.height as f64 * scale) as i64,
        );
        // Chrome refuses while a page is between two documents, so retry briefly.
        for try_ in 0..10 {
            let r = t
                .call(
                    "Page.startScreencast",
                    json!({"format": "jpeg", "quality": q, "maxWidth": max_w, "maxHeight": max_h, "everyNthFrame": 1}),
                    Some(Duration::from_secs(5)),
                )
                .await;
            let Err(err) = r else { break };
            if t.canceled() || err.to_string().contains("already active") {
                break;
            }
            if try_ == 9 {
                golog(&format!("chrome: screencast: {err}"));
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        // A page that doesn't change sends no frame, so take one picture right away.
        let (b, t) = (self.clone(), t.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let need = {
                let st = b.st();
                st.active.as_ref().is_some_and(|a| Arc::ptr_eq(a, &t)) && !st.got_frame
            };
            if !need {
                return;
            }
            // Same clip as a normal capture: without it Chrome may hand back a frame
            // composed from an oversized surface, i.e. the viewport tiled at quarter size.
            let m = metrics(v, &b.screen_emu());
            let Ok(shot) = t
                .call_cmd("Page.captureScreenshot", capture_params(m, q))
                .await
            else {
                return;
            };
            let Some(jpeg) = decode_b64(shot["data"].as_str().unwrap_or_default()) else {
                return;
            };
            let mut pkt = Vec::with_capacity(1 + jpeg.len());
            pkt.push(PKT_FRAME);
            pkt.extend_from_slice(&jpeg);
            let pkt = Bytes::from(pkt);
            let ok = {
                let mut st = b.st();
                let ok = st.active.as_ref().is_some_and(|a| Arc::ptr_eq(a, &t)) && !st.got_frame;
                if ok {
                    st.frame = Some(pkt.clone());
                }
                ok
            };
            if ok {
                b.hub.broadcast_frame(pkt);
            }
        });
    }

    /// shot captures what tab t shows, as a JPEG.
    pub async fn shot(&self, t: &Arc<Tab>) -> BResult<Vec<u8>> {
        // Pin the capture to the configured screen. Without a clip, Chrome composes the
        // shot from whatever surface it last used for the tab, and that surface can be
        // larger than the view (the page is scrollable, or emulation changed the device
        // scale factor), which makes the viewport come out tiled 2x2 at quarter size.
        let (v, emu) = self.screen();
        let m = metrics(v, &emu);
        let shot = t
            .call("Page.captureScreenshot", capture_params(m, 80), None)
            .await
            .map_err(|e| e.to_string())?;
        decode_b64(shot["data"].as_str().unwrap_or_default())
            .ok_or_else(|| "bad screenshot data".to_string())
    }

    /// closeTab removes a tab. Closing the last tab opens a fresh one, so there is always a tab.
    pub async fn close_tab(self: &Arc<Self>, id: &str) {
        let (t, last) = {
            let st = self.st();
            let Some(i) = st.tabs.iter().position(|t| t.id == id) else {
                return;
            };
            (st.tabs[i].clone(), st.tabs.len() == 1)
        };

        if last {
            let _ = self.open_tab(&self.opt.start_url).await;
        }

        let next = {
            let mut st = self.st();
            let mut next = None;
            if let Some(i) = st.tabs.iter().position(|t| t.id == id) {
                st.tabs.remove(i);
                let is_active = st.active.as_ref().is_some_and(|a| Arc::ptr_eq(a, &t));
                if is_active && !st.tabs.is_empty() {
                    next = Some(st.tabs[i.min(st.tabs.len() - 1)].clone()); // like Chrome: the tab to the right
                }
                if is_active {
                    st.active = None;
                }
            }
            let prefix = format!("{id}|");
            st.streams.retain(|k, _| !k.starts_with(&prefix));
            next
        };
        self.notify();
        if let Some(next) = next {
            self.activate(&next).await;
        }
        t.cancel(); // closes the target in Chrome
    }

    // ---------- Events from Chrome ----------

    /// Browser-wide events: tabs appearing, disappearing, changing title/URL.
    /// Must return fast and must not call Chrome directly.
    async fn browser_events(
        self: Arc<Self>,
        mut rx: tokio::sync::mpsc::UnboundedReceiver<Arc<Event>>,
    ) {
        while let Some(ev) = rx.recv().await {
            self.on_browser_event(&ev);
        }
    }

    fn on_browser_event(self: &Arc<Self>, ev: &Event) {
        match ev.method.as_str() {
            "Target.targetCreated" => {
                let info = &ev.params["targetInfo"];
                if info["type"] == "page" && info["subtype"].as_str().unwrap_or("").is_empty() {
                    let (b, info) = (self.clone(), info.clone());
                    tokio::spawn(async move { b.adopt(info).await });
                }
            }
            "Target.targetInfoChanged" => self.on_info(&ev.params["targetInfo"]),
            "Target.targetDestroyed" => {
                let id = ev.params["targetId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                self.enqueue(move |b| async move { b.close_tab(&id).await });
            }
            "Browser.downloadWillBegin" => {
                let p = &ev.params;
                let s = |k: &str| p[k].as_str().unwrap_or_default().to_string();
                self.on_download(DownloadEvent::WillBegin {
                    guid: s("guid"),
                    url: s("url"),
                    suggested_filename: s("suggestedFilename"),
                });
            }
            "Browser.downloadProgress" => {
                let p = &ev.params;
                let s = |k: &str| p[k].as_str().unwrap_or_default().to_string();
                self.on_download(DownloadEvent::Progress {
                    guid: s("guid"),
                    total_bytes: p["totalBytes"].as_f64().unwrap_or(0.0),
                    received_bytes: p["receivedBytes"].as_f64().unwrap_or(0.0),
                    state: s("state"),
                    file_path: s("filePath"),
                });
            }
            "Target.targetCrashed" => {
                let id = ev.params["targetId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                self.chat_later("A tab crashed and was reloaded".into());
                self.enqueue(move |b| async move {
                    let t = b.st().tabs.iter().find(|t| t.id == id).cloned();
                    if let Some(t) = t {
                        let _ = t.call_cmd("Page.reload", json!({})).await;
                    }
                });
            }
            _ => {}
        }
    }

    fn on_download(&self, ev: DownloadEvent) {
        if let Some(done) = self.dl.event(ev, &self.opt.download_dir) {
            self.chat_later(format!("Downloaded {}", done.name));
            if let Some(h) = self.hooks() {
                h.download_done(&done);
            }
        }
    }

    /// adopt picks up a tab that a page opened (target=_blank link, window.open…).
    async fn adopt(self: Arc<Self>, info: Value) {
        let id = info["targetId"].as_str().unwrap_or_default().to_string();
        {
            let mut known = self.create_mu.lock().await;
            if known.contains(&id) {
                return;
            }
            known.insert(id.clone());
        }

        let (full, background) = {
            let st = self.st();
            (
                st.tabs.len() as i64 >= self.opt.max_tabs,
                st.bg_until.is_some_and(|u| Instant::now() < u),
            )
        };
        if full {
            let _ = self
                .root_call(
                    "Target.closeTarget",
                    json!({"targetId": id}),
                    Some(CMD_TIMEOUT),
                )
                .await;
            self.hub
                .system_chat("Tab limit reached — close a tab first");
            return;
        }
        let opener = info["openerId"].as_str().unwrap_or_default().to_string();
        let t = match self.attach(&id, &opener, true).await {
            Ok(t) => t,
            Err(e) => {
                golog(&format!("chrome: new tab: {e}"));
                return;
            }
        };
        let mut info = info;
        if let Ok(res) = self
            .root_call(
                "Target.getTargetInfo",
                json!({"targetId": id}),
                Some(CMD_TIMEOUT),
            )
            .await
        {
            if res.get("targetInfo").is_some_and(|v| !v.is_null()) {
                info = res["targetInfo"].clone(); // the page kept loading while we attached
            }
        }
        self.on_info(&info);
        if !background {
            let t2 = t.clone();
            self.enqueue(move |b| async move { b.activate(&t2).await });
        } else if !opener.is_empty() {
            // a background popup is never activated, so minimize its OS window here
            let id = t.id.clone();
            self.enqueue(move |b| async move { b.minimize_window(&id).await });
        }
    }

    fn on_info(self: &Arc<Self>, info: &Value) {
        let id = info["targetId"].as_str().unwrap_or_default();
        let (title, url) = (
            info["title"].as_str().unwrap_or_default(),
            info["url"].as_str().unwrap_or_default(),
        );
        let (t, changed) = {
            let st = self.st();
            let Some(t) = st.tabs.iter().find(|t| t.id == id).cloned() else {
                return;
            };
            let mut ts = t.st();
            let changed = ts.title != title || ts.url != url;
            ts.title = title.to_string();
            ts.url = url.to_string();
            drop(ts);
            (t, changed)
        };
        if self.drop_unwanted(url, id) {
            return;
        }
        if changed {
            self.notify();
            let b = self.clone();
            tokio::spawn(async move { b.refresh_nav(&t).await });
        }
    }

    /// dropUnwanted closes a tab that Chrome opened on its own to an internal settings page — its
    /// auto-update "About Chrome" page (chrome://settings/help). The app never opens settings itself,
    /// so any such tab is unwanted; closing it stops the browser from "jumping to Settings".
    fn drop_unwanted(&self, url: &str, id: &str) -> bool {
        if !url.starts_with("chrome://settings") {
            return false;
        }
        let id = id.to_string();
        self.enqueue(move |b| async move { b.close_tab(&id).await });
        true
    }

    /// Events of one tab. Must return fast and must not call Chrome directly.
    async fn tab_events(
        self: Arc<Self>,
        t: Arc<Tab>,
        mut rx: tokio::sync::mpsc::UnboundedReceiver<Arc<Event>>,
    ) {
        let mut cancel = t.cancel.subscribe();
        loop {
            let ev = tokio::select! {
                ev = rx.recv() => ev,
                _ = cdp::flagged(&mut cancel) => None,
            };
            let Some(ev) = ev else { return };
            self.on_tab_event(&t, &ev);
        }
    }

    fn on_tab_event(self: &Arc<Self>, t: &Arc<Tab>, ev: &Event) {
        if let Some(h) = self.hooks() {
            h.tab_event(t, ev);
        }
        let p = &ev.params;
        match ev.method.as_str() {
            "Page.screencastFrame" => {
                let sid = p["sessionId"].as_i64().unwrap_or(0);
                let t2 = t.clone();
                tokio::spawn(async move {
                    let _ = t2
                        .call_cmd("Page.screencastFrameAck", json!({"sessionId": sid}))
                        .await;
                });
                let Some(jpeg) = decode_b64(p["data"].as_str().unwrap_or_default()) else {
                    return;
                };
                let mut pkt = Vec::with_capacity(1 + jpeg.len());
                pkt.push(PKT_FRAME);
                pkt.extend_from_slice(&jpeg);
                let pkt = Bytes::from(pkt);
                let (active, drift) = {
                    let mut st = self.st();
                    let active = st.active.as_ref().is_some_and(|a| Arc::ptr_eq(a, t));
                    if active {
                        st.frame = Some(pkt.clone());
                        st.got_frame = true;
                        if let Some(h) = self.hooks() {
                            h.frame(&pkt[1..]);
                        }
                    }
                    let mut drift = false;
                    if active {
                        if let Some(md) = p.get("metadata").filter(|m| m.is_object()) {
                            let want = metrics(st.view, &st.emu);
                            let (dw, dh) = (
                                md["deviceWidth"].as_f64().unwrap_or(0.0),
                                md["deviceHeight"].as_f64().unwrap_or(0.0),
                            );
                            drift = (dw - want.width as f64).abs() > 1.0
                                || (dh - want.height as f64).abs() > 1.0;
                            let mut ts = t.st();
                            if drift
                                && ts
                                    .refit_at
                                    .is_none_or(|r| r.elapsed() > Duration::from_secs(2))
                            {
                                ts.refit_at = Some(Instant::now());
                            } else {
                                drift = false;
                            }
                        }
                    }
                    (active, drift)
                };
                if drift {
                    let t = t.clone();
                    self.enqueue(move |b| async move { b.refit(&t).await });
                }
                if active {
                    self.hub.broadcast_frame(pkt);
                }
            }

            "Runtime.bindingCalled" => {
                let name = p["name"].as_str().unwrap_or_default();
                let payload = p["payload"].as_str().unwrap_or_default();
                match name {
                    AUDIO_BINDING => self.on_audio(t, payload),
                    META_BINDING => self.on_meta(t, payload),
                    _ => {
                        if let Some(h) = self.hooks() {
                            h.binding(t, name, payload);
                        }
                    }
                }
            }

            // single-page apps change the address without loading
            "Page.navigatedWithinDocument" => {
                if p["frameId"].as_str() == Some(t.id.as_str()) {
                    let url = p["url"].as_str().unwrap_or_default().to_string();
                    self.page_changed_later(t, url);
                }
            }

            "Page.frameNavigated" => {
                let f = &p["frame"];
                if f["parentId"].as_str().is_some_and(|s| !s.is_empty()) {
                    return;
                }
                let url = f["url"].as_str().unwrap_or_default();
                if self.drop_unwanted(url, &t.id) {
                    return;
                }
                let reset = {
                    let mut ts = t.st();
                    // a new document reports its own
                    ts.doc_title.clear();
                    ts.favicon.clear();
                    let reset = ts.fresh && url != "about:blank";
                    if reset {
                        ts.fresh = false;
                    }
                    reset
                };
                self.notify();
                self.page_changed_later(t, url.to_string());
                if reset {
                    // a new tab starts at about:blank; going "back" there makes no sense
                    let (b, t) = (self.clone(), t.clone());
                    tokio::spawn(async move {
                        let _ = t.call_cmd("Page.resetNavigationHistory", json!({})).await;
                        b.refresh_nav(&t).await;
                    });
                }
            }

            "Page.frameStartedLoading" => {
                if p["frameId"].as_str() == Some(t.id.as_str()) {
                    self.set_loading(t, true);
                }
            }

            "Page.frameStoppedLoading" => {
                if p["frameId"].as_str() == Some(t.id.as_str()) {
                    self.set_loading(t, false);
                    let (b, t) = (self.clone(), t.clone());
                    tokio::spawn(async move { b.refresh_nav(&t).await });
                }
            }

            "Page.javascriptDialogOpening" => {
                let t2 = t.clone();
                tokio::spawn(async move {
                    let _ = t2
                        .call_cmd("Page.handleJavaScriptDialog", json!({"accept": true}))
                        .await;
                });
                let msg = p["message"].as_str().unwrap_or_default();
                if !msg.is_empty() {
                    self.chat_later(format!("Page says: {msg}"));
                }
            }
            _ => {}
        }
    }

    fn page_changed_later(&self, t: &Arc<Tab>, url: String) {
        if let Some(h) = self.hooks().cloned() {
            let t = t.clone();
            tokio::spawn(async move { h.page_changed(&t, &url) });
        }
    }

    fn set_loading(&self, t: &Arc<Tab>, v: bool) {
        let changed = {
            let mut ts = t.st();
            let changed = ts.loading != v;
            ts.loading = v;
            changed
        };
        if changed {
            self.notify();
        }
    }

    /// onMeta handles the title/favicon report of a page. The page can call the binding itself, so validate.
    fn on_meta(&self, t: &Arc<Tab>, p: &str) {
        #[derive(Deserialize)]
        struct Meta {
            t: Option<String>,
            i: Option<String>,
        }
        if p.len() > 80 * 1024 {
            return;
        }
        let Ok(m) = serde_json::from_str::<Meta>(p) else {
            return;
        };
        let (mut title, mut icon) = (m.t.unwrap_or_default(), m.i.unwrap_or_default());
        if title.chars().count() > 300 {
            title = title.chars().take(300).collect();
        }
        if icon.len() > 64 * 1024
            || !(icon.starts_with("http://")
                || icon.starts_with("https://")
                || icon.starts_with("data:image/"))
        {
            icon.clear();
        }
        let changed = {
            let mut ts = t.st();
            let changed = ts.doc_title != title || ts.favicon != icon;
            ts.doc_title = title;
            ts.favicon = icon;
            changed
        };
        if changed {
            self.notify();
        }
    }

    async fn refresh_nav(&self, t: &Arc<Tab>) {
        let seq = {
            let mut ts = t.st();
            ts.nav_seq += 1;
            ts.nav_seq
        };
        let Ok(h) = t.call_cmd("Page.getNavigationHistory", json!({})).await else {
            return;
        };
        let cur = h["currentIndex"].as_i64().unwrap_or(0);
        let n = h["entries"].as_array().map_or(0, |e| e.len()) as i64;
        let (back, fwd) = (cur > 0, cur < n - 1);
        let changed = {
            let mut ts = t.st();
            if seq != ts.nav_seq {
                // a newer check started meanwhile
                return;
            }
            let changed = ts.can_back != back || ts.can_fwd != fwd;
            ts.can_back = back;
            ts.can_fwd = fwd;
            changed
        };
        if changed {
            self.notify();
        }
    }

    /// Payload from a page: "<stream id>|<sample rate>|<base64 int16 stereo PCM>".
    /// The page itself can call the binding, so validate everything.
    fn on_audio(&self, t: &Arc<Tab>, p: &str) {
        let parts: Vec<&str> = p.splitn(3, '|').collect();
        if parts.len() != 3 || parts[2].len() > 512 * 1024 {
            return;
        }
        let (Ok(js_id), Ok(rate)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) else {
            return;
        };
        if !(1..=255).contains(&js_id) || !(8000..=96000).contains(&rate) {
            return;
        }
        let Some(pcm) = decode_b64(parts[2]) else {
            return;
        };
        if pcm.is_empty() || pcm.len() % 4 != 0 {
            return;
        }

        let (was_audible, muted, sid) = {
            let mut st = self.st();
            let mut ts = t.st();
            let was = ts.audible;
            ts.audible = true;
            ts.last_audio = Some(Instant::now());
            let muted = ts.muted;
            drop(ts);
            let key = format!("{}|{}", t.id, parts[0]);
            let sid = match st.streams.get(&key) {
                Some(s) => *s,
                None => {
                    // give every (tab, stream) its own id so viewers keep them apart
                    st.next_stream = st.next_stream % 255 + 1;
                    let s = st.next_stream as u8;
                    st.streams.insert(key, s);
                    s
                }
            };
            (was, muted, sid)
        };
        if !was_audible {
            self.notify();
        }
        if muted {
            return;
        }
        if let Some(h) = self.hooks() {
            h.audio(t, rate, parts[2]);
        }

        let mut pkt = Vec::with_capacity(6 + pcm.len());
        pkt.push(PKT_AUDIO);
        pkt.push(sid);
        pkt.extend_from_slice(&(rate as u32).to_le_bytes());
        pkt.extend_from_slice(&pcm);
        self.hub.broadcast_audio(Bytes::from(pkt));
    }

    /// Clears the speaker icon of tabs that went quiet.
    async fn watch_audio(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.tick().await;
        loop {
            tick.tick().await;
            let mut changed = false;
            {
                let st = self.st();
                for t in &st.tabs {
                    let mut ts = t.st();
                    if ts.audible
                        && ts
                            .last_audio
                            .is_none_or(|l| l.elapsed() > Duration::from_millis(1500))
                    {
                        ts.audible = false;
                        changed = true;
                    }
                }
            }
            if changed {
                self.notify();
            }
        }
    }

    // ---------- State for the web UI ----------

    /// notify sends the tab list to everyone, batching changes that come in quick succession.
    pub fn notify(&self) {
        {
            let mut n = lock(&self.notifying);
            if *n {
                return;
            }
            *n = true;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            *lock(&self.notifying) = false;
            return;
        }
        let b = self.arc();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            *lock(&b.notifying) = false;
            let (tabs, active, _, _) = b.state();
            b.hub.broadcast_tabs(tabs, active);
        });
    }

    /// `State`: tabs, active tab id, the cached frame packet (`[1][JPEG]`), the view.
    pub fn state(&self) -> (Vec<TabInfo>, String, Option<Bytes>, View) {
        let st = self.st();
        let tabs = st
            .tabs
            .iter()
            .map(|t| {
                let ts = t.st();
                let title = if ts.doc_title.is_empty() {
                    ts.title.clone()
                } else {
                    ts.doc_title.clone()
                };
                TabInfo {
                    id: t.id.clone(),
                    title,
                    url: ts.url.clone(),
                    favicon: ts.favicon.clone(),
                    loading: ts.loading,
                    audible: ts.audible,
                    muted: ts.muted,
                    can_back: ts.can_back,
                    can_fwd: ts.can_fwd,
                }
            })
            .collect();
        let active = st.active.as_ref().map(|a| a.id.clone()).unwrap_or_default();
        (tabs, active, st.frame.clone(), st.view)
    }

    // ---------- Commands from the web UI ----------

    pub fn new_tab_cmd(&self, raw: &str) {
        let mut raw = raw.to_string();
        self.enqueue(move |b| async move {
            let full = b.st().tabs.len() as i64 >= b.opt.max_tabs;
            if full {
                b.hub.system_chat("Tab limit reached — close a tab first");
                return;
            }
            if raw.is_empty() {
                raw = b.opt.start_url.clone();
            }
            if let (_, Some(e)) = b.open_tab(&raw).await {
                golog(&format!("chrome: new tab: {e}"));
            }
        });
    }

    pub fn close_tab_cmd(&self, id: &str) {
        let id = id.to_string();
        self.enqueue(move |b| async move { b.close_tab(&id).await });
    }

    pub fn activate_tab_cmd(&self, id: &str) {
        let id = id.to_string();
        self.enqueue(move |b| async move {
            let t = b.st().tabs.iter().find(|t| t.id == id).cloned();
            if let Some(t) = t {
                b.activate(&t).await;
            }
        });
    }

    pub fn move_tab_cmd(&self, id: &str, to: i64) {
        {
            let mut st = self.st();
            if let Some(i) = st.tabs.iter().position(|t| t.id == id) {
                let t = st.tabs.remove(i);
                let to = to.clamp(0, st.tabs.len() as i64) as usize;
                st.tabs.insert(to, t);
            }
        }
        self.notify();
    }

    pub fn mute_tab_cmd(&self, id: &str) {
        {
            let st = self.st();
            if let Some(t) = st.tabs.iter().find(|t| t.id == id) {
                let mut ts = t.st();
                ts.muted = !ts.muted;
            }
        }
        self.notify();
    }

    /// SetView changes the screen size and image quality for everyone.
    pub fn set_view_cmd(&self, mut v: View) {
        v.width = v.width.clamp(640, 2560);
        v.height = v.height.clamp(360, 1600);
        v.quality = v.quality.clamp(10, 100);
        self.enqueue(move |b| async move {
            let (emu, tabs, t) = {
                let mut st = b.st();
                st.view = v;
                st.got_frame = false;
                (st.emu.clone(), st.tabs.clone(), st.active.clone())
            };
            for tab in &tabs {
                let _ = tab
                    .call(
                        "Emulation.setDeviceMetricsOverride",
                        metrics(v, &emu).to_json(),
                        Some(Duration::from_secs(5)),
                    )
                    .await;
            }
            if let Some(t) = t {
                let _ = t
                    .call(
                        "Page.stopScreencast",
                        json!({}),
                        Some(Duration::from_secs(5)),
                    )
                    .await;
                b.start_screencast(&t, v).await;
            }
            b.hub.broadcast_view(v);
        });
    }

    /// refit restores a tab's screen size after Chrome changed it on its own (the
    /// frames came out squashed until the size was switched back and forth by hand).
    /// Setting the same size again is ignored by Chrome, so step off it first.
    async fn refit(self: &Arc<Self>, t: &Arc<Tab>) {
        let (v, emu, active) = {
            let st = self.st();
            (
                st.view,
                st.emu.clone(),
                st.active.as_ref().is_some_and(|a| Arc::ptr_eq(a, t)),
            )
        };
        let m = metrics(v, &emu);
        let mut nudge = m;
        nudge.height += 1;
        let d = Some(Duration::from_secs(5));
        let _ = t
            .call("Emulation.setDeviceMetricsOverride", nudge.to_json(), d)
            .await;
        let _ = t
            .call("Emulation.setDeviceMetricsOverride", m.to_json(), d)
            .await;
        if active {
            let _ = t.call("Page.stopScreencast", json!({}), d).await;
            self.start_screencast(t, v).await;
        }
    }

    pub fn navigate_cmd(&self, raw: &str) {
        let u = normalize_url(raw);
        if u.is_empty() {
            return;
        }
        self.on_tab(move |t| async move {
            t.call_cmd("Page.navigate", json!({"url": u}))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
    }

    pub fn history_cmd(&self, delta: i64) {
        self.on_tab(move |t| async move { go_history(&t, delta).await.map(|_| ()) });
    }

    pub fn reload_cmd(&self) {
        self.on_tab(|t| async move {
            t.call_cmd("Page.reload", json!({}))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
    }

    pub fn stop_cmd(&self) {
        self.on_tab(|t| async move {
            t.call_cmd("Page.stopLoading", json!({}))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
    }
}

/// `page.CaptureScreenshotParams` with the clip of the configured screen (see `shot`). A browser
/// that was never sized (tests) has no view: the clip is left off then, so the capture keeps
/// Chrome's own default instead of an empty region.
fn capture_params(m: crate::emulate::Metrics, quality: i64) -> Value {
    let mut p = json!({
        "format": "jpeg",
        "quality": quality,
        "fromSurface": false,
        "captureBeyondViewport": false,
    });
    if m.width > 0 && m.height > 0 {
        let mut scale = m.device_scale_factor;
        if scale <= 0.0 {
            scale = 1.0;
        }
        // Clip is in CSS pixels; Scale keeps the device-pixel resolution the shots had before.
        p["clip"] = json!({"x": 0, "y": 0, "width": m.width as f64, "height": m.height as f64, "scale": scale});
    }
    p
}

fn decode_b64(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// go_history moves back (-1) or forward (1) in the tab's history; false if there is nowhere to go.
pub async fn go_history(t: &Arc<Tab>, delta: i64) -> BResult<bool> {
    let h = t
        .call_cmd("Page.getNavigationHistory", json!({}))
        .await
        .map_err(|e| e.to_string())?;
    let i = h["currentIndex"].as_i64().unwrap_or(0) + delta;
    let entries = h["entries"].as_array().cloned().unwrap_or_default();
    if i < 0 || i >= entries.len() as i64 {
        return Ok(false);
    }
    t.call_cmd(
        "Page.navigateToHistoryEntry",
        json!({"entryId": entries[i as usize]["id"]}),
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(true)
}

pub fn normalize_url(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    if raw.starts_with("http://") || raw.starts_with("https://") || raw == "about:blank" {
        return raw.to_string();
    }
    if raw.contains("://") {
        return String::new(); // block file://, chrome:// … so nobody can read files on the server
    }
    let looks_like_host = !raw.contains(' ') && (raw.contains('.') || raw.starts_with("localhost"));
    if looks_like_host {
        let host = raw
            .split('/')
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("");
        if host == "localhost" || host.parse::<std::net::IpAddr>().is_ok() {
            // LAN machines rarely have https
            return format!("http://{raw}");
        }
        return format!("https://{raw}");
    }
    format!("https://www.google.com/search?q={}", query_escape(raw))
}

/// url.QueryEscape
fn query_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------- Mouse ----------

const BUTTON_NAMES: [&str; 3] = ["left", "middle", "right"];

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

impl Browser {
    pub fn drop_file_cmd(&self, x: f64, y: f64, path: &str) {
        let path = path.to_string();
        self.enqueue(move |b| async move {
            let (t, v) = {
                let st = b.st();
                (st.active.clone(), st.view)
            };
            let Some(t) = t else {
                return;
            };
            let (px, py) = (
                crate::browser::clamp01(x) * v.width as f64,
                crate::browser::clamp01(y) * v.height as f64,
            );
            let d = Some(INPUT_TIMEOUT);
            // items stays empty: newer Chrome requires DragDataItem.data, and the files
            // alone arrive as a proper File drop ("Files" in the DataTransfer types).
            for typ in ["dragEnter", "dragOver", "drop"] {
                if let Err(e) = t
                    .call(
                        "Input.dispatchDragEvent",
                        serde_json::json!({
                            "type": typ,
                            "x": px,
                            "y": py,
                            "modifiers": 0,
                            "data": {
                                "items": [],
                                "files": [path],
                                "dragOperationsMask": 1
                            }
                        }),
                        d,
                    )
                    .await
                {
                    if !t.canceled() {
                        golog(&format!("chrome: file drop ({typ}): {e}"));
                    }
                }
            }
        });
    }

    /// pasteImage: paste a picture (from the viewer's clipboard) into the active page as a
    /// ClipboardEvent, the way an app that accepts pasted images (Gmail, chat boxes, ...) gets one.
    pub fn paste_image_cmd(&self, mime: &str, name: &str, data: &[u8]) {
        let mime = mime.to_string();
        let name = name.to_string();
        let b64 = base64::engine::general_purpose::STANDARD.encode(data);
        self.enqueue(move |b| async move {
            let Some(t) = b.st().active.clone() else {
                return;
            };
            // the base64 rides as a JSON string literal, so it needs no extra escaping for JS
            let expr = format!(
                r#"(() => {{
  const bin = atob("{}");
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  const file = new File([bytes], "{}", {{ type: "{}" }});
  const dt = new DataTransfer();
  dt.items.add(file);
  const el = document.activeElement || document.body;
  const ok = el.dispatchEvent(new ClipboardEvent("paste", {{ clipboardData: dt, bubbles: true, cancelable: true }}));
  return JSON.stringify({{ target: el.id || el.tagName || "?", accepted: !ok }});
}})()"#,
                b64,
                name.replace('"', ""),
                mime.replace('"', "")
            );
            match t
                .call("Runtime.evaluate", serde_json::json!({"expression": expr}), Some(INPUT_TIMEOUT))
                .await
            {
                Err(e) => golog(&format!("chrome: paste image: {e}")),
                Ok(r) => {
                    if let Some(d) = r.get("exceptionDetails") {
                        golog(&format!("chrome: paste image: {}", crate::agent::exception_error(d)));
                    }
                }
            }
        });
    }

    pub fn mouse_cmd(&self, m: &Msg) {
        let kind = match m.event.as_str() {
            "move" => JobKind::Move,
            "wheel" => JobKind::Wheel,
            _ => JobKind::Mouse,
        };
        self.push(Job {
            kind,
            f: None,
            mouse: m.clone(),
        });
    }

    /// dispatchMouse runs from the queue.
    async fn dispatch_mouse(self: Arc<Self>, m: Msg) {
        let mods = fix_mods(m.mods);
        let (t, v) = {
            let mut st = self.st();
            if m.event == "down" && (m.button == 1 || mods & (MOD_CTRL | MOD_META) != 0) {
                st.bg_until = Some(Instant::now() + Duration::from_millis(1500));
                // middle/ctrl-click opens a background tab
            }
            (st.active.clone(), st.view)
        };
        let Some(t) = t else { return };
        if let Err(e) = self.send_mouse(&t, v, &m, mods).await {
            if !t.canceled() {
                golog(&format!("chrome: {e}"));
            }
        }
    }

    async fn send_mouse(&self, t: &Arc<Tab>, v: View, m: &Msg, mods: i64) -> BResult<()> {
        let (x, y) = (
            clamp01(m.x) * v.width as f64,
            clamp01(m.y) * v.height as f64,
        );
        let d = Some(INPUT_TIMEOUT);
        if m.event == "wheel" {
            // cdproto drops zero deltas from the JSON, but Chrome requires both, so wheel events use our own params.
            return t
                .call(
                    "Input.dispatchMouseEvent",
                    json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": m.dx, "deltaY": m.dy, "modifiers": mods}),
                    d,
                )
                .await
                .map(|_| ())
                .map_err(|e| e.to_string());
        }
        let mut p = json!({"x": x, "y": y, "button": "none"});
        if mods != 0 {
            p["modifiers"] = json!(mods);
        }
        if m.buttons != 0 {
            p["buttons"] = json!(m.buttons);
        }
        match m.event.as_str() {
            "down" | "up" => {
                p["type"] = json!(if m.event == "up" {
                    "mouseReleased"
                } else {
                    "mousePressed"
                });
                if m.button >= 0 && (m.button as usize) < BUTTON_NAMES.len() {
                    p["button"] = json!(BUTTON_NAMES[m.button as usize]);
                }
                p["clickCount"] = json!(m.count.max(1));
            }
            "move" => {
                p["type"] = json!("mouseMoved");
                // button held while dragging
                if m.buttons & 1 != 0 {
                    p["button"] = json!("left");
                } else if m.buttons & 2 != 0 {
                    p["button"] = json!("right");
                } else if m.buttons & 4 != 0 {
                    p["button"] = json!("middle");
                }
            }
            _ => return Ok(()),
        }
        t.call("Input.dispatchMouseEvent", p, d)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// inputCtx bounds one input command, so a page that hangs can't stall the queue
/// (and every later click and key) forever.
pub const INPUT_TIMEOUT: Duration = Duration::from_secs(5);

// ---------- Keyboard ----------

pub const MOD_CTRL: i64 = 2;
pub const MOD_META: i64 = 4;
pub const MOD_SHIFT: i64 = 8;

/// A Mac user driving a Windows/Linux server: Cmd acts as Ctrl.
pub fn fix_mods(mut mods: i64) -> i64 {
    if !cfg!(target_os = "macos") && mods & MOD_META != 0 {
        mods = (mods & !MOD_META) | MOD_CTRL;
    }
    mods
}

/// On macOS, Chrome doesn't handle Cmd+A/C/X/Z from DevTools key events by itself;
/// the editing command has to be sent along.
fn mac_command(key: &str) -> Option<&'static str> {
    Some(match key {
        "a" => "selectAll",
        "c" => "copy",
        "x" => "cut",
        "z" => "undo",
        _ => return None,
    })
}

const SELECTION_JS: &str = r#"(() => {
  const a = document.activeElement;
  if (a && (a.tagName === 'INPUT' || a.tagName === 'TEXTAREA') && typeof a.selectionStart === 'number')
    return a.value.substring(a.selectionStart, a.selectionEnd);
  return String(window.getSelection() || '');
})()"#;

impl Browser {
    /// onCopy receives the selected text when the controller presses Ctrl/Cmd+C or X.
    pub fn key_cmd(&self, m: &Msg, on_copy: Box<dyn FnOnce(String) + Send>) {
        let mods = fix_mods(m.mods);
        let shortcut = mods & (MOD_CTRL | MOD_META) != 0;
        let lower = m.key.to_lowercase();

        let mut p = serde_json::Map::new();
        // cdproto omits empty strings and zero numbers
        if !m.key.is_empty() {
            p.insert("key".into(), json!(m.key));
        }
        if !m.code.is_empty() {
            p.insert("code".into(), json!(m.code));
        }
        if mods != 0 {
            p.insert("modifiers".into(), json!(mods));
        }
        if m.key_code != 0 {
            p.insert("windowsVirtualKeyCode".into(), json!(m.key_code));
            p.insert("nativeVirtualKeyCode".into(), json!(m.key_code));
        }
        if m.event == "up" {
            p.insert("type".into(), json!("keyUp"));
        } else {
            let mut text = "";
            if m.key.chars().count() == 1 && !shortcut {
                text = &m.key;
            } else if m.key == "Enter" {
                text = "\r";
            }
            if !text.is_empty() {
                p.insert("type".into(), json!("keyDown"));
                p.insert("text".into(), json!(text));
                p.insert("unmodifiedText".into(), json!(text));
            } else {
                p.insert("type".into(), json!("rawKeyDown"));
            }
            if shortcut && cfg!(target_os = "macos") {
                if let Some(mut cmd) = mac_command(&lower) {
                    if cmd == "undo" && mods & MOD_SHIFT != 0 {
                        cmd = "redo";
                    }
                    p.insert("commands".into(), json!([cmd]));
                }
            }
        }

        let want_copy = m.event != "up" && shortcut && (lower == "c" || lower == "x");
        let p = Value::Object(p);
        self.on_tab(move |t| async move {
            if want_copy {
                if let Ok(Value::String(s)) = t.evaluate(SELECTION_JS, Some(INPUT_TIMEOUT)).await {
                    if !s.is_empty() {
                        on_copy(s);
                    }
                }
            }
            t.call("Input.dispatchKeyEvent", p, Some(INPUT_TIMEOUT))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
    }

    /// Text types a piece of text directly (IME input, paste…).
    pub fn text_cmd(&self, s: &str) {
        if s.is_empty() {
            return;
        }
        let s = s.to_string();
        self.on_tab(move |t| async move {
            t.call("Input.insertText", json!({"text": s}), Some(INPUT_TIMEOUT))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
    }
}

// ---------- What the hub sees (main.rs: `BrowserApi`) ----------

/// The hub's view of the shared browser.
/// implements it on the real browser and installs it with `Hub::set_browser`; `None` = `-no-browser`.
///
/// Every method is synchronous and must not block: the hub calls them while holding its state lock,
/// exactly where calls them under `h.mu` (long work is spawned onto the runtime). They must not
/// call back into the hub synchronously.
pub trait BrowserApi: Send + Sync {
    /// `State`: tabs, active tab id, the cached frame packet (`[1][JPEG]`), the view.
    fn state(&self) -> (Vec<TabInfo>, String, Option<Bytes>, View);
    fn navigate(&self, url: &str);
    /// `History(-1)` back, `History(1)` forward.
    fn history(&self, delta: i32);
    fn reload(&self);
    fn stop(&self);
    fn mouse(&self, m: &crate::Msg);
    /// `Key(m, onCopy)`: `on_copy` gets the copied text (called later, from the browser's own task).
    fn key(&self, m: &crate::Msg, on_copy: Box<dyn FnOnce(String) + Send>);
    fn text(&self, text: &str);
    fn drop_file(&self, x: f64, y: f64, path: &str);
    fn paste_image(&self, mime: &str, name: &str, data: &[u8]);
    fn new_tab(&self, url: &str);
    fn close_tab(&self, id: &str);
    fn activate_tab(&self, id: &str);
    fn move_tab(&self, id: &str, index: i64);
    fn mute_tab(&self, id: &str);
    fn set_view(&self, v: View);
    /// `pageOf`: URL and title of the active page.
    fn page_of(&self) -> (String, String);
    /// `releaseOwner(handle)`: drop the tab locks of a coding agent that left.
    fn release_owner(&self, handle: &str);
    /// `Close`: close Chrome on the way out.
    fn close(&self);
}

impl BrowserApi for Browser {
    fn state(&self) -> (Vec<TabInfo>, String, Option<Bytes>, View) {
        Browser::state(self)
    }
    fn navigate(&self, url: &str) {
        self.navigate_cmd(url);
    }
    fn history(&self, delta: i32) {
        self.history_cmd(delta as i64);
    }
    fn reload(&self) {
        self.reload_cmd();
    }
    fn stop(&self) {
        self.stop_cmd();
    }
    fn mouse(&self, m: &Msg) {
        self.mouse_cmd(m);
    }
    fn key(&self, m: &Msg, on_copy: Box<dyn FnOnce(String) + Send>) {
        self.key_cmd(m, on_copy);
    }
    fn text(&self, text: &str) {
        self.text_cmd(text);
    }
    fn drop_file(&self, x: f64, y: f64, path: &str) {
        self.drop_file_cmd(x, y, path);
    }
    fn paste_image(&self, mime: &str, name: &str, data: &[u8]) {
        self.paste_image_cmd(mime, name, data);
    }
    fn new_tab(&self, url: &str) {
        self.new_tab_cmd(url);
    }
    fn close_tab(&self, id: &str) {
        self.close_tab_cmd(id);
    }
    fn activate_tab(&self, id: &str) {
        self.activate_tab_cmd(id);
    }
    fn move_tab(&self, id: &str, index: i64) {
        self.move_tab_cmd(id, index);
    }
    fn mute_tab(&self, id: &str) {
        self.mute_tab_cmd(id);
    }
    fn set_view(&self, v: View) {
        self.set_view_cmd(v);
    }
    /// assets.rs `pageOf`
    fn page_of(&self) -> (String, String) {
        let st = self.st();
        let Some(a) = &st.active else {
            return (String::new(), String::new());
        };
        let ts = a.st();
        let title = if ts.doc_title.is_empty() {
            &ts.title
        } else {
            &ts.doc_title
        };
        (ts.url.clone(), title.clone())
    }
    fn release_owner(&self, handle: &str) {
        Browser::release_owner(self, handle);
    }
    fn close(&self) {
        self.close_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    /// browser_lane_test.rs: TestRunTabSerializesEachTabIndependently
    #[tokio::test(flavor = "multi_thread")]
    async fn run_tab_serializes_each_tab_independently() {
        let b = Browser::for_test(vec![Tab::detached("one"), Tab::detached("two")]);
        let started: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let (one_done_tx, one_done_rx) = oneshot::channel::<BResult<()>>();
        {
            let (b, started) = (b.clone(), started.clone());
            tokio::spawn(async move {
                let r = b
                    .run_tab("one", |_| async move {
                        lock(&started).insert("one".into(), Instant::now());
                        let _ = release_rx.await;
                        Ok(())
                    })
                    .await;
                let _ = one_done_tx.send(r);
            });
        }
        loop {
            if lock(&started).contains_key("one") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        {
            let started = started.clone();
            b.run_tab("two", |_| async move {
                lock(&started).insert("two".into(), Instant::now());
                Ok(())
            })
            .await
            .unwrap();
        }
        let parallel = {
            let s = lock(&started);
            s["two"] < s["one"] + Duration::from_millis(100)
        };
        assert!(
            parallel,
            "different tab lane did not start while the first tab was blocked"
        );
        release_tx.send(()).unwrap();
        one_done_rx.await.unwrap().unwrap();

        let (second_tx, mut second_rx) = mpsc::unbounded_channel::<()>();
        let (first_started_tx, first_started_rx) = oneshot::channel::<()>();
        let (finish_first_tx, finish_first_rx) = oneshot::channel::<()>();
        {
            let b = b.clone();
            tokio::spawn(async move {
                let _ = b
                    .run_tab("one", |_| async move {
                        let _ = first_started_tx.send(());
                        let _ = finish_first_rx.await;
                        Ok(())
                    })
                    .await;
            });
        }
        first_started_rx.await.unwrap();
        {
            let b = b.clone();
            tokio::spawn(async move {
                let _ = b
                    .run_tab("one", |_| async move {
                        let _ = second_tx.send(());
                        Ok(())
                    })
                    .await;
            });
        }
        tokio::select! {
            _ = second_rx.recv() => panic!("same tab lane ran callbacks concurrently"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        finish_first_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), second_rx.recv())
            .await
            .expect("second same-tab callback did not run after the first finished")
            .unwrap();
    }

    #[tokio::test]
    async fn run_tab_unknown_tab() {
        let b = Browser::for_test(vec![Tab::detached("one")]);
        let r: BResult<()> = b.run_tab("nope", |_| async { Ok(()) }).await;
        assert_eq!(r.unwrap_err(), "tab not found: nope");
    }

    /// Sessions and gesture locks: another agent's call waits for the session, the owner passes.
    #[tokio::test]
    async fn sessions_and_gestures() {
        let b = Browser::for_test(vec![Tab::detached("one")]);
        b.claim_tab("one", "alice", Duration::from_secs(60))
            .await
            .unwrap();
        // the owner and its sub-agents' tool calls are not blocked by its own session
        assert!(b.lock_gesture("one", "alice").await.unwrap().is_some());
        // another agent waits (not forever: the claim below is released)
        let b2 = b.clone();
        let waiter =
            tokio::spawn(async move { b2.lock_gesture("one", "bob").await.map(|g| g.is_some()) });
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(!waiter.is_finished(), "bob must wait for alice's session");
        b.release_owner("alice");
        assert!(waiter.await.unwrap().unwrap());
        // a missing tab means no lock to hold
        assert!(b.lock_gesture("zzz", "bob").await.unwrap().is_none());
        // sub-agents (who/…) are released with their parent
        b.claim_tab("one", "carol/1", Duration::from_secs(60))
            .await
            .unwrap();
        b.release_owner("carol");
        b.claim_tab("one", "dave", Duration::from_secs(60))
            .await
            .unwrap();
    }

    fn mouse(event: &str, x: f64, dx: f64, mods: i64) -> Job {
        let m = Msg {
            event: event.into(),
            x,
            dx,
            mods,
            ..Default::default()
        };
        Job {
            kind: match event {
                "move" => JobKind::Move,
                "wheel" => JobKind::Wheel,
                _ => JobKind::Mouse,
            },
            f: None,
            mouse: m,
        }
    }

    #[test]
    fn queued_moves_and_wheels_merge_but_clicks_keep_order() {
        let mut q = VecDeque::new();
        for j in [mouse("move", 0.1, 0.0, 0), mouse("move", 0.2, 0.0, 0)] {
            if !merge_locked(&mut q, &j) {
                q.push_back(j);
            }
        }
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].mouse.x, 0.2); // newest position
        for j in [mouse("down", 0.3, 0.0, 0), mouse("move", 0.4, 0.0, 0)] {
            if !merge_locked(&mut q, &j) {
                q.push_back(j);
            }
        }
        // the move after the click does not merge into the move before it
        assert_eq!(q.len(), 3);
        for j in [
            mouse("wheel", 0.5, 3.0, 0),
            mouse("wheel", 0.6, 4.0, 0),
            mouse("wheel", 0.7, 1.0, 2),
        ] {
            if !merge_locked(&mut q, &j) {
                q.push_back(j);
            }
        }
        assert_eq!(q.len(), 5);
        assert_eq!(q[3].mouse.dx, 7.0); // wheel deltas add up
        assert_eq!(q[4].mouse.mods, 2); // other modifiers stay apart
    }

    #[test]
    fn normalize_url_cases() {
        for (raw, want) in [
            ("", ""),
            ("  https://a.b/c ", "https://a.b/c"),
            ("about:blank", "about:blank"),
            ("file:///etc/passwd", ""),
            ("chrome://settings", ""),
            ("example.com/x", "https://example.com/x"),
            ("localhost:3000", "http://localhost:3000"),
            ("192.168.1.2:8080/a", "http://192.168.1.2:8080/a"),
            ("hello world", "https://www.google.com/search?q=hello+world"),
            ("a&b=c/d", "https://www.google.com/search?q=a%26b%3Dc%2Fd"),
        ] {
            assert_eq!(normalize_url(raw), want, "normalize_url({raw:?})");
        }
    }

    #[test]
    fn mods_follow_the_server_os() {
        let want = if cfg!(target_os = "macos") { 4 } else { 2 };
        assert_eq!(fix_mods(MOD_META), want);
        assert_eq!(fix_mods(MOD_META | MOD_SHIFT), want | MOD_SHIFT);
    }

    #[test]
    fn chrome_flags_as_chromedp_would_pass_them() {
        let opt = BrowserOptions {
            width: 1280,
            height: 720,
            proxy: "socks5://127.0.0.1:1".into(),
            engine: Engine {
                name: "chrome".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let a = chrome_flags(&opt).args();
        assert_eq!(a[0], "--no-first-run");
        assert!(a.contains(&"--headless=new".to_string()));
        assert!(a.iter().any(|x| x == "--disable-extensions"));
        let enabled = chrome_flags(&BrowserOptions {
            enable_extensions: true,
            ..Default::default()
        })
        .args();
        assert!(!enabled.iter().any(|x| x == "--disable-extensions"));
        assert!(!a
            .iter()
            .any(|x| x == "--headless" || x.starts_with("--enable-automation")));
        assert!(a.contains(&"--window-size=1280,720".to_string()));
        assert!(a.contains(&"--disable-blink-features=AutomationControlled".to_string()));
        assert!(a.contains(&"--proxy-server=socks5://127.0.0.1:1".to_string()));
        assert!(a.contains(&"--disable-features=Translate,BlinkGenPropertyTrees,SpareRendererForSitePerProcess,OptimizationHints,MediaRouter,DialMediaRouteProvider,IsolateOrigins,site-per-process,ChromeWhatsNewUI".to_string()));
        // a flag set again keeps its first place, and appears once
        assert_eq!(
            a.iter().filter(|x| x.starts_with("--mute-audio")).count(),
            1
        );
        let shell = BrowserOptions {
            engine: Engine {
                shell: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let a = chrome_flags(&shell).args();
        assert!(
            a.contains(&"--headless".to_string())
                && !a.iter().any(|x| x.starts_with("--headless="))
        );
        assert!(!a.iter().any(|x| x.starts_with("--proxy-server")));
    }

    #[test]
    fn audio_and_meta_scripts_name_their_bindings() {
        assert!(AUDIO_JS.contains("window.__tbAudioOut;"));
        assert!(META_JS.contains("window.__tbMetaOut;"));
    }

    #[test]
    fn tab_info_json_is_omitempty() {
        let t = TabInfo {
            id: "A".into(),
            title: "t".into(),
            url: "u".into(),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&t).unwrap(),
            r#"{"id":"A","title":"t","url":"u"}"#
        );
    }
}
