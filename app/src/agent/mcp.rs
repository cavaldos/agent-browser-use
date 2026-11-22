//! MCP (Model Context Protocol) server, so AI agents (Claude Code, Claude Desktop,
//! Cursor, VS Code…) can drive the shared browser with tools in the style of
//! Playwright MCP: browser_navigate, browser_snapshot, browser_click…
//!
//! Two transports, same tools:
//!   - Streamable HTTP at /mcp on the web server (plain JSON responses).
//!   - stdio: `agent-browser-use mcp`, one JSON-RPC message per line. If a server
//!     is already running on the port, it just forwards to that one.
//!
//! # Rust port notes
//!   * The 36 tool definitions (`var tools`) are kept as data in `mcp_tools.json` (captured from the
//!     server's `tools/list`, so names, descriptions and schemas are byte-identical) and parsed once.
//!   * `Hub::read_chat` is the async-aware version of the unlock around `browser_read_chat`: the
//!     worker's call guard (`Agent.mu`, an owned tokio guard) is dropped while waiting for a message and
//!     taken again afterwards, so the worker's other calls are not blocked by the wait.
//!   * With `-no-browser` crashes on `initialize` (nil agent). Here `initialize` and `tools/list` work,
//!     the instructions carry a notice, and every `tools/call` answers with an error result.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::agent::{cmp_or, Agent};
use crate::chat::SeatRef;
use crate::observe::NetFilter;
use crate::tools::{CookieArgs, UploadFile};
use crate::{auth, go_json, golog, Hub, APP_NAME};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub const APP_VERSION: &str = "1.0.0";

const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const SERVER_INSTRUCTIONS: &str = r#"This browser is watched live by people in a web UI: they see your cursor, the element you act on, and a caption for every step.
Work like Playwright MCP: call browser_snapshot to get the page outline, then act on elements by their ref (e.g. ref "e12"). Action tools return a fresh snapshot.
To react to the page (the other player's move in a game, a chat reply, a result appearing), call browser_wait_for_change: it returns as soon as the page changes, with what changed.
Use browser_say to tell the people watching what you are doing or what you need from them (for example, to log in for you).
You take control by yourself when you act, even from a person, and give it back after you have been idle a little while.
After an action, the result lists new console errors and failed requests ("Page problems"). Dig deeper with browser_console_messages and browser_network_requests.
Files: browser_downloads waits for/reads downloaded files; browser_file_upload fills file inputs (from the upload folder only).
Logins: browser_cookies, browser_storage and browser_state (save/load a whole session by name, or export/import sessions as a JSON file to move them between machines). browser_snapshot_diff shows what changed on the page. browser_emulate switches to a phone/tablet or changes locale, dark mode, location and network speed.
People can chat with you from the viewer: when idle, call browser_read_chat (time: 60) to wait for messages written to you with @<your name> or @agent, and answer with browser_say.
Multi-agent: to run sub-agents in parallel, call browser_tabs {action:"new", background:true, url} once per sub-agent and give each its tabId. A sub-agent passes tabId on every browser_* call; it then has its own cursor, refs and snapshots, and never touches other tabs. Calls on different tabs run concurrently, calls on the same tab run in order. Sub-agents may also share a tab: give each a distinct "agent" name; to run a multi-step sequence that depends on page state (pick a tool, then draw), wrap it in browser_lock_tab / browser_unlock_tab.
Recorded flows: when a person's message contains an id like flow-3fa9c1 (often nothing else), it names a flow they recorded in the viewer. Look it up with browser_get_flow {flowId}, then do what they ask with it: replay it with browser_run_flow (repeat, inputs to change typed text), or follow its steps yourself to adapt them. With only the id and no other request, replay it once.
If a tool says the agent is blocked, a person clicked "Block agent": wait, or ask them (browser_say) to click "Unblock"."#;

// ---------- JSON-RPC ----------

#[derive(Deserialize)]
struct RpcRequest {
    #[allow(dead_code)]
    #[serde(default)]
    jsonrpc: String,
    #[serde(default)]
    id: Value,
    #[serde(default)]
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct RpcError {
    code: i64,
    message: String,
}

/// What `Result any` holds in maps (sorted keys, so a `Value`) or the structs that keep field order.
#[derive(Serialize)]
#[serde(untagged)]
enum RpcResult {
    Json(Value),
    Tools(ToolsList),
    Tool(ToolResult),
}

#[derive(Serialize)]
struct ToolsList {
    tools: &'static Vec<Tool>,
}

#[derive(Serialize)]
pub struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<RpcResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Serialize)]
struct Content {
    #[serde(rename = "type")]
    type_: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    text: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    data: String,
    #[serde(rename = "mimeType", skip_serializing_if = "String::is_empty")]
    mime_type: String,
}

#[derive(Serialize)]
pub struct ToolResult {
    content: Vec<Content>,
    #[serde(rename = "isError", skip_serializing_if = "crate::is_false")]
    is_error: bool,
}

fn text_result(s: impl Into<String>) -> ToolResult {
    ToolResult {
        content: vec![Content {
            type_: "text".into(),
            text: s.into(),
            data: String::new(),
            mime_type: String::new(),
        }],
        is_error: false,
    }
}

fn error_result(err: impl std::fmt::Display) -> ToolResult {
    ToolResult {
        content: vec![Content {
            type_: "text".into(),
            text: format!("Error: {err}"),
            data: String::new(),
            mime_type: String::new(),
        }],
        is_error: true,
    }
}

pub struct McpServer {
    hub: Arc<Hub>,
    agent: OnceLock<Arc<Agent>>,
    /// Mcp-Session-Id -> *Seat
    sessions: Mutex<HashMap<String, SeatRef>>,
}

/// mcpConn is one MCP client connection: it knows which coding agent is on the other side.
#[derive(Default)]
pub struct McpConn {
    seat: Mutex<Option<SeatRef>>,
}

fn rpc_ok(id: Value, result: Value) -> RpcResponse {
    rpc_result(id, RpcResult::Json(result))
}

fn rpc_result(id: Value, result: RpcResult) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

fn rpc_err(id: Value, code: i64, message: String) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(RpcError { code, message }),
    }
}

impl McpServer {
    pub fn new(hub: Arc<Hub>) -> Arc<McpServer> {
        Arc::new(McpServer {
            hub,
            agent: OnceLock::new(),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// `mcp.agent = newAgent(...)` (not set with -no-browser).
    pub fn set_agent(&self, a: Arc<Agent>) {
        let _ = self.agent.set(a);
    }

    pub fn agent(&self) -> Option<Arc<Agent>> {
        self.agent.get().cloned()
    }

    /// handle answers one message; None for notifications.
    pub async fn handle(&self, raw: &[u8], c: &McpConn) -> Option<RpcResponse> {
        let req: RpcRequest = match serde_json::from_slice(raw) {
            Ok(r) => r,
            Err(_) => return Some(rpc_err(Value::Null, -32700, "parse error".into())),
        };
        if req.id.is_null() {
            return None; // notification (initialized, cancelled…)
        }
        let id = req.id.clone();
        let resp = match req.method.as_str() {
            "initialize" => {
                let name = req
                    .params
                    .pointer("/clientInfo/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let want = req
                    .params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let mut instructions = SERVER_INSTRUCTIONS.to_string();
                if self.agent().is_some() {
                    *lock(&c.seat) = Some(self.hub.register_seat(name, true));
                } else {
                    // would crash here (nil agent); say what is going on instead
                    instructions.push_str("\n\nNote: the shared browser is disabled on this server (started with -no-browser): every browser tool will return an error.");
                }
                let v = if PROTOCOL_VERSIONS.contains(&want) {
                    want
                } else {
                    PROTOCOL_VERSIONS[0]
                };
                rpc_ok(
                    id,
                    json!({
                        "protocolVersion": v,
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {"name": APP_NAME, "title": "Agent Browser Use", "version": APP_VERSION},
                        "instructions": instructions,
                    }),
                )
            }
            "ping" => rpc_ok(id, json!({})),
            "tools/list" => rpc_result(id, RpcResult::Tools(ToolsList { tools: &TOOLS })),
            "tools/call" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(default)]
                    name: String,
                    #[serde(default)]
                    arguments: Value,
                }
                let Ok(p) = serde_json::from_value::<P>(req.params.clone()) else {
                    return Some(rpc_err(id, -32602, "invalid params".into()));
                };
                if !TOOLS.iter().any(|t| t.name == p.name) {
                    return Some(rpc_err(id, -32602, format!("unknown tool: {}", p.name)));
                }
                let seat = {
                    let mut s = lock(&c.seat);
                    if s.is_none() && self.agent().is_some() {
                        // the client did not say who it is (no initialize / no session id)
                        *s = Some(self.hub.register_seat("", false));
                    }
                    s.clone()
                };
                let res = self.call_tool(&p.name, p.arguments, seat).await;
                rpc_result(id, RpcResult::Tool(res))
            }
            "resources/list" => rpc_ok(id, json!({"resources": []})),
            "prompts/list" => rpc_ok(id, json!({"prompts": []})),
            m => rpc_err(id, -32601, format!("method not found: {m}")),
        };
        Some(resp)
    }
}

// ---------- Tools ----------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
}

/// `var tools`: see the module docs.
pub static TOOLS: LazyLock<Vec<Tool>> =
    LazyLock::new(|| serde_json::from_str(include_str!("mcp_tools.json")).expect("mcp_tools.json"));

/// toolArgs: the union of every tool's arguments (unknown keys are ignored, `null` counts as absent).
#[derive(Default, Deserialize)]
#[serde(default)]
struct ToolArgs {
    url: String,
    element: String,
    #[serde(rename = "ref")]
    ref_: String,
    #[serde(rename = "doubleClick")]
    double_click: bool,
    button: String,
    text: String,
    submit: bool,
    clear: Option<bool>,
    key: String,
    values: Vec<String>,
    direction: String,
    amount: f64,
    #[serde(rename = "startElement")]
    start_element: String,
    #[serde(rename = "startRef")]
    start_ref: String,
    #[serde(rename = "endElement")]
    end_element: String,
    #[serde(rename = "endRef")]
    end_ref: String,
    #[serde(rename = "textGone")]
    text_gone: String,
    time: f64,
    function: String,
    action: String,
    index: Option<f64>,
    message: String,
    selector: String,
    code: String,
    #[serde(rename = "flowId")]
    flow_id: String,
    repeat: f64,
    inputs: HashMap<String, String>,
    #[serde(rename = "skipStart")]
    skip_start: bool,

    level: String,
    limit: f64,
    filter: String,
    method: String,
    #[serde(rename = "failedOnly")]
    failed_only: bool,
    #[serde(rename = "includeStatic")]
    include_static: bool,
    id: f64,
    body: bool,
    history: bool,
    name: String,
    paths: Vec<String>,
    files: Vec<UploadFile>,
    value: String,
    domain: String,
    path: String,
    secure: Option<bool>,
    #[serde(rename = "httpOnly")]
    http_only: Option<bool>,
    #[serde(rename = "sameSite")]
    same_site: String,
    expires: f64,
    all: bool,
    full: bool,
    #[serde(rename = "type")]
    type_: String,
    baseline: String,
    save: String,
    device: String,
    width: Option<i64>,
    height: Option<i64>,
    scale: Option<f64>,
    mobile: Option<bool>,
    touch: Option<bool>,
    #[serde(rename = "userAgent")]
    user_agent: Option<String>,
    locale: Option<String>,
    timezone: Option<String>,
    #[serde(rename = "colorScheme")]
    color_scheme: Option<String>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    accuracy: Option<f64>,
    network: Option<String>,
    #[serde(rename = "tabId")]
    tab_id: String,
    background: Option<bool>,
    agent: String,
    x: Option<f64>,
    y: Option<f64>,
    #[serde(rename = "startX")]
    start_x: Option<f64>,
    #[serde(rename = "startY")]
    start_y: Option<f64>,
    #[serde(rename = "endX")]
    end_x: Option<f64>,
    #[serde(rename = "endY")]
    end_y: Option<f64>,
}

/// inputTools act on the page with the mouse or keyboard; they run one at a time per tab.
const INPUT_TOOLS: [&str; 10] = [
    "browser_click",
    "browser_drag",
    "browser_hover",
    "browser_type",
    "browser_press_key",
    "browser_scroll",
    "browser_select_option",
    "browser_open_picker",
    "browser_close_picker",
    "browser_run_flow",
];

/// `defer a.hub.agentDone`
struct DoneGuard(Arc<Hub>);

impl Drop for DoneGuard {
    fn drop(&mut self) {
        self.0.agent_done();
    }
}

impl McpServer {
    /// withSnapshot runs an action and returns its note plus the page afterwards.
    async fn with_snapshot(
        a: &Agent,
        seat: &SeatRef,
        done: &str,
        note: &str,
        err: Result<(), String>,
    ) -> ToolResult {
        if let Err(e) = err {
            return error_result(e);
        }
        let snap = match a.changes().await {
            Ok(s) => s,
            Err(e) => format!("(could not read the page: {e})"),
        };
        let mut out = format!("{done}\n");
        if !note.is_empty() {
            out.push_str(note);
            out.push('\n');
        }
        let problems = a.alerts();
        if !problems.is_empty() {
            out.push('\n');
            out.push_str(&problems);
        }
        let n = a.hub.unread_for(seat);
        if n > 0 {
            out.push_str(&format!(
                "\n{n} new chat message(s) for you: call browser_read_chat.\n"
            ));
        }
        text_result(format!("{out}\n{snap}"))
    }

    async fn call_tool(&self, name: &str, raw: Value, seat: Option<SeatRef>) -> ToolResult {
        let Some(root) = self.agent() else {
            return error_result(
                "the shared browser is disabled on this server (started with -no-browser)",
            );
        };
        let Some(seat) = seat else {
            return error_result("no session");
        };
        let mut args = ToolArgs::default();
        if !raw.is_null() {
            // json.Unmarshal: a null value leaves the field alone
            let raw = match raw {
                Value::Object(o) => Value::Object(
                    o.into_iter()
                        .filter(|(_, v)| !v.is_null())
                        .collect::<Map<_, _>>(),
                ),
                v => v,
            };
            match serde_json::from_value(raw) {
                Ok(a) => args = a,
                Err(e) => return error_result(format!("invalid arguments: {e}")),
            }
        }

        if let Err(e) = root.hub.seat_gone(&seat) {
            return error_result(e);
        }
        let handle = lock(&seat).handle.clone();
        if name == "browser_disconnect" {
            root.hub.remove_seat(&seat, "self");
            return text_result(format!("Disconnected: you (@{handle}) left the shared browser and its room, and gave back control and your tab locks. Calling any browser tool connects you again."));
        }
        let who = if args.agent.is_empty() {
            handle.clone() // every coding agent has its own cursor, named after its @handle
        } else {
            format!("{handle}/{}", args.agent) // its sub-agents
        };
        root.hub.agent_as(&seat);
        let a = root.worker_or_self(&who, &args.tab_id);
        let me = cmp_or(&who, "default");
        match name {
            "browser_lock_tab" => {
                let ttl = Duration::from_secs_f64(args.time.clamp(5.0, 120.0));
                if let Err(e) = a.b.claim_tab(&args.tab_id, &me, ttl).await {
                    return error_result(e);
                }
                return text_result(format!("Tab locked for {me} for {:.0}s. Other agents wait until you call browser_unlock_tab (or it expires).", ttl.as_secs_f64()));
            }
            "browser_unlock_tab" => {
                a.b.release_tab(&args.tab_id, &me);
                return text_result("Tab unlocked.");
            }
            _ => {}
        }
        // Drop order: agent done, then the agent lock, then the gesture lock.
        let _gesture = if INPUT_TOOLS.contains(&name) {
            match a.b.lock_gesture(&args.tab_id, &me).await {
                // always gesture lock first, then the agent lock
                Ok(g) => g,
                Err(e) => return error_result(e),
            }
        } else {
            None
        };
        let mut _call_guard = Some(a.mu.clone().lock_owned().await);
        a.begin();
        let _done = DoneGuard(a.hub.clone());

        let a = &*a;
        match name {
            // leaving the page: its old problems are not news for the next one
            "browser_navigate"
            | "browser_navigate_back"
            | "browser_navigate_forward"
            | "browser_reload" => {
                if let Some(o) = a.active_obs() {
                    o.mark_seen();
                }
            }
            _ => {}
        }

        macro_rules! try_text {
            ($e:expr) => {
                match $e {
                    Ok(out) => text_result(out),
                    Err(e) => error_result(e),
                }
            };
        }

        match name {
            "browser_navigate" => {
                let r = a.navigate(&args.url).await;
                Self::with_snapshot(a, &seat, &format!("Opened {}", args.url), "", r).await
            }
            "browser_navigate_back" => {
                let r = a.history(-1).await;
                Self::with_snapshot(a, &seat, "Went back", "", r).await
            }
            "browser_navigate_forward" => {
                let r = a.history(1).await;
                Self::with_snapshot(a, &seat, "Went forward", "", r).await
            }
            "browser_reload" => {
                let r = a.reload().await;
                Self::with_snapshot(a, &seat, "Reloaded", "", r).await
            }
            "browser_snapshot" => try_text!(a.snapshot_text().await),
            "browser_click" => {
                let (note, err) = match a
                    .click(
                        &args.ref_,
                        &args.element,
                        &args.button,
                        args.double_click,
                        args.x,
                        args.y,
                    )
                    .await
                {
                    Ok(n) => (n, Ok(())),
                    Err(e) => (String::new(), Err(e)),
                };
                let done = format!("Clicked {}", cmp_or(&args.element, &args.ref_));
                Self::with_snapshot(a, &seat, &done, &note, err).await
            }
            "browser_get_element" => try_text!(a.get_picked(&args.code).await),
            "browser_get_flow" => try_text!(a.get_flow(&args.flow_id).await),
            "browser_run_flow" => {
                let (out, err) = match a
                    .run_flow(
                        &args.flow_id,
                        args.repeat as i64,
                        &args.inputs,
                        args.skip_start,
                    )
                    .await
                {
                    Ok(o) => (o, Ok(())),
                    Err(e) => (String::new(), Err(e)),
                };
                let done = format!(
                    "Flow {} finished",
                    crate::tools::norm_flow_id(&args.flow_id)
                );
                Self::with_snapshot(a, &seat, &done, &out, err).await
            }
            "browser_hover" => {
                let r = a.hover(&args.ref_, &args.element).await;
                let done = format!("Hovered {}", cmp_or(&args.element, &args.ref_));
                Self::with_snapshot(a, &seat, &done, "", r).await
            }
            "browser_type" => {
                let clear = args.clear.unwrap_or(true);
                let r = a
                    .type_(&args.ref_, &args.element, &args.text, args.submit, clear)
                    .await;
                let done = format!(
                    "Typed into {}",
                    cmp_or(&args.element, &cmp_or(&args.ref_, "the focused field"))
                );
                Self::with_snapshot(a, &seat, &done, "", r).await
            }
            "browser_press_key" => {
                let r = a.press(&args.key).await;
                Self::with_snapshot(a, &seat, &format!("Pressed {}", args.key), "", r).await
            }
            "browser_select_option" => {
                let (picked, err) = match a
                    .select_option(&args.ref_, &args.element, &args.values)
                    .await
                {
                    Ok(p) => (p, Ok(())),
                    Err(e) => (Vec::new(), Err(e)),
                };
                let done = format!("Selected {}", picked.join(", "));
                Self::with_snapshot(a, &seat, &done, "", err).await
            }
            "browser_open_picker" => {
                let (msg, err) = match a.open_picker(&args.ref_, &args.element).await {
                    Ok(m) => (m, Ok(())),
                    Err(e) => (String::new(), Err(e)),
                };
                Self::with_snapshot(a, &seat, &msg, "", err).await
            }
            "browser_close_picker" => {
                let (msg, err) = match a.close_picker(&args.ref_, &args.element).await {
                    Ok(m) => (m, Ok(())),
                    Err(e) => (String::new(), Err(e)),
                };
                Self::with_snapshot(a, &seat, &msg, "", err).await
            }
            "browser_scroll" => {
                let r = a
                    .scroll(&args.direction, args.amount, &args.ref_, &args.element)
                    .await;
                let done = format!("Scrolled {}", cmp_or(&args.direction, "down"));
                Self::with_snapshot(a, &seat, &done, "", r).await
            }
            "browser_drag" => {
                let r = a
                    .drag(
                        &args.start_ref,
                        &args.start_element,
                        &args.end_ref,
                        &args.end_element,
                        args.start_x,
                        args.start_y,
                        args.end_x,
                        args.end_y,
                    )
                    .await;
                let done = format!("Dragged {} to {}", args.start_element, args.end_element);
                Self::with_snapshot(a, &seat, &done, "", r).await
            }
            "browser_wait_for" => {
                let r = a.wait_for(args.time, &args.text, &args.text_gone).await;
                Self::with_snapshot(a, &seat, "Done waiting", "", r).await
            }
            "browser_wait_for_change" => {
                let (changed, err) = match a
                    .wait_for_change(args.time, &args.ref_, &args.selector)
                    .await
                {
                    Ok(c) => (c, Ok(())),
                    Err(e) => (false, Err(e)),
                };
                let done = if changed {
                    "The page changed"
                } else {
                    "Nothing changed yet (timed out). Call browser_wait_for_change again to keep waiting"
                };
                Self::with_snapshot(a, &seat, done, "", err).await
            }
            "browser_take_screenshot" => match a.screenshot().await {
                Err(e) => error_result(e),
                Ok(data) => ToolResult {
                    content: vec![
                        Content {
                            type_: "image".into(),
                            text: String::new(),
                            data,
                            mime_type: "image/jpeg".into(),
                        },
                        Content {
                            type_: "text".into(),
                            text: "Screenshot of the current tab.".into(),
                            data: String::new(),
                            mime_type: String::new(),
                        },
                    ],
                    is_error: false,
                },
            },
            "browser_evaluate" => try_text!(a.evaluate(&args.function, &args.ref_).await),
            "browser_tabs" => {
                let index = args.index.map_or(-1, |i| i as i64);
                if args.action == "select" && index < 0 {
                    return error_result("index is required to select a tab");
                }
                match a
                    .tabs(
                        &args.action,
                        index,
                        &args.url,
                        &args.tab_id,
                        args.background.unwrap_or(false),
                    )
                    .await
                {
                    Err(e) => error_result(e),
                    Ok(out) if !out.is_empty() => text_result(out),
                    Ok(_) => Self::with_snapshot(a, &seat, "Done", "", Ok(())).await,
                }
            }
            "browser_console_messages" => try_text!(
                a.console(
                    &args.level,
                    args.limit as i64,
                    args.clear.unwrap_or(false),
                    args.history
                )
                .await
            ),
            "browser_network_requests" => try_text!(
                a.network(
                    NetFilter {
                        match_: args.filter.clone(),
                        method: args.method.clone(),
                        failed_only: args.failed_only,
                        static_: args.include_static,
                        history: args.history,
                        limit: args.limit as i64,
                    },
                    args.id as i64,
                    args.body,
                    args.clear.unwrap_or(false),
                )
                .await
            ),
            "browser_downloads" => {
                try_text!(a.downloads(&args.action, &args.name, args.time).await)
            }
            "browser_file_upload" => {
                match a
                    .upload(&args.ref_, &args.element, args.paths.clone(), &args.files)
                    .await
                {
                    Err(e) => error_result(e),
                    Ok(out) => Self::with_snapshot(a, &seat, &out, "", Ok(())).await,
                }
            }
            "browser_cookies" => try_text!(
                a.cookies(CookieArgs {
                    action: args.action.clone(),
                    url: args.url.clone(),
                    name: args.name.clone(),
                    value: args.value.clone(),
                    domain: args.domain.clone(),
                    path: args.path.clone(),
                    same_site: args.same_site.clone(),
                    secure: args.secure,
                    http_only: args.http_only,
                    expires: args.expires,
                    all: args.all,
                    full: args.full,
                })
                .await
            ),
            "browser_storage" => try_text!(
                a.storage(&args.action, &args.type_, &args.key, &args.value)
                    .await
            ),
            "browser_state" => {
                try_text!(a.state_tool(&args.action, &args.name, &args.path).await)
            }
            "browser_snapshot_diff" => {
                try_text!(a.snapshot_diff(&args.baseline, &args.save).await)
            }
            "browser_emulate" => {
                let opts = crate::emulate::EmulateOpts {
                    device: args.device.clone(),
                    width: args.width,
                    height: args.height,
                    scale: args.scale,
                    mobile: args.mobile,
                    touch: args.touch,
                    user_agent: args.user_agent.clone(),
                    locale: args.locale.clone(),
                    timezone: args.timezone.clone(),
                    color_scheme: args.color_scheme.clone(),
                    lat: args.latitude,
                    lon: args.longitude,
                    accuracy: args.accuracy,
                    network: args.network.clone(),
                };
                let empty = opts.is_empty();
                let dev_list = opts.device.eq_ignore_ascii_case("list");
                match a.emulate(opts).await {
                    Err(e) => error_result(e),
                    Ok(out) => {
                        if empty || dev_list {
                            // just asking
                            return text_result(out);
                        }
                        Self::with_snapshot(a, &seat, "Emulation updated", &out, Ok(())).await
                    }
                }
            }
            "browser_read_chat" => {
                // waiting for a message must not block this worker's other calls
                drop(_call_guard.take());
                let out = a.hub.read_chat(&seat, args.time, args.all).await;
                _call_guard.replace(a.mu.clone().lock_owned().await);
                text_result(out)
            }
            "browser_say" => {
                if args.message.trim().is_empty() {
                    return error_result("message is required");
                }
                a.say(&args.message, &handle);
                text_result("Shown to the people watching.")
            }
            _ => error_result(format!("unknown tool {name}")),
        }
    }
}

impl Hub {
    /// agentAs makes the shared agent (the one holding control) show as this seat.
    pub fn agent_as(&self, seat: &SeatRef) {
        let handle = lock(seat).handle.clone();
        self.lock().agent_as(&handle, seat);
    }
}

// ---------- Streamable HTTP transport (/mcp) ----------

fn is_loopback(addr: &str) -> bool {
    let host = match addr.parse::<SocketAddr>() {
        Ok(sa) => sa.ip().to_string(),
        Err(_) => addr
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string(),
    };
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(v6)) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
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

impl McpServer {
    /// The route of /mcp (`mux.HandleFunc("/mcp", mcp.ServeHTTP(*mcpRemote))`).
    pub fn routes(self: &Arc<Self>, allow_remote: bool) -> Router {
        Router::new()
            .route("/mcp", any(serve_http))
            .with_state((self.clone(), allow_remote))
    }

    async fn serve_http(&self, allow_remote: bool, remote: SocketAddr, r: Request) -> Response {
        if !allow_remote && !is_loopback(&remote.to_string()) {
            return http_error(
                StatusCode::FORBIDDEN,
                "MCP is only available from this machine (start with -mcp-remote to allow other machines)",
            );
        }
        // Web pages must not reach the agent (DNS rebinding): only local origins, or none (non-browser clients).
        let o = r
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !o.is_empty() {
            let host_hdr = r
                .headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let ok = url::Url::parse(o).ok().is_some_and(|u| {
                let hostname = u.host_str().unwrap_or("");
                let hostname = hostname.trim_start_matches('[').trim_end_matches(']');
                let host = match u.port() {
                    Some(p) => format!("{}:{p}", u.host_str().unwrap_or("")),
                    None => u.host_str().unwrap_or("").to_string(),
                };
                hostname == "localhost"
                    || is_loopback(hostname)
                    || (allow_remote && host == host_hdr)
            });
            if !ok {
                return http_error(StatusCode::FORBIDDEN, "origin not allowed");
            }
        }
        let sid_hdr = r
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        match *r.method() {
            Method::POST => {}
            Method::DELETE => {
                // the client ends its session: its agent leaves the room
                let removed = lock(&self.sessions).remove(&sid_hdr);
                if let (Some(seat), false) = (removed, sid_hdr.is_empty()) {
                    self.hub.remove_seat(&seat, "");
                }
                return Response::builder().body(Body::empty()).unwrap();
            }
            _ => {
                // no server-initiated stream
                let mut resp = http_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
                resp.headers_mut()
                    .insert(header::ALLOW, HeaderValue::from_static("POST, DELETE"));
                return resp;
            }
        }
        let body = match axum::body::to_bytes(r.into_body(), 4 << 20).await {
            Ok(b) => b,
            Err(_) => return http_error(StatusCode::BAD_REQUEST, "bad request"),
        };
        let body = body.trim_ascii();
        let conn = McpConn::default();
        let mut sid = sid_hdr;
        if !sid.is_empty() {
            if let Some(seat) = lock(&self.sessions).get(&sid) {
                *lock(&conn.seat) = Some(seat.clone());
            }
        }
        let out: Option<String> = if body.first() == Some(&b'[') {
            // batch (older protocol versions)
            let Ok(batch) = serde_json::from_slice::<Vec<Value>>(body) else {
                return http_error(StatusCode::BAD_REQUEST, "bad request");
            };
            let mut resps = Vec::new();
            for m in batch {
                let raw = serde_json::to_vec(&m).unwrap_or_default();
                if let Some(resp) = self.handle(&raw, &conn).await {
                    resps.push(resp);
                }
            }
            (!resps.is_empty()).then(|| go_json(&resps))
        } else {
            self.handle(body, &conn).await.map(|r| go_json(&r))
        };
        let mut b = Response::builder();
        let seat = lock(&conn.seat).clone();
        if let (true, Some(seat)) = (sid.is_empty(), seat) {
            // initialize: give the client a session so we know who it is next time
            sid = crate::new_id();
            lock(&self.sessions).insert(sid.clone(), seat);
            b = b.header("mcp-session-id", sid.as_str());
        }
        let Some(out) = out else {
            return b.status(StatusCode::ACCEPTED).body(Body::empty()).unwrap();
        };
        b.header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!("{out}\n")))
            .unwrap()
    }
}

async fn serve_http(
    State((s, allow_remote)): State<(Arc<McpServer>, bool)>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    r: Request,
) -> Response {
    s.serve_http(allow_remote, remote, r).await
}

// ---------- stdio transport ----------

impl McpServer {
    /// serveStdio reads one message per line from stdin and answers on stdout until stdin closes.
    pub async fn serve_stdio(self: Arc<Self>) {
        let out = Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
        let conn = Arc::new(McpConn::default());
        dispatch_lines(tokio::io::stdin(), move |line| {
            let (s, conn, out) = (self.clone(), conn.clone(), out.clone());
            async move {
                if let Some(resp) = s.handle(&line, &conn).await {
                    use tokio::io::AsyncWriteExt;
                    let mut b = go_json(&resp).into_bytes();
                    b.push(b'\n');
                    let mut w = out.lock().await;
                    let _ = w.write_all(&b).await;
                    let _ = w.flush().await;
                }
            }
        })
        .await;
    }
}

/// dispatchLines runs requests concurrently; tool workers provide per-tab ordering.
async fn dispatch_lines<R, F, Fut>(r: R, f: F)
where
    R: tokio::io::AsyncRead + Unpin,
    F: Fn(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let mut set = tokio::task::JoinSet::new();
    read_lines(r, |line| {
        set.spawn(f(line));
    })
    .await;
    while set.join_next().await.is_some() {}
}

/// readLines: trimmed non-empty lines (scanner stops at a 16 MB line).
async fn read_lines<R: tokio::io::AsyncRead + Unpin>(r: R, mut f: impl FnMut(Vec<u8>)) {
    use tokio::io::AsyncBufReadExt;
    let mut rd = tokio::io::BufReader::with_capacity(64 * 1024, r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match rd.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if buf.len() > 16 << 20 {
            return;
        }
        let line = buf.trim_ascii();
        if !line.is_empty() {
            f(line.to_vec());
        }
    }
}

/// probe_mcp reports whether an agent-browser-use server already answers at url with the
/// current password. needAuth is true when one answers but rejected our password.
pub async fn probe_mcp(u: &str) -> (bool, bool) {
    let Ok(c) = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return (false, false);
    };
    let mut req = c
        .post(u)
        .header("Content-Type", "application/json")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    let pw = auth::password();
    if !pw.is_empty() {
        req = req.header("Authorization", format!("Bearer {pw}"));
    }
    let Ok(resp) = req.send().await else {
        return (false, false);
    };
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED
        && resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("agent-browser-use"))
    {
        return (false, true);
    }
    let ok = resp.status() == reqwest::StatusCode::OK
        && resp
            .json::<Value>()
            .await
            .is_ok_and(|v| v.is_object() && v.get("error").is_none_or(|e| e.is_null()));
    (ok, false)
}

/// proxyStdio forwards stdin messages to a running server and writes its answers to stdout.
/// It keeps the MCP session the server hands out on initialize, so this agent stays its own seat
/// (its own #id in the room) instead of merging with the other agents, and ends it on exit.
pub async fn proxy_stdio(u: &str) {
    golog(&format!(
        "{APP_NAME} is already running, forwarding MCP to {u}"
    ));
    let Ok(c) = reqwest::Client::builder()
        .timeout(Duration::from_secs(10 * 60))
        .build()
    else {
        return;
    };
    let out = Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
    let sid = Arc::new(Mutex::new(String::new()));
    // set once initialize has been answered (we know the session)
    let (ready_tx, ready_rx) = tokio::sync::watch::channel(false);
    let ready_tx = Arc::new(ready_tx);
    {
        let tx = ready_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(15)).await; // a client that never initializes
            tx.send_replace(true);
        });
    }
    let post = {
        let (c, sid, u) = (c.clone(), sid.clone(), u.to_string());
        move |method: Method, body: Vec<u8>| {
            let (c, sid, u) = (c.clone(), sid.clone(), u.clone());
            async move {
                let mut req = c
                    .request(method, &u)
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json, text/event-stream")
                    .body(body);
                let pw = auth::password();
                if !pw.is_empty() {
                    req = req.header("Authorization", format!("Bearer {pw}"));
                }
                let s = lock(&sid).clone();
                if !s.is_empty() {
                    req = req.header("Mcp-Session-Id", s);
                }
                req.send().await
            }
        }
    };
    dispatch_lines(tokio::io::stdin(), {
        let (post, sid, out, ready_tx, ready_rx) = (
            post.clone(),
            sid.clone(),
            out.clone(),
            ready_tx.clone(),
            ready_rx.clone(),
        );
        move |line| {
            let (post, sid, out, ready_tx, mut ready_rx) = (
                post.clone(),
                sid.clone(),
                out.clone(),
                ready_tx.clone(),
                ready_rx.clone(),
            );
            async move {
                let m: Value = serde_json::from_slice(&line).unwrap_or(Value::Null);
                let is_init = m.get("method").and_then(Value::as_str) == Some("initialize");
                if !is_init {
                    let _ = ready_rx.wait_for(|r| *r).await;
                }
                let resp = post(Method::POST, line.clone()).await;
                let mut body: Vec<u8> = Vec::new();
                match resp {
                    Err(e) => {
                        if let Some(id) = m.get("id") {
                            let r =
                                rpc_err(id.clone(), -32603, format!("server not reachable: {e}"));
                            body = go_json(&r).into_bytes();
                        }
                    }
                    Ok(resp) => {
                        if let Some(v) = resp
                            .headers()
                            .get("mcp-session-id")
                            .and_then(|v| v.to_str().ok())
                            .filter(|v| !v.is_empty())
                        {
                            *lock(&sid) = v.to_string();
                        }
                        let ok = resp.status() == reqwest::StatusCode::OK;
                        let b = resp.bytes().await.unwrap_or_default();
                        if ok {
                            body = b.to_vec();
                        }
                    }
                }
                if is_init {
                    ready_tx.send_replace(true);
                }
                let body = body.trim_ascii();
                if !body.is_empty() {
                    use tokio::io::AsyncWriteExt;
                    let mut b = body.to_vec();
                    b.push(b'\n');
                    let mut w = out.lock().await;
                    let _ = w.write_all(&b).await;
                    let _ = w.flush().await;
                }
            }
        }
    })
    .await;
    let have = !lock(&sid).is_empty();
    if have {
        // stdin closed: the agent quit, take it out of the room
        let _ = post(Method::DELETE, Vec::new()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_list_has_every_tool_in_go_field_order() {
        assert_eq!(TOOLS.len(), 36);
        let s = go_json(&ToolsList { tools: &TOOLS });
        assert!(s.starts_with(r#"{"tools":[{"name":"browser_navigate","title":"Navigate to a URL","description":"Open a URL"#));
        // escapes < and > in strings
        assert!(s.contains("\\u003cselect\\u003e"));
        assert!(s.contains(r#""annotations":{"readOnlyHint":true}"#));
    }

    #[test]
    fn every_tool_is_dispatched() {
        // a tool in the list that call_tool does not know would answer "unknown tool"
        let src = include_str!("mcp.rs");
        for t in TOOLS.iter() {
            assert!(
                src.contains(&format!("\"{}\"", t.name)),
                "{} is not handled",
                t.name
            );
        }
    }

    #[test]
    fn loopback_addresses() {
        assert!(is_loopback("127.0.0.1:5000"));
        assert!(is_loopback("[::1]:5000"));
        assert!(is_loopback("::1"));
        assert!(!is_loopback("localhost"));
        assert!(!is_loopback("192.168.1.2:80"));
    }

    #[tokio::test]
    async fn protocol_without_a_browser() {
        let s = McpServer::new(Hub::new(""));
        let c = McpConn::default();
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","clientInfo":{"name":"x"}}}"#;
        let r = go_json(&s.handle(init.as_bytes(), &c).await.unwrap());
        assert!(r.contains(r#""protocolVersion":"2024-11-05""#));
        assert!(r.contains("-no-browser"));
        let call = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_snapshot"}}"#;
        let r = go_json(&s.handle(call.as_bytes(), &c).await.unwrap());
        assert!(r.contains(r#""isError":true"#));
        assert!(s
            .handle(
                br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                &c
            )
            .await
            .is_none());
        let r = go_json(&s.handle(b"{not json", &c).await.unwrap());
        assert_eq!(
            r,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#
        );
    }
}
