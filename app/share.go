package main

// Screen sharing from inside the shared browser (Discord, Meet…).
//
// Headless Chrome has no source picker, so getDisplayMedia would hang. Instead the
// page gets a stream drawn from the shared screen itself: the frames the viewers
// see (whichever tab is active) are pushed into a canvas in the sharing page, and
// the sound of the other tabs into an audio track. No tab capture is involved, so
// the shared screen keeps its size.

import (
	"context"
	"encoding/base64"
	"strconv"
	"strings"
	"sync"

	cdpruntime "github.com/chromedp/cdproto/runtime"
	"github.com/chromedp/chromedp"
)

const shareBinding = "__tbShareOut"

const shareJS = `(() => {
  if (window.top !== window || window.__tbShare) return;
  window.__tbShare = true;
  const md = navigator.mediaDevices;
  if (!md || !md.getDisplayMedia) return;
  const out = (s) => { const f = window.` + shareBinding + `; if (typeof f === 'function') f(s); };
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

  const wrapped = function getDisplayMedia(opts) { return start(opts || {}); };
  try {
    Object.defineProperty(Object.getPrototypeOf(md), 'getDisplayMedia', { value: wrapped, configurable: true, writable: true });
  } catch (e) { try { md.getDisplayMedia = wrapped; } catch (e2) {} }
  window.addEventListener('pagehide', stop);
})();`

// shareOut pushes the shared screen into one sharing tab, one call at a time.
// Newer frames replace older ones, so a slow page skips ahead instead of lagging.
type shareOut struct {
	mu     sync.Mutex
	frame  []byte   // newest JPEG not sent yet
	audio  []string // pending JS audio calls
	wake   chan struct{}
	cancel context.CancelFunc
}

func (s *shareOut) poke() {
	select {
	case s.wake <- struct{}{}:
	default:
	}
}

// onShare handles start/stop messages from a page's getDisplayMedia.
func (b *Browser) onShare(t *Tab, p string) {
	switch p {
	case "start":
		b.mu.Lock()
		if t.share != nil {
			b.mu.Unlock()
			return
		}
		ctx, cancel := context.WithCancel(t.ctx)
		s := &shareOut{wake: make(chan struct{}, 1), cancel: cancel}
		t.share = s
		if b.frame != nil { // start with the current picture
			s.frame = b.frame[1:]
		}
		b.mu.Unlock()
		go b.shareLoop(ctx, t, s)
		s.poke()
	case "stop":
		b.stopShare(t)
	}
}

func (b *Browser) stopShare(t *Tab) {
	b.mu.Lock()
	s := t.share
	t.share = nil
	b.mu.Unlock()
	if s != nil {
		s.cancel()
	}
}

// shareFrame hands a frame of the active tab to every sharing tab. Called with b.mu held.
func (b *Browser) shareFrameLocked(jpeg []byte) {
	for _, t := range b.tabs {
		if s := t.share; s != nil {
			s.mu.Lock()
			s.frame = jpeg
			s.mu.Unlock()
			s.poke()
		}
	}
}

// shareAudio hands sound from tab src to every other sharing tab.
func (b *Browser) shareAudio(src *Tab, rate int, b64 string) {
	call := "__tbShareAudio(" + strconv.Itoa(rate) + ",\"" + b64 + "\")"
	b.mu.Lock()
	defer b.mu.Unlock()
	for _, t := range b.tabs {
		s := t.share
		if s == nil || t == src { // never feed a page its own sound back
			continue
		}
		s.mu.Lock()
		if len(s.audio) >= 25 { // the page is far behind: drop the oldest
			s.audio = s.audio[1:]
		}
		s.audio = append(s.audio, call)
		s.mu.Unlock()
		s.poke()
	}
}

func (b *Browser) shareLoop(ctx context.Context, t *Tab, s *shareOut) {
	ret := true
	for {
		select {
		case <-ctx.Done():
			return
		case <-s.wake:
		}
		s.mu.Lock()
		frame, audio := s.frame, s.audio
		s.frame, s.audio = nil, nil
		s.mu.Unlock()
		if frame == nil && len(audio) == 0 {
			continue
		}
		var js strings.Builder
		js.WriteString("(typeof __tbShareFrame==='function')?(")
		for _, a := range audio {
			js.WriteString(a)
			js.WriteString(",")
		}
		if frame != nil {
			js.WriteString("__tbShareFrame(\"")
			js.WriteString(base64.StdEncoding.EncodeToString(frame))
			js.WriteString("\")")
		} else {
			js.WriteString("1")
		}
		js.WriteString("):0")
		cctx, cancel := cmdCtx(ctx)
		res, err := chromedp.Call(cctx, cdpruntime.Evaluate, cdpruntime.EvaluateParams{Expression: js.String(), ReturnByValue: &ret})
		cancel()
		if ctx.Err() != nil {
			return
		}
		// The page left or ended the share without telling us.
		if err == nil && res.Result != nil && string(res.Result.Value) == "0" {
			b.stopShare(t)
			return
		}
	}
}
