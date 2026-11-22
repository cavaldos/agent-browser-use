//! HTTP API behind the viewer's Flows tab: list, read, rename, edit, delete, replay, export and
//! import recorded flows (see flow.rs). Every change is announced to all viewers with a "flows"
//! message, so their lists stay current; a replay is announced with "flow_run".

#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use serde::{Deserialize, Serialize};

use crate::agent::Agent;
use crate::flow::{
    cut, default_flow_name, flow_json, flow_path, hints, list_flows, load_flow, new_flow_id,
    save_flow, tidy, Flow, FlowInfo, FlowStep, MAX_FLOW_STEPS,
};
use crate::{go_json, Hub, Msg};

/// one replay from the viewer at a time
static FLOW_RUNNING: AtomicBool = AtomicBool::new(false);
/// a person asked the running replay to stop (checked between steps)
pub static FLOW_STOP: AtomicBool = AtomicBool::new(false);
/// `Hub.runningFlow`
static RUNNING_FLOW: Mutex<String> = Mutex::new(String::new());

/// flowRun tells every viewer that a flow replay started or ended (they show it, with a Stop button).
pub fn flow_run(hub: &Hub, id: &str, on: bool) {
    let st = hub.lock();
    *RUNNING_FLOW.lock().unwrap_or_else(|e| e.into_inner()) =
        if on { id.to_string() } else { String::new() };
    st.broadcast(
        &Msg {
            type_: "flow_run".into(),
            rec: Some(FlowInfo {
                id: id.to_string(),
                on,
                ..Default::default()
            }),
            ..Default::default()
        },
        None,
    );
}

/// flowsChanged tells every viewer to reload its flow list.
fn flows_changed(hub: &Hub) {
    let st = hub.lock();
    st.broadcast(
        &Msg {
            type_: "flows".into(),
            ..Default::default()
        },
        None,
    );
}

/// flowStepView is a step as the Flows tab shows it.
#[derive(Serialize)]
struct FlowStepView {
    #[serde(flatten)]
    step: FlowStep,
    label: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    hints: String,
}

/// validFlow checks a flow that came from outside (an import), and bounds what it holds.
fn valid_flow(f: &mut Flow) -> Result<(), String> {
    const KINDS: [&str; 15] = [
        "goto",
        "click",
        "dblclick",
        "rclick",
        "fill",
        "select",
        "key",
        "drag",
        "back",
        "forward",
        "reload",
        "tab_new",
        "tab_switch",
        "tab_close",
        "page",
    ];
    if f.steps.is_empty() {
        return Err("the flow has no steps".into());
    }
    if f.steps.len() > MAX_FLOW_STEPS {
        return Err("the flow has too many steps".into());
    }
    for (i, s) in f.steps.iter_mut().enumerate() {
        if !KINDS.contains(&s.kind.as_str()) {
            return Err(format!(
                "unknown step kind {}",
                crate::agent::go_quote(&s.kind)
            ));
        }
        s.target = tidy(s.target.take());
        s.to = tidy(s.to.take());
        let needs = matches!(
            s.kind.as_str(),
            "click" | "dblclick" | "rclick" | "fill" | "select" | "drag"
        );
        if (needs && s.target.is_none()) || (s.kind == "drag" && s.to.is_none()) {
            return Err(format!("step {} has no element", i + 1));
        }
        if s.kind == "key" {
            crate::agent::parse_key(&s.value)?;
        }
        if (s.kind == "goto" || s.kind == "page")
            && crate::browser::normalize_url(&s.url).is_empty()
        {
            return Err(format!("step {} has no valid address", i + 1));
        }
        s.value = cut(&s.value, 5000);
        s.url = cut(&s.url, 2000);
        if s.secret {
            s.value.clear();
        }
    }
    f.name = cut(&f.name, 80).trim().to_string();
    Ok(())
}

#[derive(Clone)]
struct Api {
    hub: Arc<Hub>,
    mcp: Arc<crate::mcp::McpServer>,
    /// `h.browser.opt.FlowDir`; None when no browser is running
    dir: Option<String>,
}

type Q = Query<HashMap<String, String>>;

fn http_error(status: StatusCode, msg: &str) -> Response {
    // http.Error
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header("x-content-type-options", "nosniff")
        .body(Body::from(format!("{msg}\n")))
        .unwrap()
}

/// writeJSON: json.NewEncoder(w).Encode(v)
fn write_json<T: Serialize>(v: &T) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!("{}\n", go_json(v))))
        .unwrap()
}

fn no_content() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap()
}

fn post_only(r: &Request) -> Option<Response> {
    if r.method() != axum::http::Method::POST {
        return Some(http_error(StatusCode::METHOD_NOT_ALLOWED, "POST only"));
    }
    None
}

fn qget(q: &Q, k: &str) -> String {
    q.get(k).cloned().unwrap_or_default()
}

/// Decodes the first JSON value of a body, like `json.NewDecoder(io.LimitReader(r.Body, n)).Decode`.
async fn decode_body<T: for<'de> Deserialize<'de>>(req: Request, limit: usize) -> Option<T> {
    let bytes = axum::body::to_bytes(req.into_body(), limit).await.ok()?;
    serde_json::Deserializer::from_slice(&bytes)
        .into_iter::<T>()
        .next()?
        .ok()
}

impl Api {
    fn dir(&self) -> Result<&str, Response> {
        self.dir
            .as_deref()
            .ok_or_else(|| http_error(StatusCode::SERVICE_UNAVAILABLE, "no browser is running"))
    }

    /// load reads the flow named by ?id=, answering 404 itself when there is none.
    fn load(&self, q: &Q) -> Result<(String, Flow), Response> {
        let d = self.dir()?.to_string();
        match load_flow(&d, &qget(q, "id")) {
            Ok(f) => Ok((d, f)),
            Err(e) => Err(http_error(StatusCode::NOT_FOUND, &e)),
        }
    }
}

/// registerFlowAPI
pub fn routes(hub: Arc<Hub>, mcp: Arc<crate::mcp::McpServer>, dir: Option<String>) -> Router {
    let api = Api { hub, mcp, dir };
    Router::new()
        .route("/api/flows", any(api_list))
        .route("/api/flows/get", any(api_get))
        .route("/api/flows/rename", any(api_rename))
        .route("/api/flows/step", any(api_step))
        .route("/api/flows/delete", any(api_delete))
        .route("/api/flows/export", any(api_export))
        .route("/api/flows/import", any(api_import))
        .route("/api/flows/run", any(api_run))
        .route("/api/flows/stop", any(api_stop))
        .with_state(api)
}

/// GET /api/flows -> { flows: [...], running: "flow-…" }
async fn api_list(State(a): State<Api>) -> Response {
    let d = match a.dir() {
        Ok(d) => d,
        Err(r) => return r,
    };
    #[derive(Serialize)]
    struct List {
        flows: Vec<FlowInfo>,
        running: String,
    }
    let running = RUNNING_FLOW
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    write_json(&List {
        flows: list_flows(d),
        running,
    })
}

/// GET /api/flows/get?id=… -> the whole flow, with a readable label for every step.
async fn api_get(State(a): State<Api>, q: Q) -> Response {
    let f = match a.load(&q) {
        Ok((_, f)) => f,
        Err(r) => return r,
    };
    let steps: Vec<FlowStepView> = f
        .steps
        .iter()
        .map(|s| {
            let mut v = FlowStepView {
                step: s.clone(),
                label: s.label(),
                hints: String::new(),
            };
            if s.kind != "page" {
                v.hints = hints(&s.target)
                    .trim()
                    .trim_matches(|c| c == '[' || c == ']')
                    .trim()
                    .to_string();
            }
            v
        })
        .collect();
    #[derive(Serialize)]
    struct Out {
        by: String,
        flow: FlowInfo,
        #[serde(rename = "startTitle")]
        start_title: String,
        steps: Vec<FlowStepView>,
    }
    write_json(&Out {
        by: f.by.clone(),
        flow: f.info(),
        start_title: f.start_title.clone(),
        steps,
    })
}

/// POST /api/flows/rename?id=…&name=…
async fn api_rename(State(a): State<Api>, q: Q, req: Request) -> Response {
    if let Some(r) = post_only(&req) {
        return r;
    }
    let (d, mut f) = match a.load(&q) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let name = cut(&qget(&q, "name"), 80).trim().to_string();
    if name.is_empty() {
        return http_error(StatusCode::BAD_REQUEST, "name is required");
    }
    f.name = name;
    if let Err(e) = save_flow(&d, &f) {
        return http_error(StatusCode::INTERNAL_SERVER_ERROR, &e);
    }
    flows_changed(&a.hub);
    no_content()
}

/// POST /api/flows/step?id=…&n=3&action=delete|value  (value: JSON body {"value": "…"})
async fn api_step(State(a): State<Api>, q: Q, req: Request) -> Response {
    if let Some(r) = post_only(&req) {
        return r;
    }
    let (d, mut f) = match a.load(&q) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let n = match qget(&q, "n").parse::<usize>() {
        Ok(n) if n >= 1 && n <= f.steps.len() => n,
        _ => return http_error(StatusCode::BAD_REQUEST, "no such step"),
    };
    match qget(&q, "action").as_str() {
        "delete" => {
            if f.steps.len() == 1 {
                return http_error(
                    StatusCode::BAD_REQUEST,
                    "a flow needs at least one step: delete the flow instead",
                );
            }
            f.steps.remove(n - 1);
        }
        "value" => {
            #[derive(Deserialize)]
            struct Body {
                #[serde(default)]
                value: Option<String>,
            }
            let Some(body) = decode_body::<Body>(req, 64 << 10).await else {
                return http_error(StatusCode::BAD_REQUEST, "send {\"value\": \"…\"}");
            };
            let value = body.value.unwrap_or_default();
            let s = &mut f.steps[n - 1];
            if s.kind == "fill" && !s.secret {
                s.value = cut(&value, 5000);
            } else if s.kind == "select" && !value.trim().is_empty() {
                s.values = vec![cut(&value, 200)];
            } else {
                return http_error(
                    StatusCode::BAD_REQUEST,
                    "only typed text (not passwords) and choices can be edited",
                );
            }
        }
        _ => return http_error(StatusCode::BAD_REQUEST, "action must be delete or value"),
    }
    if let Err(e) = save_flow(&d, &f) {
        return http_error(StatusCode::INTERNAL_SERVER_ERROR, &e);
    }
    flows_changed(&a.hub);
    no_content()
}

/// POST /api/flows/delete?id=…
async fn api_delete(State(a): State<Api>, q: Q, req: Request) -> Response {
    if let Some(r) = post_only(&req) {
        return r;
    }
    let d = match a.dir() {
        Ok(d) => d,
        Err(r) => return r,
    };
    let res = flow_path(d, &qget(&q, "id"))
        .and_then(|p| std::fs::remove_file(p).map_err(|_| "no such flow".to_string()));
    if let Err(e) = res {
        return http_error(StatusCode::NOT_FOUND, &e);
    }
    flows_changed(&a.hub);
    no_content()
}

/// GET /api/flows/export?id=… -> downloads the flow as a JSON file.
async fn api_export(State(a): State<Api>, q: Q) -> Response {
    let f = match a.load(&q) {
        Ok((_, f)) => f,
        Err(r) => return r,
    };
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&format!("attachment; filename=\"{}.json\"", f.id))
                .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
        )
        .body(Body::from(format!("{}\n", flow_json(&f))))
        .unwrap()
}

/// The content of the multipart field `name` (a minimal parser: the flow file is the only field we read).
fn multipart_field(body: &[u8], boundary: &str, name: &str) -> Option<Vec<u8>> {
    let delim = format!("--{boundary}").into_bytes();
    let find = |hay: &[u8], needle: &[u8], from: usize| -> Option<usize> {
        if needle.is_empty() || hay.len() < needle.len() {
            return None;
        }
        (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
    };
    let mut pos = find(body, &delim, 0)?;
    loop {
        let start = pos + delim.len();
        if body.get(start..start + 2) == Some(b"--") {
            return None;
        }
        let hdr_start = start + 2; // after the CRLF
        let hdr_end = find(body, b"\r\n\r\n", hdr_start)?;
        let headers = String::from_utf8_lossy(&body[hdr_start..hdr_end]).to_string();
        let data_start = hdr_end + 4;
        let next = find(body, &[b"\r\n".as_slice(), &delim].concat(), data_start)?;
        if headers.contains(&format!("name=\"{name}\"")) {
            return Some(body[data_start..next].to_vec());
        }
        pos = next + 2;
    }
}

/// POST /api/flows/import (multipart field "file", or a raw JSON body) -> {id}. Always gets a new id.
async fn api_import(State(a): State<Api>, req: Request) -> Response {
    if let Some(r) = post_only(&req) {
        return r;
    }
    let d = match a.dir() {
        Ok(d) => d.to_string(),
        Err(r) => return r,
    };
    let ct = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = axum::body::to_bytes(req.into_body(), 8 << 20)
        .await
        .unwrap_or_default();
    let data: Vec<u8> = if ct.starts_with("multipart/") {
        // an uploaded file; else the body is the JSON
        let boundary = ct
            .split(';')
            .filter_map(|p| p.trim().strip_prefix("boundary="))
            .next()
            .map(|b| b.trim_matches('"').to_string());
        match boundary.and_then(|b| multipart_field(&body, &b, "file")) {
            Some(d) => d,
            None => {
                return http_error(
                    StatusCode::BAD_REQUEST,
                    "send the flow file in the field \"file\"",
                )
            }
        }
    } else {
        body.to_vec()
    };
    let mut f: Flow = match serde_json::from_slice(&data) {
        Ok(f) => f,
        Err(e) => {
            return http_error(StatusCode::BAD_REQUEST, &format!("not a flow file: {e}"));
        }
    };
    if let Err(e) = valid_flow(&mut f) {
        return http_error(StatusCode::BAD_REQUEST, &format!("not a usable flow: {e}"));
    }
    f.id = new_flow_id(&d);
    if crate::flow::is_zero_time(&f.created) {
        f.created = crate::assets::now_rfc3339();
    }
    if f.name.is_empty() {
        f.name = default_flow_name(&f);
    }
    if let Err(e) = save_flow(&d, &f) {
        return http_error(StatusCode::INTERNAL_SERVER_ERROR, &e);
    }
    flows_changed(&a.hub);
    let mut m = HashMap::new();
    m.insert("id", f.id.clone());
    write_json(&m)
}

/// POST /api/flows/run {id, repeat, inputs} -> {log} once the replay is over (the agent does it, everyone watches).
async fn api_run(State(a): State<Api>, req: Request) -> Response {
    if let Some(r) = post_only(&req) {
        return r;
    }
    let Some(agent) = a.mcp.agent() else {
        return http_error(StatusCode::SERVICE_UNAVAILABLE, "no browser is running");
    };
    #[derive(Deserialize)]
    struct Body {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        repeat: Option<i64>,
        #[serde(default)]
        inputs: Option<HashMap<String, String>>,
    }
    let Some(body) = decode_body::<Body>(req, 256 << 10).await else {
        return http_error(StatusCode::BAD_REQUEST, "send {\"id\": \"flow-…\"}");
    };
    if FLOW_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return http_error(
            StatusCode::CONFLICT,
            "a flow is already running: wait for it or stop it",
        );
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            FLOW_RUNNING.store(false, Ordering::SeqCst);
        }
    }
    let _reset = Reset;
    a.hub.lock().human_last = None; // the person asked for it: the agent may take control right away
    #[derive(Serialize)]
    struct Out {
        log: String,
        ok: bool,
    }
    match run_flow_from_viewer(
        &agent,
        &body.id.unwrap_or_default(),
        body.repeat.unwrap_or(0),
        &body.inputs.unwrap_or_default(),
    )
    .await
    {
        Err(e) => write_json(&Out { log: e, ok: false }),
        Ok(log) => write_json(&Out { log, ok: true }),
    }
}

/// POST /api/flows/stop -> the running replay stops after its current step.
async fn api_stop(req: Request) -> Response {
    if let Some(r) = post_only(&req) {
        return r;
    }
    FLOW_STOP.store(true, Ordering::SeqCst);
    no_content()
}

/// runFlowFromViewer replays a flow as the agent, the way an MCP tool call would.
async fn run_flow_from_viewer(
    a: &Arc<Agent>,
    id: &str,
    repeat: i64,
    inputs: &HashMap<String, String>,
) -> Result<String, String> {
    let _gesture = a.b.lock_gesture("", "viewer").await?;
    let _mu = a.mu.clone().lock_owned().await;
    a.begin();
    struct Done(Arc<Hub>);
    impl Drop for Done {
        fn drop(&mut self) {
            self.0.agent_done();
        }
    }
    let _done = Done(a.hub.clone());
    a.run_flow(id, repeat, inputs, false).await
}
