//! Static file serving for the embedded UI, and the path cleaning that sits in front of every
//! route. It follows the standard library's file server and mux, which main.rs relies on, so the
//! byte-level details are kept:
//!   - `/index.html` and `/x/index.html` redirect to `./` (301), a directory without a trailing slash
//!     redirects to `dir/`, a file with a trailing slash redirects to `../file`;
//!   - a directory serves its index.html, else a plain `<pre>` listing;
//!   - embedded files have no modification time, so there is no Last-Modified, and If-Modified-Since
//!     is never answered with 304 (skips both when the time is zero);
//!   - Content-Type from the file extension, Accept-Ranges, single and multiple Range requests, 416;
//!   - 404 body "404 page not found\n";
//!   - ServeMux answers a non-canonical path (`//x`, `/a/../b`) with a 307 to the cleaned one.

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use crate::StaticFiles;

fn text(status: StatusCode, msg: &str) -> Response {
    // http.Error
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header("x-content-type-options", "nosniff")
        .body(Body::from(format!("{msg}\n")))
        .unwrap()
}

/// path.Clean (slash separated, rooted or not).
pub fn path_clean(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let rooted = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|l| *l != "..") {
                    out.pop();
                } else if !rooted {
                    out.push("..");
                }
            }
            s => out.push(s),
        }
    }
    let joined = out.join("/");
    if rooted {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

/// ServeMux's cleanPath: path.Clean that keeps a trailing slash.
fn mux_clean_path(p: &str) -> String {
    if p.is_empty() {
        return "/".into();
    }
    let p = if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    };
    let mut np = path_clean(&p);
    if p.ends_with('/') && np != "/" {
        if p.len() == np.len() + 1 && p.starts_with(&np) {
            np = p;
        } else {
            np.push('/');
        }
    }
    np
}

/// url.PathUnescape-ish: %XX to bytes (invalid escapes stay as they are).
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(v) = h {
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

/// Encode a path the way url.URL.String does (keeps "/", escapes spaces, non-ASCII, "?", "#", "%").
fn escape_path(p: &str) -> String {
    let mut o = String::new();
    for &c in p.as_bytes() {
        let keep = c.is_ascii_alphanumeric() || b"-_.~/!$&'()*+,;=:@".contains(&c);
        if keep {
            o.push(c as char);
        } else {
            o.push_str(&format!("%{c:02X}"));
        }
    }
    o
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&#34;")
        .replace('\'', "&#39;")
}

/// http.Redirect for GET/HEAD (body with a link) and others (no body).
fn redirect(method: &Method, url: &str, code: StatusCode) -> Response {
    let mut r = Response::builder()
        .status(code)
        .header(header::LOCATION, url);
    let body = if method == Method::GET || method == Method::HEAD {
        r = r.header(header::CONTENT_TYPE, "text/html; charset=utf-8");
        let b = format!(
            "<a href=\"{}\">{}</a>.\n\n",
            html_escape(url),
            code.canonical_reason().unwrap_or("")
        );
        if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(b)
        }
    } else {
        Body::empty()
    };
    r.body(body).unwrap()
}

/// FileServer's localRedirect: keeps the query, 301.
fn local_redirect(req_query: Option<&str>, new_path: &str) -> Response {
    let mut loc = new_path.to_string();
    if let Some(q) = req_query {
        loc.push('?');
        loc.push_str(q);
    }
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header(header::LOCATION, loc)
        .body(Body::empty())
        .unwrap()
}

/// Middleware: ServeMux cleans every path before routing (`//x`, `/a/../b` get a 307).
pub async fn clean_path(req: Request, next: Next) -> Response {
    let raw = req.uri().path();
    let decoded = percent_decode(raw);
    let cleaned = mux_clean_path(&decoded);
    if req.method() != Method::CONNECT && cleaned != decoded {
        let mut loc = escape_path(&cleaned);
        if let Some(q) = req.uri().query() {
            loc.push('?');
            loc.push_str(q);
        }
        return redirect(req.method(), &loc, StatusCode::TEMPORARY_REDIRECT);
    }
    next.run(req).await
}

/// mime.TypeByExtension with built-in table.
fn type_by_ext(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "avif" => "image/avif",
        "css" => "text/css; charset=utf-8",
        "gif" => "image/gif",
        "htm" | "html" => "text/html; charset=utf-8",
        "jpeg" | "jpg" => "image/jpeg",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "wasm" => "application/wasm",
        "webp" => "image/webp",
        "xml" => "text/xml; charset=utf-8",
        "ico" => "image/vnd.microsoft.icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "txt" => "text/plain; charset=utf-8",
        _ => return None,
    })
}

/// http.DetectContentType, reduced to what matters for files without a known extension.
fn sniff(data: &[u8]) -> &'static str {
    let head = &data[..data.len().min(512)];
    if head
        .iter()
        .any(|&b| b < 0x09 || (b > 0x0d && b < 0x20 && b != 0x1b))
    {
        "application/octet-stream"
    } else {
        "text/plain; charset=utf-8"
    }
}

struct Range {
    start: u64,
    length: u64,
}

enum RangeErr {
    Invalid,
    NoOverlap,
}

/// parseRange.
fn parse_range(s: &str, size: u64) -> Result<Vec<Range>, RangeErr> {
    if s.is_empty() {
        return Ok(vec![]);
    }
    let Some(rest) = s.strip_prefix("bytes=") else {
        return Err(RangeErr::Invalid);
    };
    let mut ranges = Vec::new();
    let mut no_overlap = false;
    for ra in rest.split(',') {
        let ra = ra.trim_matches([' ', '\t']);
        if ra.is_empty() {
            continue;
        }
        let Some((start, end)) = ra.split_once('-') else {
            return Err(RangeErr::Invalid);
        };
        let (start, end) = (
            start.trim_matches([' ', '\t']),
            end.trim_matches([' ', '\t']),
        );
        let r = if start.is_empty() {
            // suffix: the last N bytes
            if end.is_empty() || end.starts_with('-') {
                return Err(RangeErr::Invalid);
            }
            let i: i64 = end.parse().map_err(|_| RangeErr::Invalid)?;
            if i < 0 {
                return Err(RangeErr::Invalid);
            }
            let i = (i as u64).min(size);
            Range {
                start: size - i,
                length: i,
            }
        } else {
            let i: i64 = start.parse().map_err(|_| RangeErr::Invalid)?;
            if i < 0 {
                return Err(RangeErr::Invalid);
            }
            let i = i as u64;
            if i >= size {
                // "If the range begins after the size of the content, then it does not overlap."
                no_overlap = true;
                continue;
            }
            if end.is_empty() {
                Range {
                    start: i,
                    length: size - i,
                }
            } else {
                let e: i64 = end.parse().map_err(|_| RangeErr::Invalid)?;
                if e < 0 || i > e as u64 {
                    return Err(RangeErr::Invalid);
                }
                let e = (e as u64).min(size - 1);
                Range {
                    start: i,
                    length: e - i + 1,
                }
            }
        };
        ranges.push(r);
    }
    if no_overlap && ranges.is_empty() {
        return Err(RangeErr::NoOverlap);
    }
    Ok(ranges)
}

/// http.ServeContent for content with no modification time and no ETag.
fn serve_content(req: &Request, name: &str, data: Bytes) -> Response {
    let ctype = type_by_ext(name).unwrap_or_else(|| sniff(&data));
    let size = data.len() as u64;
    let head = req.method() == Method::HEAD;
    let mut b = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, ctype);

    // If-Range can only be satisfied by an ETag or Last-Modified, neither of which we send.
    let range_hdr = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let has_if_range = req.headers().contains_key(header::IF_RANGE);
    let mut ranges = Vec::new();
    if !range_hdr.is_empty() && !has_if_range && (req.method() == Method::GET || head) {
        match parse_range(range_hdr, size) {
            Ok(r) => ranges = r,
            Err(e) => {
                let mut r = text(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    match e {
                        RangeErr::Invalid => "invalid range",
                        RangeErr::NoOverlap => "invalid range: failed to overlap",
                    },
                );
                if matches!(e, RangeErr::NoOverlap) {
                    r.headers_mut().insert(
                        header::CONTENT_RANGE,
                        HeaderValue::from_str(&format!("bytes */{size}")).unwrap(),
                    );
                }
                return r;
            }
        }
        // The sum of the ranges must not be larger than the file: then serve it whole.
        if ranges.iter().map(|r| r.length).sum::<u64>() > size {
            ranges.clear();
        }
    }
    let (status, body) = match ranges.len() {
        0 => (StatusCode::OK, data),
        1 => {
            let r = &ranges[0];
            b = b.header(
                header::CONTENT_RANGE,
                format!("bytes {}-{}/{}", r.start, r.start + r.length - 1, size),
            );
            (
                StatusCode::PARTIAL_CONTENT,
                data.slice(r.start as usize..(r.start + r.length) as usize),
            )
        }
        _ => {
            let boundary: String = (0..30)
                .map(|_| format!("{:x}", rand::random::<u8>() & 0xf))
                .collect();
            let mut buf = Vec::new();
            for r in &ranges {
                buf.extend_from_slice(
                    format!(
                        "\r\n--{boundary}\r\nContent-Range: bytes {}-{}/{}\r\nContent-Type: {ctype}\r\n\r\n",
                        r.start,
                        r.start + r.length - 1,
                        size
                    )
                    .as_bytes(),
                );
                buf.extend_from_slice(&data[r.start as usize..(r.start + r.length) as usize]);
            }
            buf.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
            // multipart: starts the first part without the leading CRLF
            let buf = buf[2..].to_vec();
            b = b.header(
                header::CONTENT_TYPE,
                format!("multipart/byteranges; boundary={boundary}"),
            );
            (StatusCode::PARTIAL_CONTENT, Bytes::from(buf))
        }
    };
    b = b.header(header::CONTENT_LENGTH, body.len());
    b.status(status)
        .body(if head {
            Body::empty()
        } else {
            Body::from(body)
        })
        .unwrap()
}

/// Directory listing (dirList).
fn dir_list(prefix: &str) -> String {
    let mut names: Vec<String> = Vec::new();
    for f in StaticFiles::iter() {
        let Some(rest) = f.strip_prefix(prefix) else {
            continue;
        };
        let name = match rest.split_once('/') {
            Some((d, _)) => format!("{d}/"),
            None => rest.to_string(),
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names.sort();
    let mut s = String::from(
        "<!doctype html>\n<meta name=\"viewport\" content=\"width=device-width\">\n<pre>\n",
    );
    for n in names {
        s.push_str(&format!(
            "<a href=\"{}\">{}</a>\n",
            escape_path(&n).replace('"', "%22"),
            html_escape(&n)
        ));
    }
    s.push_str("</pre>\n");
    s
}

/// `http.FileServer(http.FS(sub))` as the router's fallback.
pub async fn file_server(req: Request) -> Response {
    let url_path = percent_decode(req.uri().path());
    let query = req.uri().query().map(str::to_string);
    let url_path = if url_path.starts_with('/') {
        url_path
    } else {
        format!("/{url_path}")
    };
    // serveFile(w, r, fs, path.Clean(upath), redirect=true)
    let name = path_clean(&url_path);
    if url_path.ends_with("/index.html") {
        return local_redirect(query.as_deref(), "./");
    }
    let rel = name.trim_start_matches('/');
    let file = if rel.is_empty() {
        None
    } else {
        StaticFiles::get(rel)
    };
    let prefix = if rel.is_empty() {
        String::new()
    } else {
        format!("{rel}/")
    };
    let is_dir =
        file.is_none() && (rel.is_empty() || StaticFiles::iter().any(|f| f.starts_with(&prefix)));
    if file.is_none() && !is_dir {
        return text(StatusCode::NOT_FOUND, "404 page not found");
    }
    let base = |p: &str| p.rsplit('/').next().unwrap_or("").to_string();
    if is_dir {
        if !url_path.ends_with('/') {
            return local_redirect(query.as_deref(), &format!("{}/", base(&url_path)));
        }
        if let Some(idx) = StaticFiles::get(&format!("{prefix}index.html")) {
            return serve_content(&req, "index.html", Bytes::from(idx.data.into_owned()));
        }
        let body = dir_list(&prefix);
        let head = req.method() == Method::HEAD;
        return Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CONTENT_LENGTH, body.len())
            .body(if head {
                Body::empty()
            } else {
                Body::from(body)
            })
            .unwrap();
    }
    if url_path.ends_with('/') {
        return local_redirect(
            query.as_deref(),
            &format!("../{}", base(&url_path[..url_path.len() - 1])),
        );
    }
    let f = file.unwrap();
    serve_content(&req, &base(rel), Bytes::from(f.data.into_owned()))
}
