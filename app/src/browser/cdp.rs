//! Chrome DevTools Protocol transport. It provides the parts that browser.rs leans on (the exec allocator that starts Chrome, the pipe/websocket connection,
//! flat target sessions, per-session event listeners), because chromiumoxide's launcher can neither
//! run Chrome over `--remote-debugging-pipe` (keeps the debugging port off with -no-devtools) nor
//! take a `pre_exec` hook (Pdeathsig, see chromecmd.rs), and its handler would second-guess the tab
//! bookkeeping that browser.rs does itself. Command and event payloads stay chromiumoxide_cdp-shaped
//! JSON; browser.rs builds them with `serde_json::json!` so "omitted" and "zero" stay exact.
//!
//! Shape:
//!   * `Conn`: one connection to Chrome. `call(session, method, params)` sends a command and waits for
//!     its answer; `subscribe(session)` listens to browser-level events (None) or to one session (Some).
//!   * `Chrome`: the Chrome process plus its `Conn` (its root context: an error on it
//!     becomes `Chrome::is_dead`).
//!   * Every event of a session reaches its listeners in order, from one reader task, as `Arc<Event>`;
//!     listeners must not block (browser.rs forwards them to a per-tab task).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

/// Why a command did not produce a result. `Display` keeps the wording that browser.rs matches on
/// ("no browser is open", "already active").
#[derive(Debug, Clone, PartialEq)]
pub enum CdpError {
    /// Chrome answered with an error object
    Remote { code: i64, message: String },
    /// `context deadline exceeded`
    Timeout,
    /// `context canceled` (the tab was closed or replaced)
    Canceled,
    /// the connection to Chrome is gone
    Closed,
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CdpError::Remote { code, message } => write!(f, "{message} ({code})"),
            CdpError::Timeout => f.write_str("context deadline exceeded"),
            CdpError::Canceled => f.write_str("context canceled"),
            CdpError::Closed => f.write_str("connection closed"),
        }
    }
}

impl std::error::Error for CdpError {}

pub type CdpResult<T = Value> = Result<T, CdpError>;

/// One CDP event.
#[derive(Debug, Clone)]
pub struct Event {
    pub method: String,
    pub params: Value,
    /// flat session id; None = the browser itself
    pub session: Option<String>,
}

impl Event {
    /// `serde_json::from_value` of the params into one of chromiumoxide_cdp's event types.
    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_value(self.params.clone()).ok()
    }
}

type Reply = oneshot::Sender<CdpResult>;
type Subs = Vec<(Option<String>, mpsc::UnboundedSender<Arc<Event>>)>;

pub struct Conn {
    next_id: AtomicU64,
    out: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<u64, Reply>>,
    subs: Mutex<Subs>,
    closed: watch::Sender<bool>,
    /// methods the engine on the other end does not have: they fail at once (see `OBSCURA_UNSUPPORTED`)
    unsupported: &'static [&'static str],
}

/// Commands this program sends that Obscura does not implement. They are answered here with a clear
/// error instead of being sent, so a server that ignores unknown methods can never leave one waiting.
pub const OBSCURA_UNSUPPORTED: &[&str] = &[
    "Emulation.setFocusEmulationEnabled",
    "Emulation.setTouchEmulationEnabled",
    "Emulation.setLocaleOverride",
    "Emulation.setTimezoneOverride",
    "Emulation.setGeolocationOverride",
    "Emulation.clearGeolocationOverride",
    "Browser.setPermission",
    "Browser.setDownloadBehavior",
    "Log.enable",
    "Log.disable",
];

/// Waits until a watch flag is true (the `Ref` guard of `wait_for` is not `Send`, so it is dropped here).
pub async fn flagged(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|v| *v).await;
}

fn lk<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Conn {
    fn new(unsupported: &'static [&'static str]) -> (Arc<Conn>, mpsc::UnboundedReceiver<String>) {
        let (out, out_rx) = mpsc::unbounded_channel();
        let (closed, _) = watch::channel(false);
        (
            Arc::new(Conn {
                next_id: AtomicU64::new(1),
                out,
                pending: Mutex::new(HashMap::new()),
                subs: Mutex::new(Vec::new()),
                closed,
                unsupported,
            }),
            out_rx,
        )
    }

    /// A connection over two byte streams with NUL-terminated JSON messages (`--remote-debugging-pipe`).
    pub fn over_pipe<R, W>(r: R, mut w: W) -> Arc<Conn>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (c, mut out_rx) = Conn::new(&[]);
        let rc = c.clone();
        tokio::spawn(async move {
            let mut r = BufReader::new(r);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(0, &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if buf.last() == Some(&0) {
                            buf.pop();
                        }
                        rc.dispatch(&buf);
                    }
                }
            }
            rc.close();
        });
        let wc = c.clone();
        tokio::spawn(async move {
            let mut closed = wc.closed.subscribe();
            loop {
                tokio::select! {
                    m = out_rx.recv() => {
                        let Some(mut m) = m else { break };
                        m.push('\0');
                        if w.write_all(m.as_bytes()).await.is_err() || w.flush().await.is_err() {
                            break;
                        }
                    }
                    _ = flagged(&mut closed) => break,
                }
            }
            wc.close();
        });
        c
    }

    /// A connection over a DevTools websocket (used where Chrome cannot be given a pipe).
    #[cfg_attr(unix, allow(dead_code))]
    pub async fn over_ws(url: &str) -> Result<Arc<Conn>, String> {
        Conn::over_ws_with(url, &[]).await
    }

    /// `over_ws` for an engine that lacks some commands (they fail at once, see `call`).
    pub async fn over_ws_with(
        url: &str,
        unsupported: &'static [&'static str],
    ) -> Result<Arc<Conn>, String> {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| format!("could not dial {url:?}: {e}"))?;
        let (mut tx, mut rx) = ws.split();
        let (c, mut out_rx) = Conn::new(unsupported);
        let rc = c.clone();
        tokio::spawn(async move {
            while let Some(Ok(m)) = rx.next().await {
                match m {
                    Message::Text(t) => rc.dispatch(t.as_bytes()),
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            rc.close();
        });
        let wc = c.clone();
        tokio::spawn(async move {
            let mut closed = wc.closed.subscribe();
            loop {
                tokio::select! {
                    m = out_rx.recv() => {
                        let Some(m) = m else { break };
                        if tx.send(Message::text(m)).await.is_err() {
                            break;
                        }
                    }
                    _ = flagged(&mut closed) => break,
                }
            }
            wc.close();
        });
        Ok(c)
    }

    fn dispatch(&self, raw: &[u8]) {
        let Ok(v) = serde_json::from_slice::<Value>(raw) else {
            return;
        };
        if let Some(id) = v.get("id").and_then(Value::as_u64) {
            let reply = lk(&self.pending).remove(&id);
            if let Some(reply) = reply {
                let res = match v.get("error") {
                    Some(e) => Err(CdpError::Remote {
                        code: e.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: e
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    }),
                    None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = reply.send(res);
            }
            return;
        }
        let Some(method) = v.get("method").and_then(Value::as_str) else {
            return;
        };
        let ev = Arc::new(Event {
            method: method.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
            session: v
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
        lk(&self.subs).retain(|(s, tx)| {
            if *s != ev.session {
                return true;
            }
            tx.send(ev.clone()).is_ok()
        });
    }

    /// Marks the connection closed: pending commands fail, listeners end.
    fn close(&self) {
        self.closed.send_replace(true);
        let pending: Vec<_> = lk(&self.pending).drain().collect();
        for (_, r) in pending {
            let _ = r.send(Err(CdpError::Closed));
        }
        lk(&self.subs).clear();
    }

    pub fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    /// Sends a command nobody waits for (no answer is expected). Safe from any thread, never blocks.
    pub fn send_nowait(&self, method: &str, params: Value) {
        if self.unsupported.contains(&method) {
            return;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let _ = self
            .out
            .send(json!({"id": id, "method": method, "params": params}).to_string());
    }

    /// Events of one session (`None` = browser-level events), in order, until the session or the
    /// connection ends (the receiver is dropped to unsubscribe).
    pub fn subscribe(&self, session: Option<&str>) -> mpsc::UnboundedReceiver<Arc<Event>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut subs = lk(&self.subs);
        if !self.is_closed() {
            subs.push((session.map(str::to_string), tx));
        }
        rx
    }

    /// Sends one command and waits for the answer (no timeout of its own: see `call_with`).
    pub async fn call(&self, session: Option<&str>, method: &str, params: Value) -> CdpResult {
        if self.unsupported.contains(&method) {
            return Err(CdpError::Remote {
                code: -32601,
                message: format!("{method} is not supported by this browser engine"),
            });
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lk(&self.pending).insert(id, tx);
        if self.is_closed() {
            lk(&self.pending).remove(&id);
            return Err(CdpError::Closed);
        }
        struct Guard<'a>(&'a Conn, u64);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                lk(&self.0.pending).remove(&self.1);
            }
        }
        let _g = Guard(self, id);
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        if self.out.send(msg.to_string()).is_err() {
            return Err(CdpError::Closed);
        }
        match rx.await {
            Ok(r) => r,
            Err(_) => Err(CdpError::Closed),
        }
    }

    /// `call` bounded by a timeout and by a cancel flag (a tab's context: set when the tab is
    /// closed, which ends every command still waiting on it).
    pub async fn call_with(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
        cancel: Option<watch::Receiver<bool>>,
    ) -> CdpResult {
        if let Some(c) = &cancel {
            if *c.borrow() {
                return Err(CdpError::Canceled);
            }
        }
        let fut = self.call(session, method, params);
        let fut = async {
            match timeout {
                Some(d) => tokio::time::timeout(d, fut)
                    .await
                    .unwrap_or(Err(CdpError::Timeout)),
                None => fut.await,
            }
        };
        match cancel {
            Some(mut c) => tokio::select! {
                r = fut => r,
                _ = flagged(&mut c) => Err(CdpError::Canceled),
            },
            None => fut.await,
        }
    }

    pub async fn closed(&self) {
        let mut c = self.closed.subscribe();
        flagged(&mut c).await;
    }
}

// ---------- Flags ----------

/// Allocator flags: a map that keeps the place of a flag set again (`set_flag`).
#[derive(Debug, Clone, Default)]
pub struct Flags(Vec<(String, FlagVal)>);

#[derive(Debug, Clone, PartialEq)]
pub enum FlagVal {
    Bool(bool),
    Str(String),
}

impl Flags {
    pub fn set(&mut self, name: &str, v: FlagVal) {
        match self.0.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = v,
            None => self.0.push((name.to_string(), v)),
        }
    }
    pub fn bool(&mut self, name: &str, v: bool) {
        self.set(name, FlagVal::Bool(v));
    }
    pub fn str(&mut self, name: &str, v: &str) {
        self.set(name, FlagVal::Str(v.to_string()));
    }
    pub fn get(&self, name: &str) -> Option<&FlagVal> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
    /// `flagArgs`: `--name=value` for strings, `--name` for true, nothing for false.
    pub fn args(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (n, v) in &self.0 {
            match v {
                FlagVal::Str(s) => out.push(format!("--{n}={s}")),
                FlagVal::Bool(true) => out.push(format!("--{n}")),
                FlagVal::Bool(false) => {}
            }
        }
        out
    }
}

// ---------- The Chrome process ----------

pub struct Chrome {
    pub conn: Arc<Conn>,
    exited: watch::Receiver<bool>,
    kill: mpsc::UnboundedSender<()>,
    /// Obscura: it starts with no tab, and lacks some commands
    pub obscura: bool,
    /// OS pid of the browser process (None for an Obscura reached over `-cdp-url`)
    pub pid: Option<u32>,
}

pub struct LaunchSpec {
    pub exec_path: String,
    pub flags: Flags,
    /// start Obscura (`exec_path serve --port …`), or connect to `cdp_url`, instead of Chrome
    pub obscura: bool,
    /// Obscura that is already running (`ws://…` or `http://host:port`); no process is started
    pub cdp_url: String,
    /// `--remote-debugging-port=0` next to the pipe (Inspect in the viewer needs a port)
    pub debug_port: bool,
}

impl Chrome {
    /// Chrome exited or the connection dropped.
    pub fn is_dead(&self) -> bool {
        self.conn.is_closed() || *self.exited.borrow()
    }

    /// `cancelAlloc`: kills the process if it is still there (returns at once).
    pub fn kill(&self) {
        let _ = self.kill.send(());
    }

    /// Close Chrome gracefully (Browser.close), then make sure it is gone.
    /// Callers bound it with their own timeout and call `kill` after.
    pub async fn close_gracefully(&self) {
        let _ = self.conn.call(None, "Browser.close", json!({})).await;
        let mut e = self.exited.clone();
        flagged(&mut e).await;
    }

    /// the process has exited
    pub fn exited(&self) -> bool {
        *self.exited.borrow()
    }

    pub async fn wait_exit(&self) {
        let mut e = self.exited.clone();
        flagged(&mut e).await;
    }
}

/// Starts Chrome: flags in order, `--no-sandbox` as root,
/// `--remote-debugging-pipe` (unix) and the blank start page; a throwaway profile when the flags name none.
pub async fn launch(spec: LaunchSpec) -> Result<Arc<Chrome>, String> {
    if spec.obscura {
        return launch_obscura(&spec).await;
    }
    let mut args = spec.flags.args();
    let mut temp_dir = None;
    if spec.flags.get("user-data-dir").is_none() {
        let d = std::env::temp_dir().join(format!("chromedp-runner{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
        args.push(format!("--user-data-dir={}", d.display()));
        temp_dir = Some(d);
    }
    #[cfg(unix)]
    if spec.flags.get("no-sandbox").is_none() && unsafe { libc::geteuid() } == 0 {
        // We run as root, for example in a Linux container. Chrome needs --no-sandbox as root.
        args.push("--no-sandbox".into());
    }
    let r = launch_inner(&spec, args, temp_dir.clone()).await;
    if r.is_err() {
        if let Some(d) = temp_dir {
            let _ = std::fs::remove_dir_all(d);
        }
    }
    r
}

#[cfg(unix)]
async fn launch_inner(
    spec: &LaunchSpec,
    mut args: Vec<String>,
    temp_dir: Option<std::path::PathBuf>,
) -> Result<Arc<Chrome>, String> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::io::RawFd;
    use tokio::net::unix::pipe;

    fn pipe_pair() -> std::io::Result<(OwnedFd, OwnedFd)> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: pipe(2) fills the two descriptors, which we take ownership of at once.
        unsafe {
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            for fd in fds {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])))
        }
    }
    /// A copy of fd at 5 or above (so dup2 onto 3 and 4 in the child cannot collide), close-on-exec.
    fn high_dup(fd: &OwnedFd) -> std::io::Result<OwnedFd> {
        use std::os::fd::AsRawFd;
        // SAFETY: F_DUPFD_CLOEXEC returns a new descriptor we own.
        unsafe {
            let n = libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 5);
            if n < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(OwnedFd::from_raw_fd(n))
        }
    }

    let (cmd_r, cmd_w) = pipe_pair().map_err(|e| e.to_string())?; // we write commands, Chrome reads fd 3
    let (resp_r, resp_w) = pipe_pair().map_err(|e| e.to_string())?; // Chrome writes answers to fd 4
    let (child_r, child_w) = (
        high_dup(&cmd_r).map_err(|e| e.to_string())?,
        high_dup(&resp_w).map_err(|e| e.to_string())?,
    );
    drop(cmd_r);
    drop(resp_w);

    args.push("--remote-debugging-pipe".into());
    // Force the first page to be blank, and not the welcome page. --no-first-run does not do that.
    args.push("about:blank".into());
    if spec.debug_port {
        args.push("--remote-debugging-port=0".into());
    }
    let mut cmd = tokio::process::Command::new(&spec.exec_path);
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    crate::chromecmd::chrome_cmd_options(&mut cmd);
    {
        use std::os::fd::AsRawFd;
        let (cr, cw) = (child_r.as_raw_fd(), child_w.as_raw_fd());
        // SAFETY: dup2 is async-signal-safe and allocates nothing.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(cr, 3) < 0 || libc::dup2(cw, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let child = cmd.spawn().map_err(|e| e.to_string())?;
    drop(child_r);
    drop(child_w);

    let w = pipe::Sender::from_owned_fd(cmd_w).map_err(|e| e.to_string())?;
    let r = pipe::Receiver::from_owned_fd(resp_r).map_err(|e| e.to_string())?;
    let conn = Conn::over_pipe(r, w);
    Ok(supervise(conn, child, temp_dir))
}

#[cfg(not(unix))]
async fn launch_inner(
    spec: &LaunchSpec,
    mut args: Vec<String>,
    temp_dir: Option<std::path::PathBuf>,
) -> Result<Arc<Chrome>, String> {
    // No pipe on this platform: Chrome prints "DevTools listening on ws://…" and we dial that.
    args.push("about:blank".into());
    args.push("--remote-debugging-port=0".into());
    let _ = spec.debug_port;
    let mut cmd = tokio::process::Command::new(&spec.exec_path);
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    crate::chromecmd::chrome_cmd_options(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let stderr = child.stderr.take().ok_or("no stderr")?;
    let mut lines = BufReader::new(stderr).lines();
    let url = tokio::time::timeout(Duration::from_secs(20), async {
        let mut seen = String::new();
        while let Ok(Some(l)) = lines.next_line().await {
            if let Some(i) = l.find("DevTools listening on ") {
                return Ok(l[i + "DevTools listening on ".len()..].trim().to_string());
            }
            seen.push_str(&l);
            seen.push('\n');
        }
        Err(format!(
            "chrome exited before it printed its address:\n{seen}"
        ))
    })
    .await
    .map_err(|_| "websocket url timeout reached".to_string())??;
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    let conn = Conn::over_ws(&url).await?;
    Ok(supervise(conn, child, temp_dir))
}

/// One task owns the process: it reports the exit, kills on request and removes a throwaway profile.
fn supervise(
    conn: Arc<Conn>,
    mut child: tokio::process::Child,
    temp_dir: Option<std::path::PathBuf>,
) -> Arc<Chrome> {
    let pid = child.id();
    let (exited_tx, exited) = watch::channel(false);
    let (kill, mut kill_rx) = mpsc::unbounded_channel::<()>();
    let c2 = conn.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = child.wait() => {}
            _ = kill_rx.recv() => { let _ = child.kill().await; }
        }
        exited_tx.send_replace(true);
        c2.close();
        if let Some(d) = temp_dir {
            // Child processes of Chrome can still write there for a moment.
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_millis(10)).await;
                if std::fs::remove_dir_all(&d).is_ok() {
                    break;
                }
            }
        }
    });
    Arc::new(Chrome {
        conn,
        exited,
        kill,
        obscura: false,
        pid,
    })
}

// ---------- Obscura ----------

/// The websocket address of a CDP server: `ws://…` as is; `http://host:port` is asked
/// for `/json/version` (`webSocketDebuggerUrl`).
pub async fn discover_ws_url(base: &str) -> Result<String, String> {
    if base.starts_with("ws://") || base.starts_with("wss://") {
        return Ok(base.to_string());
    }
    let url = format!("{}/json/version", base.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    let v: Value = client
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    v["webSocketDebuggerUrl"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{url} has no webSocketDebuggerUrl"))
}

async fn launch_obscura(spec: &LaunchSpec) -> Result<Arc<Chrome>, String> {
    if !spec.cdp_url.is_empty() {
        let ws = discover_ws_url(&spec.cdp_url)
            .await
            .map_err(|e| format!("could not reach Obscura at {}: {e}", spec.cdp_url))?;
        let conn = Conn::over_ws_with(&ws, OBSCURA_UNSUPPORTED).await?;
        return Ok(supervise_obscura(conn, None));
    }
    // a free local port: Obscura has no "pick one" mode
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map_err(|e| e.to_string())?
        .port();
    let mut cmd = tokio::process::Command::new(&spec.exec_path);
    cmd.arg("serve").arg("--port").arg(port.to_string());
    if let Some(FlagVal::Str(p)) = spec.flags.get("proxy-server") {
        if !p.is_empty() {
            cmd.arg("--proxy").arg(p);
        }
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    crate::chromecmd::chrome_cmd_options(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("{}: {e}", crate::engine::OBSCURA_INSTALL)
        } else {
            format!("could not start {}: {e}", spec.exec_path)
        }
    })?;
    let base = format!("http://127.0.0.1:{port}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let ws = loop {
        if let Ok(Some(st)) = child.try_wait() {
            return Err(format!("obscura exited before it was ready ({st})"));
        }
        match discover_ws_url(&base).await {
            Ok(u) => break u,
            Err(e) => {
                if tokio::time::Instant::now() >= deadline {
                    let _ = child.kill().await;
                    return Err(format!("obscura did not come up on port {port}: {e}"));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    match Conn::over_ws_with(&ws, OBSCURA_UNSUPPORTED).await {
        Ok(conn) => Ok(supervise_obscura(conn, Some(child))),
        Err(e) => {
            let _ = child.kill().await;
            Err(e)
        }
    }
}

/// Like `supervise` for Obscura; without a process (`-cdp-url`) killing only drops the connection.
fn supervise_obscura(conn: Arc<Conn>, child: Option<tokio::process::Child>) -> Arc<Chrome> {
    let pid = child.as_ref().and_then(|c| c.id());
    let (exited_tx, exited) = watch::channel(false);
    let (kill, mut kill_rx) = mpsc::unbounded_channel::<()>();
    let c2 = conn.clone();
    tokio::spawn(async move {
        match child {
            Some(mut child) => {
                tokio::select! {
                    _ = child.wait() => {}
                    _ = kill_rx.recv() => { let _ = child.kill().await; }
                }
            }
            None => {
                let mut closed = c2.closed.subscribe();
                tokio::select! {
                    _ = flagged(&mut closed) => {}
                    _ = kill_rx.recv() => {}
                }
            }
        }
        exited_tx.send_replace(true);
        c2.close();
    });
    Arc::new(Chrome {
        conn,
        exited,
        kill,
        obscura: true,
        pid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;

    /// A tiny CDP server: answers every command with `{"echo": method}`, and sends one event first.
    async fn serve_ws() -> String {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(s).await else {
                        return;
                    };
                    let ev = json!({"method": "Target.targetCreated", "params": {"n": 1}});
                    let _ = ws.send(Message::text(ev.to_string())).await;
                    while let Some(Ok(Message::Text(t))) = ws.next().await {
                        let v: Value = serde_json::from_str(t.as_str()).unwrap();
                        if v["method"] == "Test.hang" {
                            continue; // never answers
                        }
                        let r = json!({"id": v["id"], "result": {"echo": v["method"]}});
                        let _ = ws.send(Message::text(r.to_string())).await;
                    }
                });
            }
        });
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn ws_transport_calls_and_events() {
        let url = serve_ws().await;
        let conn = Conn::over_ws(&url).await.unwrap();
        let mut ev = conn.subscribe(None);
        let r = conn.call(None, "Browser.ping", json!({})).await.unwrap();
        assert_eq!(r["echo"], "Browser.ping");
        // a command nobody answers is bounded by its timeout, not hung
        let r = conn
            .call_with(
                None,
                "Test.hang",
                json!({}),
                Some(Duration::from_millis(100)),
                None,
            )
            .await;
        assert_eq!(r, Err(CdpError::Timeout));
        let e = tokio::time::timeout(Duration::from_secs(2), ev.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(e.method, "Target.targetCreated");
        assert!(!conn.is_closed());
    }

    #[tokio::test]
    async fn unsupported_commands_fail_fast_and_dead_server_closes() {
        let url = serve_ws().await;
        let conn = Conn::over_ws_with(&url, OBSCURA_UNSUPPORTED).await.unwrap();
        let r = conn.call(None, "Log.enable", json!({})).await;
        match r {
            Err(CdpError::Remote { code, message }) => {
                assert_eq!(code, -32601);
                assert!(message.contains("Log.enable"), "{message}");
            }
            o => panic!("{o:?}"),
        }
        // supported ones still go through
        assert!(conn.call(None, "Page.enable", json!({})).await.is_ok());
        assert!(Conn::over_ws("ws://127.0.0.1:1").await.is_err());
    }

    #[tokio::test]
    async fn discovers_ws_url_from_json_version() {
        // a one-shot HTTP server
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await;
                let body = format!(
                    r#"{{"webSocketDebuggerUrl":"ws://127.0.0.1:{port}/devtools/browser"}}"#
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        let u = discover_ws_url(&format!("http://127.0.0.1:{port}"))
            .await
            .unwrap();
        assert_eq!(u, format!("ws://127.0.0.1:{port}/devtools/browser"));
        assert_eq!(discover_ws_url("ws://x:1/y").await.unwrap(), "ws://x:1/y");
        assert!(discover_ws_url("http://127.0.0.1:1").await.is_err());
    }

    #[tokio::test]
    async fn cdp_url_launch_without_a_process() {
        let url = serve_ws().await;
        let c = launch(LaunchSpec {
            exec_path: String::new(),
            flags: Flags::default(),
            obscura: true,
            cdp_url: url,
            debug_port: false,
        })
        .await
        .unwrap();
        assert!(c.obscura && !c.is_dead());
        c.kill();
        tokio::time::timeout(Duration::from_secs(2), c.wait_exit())
            .await
            .unwrap();
        assert!(c.is_dead());
    }

    #[tokio::test]
    async fn missing_obscura_binary_says_how_to_install() {
        let e = launch(LaunchSpec {
            exec_path: "/nonexistent/obscura".into(),
            flags: Flags::default(),
            obscura: true,
            cdp_url: String::new(),
            debug_port: false,
        })
        .await
        .err()
        .unwrap();
        assert!(e.contains("releases"), "{e}");
    }
}
