//! Built-in Cloudflare WARP: a WireGuard tunnel that lives entirely inside this process
//! (userspace network stack). It creates no network interface and changes no route, so
//! the machine keeps its own IP; only connections dialled through it (the agent
//! browser's, via ProxySwitch) leave with a Cloudflare IP.
//!
//! uses wireguard-go + gVisor netstack; here boringtun (WireGuard state machine) is
//! driven by one tokio task that also owns a smoltcp userspace TCP/IP stack and a UDP
//! socket to the peer. Each dialled connection is a smoltcp TCP socket bridged to the
//! caller through an in-memory duplex pipe.

use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};
use futures_util::task::{waker, ArcWake};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::dns::{self, GetQueryResultError, QueryHandle};
use smoltcp::socket::tcp;
use smoltcp::time::Instant as SInstant;
use smoltcp::wire::{DnsQueryType, HardwareAddress, IpAddress, IpCidr, IpEndpoint};
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};
use tokio::net::{lookup_host, UdpSocket};
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio::time::{sleep, timeout};

use crate::netproxy::{BoxFut, BoxedConn, BuiltinUpstream};

/// warp.rs `netstack.CreateNetTUN(addrs, dns, 1280)`
const MTU: usize = 1280;
/// warp.rs `persistent_keepalive_interval=25`
const KEEPALIVE: u16 = 25;
/// warp.rs `warpPeerKey`
const WARP_PEER_KEY: &str = "bmXOC+F1FxEMF9dyiK2H5/1SUtzH0JuVo51h2wPfgyo=";
/// warp.rs `(&http.Client{Timeout: 20 * time.Second})`
const REGISTER_TIMEOUT: Duration = Duration::from_secs(20);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const PIPE: usize = 64 * 1024;

/// warp.rs `warpAccount`: saved account + keys (warp.json, private).
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct WarpAccount {
    #[serde(default)]
    pub private_key: String, // base64
    #[serde(default)]
    pub peer_key: String, // base64
    #[serde(default)]
    pub endpoint: String, // ip:port
    #[serde(default)]
    pub addr_v4: String,
    #[serde(default)]
    pub addr_v6: String,
}

/// warp.rs `load` (parsing half)
fn parse_account(b: &[u8]) -> Result<WarpAccount, String> {
    let a: WarpAccount = serde_json::from_slice(b).map_err(|e| e.to_string())?;
    if a.private_key.is_empty() || a.addr_v4.is_empty() {
        return Err("incomplete warp account".into());
    }
    Ok(a)
}

/// warp.rs `save`: indented JSON, mode 0600, parent folder created.
fn save_account(file: &PathBuf, a: &WarpAccount) -> io::Result<()> {
    if let Some(d) = file.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let b = serde_json::to_vec_pretty(a).unwrap_or_default();
    std::fs::write(file, b)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // os.WriteFile applies 0600 only when it creates the file
        let _ = std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// time.Now.UTC.Format(time.RFC3339)
fn now_utc_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // civil from days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// warp.rs `warpRegister` (reply half): the saved account for a sign-up reply.
fn account_from_reply(private_key: &[u8; 32], b: &[u8]) -> Result<WarpAccount, String> {
    let unexpected = || "WARP sign-up: unexpected reply".to_string();
    let r: Value = serde_json::from_slice(b).map_err(|_| unexpected())?;
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    let p = r["config"]["peers"].get(0).ok_or_else(unexpected)?;
    let v4 = s(&r["config"]["interface"]["addresses"]["v4"]);
    if v4.is_empty() {
        return Err(unexpected());
    }
    let mut a = WarpAccount {
        private_key: B64.encode(private_key),
        peer_key: s(&p["public_key"]),
        addr_v4: v4,
        addr_v6: s(&r["config"]["interface"]["addresses"]["v6"]),
        endpoint: "162.159.192.1:2408".into(),
    };
    if a.peer_key.is_empty() {
        a.peer_key = WARP_PEER_KEY.into();
    }
    if let Some((h, _)) = split_host_port(&s(&p["endpoint"]["v4"])) {
        if !h.is_empty() {
            a.endpoint = join_host_port(&h, "2408"); // the API reports port 0
        }
    }
    Ok(a)
}

/// net.SplitHostPort (the cases that matter here)
fn split_host_port(s: &str) -> Option<(String, String)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (h, p) = rest.split_once("]:")?;
        return Some((h.to_string(), p.to_string()));
    }
    let (h, p) = s.rsplit_once(':')?;
    if h.contains(':') {
        return None;
    }
    Some((h.to_string(), p.to_string()))
}

/// net.JoinHostPort
fn join_host_port(h: &str, p: &str) -> String {
    if h.contains(':') {
        format!("[{h}]:{p}")
    } else {
        format!("{h}:{p}")
    }
}

/// warp.rs `warpRegister`: creates a free WARP account for a new WireGuard key.
async fn register() -> Result<WarpAccount, String> {
    let key: [u8; 32] = rand::random();
    let public = PublicKey::from(&StaticSecret::from(key));
    let body = json!({
        "key": B64.encode(public.as_bytes()), "install_id": "", "fcm_token": "",
        "tos": now_utc_rfc3339(), "model": "PC", "serial_number": "", "locale": "en_US",
    });
    let signup = |e: String| format!("WARP sign-up: {e}");
    let client = reqwest::Client::builder()
        .timeout(REGISTER_TIMEOUT)
        .build()
        .map_err(|e| signup(e.to_string()))?;
    let resp = client
        .post("https://api.cloudflareclient.com/v0a2158/reg")
        .header("Content-Type", "application/json")
        .header("User-Agent", "okhttp/3.12.1")
        .header("CF-Client-Version", "a-6.10-2158")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| signup(e.to_string()))?;
    let status = resp.status();
    let mut b = resp
        .bytes()
        .await
        .map_err(|e| signup(e.to_string()))?
        .to_vec();
    b.truncate(1 << 16);
    if !status.is_success() {
        let t = String::from_utf8_lossy(&b[..b.len().min(200)])
            .trim()
            .to_string();
        return Err(signup(format!("HTTP {} {t}", status.as_u16())));
    }
    account_from_reply(&key, &b)
}

/// What `net` derives from an account before starting the tunnel.
#[derive(Debug, PartialEq)]
struct TunnelConfig {
    addrs: Vec<IpAddr>,
    dns: Vec<IpAddr>,
    private: [u8; 32],
    peer: [u8; 32],
    endpoint: String,
}

/// warp.rs `net` (config half): addresses without `/CIDR`, Cloudflare DNS, decoded keys.
fn build_config(a: &WarpAccount) -> Result<TunnelConfig, String> {
    let addrs = [&a.addr_v4, &a.addr_v6]
        .iter()
        .filter_map(|s| s.split('/').next().and_then(|x| x.parse::<IpAddr>().ok()))
        .collect();
    let dec = |s: &str| -> Option<[u8; 32]> { B64.decode(s).ok()?.try_into().ok() };
    let (Some(private), Some(peer)) = (dec(&a.private_key), dec(&a.peer_key)) else {
        return Err("warp.json: bad keys (delete it to sign up again)".into());
    };
    Ok(TunnelConfig {
        addrs,
        dns: vec!["1.1.1.1".parse().unwrap(), "1.0.0.1".parse().unwrap()],
        private,
        peer,
        endpoint: a.endpoint.clone(),
    })
}

fn ip_addr(a: IpAddr) -> IpAddress {
    match a {
        IpAddr::V4(v) => IpAddress::Ipv4(v),
        IpAddr::V6(v) => IpAddress::Ipv6(v),
    }
}

// ---- smoltcp device: IP packets in memory, shuttled to/from boringtun ----

#[derive(Default)]
struct Pipe {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}

struct PRx(Vec<u8>);
struct PTx<'a>(&'a mut VecDeque<Vec<u8>>);

impl RxToken for PRx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}
impl TxToken for PTx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut b = vec![0u8; len];
        let r = f(&mut b);
        self.0.push_back(b);
        r
    }
}
impl Device for Pipe {
    type RxToken<'a> = PRx;
    type TxToken<'a> = PTx<'a>;
    fn receive(&mut self, _: SInstant) -> Option<(PRx, PTx<'_>)> {
        let p = self.rx.pop_front()?;
        Some((PRx(p), PTx(&mut self.tx)))
    }
    fn transmit(&mut self, _: SInstant) -> Option<PTx<'_>> {
        Some(PTx(&mut self.tx))
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ip;
        c.max_transmission_unit = MTU;
        c
    }
}

// ---- the tunnel task ----

enum Cmd {
    Resolve {
        name: String,
        v6: bool,
        reply: oneshot::Sender<Result<Vec<IpAddr>, String>>,
    },
    Dial {
        to: SocketAddr,
        reply: oneshot::Sender<io::Result<DuplexStream>>,
    },
}

struct Conn {
    sock: SocketHandle,
    pipe: DuplexStream, // this task's end; the caller holds the other
    reply: Option<oneshot::Sender<io::Result<DuplexStream>>>,
    app: Option<DuplexStream>,
    eof_sent: bool,
    fin_sent: bool,
}

struct Query {
    handle: QueryHandle,
    reply: oneshot::Sender<Result<Vec<IpAddr>, String>>,
}

struct Wake(Arc<Notify>);
impl ArcWake for Wake {
    fn wake_by_ref(a: &Arc<Self>) {
        a.0.notify_one();
    }
}

struct Task {
    tunn: Tunn,
    udp: UdpSocket,
    dev: Pipe,
    iface: Interface,
    socks: SocketSet<'static>,
    dns: SocketHandle,
    conns: Vec<Conn>,
    queries: Vec<Query>,
    start: Instant,
    next_port: u16,
    notify: Arc<Notify>,
}

impl Task {
    fn now(&self) -> SInstant {
        SInstant::from_millis(self.start.elapsed().as_millis() as i64)
    }

    async fn send(&self, p: &[u8]) {
        let _ = self.udp.send(p).await;
    }

    async fn on_datagram(&mut self, d: &[u8], from: IpAddr) {
        let mut out = vec![0u8; 65536];
        let mut input: &[u8] = d;
        loop {
            match self.tunn.decapsulate(Some(from), input, &mut out) {
                TunnResult::WriteToNetwork(p) => {
                    let _ = self.udp.send(p).await;
                    input = &[]; // call again until Done
                }
                TunnResult::WriteToTunnelV4(p, _) | TunnResult::WriteToTunnelV6(p, _) => {
                    self.dev.rx.push_back(p.to_vec());
                    break;
                }
                TunnResult::Done | TunnResult::Err(_) => break,
            }
        }
    }

    async fn timers(&mut self) {
        let mut out = vec![0u8; 2048];
        if let TunnResult::WriteToNetwork(p) = self.tunn.update_timers(&mut out) {
            self.send(p).await;
        }
    }

    fn on_cmd(&mut self, c: Cmd) {
        match c {
            Cmd::Resolve { name, v6, reply } => {
                let t = if v6 {
                    DnsQueryType::Aaaa
                } else {
                    DnsQueryType::A
                };
                let s = self.socks.get_mut::<dns::Socket>(self.dns);
                match s.start_query(self.iface.context(), &name, t) {
                    Ok(handle) => self.queries.push(Query { handle, reply }),
                    Err(e) => {
                        let _ = reply.send(Err(format!("lookup {name}: {e:?}")));
                    }
                }
            }
            Cmd::Dial { to, reply } => {
                let mut s = tcp::Socket::new(
                    tcp::SocketBuffer::new(vec![0; 65535]),
                    tcp::SocketBuffer::new(vec![0; 65535]),
                );
                self.next_port = if self.next_port >= 65000 {
                    49152
                } else {
                    self.next_port + 1
                };
                let remote = IpEndpoint::new(ip_addr(to.ip()), to.port());
                if let Err(e) = s.connect(self.iface.context(), remote, self.next_port) {
                    let _ = reply.send(Err(io::Error::other(format!("dial {to}: {e}"))));
                    return;
                }
                let (mine, app) = tokio::io::duplex(PIPE);
                let sock = self.socks.add(s);
                self.conns.push(Conn {
                    sock,
                    pipe: mine,
                    reply: Some(reply),
                    app: Some(app),
                    eof_sent: false,
                    fin_sent: false,
                });
            }
        }
    }

    /// Moves bytes between the smoltcp sockets and the callers' pipes.
    fn pump(&mut self) {
        let w = waker(Arc::new(Wake(self.notify.clone())));
        let mut cx = Context::from_waker(&w);
        // DNS results
        let mut i = 0;
        while i < self.queries.len() {
            let q = &self.queries[i];
            if q.reply.is_closed() {
                self.socks
                    .get_mut::<dns::Socket>(self.dns)
                    .cancel_query(q.handle);
                self.queries.swap_remove(i);
                continue;
            }
            match self
                .socks
                .get_mut::<dns::Socket>(self.dns)
                .get_query_result(q.handle)
            {
                Err(GetQueryResultError::Pending) => i += 1,
                r => {
                    let q = self.queries.swap_remove(i);
                    let _ = q.reply.send(match r {
                        Ok(v) => Ok(v
                            .iter()
                            .map(|a| match a {
                                IpAddress::Ipv4(a) => IpAddr::V4(*a),
                                IpAddress::Ipv6(a) => IpAddr::V6(*a),
                            })
                            .collect()),
                        Err(_) => Err("no such host".into()),
                    });
                }
            }
        }
        // TCP connections
        let mut i = 0;
        while i < self.conns.len() {
            let c = &mut self.conns[i];
            let s = self.socks.get_mut::<tcp::Socket>(c.sock);
            let mut done = false;
            if c.reply.is_some() {
                if s.may_send() {
                    if let (Some(r), Some(a)) = (c.reply.take(), c.app.take()) {
                        let _ = r.send(Ok(a));
                    }
                } else if s.state() == tcp::State::Closed {
                    if let Some(r) = c.reply.take() {
                        let _ = r.send(Err(io::Error::from(io::ErrorKind::ConnectionRefused)));
                    }
                    done = true;
                } else if c.reply.as_ref().is_some_and(|r| r.is_closed()) {
                    s.abort();
                    done = true;
                }
            }
            if c.reply.is_none() && !done {
                // socket -> caller
                while s.can_recv() && !c.eof_sent {
                    let pipe = &mut c.pipe;
                    let r = s.recv(|buf| match Pin::new(&mut *pipe).poll_write(&mut cx, buf) {
                        Poll::Ready(Ok(n)) => (n, Ok(n)),
                        Poll::Ready(Err(e)) => (0, Err(Some(e))),
                        Poll::Pending => (0, Err(None)),
                    });
                    match r {
                        Ok(Ok(n)) if n > 0 => {}
                        Ok(Err(Some(_))) => {
                            s.abort(); // caller went away
                            done = true;
                            break;
                        }
                        _ => break,
                    }
                }
                if !done && !s.may_recv() && !s.can_recv() && !c.eof_sent {
                    c.eof_sent = true; // peer closed: EOF for the caller
                    let _ = Pin::new(&mut c.pipe).poll_shutdown(&mut cx);
                }
                // caller -> socket
                while !done && !c.fin_sent && s.can_send() {
                    let room = s.send_capacity() - s.send_queue();
                    if room == 0 {
                        break;
                    }
                    let mut buf = vec![0u8; room.min(16384)];
                    let mut rb = ReadBuf::new(&mut buf);
                    match Pin::new(&mut c.pipe).poll_read(&mut cx, &mut rb) {
                        Poll::Ready(Ok(())) if rb.filled().is_empty() => {
                            s.close(); // caller finished writing
                            c.fin_sent = true;
                        }
                        Poll::Ready(Ok(())) => {
                            let n = rb.filled().len();
                            let _ = s.send_slice(&buf[..n]);
                        }
                        Poll::Ready(Err(_)) => {
                            s.abort();
                            done = true;
                        }
                        Poll::Pending => break,
                    }
                }
                if s.state() == tcp::State::Closed {
                    done = true;
                    if !c.eof_sent {
                        let _ = Pin::new(&mut c.pipe).poll_shutdown(&mut cx);
                    }
                }
            }
            if done {
                let c = self.conns.swap_remove(i);
                self.socks.remove(c.sock);
            } else {
                i += 1;
            }
        }
    }

    /// Polls the stack until it is idle, then encrypts what it produced.
    async fn run_stack(&mut self) {
        let mut out = vec![0u8; MTU + 256];
        for _ in 0..16 {
            let now = self.now();
            let changed = self.iface.poll(now, &mut self.dev, &mut self.socks);
            self.pump();
            let mut sent = false;
            while let Some(p) = self.dev.tx.pop_front() {
                if let TunnResult::WriteToNetwork(w) = self.tunn.encapsulate(&p, &mut out) {
                    let _ = self.udp.send(w).await;
                }
                sent = true;
            }
            if changed == smoltcp::iface::PollResult::None && !sent {
                break;
            }
        }
    }

    async fn run(mut self, mut cmds: mpsc::UnboundedReceiver<Cmd>) {
        let mut buf = vec![0u8; 65536];
        let mut out = vec![0u8; 2048];
        if let TunnResult::WriteToNetwork(p) =
            self.tunn.format_handshake_initiation(&mut out, false)
        {
            self.send(p).await;
        }
        let peer = self.udp.peer_addr().map(|a| a.ip()).ok();
        let mut last_timer = Instant::now();
        loop {
            let delay = self
                .iface
                .poll_delay(self.now(), &self.socks)
                .map(|d| Duration::from_micros(d.total_micros()))
                .unwrap_or(Duration::from_millis(250))
                .min(Duration::from_millis(250));
            tokio::select! {
                r = self.udp.recv(&mut buf) => {
                    if let (Ok(n), Some(ip)) = (r, peer) {
                        let d = buf[..n].to_vec();
                        self.on_datagram(&d, ip).await;
                    }
                }
                c = cmds.recv() => match c {
                    Some(c) => self.on_cmd(c),
                    None => return,
                },
                _ = self.notify.notified() => {}
                _ = sleep(delay) => {}
            }
            if last_timer.elapsed() >= Duration::from_millis(250) {
                last_timer = Instant::now();
                self.timers().await;
            }
            self.run_stack().await;
        }
    }
}

/// A running tunnel: the command channel of its task.
struct Running {
    cmds: mpsc::UnboundedSender<Cmd>,
}

async fn spawn_tunnel(cfg: TunnelConfig) -> Result<Running, String> {
    let ep: SocketAddr = match cfg.endpoint.parse() {
        Ok(a) => a,
        Err(_) => lookup_host(cfg.endpoint.as_str())
            .await
            .map_err(|e| e.to_string())?
            .next()
            .ok_or("warp.json: bad endpoint")?,
    };
    let udp = UdpSocket::bind(if ep.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })
        .await
        .map_err(|e| e.to_string())?;
    udp.connect(ep).await.map_err(|e| e.to_string())?;
    let tunn = Tunn::new(
        StaticSecret::from(cfg.private),
        PublicKey::from(cfg.peer),
        None,
        Some(KEEPALIVE),
        rand::random::<u32>() & 0x00ff_ffff,
        None,
    );
    let mut dev = Pipe::default();
    let mut conf = Config::new(HardwareAddress::Ip);
    conf.random_seed = rand::random();
    let mut iface = Interface::new(conf, &mut dev, SInstant::from_millis(0));
    iface.update_ip_addrs(|v| {
        for a in &cfg.addrs {
            let c = match a {
                IpAddr::V4(a) => IpCidr::new(IpAddress::Ipv4(*a), 32),
                IpAddr::V6(a) => IpCidr::new(IpAddress::Ipv6(*a), 128),
            };
            let _ = v.push(c);
        }
    });
    // allowed_ip=0.0.0.0/0 and ::/0: everything goes to the peer
    let via4 = cfg.addrs.iter().find_map(|a| match a {
        IpAddr::V4(a) => Some(*a),
        _ => None,
    });
    let via6 = cfg.addrs.iter().find_map(|a| match a {
        IpAddr::V6(a) => Some(*a),
        _ => None,
    });
    if let Some(a) = via4 {
        let _ = iface.routes_mut().add_default_ipv4_route(a);
    }
    if let Some(a) = via6 {
        let _ = iface.routes_mut().add_default_ipv6_route(a);
    }
    let servers: Vec<IpAddress> = cfg.dns.iter().map(|a| ip_addr(*a)).collect();
    let mut socks = SocketSet::new(vec![]);
    let dns = socks.add(dns::Socket::new(
        &servers,
        Vec::<Option<dns::DnsQuery>>::new(),
    ));
    let (tx, rx) = mpsc::unbounded_channel();
    let task = Task {
        tunn,
        udp,
        dev,
        iface,
        socks,
        dns,
        conns: vec![],
        queries: vec![],
        start: Instant::now(),
        next_port: 49152 + (rand::random::<u16>() % 8000),
        notify: Arc::new(Notify::new()),
    };
    tokio::spawn(task.run(rx));
    Ok(Running { cmds: tx })
}

/// warp.rs `warpTunnel`
pub struct WarpTunnel {
    file: PathBuf, // saved account + keys (warp.json, private)
    run: Mutex<Option<Arc<Running>>>,
}

impl WarpTunnel {
    pub fn new(file: &str) -> Self {
        WarpTunnel {
            file: PathBuf::from(file),
            run: Mutex::new(None),
        }
    }

    /// warp.rs `net`: the running tunnel, signing up and starting it on first use.
    async fn net(&self) -> Result<Arc<Running>, String> {
        let mut g = self.run.lock().await;
        if let Some(r) = &*g {
            return Ok(r.clone());
        }
        let a = match std::fs::read(&self.file)
            .map_err(|e| e.to_string())
            .and_then(|b| parse_account(&b))
        {
            Ok(a) => a,
            Err(_) => {
                let a = register().await?;
                save_account(&self.file, &a).map_err(|e| e.to_string())?;
                a
            }
        };
        let r = Arc::new(spawn_tunnel(build_config(&a)?).await?);
        *g = Some(r.clone());
        Ok(r)
    }

    /// warp.rs `dial`: the host name is resolved inside the tunnel (Cloudflare DNS),
    /// not by this machine.
    async fn dial_addr(&self, addr: &str) -> io::Result<BoxedConn> {
        let r = self.net().await.map_err(io::Error::other)?;
        let bad = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("missing port in address {addr}"),
            )
        };
        let (host, port) = split_host_port(addr).ok_or_else(bad)?;
        let port: u16 = port.parse().map_err(|_| bad())?;
        let ips: Vec<IpAddr> = match host.parse::<IpAddr>() {
            Ok(ip) => vec![ip],
            Err(_) => {
                let mut v = vec![];
                for v6 in [false, true] {
                    let (tx, rx) = oneshot::channel();
                    let cmd = Cmd::Resolve {
                        name: host.clone(),
                        v6,
                        reply: tx,
                    };
                    r.cmds
                        .send(cmd)
                        .map_err(|_| io::Error::other("warp tunnel stopped"))?;
                    if let Ok(Ok(Ok(a))) = timeout(RESOLVE_TIMEOUT, rx).await {
                        v.extend(a);
                    }
                    if !v.is_empty() {
                        break;
                    }
                }
                if v.is_empty() {
                    return Err(io::Error::other(format!("lookup {host}: no such host")));
                }
                v
            }
        };
        let mut last = io::Error::other("no address");
        for ip in ips {
            let (tx, rx) = oneshot::channel();
            let cmd = Cmd::Dial {
                to: SocketAddr::new(ip, port),
                reply: tx,
            };
            r.cmds
                .send(cmd)
                .map_err(|_| io::Error::other("warp tunnel stopped"))?;
            match timeout(CONNECT_TIMEOUT, rx).await {
                Ok(Ok(Ok(s))) => return Ok(Box::new(s)),
                Ok(Ok(Err(e))) => last = e,
                Ok(Err(_)) => last = io::Error::other("warp tunnel stopped"),
                Err(_) => last = io::Error::from(io::ErrorKind::TimedOut),
            }
        }
        Err(last)
    }
}

impl BuiltinUpstream for WarpTunnel {
    fn start(&self) -> BoxFut<'_, Result<(), String>> {
        Box::pin(async { self.net().await.map(|_| ()) })
    }
    fn dial<'a>(&'a self, addr: &'a str) -> BoxFut<'a, io::Result<BoxedConn>> {
        Box::pin(self.dial_addr(addr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct() -> WarpAccount {
        WarpAccount {
            private_key: B64.encode([7u8; 32]),
            peer_key: WARP_PEER_KEY.into(),
            endpoint: "162.159.192.1:2408".into(),
            addr_v4: "172.16.0.2/32".into(),
            addr_v6: "2606:4700:110:8a36::1/128".into(),
        }
    }

    #[test]
    fn parses_go_warp_json() {
        let j = br#"{
  "private_key": "abc",
  "peer_key": "def",
  "endpoint": "162.159.192.1:2408",
  "addr_v4": "172.16.0.2",
  "addr_v6": ""
}"#;
        let a = parse_account(j).unwrap();
        assert_eq!(a.private_key, "abc");
        assert_eq!(a.endpoint, "162.159.192.1:2408");
        assert!(parse_account(b"{}").is_err());
        assert!(parse_account(br#"{"private_key":"x"}"#).is_err());
        assert!(parse_account(b"nope").is_err());
        let out = serde_json::to_string_pretty(&acct()).unwrap();
        assert!(out.contains("\"private_key\"") && out.contains("\"addr_v6\""));
    }

    #[test]
    fn builds_config() {
        let c = build_config(&acct()).unwrap();
        assert_eq!(
            c.addrs,
            vec![
                "172.16.0.2".parse::<IpAddr>().unwrap(),
                "2606:4700:110:8a36::1".parse().unwrap()
            ]
        );
        assert_eq!(c.dns.len(), 2);
        assert_eq!(c.private, [7u8; 32]);
        let mut bad = acct();
        bad.peer_key = "!!".into();
        assert!(build_config(&bad).unwrap_err().contains("bad keys"));
    }

    #[test]
    fn reads_signup_reply() {
        let r = br#"{"config":{"peers":[{"public_key":"","endpoint":{"v4":"162.159.192.7:0","host":"x:0"}}],
          "interface":{"addresses":{"v4":"172.16.0.2","v6":"2606::2"}}}}"#;
        let a = account_from_reply(&[1u8; 32], r).unwrap();
        assert_eq!(a.endpoint, "162.159.192.7:2408");
        assert_eq!(a.peer_key, WARP_PEER_KEY);
        assert_eq!(a.addr_v6, "2606::2");
        assert!(account_from_reply(&[1u8; 32], b"{}").is_err());
    }

    #[test]
    fn rfc3339_shape() {
        let s = now_utc_rfc3339();
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z') && &s[10..11] == "T");
    }
}
