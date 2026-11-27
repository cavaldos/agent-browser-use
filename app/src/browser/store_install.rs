//! Local-only extension sessions: Chrome owns the install and removal prompts.
//! The viewer sees only the temporary Chrome window and can press its accessible buttons.
use std::ffi::{c_char, c_void, CStr};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{routing::get, Router};
use serde::Deserialize;
use serde_json::json;

use super::{first_page_session, Browser, CMD_TIMEOUT};
use crate::cdp::{self, Flags, LaunchSpec};

pub fn routes(browser: Option<Arc<Browser>>) -> Router {
    Router::new()
        .route("/api/extension-install", get(available))
        .route("/ws/extension-install", get(upgrade))
        .with_state(browser.filter(|b| b.opt.engine.name == "chrome"))
}

fn local(peer: SocketAddr, headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    peer.ip().is_loopback()
        && !crate::keepalive::via_tunnel(headers)
        && host
            .parse::<axum::http::uri::Authority>()
            .is_ok_and(|a| matches!(a.host(), "localhost" | "127.0.0.1" | "[::1]"))
}

async fn available(
    State(browser): State<Option<Arc<Browser>>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> StatusCode {
    if browser.is_some() && local(peer, &headers) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn upgrade(
    State(browser): State<Option<Arc<Browser>>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(browser) = browser else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !local(peer, &headers)
        || headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
            != headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(|h| format!("http://{h}"))
                .as_deref()
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    ws.max_message_size(4096)
        .max_frame_size(4096)
        .on_upgrade(move |mut socket| async move {
            let result = extension_session(&browser, &mut socket).await;
            let message = match result {
                Ok(()) => json!({"done":true}),
                Err(error) => json!({"error":error,"done":true}),
            };
            let _ = socket.send(Message::Text(message.to_string().into())).await;
        })
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
    Start { value: String },
    Manage,
    Frame,
    Click { x: f64, y: f64 },
    Scroll { delta_y: f64 },
    Finish { enable: bool },
}

async fn receive(socket: &mut WebSocket) -> Result<Option<Request>, String> {
    while let Some(message) = socket.recv().await {
        match message.map_err(|e| e.to_string())? {
            Message::Text(text) => {
                return serde_json::from_str(&text)
                    .map(Some)
                    .map_err(|e| e.to_string());
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(None)
}

async fn extension_session(browser: &Arc<Browser>, socket: &mut WebSocket) -> Result<(), String> {
    let request = tokio::time::timeout(Duration::from_secs(10), receive(socket))
        .await
        .map_err(|_| "Timed out waiting for an extension action".to_string())??;
    let (url, managing) = match request {
        Some(Request::Start { value }) => (
            crate::web_store_url(&value)
                .ok_or("Enter a Chrome Web Store detail URL or extension ID")?,
            false,
        ),
        Some(Request::Manage) => ("chrome://extensions/".into(), true),
        None => return Ok(()),
        _ => return Err("Choose an extension action first".into()),
    };
    if !unsafe { CGPreflightScreenCaptureAccess() && AXIsProcessTrusted() } {
        return Err("Allow Screen Recording and Accessibility for Agent Browser Use (or the app that launched it) in macOS System Settings, then restart it and try again.".into());
    }
    let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let _guard = browser
        .rmu
        .try_lock()
        .map_err(|_| "The browser is busy restarting or managing extensions")?;
    if !browser.stop_for_restart().await {
        return Err("The browser is shutting down".into());
    }
    let result = run_window(
        browser,
        socket,
        &temp.path().join("frame.jpg"),
        &url,
        managing,
    )
    .await;
    browser.relaunch().await;
    result
}

async fn run_window(
    browser: &Browser,
    socket: &mut WebSocket,
    image: &std::path::Path,
    url: &str,
    managing: bool,
) -> Result<(), String> {
    let mut flags = Flags::default();
    flags.str("user-data-dir", &browser.opt.profile);
    flags.bool("no-first-run", true);
    flags.bool("no-default-browser-check", true);
    flags.str("password-store", "basic");
    flags.bool("use-mock-keychain", true);
    flags.bool("force-renderer-accessibility", true);
    let chrome = cdp::launch(LaunchSpec {
        exec_path: browser.opt.engine.path.clone(),
        flags,
        obscura: false,
        cdp_url: String::new(),
        debug_port: false,
    })
    .await?;
    let result = async {
        let (_, tab) = tokio::time::timeout(CMD_TIMEOUT, first_page_session(&chrome))
            .await
            .map_err(|_| "Timed out opening the Chrome window")??;
        chrome
            .conn
            .call_with(
                Some(&tab),
                "Page.navigate",
                json!({"url":url}),
                Some(CMD_TIMEOUT),
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        tokio::select! {
            _ = chrome.wait_exit() => Ok(false),
            result = tokio::time::timeout(Duration::from_secs(300), frames(socket, &chrome, image, managing.then_some(tab.as_str()))) => {
                result.map_err(|_| "The extensions session timed out after 5 minutes")?
            }
        }
    }
    .await;
    if tokio::time::timeout(Duration::from_secs(3), chrome.close_gracefully())
        .await
        .is_err()
    {
        chrome.kill();
        let _ = tokio::time::timeout(Duration::from_secs(3), chrome.wait_exit()).await;
    }
    if result? && !managing {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(std::path::Path::new(&browser.opt.profile).join(".abu-extensions-enabled"))
            .map_err(|e| format!("Could not enable extensions for this profile: {e}"))?;
    }
    Ok(())
}

async fn frames(
    socket: &mut WebSocket,
    chrome: &cdp::Chrome,
    image: &std::path::Path,
    manager: Option<&str>,
) -> Result<bool, String> {
    let pid = chrome.pid.ok_or("Chrome did not start a local window")?;
    let mut current = None;
    let mut next_frame = tokio::time::Instant::now();
    while let Some(request) = receive(socket).await? {
        match request {
            Request::Frame => {
                tokio::time::sleep_until(next_frame).await;
                next_frame = tokio::time::Instant::now() + Duration::from_millis(350);
                current = window(pid);
                let Some(w) = current else {
                    socket
                        .send(Message::Text("{}".into()))
                        .await
                        .map_err(|e| e.to_string())?;
                    continue;
                };
                let mut capture = tokio::process::Command::new("/usr/sbin/screencapture");
                capture
                    .args(["-x", "-o", "-l", &w.id.to_string(), "-t", "jpg"])
                    .arg(image)
                    .kill_on_drop(true)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                if tokio::time::timeout(Duration::from_secs(3), capture.status())
                    .await
                    .is_ok_and(|r| r.is_ok_and(|s| s.success()))
                {
                    if let Ok(bytes) = tokio::fs::read(image).await {
                        socket
                            .send(Message::Binary(bytes.into()))
                            .await
                            .map_err(|e| e.to_string())?;
                        continue;
                    }
                }
                current = None;
                socket
                    .send(Message::Text("{}".into()))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Request::Click { x, y } => {
                if let Some(w) = current.filter(|w| window(pid) == Some(*w)) {
                    if let Some((sx, sy)) = w.position(x, y) {
                        let _ = tokio::task::spawn_blocking(move || press(pid, sx, sy)).await;
                    }
                }
            }
            Request::Scroll { delta_y } => {
                if let Some(tab) = manager.filter(|_| current.is_some()) {
                    scroll_manager(chrome, tab, delta_y).await;
                }
            }
            Request::Finish { enable } => return Ok(enable),
            Request::Start { .. } | Request::Manage => {
                return Err("An extensions session is already active".into())
            }
        }
    }
    Ok(false)
}

async fn scroll_manager(chrome: &cdp::Chrome, tab: &str, delta_y: f64) {
    let timeout = Some(Duration::from_secs(1));
    let Ok(metrics) = chrome
        .conn
        .call_with(Some(tab), "Page.getLayoutMetrics", json!({}), timeout, None)
        .await
    else {
        return;
    };
    let viewport = &metrics["cssVisualViewport"];
    let (Some(width), Some(height)) = (
        viewport["clientWidth"].as_f64(),
        viewport["clientHeight"].as_f64(),
    ) else {
        return;
    };
    // Send wheel input only to the temporary management tab, never to the OS or shared browser.
    let _ = chrome
        .conn
        .call_with(
            Some(tab),
            "Input.dispatchMouseEvent",
            json!({
                "type":"mouseWheel", "x":width / 2.0, "y":height / 2.0,
                "deltaX":0, "deltaY":delta_y.clamp(-1000.0, 1000.0),
            }),
            timeout,
            None,
        )
        .await;
}

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq)]
struct Bounds {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Clone, Copy, PartialEq)]
struct Window {
    id: u32,
    bounds: Bounds,
}

impl Window {
    fn position(self, x: f64, y: f64) -> Option<(f32, f32)> {
        if !(0.0..1.0).contains(&x) || !(0.0..1.0).contains(&y) {
            return None;
        }
        let b = self.bounds;
        Some(((b.x + x * b.width) as f32, (b.y + y * b.height) as f32))
    }
}

type Cf = *const c_void;

struct Owned(Cf);
impl Owned {
    fn new(value: Cf) -> Option<Self> {
        (!value.is_null()).then(|| Self(value))
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(value: Cf);
    fn CFArrayGetCount(array: Cf) -> isize;
    fn CFArrayGetValueAtIndex(array: Cf, index: isize) -> Cf;
    fn CFDictionaryGetValue(dict: Cf, key: Cf) -> Cf;
    fn CFNumberGetValue(number: Cf, kind: isize, out: *mut i32) -> bool;
    fn CFStringCreateWithCString(allocator: Cf, string: *const c_char, encoding: u32) -> Cf;
    fn CFEqual(a: Cf, b: Cf) -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGWindowListCopyWindowInfo(options: u32, window: u32) -> Cf;
    fn CGRectMakeWithDictionaryRepresentation(dict: Cf, rect: *mut Bounds) -> bool;
    static kCGWindowOwnerPID: Cf;
    static kCGWindowLayer: Cf;
    static kCGWindowNumber: Cf;
    static kCGWindowBounds: Cf;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXUIElementCreateApplication(pid: i32) -> Cf;
    fn AXUIElementSetMessagingTimeout(element: Cf, timeout: f32) -> i32;
    fn AXUIElementCopyElementAtPosition(app: Cf, x: f32, y: f32, out: *mut Cf) -> i32;
    fn AXUIElementCopyAttributeValue(element: Cf, attribute: Cf, out: *mut Cf) -> i32;
    fn AXUIElementGetPid(element: Cf, out: *mut i32) -> i32;
    fn AXUIElementPerformAction(element: Cf, action: Cf) -> i32;
}

fn window(pid: u32) -> Option<Window> {
    unsafe {
        // On-screen windows only, excluding desktop elements. Never select by a client-supplied ID.
        let list = Owned::new(CGWindowListCopyWindowInfo(1 | 16, 0))?;
        let number = |dict, key| {
            let raw = CFDictionaryGetValue(dict, key);
            let mut value = 0;
            (!raw.is_null() && CFNumberGetValue(raw, 3, &mut value)).then_some(value)
        };
        let mut largest = None;
        let mut area = 0.0;
        for i in 0..CFArrayGetCount(list.0) {
            let dict = CFArrayGetValueAtIndex(list.0, i);
            if number(dict, kCGWindowOwnerPID) != Some(pid as i32)
                || number(dict, kCGWindowLayer) != Some(0)
            {
                continue;
            }
            let raw = CFDictionaryGetValue(dict, kCGWindowBounds);
            let mut bounds = Bounds::default();
            if raw.is_null() || !CGRectMakeWithDictionaryRepresentation(raw, &mut bounds) {
                continue;
            }
            if let Some(id) = number(dict, kCGWindowNumber) {
                let size = bounds.width * bounds.height;
                if size > area {
                    largest = Some(Window {
                        id: id as u32,
                        bounds,
                    });
                    area = size;
                }
            }
        }
        largest
    }
}

fn press(pid: u32, x: f32, y: f32) -> Option<()> {
    let string = |s: &CStr| {
        Owned::new(unsafe { CFStringCreateWithCString(std::ptr::null(), s.as_ptr(), 0x08000100) })
    };
    unsafe {
        let app = Owned::new(AXUIElementCreateApplication(pid as i32))?;
        AXUIElementSetMessagingTimeout(app.0, 2.0);
        let mut element = std::ptr::null();
        if AXUIElementCopyElementAtPosition(app.0, x, y, &mut element) != 0 {
            return None;
        }
        let mut element = Owned::new(element)?;
        let role_attr = string(c"AXRole")?;
        let parent_attr = string(c"AXParent")?;
        let button = string(c"AXButton")?;
        for _ in 0..8 {
            let mut owner = 0;
            if AXUIElementGetPid(element.0, &mut owner) != 0 || owner != pid as i32 {
                return None;
            }
            let mut role = std::ptr::null();
            if AXUIElementCopyAttributeValue(element.0, role_attr.0, &mut role) != 0 {
                return None;
            }
            if CFEqual(Owned::new(role)?.0, button.0) {
                return (AXUIElementPerformAction(element.0, string(c"AXPress")?.0) == 0)
                    .then_some(());
            }
            let mut parent = std::ptr::null();
            if AXUIElementCopyAttributeValue(element.0, parent_attr.0, &mut parent) != 0 {
                return None;
            }
            element = Owned::new(parent)?;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_install_is_loopback_only() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "localhost:3000".parse().unwrap());
        let loopback = "127.0.0.1:8800".parse().unwrap();
        assert!(local(loopback, &headers));
        assert!(!local("192.0.2.4:8800".parse().unwrap(), &headers));
        headers.insert("cf-ray", "external".parse().unwrap());
        assert!(!local(loopback, &headers));
        headers.remove("cf-ray");
        headers.insert(header::HOST, "localhost.example:3000".parse().unwrap());
        assert!(!local(loopback, &headers));
    }

    #[test]
    fn clicks_are_inside_the_captured_window() {
        let w = Window {
            id: 1,
            bounds: Bounds {
                x: -50.0,
                y: 10.0,
                width: 100.0,
                height: 200.0,
            },
        };
        assert_eq!(w.position(0.5, 0.5), Some((0.0, 110.0)));
        assert!(w.position(1.0, 0.5).is_none());
        assert!(w.position(f64::NAN, 0.0).is_none());
    }
}
