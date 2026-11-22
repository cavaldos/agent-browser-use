//! HTTP API behind the Settings panel's saved sessions (states_http.rs): list, export (download a
//! JSON bundle), import (upload one), save, load and delete. They operate on the states folder on
//! disk; save, load and import also drive the shared browser, so they go through the agent.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use serde::Serialize;
use serde_json::json;

use crate::agent::Agent;
use crate::mcp::McpServer;
use crate::states::{build_bundle, list_states};

/// registerStateAPI wires the saved-session (login state) endpoints used by the Settings panel:
/// list, export (download a JSON bundle), import (upload one), load and delete. They operate on
/// the states folder on disk; load also drives the shared browser, so it needs the agent.
pub fn routes(mcp: Arc<McpServer>) -> Router {
    Router::new()
        .route("/api/states", any(api_states))
        .route("/api/states/export", any(api_export))
        .route("/api/states/import", any(api_import))
        .route("/api/states/save", any(api_save))
        .route("/api/states/load", any(api_load))
        .route("/api/states/delete", any(api_delete))
        .with_state(mcp)
}

/// `http.Error`: the message as plain text, one line.
fn http_error(status: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header("x-content-type-options", "nosniff")
        .body(Body::from(format!("{msg}\n")))
        .unwrap()
}

/// `writeJSON`: the value as JSON, followed by a newline (json.Encoder).
fn write_json(v: &impl Serialize) -> Response {
    let mut body = serde_json::to_vec(v).unwrap_or_default();
    body.push(b'\n');
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

/// `agent(w)`: the running agent, or None when no browser is running.
fn agent_of(mcp: &McpServer) -> Option<Arc<Agent>> {
    mcp.agent()
}

fn no_browser() -> Response {
    http_error(StatusCode::SERVICE_UNAVAILABLE, "no browser is running")
}

/// The first value of a query parameter (`r.URL.Query.Get`).
fn query_get(req: &Request, key: &str) -> String {
    req.uri()
        .query()
        .and_then(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        })
        .unwrap_or_default()
}

/// bodyName reads {"name": "..."} from a request body (empty when it is missing or broken).
async fn body_name(req: Request) -> String {
    #[derive(serde::Deserialize)]
    struct Body {
        name: Option<String>,
    }
    let bytes = to_bytes(req.into_body(), 1 << 16).await.unwrap_or_default();
    serde_json::from_slice::<Body>(&bytes)
        .ok()
        .and_then(|b| b.name)
        .unwrap_or_default()
}

/// trimJSON: the file name without its ".json" suffix.
fn trim_json(s: &str) -> String {
    s.strip_suffix(".json").unwrap_or(s).to_string()
}

/// GET /api/states -> { "states": [ {name, cookies, sites, origin, saved}, ... ] }
async fn api_states(State(mcp): State<Arc<McpServer>>, _req: Request) -> Response {
    let Some(a) = agent_of(&mcp) else {
        return no_browser();
    };
    write_json(&json!({ "states": list_states(&a.b.opt.state_dir) }))
}

/// GET /api/states/export?current=1 -> downloads every login of the running browser right now.
/// GET /api/states/export[?name=foo] -> downloads saved state(s) (all of them if no name).
async fn api_export(State(mcp): State<Arc<McpServer>>, req: Request) -> Response {
    let Some(a) = agent_of(&mcp) else {
        return no_browser();
    };
    let (bundle, prefix) = if !query_get(&req, "current").is_empty() {
        (a.live_bundle().await, "browser-sessions-")
    } else {
        (
            build_bundle(&a.b.opt.state_dir, &query_get(&req, "name")),
            "sessions-",
        )
    };
    let bundle = match bundle {
        Ok(b) => b,
        Err(e) => return http_error(StatusCode::NOT_FOUND, &e),
    };
    a.announce(
        "📤",
        &format!("Export {} session(s)", bundle.states.len()),
        None,
    );
    let fname = format!("{prefix}{}.json", export_stamp());
    let mut body = serde_json::to_vec_pretty(&bundle).unwrap_or_default();
    body.push(b'\n');
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{fname}\""),
        )
        .body(Body::from(body))
        .unwrap()
}

/// Local time as Format("20060102-150405").
fn export_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (y, mo, d, h, mi, s) = crate::chat::local_tm(secs);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// The file part "file" of a multipart/form-data body, as (file name, content). None when the
/// body is not multipart or has no such file part (FormFile failing).
fn multipart_file(body: &[u8], content_type: &str) -> Option<(String, Vec<u8>)> {
    let ct = content_type.trim();
    if !ct.to_ascii_lowercase().starts_with("multipart/form-data") {
        return None;
    }
    let boundary = ct.split(';').skip(1).find_map(|p| {
        let (k, v) = p.trim().split_once('=')?;
        if k.trim().eq_ignore_ascii_case("boundary") {
            Some(v.trim().trim_matches('"').to_string())
        } else {
            None
        }
    })?;
    let delim = format!("--{boundary}").into_bytes();
    let find = |hay: &[u8], needle: &[u8], from: usize| -> Option<usize> {
        if from > hay.len() {
            return None;
        }
        hay[from..]
            .windows(needle.len())
            .position(|w| w == needle)
            .map(|p| p + from)
    };
    let mut pos = find(body, &delim, 0)? + delim.len();
    loop {
        if body[pos..].starts_with(b"--") {
            return None;
        }
        let start = if body[pos..].starts_with(b"\r\n") {
            pos + 2
        } else {
            pos
        };
        let next = find(body, &delim, start)?;
        let part = &body[start..next];
        let part = part.strip_suffix(b"\r\n").unwrap_or(part);
        if let Some(h) = find(part, b"\r\n\r\n", 0) {
            let headers = String::from_utf8_lossy(&part[..h]).into_owned();
            for line in headers.lines() {
                let Some((k, v)) = line.split_once(':') else {
                    continue;
                };
                if !k.trim().eq_ignore_ascii_case("content-disposition") {
                    continue;
                }
                let mut field = None;
                let mut filename = None;
                for p in v.split(';').skip(1) {
                    if let Some((pk, pv)) = p.trim().split_once('=') {
                        let pv = pv.trim().trim_matches('"').to_string();
                        match pk.trim() {
                            "name" => field = Some(pv),
                            "filename" => filename = Some(pv),
                            _ => {}
                        }
                    }
                }
                if let (Some(f), Some(fname)) = (field, filename) {
                    if f == "file" {
                        return Some((fname, part[h + 4..].to_vec()));
                    }
                }
            }
        }
        pos = next + delim.len();
    }
}

/// POST /api/states/import -> upload a sessions file (multipart field "file", or a raw JSON body).
/// Every session in it is saved and loaded into the browser right away.
async fn api_import(State(mcp): State<Arc<McpServer>>, req: Request) -> Response {
    let Some(a) = agent_of(&mcp) else {
        return no_browser();
    };
    if req.method() != Method::POST {
        return http_error(StatusCode::METHOD_NOT_ALLOWED, "use POST");
    }
    let ct = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let raw = to_bytes(req.into_body(), 8 << 20).await.unwrap_or_default();
    let (data, fallback) = match multipart_file(&raw, &ct) {
        Some((fname, bytes)) => (bytes, trim_json(&fname)),
        None => (raw.to_vec(), "imported".to_string()),
    };
    if data.is_empty() {
        return http_error(StatusCode::BAD_REQUEST, "no file uploaded");
    }
    let (names, outcome) = match a.import_and_apply(&data, &fallback).await {
        Ok(v) => v,
        Err(e) => return http_error(StatusCode::BAD_REQUEST, &e), // nothing saved
    };
    a.announce("📥", &format!("Import {} session(s)", names.len()), None);
    match outcome {
        // saved, but the browser did not take it
        Err(e) => write_json(&json!({ "imported": names, "loaded": false, "error": e })),
        Ok(msg) => write_json(&json!({ "imported": names, "loaded": true, "message": msg })),
    }
}

/// POST /api/states/save {"name":"foo"} -> capture the browser's current login under a name.
async fn api_save(State(mcp): State<Arc<McpServer>>, req: Request) -> Response {
    let Some(a) = agent_of(&mcp) else {
        return no_browser();
    };
    let name = body_name(req).await;
    match a.state_tool("save", &name, "").await {
        Ok(out) => write_json(&json!({ "ok": true, "message": out })),
        Err(e) => http_error(StatusCode::BAD_REQUEST, &e),
    }
}

/// POST /api/states/load {"name":"foo"} -> restore a session into the shared browser.
async fn api_load(State(mcp): State<Arc<McpServer>>, req: Request) -> Response {
    let Some(a) = agent_of(&mcp) else {
        return no_browser();
    };
    let name = body_name(req).await;
    match a.state_tool("load", &name, "").await {
        Ok(out) => write_json(&json!({ "ok": true, "message": out })),
        Err(e) => http_error(StatusCode::BAD_REQUEST, &e),
    }
}

/// POST /api/states/delete {"name":"foo"} -> remove a saved session file.
async fn api_delete(State(mcp): State<Arc<McpServer>>, req: Request) -> Response {
    let Some(a) = agent_of(&mcp) else {
        return no_browser();
    };
    let name = body_name(req).await;
    match a.state_tool("delete", &name, "").await {
        Ok(_) => write_json(&json!({ "ok": true })),
        Err(e) => http_error(StatusCode::BAD_REQUEST, &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipart_file_part_is_found() {
        let body = b"--XYZ\r\nContent-Disposition: form-data; name=\"other\"\r\n\r\nno\r\n--XYZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"mine.json\"\r\nContent-Type: application/json\r\n\r\n{\"a\":1}\r\n--XYZ--\r\n";
        let (name, data) = multipart_file(body, "multipart/form-data; boundary=XYZ").unwrap();
        assert_eq!(name, "mine.json");
        assert_eq!(data, b"{\"a\":1}");
    }

    #[test]
    fn raw_body_is_not_multipart() {
        assert!(multipart_file(b"{}", "application/json").is_none());
        assert!(multipart_file(b"--XYZ--\r\n", "multipart/form-data; boundary=XYZ").is_none());
    }

    #[test]
    fn trim_json_drops_the_suffix() {
        assert_eq!(trim_json("sessions.json"), "sessions");
        assert_eq!(trim_json("sessions"), "sessions");
    }
}
