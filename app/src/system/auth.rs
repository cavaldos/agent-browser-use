//! Password protection (-pw). People get a login page and then a cookie; programs
//! (MCP clients) send the password as "Authorization: Bearer <password>" (or Basic auth).
//! Password and login handling for the HTTP routes.
//!
//! What main.rs must call:
//! - `set_password(&str)` at startup with the `-pw` value, or `AGENT_BROWSER_USE_PASSWORD` when
//!   `-pw` is empty (main.rs). Only the tunnel button changes it later (tunnel.rs).
//! - `password` where it reads `password` (main.rs).
//! - Wrap the whole router with `requireAuth(mux)` (main.rs):
//!   `router.layer(axum::middleware::from_fn(auth::require_auth))`.
//!   The middleware answers `/login` itself, so it must sit outside every route.
//! - `authorized(headers, password)` is the pure check (cookie, Bearer, Basic) for any other use.

use std::sync::RwLock;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::Request;
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// auth.rs: `authPassword`, guarded by `pwMu`. Empty means no password.
static AUTH_PASSWORD: RwLock<String> = RwLock::new(String::new());

/// auth.rs: `password`.
pub fn password() -> String {
    AUTH_PASSWORD
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// auth.rs: `setPassword(p)`.
pub fn set_password(p: &str) {
    *AUTH_PASSWORD.write().unwrap_or_else(|e| e.into_inner()) = p.to_string();
}

/// auth.rs: `const authCookie`.
pub const AUTH_COOKIE: &str = "abu_auth";

/// auth.rs: the HMAC message that is keyed by the password.
const SESSION_MESSAGE: &str = "agent-browser-use session";

/// auth.rs: `authToken`. What the cookie holds: derived from the password, never the password.
pub fn auth_token(password: &str) -> String {
    let mut m = <Hmac<Sha256>>::new_from_slice(password.as_bytes())
        .expect("HMAC accepts keys of any length");
    m.update(SESSION_MESSAGE.as_bytes());
    hex::encode(m.finalize().into_bytes())
}

/// auth.rs: `same(a, b)`. Constant time (length mismatch returns false at once).
fn same(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

/// `r.Cookie(name)`: the first cookie with that name, across all Cookie header lines.
fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a [u8]> {
    for line in headers.get_all(header::COOKIE) {
        for part in line.as_bytes().split(|&b| b == b';') {
            let part = part.trim_ascii();
            let Some(eq) = part.iter().position(|&b| b == b'=') else {
                continue;
            };
            if &part[..eq] != name.as_bytes() {
                continue;
            }
            let mut value = &part[eq + 1..];
            if value.len() >= 2 && value.starts_with(b"\"") && value.ends_with(b"\"") {
                value = &value[1..value.len() - 1];
            }
            return Some(value);
        }
    }
    None
}

/// `r.BasicAuth`: the password half of `Authorization: Basic <base64(user:pass)>`.
/// The scheme name is matched case-insensitively, as parseBasicAuth does.
fn basic_password(auth: &[u8]) -> Option<Vec<u8>> {
    const PREFIX: &[u8] = b"Basic ";
    if auth.len() < PREFIX.len() || !auth[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&auth[PREFIX.len()..])
        .ok()?;
    let colon = decoded.iter().position(|&b| b == b':')?;
    Some(decoded[colon + 1..].to_vec())
}

/// auth.rs: `authorized(r)`, with the password passed in instead of read from the global.
/// Accepts the session cookie, `Authorization: Bearer <password>`, or Basic auth whose password
/// is the server password. With no password set everything is allowed.
pub fn authorized(headers: &HeaderMap, password: &str) -> bool {
    if password.is_empty() {
        return true;
    }
    if let Some(c) = cookie_value(headers, AUTH_COOKIE) {
        if same(c, auth_token(password).as_bytes()) {
            return true;
        }
    }
    // Header.Get returns the first Authorization value only.
    if let Some(h) = headers.get(header::AUTHORIZATION).map(|v| v.as_bytes()) {
        if let Some(token) = h.strip_prefix(b"Bearer ") {
            if same(token, password.as_bytes()) {
                return true;
            }
        }
        if let Some(pw) = basic_password(h) {
            if same(&pw, password.as_bytes()) {
                return true;
            }
        }
    }
    false
}

/// auth.rs: `requireAuth`, as an axum middleware. Browsers that are not logged in get the login
/// page; programs get `401 password required` with a `WWW-Authenticate: Bearer` challenge.
/// Use with `axum::middleware::from_fn(require_auth)`.
pub async fn require_auth(req: Request, next: Next) -> Response {
    let pw = password();
    if pw.is_empty() {
        return next.run(req).await;
    }
    if req.uri().path() == "/login" {
        return login(req).await;
    }
    if authorized(req.headers(), &pw) {
        return next.run(req).await;
    }
    let accepts_html = req
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"));
    if *req.method() == Method::GET && accepts_html {
        let next_url = req
            .uri()
            .path_and_query()
            .map_or("/", |pq| pq.as_str())
            .to_string();
        return login_page(&next_url, false);
    }
    (
        StatusCode::UNAUTHORIZED,
        [
            (
                header::WWW_AUTHENTICATE,
                "Bearer realm=\"agent-browser-use\"",
            ),
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        "password required\n",
    )
        .into_response()
}

/// auth.rs: `login(w, r)`. GET shows the page; POST checks the `password` field, sets the
/// cookie and redirects to `next`. A wrong password waits one second (slows down guessing).
pub async fn login(req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body = to_bytes(body, 10 << 20).await.unwrap_or_default();
    let form = |key: &str| form_value(&parts.method, &parts.uri, &parts.headers, &body, key);

    let mut next = form("next").unwrap_or_default();
    if !next.starts_with('/') || next.starts_with("//") {
        next = "/".to_string();
    }
    if parts.method != Method::POST {
        return login_page(&next, false);
    }
    let pw = password();
    let given = form("password").unwrap_or_default();
    if !same(given.as_bytes(), pw.as_bytes()) {
        tokio::time::sleep(Duration::from_secs(1)).await;
        return login_page(&next, true);
    }
    // http.SetCookie: Path=/, HttpOnly, SameSite=Lax, MaxAge 30 days.
    let cookie = format!(
        "{AUTH_COOKIE}={}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax",
        auth_token(&pw),
        30 * 24 * 3600
    );
    // http.Redirect(303): hex-escapes non-ASCII bytes in the Location.
    (
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, cookie),
            (header::LOCATION, hex_escape_non_ascii(&next)),
        ],
    )
        .into_response()
}

/// `r.FormValue(key)`: POST/PUT/PATCH form bodies come first, then the URL query; the first
/// match wins. Only urlencoded bodies are read (ParseForm does the same for other types).
fn form_value(
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
    key: &str,
) -> Option<String> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let is_body_method = matches!(*method, Method::POST | Method::PUT | Method::PATCH);
    let urlencoded = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        })
        .unwrap_or(false);
    if is_body_method && urlencoded {
        pairs.extend(url::form_urlencoded::parse(body).into_owned());
    }
    if let Some(q) = uri.query() {
        pairs.extend(url::form_urlencoded::parse(q.as_bytes()).into_owned());
    }
    pairs.into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// `hexEscapeNonASCII`: bytes >= 0x80 become %XX in a redirect target.
fn hex_escape_non_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b < 0x80 {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02x}"));
        }
    }
    out
}

/// `html.EscapeString`.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            c => out.push(c),
        }
    }
    out
}

/// auth.rs: `loginPage(w, next, wrong)`. Always 401, like the original.
fn login_page(next: &str, wrong: bool) -> Response {
    let msg = if wrong {
        r#"<p class="err">Wrong password</p>"#
    } else {
        ""
    };
    let mut page =
        String::with_capacity(LOGIN_PART_1.len() + LOGIN_PART_2.len() + LOGIN_PART_3.len() + 64);
    page.push_str(LOGIN_PART_1);
    page.push_str(msg);
    page.push_str(LOGIN_PART_2);
    page.push_str(&html_escape(next));
    page.push_str(LOGIN_PART_3);
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Body::from(page),
    )
        .into_response()
}

// The login page is copied byte for byte from auth.rs (loginPage). Only `msg` and `next` are
// spliced in, at the same places as the string concatenation.
const LOGIN_PART_1: &str = r##"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="color-scheme" content="dark light">
<script>try{var t=localStorage.getItem("theme")||(matchMedia("(prefers-color-scheme: light)").matches?"light":"dark");document.documentElement.dataset.theme=t}catch(e){}</script>
<title>Agent Browser Use</title>
<link rel="preconnect" href="https://fonts.googleapis.com"><link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&display=swap" rel="stylesheet">
<style>
:root{--bg:#0a0a0c;--card:#111114;--field:#0d0d10;--line:rgba(255,255,255,.22);--fg:#f4f4f5;--muted:#a1a1aa;--dim:#71717a;--accent:#6366f1;--accent-hi:#818cf8;--shadow:rgba(0,0,0,.7);--err:#f87171}
:root[data-theme="light"]{--bg:#e9eaee;--card:#fff;--field:#f1f2f6;--line:rgba(15,15,30,.28);--fg:#14141a;--muted:#52525b;--dim:#71717a;--accent:#4f46e5;--accent-hi:#4338ca;--shadow:rgba(20,20,40,.25);--err:#dc2626}
*{box-sizing:border-box}
body{margin:0;min-height:100vh;display:grid;place-items:center;padding:16px;font:14px/1.5 Inter,ui-sans-serif,system-ui,sans-serif;background:var(--bg);color:var(--fg)}
form{width:100%;max-width:384px;padding:32px;border-radius:2px;border:1px solid var(--line);background:var(--card);box-shadow:0 28px 80px var(--shadow);animation:rise .35s ease both}
@keyframes rise{from{opacity:0;transform:translateY(8px)}to{opacity:1;transform:none}}
.head{display:flex;flex-direction:column;align-items:center;text-align:center;margin-bottom:32px}
.logo{display:grid;place-items:center;width:56px;height:56px;border-radius:2px;background:var(--accent);color:#fff;margin-bottom:16px}
h1{margin:0;font-size:24px;font-weight:600;letter-spacing:-.02em}
.sub{margin:6px 0 0;color:var(--muted)}
label{display:block;margin-bottom:8px;font-size:11px;font-weight:600;letter-spacing:.06em;text-transform:uppercase;color:var(--dim)}
input[type=password]{width:100%;height:44px;padding:0 14px;font:inherit;border-radius:2px;border:1px solid var(--line);background:var(--field);color:var(--fg);outline:none;transition:border-color .15s,box-shadow .15s}
input[type=password]::placeholder{color:var(--dim)}
input[type=password]:focus{border-color:var(--accent);box-shadow:0 0 0 4px rgba(99,102,241,.18)}
button{display:flex;align-items:center;justify-content:center;gap:8px;width:100%;height:44px;margin-top:16px;font:inherit;font-size:15px;font-weight:500;color:#fff;border:0;border-radius:2px;background:var(--accent);cursor:pointer;transition:background .15s,transform .1s}
button:hover{background:var(--accent-hi)}button:active{transform:scale(.98)}
.err{margin:0 0 12px;font-size:13px;color:var(--err)}
.note{margin:20px 0 0;text-align:center;font-size:12px;color:var(--dim)}
</style></head><body><form method="post" action="/login">
<div class="head"><div class="logo"><svg width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2"/><path d="M8 21h8M12 17v4"/><path d="m9 10 2 2 4-4"/></svg></div>
<h1>Agent Browser Use</h1><p class="sub">Watch your AI agent use the web — live.</p></div>"##;
const LOGIN_PART_2: &str = r##"
<input type="hidden" name="next" value=""##;
const LOGIN_PART_3: &str = r##"">
<label for="pw">Password</label>
<input id="pw" type="password" name="password" placeholder="Enter the room password" autofocus required>
<button type="submit">Enter room <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12h14"/><path d="m12 5 7 7-7 7"/></svg></button>
<p class="note">This room is password protected. Ask whoever shared the link.</p>
</form></body></html>"##;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use base64::engine::general_purpose::STANDARD;

    const PW: &str = "hunter2";

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn no_password_allows_everything() {
        assert!(authorized(&HeaderMap::new(), ""));
    }

    #[test]
    fn no_credentials_is_refused() {
        assert!(!authorized(&HeaderMap::new(), PW));
    }

    #[test]
    fn cookie_with_session_token_is_accepted() {
        let cookie = format!("theme=dark; {AUTH_COOKIE}={}", auth_token(PW));
        assert!(authorized(&headers(&[("cookie", &cookie)]), PW));
    }

    #[test]
    fn cookie_with_wrong_token_is_refused() {
        let cookie = format!("{AUTH_COOKIE}={}", auth_token("other"));
        assert!(!authorized(&headers(&[("cookie", &cookie)]), PW));
        // The password itself is not a valid cookie value.
        let cookie = format!("{AUTH_COOKIE}={PW}");
        assert!(!authorized(&headers(&[("cookie", &cookie)]), PW));
    }

    #[test]
    fn only_the_first_cookie_with_the_name_counts() {
        // r.Cookie returns the first match only.
        let cookie = format!("{AUTH_COOKIE}=bad; {AUTH_COOKIE}={}", auth_token(PW));
        assert!(!authorized(&headers(&[("cookie", &cookie)]), PW));
    }

    #[test]
    fn bearer_password_is_accepted() {
        assert!(authorized(
            &headers(&[("authorization", "Bearer hunter2")]),
            PW
        ));
    }

    #[test]
    fn bearer_wrong_password_is_refused() {
        assert!(!authorized(
            &headers(&[("authorization", "Bearer nope")]),
            PW
        ));
        // matches the scheme case-sensitively for Bearer.
        assert!(!authorized(
            &headers(&[("authorization", "bearer hunter2")]),
            PW
        ));
        assert!(!authorized(&headers(&[("authorization", "Bearer")]), PW));
    }

    #[test]
    fn basic_password_is_accepted_whatever_the_user_name() {
        let ok = format!("Basic {}", STANDARD.encode("anyone:hunter2"));
        assert!(authorized(&headers(&[("authorization", &ok)]), PW));
        let empty_user = format!("Basic {}", STANDARD.encode(":hunter2"));
        assert!(authorized(&headers(&[("authorization", &empty_user)]), PW));
        // The scheme name is case-insensitive.
        let lower = format!("basic {}", STANDARD.encode("u:hunter2"));
        assert!(authorized(&headers(&[("authorization", &lower)]), PW));
    }

    #[test]
    fn basic_wrong_password_or_malformed_is_refused() {
        let wrong = format!("Basic {}", STANDARD.encode("u:nope"));
        assert!(!authorized(&headers(&[("authorization", &wrong)]), PW));
        let no_colon = format!("Basic {}", STANDARD.encode("hunter2"));
        assert!(!authorized(&headers(&[("authorization", &no_colon)]), PW));
        assert!(!authorized(&headers(&[("authorization", "Basic !!!")]), PW));
    }

    #[test]
    fn html_escape_matches_go() {
        assert_eq!(
            html_escape(r#"<a href='x'>&"#),
            "&lt;a href=&#39;x&#39;&gt;&amp;"
        );
        assert_eq!(html_escape(r#"""#), "&#34;");
    }

    #[tokio::test]
    async fn login_page_is_401_html_with_escaped_next_and_wrong_notice() {
        let plain = login_page("/x?y=\"1\"", false);
        assert_eq!(plain.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            plain.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let bytes = to_bytes(plain.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains(r#"value="/x?y=&#34;1&#34;">"#));
        assert!(!text.contains("Wrong password"));

        let wrong = login_page("/", true);
        let bytes = to_bytes(wrong.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains(r#"<p class="err">Wrong password</p>"#));
    }
}
