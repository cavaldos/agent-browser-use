//! Screen sharing from inside the shared browser (Discord, Meet…).
//!
//! Headless Chrome has no source picker, so getDisplayMedia would hang. Instead the
//! page gets a stream drawn from the shared screen itself: the frames the viewers
//! see (whichever tab is active) are pushed into a canvas in the sharing page, and
//! the sound of the other tabs into an audio track. No tab capture is involved, so
//! the shared screen keeps its size.
//!
//! Rust port: `Arc<Share>` is the `BrowserHooks` of this module. `tab_scripts` installs the binding and
//! the new-document script in every tab, `binding` receives the page's start/stop messages, `frame` is
//! `shareFrameLocked` and `audio` is `shareAudio`. The per-tab `t.share` of lives in `Share.tabs`
//! (keyed by tab id) because `Tab.ext` belongs to the observer. Call `Share::attach` once the browser
//! exists (it reads the cached frame when a share starts).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

use base64::Engine as _;
use serde_json::json;
use tokio::sync::{watch, Notify};

use crate::browser::{Browser, BrowserHooks, Tab, TabScript};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub const SHARE_BINDING: &str = "__tbShareOut";

pub static SHARE_JS: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let a = r####"(() => {
  if (window.top !== window || window.__tbShare) return;
  Object.defineProperty(window, '__tbShare', { value: true });
  const md = navigator.mediaDevices;
  if (!md || !md.getDisplayMedia) return;
  const out = (s) => { const f = window."####;
    let b = r####"; if (typeof f === 'function') f(s); };
  let cur = null; // the live share: { canvas, g, stream, ac, dest, next }

  function stop() {
    if (!cur) return;
    const c = cur; cur = null;
    c.stream.getTracks().forEach((t) => { if (t.readyState !== 'ended') t.stop(); });
    if (c.ac) c.ac.close().catch(() => {});
    out('stop');
  }

  async function start(opts) {
    if (cur) stop();
    const canvas = document.createElement('canvas');
    canvas.width = 1280; canvas.height = 720;
    const g = canvas.getContext('2d', { alpha: false });
    g.fillStyle = '#000'; g.fillRect(0, 0, canvas.width, canvas.height);
    const stream = canvas.captureStream(30);
    const c = { canvas, g, stream, ac: null, dest: null, next: 0, busy: false, pending: null };
    if (opts && opts.audio) {
      try {
        c.ac = new AudioContext({ latencyHint: 'interactive' });
        c.dest = c.ac.createMediaStreamDestination();
        c.dest.stream.getAudioTracks().forEach((t) => stream.addTrack(t));
        c.ac.resume().catch(() => {});
      } catch (e) { c.ac = null; }
    }
    const vt = stream.getVideoTracks()[0];
    try { vt.contentHint = 'detail'; } catch (e) {}
    // Look like a real screen share to the page.
    const settings = vt.getSettings.bind(vt);
    vt.getSettings = () => Object.assign(settings(), { displaySurface: 'monitor', logicalSurface: true, cursor: 'never' });
    stream.getTracks().forEach((t) => t.addEventListener('ended', () => { if (cur === c && stream.getTracks().every((x) => x.readyState === 'ended')) stop(); }));
    const tstop = MediaStreamTrack.prototype.stop;
    stream.getTracks().forEach((t) => { t.stop = function () { tstop.call(this); if (cur === c && stream.getTracks().every((x) => x.readyState === 'ended')) stop(); }; });
    cur = c;
    out('start');
    return stream;
  }

  // Called by the server with the newest JPEG frame (base64). Frames that come in
  // while one is decoding replace each other, so a slow page never falls behind.
  window.__tbShareFrame = (b64) => {
    const c = cur;
    if (!c) return 0;
    c.pending = b64;
    if (c.busy) return 1;
    c.busy = true;
    (async () => {
      while (c.pending && cur === c) {
        const s = c.pending; c.pending = null;
        try {
          const bin = atob(s), buf = new Uint8Array(bin.length);
          for (let i = 0; i < bin.length; i++) buf[i] = bin.charCodeAt(i);
          const bmp = await createImageBitmap(new Blob([buf], { type: 'image/jpeg' }));
          // Keep the screen's shape; cap the size so encoding stays cheap.
          const k = Math.min(1, 1920 / bmp.width, 1080 / bmp.height);
          const w = Math.round(bmp.width * k) & ~1, h = Math.round(bmp.height * k) & ~1;
          if (c.canvas.width !== w || c.canvas.height !== h) { c.canvas.width = w; c.canvas.height = h; }
          c.g.drawImage(bmp, 0, 0, w, h);
          bmp.close();
        } catch (e) {}
      }
      c.busy = false;
    })();
    return 1;
  };

  // Called by the server with int16 stereo PCM (base64) from the other tabs.
  window.__tbShareAudio = (rate, b64) => {
    const c = cur;
    if (!c) return 0;
    if (!c.ac) return 1;
    try {
      const bin = atob(b64), n = bin.length >> 2;
      if (!n) return 1;
      const buf = c.ac.createBuffer(2, n, rate), L = buf.getChannelData(0), R = buf.getChannelData(1);
      for (let i = 0, j = 0; i < n; i++, j += 4) {
        let l = bin.charCodeAt(j) | (bin.charCodeAt(j + 1) << 8); if (l > 32767) l -= 65536;
        let r = bin.charCodeAt(j + 2) | (bin.charCodeAt(j + 3) << 8); if (r > 32767) r -= 65536;
        L[i] = l / 32768; R[i] = r / 32768;
      }
      const src = c.ac.createBufferSource();
      src.buffer = buf;
      src.connect(c.dest);
      const now = c.ac.currentTime;
      if (c.next < now + 0.02 || c.next > now + 0.5) c.next = now + 0.08; // resync after a gap or drift
      src.start(c.next);
      c.next += buf.duration;
    } catch (e) {}
    return 1;
  };

  const wrapped = { getDisplayMedia() { return start(arguments[0] || {}); } }.getDisplayMedia; // length 0, like the built-in
  if (window.__abuNative) window.__abuNative(wrapped, 'getDisplayMedia');
  try {
    Object.defineProperty(Object.getPrototypeOf(md), 'getDisplayMedia', { value: wrapped, configurable: true, writable: true, enumerable: true });
  } catch (e) { try { md.getDisplayMedia = wrapped; } catch (e2) {} }
  window.addEventListener('pagehide', stop);
  for (const k of ['__tbShareFrame', '__tbShareAudio']) Object.defineProperty(window, k, { enumerable: false });
})();"####;
    format!("{a}{SHARE_BINDING}{b}")
});

/// shareOut pushes the shared screen into one sharing tab, one call at a time.
/// Newer frames replace older ones, so a slow page skips ahead instead of lagging.
struct ShareOut {
    st: Mutex<ShareBuf>,
    wake: Notify,
    cancel: watch::Sender<bool>,
}

#[derive(Default)]
struct ShareBuf {
    /// newest JPEG not sent yet
    frame: Option<Vec<u8>>,
    /// pending JS audio calls
    audio: VecDeque<String>,
}

impl ShareOut {
    fn poke(&self) {
        self.wake.notify_one();
    }
}

#[derive(Default)]
pub struct Share {
    b: OnceLock<Weak<Browser>>,
    /// `t.share` of every sharing tab, by tab id
    tabs: Mutex<HashMap<String, Arc<ShareOut>>>,
}

impl Share {
    pub fn new() -> Arc<Share> {
        Arc::new(Share::default())
    }

    /// Gives the module the browser (for the cached frame a share starts with).
    pub fn attach(&self, b: &Arc<Browser>) {
        let _ = self.b.set(Arc::downgrade(b));
    }

    /// onShare handles start/stop messages from a page's getDisplayMedia.
    fn on_share(self: &Arc<Self>, t: &Arc<Tab>, p: &str) {
        match p {
            "start" => {
                // start with the current picture (read before taking our lock: the frame hook
                // runs under Browser.mu and then takes ours)
                let cur = self
                    .b
                    .get()
                    .and_then(Weak::upgrade)
                    .and_then(|b| b.st().frame.clone());
                let s = {
                    let mut tabs = lock(&self.tabs);
                    if tabs.contains_key(&t.id) {
                        return;
                    }
                    let s = Arc::new(ShareOut {
                        st: Mutex::new(ShareBuf {
                            frame: cur.map(|f| f[1..].to_vec()),
                            audio: VecDeque::new(),
                        }),
                        wake: Notify::new(),
                        cancel: watch::channel(false).0,
                    });
                    tabs.insert(t.id.clone(), s.clone());
                    s
                };
                tokio::spawn(self.clone().share_loop(t.clone(), s.clone()));
                s.poke();
            }
            "stop" => self.stop_share(t),
            _ => {}
        }
    }

    fn stop_share(&self, t: &Tab) {
        let s = lock(&self.tabs).remove(&t.id);
        if let Some(s) = s {
            let _ = s.cancel.send(true);
        }
    }

    async fn share_loop(self: Arc<Self>, t: Arc<Tab>, s: Arc<ShareOut>) {
        let mut cancel = s.cancel.subscribe();
        loop {
            tokio::select! {
                _ = crate::cdp::flagged(&mut cancel) => return,
                // wakes up now and then so the loop of a tab that was closed ends too
                r = tokio::time::timeout(Duration::from_secs(5), s.wake.notified()) => {
                    if r.is_err() {
                        if t.canceled() {
                            lock(&self.tabs).remove(&t.id);
                            return;
                        }
                        continue;
                    }
                }
            }
            let (frame, audio) = {
                let mut b = lock(&s.st);
                (b.frame.take(), std::mem::take(&mut b.audio))
            };
            if frame.is_none() && audio.is_empty() {
                continue;
            }
            let mut js = String::from("(typeof __tbShareFrame==='function')?(");
            for a in &audio {
                js.push_str(a);
                js.push(',');
            }
            match &frame {
                Some(f) => {
                    js.push_str("__tbShareFrame(\"");
                    js.push_str(&base64::engine::general_purpose::STANDARD.encode(f));
                    js.push_str("\")");
                }
                None => js.push('1'),
            }
            js.push_str("):0");
            let res = t
                .call_cmd(
                    "Runtime.evaluate",
                    json!({"expression": js, "returnByValue": true}),
                )
                .await;
            if *cancel.borrow() || t.canceled() {
                return;
            }
            // The page left or ended the share without telling us.
            if let Ok(r) = res {
                if r.pointer("/result/value").is_some_and(|v| *v == 0) {
                    self.stop_share(&t);
                    return;
                }
            }
        }
    }
}

impl BrowserHooks for Arc<Share> {
    fn tab_scripts(&self) -> Vec<TabScript> {
        vec![TabScript {
            binding: SHARE_BINDING,
            source: SHARE_JS.clone(),
        }]
    }

    fn binding(&self, t: &Arc<Tab>, name: &str, payload: &str) {
        if name == SHARE_BINDING {
            self.on_share(t, payload);
        }
    }

    /// shareFrame hands a frame of the active tab to every sharing tab. Called with b.mu held.
    fn frame(&self, jpeg: &[u8]) {
        for s in lock(&self.tabs).values() {
            lock(&s.st).frame = Some(jpeg.to_vec());
            s.poke();
        }
    }

    /// shareAudio hands sound from tab src to every other sharing tab.
    fn audio(&self, src: &Arc<Tab>, rate: i64, b64: &str) {
        let call = format!("__tbShareAudio({rate},\"{b64}\")");
        for (id, s) in lock(&self.tabs).iter() {
            if *id == src.id {
                continue; // never feed a page its own sound back
            }
            let mut b = lock(&s.st);
            if b.audio.len() >= 25 {
                // the page is far behind: drop the oldest
                b.audio.pop_front();
            }
            b.audio.push_back(call.clone());
            drop(b);
            s.poke();
        }
    }
}
