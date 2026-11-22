//! Inspect: Chrome DevTools for the tabs of the shared browser, inside the viewer.
//!
//! Chrome already serves the DevTools front end and a WebSocket per tab on its debugging port, but
//! that port only listens on this machine and has no password. So the viewer never talks to it: it
//! goes through /devtools/ here, which sits behind the same login as everything else, and only lets
//! through what the front end needs (its own files and the sockets of the tabs we own). The
//! browser-wide socket (…/devtools/browser/…) is refused: it could open or close any tab.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{header, HeaderMap, HeaderName, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message as TMessage;

use crate::browser::Browser;

/// `http.Error`.
fn http_error(msg: &str, code: StatusCode) -> Response {
    Response::builder()
        .status(code)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header("x-content-type-options", "nosniff")
        .body(Body::from(format!("{msg}\n")))
        .expect("static response")
}

/// `json.NewEncoder(w).Encode(v)`
fn json_body(v: serde_json::Value) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!("{v}\n")))
        .expect("static response")
}

impl Browser {
    /// debugAddr is host:port of Chrome's debugging server. Chrome picks the port itself and writes
    /// it to DevToolsActivePort in the profile folder, so read it every time (a restart changes it).
    pub fn debug_addr(&self) -> Result<String, String> {
        if self.opt.no_dev_tools {
            return Err("DevTools is turned off on this server (-no-devtools)".into());
        }
        let raw = std::fs::read_to_string(Path::new(&self.opt.profile).join("DevToolsActivePort"))
            .map_err(|_| "the browser has not published its debugging port".to_string())?;
        let port = raw.trim().split('\n').next().unwrap_or_default().trim();
        match port.parse::<i64>() {
            Ok(n) if n > 0 => Ok(format!("127.0.0.1:{port}")),
            _ => Err("the browser's debugging port is unreadable".into()),
        }
    }

    /// ownsTab reports whether id is one of the shared browser's tabs.
    pub fn owns_tab(&self, id: &str) -> bool {
        self.st().tabs.iter().any(|t| t.id == id)
    }

    /// devtoolsOK says whether this browser can show DevTools: its debugging server answers and it
    /// has the front end (a slimmed-down chrome-headless-shell does not).
    pub async fn devtools_ok(&self) -> Result<(), String> {
        let addr = self.debug_addr()?;
        let c = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .map_err(|e| e.to_string())?;
        let resp = c
            .get(format!("http://{addr}/devtools/inspector.html"))
            .send()
            .await
            .map_err(|_| "the browser's debugging server does not answer".to_string())?;
        if resp.status() != StatusCode::OK {
            return Err(format!(
                "this browser ({}) has no built-in DevTools; use -engine chrome",
                self.opt.engine.label
            ));
        }
        Ok(())
    }
}

/// registerDevtoolsAPI: `/api/devtools` and `/devtools/…` (`b` is `h.browser`, None without a browser).
pub fn routes(b: Option<Arc<Browser>>) -> Router {
    Router::new()
        .route("/api/devtools", any(api_devtools))
        .route("/devtools/", any(proxy))
        .route("/devtools/{*rest}", any(proxy))
        .with_state(b)
}

/// GET /api/devtools -> { ok: true } or { ok: false, error: "…" }
async fn api_devtools(State(b): State<Option<Arc<Browser>>>) -> Response {
    let Some(b) = b else {
        return json_body(json!({"ok": false, "error": "no browser is running"}));
    };
    match b.devtools_ok().await {
        Err(e) => json_body(json!({"ok": false, "error": e})),
        Ok(()) => json_body(json!({"ok": true})),
    }
}

/// `url.Parse(origin).Host`
fn origin_host(o: &str) -> &str {
    let rest = o.split_once("://").map_or("", |(_, r)| r);
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let auth = &rest[..end];
    auth.rsplit_once('@').map_or(auth, |(_, h)| h)
}

/// `url.URL.Path`: the percent-decoded path.
fn decode_path(p: &str) -> String {
    let b = p.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or_default();
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "proxy-connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// /devtools/… -> Chrome's debugging server (the front end's files, and a socket per tab).
async fn proxy(State(b): State<Option<Arc<Browser>>>, req: Request) -> Response {
    let Some(b) = b else {
        return http_error("no browser is running", StatusCode::SERVICE_UNAVAILABLE);
    };
    let (mut parts, _body) = req.into_parts();
    let path = decode_path(parts.uri.path());
    // Only the front end's files and the sockets of our own tabs.
    if let Some(rest) = path.strip_prefix("/devtools/page/") {
        if !b.owns_tab(rest) {
            return http_error("not a tab of the shared browser", StatusCode::FORBIDDEN);
        }
    } else if path.starts_with("/devtools/browser") || path.contains("..") {
        return http_error("forbidden", StatusCode::FORBIDDEN);
    }
    if parts.method != Method::GET && parts.method != Method::HEAD {
        return http_error("GET only", StatusCode::METHOD_NOT_ALLOWED);
    }
    // A page on another site must not be able to open a socket to a tab through the visitor's
    // browser (nothing else guards it when the room has no password): same origin only.
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::to_string)
        .or_else(|| parts.uri.authority().map(|a| a.as_str().to_string()))
        .unwrap_or_default();
    if let Some(o) = parts.headers.get(header::ORIGIN) {
        let o = o.to_str().unwrap_or_default();
        if !o.is_empty() && origin_host(o) != host {
            return http_error(
                "cross-origin access to DevTools is not allowed",
                StatusCode::FORBIDDEN,
            );
        }
    }
    let addr = match b.debug_addr() {
        Ok(a) => a,
        Err(e) => return http_error(&e, StatusCode::SERVICE_UNAVAILABLE),
    };
    let pq = parts.uri.path_and_query().map_or("/", |p| p.as_str());

    let is_ws = parts
        .headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if is_ws {
        // Chrome only talks to callers that look local: no Origin, no cookies (the room's login is not for Chrome)
        let url = format!("ws://{addr}{pq}");
        let up = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(u) => u,
            Err(e) => return e.into_response(),
        };
        let chrome = match tokio_tungstenite::connect_async(url).await {
            Ok((c, _)) => c,
            Err(e) => {
                return http_error(
                    &format!("DevTools is not reachable: {e}"),
                    StatusCode::BAD_GATEWAY,
                )
            }
        };
        return up.on_upgrade(move |sock| bridge(sock, chrome));
    }

    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return http_error(
                &format!("DevTools is not reachable: {e}"),
                StatusCode::BAD_GATEWAY,
            )
        }
    };
    let mut hdrs = HeaderMap::new();
    for (k, v) in parts.headers.iter() {
        // Chrome only talks to callers that look local: it refuses other Host and Origin values.
        if is_hop(k)
            || [
                header::HOST,
                header::ORIGIN,
                header::COOKIE,
                header::AUTHORIZATION,
            ]
            .contains(k)
        {
            continue;
        }
        hdrs.append(k.clone(), v.clone());
    }
    let res = client
        .request(parts.method.clone(), format!("http://{addr}{pq}"))
        .headers(hdrs)
        .send()
        .await;
    let res = match res {
        Ok(r) => r,
        Err(e) => {
            return http_error(
                &format!("DevTools is not reachable: {e}"),
                StatusCode::BAD_GATEWAY,
            )
        }
    };
    let mut out = Response::builder().status(res.status());
    for (k, v) in res.headers() {
        if !is_hop(k) {
            out = out.header(k, v.clone());
        }
    }
    out.body(Body::from_stream(res.bytes_stream()))
        .unwrap_or_else(|e| {
            http_error(
                &format!("DevTools is not reachable: {e}"),
                StatusCode::BAD_GATEWAY,
            )
        })
}

type ChromeWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Pipes the viewer's socket to the tab's socket until either side ends.
async fn bridge(client: WebSocket, chrome: ChromeWs) {
    let (mut c_tx, mut c_rx) = client.split();
    let (mut k_tx, mut k_rx) = chrome.split();
    {
        let up = async {
            while let Some(Ok(m)) = c_rx.next().await {
                let m = match m {
                    Message::Text(t) => TMessage::text(t.as_str()),
                    Message::Binary(b) => TMessage::binary(b),
                    Message::Ping(b) => TMessage::Ping(b),
                    Message::Pong(b) => TMessage::Pong(b),
                    Message::Close(_) => break,
                };
                if k_tx.send(m).await.is_err() {
                    break;
                }
            }
            let _ = k_tx.close().await;
        };
        let down = async {
            while let Some(Ok(m)) = k_rx.next().await {
                let m = match m {
                    TMessage::Text(t) => Message::Text(t.as_str().into()),
                    TMessage::Binary(b) => Message::Binary(b),
                    TMessage::Ping(b) => Message::Ping(b),
                    TMessage::Pong(b) => Message::Pong(b),
                    TMessage::Close(_) => break,
                    TMessage::Frame(_) => continue,
                };
                if c_tx.send(m).await.is_err() {
                    break;
                }
            }
            let _ = c_tx.close().await;
        };
        tokio::select! { _ = up => {}, _ = down => {} }
    }
    // The side that ended first was closed above; the other one still needs its close frame
    // (a viewer that just vanished the rest of the way gets a plain close instead of 1006).
    let _ = c_tx.close().await;
    let _ = k_tx.close().await;
}
