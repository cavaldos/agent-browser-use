//! Keep-alive: while it is on, the shared browser moves the mouse and scrolls a little every
//! so often, like a person sitting at the page, so sites that stop on inactivity
//! (Google Colab, dashboards, ...) keep the session. Off by default; it runs on the
//! server, so it keeps going when nobody has the web page open.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use rand::Rng;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::browser::{BResult, Browser, Tab, View, INPUT_TIMEOUT};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
struct KaState {
    on: bool,
    /// dropped (or set) to end the loop
    stop: Option<watch::Sender<bool>>,
    last: Option<SystemTime>,
}

pub struct KeepAlive {
    b: Option<Arc<Browser>>,
    mu: Mutex<KaState>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct KeepAliveStatus {
    pub on: bool,
    pub available: bool,
    #[serde(rename = "canToggle")]
    pub can_toggle: bool,
    /// unix ms of the last simulated activity
    #[serde(skip_serializing_if = "is_zero")]
    pub last: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

impl KeepAlive {
    pub fn new(b: Option<Arc<Browser>>) -> Arc<KeepAlive> {
        Arc::new(KeepAlive {
            b,
            mu: Mutex::new(KaState::default()),
        })
    }

    pub fn status(&self) -> KeepAliveStatus {
        let k = lock(&self.mu);
        KeepAliveStatus {
            on: k.on,
            available: self.b.is_some(),
            can_toggle: false,
            last: k.last.map_or(0, |t| {
                t.duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as i64)
            }),
        }
    }

    pub fn set(self: &Arc<Self>, on: bool) {
        if self.b.is_none() {
            return;
        }
        let mut k = lock(&self.mu);
        if on == k.on {
            return;
        }
        k.on = on;
        if !on {
            // close(k.stop)
            if let Some(stop) = k.stop.take() {
                let _ = stop.send(true);
            }
            return;
        }
        let (tx, rx) = watch::channel(false);
        k.stop = Some(tx);
        tokio::spawn(self.clone().run_loop(rx));
    }

    async fn run_loop(self: Arc<Self>, mut stop: watch::Receiver<bool>) {
        loop {
            // Colab goes idle after a few minutes, so every 20-45 s is plenty.
            let wait = Duration::from_secs(20 + rand::rng().random_range(0..25));
            tokio::select! {
                _ = stop.changed() => return,
                _ = tokio::time::sleep(wait) => {}
            }
            self.tick().await;
        }
    }

    /// tick nudges every open tab once. A tab that is loading or busy is skipped.
    async fn tick(&self) {
        let Some(b) = &self.b else { return };
        let (ids, v) = {
            let st = b.st();
            (
                st.tabs.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
                st.view,
            )
        };
        if v.width <= 0 || v.height <= 0 {
            return;
        }
        for id in ids {
            if b.busy_tab(&id) {
                continue;
            }
            let _ = b
                .run_tab(&id, |t| async move { human_nudge(&t, v).await })
                .await;
        }
        lock(&self.mu).last = Some(SystemTime::now());
    }
}

async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// humanNudge: a short curved mouse path, and often a small scroll down and back up.
async fn human_nudge(t: &Arc<Tab>, v: View) -> BResult<()> {
    let (w, h) = (v.width as f64, v.height as f64);
    let (x, y, tx, ty) = {
        let mut r = rand::rng();
        (
            w * (0.3 + r.random::<f64>() * 0.4),
            h * (0.3 + r.random::<f64>() * 0.4),
            w * (0.2 + r.random::<f64>() * 0.6),
            h * (0.2 + r.random::<f64>() * 0.6),
        )
    };
    async fn call(t: &Tab, p: Value) -> BResult<()> {
        t.call("Input.dispatchMouseEvent", p, Some(INPUT_TIMEOUT))
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    const STEPS: i32 = 12;
    for i in 1..=STEPS {
        let f = i as f64 / STEPS as f64;
        let f = f * f * (3.0 - 2.0 * f); // ease in/out
        let (jitter, nap) = {
            let mut r = rand::rng();
            (r.random::<f64>() * 4.0 - 2.0, 15 + r.random_range(0..25u64))
        };
        call(
            t,
            json!({"type": "mouseMoved", "x": x + (tx - x) * f + jitter, "y": y + (ty - y) * f + jitter, "button": "none"}),
        )
        .await?;
        sleep_ms(nap).await;
    }
    if rand::rng().random_range(0..2) == 0 {
        return Ok(());
    }
    let d = (60 + rand::rng().random_range(0..120)) as f64;
    for dy in [d, -d] {
        // cdproto drops zero deltas from the JSON, but Chrome requires both, so wheel events use our own params.
        call(
            t,
            json!({"type": "mouseWheel", "x": tx, "y": ty, "deltaX": 0.0, "deltaY": dy, "modifiers": 0}),
        )
        .await?;
        let nap = 300 + rand::rng().random_range(0..500u64);
        sleep_ms(nap).await;
    }
    Ok(())
}

/// tunnel.rs `viaTunnel`
pub(crate) fn via_tunnel(h: &HeaderMap) -> bool {
    ["cf-connecting-ip", "cf-ray", "x-forwarded-for"]
        .iter()
        .any(|k| h.get(*k).is_some_and(|v| !v.is_empty()))
}

/// registerKeepAliveAPI (`b` is `h.browser`, None without a browser)
pub fn routes(b: Option<Arc<Browser>>) -> Router {
    Router::new()
        .route("/api/keepalive", any(api_keepalive))
        .with_state(KeepAlive::new(b))
}

async fn api_keepalive(State(k): State<Arc<KeepAlive>>, req: Request) -> Response {
    let can = !via_tunnel(req.headers());
    let (parts, body) = req.into_parts();
    if parts.method == axum::http::Method::POST {
        if !can {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                .header("x-content-type-options", "nosniff")
                .body(Body::from(
                    "keep-alive can only be switched from the machine that runs the app\n",
                ))
                .expect("static response");
        }
        let bytes = to_bytes(body, 4096).await.unwrap_or_default();
        // `json.NewDecoder(...).Decode(&req)` with the error ignored: on = false when it does not parse
        let on = serde_json::Deserializer::from_slice(&bytes)
            .into_iter::<Value>()
            .next()
            .and_then(Result::ok)
            .and_then(|v| v.get("on").and_then(Value::as_bool))
            .unwrap_or(false);
        k.set(on);
    }
    let mut s = k.status();
    s.can_toggle = can;
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(
            "{}\n",
            serde_json::to_string(&s).unwrap_or_default()
        )))
        .expect("static response")
}
