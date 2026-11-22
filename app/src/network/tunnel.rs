//! Public tunnel (-tunnel): same idea as https://github.com/tuanngocptn/nport, built in.
//! We ask the nport backend for a Cloudflare tunnel (no account needed), then run the
//! cloudflared binary that is embedded in this program. No router/port forwarding needed.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::{auth_token, password, set_password, AUTH_COOKIE};
use crate::{is_false, APP_NAME};

/// The cloudflared binary, embedded at build time: scripts/fetch-cloudflared.sh puts the binary for
/// the build platform in app/cloudflared/.
#[derive(rust_embed::RustEmbed)]
#[folder = "cloudflared"]
pub struct CloudflaredBin;

/// tunnel.rs: `defaultTunnelBackend`
const DEFAULT_TUNNEL_BACKEND: &str = "https://api.nport.link";

/// tunnel.rs: `func randomHex(n int) string` (n random bytes, hex encoded).
fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::rng().fill(&mut b[..]);
    hex::encode(b)
}

/// tunnel.rs: `tunnelAPIResponse`
#[derive(Deserialize, Default, Debug)]
#[serde(rename_all = "camelCase", default)]
struct TunnelApiResponse {
    success: bool,
    error: String,
    tunnel_id: String,
    tunnel_token: String,
    url: String,
}

/// tunnel.rs: `os.UserCacheDir` (macOS ~/Library/Caches, Windows %LocalAppData%, else
/// $XDG_CACHE_HOME or ~/.cache). None where returns an error.
fn user_cache_dir() -> Option<PathBuf> {
    let non_empty = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(target_os = "macos") {
        non_empty("HOME").map(|h| h.join("Library/Caches"))
    } else if cfg!(windows) {
        non_empty("LocalAppData")
    } else {
        non_empty("XDG_CACHE_HOME").or_else(|| non_empty("HOME").map(|h| h.join(".cache")))
    }
}

/// exec.LookPath: the first executable `name` on PATH (`name.exe` on Windows).
fn look_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.exe")]
    } else {
        vec![name.to_string()]
    };
    for dir in std::env::split_paths(&path) {
        for n in &names {
            let p = dir.join(n);
            if is_executable(&p) {
                return Some(p);
            }
        }
    }
    None
}

fn is_executable(p: &Path) -> bool {
    let Ok(m) = fs::metadata(p) else {
        return false;
    };
    if !m.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// os.WriteFile(path, data, 0755)
fn write_executable(path: &Path, data: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    let mut f = {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut f = fs::File::create(path)?;
    f.write_all(data)
}

/// os.MkdirAll(dir, 0755)
fn mkdir_all(dir: &Path) -> std::io::Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o755);
    }
    b.create(dir)
}

/// tunnel.rs: `extractCloudflared`. Writes the embedded binary to the cache folder and returns
/// its path. Falls back to a cloudflared found in PATH when none was embedded.
fn extract_cloudflared() -> Result<PathBuf, String> {
    let name = if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    };
    let Some(file) = CloudflaredBin::get(name) else {
        if let Some(p) = look_path("cloudflared") {
            return Ok(p);
        }
        return Err("cloudflared is not embedded in this build: run scripts/fetch-cloudflared.sh and rebuild (or install cloudflared)".into());
    };
    let data = &file.data;
    let dir = user_cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_NAME);
    mkdir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{name}-{}", data.len()));
    if let Ok(st) = fs::metadata(&path) {
        if st.len() == data.len() as u64 {
            return Ok(path);
        }
    }
    let tmp = dir.join(format!("{name}-{}.tmp", data.len()));
    write_executable(&tmp, data).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(path)
}

/// tunnel.rs: the body of `tunnelRequest` after the HTTP call. Decodes the backend answer;
/// non-JSON answers and `>= 400` (or a POST with success=false) become errors.
fn parse_response(method: &Method, status: u16, raw: &[u8]) -> Result<TunnelApiResponse, String> {
    let r: TunnelApiResponse = serde_json::from_slice(raw).map_err(|_| {
        format!(
            "backend answered {status}: {}",
            String::from_utf8_lossy(raw).trim()
        )
    })?;
    if status >= 400 || (*method == Method::POST && !r.success) {
        let msg = if r.error.is_empty() {
            String::from_utf8_lossy(raw).into_owned()
        } else {
            r.error.clone()
        };
        return Err(format!("backend: {msg}"));
    }
    Ok(r)
}

/// tunnel.rs: `tunnelRequest(method, backend, body)`
async fn tunnel_request(
    method: Method,
    backend: &str,
    body: Value,
) -> Result<TunnelApiResponse, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .request(method.clone(), backend)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let raw = resp.bytes().await.unwrap_or_default();
    parse_response(&method, status, &raw)
}

/// tunnel.rs: the `strings.Contains(line, "Registered tunnel connection")` check.
fn is_registered_line(line: &str) -> bool {
    line.contains("Registered tunnel connection")
}

/// tunnel.rs: lines cloudflared prints on stderr that we pass on (errors, not retries).
fn is_report_line(line: &str) -> bool {
    line.contains("ERR") && !line.contains("retrying")
}

/// A live tunnel: the cloudflared process plus the backend record to delete on Close.
pub struct Tunnel {
    pub url: String,
    backend: String,
    subdomain: String,
    tunnel_id: String,
    child: Option<Child>,
}

impl Tunnel {
    /// tunnel.rs: `Close`. Stops cloudflared and deletes the tunnel from the backend.
    pub async fn close(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if !self.tunnel_id.is_empty() {
            let _ = tunnel_request(
                Method::DELETE,
                &self.backend,
                json!({"subdomain": self.subdomain, "tunnelId": self.tunnel_id}),
            )
            .await;
        }
    }
}

/// tunnel.rs: `startTunnel(port, subdomain, backend)`. Exposes localhost:port on a public https URL.
async fn start_tunnel(port: u16, subdomain: &str, backend: &str) -> Result<Tunnel, String> {
    let backend = if backend.is_empty() {
        DEFAULT_TUNNEL_BACKEND
    } else {
        backend
    };
    let subdomain = if subdomain.is_empty() {
        format!("abu-{}", random_hex(4))
    } else {
        subdomain.to_string()
    };
    let bin = extract_cloudflared()?;
    let r = tunnel_request(Method::POST, backend, json!({"subdomain": subdomain})).await?;
    let mut url = r.url.clone();
    if !url.contains("://") {
        url = format!("https://{url}");
    }
    let mut t = Tunnel {
        url,
        backend: backend.to_string(),
        subdomain,
        tunnel_id: r.tunnel_id.clone(),
        child: None,
    };

    let mut cmd = Command::new(&bin);
    cmd.args([
        "tunnel",
        "--no-autoupdate",
        "run",
        "--token",
        &r.tunnel_token,
        "--url",
        &format!("http://localhost:{port}"),
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            t.close().await;
            return Err(e.to_string());
        }
    };
    let stderr = child.stderr.take();
    t.child = Some(child);

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        let Some(stderr) = stderr else {
            return;
        };
        let mut tx = Some(tx);
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else {
                break;
            };
            if is_registered_line(&line) {
                if let Some(tx) = tx.take() {
                    let _ = tx.send(());
                }
            }
            if is_report_line(&line) {
                eprintln!("[cloudflared] {line}");
            }
        }
    });
    // a stderr that closes early still waits out the full 30s; a dropped sender must not end it sooner.
    let connected = tokio::time::timeout(Duration::from_secs(30), async {
        if rx.await.is_ok() {
            return;
        }
        std::future::pending::<()>().await
    })
    .await;
    if connected.is_err() {
        t.close().await;
        return Err("tunnel did not connect within 30s (check your internet connection)".into());
    }
    Ok(t)
}

// ---------- Switch on/off at runtime (the button in the web UI) ----------

/// tunnel.rs: `TunnelStatus` (JSON shape of GET /api/tunnel).
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TunnelStatus {
    pub on: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub busy: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// only when we generated it
    #[serde(skip_serializing_if = "String::is_empty")]
    pub password: String,
    /// we set the password for the tunnel
    #[serde(skip_serializing_if = "is_false")]
    pub generated: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    pub can_toggle: bool,
}

#[derive(Default)]
struct TunnelState {
    t: Option<Tunnel>,
    busy: bool,
    /// we created the password and clear it again on Stop
    generated: bool,
    last_err: String,
}

/// tunnel.rs: `TunnelManager`
pub struct TunnelManager {
    port: u16,
    backend: String,
    state: Mutex<TunnelState>,
}

fn lock(m: &Mutex<TunnelState>) -> MutexGuard<'_, TunnelState> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl TunnelManager {
    /// tunnel.rs: `&TunnelManager{port: ..., backend: ...}`
    pub fn new(port: u16, backend: String) -> Arc<Self> {
        Arc::new(TunnelManager {
            port,
            backend,
            state: Mutex::new(TunnelState::default()),
        })
    }

    /// tunnel.rs: `Status`
    pub fn status(&self) -> TunnelStatus {
        let g = lock(&self.state);
        let mut s = TunnelStatus {
            busy: g.busy,
            error: g.last_err.clone(),
            generated: g.generated,
            ..Default::default()
        };
        if let Some(t) = &g.t {
            s.on = true;
            s.url = t.url.clone();
            if g.generated {
                s.password = password();
            }
        }
        s
    }

    /// tunnel.rs: `Start(subdomain)`
    pub async fn start(&self, subdomain: &str) -> Result<(), String> {
        {
            let mut g = lock(&self.state);
            if g.t.is_some() || g.busy {
                return Ok(());
            }
            g.busy = true;
            g.last_err.clear();
        }

        // A public URL without a password would hand the browser to anyone.
        let mut gen = false;
        if password().is_empty() {
            set_password(&random_hex(8));
            gen = true;
        }
        let res = start_tunnel(self.port, subdomain, &self.backend).await;

        let mut g = lock(&self.state);
        g.busy = false;
        match res {
            Err(e) => {
                if gen {
                    set_password("");
                }
                g.last_err = e.clone();
                Err(e)
            }
            Ok(t) => {
                g.t = Some(t);
                g.generated = gen;
                Ok(())
            }
        }
    }

    /// tunnel.rs: `Stop`
    pub async fn stop(&self) {
        let (t, gen) = {
            let mut g = lock(&self.state);
            let t = g.t.take();
            let gen = g.generated;
            g.generated = false;
            g.last_err.clear();
            (t, gen)
        };
        let Some(mut t) = t else {
            return;
        };
        t.close().await;
        if gen {
            set_password("");
        }
    }
}

/// tunnel.rs: the `tunnels` global (set once in run).
static TUNNELS: OnceLock<Arc<TunnelManager>> = OnceLock::new();

/// tunnel.rs: `tunnels = tm`
pub fn set_global(m: Arc<TunnelManager>) {
    let _ = TUNNELS.set(m);
}

/// The manager set by `set_global`, if any.
pub fn global() -> Option<Arc<TunnelManager>> {
    TUNNELS.get().cloned()
}

/// tunnel.rs: `viaTunnel(r)`. Requests that came in through the tunnel carry Cloudflare headers.
fn via_tunnel(h: &HeaderMap) -> bool {
    ["cf-connecting-ip", "cf-ray", "x-forwarded-for"]
        .iter()
        .any(|k| h.get(*k).is_some_and(|v| !v.is_empty()))
}

/// registerTunnelAPI: GET /api/tunnel -> status; POST /api/tunnel {"on":true|false,"subdomain":"x"}
pub fn routes(m: Arc<TunnelManager>) -> Router {
    Router::new()
        .route("/api/tunnel", any(api_tunnel))
        .with_state(m)
}

async fn api_tunnel(State(m): State<Arc<TunnelManager>>, req: Request) -> Response {
    // Only people on this machine/network may switch the tunnel; someone who got the link cannot switch it off.
    let can = !via_tunnel(req.headers());
    let (parts, body) = req.into_parts();
    let mut set_cookie: Option<String> = None;
    if parts.method == Method::POST {
        if !can {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                .header("x-content-type-options", "nosniff")
                .body(Body::from(
                    "the tunnel can only be switched from the machine that runs the app\n",
                ))
                .expect("static response");
        }
        // json.NewDecoder(io.LimitReader(r.Body, 4096)).Decode(&req), error ignored
        let bytes = to_bytes(body, 4096).await.unwrap_or_default();
        let v = serde_json::Deserializer::from_slice(&bytes)
            .into_iter::<Value>()
            .next()
            .and_then(Result::ok);
        let on = v
            .as_ref()
            .and_then(|v| v.get("on"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if on {
            let subdomain = v
                .as_ref()
                .and_then(|v| v.get("subdomain"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let _ = m.start(&subdomain.trim().to_lowercase()).await;
            // we may just have set a password: keep this browser logged in
            let pw = password();
            if !pw.is_empty() {
                set_cookie = Some(format!(
                    "{AUTH_COOKIE}={}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax",
                    auth_token(&pw),
                    30 * 24 * 3600
                ));
            }
        } else {
            m.stop().await;
        }
    }
    let mut s = m.status();
    s.can_toggle = can;
    if !can {
        s.password.clear();
    }
    let mut b = Response::builder().header(header::CONTENT_TYPE, "application/json");
    if let Some(c) = set_cookie {
        b = b.header(header::SET_COOKIE, c);
    }
    b.body(Body::from(format!(
        "{}\n",
        serde_json::to_string(&s).unwrap_or_default()
    )))
    .expect("static response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connected_line_is_detected() {
        assert!(is_registered_line(
            "2026-01-01T00:00:00Z INF Registered tunnel connection connIndex=0"
        ));
        assert!(!is_registered_line("INF Starting tunnel"));
    }

    #[test]
    fn only_errors_that_are_not_retries_are_reported() {
        assert!(is_report_line("ERR failed to dial origin"));
        assert!(!is_report_line("ERR connection failed, retrying in 1s"));
        assert!(!is_report_line("INF Requesting new quick Tunnel"));
    }

    #[test]
    fn backend_answer_is_parsed() {
        let ok = parse_response(
            &Method::POST,
            200,
            br#"{"success":true,"tunnelId":"abc","tunnelToken":"tok","url":"x.nport.link"}"#,
        )
        .unwrap();
        assert_eq!(ok.tunnel_id, "abc");
        assert_eq!(ok.tunnel_token, "tok");
        assert_eq!(ok.url, "x.nport.link");
    }

    #[test]
    fn backend_errors_are_reported() {
        let e = parse_response(&Method::POST, 200, br#"{"success":false,"error":"taken"}"#)
            .unwrap_err();
        assert_eq!(e, "backend: taken");
        let e = parse_response(&Method::POST, 500, b"{\"error\":\"boom\"}").unwrap_err();
        assert_eq!(e, "backend: boom");
        let e = parse_response(&Method::POST, 502, b"<html>").unwrap_err();
        assert_eq!(e, "backend answered 502: <html>");
    }

    #[test]
    fn idle_status_json_shape() {
        let s = TunnelStatus {
            can_toggle: true,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"on":false,"canToggle":true}"#
        );
    }

    #[test]
    fn random_hex_has_double_length() {
        let h = random_hex(4);
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
