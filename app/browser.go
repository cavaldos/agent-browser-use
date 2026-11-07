package main

// Shared browser: one headless Chrome on the server, with real tabs.
//   - Video: the active tab streams JPEG frames (screencast) whenever it changes.
//   - Audio: a script injected into every tab taps <audio>/<video>/Web Audio and
//     sends PCM back through a DevTools binding. Background tabs keep playing, like Chrome.
//   - Input: mouse/keyboard from whoever is in control goes to the active tab.
//   - Tabs: opened/closed/switched from the web UI. Tabs that a page opens
//     (target=_blank links, window.open) are picked up automatically.
//   - Agent: an AI agent (over MCP, see mcp.go/agent.go) drives the same tabs,
//     so people watching the web UI see every step it takes.
// No Docker, X server or video encoder, so it stays far lighter than neko.

import (
	"context"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"errors"
	"log"
	"net"
	"net/url"
	"os"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/chromedp/cdproto/browser"
	"github.com/chromedp/cdproto/cdp"
	"github.com/chromedp/cdproto/emulation"
	"github.com/chromedp/cdproto/input"
	cdplog "github.com/chromedp/cdproto/log"
	"github.com/chromedp/cdproto/network"
	"github.com/chromedp/cdproto/page"
	cdpruntime "github.com/chromedp/cdproto/runtime"
	"github.com/chromedp/cdproto/target"
	"github.com/chromedp/chromedp"
)

type BrowserOptions struct {
	Width, Height int
	Quality       int
	StartURL      string
	Profile       string // Chrome user data dir (cookies, logins…)
	ChromePath    string // empty = auto-detect
	Audio         bool
	MaxTabs       int
	DownloadDir   string // where Chrome saves downloads (also readable by browser_file_upload)
	UploadDir     string // files the agent may upload
	StateDir      string // saved cookie/storage states (browser_state)
}

// View is the size and image quality of the shared screen.
type View struct {
	Width   int `json:"width"`
	Height  int `json:"height"`
	Quality int `json:"quality"`
}

type Tab struct {
	id     string
	opener string // tab that opened this one, if any
	ctx    context.Context
	cancel context.CancelFunc

	// guarded by Browser.mu
	title, url              string // from Chrome's target info
	docTitle, favicon       string // reported by the page itself (metaJS)
	loading, audible, muted bool
	canBack, canFwd         bool
	fresh                   bool // first page of a new tab: drop the about:blank history entry once it commits
	navSeq                  int  // newest refreshNav wins
	lastAudio               time.Time

	obs *observer // console messages and network requests of this tab
}

// TabInfo is what the web UI gets for each tab.
type TabInfo struct {
	ID      string `json:"id"`
	Title   string `json:"title"`
	URL     string `json:"url"`
	Favicon string `json:"favicon,omitempty"`
	Loading bool   `json:"loading,omitempty"`
	Audible bool   `json:"audible,omitempty"`
	Muted   bool   `json:"muted,omitempty"`
	CanBack bool   `json:"canBack,omitempty"`
	CanFwd  bool   `json:"canFwd,omitempty"`
}

type Browser struct {
	root        context.Context // owns the Chrome process; canceled only by Close
	cancelAlloc context.CancelFunc
	hub         *Hub
	opt         BrowserOptions

	// Tab and input commands, run one at a time and in order (see push).
	qmu  sync.Mutex
	jobs []*job
	wake chan struct{}

	// createMu is held while we create a tab ourselves, so that the handler for
	// new targets can tell our tabs apart from tabs that a page opened.
	createMu sync.Mutex
	known    map[string]bool

	mu         sync.Mutex
	view       View
	tabs       []*Tab
	active     *Tab
	frame      []byte // latest frame packet of the active tab
	gotFrame   bool   // the active tab sent a screencast frame since it became active
	streams    map[string]byte
	nextStream int
	bgUntil    time.Time // a tab opened before this time stays in the background (middle/ctrl-click)

	nmu       sync.Mutex
	notifying bool

	ua  string    // Chrome's normal user agent (what "no emulation" means)
	emu Emulation // device emulation, guarded by mu
	dl  *downloads
}

// Binary packet types sent to the web clients (first byte).
const (
	pktFrame byte = 1 // [1][jpeg…]
	pktAudio byte = 2 // [2][stream id][sample rate uint32 LE][int16 stereo PCM…]
)

const audioBinding = "__tbAudioOut"

const audioJS = `(() => {
  if (window.__tbAudio) return;
  window.__tbAudio = true;
  const Ctx = window.AudioContext || window.webkitAudioContext;
  if (!Ctx) return;
  const out = (s) => { const f = window.` + audioBinding + `; if (typeof f === 'function') f(s); };
  const connect = AudioNode.prototype.connect;
  const taps = new WeakMap();

  // Float samples -> base64 int16 stereo. step=2 halves 44.1/48 kHz to save bandwidth.
  function encode(L, R, step) {
    const n = Math.floor(L.length / step), pcm = new Int16Array(n * 2);
    let peak = 0;
    for (let i = 0; i < n; i++) {
      let l = 0, r = 0;
      for (let k = 0; k < step; k++) { l += L[i * step + k]; r += R[i * step + k]; }
      l = Math.max(-1, Math.min(1, l / step)); r = Math.max(-1, Math.min(1, r / step));
      peak = Math.max(peak, Math.abs(l), Math.abs(r));
      pcm[2 * i] = l * 32767; pcm[2 * i + 1] = r * 32767;
    }
    if (peak < 0.0005) return null; // silence: send nothing
    const bytes = new Uint8Array(pcm.buffer);
    let bin = '';
    for (let i = 0; i < bytes.length; i += 0x8000) bin += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    return btoa(bin);
  }

  function tap(ctx) {
    let t = taps.get(ctx);
    if (t) return t;
    const id = 1 + Math.floor(Math.random() * 254);
    const step = ctx.sampleRate >= 44100 ? 2 : 1;
    const rate = Math.round(ctx.sampleRate / step);
    t = ctx.createScriptProcessor(2048, 2, 2);
    t.onaudioprocess = (e) => {
      const b = e.inputBuffer;
      const L = b.getChannelData(0), R = b.numberOfChannels > 1 ? b.getChannelData(1) : L;
      const data = encode(L, R, step);
      if (data) out(id + '|' + rate + '|' + data);
    };
    const silent = ctx.createGain();
    silent.gain.value = 0;
    connect.call(t, silent);
    connect.call(silent, ctx.destination); // keeps the tap running, outputs nothing
    taps.set(ctx, t);
    return t;
  }

  // Web Audio: whatever the page sends to the speakers also goes to the tap.
  AudioNode.prototype.connect = function (dest) {
    const r = connect.apply(this, arguments);
    try { if (dest instanceof AudioDestinationNode && this.context) connect.call(this, tap(this.context)); } catch (e) {}
    return r;
  };

  // <audio> / <video>: route the element into our own context.
  let mctx = null;
  const routed = new WeakSet();
  function route(el) {
    if (!(el instanceof HTMLMediaElement) || routed.has(el)) return;
    routed.add(el);
    try {
      if (!mctx) mctx = new Ctx({ sampleRate: 32000 });
      if (mctx.state !== 'running') mctx.resume();
      connect.call(mctx.createMediaElementSource(el), tap(mctx));
    } catch (e) {}
  }
  const play = HTMLMediaElement.prototype.play;
  HTMLMediaElement.prototype.play = function () { route(this); return play.apply(this, arguments); };
  document.addEventListener('play', (e) => route(e.target), true);
  document.querySelectorAll('audio,video').forEach((el) => { if (!el.paused) route(el); });
})();`

const metaBinding = "__tbMetaOut"

// Reports the page title and favicon whenever they change (Chrome sends no event for that).
const metaJS = `(() => {
  if (window.top !== window || window.__tbMeta) return;
  window.__tbMeta = true;
  let last = '', timer = 0, obs = null;
  const icon = () => {
    const l = document.querySelector('link[rel~="icon"]');
    if (l && l.href) return l.href;
    return /^https?:$/.test(location.protocol) ? location.origin + '/favicon.ico' : '';
  };
  const send = () => {
    timer = 0;
    const s = JSON.stringify({ t: document.title, i: icon() });
    if (s === last) return;
    last = s;
    const f = window.` + metaBinding + `;
    if (typeof f === 'function') f(s);
  };
  const soon = () => { if (!timer) timer = setTimeout(send, 50); };
  const watch = () => {
    if (obs || !document.head) return;
    obs = new MutationObserver(soon);
    obs.observe(document.head, { subtree: true, childList: true, characterData: true, attributes: true, attributeFilter: ['href', 'rel'] });
    soon();
  };
  document.addEventListener('DOMContentLoaded', watch);
  window.addEventListener('load', soon);
  watch();
})();`

func chromeFlags(opt BrowserOptions) []chromedp.ExecAllocatorOption {
	opts := append(chromedp.DefaultExecAllocatorOptions[:],
		chromedp.Flag("headless", "new"),
		chromedp.Flag("enable-automation", false),
		chromedp.Flag("disable-blink-features", "AutomationControlled"),
		chromedp.Flag("mute-audio", true), // never play sound on the server itself
		chromedp.Flag("autoplay-policy", "no-user-gesture-required"),
		// Fewer processes = less RAM. Keeping iframes in the page process also lets
		// the audio tap reach embedded players.
		chromedp.Flag("disable-gpu", true),
		chromedp.Flag("renderer-process-limit", "4"),
		chromedp.Flag("disable-site-isolation-trials", true),
		chromedp.Flag("disable-features", "Translate,BlinkGenPropertyTrees,SpareRendererForSitePerProcess,OptimizationHints,MediaRouter,DialMediaRouteProvider,IsolateOrigins,site-per-process,ChromeWhatsNewUI"),
		chromedp.Flag("disable-component-update", true),
		// Stop Chrome from phoning home: its update check opens an "About Chrome"
		// (chrome://settings/help) tab on its own. These also silence the default-browser
		// and first-run nags, and the "restore last session" bubble.
		chromedp.Flag("disable-background-networking", true),
		chromedp.Flag("no-default-browser-check", true),
		chromedp.Flag("disable-session-crashed-bubble", true),
		chromedp.Flag("hide-crash-restore-bubble", true),
		chromedp.Flag("js-flags", "--max-old-space-size=512"),
		chromedp.WindowSize(opt.Width, opt.Height),
	)
	if opt.ChromePath != "" {
		opts = append(opts, chromedp.ExecPath(opt.ChromePath))
	}
	return opts
}

// Headless Chrome says "HeadlessChrome" in its user agent, and some sites block
// that. Start Chrome once to read the real version, so the shared browser can
// run with a normal user agent everywhere (tabs, popups, workers).
func detectUserAgent(opt BrowserOptions) (string, error) {
	actx, cancelAlloc := chromedp.NewExecAllocator(context.Background(), chromeFlags(opt)...)
	defer cancelAlloc()
	ctx, cancel := chromedp.NewContext(actx)
	defer cancel()
	v, err := chromedp.CallBrowser(ctx, browser.GetVersion, cdp.Empty{})
	if err != nil {
		return "", err
	}
	return strings.Replace(v.UserAgent, "HeadlessChrome", "Chrome", 1), nil
}

func startBrowser(hub *Hub, opt BrowserOptions) (*Browser, error) {
	ua, err := detectUserAgent(opt)
	if err != nil {
		return nil, err
	}
	opts := append(chromeFlags(opt), chromedp.UserAgent(ua), chromedp.UserDataDir(opt.Profile))
	allocCtx, cancelAlloc := chromedp.NewExecAllocator(context.Background(), opts...)
	root, _ := chromedp.NewContext(allocCtx)
	if err := chromedp.Do(root); err != nil { // starts Chrome and attaches to its first blank tab
		cancelAlloc()
		return nil, err
	}

	b := &Browser{
		root:        root,
		cancelAlloc: cancelAlloc,
		hub:         hub,
		opt:         opt,
		wake:        make(chan struct{}, 1),
		known:       map[string]bool{},
		view:        View{opt.Width, opt.Height, opt.Quality},
		streams:     map[string]byte{},
		ua:          ua,
		dl:          newDownloads(),
	}
	for _, d := range []string{opt.DownloadDir, opt.UploadDir, opt.StateDir} {
		if d != "" {
			os.MkdirAll(d, 0o700)
		}
	}
	firstBlank := chromedp.FromContext(root).Target.TargetID
	b.known[string(firstBlank)] = true

	if err := chromedp.ListenBrowser(root, b.onBrowserEvent); err != nil {
		return nil, err
	}
	if opt.DownloadDir != "" {
		ev := true
		if _, err := chromedp.CallBrowser(root, browser.SetDownloadBehavior, browser.SetDownloadBehaviorParams{
			Behavior: browser.SetDownloadBehaviorBehaviorAllow, DownloadPath: opt.DownloadDir, EventsEnabled: &ev}); err != nil {
			log.Println("chrome: downloads:", err)
		}
	}
	if _, err := b.openTab(opt.StartURL); err != nil {
		return nil, err
	}
	// Our tabs live in the same window, so closing the startup tab keeps Chrome running.
	chromedp.CallBrowser(root, target.CloseTarget, target.CloseTargetParams{TargetID: firstBlank})

	go b.loop()
	go b.watchAudio()
	return b, nil
}

// ---------- Command queue ----------

// Each Chrome call waits for Chrome, and mouse moves and wheel turns can come in
// faster than a busy page handles them. Queued moves and wheel turns are merged,
// so the screen follows the hand instead of replaying a backlog for seconds.
type job struct {
	kind  byte // jobFunc runs f, the others send mouse to the active tab
	f     func()
	mouse Msg
}

const (
	jobFunc byte = iota
	jobMouse
	jobMove
	jobWheel
)

func (b *Browser) loop() {
	for range b.wake {
		for {
			b.qmu.Lock()
			if len(b.jobs) == 0 {
				b.qmu.Unlock()
				break
			}
			j := b.jobs[0]
			b.jobs[0] = nil
			b.jobs = b.jobs[1:]
			b.qmu.Unlock()
			if j.kind == jobFunc {
				j.f()
			} else {
				b.dispatchMouse(j.mouse)
			}
		}
	}
}

func (b *Browser) push(j *job) {
	b.qmu.Lock()
	if !b.mergeLocked(j) && len(b.jobs) < 1024 { // queue full: drop
		b.jobs = append(b.jobs, j)
	}
	b.qmu.Unlock()
	select {
	case b.wake <- struct{}{}:
	default:
	}
}

// mergeLocked folds a move or wheel turn into a queued one of the same kind.
// Only the moves and wheel turns after the last other command are looked at,
// so clicks and keys keep their order.
func (b *Browser) mergeLocked(j *job) bool {
	if j.kind != jobMove && j.kind != jobWheel {
		return false
	}
	for i := len(b.jobs) - 1; i >= 0; i-- {
		q := b.jobs[i]
		if q.kind != jobMove && q.kind != jobWheel {
			return false
		}
		if q.kind != j.kind || q.mouse.Mods != j.mouse.Mods || q.mouse.Buttons != j.mouse.Buttons {
			continue
		}
		dx, dy := q.mouse.DX, q.mouse.DY
		q.mouse = j.mouse // newest position
		if j.kind == jobWheel {
			q.mouse.DX += dx
			q.mouse.DY += dy
		}
		return true
	}
	return false
}

func (b *Browser) enqueue(f func()) { b.push(&job{kind: jobFunc, f: f}) }

// call runs f from the queue and waits for it (used by the AI agent, which needs results).
func (b *Browser) call(f func() error) error {
	done := make(chan error, 1)
	b.enqueue(func() { done <- f() })
	select {
	case err := <-done:
		return err
	case <-time.After(45 * time.Second):
		return errors.New("the browser did not respond in time")
	}
}

// run runs f on the active tab from the queue and waits for it.
func (b *Browser) run(f func(t *Tab) error) error {
	return b.call(func() error {
		b.mu.Lock()
		t := b.active
		b.mu.Unlock()
		if t == nil {
			return errors.New("no tab is open")
		}
		return f(t)
	})
}

// busy reports whether the active tab is still loading.
func (b *Browser) busy() bool {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.active != nil && b.active.loading
}

func (b *Browser) viewSize() View {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.view
}

// Close shuts Chrome down, so no process is left behind when we exit.
func (b *Browser) Close() {
	done := make(chan struct{})
	go func() { chromedp.Cancel(b.root); close(done) }()
	select {
	case <-done:
	case <-time.After(3 * time.Second):
	}
	b.cancelAlloc()
}

// onTab runs f on the active tab from the queue.
func (b *Browser) onTab(f func(t *Tab) error) {
	b.enqueue(func() {
		b.mu.Lock()
		t := b.active
		b.mu.Unlock()
		if t == nil {
			return
		}
		if err := f(t); err != nil && t.ctx.Err() == nil {
			log.Println("chrome:", err)
		}
	})
}

// ---------- Tabs ----------

// createTab opens a blank tab in the shared window.
func (b *Browser) createTab() (*Tab, error) {
	b.createMu.Lock()
	nw := false
	res, err := chromedp.CallBrowser(b.root, target.CreateTarget, target.CreateTargetParams{URL: "about:blank", NewWindow: &nw})
	if err == nil {
		b.known[string(res.TargetID)] = true
	}
	b.createMu.Unlock()
	if err != nil {
		return nil, err
	}
	return b.attach(string(res.TargetID), "", false)
}

// openTab opens a tab, shows it and then loads the page, like Chrome's new tab button.
func (b *Browser) openTab(rawURL string) (*Tab, error) {
	t, err := b.createTab()
	if err != nil {
		return nil, err
	}
	b.activate(t)
	if u := normalizeURL(rawURL); u != "" {
		b.mu.Lock()
		t.fresh = true
		b.mu.Unlock()
		_, err = chromedp.Call(t.ctx, page.Navigate, page.NavigateParams{URL: u})
	}
	return t, err
}

// attach connects to a tab, prepares it (screen size, audio tap) and adds it to the tab strip.
func (b *Browser) attach(id, openerID string, opened bool) (*Tab, error) {
	t := &Tab{id: id, opener: openerID, title: "New Tab", url: "about:blank", obs: newObserver(id)}
	ctx, cancel := chromedp.NewContext(b.root, chromedp.WithTargetID(target.ID(id)), chromedp.WithListener(b.tabListener(t)))
	t.ctx, t.cancel = ctx, cancel
	if err := chromedp.Do(ctx); err != nil {
		cancel()
		return nil, err
	}

	b.mu.Lock()
	v, emu := b.view, b.emu
	b.mu.Unlock()
	run := true
	steps := []func() error{
		func() error { // console and network, so the agent can see what the page does
			bufTotal, bufRes := int64(10<<20), int64(2<<20)
			if _, err := chromedp.Call(ctx, network.Enable, network.EnableParams{MaxTotalBufferSize: &bufTotal, MaxResourceBufferSize: &bufRes}); err != nil {
				return err
			}
			if _, err := chromedp.Call(ctx, cdpruntime.Enable, cdp.Empty{}); err != nil {
				return err
			}
			_, err := chromedp.Call(ctx, cdplog.Enable, cdp.Empty{})
			return err
		},
		func() error {
			if emu != (Emulation{}) { // tabs opened later get the same device
				return b.applyEmulation(t, v, emu)
			}
			_, err := chromedp.Call(ctx, emulation.SetDeviceMetricsOverride, metrics(v, emu))
			return err
		},
		func() error {
			if _, err := chromedp.Call(ctx, cdpruntime.AddBinding, cdpruntime.AddBindingParams{Name: metaBinding}); err != nil {
				return err
			}
			_, err := chromedp.Call(ctx, page.AddScriptToEvaluateOnNewDocument, page.AddScriptToEvaluateOnNewDocumentParams{Source: metaJS, RunImmediately: &run})
			return err
		},
		func() error {
			if !b.opt.Audio {
				return nil
			}
			if _, err := chromedp.Call(ctx, cdpruntime.AddBinding, cdpruntime.AddBindingParams{Name: audioBinding}); err != nil {
				return err
			}
			// A tab that a page opened is already loading, so also run the tap in it now.
			_, err := chromedp.Call(ctx, page.AddScriptToEvaluateOnNewDocument, page.AddScriptToEvaluateOnNewDocumentParams{Source: audioJS, RunImmediately: &run})
			return err
		},
	}
	for _, s := range steps {
		if err := s(); err != nil {
			go cancel()
			return nil, err
		}
	}

	b.mu.Lock()
	i := len(b.tabs)
	if opened { // like Chrome: right after the page that opened it (and its earlier tabs)
		if j := b.findLocked(openerID); j >= 0 {
			i = j + 1
			for i < len(b.tabs) && b.tabs[i].opener == openerID {
				i++
			}
		}
	}
	b.tabs = slices.Insert(b.tabs, i, t)
	b.mu.Unlock()
	b.notify()
	go b.refreshNav(t)
	return t, nil
}

// activate shows tab t to everyone: only the active tab streams frames.
func (b *Browser) activate(t *Tab) {
	b.mu.Lock()
	old := b.active
	if old == t || !slices.Contains(b.tabs, t) {
		b.mu.Unlock()
		return
	}
	b.active = t
	b.gotFrame = false
	v := b.view
	b.mu.Unlock()
	b.notify()

	if old != nil {
		chromedp.Call(old.ctx, page.StopScreencast, cdp.Empty{})
	}
	chromedp.CallBrowser(b.root, target.ActivateTarget, target.ActivateTargetParams{TargetID: target.ID(t.id)})
	b.startScreencast(t, v)
}

func (b *Browser) startScreencast(t *Tab, v View) {
	q := int64(v.Quality)
	// Chrome refuses while a page is between two documents, so retry briefly.
	for try := 0; try < 10; try++ {
		_, err := chromedp.Call(t.ctx, page.StartScreencast, page.StartScreencastParams{
			Format: page.StartScreencastFormatJpeg, Quality: &q,
			MaxWidth: int64(v.Width), MaxHeight: int64(v.Height), EveryNthFrame: 1,
		})
		if err == nil || t.ctx.Err() != nil || strings.Contains(err.Error(), "already active") {
			break
		}
		if try == 9 {
			log.Println("chrome: screencast:", err)
		}
		time.Sleep(150 * time.Millisecond)
	}
	// A page that doesn't change sends no frame, so take one picture right away.
	go func() {
		time.Sleep(300 * time.Millisecond)
		b.mu.Lock()
		need := b.active == t && !b.gotFrame
		b.mu.Unlock()
		if !need {
			return
		}
		shot, err := chromedp.Call(t.ctx, page.CaptureScreenshot, page.CaptureScreenshotParams{Format: page.CaptureScreenshotFormatJpeg, Quality: &q})
		if err != nil {
			return
		}
		pkt := append([]byte{pktFrame}, shot.Data...)
		b.mu.Lock()
		ok := b.active == t && !b.gotFrame
		if ok {
			b.frame = pkt
		}
		b.mu.Unlock()
		if ok {
			b.hub.broadcastFrame(pkt)
		}
	}()
}

func (b *Browser) findLocked(id string) int {
	return slices.IndexFunc(b.tabs, func(t *Tab) bool { return t.id == id })
}

// closeTab removes a tab. Closing the last tab opens a fresh one, so there is always a tab.
func (b *Browser) closeTab(id string) {
	b.mu.Lock()
	i := b.findLocked(id)
	if i < 0 {
		b.mu.Unlock()
		return
	}
	t := b.tabs[i]
	last := len(b.tabs) == 1
	b.mu.Unlock()

	if last {
		b.openTab(b.opt.StartURL)
	}

	b.mu.Lock()
	var next *Tab
	if i = b.findLocked(id); i >= 0 {
		b.tabs = slices.Delete(b.tabs, i, i+1)
		if b.active == t && len(b.tabs) > 0 {
			next = b.tabs[min(i, len(b.tabs)-1)] // like Chrome: the tab to the right
		}
		if b.active == t {
			b.active = nil
		}
	}
	for k := range b.streams {
		if strings.HasPrefix(k, id+"|") {
			delete(b.streams, k)
		}
	}
	b.mu.Unlock()
	b.notify()
	if next != nil {
		b.activate(next)
	}
	go t.cancel() // closes the target in Chrome
}

// ---------- Events from Chrome ----------

// Browser-wide events: tabs appearing, disappearing, changing title/URL.
// Must return fast and must not call Chrome directly.
func (b *Browser) onBrowserEvent(ev any) {
	switch e := ev.(type) {
	case *target.EventTargetCreated:
		if info := e.TargetInfo; info.Type == "page" && info.Subtype == "" {
			go b.adopt(info)
		}
	case *target.EventTargetInfoChanged:
		b.onInfo(e.TargetInfo)
	case *target.EventTargetDestroyed:
		id := string(e.TargetID)
		b.enqueue(func() { b.closeTab(id) })
	case *browser.EventDownloadWillBegin, *browser.EventDownloadProgress:
		if done := b.dl.event(ev, b.opt.DownloadDir); done != nil {
			go b.hub.systemChat("Downloaded " + done.Name)
		}
	case *target.EventTargetCrashed:
		id := string(e.TargetID)
		go b.hub.systemChat("A tab crashed and was reloaded")
		b.enqueue(func() {
			b.mu.Lock()
			i := b.findLocked(id)
			var t *Tab
			if i >= 0 {
				t = b.tabs[i]
			}
			b.mu.Unlock()
			if t != nil {
				chromedp.Call(t.ctx, page.Reload, page.ReloadParams{})
			}
		})
	}
}

// adopt picks up a tab that a page opened (target=_blank link, window.open…).
func (b *Browser) adopt(info *target.Info) {
	id := string(info.TargetID)
	b.createMu.Lock()
	if b.known[id] {
		b.createMu.Unlock()
		return
	}
	b.known[id] = true
	b.createMu.Unlock()

	b.mu.Lock()
	full := len(b.tabs) >= b.opt.MaxTabs
	background := time.Now().Before(b.bgUntil)
	b.mu.Unlock()
	if full {
		chromedp.CallBrowser(b.root, target.CloseTarget, target.CloseTargetParams{TargetID: info.TargetID})
		b.hub.systemChat("Tab limit reached — close a tab first")
		return
	}
	t, err := b.attach(id, string(info.OpenerID), true)
	if err != nil {
		log.Println("chrome: new tab:", err)
		return
	}
	if res, err := chromedp.CallBrowser(b.root, target.GetTargetInfo, target.GetTargetInfoParams{TargetID: info.TargetID}); err == nil && res.TargetInfo != nil {
		info = res.TargetInfo // the page kept loading while we attached
	}
	b.onInfo(info)
	if !background {
		b.enqueue(func() { b.activate(t) })
	}
}

func (b *Browser) onInfo(info *target.Info) {
	b.mu.Lock()
	i := b.findLocked(string(info.TargetID))
	if i < 0 {
		b.mu.Unlock()
		return
	}
	t := b.tabs[i]
	changed := t.title != info.Title || t.url != info.URL
	t.title, t.url = info.Title, info.URL
	b.mu.Unlock()
	if b.dropUnwanted(info.URL, string(info.TargetID)) {
		return
	}
	if changed {
		b.notify()
		go b.refreshNav(t)
	}
}

// dropUnwanted closes a tab that Chrome opened on its own to an internal settings page — its
// auto-update "About Chrome" page (chrome://settings/help). The app never opens settings itself,
// so any such tab is unwanted; closing it stops the browser from "jumping to Settings".
func (b *Browser) dropUnwanted(url, id string) bool {
	if !strings.HasPrefix(url, "chrome://settings") {
		return false
	}
	b.enqueue(func() { b.closeTab(id) })
	return true
}

// Events of one tab. Must return fast and must not call Chrome directly.
func (b *Browser) tabListener(t *Tab) func(ev any) {
	return func(ev any) {
		t.obs.event(ev)
		switch e := ev.(type) {
		case *page.EventScreencastFrame:
			sid := e.SessionID
			go chromedp.Call(t.ctx, page.ScreencastFrameAck, page.ScreencastFrameAckParams{SessionID: sid})
			pkt := make([]byte, 1+len(e.Data))
			pkt[0] = pktFrame
			copy(pkt[1:], e.Data)
			b.mu.Lock()
			active := b.active == t
			if active {
				b.frame, b.gotFrame = pkt, true
			}
			b.mu.Unlock()
			if active {
				b.hub.broadcastFrame(pkt)
			}

		case *cdpruntime.EventBindingCalled:
			switch e.Name {
			case audioBinding:
				b.onAudio(t, e.Payload)
			case metaBinding:
				b.onMeta(t, e.Payload)
			}

		case *page.EventFrameNavigated:
			if e.Frame.ParentID != "" {
				return
			}
			if b.dropUnwanted(e.Frame.URL, t.id) {
				return
			}
			b.mu.Lock()
			t.docTitle, t.favicon = "", "" // a new document reports its own
			reset := t.fresh && e.Frame.URL != "about:blank"
			if reset {
				t.fresh = false
			}
			b.mu.Unlock()
			b.notify()
			if reset { // a new tab starts at about:blank; going "back" there makes no sense
				go func() {
					chromedp.Call(t.ctx, page.ResetNavigationHistory, cdp.Empty{})
					b.refreshNav(t)
				}()
			}

		case *page.EventFrameStartedLoading:
			if string(e.FrameID) == t.id {
				b.setLoading(t, true)
			}

		case *page.EventFrameStoppedLoading:
			if string(e.FrameID) == t.id {
				b.setLoading(t, false)
				go b.refreshNav(t)
			}

		case *page.EventJavascriptDialogOpening:
			go chromedp.Call(t.ctx, page.HandleJavaScriptDialog, page.HandleJavaScriptDialogParams{Accept: true})
			if e.Message != "" {
				go b.hub.systemChat("Page says: " + e.Message)
			}
		}
	}
}

func (b *Browser) setLoading(t *Tab, v bool) {
	b.mu.Lock()
	changed := t.loading != v
	t.loading = v
	b.mu.Unlock()
	if changed {
		b.notify()
	}
}

// onMeta handles the title/favicon report of a page. The page can call the binding itself, so validate.
func (b *Browser) onMeta(t *Tab, p string) {
	var m struct {
		T string `json:"t"`
		I string `json:"i"`
	}
	if len(p) > 80*1024 || json.Unmarshal([]byte(p), &m) != nil {
		return
	}
	if r := []rune(m.T); len(r) > 300 {
		m.T = string(r[:300])
	}
	if len(m.I) > 64*1024 || !(strings.HasPrefix(m.I, "http://") || strings.HasPrefix(m.I, "https://") || strings.HasPrefix(m.I, "data:image/")) {
		m.I = ""
	}
	b.mu.Lock()
	changed := t.docTitle != m.T || t.favicon != m.I
	t.docTitle, t.favicon = m.T, m.I
	b.mu.Unlock()
	if changed {
		b.notify()
	}
}

func (b *Browser) refreshNav(t *Tab) {
	b.mu.Lock()
	t.navSeq++
	seq := t.navSeq
	b.mu.Unlock()
	h, err := chromedp.Call(t.ctx, page.GetNavigationHistory, cdp.Empty{})
	if err != nil {
		return
	}
	back, fwd := h.CurrentIndex > 0, h.CurrentIndex < int64(len(h.Entries))-1
	b.mu.Lock()
	if seq != t.navSeq { // a newer check started meanwhile
		b.mu.Unlock()
		return
	}
	changed := t.canBack != back || t.canFwd != fwd
	t.canBack, t.canFwd = back, fwd
	b.mu.Unlock()
	if changed {
		b.notify()
	}
}

// Payload from a page: "<stream id>|<sample rate>|<base64 int16 stereo PCM>".
// The page itself can call the binding, so validate everything.
func (b *Browser) onAudio(t *Tab, p string) {
	parts := strings.SplitN(p, "|", 3)
	if len(parts) != 3 || len(parts[2]) > 512*1024 {
		return
	}
	jsID, err1 := strconv.Atoi(parts[0])
	rate, err2 := strconv.Atoi(parts[1])
	if err1 != nil || err2 != nil || jsID < 1 || jsID > 255 || rate < 8000 || rate > 96000 {
		return
	}
	pcm, err := base64.StdEncoding.DecodeString(parts[2])
	if err != nil || len(pcm) == 0 || len(pcm)%4 != 0 {
		return
	}

	b.mu.Lock()
	wasAudible := t.audible
	t.audible, t.lastAudio = true, time.Now()
	muted := t.muted
	key := t.id + "|" + parts[0]
	sid, ok := b.streams[key]
	if !ok { // give every (tab, stream) its own id so viewers keep them apart
		b.nextStream = b.nextStream%255 + 1
		sid = byte(b.nextStream)
		b.streams[key] = sid
	}
	b.mu.Unlock()
	if !wasAudible {
		b.notify()
	}
	if muted {
		return
	}

	pkt := make([]byte, 6+len(pcm))
	pkt[0] = pktAudio
	pkt[1] = sid
	binary.LittleEndian.PutUint32(pkt[2:], uint32(rate))
	copy(pkt[6:], pcm)
	b.hub.broadcastAudio(pkt)
}

// Clears the speaker icon of tabs that went quiet.
func (b *Browser) watchAudio() {
	for range time.Tick(500 * time.Millisecond) {
		changed := false
		b.mu.Lock()
		for _, t := range b.tabs {
			if t.audible && time.Since(t.lastAudio) > 1500*time.Millisecond {
				t.audible, changed = false, true
			}
		}
		b.mu.Unlock()
		if changed {
			b.notify()
		}
	}
}

// ---------- State for the web UI ----------

// notify sends the tab list to everyone, batching changes that come in quick succession.
func (b *Browser) notify() {
	b.nmu.Lock()
	defer b.nmu.Unlock()
	if b.notifying {
		return
	}
	b.notifying = true
	time.AfterFunc(40*time.Millisecond, func() {
		b.nmu.Lock()
		b.notifying = false
		b.nmu.Unlock()
		tabs, active, _, _ := b.State()
		b.hub.broadcastTabs(tabs, active)
	})
}

func (b *Browser) State() (tabs []TabInfo, active string, frame []byte, view View) {
	b.mu.Lock()
	defer b.mu.Unlock()
	tabs = make([]TabInfo, 0, len(b.tabs))
	for _, t := range b.tabs {
		title := t.docTitle
		if title == "" {
			title = t.title
		}
		tabs = append(tabs, TabInfo{ID: t.id, Title: title, URL: t.url, Favicon: t.favicon,
			Loading: t.loading, Audible: t.audible, Muted: t.muted, CanBack: t.canBack, CanFwd: t.canFwd})
	}
	if b.active != nil {
		active = b.active.id
	}
	return tabs, active, b.frame, b.view
}

// ---------- Commands from the web UI ----------

func (b *Browser) NewTab(raw string) {
	b.enqueue(func() {
		b.mu.Lock()
		full := len(b.tabs) >= b.opt.MaxTabs
		b.mu.Unlock()
		if full {
			b.hub.systemChat("Tab limit reached — close a tab first")
			return
		}
		if raw == "" {
			raw = b.opt.StartURL
		}
		if _, err := b.openTab(raw); err != nil {
			log.Println("chrome: new tab:", err)
		}
	})
}

func (b *Browser) CloseTab(id string) { b.enqueue(func() { b.closeTab(id) }) }

func (b *Browser) ActivateTab(id string) {
	b.enqueue(func() {
		b.mu.Lock()
		i := b.findLocked(id)
		var t *Tab
		if i >= 0 {
			t = b.tabs[i]
		}
		b.mu.Unlock()
		if t != nil {
			b.activate(t)
		}
	})
}

func (b *Browser) MoveTab(id string, to int) {
	b.mu.Lock()
	if i := b.findLocked(id); i >= 0 {
		t := b.tabs[i]
		b.tabs = slices.Delete(b.tabs, i, i+1)
		to = min(max(to, 0), len(b.tabs))
		b.tabs = slices.Insert(b.tabs, to, t)
	}
	b.mu.Unlock()
	b.notify()
}

func (b *Browser) MuteTab(id string) {
	b.mu.Lock()
	if i := b.findLocked(id); i >= 0 {
		b.tabs[i].muted = !b.tabs[i].muted
	}
	b.mu.Unlock()
	b.notify()
}

// SetView changes the screen size and image quality for everyone.
func (b *Browser) SetView(v View) {
	v.Width = min(max(v.Width, 640), 2560)
	v.Height = min(max(v.Height, 360), 1600)
	v.Quality = min(max(v.Quality, 10), 95)
	b.enqueue(func() {
		b.mu.Lock()
		b.view = v
		emu := b.emu
		tabs := slices.Clone(b.tabs)
		t := b.active
		b.gotFrame = false
		b.mu.Unlock()
		for _, tab := range tabs {
			chromedp.Call(tab.ctx, emulation.SetDeviceMetricsOverride, metrics(v, emu))
		}
		if t != nil {
			chromedp.Call(t.ctx, page.StopScreencast, cdp.Empty{})
			b.startScreencast(t, v)
		}
		b.hub.broadcastView(v)
	})
}

func (b *Browser) Navigate(raw string) {
	u := normalizeURL(raw)
	if u == "" {
		return
	}
	b.onTab(func(t *Tab) error {
		_, err := chromedp.Call(t.ctx, page.Navigate, page.NavigateParams{URL: u})
		return err
	})
}

func (b *Browser) History(delta int64) {
	b.onTab(func(t *Tab) error {
		_, err := goHistory(t, delta)
		return err
	})
}

// goHistory moves back (-1) or forward (1) in the tab's history; false if there is nowhere to go.
func goHistory(t *Tab, delta int64) (bool, error) {
	h, err := chromedp.Call(t.ctx, page.GetNavigationHistory, cdp.Empty{})
	if err != nil {
		return false, err
	}
	i := h.CurrentIndex + delta
	if i < 0 || i >= int64(len(h.Entries)) {
		return false, nil
	}
	_, err = chromedp.Call(t.ctx, page.NavigateToHistoryEntry, page.NavigateToHistoryEntryParams{EntryID: h.Entries[i].ID})
	return true, err
}

func (b *Browser) Reload() {
	b.onTab(func(t *Tab) error {
		_, err := chromedp.Call(t.ctx, page.Reload, page.ReloadParams{})
		return err
	})
}

func (b *Browser) Stop() {
	b.onTab(func(t *Tab) error {
		_, err := chromedp.Call(t.ctx, page.StopLoading, cdp.Empty{})
		return err
	})
}

func normalizeURL(raw string) string {
	raw = strings.TrimSpace(raw)
	if raw == "" {
		return ""
	}
	if strings.HasPrefix(raw, "http://") || strings.HasPrefix(raw, "https://") || raw == "about:blank" {
		return raw
	}
	if strings.Contains(raw, "://") {
		return "" // block file://, chrome:// … so nobody can read files on the server
	}
	looksLikeHost := !strings.Contains(raw, " ") && (strings.Contains(raw, ".") || strings.HasPrefix(raw, "localhost"))
	if looksLikeHost {
		host := strings.Split(strings.Split(raw, "/")[0], ":")[0]
		if host == "localhost" || net.ParseIP(host) != nil { // LAN machines rarely have https
			return "http://" + raw
		}
		return "https://" + raw
	}
	return "https://www.google.com/search?q=" + url.QueryEscape(raw)
}

// ---------- Mouse ----------

var buttonNames = []input.MouseButton{input.MouseButtonLeft, input.MouseButtonMiddle, input.MouseButtonRight}

// cdproto drops zero deltas from the JSON, but Chrome requires both, so wheel events use our own params.
type wheelParams struct {
	Type      string  `json:"type"`
	X         float64 `json:"x"`
	Y         float64 `json:"y"`
	DeltaX    float64 `json:"deltaX"`
	DeltaY    float64 `json:"deltaY"`
	Modifiers int64   `json:"modifiers"`
}

var dispatchWheel = cdp.Command[wheelParams, cdp.Empty]{Method: input.CommandDispatchMouseEvent}

func clamp01(v float64) float64 { return min(max(v, 0), 1) }

func (b *Browser) Mouse(m Msg) {
	j := &job{kind: jobMouse, mouse: m}
	switch m.Event {
	case "move":
		j.kind = jobMove
	case "wheel":
		j.kind = jobWheel
	}
	b.push(j)
}

// dispatchMouse runs from the queue.
func (b *Browser) dispatchMouse(m Msg) {
	mods := fixMods(m.Mods)
	b.mu.Lock()
	t, v := b.active, b.view
	if m.Event == "down" && (m.Button == 1 || mods&(modCtrl|modMeta) != 0) {
		b.bgUntil = time.Now().Add(1500 * time.Millisecond) // middle/ctrl-click opens a background tab
	}
	b.mu.Unlock()
	if t == nil {
		return
	}
	if err := b.sendMouse(t, v, m, mods); err != nil && t.ctx.Err() == nil {
		log.Println("chrome:", err)
	}
}

func (b *Browser) sendMouse(t *Tab, v View, m Msg, mods int) error {
	x, y := clamp01(m.X)*float64(v.Width), clamp01(m.Y)*float64(v.Height)
	if m.Event == "wheel" {
		_, err := chromedp.Call(t.ctx, dispatchWheel, wheelParams{Type: string(input.DispatchMouseEventTypeMouseWheel), X: x, Y: y, DeltaX: m.DX, DeltaY: m.DY, Modifiers: int64(mods)})
		return err
	}
	p := input.DispatchMouseEventParams{X: x, Y: y, Modifiers: int64(mods), Buttons: int64(m.Buttons), Button: input.MouseButtonNone}
	switch m.Event {
	case "down", "up":
		p.Type = input.DispatchMouseEventTypeMousePressed
		if m.Event == "up" {
			p.Type = input.DispatchMouseEventTypeMouseReleased
		}
		if m.Button >= 0 && m.Button < len(buttonNames) {
			p.Button = buttonNames[m.Button]
		}
		p.ClickCount = int64(max(m.Count, 1))
	case "move":
		p.Type = input.DispatchMouseEventTypeMouseMoved
		switch { // button held while dragging
		case m.Buttons&1 != 0:
			p.Button = input.MouseButtonLeft
		case m.Buttons&2 != 0:
			p.Button = input.MouseButtonRight
		case m.Buttons&4 != 0:
			p.Button = input.MouseButtonMiddle
		}
	default:
		return nil
	}
	_, err := chromedp.Call(t.ctx, input.DispatchMouseEvent, p)
	return err
}

// ---------- Keyboard ----------

const (
	modCtrl  = 2
	modMeta  = 4
	modShift = 8
)

// A Mac user driving a Windows/Linux server: Cmd acts as Ctrl.
func fixMods(mods int) int {
	if runtime.GOOS != "darwin" && mods&modMeta != 0 {
		mods = mods&^modMeta | modCtrl
	}
	return mods
}

// On macOS, Chrome doesn't handle Cmd+A/C/X/Z from DevTools key events by itself;
// the editing command has to be sent along.
var macCommands = map[string]string{"a": "selectAll", "c": "copy", "x": "cut", "z": "undo"}

const selectionJS = `(() => {
  const a = document.activeElement;
  if (a && (a.tagName === 'INPUT' || a.tagName === 'TEXTAREA') && typeof a.selectionStart === 'number')
    return a.value.substring(a.selectionStart, a.selectionEnd);
  return String(window.getSelection() || '');
})()`

// onCopy receives the selected text when the controller presses Ctrl/Cmd+C or X.
func (b *Browser) Key(m Msg, onCopy func(string)) {
	mods := fixMods(m.Mods)
	shortcut := mods&(modCtrl|modMeta) != 0
	lower := strings.ToLower(m.Key)

	p := input.DispatchKeyEventParams{
		Key: m.Key, Code: m.Code, Modifiers: int64(mods),
		WindowsVirtualKeyCode: int64(m.KeyCode), NativeVirtualKeyCode: int64(m.KeyCode),
	}
	if m.Event == "up" {
		p.Type = input.DispatchKeyEventTypeKeyUp
	} else {
		text := ""
		if len([]rune(m.Key)) == 1 && !shortcut {
			text = m.Key
		} else if m.Key == "Enter" {
			text = "\r"
		}
		if text != "" {
			p.Type = input.DispatchKeyEventTypeKeyDown
			p.Text, p.UnmodifiedText = text, text
		} else {
			p.Type = input.DispatchKeyEventTypeRawKeyDown
		}
		if shortcut && runtime.GOOS == "darwin" {
			if cmd, ok := macCommands[lower]; ok {
				if cmd == "undo" && mods&modShift != 0 {
					cmd = "redo"
				}
				p.Commands = []string{cmd}
			}
		}
	}

	wantCopy := m.Event != "up" && shortcut && (lower == "c" || lower == "x")
	b.onTab(func(t *Tab) error {
		if wantCopy {
			if s, err := chromedp.Run(t.ctx, chromedp.Evaluate[string](selectionJS)); err == nil && s != "" {
				onCopy(s)
			}
		}
		_, err := chromedp.Call(t.ctx, input.DispatchKeyEvent, p)
		return err
	})
}

// Text types a piece of text directly (IME input, paste…).
func (b *Browser) Text(s string) {
	if s == "" {
		return
	}
	b.onTab(func(t *Tab) error {
		_, err := chromedp.Call(t.ctx, input.InsertText, input.InsertTextParams{Text: s})
		return err
	})
}
