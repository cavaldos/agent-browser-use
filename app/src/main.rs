// Agent Browser Use server: command-line flags, paths, the HTTP server (static UI, /ws hub),
// the shared browser, the agent, MCP and the /api/* routes.
//
// The modules plug in through `BrowserApi` / `HubHooks` / `build_router`.
#![allow(dead_code)]
// Module docs are plain comments whose list layout is not markdown-clean.
#![allow(clippy::doc_lazy_continuation, clippy::doc_overindented_list_items)]

#[path = "agent/agent.rs"]
mod agent;
#[path = "recording/assets.rs"]
mod assets;
#[path = "system/auth.rs"]
mod auth;
#[path = "browser/browser.rs"]
mod browser;
#[path = "browser/cdp.rs"]
mod cdp;
#[path = "agent/chat.rs"]
mod chat;
#[path = "browser/chromecmd.rs"]
mod chromecmd;
#[path = "browser/devtools.rs"]
mod devtools;
#[path = "browser/emulate.rs"]
mod emulate;
#[path = "browser/engine.rs"]
mod engine;
#[path = "recording/files.rs"]
mod files;
#[path = "system/fileserver.rs"]
mod fileserver;
#[path = "recording/flow.rs"]
mod flow;
#[path = "recording/flow_http.rs"]
mod flow_http;
#[path = "browser/frames.rs"]
mod frames;
#[path = "network/keepalive.rs"]
mod keepalive;
#[path = "agent/mcp.rs"]
mod mcp;
#[path = "system/menu.rs"]
mod menu;
#[path = "network/netproxy.rs"]
mod netproxy;
#[path = "browser/observe.rs"]
mod observe;
#[path = "agent/pick.rs"]
mod pick;
#[path = "system/proc.rs"]
mod proc;
#[path = "system/prompt.rs"]
mod prompt;
#[path = "browser/share.rs"]
mod share;
#[path = "recording/states.rs"]
mod states;
#[path = "recording/states_http.rs"]
mod states_http;
#[path = "browser/stealth.rs"]
mod stealth;
#[path = "agent/tools.rs"]
mod tools;
#[path = "network/tunnel.rs"]
mod tunnel;
#[path = "system/update.rs"]
mod update;
#[path = "network/warp.rs"]
mod warp;

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

/// The web UI (`static/`), embedded in the binary.
#[derive(rust_embed::RustEmbed)]
#[folder = "static"]
pub struct StaticFiles;

/// mcp.rs: `const appName`.
pub const APP_NAME: &str = "agent-browser-use";
/// prompt.rs: `const passwordEnv`.
pub const PASSWORD_ENV: &str = "AGENT_BROWSER_USE_PASSWORD";
/// Env fallback of `-proxy`.
pub const PROXY_ENV: &str = "AGENT_BROWSER_USE_PROXY";

/// Set at build time with AGENT_BROWSER_USE_VERSION=vX.Y.Z.
pub const VERSION: &str = match option_env!("AGENT_BROWSER_USE_VERSION") {
    Some(v) => v,
    None => "dev",
};

// ---------- command line ----------

/// Every flag of main.rs, with the same names, defaults and meaning.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub port: i64,
    pub pw: String,
    pub data: String,
    pub no_browser: bool,
    pub no_audio: bool,
    pub width: i64,
    pub height: i64,
    pub quality: i64,
    pub max_tabs: i64,
    pub start: String,
    pub home: String,
    pub profile: String,
    pub chrome: String,
    pub proxy: String,
    pub engine: String,
    pub downloads: String,
    pub uploads: String,
    pub states: String,
    pub no_devtools: bool,
    pub flows: String,
    pub agent_speed: f64,
    pub open: bool,
    pub agent_idle: Duration,
    pub no_mcp: bool,
    pub mcp_remote: bool,
    pub tunnel: bool,
    pub subdomain: String,
    pub tunnel_backend: String,
    pub version: bool,
    pub update: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Val {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Dur(Duration),
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Bool,
    Int,
    Float,
    Str,
    Dur,
}

struct Spec {
    name: &'static str,
    /// Flags that share a target (-p/-port) write the same value.
    target: &'static str,
    kind: Kind,
    usage: String,
    def: Val,
}

fn spec(name: &'static str, kind: Kind, def: Val, usage: &str) -> Spec {
    Spec {
        name,
        target: name,
        kind,
        usage: usage.to_string(),
        def,
    }
}

fn alias(name: &'static str, target: &'static str, kind: Kind, def: Val, usage: &str) -> Spec {
    Spec {
        name,
        target,
        kind,
        usage: usage.to_string(),
        def,
    }
}

/// The flag table, in the order main.rs registers them. `env` supplies the -proxy default.
fn specs(env: &dyn Fn(&str) -> Option<String>) -> Vec<Spec> {
    use Kind::*;
    let s = |x: &str| Val::Str(x.to_string());
    let i = Val::Int;
    let b = Val::Bool;
    vec![
        spec("port", Int, i(3000), "HTTP port"),
        alias("p", "port", Int, i(3000), "HTTP port (short for -port)"),
        spec("pw", Str, s(""), "password needed to open the web UI and use /mcp (empty = no password)"),
        spec("data", Str, s(""), "file to keep the chat history in across restarts (empty = memory only)"),
        spec("no-browser", Bool, b(false), "disable the shared browser"),
        spec("no-audio", Bool, b(false), "disable shared browser audio"),
        spec("width", Int, i(1280), "shared screen width"),
        spec("height", Int, i(720), "shared screen height"),
        spec("quality", Int, i(92), "image quality 10-100 (lower = less bandwidth)"),
        spec("max-tabs", Int, i(12), "maximum number of open tabs"),
        spec("start", Str, s("https://www.google.com"), "page for new tabs"),
        spec("home", Str, s(""), "one folder for everything the app stores: browser profile, downloads, uploads, states, flows, warp.json, log (empty = \"log\" in the current folder; ~/.agent-browser-use for the mcp command)"),
        spec("profile", Str, s(""), "folder for the shared browser's cookies/logins (empty = \"chrome-profile\" inside the home folder)"),
        spec("chrome", Str, s(""), "path to the browser executable (empty = auto-detect)"),
        spec("proxy", Str, Val::Str(env(PROXY_ENV).unwrap_or_default()), "upstream SOCKS5 proxy for the shared browser only, e.g. socks5://127.0.0.1:40000 (empty = built-in Cloudflare WARP); the header switch turns it on/off"),
        spec("engine", Str, s("auto"), "browser to run: chromium, chrome, edge, brave, vivaldi, headless-shell (lightest); auto = Chromium, else the first one installed"),
        spec("downloads", Str, s(""), "folder for downloaded files (empty = \"downloads\" next to the profile folder)"),
        spec("uploads", Str, s(""), "folder the agent may upload files from (empty = \"uploads\" next to the profile folder)"),
        spec("states", Str, s(""), "folder for saved logins/sessions (empty = \"states\" next to the profile folder)"),
        spec("no-devtools", Bool, b(false), "do not open Chrome's local debugging port (turns off Inspect in the viewer)"),
        spec("flows", Str, s(""), "folder for recorded flows (empty = \"flows\" next to the profile folder)"),
        spec("agent-speed", Float, Val::Float(0.0), "pace of the AI agent's cursor and typing, for people who want to follow it: 0 instant, 1 normal, 2 slower"),
        spec("open", Bool, b(true), "open the web UI in your browser when the agent starts and nobody is watching"),
        spec("agent-idle", Dur, Val::Dur(Duration::from_secs(15)), "give control back (to whoever had it before) after the agent has been idle this long (0 = never)"),
        spec("no-mcp", Bool, b(false), "disable the MCP endpoint /mcp"),
        spec("mcp-remote", Bool, b(false), "allow /mcp from other machines (default: this machine only)"),
        spec("tunnel", Bool, b(false), "share the web UI on a public https URL (built-in Cloudflare tunnel, no router setup); sets a password if you gave none"),
        spec("subdomain", Str, s(""), "with -tunnel: name for the public URL (empty = random)"),
        spec("tunnel-backend", Str, s(""), "with -tunnel: tunnel backend URL (empty = https://api.nport.link)"),
        spec("version", Bool, b(false), "print the version and exit"),
        alias("v", "version", Bool, b(false), "print the version and exit (short for -version)"),
        spec("update", Bool, b(false), "update this program to the latest release and exit"),
        alias("u", "update", Bool, b(false), "update to the latest release (short for -update)"),
    ]
}

/// What the command line asks for.
#[derive(Debug, PartialEq)]
pub struct Parsed {
    /// `agent-browser-use mcp …`: MCP on stdin/stdout, so stdout carries only MCP messages.
    pub stdio: bool,
    pub config: Config,
    /// Arguments left after the flags (fl.Args).
    pub rest: Vec<String>,
}

/// Why parsing stopped.
#[derive(Debug, PartialEq)]
pub enum Exit {
    /// -h / -help: usage on stderr, exit 0.
    Help,
    /// Bad flag: message, then usage on stderr, exit 2.
    Bad(String),
}

/// strconv.ParseInt(s, 0, 64).
fn parse_go_int(s: &str) -> Option<i64> {
    let (neg, t) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let (radix, digits) = if let Some(r) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        (16, r)
    } else if let Some(r) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        (2, r)
    } else if let Some(r) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        (8, r)
    } else if t.len() > 1 && t.starts_with('0') {
        (8, &t[1..])
    } else {
        (10, t)
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return None;
    }
    let digits: String = digits.chars().filter(|c| *c != '_').collect();
    let n = i128::from_str_radix(&digits, radix).ok()?;
    let n = if neg { -n } else { n };
    i64::try_from(n).ok()
}

/// time.ParseDuration ("15s", "1m30s", "1.5h", "0"; negative allowed).
fn parse_go_duration(s: &str) -> Option<Duration> {
    let (neg, mut r) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    if r == "0" {
        return Some(Duration::ZERO);
    }
    if r.is_empty() {
        return None;
    }
    let mut total = 0f64;
    while !r.is_empty() {
        let end = r
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(r.len());
        if end == 0 {
            return None;
        }
        let num: f64 = r[..end].parse().ok()?;
        r = &r[end..];
        let uend = r
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(r.len());
        let unit = match &r[..uend] {
            "ns" => 1e-9,
            "us" | "µs" | "μs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        r = &r[uend..];
        total += num * unit;
    }
    // A negative duration is accepted; a Duration here cannot be negative, so clamp to 0.
    Some(if neg {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(total)
    })
}

/// Duration.String for the values we print (>= 1s or 0).
fn go_duration_string(d: Duration) -> String {
    let secs = d.as_secs();
    if d.is_zero() {
        return "0s".into();
    }
    if d.subsec_nanos() != 0 || secs == 0 {
        return humantime::format_duration(d).to_string();
    }
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m{s}s"),
        _ => format!("{h}h{m}m{s}s"),
    }
}

/// %q for the default of a string flag.
fn go_quote(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                o.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn is_zero(v: &Val) -> bool {
    match v {
        Val::Bool(b) => !b,
        Val::Int(i) => *i == 0,
        Val::Float(f) => *f == 0.0,
        Val::Str(s) => s.is_empty(),
        Val::Dur(d) => d.is_zero(),
    }
}

fn show_default(v: &Val) -> String {
    match v {
        Val::Bool(b) => b.to_string(),
        Val::Int(i) => i.to_string(),
        Val::Float(f) => f.to_string(),
        Val::Str(s) => go_quote(s),
        Val::Dur(d) => go_duration_string(*d),
    }
}

/// The text of `-h`: the usage header followed by the flag defaults.
pub fn usage_text(env: &dyn Fn(&str) -> Option<String>) -> String {
    let a = APP_NAME;
    let mut out = format!(
        "Usage:\n  {a} [flags]       web UI + MCP over HTTP at /mcp\n  {a} mcp [flags]   MCP over stdio (also serves the web UI)\n\nFlags:\n"
    );
    let mut all = specs(env);
    all.sort_by(|x, y| x.name.cmp(y.name));
    for s in &all {
        let mut line = format!("  -{}", s.name);
        let tname = match s.kind {
            Kind::Bool => "",
            Kind::Int => "int",
            Kind::Float => "float",
            Kind::Str => "string",
            Kind::Dur => "duration",
        };
        if !tname.is_empty() {
            line.push(' ');
            line.push_str(tname);
        }
        // b.Len <= 4 means a one-letter flag without a type name; the usage stays on the line.
        if line.len() <= 4 {
            line.push('\t');
        } else {
            line.push_str("\n    \t");
        }
        line.push_str(&s.usage.replace('\n', "\n    \t"));
        if !is_zero(&s.def) {
            line.push_str(&format!(" (default {})", show_default(&s.def)));
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Parse like flag package (ExitOnError): -name, --name, -name=value, -name value; bool flags
/// take only -name or -name=value; parsing stops at the first non-flag argument or after "--".
pub fn parse_args(
    args: &[String],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(Config, Vec<String>), Exit> {
    let all = specs(env);
    let mut vals: HashMap<&'static str, Val> = HashMap::new();
    for s in &all {
        vals.entry(s.target).or_insert_with(|| s.def.clone());
    }
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.len() < 2 || !a.starts_with('-') {
            break; // a lone "-" or a plain word: the flags are over
        }
        let mut name = &a[1..];
        if name.starts_with('-') {
            name = &name[1..];
            if name.is_empty() {
                i += 1; // "--" ends the flags
                break;
            }
        }
        if name.starts_with('-') || name.starts_with('=') {
            return Err(Exit::Bad(format!("bad flag syntax: {a}")));
        }
        i += 1;
        let (name, inline) = match name.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (name, None),
        };
        let Some(sp) = all.iter().find(|s| s.name == name) else {
            if name == "help" || name == "h" {
                return Err(Exit::Help);
            }
            return Err(Exit::Bad(format!("flag provided but not defined: -{name}")));
        };
        let val = if sp.kind == Kind::Bool {
            match inline.as_deref() {
                None => Val::Bool(true),
                Some("1" | "t" | "T" | "true" | "TRUE" | "True") => Val::Bool(true),
                Some("0" | "f" | "F" | "false" | "FALSE" | "False") => Val::Bool(false),
                Some(_) => {
                    return Err(Exit::Bad(format!(
                        "invalid boolean value {} for -{name}: parse error",
                        go_quote(&inline.unwrap())
                    )))
                }
            }
        } else {
            let raw = match inline {
                Some(v) => v,
                None if i < args.len() => {
                    i += 1;
                    args[i - 1].clone()
                }
                None => return Err(Exit::Bad(format!("flag needs an argument: -{name}"))),
            };
            let bad = || {
                Exit::Bad(format!(
                    "invalid value {} for flag -{name}: parse error",
                    go_quote(&raw)
                ))
            };
            match sp.kind {
                Kind::Int => Val::Int(parse_go_int(&raw).ok_or_else(bad)?),
                Kind::Float => Val::Float(raw.parse::<f64>().map_err(|_| bad())?),
                Kind::Dur => Val::Dur(parse_go_duration(&raw).ok_or_else(bad)?),
                _ => Val::Str(raw),
            }
        };
        vals.insert(sp.target, val);
    }
    let rest = args[i..].to_vec();
    let int = |k: &str| match &vals[k] {
        Val::Int(v) => *v,
        _ => unreachable!(),
    };
    let st = |k: &str| match &vals[k] {
        Val::Str(v) => v.clone(),
        _ => unreachable!(),
    };
    let bo = |k: &str| matches!(&vals[k], Val::Bool(true));
    let mut cfg = Config {
        port: int("port"),
        pw: st("pw"),
        data: st("data"),
        no_browser: bo("no-browser"),
        no_audio: bo("no-audio"),
        width: int("width"),
        height: int("height"),
        quality: int("quality"),
        max_tabs: int("max-tabs"),
        start: st("start"),
        home: st("home"),
        profile: st("profile"),
        chrome: st("chrome"),
        proxy: st("proxy"),
        engine: st("engine"),
        downloads: st("downloads"),
        uploads: st("uploads"),
        states: st("states"),
        no_devtools: bo("no-devtools"),
        flows: st("flows"),
        agent_speed: match &vals["agent-speed"] {
            Val::Float(f) => *f,
            _ => unreachable!(),
        },
        open: bo("open"),
        agent_idle: match &vals["agent-idle"] {
            Val::Dur(d) => *d,
            _ => unreachable!(),
        },
        no_mcp: bo("no-mcp"),
        mcp_remote: bo("mcp-remote"),
        tunnel: bo("tunnel"),
        subdomain: st("subdomain"),
        tunnel_backend: st("tunnel-backend"),
        version: bo("version"),
        update: bo("update"),
    };
    if cfg.pw.is_empty() {
        // mcp.json can set it with "env": {"AGENT_BROWSER_USE_PASSWORD": "..."}
        cfg.pw = env(PASSWORD_ENV).unwrap_or_default();
    }
    Ok((cfg, rest))
}

/// Split off the `mcp` subcommand (only when it is the first argument), then parse the flags.
pub fn parse_command_line(
    argv: &[String],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Parsed, Exit> {
    let stdio = argv.first().map(String::as_str) == Some("mcp");
    let args = if stdio { &argv[1..] } else { argv };
    let (config, rest) = parse_args(args, env)?;
    Ok(Parsed {
        stdio,
        config,
        rest,
    })
}

// ---------- Data ----------

/// `omitempty` helpers for serde.
pub fn is_false(b: &bool) -> bool {
    !*b
}
pub fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}
pub fn is_zero_f64(n: &f64) -> bool {
    *n == 0.0
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct User {
    pub id: String,
    pub name: String,
    pub color: String,
    /// the AI agent (MCP), not a person
    #[serde(default, skip_serializing_if = "is_false")]
    pub agent: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ChatMsg {
    pub user: String,
    pub color: String,
    pub text: String,
    pub time: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub agent: bool,
    /// order in the room; coding agents read from here
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub id: i64,
    /// addressed to every agent with @agent
    #[serde(default, rename = "toAgent", skip_serializing_if = "is_false")]
    pub to_bot: bool,
    /// addressed to these agents, e.g. "opencode"
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<String>,
}

/// JSON message between the web page and the server. Field order and `omitempty` as serialised.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Msg {
    #[serde(default, rename = "type")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub x: f64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub y: f64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub on: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub you: Option<User>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<User>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<User>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chat: Vec<ChatMsg>,
    /// handles of the connected coding agents (a pointer in `[]` is kept, `None` is omitted)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<ChatMsg>,

    // Shared browser
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    /// who is in control
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctrl: Option<User>,
    /// who blocked the agent from taking control (None = not blocked)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<User>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<browser::TabInfo>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub active: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<browser::View>,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub index: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub area: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub event: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub button: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub buttons: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub count: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub mods: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub code: String,
    #[serde(default, rename = "keyCode", skip_serializing_if = "is_zero_i64")]
    pub key_code: i64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub dx: f64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub dy: f64,

    /// Picked page element (type "pick", Event hover|click)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pick: Option<pick::Pick>,

    /// Flow recording (type "rec": live state; "rec_done": the saved flow)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rec: Option<flow::FlowInfo>,

    /// What the AI agent is doing (type "agent"): step, click, key, say
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub act: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<agent::Rect>,
    #[serde(default, rename = "tabId", skip_serializing_if = "String::is_empty")]
    pub tab_id: String,
    #[serde(default, rename = "workerId", skip_serializing_if = "String::is_empty")]
    pub worker_id: String,
}

/// encoding/json escapes <, >, & and U+2028/2029 inside strings (they can only occur inside
/// strings in serde_json's output), so the wire and the -data file stay byte-identical to 's.
pub fn go_escape(s: String) -> String {
    if !s.contains(['<', '>', '&', '\u{2028}', '\u{2029}']) {
        return s;
    }
    s.replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// json.Marshal
pub fn go_json<T: Serialize>(v: &T) -> String {
    go_escape(serde_json::to_string(v).unwrap_or_default())
}

/// A queued frame for one client: text (JSON) or binary (audio).
#[derive(Clone)]
pub struct OutMsg {
    pub bin: bool,
    pub data: Bytes,
}

// ---------- Hub and clients: the concurrency design ----------
//
// one mutex guards all hub state; every client has a `send` queue (512) and a
// one-slot `frame` slot, drained by a writer task; a reader task feeds `Hub.handle`.
//
// Rust keeps the same shape:
//   * `Hub.state: std::sync::Mutex<HubState>` guards the same fields. It is a *std* mutex on purpose:
//     the critical sections are short, purely in-memory (the -data save is a small sync file write) and contain no `.await`; the guard is not `Send`, so the compiler rejects holding it across an
//     await. Nothing on a socket is ever awaited under it.
//   * A client = `Arc<Client>`; its socket is owned by two tasks: the reader task (`run_client`) and a
//     writer task. The hub reaches a client only through `Client::send` (a bounded `tokio::mpsc`, 512,
//     `try_send` only, never awaited), `Client::frame` (a `watch` slot: newest frame wins) and
//     `Client::kill` (a `watch<bool>` both tasks observe). So a slow socket can never block the hub.
//   * Queue policy: `try_send_best_effort` drops when the queue is at least half full (cursor
//     broadcasts, audio); `send_important` disconnects the client when the queue is full (it reconnects and
//     gets a fresh `init`).
//   * The writer task selects (biased: queued messages first, so `init` always precedes the first frame)
//     over the queue, the frame slot, the 30 s ping and the kill flag; every write has a 10 s deadline. The
//     reader enforces the 64 KiB message limit and the 60 s read deadline (extended by pongs only).
//   * Lock order: hub state before anything the hooks own. `BrowserApi`
//     methods and `HubHooks` are called with the state lock held, and
//     must not block or call back into the hub synchronously; deferred work goes through `tokio::spawn`
//     and re-enters via `Hub::send_to` etc. after the lock is released.

pub const SEND_QUEUE: usize = 512;

pub struct Client {
    user: Mutex<User>,
    /// JSON messages and audio, in order
    send: mpsc::Sender<OutMsg>,
    /// only the newest frame; slow connections skip old ones
    frame: watch::Sender<Option<Bytes>>,
    /// set when the connection must close (`c.conn.Close`)
    kill: watch::Sender<bool>,
    /// guarded by Hub.state
    flags: Mutex<ClientFlags>,
}

#[derive(Default)]
struct ClientFlags {
    audio: bool,
    paused: bool,
    last_emote: Option<Instant>,
}

/// The receiving ends, owned by the writer task.
pub struct ClientRx {
    pub send: mpsc::Receiver<OutMsg>,
    pub frame: watch::Receiver<Option<Bytes>>,
    pub kill: watch::Receiver<bool>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Client {
    pub fn new(user: User, queue: usize) -> (Arc<Client>, ClientRx) {
        let (send, send_rx) = mpsc::channel(queue);
        let (frame, frame_rx) = watch::channel(None);
        let (kill, kill_rx) = watch::channel(false);
        let c = Arc::new(Client {
            user: Mutex::new(user),
            send,
            frame,
            kill,
            flags: Mutex::new(ClientFlags::default()),
        });
        (
            c,
            ClientRx {
                send: send_rx,
                frame: frame_rx,
                kill: kill_rx,
            },
        )
    }

    /// The AI agent: it has no connection and is never broadcast to.
    pub fn detached(user: User) -> Arc<Client> {
        Client::new(user, 1).0
    }

    pub fn user(&self) -> User {
        lock(&self.user).clone()
    }

    pub fn set_user(&self, u: User) {
        *lock(&self.user) = u;
    }

    /// len(c.send)
    fn queued(&self) -> usize {
        self.send.max_capacity() - self.send.capacity()
    }

    /// trySend is for best-effort data (cursor moves, sound). It only queues while the
    /// client has at least half its queue free, so a slow client can't fill the queue
    /// and push out the messages that matter.
    pub fn try_send_best_effort(&self, m: OutMsg) {
        if self.queued() >= self.send.max_capacity() / 2 {
            return; // client too slow, drop
        }
        let _ = self.send.try_send(m);
    }

    /// sendImportant is for messages the page must not miss: people joining or leaving,
    /// control, chat, tabs. If the client is so far behind that its queue is full, it is
    /// disconnected; the page then reconnects and gets a full snapshot (init) again.
    pub fn send_important(&self, m: OutMsg) {
        if self.send.try_send(m).is_err() {
            golog(&format!(
                "~ {} is too far behind, reconnecting it",
                self.user().name
            ));
            self.kill();
        }
    }

    /// pushFrame: drop the old frame that hasn't gone out yet, queue the new one.
    pub fn push_frame(&self, pkt: Bytes) {
        self.frame.send_replace(Some(pkt));
    }

    /// `c.conn.Close`.
    pub fn kill(&self) {
        self.kill.send_replace(true);
    }

    fn killed(&self) -> bool {
        *self.kill.borrow()
    }
}

/// Hooks the later phases fill in. Every
/// method has a no-op default so the Phase 1 server runs without them. Called with the state lock held,
/// see the design note above.
pub trait HubHooks: Send + Sync {
    /// flow.rs `recStart`
    fn rec_start(&self, _st: &mut HubState, _c: &Arc<Client>) {}
    /// flow.rs `recStop(c, discard)`; `None` = nobody (the recorder left)
    fn rec_stop(&self, _st: &mut HubState, _c: Option<&Arc<Client>>, _discard: bool) {}
    /// flow.rs `recInput(m)`
    fn rec_input(&self, _st: &mut HubState, _m: &Msg) {}
    /// flow.rs `recInfo` when `h.rec != nil` (the `init` message carries it)
    fn rec_info(&self, _st: &HubState) -> Option<flow::FlowInfo> {
        None
    }
    /// flow.rs `h.rec != nil && h.rec.by == c`
    fn rec_by(&self, _st: &HubState, _c: &Arc<Client>) -> bool {
        false
    }
    /// agent.rs `agentRelease`: give control back to whoever had it
    fn agent_release(&self, _st: &mut HubState) {}
    /// pick.rs `go h.pick(c, m)`: must spawn, not block
    fn pick(&self, _hub: &Arc<Hub>, _c: &Arc<Client>, _m: Msg) {}
    /// assets.rs `h.assets.add(Asset{Kind: "copy", ...})` for text copied in the page
    fn asset_copy(
        &self,
        _hub: &Arc<Hub>,
        _text: String,
        _url: String,
        _title: String,
        _by: String,
    ) {
    }
}

/// Everything keeps under `Hub.mu`.
pub struct HubState {
    pub clients: Vec<Arc<Client>>,
    pub chat: Vec<ChatMsg>,
    pub data_file: String,
    /// who controls the shared browser
    pub ctrl: Option<Arc<Client>>,

    /// the AI agent: has no connection, shown in the room once it acts
    pub agent: Arc<Client>,
    pub agent_on: bool,
    pub agent_last: Option<Instant>,
    /// a tool call is running (it may be waiting for the page): not idle
    pub agent_busy: bool,
    /// the person the agent took control from; gets it back when the agent is done
    pub agent_from: Option<Arc<Client>>,
    /// set while someone has blocked the agent from taking control
    pub blocked: Option<User>,
    pub chat_seq: i64,
    /// connected coding agents, by @handle
    pub seats: chat::Seats,
    /// the agent list everyone has
    pub seats_sent: Vec<String>,
    /// bumped when a message for the agent arrives
    pub chat_wake: watch::Sender<u64>,
    /// last time the person in control sent input (the agent waits while they are active)
    pub human_last: Option<Instant>,
    // TODO: assets, picks, pick_order, rec, running_flow (Hub fields owned by assets.rs,
    // pick.rs, flow.rs): those modules keep their own state behind `HubHooks` for now.
}

impl HubState {
    fn ptr_eq(a: &Option<Arc<Client>>, b: &Arc<Client>) -> bool {
        a.as_ref().is_some_and(|a| Arc::ptr_eq(a, b))
    }

    /// `h.ctrl == h.agent`
    pub fn ctrl_is_agent(&self) -> bool {
        Self::ptr_eq(&self.ctrl, &self.agent)
    }

    /// Chat history is only kept in a file when -data is given; otherwise it lives in memory.
    /// caller holds h.mu
    pub fn save(&self) {
        if self.data_file.is_empty() {
            return;
        }
        #[derive(Serialize)]
        struct Saved<'a> {
            chat: &'a [ChatMsg],
        }
        let b = go_escape(
            serde_json::to_string_pretty(&Saved { chat: &self.chat }).unwrap_or_default(),
        );
        if let Err(e) = write_file_0644(&self.data_file, b.as_bytes()) {
            golog(&format!("save failed: {e}"));
        }
    }

    /// caller holds h.mu
    pub fn broadcast(&self, m: &Msg, except: Option<&Arc<Client>>) {
        let b = Bytes::from(go_json(m));
        for c in &self.clients {
            if except.is_some_and(|e| Arc::ptr_eq(e, c)) {
                continue;
            }
            let out = OutMsg {
                bin: false,
                data: b.clone(),
            };
            if m.type_ == "cursor" {
                c.try_send_best_effort(out);
            } else {
                c.send_important(out);
            }
        }
    }

    pub fn users(&self) -> Vec<User> {
        // coding agents come separately, in "agents"
        self.clients.iter().map(|c| c.user()).collect()
    }

    /// The id is assigned on a copy, so the caller's `cm` keeps ID 0 and the
    /// broadcast `chat` message carries no id. Kept as is (parity), the stored line has the id.
    pub fn push_chat(&mut self, mut cm: ChatMsg) {
        self.chat_seq += 1;
        cm.id = self.chat_seq;
        self.chat.push(cm);
        if self.chat.len() > 200 {
            let n = self.chat.len() - 200;
            self.chat.drain(..n);
        }
    }

    /// caller holds h.mu
    pub fn add_system_chat(&mut self, text: &str) {
        let cm = ChatMsg {
            text: text.to_string(),
            time: now_ms(),
            ..Default::default()
        };
        self.push_chat(cm.clone());
        self.broadcast(
            &Msg {
                type_: "chat".into(),
                msg: Some(cm),
                ..Default::default()
            },
            None,
        );
    }

    /// caller holds h.mu
    pub fn set_ctrl(&mut self, c: Option<&Arc<Client>>) {
        self.ctrl = c.cloned();
        self.broadcast(
            &Msg {
                type_: "control".into(),
                ctrl: c.map(|c| c.user()),
                ..Default::default()
            },
            None,
        );
    }
}

pub struct Hub {
    state: Mutex<HubState>,
    /// `h.browser` (None = `-no-browser`); set once before serving
    browser: OnceLock<Arc<dyn browser::BrowserApi>>,
    hooks: OnceLock<Arc<dyn HubHooks>>,
}

/// Bright colors that read well on a dark background.
const COLORS: [&str; 10] = [
    "#f87171", "#fb923c", "#facc15", "#4ade80", "#2dd4bf", "#38bdf8", "#818cf8", "#c084fc",
    "#f472b6", "#a3e635",
];

const EMOTES: [&str; 8] = ["😍", "😴", "🤣", "🎉", "😱", "😄", "👍", "❤️"];

fn new_id() -> String {
    format!("{:x}", rand::random::<u64>() >> 1) // rand.Int63()
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn write_file_0644(path: &str, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o644);
    o.open(path)?.write_all(data)
}

/// `log.Println` to stderr: "2006/01/02 15:04:05 message".
pub fn golog(msg: &str) {
    eprintln!("{} {msg}", log_time());
}

#[cfg(unix)]
fn log_time() -> String {
    // SAFETY: localtime_r only writes into the zeroed tm we pass.
    unsafe {
        let t: libc::time_t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as libc::time_t);
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}/{:02}/{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

#[cfg(not(unix))]
fn log_time() -> String {
    // UTC fallback (prints local time)
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86400, secs % 86400);
    // civil from days (Howard Hinnant)
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}/{m:02}/{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// ---------- Persistence (chat history) ----------

impl Hub {
    pub fn new(data_file: &str) -> Arc<Hub> {
        let (wake, _) = watch::channel(0);
        let h = Arc::new(Hub {
            state: Mutex::new(HubState {
                clients: Vec::new(),
                chat: Vec::new(),
                data_file: data_file.to_string(),
                ctrl: None,
                agent: Client::detached(agent::agent_user()),
                agent_on: false,
                agent_last: None,
                agent_busy: false,
                agent_from: None,
                blocked: None,
                chat_seq: 0,
                seats: Default::default(),
                seats_sent: Vec::new(),
                chat_wake: wake,
                human_last: None,
            }),
            browser: OnceLock::new(),
            hooks: OnceLock::new(),
        });
        h.load();
        h
    }

    /// Locks the hub state. Never hold the guard across an `.await` (it is not `Send`).
    pub fn lock(&self) -> MutexGuard<'_, HubState> {
        lock(&self.state)
    }

    pub fn browser(&self) -> Option<&Arc<dyn browser::BrowserApi>> {
        self.browser.get()
    }

    /// `h.browser = b` (once, before the server starts).
    pub fn set_browser(&self, b: Arc<dyn browser::BrowserApi>) {
        let _ = self.browser.set(b);
    }

    pub fn set_hooks(&self, hooks: Arc<dyn HubHooks>) {
        let _ = self.hooks.set(hooks);
    }

    fn hooks(&self) -> Option<&Arc<dyn HubHooks>> {
        self.hooks.get()
    }

    /// `h.agentRelease` through the hooks.
    pub fn hooks_agent_release(&self, st: &mut HubState) {
        if let Some(hk) = self.hooks() {
            hk.agent_release(st);
        }
    }

    /// Chat history is only kept in a file when -data is given; otherwise it lives in memory.
    fn load(&self) {
        let mut st = self.lock();
        if st.data_file.is_empty() {
            return;
        }
        let Ok(b) = std::fs::read(&st.data_file) else {
            return;
        };
        #[derive(Deserialize)]
        struct Saved {
            chat: Option<Vec<ChatMsg>>,
        }
        if let Ok(Saved { chat: Some(chat) }) = serde_json::from_slice::<Saved>(&b) {
            st.chat = chat;
            for m in &st.chat.clone() {
                st.chat_seq = st.chat_seq.max(m.id);
            }
        }
    }

    // ---------- Sending ----------

    pub fn send_to(&self, c: &Arc<Client>, m: Msg) {
        let st = self.lock();
        if !st.clients.iter().any(|x| Arc::ptr_eq(x, c)) {
            // already left
            return;
        }
        c.send_important(OutMsg {
            bin: false,
            data: Bytes::from(go_json(&m)),
        });
    }

    pub fn system_chat(&self, text: &str) {
        self.lock().add_system_chat(text);
    }

    pub fn broadcast_tabs(&self, tabs: Vec<browser::TabInfo>, active: String) {
        self.lock().broadcast(
            &Msg {
                type_: "tabs".into(),
                tabs,
                active,
                ..Default::default()
            },
            None,
        );
    }

    pub fn broadcast_view(&self, v: browser::View) {
        self.lock().broadcast(
            &Msg {
                type_: "view".into(),
                view: Some(v),
                ..Default::default()
            },
            None,
        );
    }

    pub fn broadcast_frame(&self, pkt: Bytes) {
        let st = self.lock();
        for c in &st.clients {
            if !lock(&c.flags).paused {
                c.push_frame(pkt.clone());
            }
        }
    }

    pub fn broadcast_audio(&self, pkt: Bytes) {
        let st = self.lock();
        for c in &st.clients {
            let f = lock(&c.flags);
            if f.audio && !f.paused {
                c.try_send_best_effort(OutMsg {
                    bin: true,
                    data: pkt.clone(),
                });
            }
        }
    }

    // ---------- Handling client messages ----------

    pub fn handle(self: &Arc<Self>, c: &Arc<Client>, m: Msg) {
        let mut st = self.lock();
        let st = &mut *st;
        let hooks = self.hooks();

        match m.type_.as_str() {
            "cursor" => st.broadcast(
                &Msg {
                    type_: "cursor".into(),
                    id: c.user().id,
                    x: m.x,
                    y: m.y,
                    area: m.area.clone(),
                    ..Default::default()
                },
                Some(c),
            ),

            "audio" => lock(&c.flags).audio = m.on,

            "pause" => {
                lock(&c.flags).paused = m.on;
                if !m.on {
                    if let Some(b) = self.browser() {
                        if let (_, _, Some(f), _) = b.state() {
                            c.push_frame(f);
                        }
                    }
                }
            }

            "emote" => {
                let mut f = lock(&c.flags);
                if !EMOTES.contains(&m.text.as_str())
                    || f.last_emote
                        .is_some_and(|t| t.elapsed() < Duration::from_millis(250))
                {
                    return;
                }
                f.last_emote = Some(Instant::now());
                drop(f);
                st.broadcast(
                    &Msg {
                        type_: "emote".into(),
                        text: m.text,
                        user: Some(c.user()),
                        ..Default::default()
                    },
                    None,
                );
            }

            "chat" => {
                if m.text.is_empty() {
                    return;
                }
                let mut text = m.text;
                if text.chars().count() > 1000 {
                    text = text.chars().take(1000).collect();
                }
                let u = c.user();
                let mut cm = ChatMsg {
                    user: u.name,
                    color: u.color,
                    text,
                    time: now_ms(),
                    ..Default::default()
                };
                st.route_chat(&mut cm);
                st.push_chat(cm.clone());
                let for_agents = cm.to_bot || !cm.to.is_empty();
                if for_agents {
                    st.wake_agents();
                }
                st.broadcast(
                    &Msg {
                        type_: "chat".into(),
                        msg: Some(cm.clone()),
                        ..Default::default()
                    },
                    None,
                );
                if for_agents {
                    st.notice_unheard(&cm);
                }
                st.save();
            }

            "pick" => {
                if self.browser().is_some() && (m.event == "hover" || m.event == "click") {
                    if let Some(hk) = hooks {
                        hk.pick(self, c, m);
                    }
                }
            }

            "rec_start" => {
                if let Some(hk) = hooks {
                    hk.rec_start(st, c);
                }
            }

            "rec_stop" | "rec_discard" => {
                if let Some(hk) = hooks {
                    hk.rec_stop(st, Some(c), m.type_ == "rec_discard");
                }
            }

            "control_take" => {
                st.human_last = Some(Instant::now());
                if !HubState::ptr_eq(&st.ctrl, c) {
                    st.agent_from = None;
                    st.set_ctrl(Some(c));
                }
            }

            // a person disconnects one coding agent (m.id = its @handle)
            "agent_kick" => st.kick_seat(self, &m.id, &c.user().name),

            // while blocked, the agent can't take control (and gives it back now if it has it)
            "agent_block" => {
                if m.on && st.blocked.is_none() {
                    let u = c.user();
                    st.blocked = Some(u.clone());
                    st.broadcast(
                        &Msg {
                            type_: "block".into(),
                            blocked: st.blocked.clone(),
                            ..Default::default()
                        },
                        None,
                    );
                    st.add_system_chat(&format!("🛑 {} blocked the agent", u.name));
                    if st.ctrl_is_agent() {
                        self.hooks_agent_release(st);
                    }
                } else if !m.on && st.blocked.is_some() {
                    st.blocked = None;
                    st.broadcast(
                        &Msg {
                            type_: "block".into(),
                            ..Default::default()
                        },
                        None,
                    );
                    st.add_system_chat(&format!(
                        "✅ {} let the agent take control again",
                        c.user().name
                    ));
                }
            }

            "control_release" => {
                if HubState::ptr_eq(&st.ctrl, c) {
                    st.set_ctrl(None);
                }
            }

            "nav" | "back" | "forward" | "reload" | "stop" | "mouse" | "key" | "text"
            | "tab_new" | "tab_close" | "tab_activate" | "tab_move" | "tab_mute" | "view" => {
                let Some(b) = self.browser() else {
                    return;
                };
                // nobody drives: whoever acts first takes control
                if st.ctrl.is_none()
                    && m.type_ != "view"
                    && !(m.type_ == "mouse" && m.event == "move")
                {
                    st.agent_from = None;
                    st.set_ctrl(Some(c));
                }
                // only whoever is in control can drive
                if !HubState::ptr_eq(&st.ctrl, c) {
                    return;
                }
                st.human_last = Some(Instant::now());
                if let Some(hk) = hooks {
                    hk.rec_input(st, &m);
                }
                match m.type_.as_str() {
                    "nav" => b.navigate(&m.url),
                    "back" => b.history(-1),
                    "forward" => b.history(1),
                    "reload" => b.reload(),
                    "stop" => b.stop(),
                    "mouse" => b.mouse(&m),
                    "key" => {
                        let (hub, c2) = (self.clone(), c.clone());
                        b.key(
                            &m,
                            Box::new(move |s: String| {
                                hub.send_to(
                                    &c2,
                                    Msg {
                                        type_: "clipboard".into(),
                                        text: s.clone(),
                                        ..Default::default()
                                    },
                                );
                                if let (Some(b), Some(hk)) = (hub.browser(), hub.hooks()) {
                                    let (u, title) = b.page_of();
                                    hk.asset_copy(&hub, s, u, title, c2.user().name);
                                }
                            }),
                        );
                    }
                    "text" => b.text(&m.text),
                    "tab_new" => b.new_tab(&m.url),
                    "tab_close" => b.close_tab(&m.id),
                    "tab_activate" => b.activate_tab(&m.id),
                    "tab_move" => b.move_tab(&m.id, m.index),
                    "tab_mute" => b.mute_tab(&m.id),
                    "view" => {
                        if let Some(v) = m.view {
                            b.set_view(v);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // ---------- WebSocket ----------

    /// One connection, from the upgrade to the close. The reader runs here, the writer in its own task.
    pub async fn run_client(self: Arc<Self>, sock: WebSocket, name: String) {
        let user = User {
            id: new_id(),
            name: name.clone(),
            color: COLORS[rand::random_range(0..COLORS.len())].to_string(),
            agent: false,
        };
        let (c, crx) = Client::new(user, SEND_QUEUE);

        // Lock order is always h.mu before Browser.mu; the browser never calls the hub while holding its lock.
        let mut init = Msg {
            type_: "init".into(),
            you: Some(c.user()),
            ..Default::default()
        };
        let mut frame = None;
        if let Some(b) = self.browser() {
            let (tabs, active, f, v) = b.state();
            init.enabled = true;
            init.tabs = tabs;
            init.active = active;
            init.view = Some(v);
            frame = f;
        }

        {
            let mut st = self.lock();
            st.clients.push(c.clone());
            init.users = st.users();
            init.chat = st.chat.clone();
            init.agents = Some(st.seat_names());
            init.ctrl = st.ctrl.as_ref().map(|c| c.user());
            init.blocked = st.blocked.clone();
            if let Some(info) = self.hooks().and_then(|h| h.rec_info(&st)) {
                init.rec = Some(info);
            }
            let _ = c.send.try_send(OutMsg {
                bin: false,
                data: Bytes::from(go_json(&init)),
            });
            if let Some(f) = frame {
                c.push_frame(f);
            }
            st.broadcast(
                &Msg {
                    type_: "user_join".into(),
                    user: Some(c.user()),
                    ..Default::default()
                },
                Some(&c),
            );
        }
        golog(&format!("+ {name} joined"));

        let (sink, stream) = sock.split();
        let writer = tokio::spawn(write_loop(c.clone(), sink, crx));
        read_loop(&self, &c, stream).await;
        c.kill();
        let _ = writer.await;

        {
            let mut st = self.lock();
            let st = &mut *st;
            st.clients.retain(|x| !Arc::ptr_eq(x, &c));
            if HubState::ptr_eq(&st.ctrl, &c) {
                st.set_ctrl(None);
            }
            if HubState::ptr_eq(&st.agent_from, &c) {
                st.agent_from = None;
            }
            // whoever recorded left: keep what they did
            if let Some(hk) = self.hooks() {
                if hk.rec_by(st, &c) {
                    hk.rec_stop(st, None, false);
                }
            }
            st.broadcast(
                &Msg {
                    type_: "user_leave".into(),
                    id: c.user().id,
                    ..Default::default()
                },
                None,
            );
        }
        golog(&format!("- {name} left"));
    }
}

/// Reader loop: 64 KiB message limit (set on the upgrade), 60 s read deadline that only a
/// pong extends, any read error ends the connection.
async fn read_loop(h: &Arc<Hub>, c: &Arc<Client>, mut stream: SplitStream<WebSocket>) {
    let mut kill = c.kill.subscribe();
    let mut deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let next = tokio::select! {
            _ = kill.wait_for(|k| *k) => return,
            r = tokio::time::timeout_at(deadline.into(), stream.next()) => r,
        };
        let msg = match next {
            Ok(Some(Ok(m))) => m,
            _ => return, // closed, error or read deadline
        };
        let parsed = match msg {
            Message::Text(t) => serde_json::from_str::<Msg>(t.as_str()),
            Message::Binary(b) => serde_json::from_slice::<Msg>(&b),
            Message::Pong(_) => {
                deadline = Instant::now() + Duration::from_secs(60);
                continue;
            }
            Message::Ping(_) => continue,
            Message::Close(_) => return,
        };
        match parsed {
            Ok(m) => h.handle(c, m),
            Err(_) => return, // read error: the connection is closed
        }
    }
}

/// Writer loop: queued messages in order, the newest frame, a ping every 30 s; every write
/// has a 10 s deadline and a failed write closes the connection.
async fn write_loop(c: Arc<Client>, mut sink: SplitSink<WebSocket, Message>, mut rx: ClientRx) {
    let mut ping = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(30),
        Duration::from_secs(30),
    );
    async fn write(sink: &mut SplitSink<WebSocket, Message>, m: Message) -> bool {
        matches!(
            tokio::time::timeout(Duration::from_secs(10), sink.send(m)).await,
            Ok(Ok(()))
        )
    }
    loop {
        let m = tokio::select! {
            biased; // queued messages first: `init` always precedes the first frame
            _ = rx.kill.wait_for(|k| *k) => break,
            m = rx.send.recv() => match m {
                Some(m) if m.bin => Message::Binary(m.data),
                Some(m) => match Utf8Bytes::try_from(m.data) {
                    Ok(t) => Message::Text(t),
                    Err(_) => continue,
                },
                None => break,
            },
            r = rx.frame.changed() => match r {
                Ok(()) => match rx.frame.borrow_and_update().clone() {
                    Some(f) => Message::Binary(f),
                    None => continue,
                },
                Err(_) => break,
            },
            _ = ping.tick() => Message::Ping(Bytes::new()),
        };
        if !write(&mut sink, m).await {
            c.kill();
            break;
        }
    }
}

/// `r.URL.Query.Get(key)`: first value, `+` and %XX decoded.
pub fn query_get(query: Option<&str>, key: &str) -> Option<String> {
    for pair in query?.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let dec = |s: &str| fileserver::percent_decode(&s.replace('+', " "));
        if dec(k) == key {
            return Some(dec(v));
        }
    }
    None
}

/// `Hub.serveWS`. Missing name -> 400 "name required"; names are cut to 30 characters; any Origin is accepted.
async fn serve_ws(
    State(h): State<Arc<Hub>>,
    RawQuery(q): RawQuery,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let mut name = query_get(q.as_deref(), "name").unwrap_or_default();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            "name required\n",
        )
            .into_response();
    }
    if name.chars().count() > 30 {
        name = name.chars().take(30).collect();
    }
    match ws {
        Ok(ws) => ws
            .max_message_size(64 * 1024)
            .max_frame_size(64 * 1024)
            .on_upgrade(move |sock| h.run_client(sock, name)),
        Err(e) => e.into_response(),
    }
}

/// The routes this file owns (`mux.HandleFunc("/ws", h.serveWS)`).
pub fn ws_routes(h: Arc<Hub>) -> Router {
    Router::new().route("/ws", any(serve_ws)).with_state(h)
}

// ---------- main ----------

/// `filepath.Abs` (lexical, no symlinks).
fn go_abs(p: &str) -> String {
    if p.starts_with('/') || (cfg!(windows) && p.len() > 1 && p.as_bytes()[1] == b':') {
        return fileserver::path_clean(p);
    }
    match std::env::current_dir() {
        Ok(cwd) => fileserver::path_clean(&format!("{}/{p}", cwd.to_string_lossy())),
        Err(_) => p.to_string(),
    }
}

/// `filepath.Dir`
fn go_dir(p: &str) -> String {
    match p.rfind('/') {
        None => ".".into(),
        Some(0) => "/".into(),
        Some(i) => fileserver::path_clean(&p[..i]),
    }
}

fn go_join(a: &str, b: &str) -> String {
    fileserver::path_clean(&format!("{a}/{b}"))
}

/// Every folder and file the server derives from -home and the folder flags (main.rs ~l.668-760).
#[derive(Debug, Clone, PartialEq)]
pub struct Paths {
    pub home: String,
    /// bo.Profile: browser profile; with a browser it becomes absolute and gets "-<engine>" for non-Chrome engines
    pub profile: String,
    /// `<profile folder>/assets` (before the engine suffix)
    pub assets: String,
    /// built-in WARP account next to the profile ("" with -no-browser)
    pub warp_file: String,
    pub downloads: String,
    pub uploads: String,
    pub states: String,
    pub flows: String,
}

impl Paths {
    /// Everything lives in one home folder, whatever folder we were started from.
    pub fn new(cfg: &Config, home: &str) -> Paths {
        let profile = if cfg.profile.is_empty() {
            go_join(home, "chrome-profile")
        } else {
            cfg.profile.clone()
        };
        let assets = go_join(&go_dir(&profile), "assets");
        let warp_file = if cfg.no_browser {
            String::new()
        } else {
            go_join(&go_dir(&go_abs(&profile)), "warp.json")
        };
        Paths {
            home: home.to_string(),
            assets,
            warp_file,
            downloads: cfg.downloads.clone(),
            uploads: cfg.uploads.clone(),
            states: cfg.states.clone(),
            flows: cfg.flows.clone(),
            profile,
        }
    }

    /// After `resolve_engine`: each browser keeps its own profile (a profile written by a newer or
    /// different browser can refuse to open in another one); the other folders default to
    /// `downloads|uploads|states|flows` next to the profile and are made absolute.
    pub fn with_engine(&mut self, engine_name: &str) {
        if engine_name != "chrome" {
            self.profile.push('-');
            self.profile.push_str(engine_name);
        }
        self.profile = go_abs(&self.profile);
        let base = go_dir(&self.profile);
        for (dir, name) in [
            (&mut self.downloads, "downloads"),
            (&mut self.uploads, "uploads"),
            (&mut self.states, "states"),
            (&mut self.flows, "flows"),
        ] {
            *dir = if dir.is_empty() {
                go_join(&base, name)
            } else {
                go_abs(dir)
            };
        }
    }
}

/// net.InterfaceAddrs: non-loopback IPv4 addresses.
fn lan_ips() -> Vec<String> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(a) => Some(a.ip.to_string()),
            _ => None,
        })
        .collect()
}

/// Listen error text for the common cases ("Port %d is busy: %v").
fn listen_error(port: i64, e: &std::io::Error) -> String {
    if !(0..=65535).contains(&port) {
        return format!("listen tcp: address {port}: invalid port");
    }
    let why = match e.kind() {
        std::io::ErrorKind::AddrInUse => "address already in use".to_string(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        _ => e.to_string(),
    };
    format!("listen tcp 0.0.0.0:{port}: bind: {why}")
}

/// What the HTTP routes share.
pub struct App {
    pub hub: Arc<Hub>,
    pub cfg: Config,
    pub paths: Paths,
    pub stdio: bool,
    /// `h.browser` (None with -no-browser)
    pub browser: Option<Arc<browser::Browser>>,
    /// the MCP server (`mcp := &MCPServer{}`); its agent is set once the browser runs
    pub mcp: Arc<mcp::McpServer>,
    /// `px` (netproxy.rs startProxy): the shared browser's route switch
    pub proxy: Arc<netproxy::ProxySwitch>,
}

/// The whole mux (`mux := http.NewServeMux` ... `requireAuth(mux)`).
pub fn build_router(app: &Arc<App>) -> Router {
    let mut r = ws_routes(app.hub.clone());
    r = r.merge(devtools::routes(app.browser.clone()));
    r = r.merge(keepalive::routes(app.browser.clone()));
    if let Some(a) = assets::global() {
        r = r.merge(assets::routes(app.hub.clone(), a, app.browser.clone()));
    }
    r = r.merge(states_http::routes(app.mcp.clone()));
    r = r.merge(flow_http::routes(
        app.hub.clone(),
        app.mcp.clone(),
        app.browser.as_ref().map(|b| b.opt.flow_dir.clone()),
    ));
    if let Some(t) = tunnel::global() {
        r = r.merge(tunnel::routes(t));
    }
    r = r.merge(netproxy::routes(app.proxy.clone()));
    // Which browser engine runs the shared screen, for the small tag in the header.
    let eng = app
        .browser
        .as_ref()
        .map(|b| (b.opt.engine.name.clone(), b.opt.engine.label.clone()));
    r = r.route(
        "/api/engine",
        axum::routing::get(move || {
            let eng = eng.clone();
            async move {
                let (name, label) = eng.unwrap_or_else(|| ("none".into(), "No browser".into()));
                axum::Json(serde_json::json!({ "name": name, "label": label, "server": "rust" }))
            }
        }),
    );
    if !app.cfg.no_mcp {
        r = r.merge(app.mcp.routes(app.cfg.mcp_remote));
    }
    r = r.fallback(fileserver::file_server);
    // ServeMux cleans every path first; requireAuth wraps the whole mux, so auth is the outer layer.
    r.layer(axum::middleware::from_fn(fileserver::clean_path))
        .layer(axum::middleware::from_fn(auth::require_auth))
        .layer(axum::middleware::from_fn(sort_headers))
}

fn real_env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match parse_command_line(&argv, &real_env) {
        Ok(p) => p,
        Err(Exit::Help) => {
            eprint!("{}", usage_text(&real_env));
            return ExitCode::SUCCESS;
        }
        Err(Exit::Bad(msg)) => {
            eprintln!("{msg}");
            eprint!("{}", usage_text(&real_env));
            return ExitCode::from(2);
        }
    };
    // mcp.json can set the password with "env": {"AGENT_BROWSER_USE_PASSWORD": "..."} (parse_args did the fallback)
    auth::set_password(&parsed.config.pw);
    if parsed.config.version {
        println!("{APP_NAME} {VERSION}");
        return ExitCode::SUCCESS;
    }
    if parsed.config.update {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("Update failed: {e}");
                return ExitCode::from(1);
            }
        };
        if let Err(e) = rt.block_on(update::self_update()) {
            eprintln!("Update failed: {e:#}");
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }
    if !parsed.rest.is_empty() {
        // e.g. `make run -p 8080`: make takes -p itself and only "8080" arrives here
        eprintln!(
            "Unexpected arguments: {}\nFlags start with -, e.g. ./run -p 8080 -pw secret (with make: make run p=8080 pw=secret)",
            parsed.rest.join(" ")
        );
        return ExitCode::from(2);
    }
    // from a terminal: run the server as a child and show the menu
    if !parsed.stdio && menu::menu_wanted() {
        let full: Vec<String> = std::env::args().collect();
        let dir = menu::state_dir(&parsed.config.home, &full);
        let args: Vec<String> = full[1..].to_vec();
        let code = menu::run_menu(&args, parsed.config.port as u16, parsed.config.tunnel, &dir);
        return ExitCode::from(code as u8);
    }
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run(parsed))
}

/// `log.Fatalf`.
fn fatal(msg: &str) -> ExitCode {
    golog(msg);
    ExitCode::from(1)
}

async fn run(parsed: Parsed) -> ExitCode {
    let Parsed {
        stdio, config: cfg, ..
    } = parsed;
    // stdout carries only MCP messages in stdio mode
    let out = |s: &str| {
        if stdio {
            eprint!("{s}");
        } else {
            print!("{s}");
        }
    };

    // Everything lives in one home folder, whatever folder we were started from.
    let full: Vec<String> = std::env::args().collect();
    let home = menu::state_dir(&cfg.home, &full)
        .to_string_lossy()
        .into_owned();
    let mut paths = Paths::new(&cfg, &home);

    let local = format!("http://localhost:{}", cfg.port);
    let bound = if (0..=65535).contains(&cfg.port) {
        tokio::net::TcpListener::bind(("0.0.0.0", cfg.port as u16))
            .await
            .map_err(|e| listen_error(cfg.port, &e))
    } else {
        Err(listen_error(cfg.port, &std::io::Error::other("")))
    };
    let ln = match bound {
        Ok(l) => l,
        Err(err) => {
            if stdio {
                let (mut ok, mut need_auth) = mcp::probe_mcp(&format!("{local}/mcp")).await;
                // A running copy wants a password we don't have (or the wrong one): ask the person.
                let mut try_ = 0;
                while need_auth && try_ < 3 {
                    let msg = if auth::password().is_empty() {
                        "Agent Browser Use is password protected. Enter the password:"
                    } else {
                        "Wrong password. Enter the password for Agent Browser Use:"
                    };
                    let pw = tokio::task::spawn_blocking(move || prompt::ask_password(msg))
                        .await
                        .unwrap_or_default();
                    if pw.is_empty() {
                        break;
                    }
                    auth::set_password(&pw);
                    (ok, need_auth) = mcp::probe_mcp(&format!("{local}/mcp")).await;
                    try_ += 1;
                }
                if ok {
                    // another copy already runs the browser: use it
                    mcp::proxy_stdio(&format!("{local}/mcp")).await;
                    return ExitCode::SUCCESS;
                }
                if need_auth {
                    return fatal(&format!(
                        "The server on port {} needs a password. Set {PASSWORD_ENV} in the MCP config (\"env\") or pass -pw.",
                        cfg.port
                    ));
                }
            }
            return fatal(&format!("Port {} is busy: {err} (use -port)", cfg.port));
        }
    };

    let tm = tunnel::TunnelManager::new(cfg.port as u16, cfg.tunnel_backend.clone());
    tunnel::set_global(tm.clone());
    let hub = Hub::new(&cfg.data);
    // h.assets = newAssets(filepath.Join(filepath.Dir(bo.Profile), "assets"), h)
    assets::Assets::new(&paths.assets, hub.clone());
    let mcp = mcp::McpServer::new(hub.clone());
    tokio::spawn(hub.clone().watch_seats());

    let (px, px_addr) = match netproxy::start_proxy(&cfg.proxy, &paths.warp_file).await {
        Ok(v) => v,
        Err(e) => return fatal(&e),
    };
    let mut shared_browser: Option<Arc<browser::Browser>> = None;
    let share_hooks = share::Share::new();
    if !cfg.no_browser {
        let eng = match engine::resolve_engine(&cfg.engine, &cfg.chrome) {
            Ok(e) => e,
            Err(e) => return fatal(&e),
        };
        // Each browser keeps its own profile: a profile written by a newer or
        // different browser can refuse to open (or get damaged) in another one.
        paths.with_engine(&eng.name);
        let bo = browser::BrowserOptions {
            width: cfg.width,
            height: cfg.height,
            quality: cfg.quality,
            start_url: cfg.start.clone(),
            profile: paths.profile.clone(),
            chrome_path: cfg.chrome.clone(),
            engine: eng.clone(),
            // Chrome talks to our local switch; "" = direct
            proxy: px_addr.clone(),
            audio: !cfg.no_audio,
            max_tabs: cfg.max_tabs,
            download_dir: paths.downloads.clone(),
            upload_dir: paths.uploads.clone(),
            state_dir: paths.states.clone(),
            flow_dir: paths.flows.clone(),
            no_dev_tools: cfg.no_devtools,
            hooks: Some(Arc::new(browser::HookSet(vec![
                Arc::new(observe::Observe),
                Arc::new(share_hooks.clone()),
                Arc::new(assets::DownloadHooks),
                Arc::new(flow::FlowHooks),
            ]))),
        };
        match browser::start_browser(hub.clone(), bo).await {
            Ok(b) => {
                share_hooks.attach(&b);
                flow::attach(&hub, &b);
                hub.set_browser(b.clone());
                shared_browser = Some(b);
            }
            Err(e) => {
                return fatal(&format!(
                    "Could not start {}: {e}\n(Pick another -engine, pass -chrome <path>, or run with -no-browser)",
                    eng.label
                ))
            }
        }
        if let Some(b) = &shared_browser {
            let ao = agent::AgentOptions {
                speed: cfg.agent_speed,
                open_viewer: cfg.open,
                viewer_url: format!("{local}/?join"),
                idle: cfg.agent_idle,
            };
            mcp.set_agent(agent::Agent::new(hub.clone(), b.clone(), ao));
        }
        out(&format!(
            "Shared browser started (headless {}: {})\n",
            eng.label, eng.path
        ));
    }

    hub.set_hooks(Arc::new(agent::AppHooks {
        browser: shared_browser.clone(),
    }));
    let app = Arc::new(App {
        hub: hub.clone(),
        cfg: cfg.clone(),
        paths,
        stdio,
        browser: shared_browser.clone(),
        mcp: mcp.clone(),
        proxy: px.clone(),
    });
    let router = build_router(&app);

    out(&format!("{APP_NAME} is running:\n  {local}\n"));
    for ip in lan_ips() {
        out(&format!(
            "  http://{ip}:{}   (share this link on your network)\n",
            cfg.port
        ));
    }
    if !cfg.no_mcp {
        out(&format!("MCP endpoint for AI agents: {local}/mcp\n"));
    }
    if !auth::password().is_empty() {
        out("Password protection is on (MCP clients: send \"Authorization: Bearer <password>\")\n");
    }
    if cfg.tunnel {
        out("Opening public tunnel...\n");
        if let Err(e) = tm.start(&cfg.subdomain).await {
            out(&format!(
                "Tunnel failed: {e}\n(the app keeps running locally)\n"
            ));
        }
    }

    // Close Chrome on the way out, so no process is left running.
    let quit = Arc::new(tokio::sync::Notify::new());
    {
        let quit = quit.clone();
        tokio::spawn(async move {
            serve(ln, router).await;
            quit.notify_one();
        });
    }
    let st = tm.status();
    if st.on {
        out(&format!("Public URL: {}\n  MCP: {}/mcp\n", st.url, st.url));
        if st.generated {
            out(&format!(
                "  Password (generated; set your own with -pw): {}\n",
                st.password
            ));
        }
    }
    {
        let quit = quit.clone();
        tokio::spawn(async move {
            wait_for_signal().await;
            quit.notify_one();
        });
    }
    if stdio {
        // the MCP client closed stdin: it is done with us
        let quit = quit.clone();
        let mcp = mcp.clone();
        tokio::spawn(async move {
            mcp.serve_stdio().await;
            quit.notify_one();
        });
    }
    quit.notified().await;
    tm.stop().await;
    if let Some(b) = hub.browser() {
        b.close();
    }
    ExitCode::SUCCESS
}

/// net/http writes header names in canonical form (`Content-Type`) and sorted by name; hyper
/// lower-cases them by default and keeps insertion order. Same bytes as on the wire:
/// title-case names (hyper option) and sort them (this middleware, outermost).
async fn sort_headers(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    if h.len() > 1 {
        let mut all: Vec<_> = std::mem::take(h).into_iter().collect();
        // into_iter yields None for repeated names: fold them back under the last seen name
        let mut cur = None;
        let mut flat = Vec::new();
        for (n, v) in all.drain(..) {
            if let Some(n) = n {
                cur = Some(n);
            }
            flat.push((cur.clone().unwrap(), v));
        }
        flat.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        for (n, v) in flat {
            h.append(n, v);
        }
    }
    res
}

/// `http.Serve(ln, handler)`: HTTP/1.1 with title-cased header names, WebSocket upgrades allowed.
async fn serve(ln: tokio::net::TcpListener, router: Router) {
    use tower::{Service, ServiceExt};
    let mut make = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
    loop {
        let (sock, peer) = match ln.accept().await {
            Ok(x) => x,
            Err(e) => {
                golog(&format!("accept: {e}"));
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(svc) = make.call(peer).await;
        tokio::spawn(async move {
            let svc =
                hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    svc.clone().oneshot(req)
                });
            let _ = hyper::server::conn::http1::Builder::new()
                .title_case_headers(true)
                .serve_connection(hyper_util::rt::TokioIo::new(sock), svc)
                .with_upgrades()
                .await;
        });
    }
}

/// `signal.Notify(sig, os.Interrupt, syscall.SIGTERM)`
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }
    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }
    fn parse(a: &[&str]) -> Result<Parsed, Exit> {
        parse_command_line(&args(a), &no_env)
    }

    #[test]
    fn defaults() {
        let c = parse(&[]).unwrap().config;
        assert_eq!(c.port, 3000);
        assert_eq!(
            (c.width, c.height, c.quality, c.max_tabs),
            (1280, 720, 92, 12)
        );
        assert_eq!(c.start, "https://www.google.com");
        assert_eq!(c.engine, "auto");
        assert!(c.open && !c.no_browser && !c.no_mcp && !c.tunnel);
        assert_eq!(c.agent_idle, Duration::from_secs(15));
        assert_eq!(c.agent_speed, 0.0);
    }

    #[test]
    fn aliases_and_styles() {
        assert_eq!(parse(&["-p", "8080"]).unwrap().config.port, 8080);
        assert_eq!(parse(&["-port=81"]).unwrap().config.port, 81);
        assert_eq!(parse(&["--port", "82"]).unwrap().config.port, 82);
        assert!(parse(&["-v"]).unwrap().config.version);
        assert!(parse(&["-version"]).unwrap().config.version);
        assert!(parse(&["-u"]).unwrap().config.update);
        assert!(parse(&["-update"]).unwrap().config.update);
        assert_eq!(parse(&["-pw", "secret"]).unwrap().config.pw, "secret");
    }

    #[test]
    fn bool_flags() {
        let c = parse(&["-no-browser", "-open=false", "-no-audio=true"])
            .unwrap()
            .config;
        assert!(c.no_browser && !c.open && c.no_audio);
        // a bool flag never swallows the next argument
        let p = parse(&["-no-browser", "extra"]).unwrap();
        assert_eq!(p.rest, vec!["extra"]);
        assert!(matches!(parse(&["-no-mcp=maybe"]), Err(Exit::Bad(_))));
    }

    #[test]
    fn durations_and_floats() {
        let c = parse(&["-agent-idle", "1m30s", "-agent-speed", "1.5"])
            .unwrap()
            .config;
        assert_eq!(c.agent_idle, Duration::from_secs(90));
        assert_eq!(c.agent_speed, 1.5);
        assert_eq!(
            parse(&["-agent-idle", "0"]).unwrap().config.agent_idle,
            Duration::ZERO
        );
        assert_eq!(
            parse(&["-agent-idle=300ms"]).unwrap().config.agent_idle,
            Duration::from_millis(300)
        );
        assert!(matches!(parse(&["-agent-idle", "soon"]), Err(Exit::Bad(_))));
    }

    #[test]
    fn mcp_subcommand_only_first() {
        let p = parse(&["mcp", "-p", "9"]).unwrap();
        assert!(p.stdio && p.config.port == 9);
        let p = parse(&["-p", "9", "mcp"]).unwrap();
        assert!(!p.stdio);
        assert_eq!(p.rest, vec!["mcp"]);
    }

    #[test]
    fn unexpected_args_and_terminators() {
        assert_eq!(parse(&["8080"]).unwrap().rest, vec!["8080"]);
        assert_eq!(
            parse(&["-p", "1", "8080", "-pw", "x"]).unwrap().rest,
            vec!["8080", "-pw", "x"]
        );
        assert_eq!(parse(&["--", "-p"]).unwrap().rest, vec!["-p"]);
    }

    #[test]
    fn errors() {
        assert_eq!(
            parse(&["-nope"]),
            Err(Exit::Bad("flag provided but not defined: -nope".into()))
        );
        assert_eq!(
            parse(&["-port"]),
            Err(Exit::Bad("flag needs an argument: -port".into()))
        );
        assert_eq!(
            parse(&["-port", "abc"]),
            Err(Exit::Bad(
                "invalid value \"abc\" for flag -port: parse error".into()
            ))
        );
        assert_eq!(parse(&["-h"]), Err(Exit::Help));
        assert_eq!(parse(&["--help"]), Err(Exit::Help));
        assert_eq!(
            parse(&["---x"]),
            Err(Exit::Bad("bad flag syntax: ---x".into()))
        );
    }

    #[test]
    fn env_fallbacks() {
        let env = |k: &str| match k {
            PROXY_ENV => Some("socks5://127.0.0.1:1".to_string()),
            PASSWORD_ENV => Some("envpw".to_string()),
            _ => None,
        };
        let c = parse_command_line(&[], &env).unwrap().config;
        assert_eq!(c.proxy, "socks5://127.0.0.1:1");
        assert_eq!(c.pw, "envpw");
        // flags win over env
        let c = parse_command_line(&args(&["-proxy", "x", "-pw", "p"]), &env)
            .unwrap()
            .config;
        assert_eq!((c.proxy.as_str(), c.pw.as_str()), ("x", "p"));
        // the env default shows up in -h
        assert!(usage_text(&env).contains("(default \"socks5://127.0.0.1:1\")"));
    }

    #[test]
    fn usage_format() {
        let u = usage_text(&no_env);
        assert!(u.starts_with(
            "Usage:\n  agent-browser-use [flags]       web UI + MCP over HTTP at /mcp\n"
        ));
        assert!(u.contains("\nFlags:\n  -agent-idle duration\n    \tgive control back"));
        assert!(u.contains("(0 = never) (default 15s)"));
        assert!(u.contains("\n  -p int\n    \tHTTP port (short for -port) (default 3000)\n"));
        assert!(u.contains("\n  -v\tprint the version and exit (short for -version)\n"));
        assert!(u.contains("\n  -open\n    \topen the web UI in your browser when the agent starts and nobody is watching (default true)\n"));
        assert!(u.contains("\n  -port int\n    \tHTTP port (default 3000)\n"));
        assert!(u.contains("(default \"https://www.google.com\")"));
        // sorted by name, like flag.PrintDefaults
        let names: Vec<&str> = u
            .lines()
            .filter_map(|l| l.strip_prefix("  -"))
            .map(|l| l.split([' ', '\t']).next().unwrap())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert_eq!(names.len(), 33);
    }

    #[test]
    fn go_ints() {
        assert_eq!(parse_go_int("0x10"), Some(16));
        assert_eq!(parse_go_int("-5"), Some(-5));
        assert_eq!(parse_go_int("1_000"), Some(1000));
        assert_eq!(parse_go_int("1.5"), None);
        assert_eq!(parse_go_int(""), None);
    }

    #[test]
    fn embedded_ui() {
        assert!(StaticFiles::get("index.html").is_some());
        assert!(StaticFiles::get("js/boot.js").is_some());
        assert!(!agent::agent_js().is_empty());
    }

    // ---------- presence_test.rs ----------

    fn test_client(name: &str, queue: usize) -> (Arc<Client>, ClientRx) {
        Client::new(
            User {
                id: name.into(),
                name: name.into(),
                ..Default::default()
            },
            queue,
        )
    }

    /// Cursor moves and sound fill a viewer's outgoing queue when its connection is slow.
    /// Presence messages (someone joined or left) must still reach that viewer, otherwise
    /// its member list silently goes out of date.
    #[test]
    fn presence_survives_cursor_flood() {
        let h = Hub::new("");
        let (mover, _mrx) = test_client("mover", 8);
        let (slow, mut slow_rx) = test_client("slow", 8);
        {
            let mut st = h.lock();
            st.clients.push(mover.clone());
            st.clients.push(slow.clone());
            for i in 0..100 {
                st.broadcast(
                    &Msg {
                        type_: "cursor".into(),
                        id: "mover".into(),
                        x: i as f64,
                        ..Default::default()
                    },
                    Some(&mover),
                );
            }
            st.broadcast(
                &Msg {
                    type_: "user_join".into(),
                    user: Some(User {
                        id: "new".into(),
                        name: "new".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            );
        }
        let mut joined = false;
        while let Ok(m) = slow_rx.send.try_recv() {
            if String::from_utf8_lossy(&m.data).contains("\"user_join\"") {
                joined = true;
            }
        }
        assert!(
            joined,
            "user_join was dropped for a viewer whose queue was full of cursor moves"
        );
        assert!(!slow.killed());
    }

    /// An important message that finds the queue full disconnects the client.
    #[test]
    fn full_queue_disconnects() {
        let (c, _rx) = test_client("slow", 2);
        for _ in 0..3 {
            c.send_important(OutMsg {
                bin: false,
                data: Bytes::from_static(b"{}"),
            });
        }
        assert!(c.killed());
    }

    type Ws = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn dial(url: &str) -> Ws {
        tokio_tungstenite::connect_async(url).await.unwrap().0
    }

    /// waitType reads text messages until one has the wanted type.
    async fn wait_type(c: &mut Ws, want: &str) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let m = c.next().await.expect("closed").expect("read");
                if let tokio_tungstenite::tungstenite::Message::Text(t) = m {
                    let v: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
                    if v["type"] == want {
                        return v;
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("waiting for {want:?}"))
    }

    async fn serve_hub(h: Arc<Hub>) -> String {
        let ln = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ln.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(ln, ws_routes(h)).await });
        format!("ws://{addr}/ws?name=")
    }

    /// Over a real WebSocket: a viewer sees others join and leave, and gets a snapshot when it connects.
    #[tokio::test]
    async fn presence_over_websocket() {
        let base = serve_hub(Hub::new("")).await;
        let mut alice = dial(&format!("{base}alice")).await;
        let init = wait_type(&mut alice, "init").await;
        assert_eq!(init["agents"], serde_json::json!([]));
        assert_eq!(init["you"]["name"], "alice");

        let mut bob = dial(&format!("{base}bob")).await;
        wait_type(&mut alice, "user_join").await;
        bob.close(None).await.unwrap();
        wait_type(&mut alice, "user_leave").await;
    }

    #[tokio::test]
    async fn chat_control_emote_and_history() {
        let dir = std::env::temp_dir().join(format!("abu-test-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&dir).unwrap();
        let data = dir.join("chat.json");
        let h = Hub::new(data.to_str().unwrap());
        let base = serve_hub(h.clone()).await;
        let mut a = dial(&format!("{base}Alice")).await;
        wait_type(&mut a, "init").await;
        use tokio_tungstenite::tungstenite::Message as M;
        a.send(M::text(r#"{"type":"chat","text":"hi <b>"}"#))
            .await
            .unwrap();
        a.send(M::text(r#"{"type":"emote","text":"👍"}"#))
            .await
            .unwrap();
        a.send(M::text(r#"{"type":"control_take"}"#)).await.unwrap();
        let chat = wait_type(&mut a, "chat").await;
        assert_eq!(chat["msg"]["text"], "hi <b>");
        assert!(
            chat["msg"].get("id").is_none(),
            "broadcast chat carries no id"
        );
        wait_type(&mut a, "emote").await;
        let c = wait_type(&mut a, "control").await;
        assert_eq!(c["ctrl"]["name"], "Alice");
        // history is saved, with HTML escaping, and reloaded with ids
        let saved = std::fs::read_to_string(&data).unwrap();
        assert!(saved.contains("hi \\u003cb\\u003e"), "{saved}");
        let h2 = Hub::new(data.to_str().unwrap());
        let st = h2.lock();
        assert_eq!(st.chat.len(), 1);
        assert_eq!(st.chat_seq, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ws_name_required() {
        let base = serve_hub(Hub::new("")).await;
        let url = base.replace("ws://", "http://").replace("?name=", "");
        let r = reqwest::get(&url).await.unwrap();
        assert_eq!(r.status(), 400);
        assert_eq!(r.text().await.unwrap(), "name required\n");
    }

    #[test]
    fn mentions_like_the_go_regex() {
        let m = |s: &str| chat::seat_base(s);
        assert_eq!(m("Open Code!"), "open-code");
        assert_eq!(m("agent"), "coder");
        let mut st_chat = ChatMsg {
            text: "@Agent hi @opencode#3f9a and mail@x.com @".into(),
            ..Default::default()
        };
        let h = Hub::new("");
        h.lock().route_chat(&mut st_chat);
        assert!(st_chat.to_bot);
        assert!(st_chat.to.is_empty()); // no such seat
        let s = h.register_seat("OpenCode", true);
        let handle = s.lock().unwrap().handle.clone();
        let mut cm = ChatMsg {
            text: format!("x @opencode and @{handle}"),
            ..Default::default()
        };
        h.lock().route_chat(&mut cm);
        assert_eq!(cm.to, vec![handle]);
    }

    #[test]
    fn json_escaping_matches_go() {
        assert_eq!(
            go_json(&"a<b>&c\u{2028}"),
            "\"a\\u003cb\\u003e\\u0026c\\u2028\""
        );
        let m = Msg {
            type_: "init".into(),
            agents: Some(vec![]),
            ..Default::default()
        };
        assert_eq!(go_json(&m), r#"{"type":"init","agents":[]}"#);
    }

    #[test]
    fn paths_like_go() {
        let cfg = parse(&["-no-browser"]).unwrap().config;
        let p = Paths::new(&cfg, "/h");
        assert_eq!(p.profile, "/h/chrome-profile");
        assert_eq!(p.assets, "/h/assets");
        assert_eq!(p.warp_file, "");
        let cfg = parse(&[]).unwrap().config;
        let mut p = Paths::new(&cfg, "/h");
        assert_eq!(p.warp_file, "/h/warp.json");
        p.with_engine("brave");
        assert_eq!(p.profile, "/h/chrome-profile-brave");
        assert_eq!(p.downloads, "/h/downloads");
        assert_eq!(p.flows, "/h/flows");
    }
}
