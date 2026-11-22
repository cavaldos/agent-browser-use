//! ProxySwitch lets the shared browser change its public IP while it runs. Chrome is
//! pointed at a tiny SOCKS5 server on 127.0.0.1 (this process); each connection it
//! opens is dialled either directly or through the upstream proxy (e.g. Cloudflare
//! WARP). Switching only changes where new connections go, so no browser restart.
//! Nothing outside the browser is touched.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State as AxState};
use axum::http::{header, Method, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{lookup_host, TcpListener, TcpStream};
use tokio::task::AbortHandle;
use tokio::time::{sleep, timeout, timeout_at};
use tokio_socks::tcp::Socks5Stream;

use crate::golog;
use crate::keepalive::via_tunnel;

/// netproxy.rs `15 * time.Second` (dial and SOCKS handshake limits)
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
/// netproxy.rs `Timeout: 12 * time.Second` of the IP lookup client
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(12);

/// Anything a connection can carry: a TCP socket or a built-in tunnel stream.
pub trait Conn: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Conn for T {}
pub type BoxedConn = Box<dyn Conn>;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The built-in upstream. Implemented by
/// `warp::WarpTunnel`.
pub trait BuiltinUpstream: Send + Sync {
    /// warp.rs `net`: the first switch-on signs up and starts the tunnel.
    fn start(&self) -> BoxFut<'_, Result<(), String>>;
    /// warp.rs `dial(ctx, addr)`.
    fn dial<'a>(&'a self, addr: &'a str) -> BoxFut<'a, io::Result<BoxedConn>>;
}

pub struct ProxySwitch {
    upstream: String, // host:port of an upstream SOCKS5 proxy (-proxy); "" = built-in WARP
    warp: Option<Arc<dyn BuiltinUpstream>>, // built-in WARP, used when there is no upstream
    st: Mutex<State>,
}

#[derive(Default)]
struct State {
    on: bool,
    listen: Option<SocketAddr>,
    /// open connections from Chrome, aborted on every switch
    conns: HashMap<u64, AbortHandle>,
    next_id: u64,
    gen: u64,
    ip: String,
    loc: String,
    warp_on: String,  // "on"/"plus"/"off" as Cloudflare sees the browser
    warp_err: String, // why the built-in WARP could not start
    checking: bool,
    err: String,
}

impl State {
    fn clear_status(&mut self) {
        self.ip.clear();
        self.loc.clear();
        self.warp_on.clear();
        self.err.clear();
        self.warp_err.clear();
    }
}

/// Parses an upstream like socks5://warp:1080. Empty = built-in WARP,
/// whose account lives in warpFile ("" = no WARP: the switch is not offered).
impl ProxySwitch {
    pub fn new(raw: &str, warp_file: &str) -> Result<Arc<Self>, String> {
        let mut p = ProxySwitch {
            upstream: String::new(),
            warp: None,
            st: Mutex::new(State::default()),
        };
        if raw.is_empty() {
            if !warp_file.is_empty() {
                // starts off: the switch turns it on
                p.warp = Some(Arc::new(crate::warp::WarpTunnel::new(warp_file)));
            }
            return Ok(Arc::new(p));
        }
        let raw = if raw.contains("://") {
            raw.to_string()
        } else {
            format!("socks5://{raw}")
        };
        let bad = || format!("bad -proxy {raw:?} (want socks5://host:port)");
        let u = url::Url::parse(&raw).map_err(|_| bad())?;
        let host = u.host_str().filter(|h| !h.is_empty()).ok_or_else(bad)?;
        // u.Host: host and port, without userinfo
        p.upstream = match u.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        if u.scheme() != "socks5" && u.scheme() != "socks5h" {
            return Err(format!(
                "-proxy {raw:?}: only socks5:// is supported (Cloudflare WARP exposes SOCKS5)"
            ));
        }
        p.st().on = true;
        Ok(Arc::new(p))
    }

    fn st(&self) -> MutexGuard<'_, State> {
        match self.st.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        }
    }

    pub fn configured(&self) -> bool {
        !self.upstream.is_empty() || self.warp.is_some()
    }

    fn is_on(&self) -> bool {
        self.st().on
    }

    fn untrack(&self, id: u64) {
        self.st().conns.remove(&id);
    }

    /// Listen starts the local SOCKS5 server and returns the address Chrome should use.
    pub async fn listen(self: &Arc<Self>) -> io::Result<String> {
        let ln = TcpListener::bind("127.0.0.1:0").await?;
        let addr = ln.local_addr()?;
        self.st().listen = Some(addr);
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                let Ok((c, _)) = ln.accept().await else {
                    return;
                };
                // Held across spawn + insert so the task's untrack waits for the entry.
                let mut st = me.st();
                let id = st.next_id;
                st.next_id += 1;
                let me2 = me.clone();
                let h = tokio::spawn(async move {
                    me2.serve(c).await;
                    me2.untrack(id);
                });
                st.conns.insert(id, h.abort_handle());
            }
        });
        Ok(format!("socks5://{addr}"))
    }

    /// dial opens a connection the way the browser's traffic currently goes.
    async fn dial(&self, addr: &str) -> io::Result<BoxedConn> {
        if self.is_on() {
            if let Some(w) = &self.warp {
                return w.dial(addr).await;
            }
        }
        if !self.is_on() || self.upstream.is_empty() {
            return Ok(Box::new(dial_direct(addr).await?));
        }
        dial_via_socks(&self.upstream, addr).await
    }

    /// serve handles one connection from Chrome: SOCKS5 CONNECT only.
    async fn serve(&self, mut c: TcpStream) {
        let deadline = tokio::time::Instant::now() + DIAL_TIMEOUT;
        let addr = match timeout_at(deadline, read_request(&mut c)).await {
            Ok(Some(Req::Connect(a))) => a,
            Ok(Some(Req::Refuse)) => {
                let _ = c.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await;
                return;
            }
            _ => return,
        };
        let out = match self.dial(&addr).await {
            Ok(o) => o,
            Err(_) => {
                let _ = c.write_all(&[5, 4, 0, 1, 0, 0, 0, 0, 0, 0]).await;
                return;
            }
        };
        if c.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.is_err() {
            return;
        }
        let (mut cr, mut cw) = tokio::io::split(c);
        let (mut or, mut ow) = tokio::io::split(out);
        // one direction ending ends the connection (both sockets are closed).
        tokio::select! {
            _ = tokio::io::copy(&mut cr, &mut ow) => {}
            _ = tokio::io::copy(&mut or, &mut cw) => {}
        }
    }

    /// Set switches the browser between its own IP route and the upstream proxy. Open
    /// connections are closed so the next requests use the new route.
    pub fn set(self: &Arc<Self>, on: bool) {
        {
            let mut st = self.st();
            if !self.configured() || st.on == on {
                return;
            }
            st.on = on;
            st.gen += 1;
            st.clear_status();
            st.checking = true;
            for (_, h) in st.conns.drain() {
                h.abort();
            }
        }
        let me = self.clone();
        tokio::spawn(async move {
            if let (Some(w), true) = (&me.warp, on) {
                // first time: sign up + start the tunnel
                if let Err(e) = w.start().await {
                    {
                        let mut st = me.st();
                        st.on = false;
                        st.warp_err = e.clone();
                        st.gen += 1;
                    }
                    golog(&format!("warp: {e}"));
                    me.refresh().await;
                    return;
                }
            }
            me.refresh().await;
        });
    }

    /// Refresh looks up the public IP the browser currently has (through the same route).
    pub async fn refresh(&self) {
        let gen = {
            let mut st = self.st();
            st.checking = true;
            st.gen
        };
        let res = self.lookup().await;
        let mut st = self.st();
        if gen != st.gen {
            // switched meanwhile: that switch starts its own lookup
            return;
        }
        st.checking = false;
        match res {
            Err(e) => {
                st.ip.clear();
                st.loc.clear();
                st.warp_on.clear();
                st.err = e;
            }
            Ok((ip, loc, warp)) => {
                st.ip = ip;
                st.loc = loc;
                st.warp_on = warp;
                st.err.clear();
            }
        }
    }

    async fn lookup(&self) -> Result<(String, String, String), String> {
        let listen = self.st().listen;
        let mut b = reqwest::Client::builder()
            .timeout(LOOKUP_TIMEOUT)
            .no_proxy()
            .pool_max_idle_per_host(0); // DisableKeepAlives
                                        // Through the local switch, so the lookup takes the same route as the browser.
                                        // Without a listener (no switch configured) the route is direct anyway.
        if let Some(a) = listen {
            b = b.proxy(reqwest::Proxy::all(format!("socks5h://{a}")).map_err(|e| e.to_string())?);
        }
        let cl = b.build().map_err(|e| e.to_string())?;
        // 1.1.1.1 is an IPv4 address, so the reply shows the IPv4 the browser goes out with.
        let resp = match cl.get("https://1.1.1.1/cdn-cgi/trace").send().await {
            Ok(r) => r,
            Err(_) => cl
                .get("https://www.cloudflare.com/cdn-cgi/trace")
                .send()
                .await
                .map_err(|e| e.to_string())?,
        };
        let b = resp.bytes().await.unwrap_or_default();
        let text = String::from_utf8_lossy(&b[..b.len().min(8192)]).into_owned();
        let (mut ip, mut loc, mut warp) = (String::new(), String::new(), String::new());
        for line in text.split('\n') {
            let Some((k, v)) = line.trim().split_once('=') else {
                continue;
            };
            match k {
                "ip" => ip = v.to_string(),
                "loc" => loc = v.to_string(),
                "warp" => warp = v.to_string(),
                _ => {}
            }
        }
        if ip.is_empty() {
            return Err("no IP in reply".to_string());
        }
        Ok((ip, loc, warp))
    }

    /// Watch keeps the IP fresh so the header always shows the current one.
    pub async fn watch(self: Arc<Self>) {
        loop {
            self.refresh().await;
            let mut wait = Duration::from_secs(30);
            if !self.status().error.is_empty() {
                // e.g. WARP still shaking hands: look again soon
                wait = Duration::from_secs(5);
            }
            sleep(wait).await;
        }
    }

    pub fn status(&self) -> ProxyStatus {
        let st = self.st();
        ProxyStatus {
            configured: self.configured(),
            on: st.on,
            ip: st.ip.clone(),
            loc: st.loc.clone(),
            warp: st.warp_on.clone(),
            checking: st.checking,
            error: st.err.clone(),
            warp_error: st.warp_err.clone(),
            can_toggle: false,
        }
    }
}

/// proxyStatus (JSON field order)
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyStatus {
    pub configured: bool,
    pub on: bool,
    pub ip: String,
    pub loc: String,
    pub warp: String,
    pub checking: bool,
    pub error: String,
    pub warp_error: String,
    pub can_toggle: bool,
}

/// startProxy: the switch, and the address Chrome uses ("" = no switch).
/// calls log.Fatal on failure; here the error goes back to the caller.
pub async fn start_proxy(raw: &str, warp_file: &str) -> Result<(Arc<ProxySwitch>, String), String> {
    let p = ProxySwitch::new(raw, warp_file)?;
    let mut addr = String::new();
    if p.configured() {
        addr = p.listen().await.map_err(|e| format!("proxy: {e}"))?;
    }
    tokio::spawn(p.clone().watch());
    Ok((p, addr))
}

/// registerProxyAPI: GET /api/proxy -> status;  POST /api/proxy {"on":true|false}
pub fn routes(p: Arc<ProxySwitch>) -> Router {
    Router::new()
        .route("/api/proxy", any(api_proxy))
        .with_state(p)
}

async fn api_proxy(AxState(p): AxState<Arc<ProxySwitch>>, req: Request) -> Response {
    let can = !via_tunnel(req.headers());
    let (parts, body) = req.into_parts();
    if parts.method == Method::POST {
        if !can {
            return forbidden("the IP can only be switched from the machine that runs the app");
        }
        // `json.NewDecoder(io.LimitReader(r.Body, 4096)).Decode(&req)` with the error ignored
        let bytes = to_bytes(body, 4096).await.unwrap_or_default();
        let on = serde_json::Deserializer::from_slice(&bytes)
            .into_iter::<Value>()
            .next()
            .and_then(Result::ok)
            .and_then(|v| v.get("on").and_then(Value::as_bool))
            .unwrap_or(false);
        p.set(on);
    }
    let mut s = p.status();
    s.can_toggle = can;
    let mut out = serde_json::to_vec(&s).expect("proxy status serialises");
    out.push(b'\n'); // json.Encoder appends a newline
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(out))
        .expect("static response")
}

/// http.Error(w, msg, 403)
fn forbidden(msg: &str) -> Response {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header("x-content-type-options", "nosniff")
        .body(Body::from(format!("{msg}\n")))
        .expect("static response")
}

/// Result of reading Chrome's request: a CONNECT target, or a refusal to answer with.
enum Req {
    Connect(String),
    Refuse,
}

/// The server side of Chrome's handshake (greeting, then CONNECT request).
/// Returns None where returns without replying.
async fn read_request<C: AsyncRead + AsyncWrite + Unpin>(c: &mut C) -> Option<Req> {
    let mut h = [0u8; 2];
    c.read_exact(&mut h).await.ok()?;
    if h[0] != 5 {
        return None;
    }
    let mut methods = vec![0u8; h[1] as usize];
    c.read_exact(&mut methods).await.ok()?;
    c.write_all(&[5, 0]).await.ok()?;
    let mut r = [0u8; 4];
    if c.read_exact(&mut r).await.is_err() || r[0] != 5 || r[1] != 1 {
        return Some(Req::Refuse);
    }
    let host = match r[3] {
        1 => {
            let mut b = [0u8; 4];
            c.read_exact(&mut b).await.ok()?;
            Ipv4Addr::from(b).to_string()
        }
        4 => {
            let mut b = [0u8; 16];
            c.read_exact(&mut b).await.ok()?;
            Ipv6Addr::from(b).to_string()
        }
        3 => {
            let mut l = [0u8; 1];
            c.read_exact(&mut l).await.ok()?;
            let mut b = vec![0u8; l[0] as usize];
            c.read_exact(&mut b).await.ok()?;
            String::from_utf8_lossy(&b).into_owned()
        }
        _ => return None,
    };
    let mut pb = [0u8; 2];
    c.read_exact(&mut pb).await.ok()?;
    let port = u16::from_be_bytes(pb);
    // join host and port (IPv6 gets brackets)
    let addr = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    Some(Req::Connect(addr))
}

async fn connect_one(a: SocketAddr) -> io::Result<TcpStream> {
    match timeout(DIAL_TIMEOUT, TcpStream::connect(a)).await {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "i/o timeout")),
    }
}

/// The direct route. IPv4 first, so sites see the same IPv4 the header shows; IPv6-only
/// hosts still work.
async fn dial_direct(addr: &str) -> io::Result<TcpStream> {
    let all: Vec<SocketAddr> = lookup_host(addr).await?.collect();
    let mut last = None;
    for a in all.iter().filter(|a| a.is_ipv4()) {
        match connect_one(*a).await {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    for a in &all {
        match connect_one(*a).await {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such host")))
}

/// The upstream route: CONNECT through the SOCKS5 proxy. The host is sent as a name, so
/// DNS is resolved by the proxy, not by this machine.
async fn dial_via_socks(upstream: &str, addr: &str) -> io::Result<BoxedConn> {
    let c = match timeout(DIAL_TIMEOUT, TcpStream::connect(upstream)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Err(io::Error::other(format!("proxy {upstream}: {e}"))),
        Err(_) => return Err(io::Error::other(format!("proxy {upstream}: i/o timeout"))),
    };
    let (host, port) = split_host_port(addr)?;
    match timeout(
        DIAL_TIMEOUT,
        Socks5Stream::connect_with_socket(c, (host, port)),
    )
    .await
    {
        Ok(Ok(s)) => Ok(Box::new(s)),
        Ok(Err(e)) => Err(io::Error::other(e)),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "socks5: handshake timeout",
        )),
    }
}

/// Splits "host:port" and "[v6]:port".
fn split_host_port(addr: &str) -> io::Result<(&str, u16)> {
    let bad = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("address {addr}: missing port"),
        )
    };
    let (host, port) = if let Some(rest) = addr.strip_prefix('[') {
        let (h, p) = rest.split_once("]:").ok_or_else(bad)?;
        (h, p)
    } else {
        addr.rsplit_once(':').ok_or_else(bad)?
    };
    let port: u16 = port
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(local: &str, url: &str) -> String {
        // socks5h: the target host is resolved by the proxy side, like Chrome's socks5://
        let cl = reqwest::Client::builder()
            .no_proxy()
            .proxy(reqwest::Proxy::all(local.replacen("socks5://", "socks5h://", 1)).unwrap())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        cl.get(url).send().await.unwrap().text().await.unwrap()
    }

    // Chrome -> local SOCKS5 -> (switch on) upstream SOCKS5 -> target, then switched off.
    #[tokio::test]
    async fn proxy_switch_chain() {
        let srv = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", srv.local_addr().unwrap());
        tokio::spawn(async move {
            let app = Router::new().fallback(|| async { "hi" });
            axum::serve(srv, app).await.unwrap();
        });

        let up = ProxySwitch::new("", "").unwrap(); // plays the upstream: a direct SOCKS5 server
        let up_addr = up.listen().await.unwrap();
        let p = ProxySwitch::new(&up_addr, "").unwrap();
        let local = p.listen().await.unwrap();

        assert_eq!(get(&local, &url).await, "hi", "on");
        p.set(false);
        assert!(!p.status().on, "still on");
        assert_eq!(get(&local, &url).await, "hi", "off");
    }
}
