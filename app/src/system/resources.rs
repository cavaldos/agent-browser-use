//! Resource limits: keep the shared browser from hogging the machine.
//!
//! * CPU: the browser's whole process tree is duty-cycled with SIGSTOP / SIGCONT (like `cpulimit`).
//!   A small feedback loop picks the fraction of each 100 ms period the tree may run so that its
//!   average use stays near the limit. Pages get slower, never killed.
//! * RAM: a soft limit. When the tree's resident memory passes the limit the browser is asked to
//!   free memory (memory-pressure signal + GC on every idle tab); if it stays over, the UI says so.
//!   A hard cap would mean killing tabs, which this never does.
//!
//! Limits are percentages of the whole machine (100 = no limit). Unix only: on Windows the panel
//! shows usage as unavailable. Settings live in memory and reset when the app restarts.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use serde::Serialize;
use serde_json::{json, Value};

use crate::browser::Browser;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

const MIN_LIMIT: u32 = 5;
const PERIOD_MS: f64 = 100.0;
const MIN_DUTY: f64 = 0.05;
/// the sampler idles while no limit is set and nobody has polled for this long
const POLL_IDLE: Duration = Duration::from_secs(12);
const SAMPLE_EVERY: Duration = Duration::from_millis(1500);
const CLEANUP_EVERY: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub rss_kb: u64,
    pub cpu_secs: f64,
}

struct St {
    cpu_limit: u32,
    mem_limit: u32,
    pids: Vec<i32>,
    cpu_pct: f64,
    mem_kb: u64,
    last_cpu: Option<(Instant, f64)>,
    /// what the tree would use (in cores) if it were never paused
    natural: f64,
    duty: f64,
    mem_over: bool,
    last_cleanup: Option<Instant>,
    cleanups: u32,
    last_poll: Instant,
}

pub struct Resources {
    b: Option<Arc<Browser>>,
    cores: usize,
    total_kb: u64,
    mu: Mutex<St>,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub available: bool,
    #[serde(rename = "canToggle")]
    pub can_toggle: bool,
    pub cores: usize,
    #[serde(rename = "totalMemMB")]
    pub total_mem_mb: u64,
    #[serde(rename = "cpuLimit")]
    pub cpu_limit: u32,
    #[serde(rename = "memLimit")]
    pub mem_limit: u32,
    /// browser CPU use, % of the whole machine
    #[serde(rename = "cpuPct")]
    pub cpu_pct: f64,
    #[serde(rename = "memMB")]
    pub mem_mb: u64,
    /// browser memory, % of the machine's RAM
    #[serde(rename = "memPct")]
    pub mem_pct: f64,
    pub procs: usize,
    /// the browser is being paused now and then to hold the CPU limit
    pub throttled: bool,
    #[serde(rename = "memOver")]
    pub mem_over: bool,
    pub cleanups: u32,
}

impl Resources {
    pub fn new(b: Option<Arc<Browser>>) -> Arc<Resources> {
        let r = Arc::new(Resources {
            b,
            cores: std::thread::available_parallelism().map_or(1, |n| n.get()),
            total_kb: total_memory_kb(),
            mu: Mutex::new(St {
                cpu_limit: 100,
                mem_limit: 100,
                pids: Vec::new(),
                cpu_pct: 0.0,
                mem_kb: 0,
                last_cpu: None,
                natural: 0.0,
                duty: 1.0,
                mem_over: false,
                last_cleanup: None,
                cleanups: 0,
                last_poll: Instant::now() - POLL_IDLE,
            }),
        });
        if r.supported() {
            tokio::spawn(r.clone().sample_loop());
            tokio::spawn(r.clone().throttle_loop());
        }
        r
    }

    /// there is a browser process of ours to look at, on a platform that can pause it
    fn supported(&self) -> bool {
        cfg!(unix) && self.b.is_some()
    }

    fn browser_pid(&self) -> Option<i32> {
        let b = self.b.as_ref()?;
        let chrome = b.st().chrome.clone()?;
        if chrome.is_dead() {
            return None;
        }
        chrome.pid.map(|p| p as i32)
    }

    pub fn status(&self) -> Status {
        let mut s = lock(&self.mu);
        s.last_poll = Instant::now();
        Status {
            available: self.supported() && self.browser_pid().is_some(),
            can_toggle: false,
            cores: self.cores,
            total_mem_mb: self.total_kb / 1024,
            cpu_limit: s.cpu_limit,
            mem_limit: s.mem_limit,
            cpu_pct: round1(s.cpu_pct),
            mem_mb: s.mem_kb / 1024,
            mem_pct: if self.total_kb == 0 {
                0.0
            } else {
                round1(s.mem_kb as f64 * 100.0 / self.total_kb as f64)
            },
            procs: s.pids.len(),
            throttled: s.duty < 0.98,
            mem_over: s.mem_over,
            cleanups: s.cleanups,
        }
    }

    pub fn set(&self, cpu: Option<u32>, mem: Option<u32>) {
        let mut s = lock(&self.mu);
        if let Some(c) = cpu {
            s.cpu_limit = c.clamp(MIN_LIMIT, 100);
            if s.cpu_limit >= 100 {
                s.duty = 1.0;
            }
        }
        if let Some(m) = mem {
            s.mem_limit = m.clamp(MIN_LIMIT, 100);
            if s.mem_limit >= 100 {
                s.mem_over = false;
            }
        }
    }

    async fn sample_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(SAMPLE_EVERY).await;
            let idle = {
                let s = lock(&self.mu);
                s.cpu_limit >= 100 && s.mem_limit >= 100 && s.last_poll.elapsed() > POLL_IDLE
            };
            if idle {
                let mut s = lock(&self.mu);
                s.pids.clear();
                s.last_cpu = None;
                continue;
            }
            let Some(root) = self.browser_pid() else {
                let mut s = lock(&self.mu);
                s.pids.clear();
                s.last_cpu = None;
                s.cpu_pct = 0.0;
                s.mem_kb = 0;
                s.duty = 1.0;
                continue;
            };
            let procs = match snapshot().await {
                Some(p) => p,
                None => continue,
            };
            let tree = descendants(&procs, root);
            let cpu_secs: f64 = tree.iter().map(|p| p.cpu_secs).sum();
            let mem_kb: u64 = tree.iter().map(|p| p.rss_kb).sum();
            let now = Instant::now();
            let mut cleanup = false;
            {
                let mut s = lock(&self.mu);
                s.pids = tree.iter().map(|p| p.pid).collect();
                s.mem_kb = mem_kb;
                if let Some((t0, c0)) = s.last_cpu {
                    let dt = now.duration_since(t0).as_secs_f64().max(0.001);
                    let used = ((cpu_secs - c0) / dt).max(0.0); // cores
                    s.cpu_pct = used * 100.0 / self.cores as f64;
                    let duty = s.duty.max(MIN_DUTY);
                    s.natural = if s.natural == 0.0 {
                        used / duty
                    } else {
                        0.5 * s.natural + 0.5 * (used / duty)
                    };
                    s.duty = if s.cpu_limit >= 100 {
                        1.0
                    } else {
                        let target = s.cpu_limit as f64 / 100.0 * self.cores as f64;
                        let want = if s.natural > target {
                            target / s.natural
                        } else {
                            1.0
                        };
                        (0.5 * s.duty + 0.5 * want).clamp(MIN_DUTY, 1.0)
                    };
                }
                s.last_cpu = Some((now, cpu_secs));
                let limit_kb = self.total_kb / 100 * s.mem_limit as u64;
                s.mem_over = s.mem_limit < 100 && self.total_kb > 0 && mem_kb > limit_kb;
                if s.mem_over && s.last_cleanup.is_none_or(|t| t.elapsed() > CLEANUP_EVERY) {
                    s.last_cleanup = Some(now);
                    s.cleanups += 1;
                    cleanup = true;
                }
            }
            if cleanup {
                if let Some(b) = self.b.clone() {
                    tokio::spawn(reclaim(b));
                }
            }
        }
    }

    /// Pauses and resumes the whole tree so it runs `duty` of every period.
    async fn throttle_loop(self: Arc<Self>) {
        loop {
            let (duty, pids) = {
                let s = lock(&self.mu);
                (s.duty, s.pids.clone())
            };
            if duty >= 0.98 || pids.is_empty() {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            signal_all(&pids, false);
            tokio::time::sleep(Duration::from_secs_f64((1.0 - duty) * PERIOD_MS / 1000.0)).await;
            signal_all(&pids, true);
            tokio::time::sleep(Duration::from_secs_f64(duty * PERIOD_MS / 1000.0)).await;
        }
    }
}

fn round1(f: f64) -> f64 {
    (f * 10.0).round() / 10.0
}

/// Asks the browser to give memory back: a critical memory-pressure notification, and a GC on
/// every tab that is not busy. Errors are ignored: it is best effort.
async fn reclaim(b: Arc<Browser>) {
    let _ = b
        .root_call(
            "Memory.simulatePressureNotification",
            json!({"level": "critical"}),
            Some(Duration::from_secs(5)),
        )
        .await;
    let ids: Vec<String> = b.st().tabs.iter().map(|t| t.id.clone()).collect();
    for id in ids {
        if b.busy_tab(&id) {
            continue;
        }
        let _ = b
            .run_tab(&id, |t| async move {
                t.call(
                    "HeapProfiler.collectGarbage",
                    json!({}),
                    Some(Duration::from_secs(5)),
                )
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
            })
            .await;
    }
}

/// `pid` and everything below it.
pub fn descendants(all: &[Proc], root: i32) -> Vec<Proc> {
    let mut out: Vec<Proc> = Vec::new();
    let mut want = vec![root];
    while let Some(p) = want.pop() {
        for q in all {
            if q.pid == p && !out.iter().any(|o| o.pid == q.pid) {
                out.push(*q);
            }
            if q.ppid == p && q.pid != p && !out.iter().any(|o| o.pid == q.pid) && !want.contains(&q.pid) {
                want.push(q.pid);
            }
        }
    }
    out
}

#[cfg(unix)]
fn signal_all(pids: &[i32], cont: bool) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    let sig = if cont { Signal::SIGCONT } else { Signal::SIGSTOP };
    for p in pids {
        let _ = kill(Pid::from_raw(*p), sig);
    }
}

#[cfg(not(unix))]
fn signal_all(_pids: &[i32], _cont: bool) {}

// ---------- reading the process table ----------

/// ps `time` (`[[d-]hh:]mm:ss[.cc]`) in seconds
pub fn parse_cpu_time(s: &str) -> Option<f64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<f64>().ok()?, r),
        None => (0.0, s),
    };
    let mut secs = 0.0;
    for part in rest.split(':') {
        secs = secs * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(days * 86400.0 + secs)
}

#[cfg(target_os = "linux")]
async fn snapshot() -> Option<Vec<Proc>> {
    tokio::task::spawn_blocking(|| {
        // SAFETY: sysconf has no preconditions.
        let (tck, page) = unsafe {
            (
                libc::sysconf(libc::_SC_CLK_TCK).max(1) as f64,
                libc::sysconf(libc::_SC_PAGESIZE).max(1) as u64,
            )
        };
        let mut out = Vec::new();
        for e in std::fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
                continue;
            };
            // "pid (comm) S ppid ..." where comm may hold spaces and parentheses
            let Some(rest) = stat.rsplit_once(')').map(|x| x.1) else {
                continue;
            };
            let f: Vec<&str> = rest.split_whitespace().collect();
            // f[0]=state f[1]=ppid ... f[11]=utime f[12]=stime ... f[21]=rss (pages)
            if f.len() < 22 {
                continue;
            }
            let num = |i: usize| f[i].parse::<u64>().unwrap_or(0);
            out.push(Proc {
                pid,
                ppid: f[1].parse().unwrap_or(0),
                rss_kb: num(21) * page / 1024,
                cpu_secs: (num(11) + num(12)) as f64 / tck,
            });
        }
        Some(out)
    })
    .await
    .ok()
    .flatten()
}

#[cfg(all(unix, not(target_os = "linux")))]
async fn snapshot() -> Option<Vec<Proc>> {
    let o = tokio::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss=,time="])
        .output()
        .await
        .ok()?;
    if !o.status.success() {
        return None;
    }
    Some(parse_ps(&String::from_utf8_lossy(&o.stdout)))
}

#[cfg(windows)]
async fn snapshot() -> Option<Vec<Proc>> {
    None
}

pub fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some(Proc {
                pid: it.next()?.parse().ok()?,
                ppid: it.next()?.parse().ok()?,
                rss_kb: it.next()?.parse().ok()?,
                cpu_secs: parse_cpu_time(it.next()?)?,
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn total_memory_kb() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

#[cfg(target_os = "macos")]
fn total_memory_kb() -> u64 {
    std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok())
        .map_or(0, |b| b / 1024)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn total_memory_kb() -> u64 {
    0
}

// ---------- HTTP ----------

pub fn routes(b: Option<Arc<Browser>>) -> Router {
    Router::new()
        .route("/api/resources", any(api_resources))
        .with_state(Resources::new(b))
}

fn text(status: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header("x-content-type-options", "nosniff")
        .body(Body::from(format!("{msg}\n")))
        .expect("static response")
}

async fn api_resources(State(r): State<Arc<Resources>>, req: Request) -> Response {
    let can = !crate::keepalive::via_tunnel(req.headers());
    let (parts, body) = req.into_parts();
    if parts.method == axum::http::Method::POST {
        if !can {
            return text(
                StatusCode::FORBIDDEN,
                "limits can only be changed from the machine that runs the app",
            );
        }
        let bytes = to_bytes(body, 4096).await.unwrap_or_default();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let pct = |k: &str| v.get(k).and_then(Value::as_u64).map(|n| n.min(100) as u32);
        r.set(pct("cpuLimit"), pct("memLimit"));
    }
    let mut s = r.status();
    s.can_toggle = can;
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(
            "{}\n",
            serde_json::to_string(&s).unwrap_or_default()
        )))
        .expect("static response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_time_formats() {
        assert_eq!(parse_cpu_time("0:12.34"), Some(12.34));
        assert_eq!(parse_cpu_time("1:02:03"), Some(3723.0));
        assert_eq!(parse_cpu_time("2-00:00:01"), Some(172801.0));
        assert_eq!(parse_cpu_time("x"), None);
    }

    #[test]
    fn tree_is_found() {
        let ps = parse_ps("  1     0  100 0:01.00\n 10     1  200 0:02.00\n 11    10  300 0:03.00\n 12    11  400 0:04.00\n 20     1  999 0:09.00\n");
        let t = descendants(&ps, 10);
        let mut pids: Vec<i32> = t.iter().map(|p| p.pid).collect();
        pids.sort();
        assert_eq!(pids, vec![10, 11, 12]);
        assert_eq!(t.iter().map(|p| p.rss_kb).sum::<u64>(), 900);
    }
}
