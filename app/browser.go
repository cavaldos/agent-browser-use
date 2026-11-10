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
	"fmt"
	"log"
	"math"
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
	Engine        Engine // resolved from -engine / -chrome
	Audio         bool
	MaxTabs       int
	DownloadDir   string // where Chrome saves downloads (also readable by browser_file_upload)
	UploadDir     string // files the agent may upload
	StateDir      string // saved cookie/storage states (browser_state)
	FlowDir       string // recorded flows (browser_get_flow / browser_run_flow)
}

// View is the size and image quality of the shared screen.
type View struct {
	Width   int `json:"width"`
	Height  int `json:"height"`
	Quality int `json:"quality"`
}

type Tab struct {
	lane       sync.Mutex // serializes single agent operations targeting this tab
	sessMu     sync.Mutex // guards owner and ownerUntil
	owner      string     // agent holding an exclusive session on this tab (browser_lock_tab)
	ownerUntil time.Time
	gesture    sync.Mutex // held for a whole input tool call, so agents sharing a tab never interleave a click, drag or typing
	id         string
	opener     string // tab that opened this one, if any
	ctx        context.Context
	cancel     context.CancelFunc

	// guarded by Browser.mu
	title, url              string // from Chrome's target info
	docTitle, favicon       string // reported by the page itself (metaJS)
	loading, audible, muted bool
	canBack, canFwd         bool
	fresh                   bool // first page of a new tab: drop the about:blank history entry once it commits
	navSeq                  int  // newest refreshNav wins
	lastAudio               time.Time
	recScript               page.ScriptIdentifier // the flow recorder, while a recording is on
	share                   *shareOut             // the page is screen sharing the shared screen (share.go)
	refitAt                 time.Time             // last time refit restored this tab's screen size

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
	root        context.Context // owns the Chrome process; replaced by restart, guarded by mu
	cancelAlloc context.CancelFunc
	rmu         sync.Mutex           // held while Chrome restarts
	closed      bool                 // Close was called: never restart again
	recovered   map[string]time.Time // URL -> when a hung tab on it was last reopened
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

	ua    string    // Chrome's normal user agent (what "no emulation" means)
	emu   Emulation // device emulation, guarded by mu
	dl    *downloads
	recOn bool // a flow is being recorded: every tab runs the recorder (guarded by mu)
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

// stealthJS makes the headless/automated browser look like a normal Chrome to
// page scripts: no webdriver flag, a real chrome object, plausible hardware,
// screen and WebGL values, and outer window dimensions.
const stealthJS = `(() => {
  if (window.__abuStealth) return;
  window.__abuStealth = true;
  const def = (obj, key, value) => {
    try { Object.defineProperty(obj, key, { get: () => value, configurable: true }); } catch (e) {}
  };
  try { def(Object.getPrototypeOf(navigator), 'webdriver', undefined); } catch (e) { def(navigator, 'webdriver', undefined); }
  if (!window.chrome) window.chrome = {};
  if (!window.chrome.runtime) window.chrome.runtime = { connect: function(){}, sendMessage: function(){} };
  if (!window.chrome.loadTimes) window.chrome.loadTimes = function(){ return {}; };
  if (!window.chrome.csi) window.chrome.csi = function(){ return {}; };
  if (!window.chrome.app) window.chrome.app = { isInstalled: false };
  def(navigator, 'languages', ['en-US', 'en']);
  (function(){
    const ua = navigator.userAgent || '';
    if (/Mac/.test(ua)) def(navigator, 'platform', 'MacIntel');
    else if (/Windows/.test(ua)) def(navigator, 'platform', 'Win32');
    else if (/Linux/.test(ua)) def(navigator, 'platform', 'Linux x86_64');
  })();
  def(navigator, 'hardwareConcurrency', 8);
  def(navigator, 'deviceMemory', 8);
  def(navigator, 'maxTouchPoints', 0);
  try {
    const origQuery = navigator.permissions && navigator.permissions.query;
    if (origQuery) {
      navigator.permissions.query = (params) => {
        return Promise.resolve({ state: 'prompt' });
      };
    }
  } catch (e) {}
  try {
    def(screen, 'width', 1920);
    def(screen, 'height', 1080);
    def(screen, 'availWidth', 1920);
    def(screen, 'availHeight', 1040);
    def(screen, 'colorDepth', 24);
    def(screen, 'pixelDepth', 24);
  } catch (e) {}
  (function(){ const ua = navigator.userAgent || ''; def(window, 'devicePixelRatio', /Mac/.test(ua) ? 2 : 1); })();
  try {
    const patchGL = (proto) => {
      if (!proto || !proto.getParameter) return;
      const orig = proto.getParameter;
      proto.getParameter = function(param) {
        const ua = navigator.userAgent || '';
        if (param === 37445) return /Mac/.test(ua) ? 'Apple Inc.' : 'Google Inc.';
        if (param === 37446) return /Mac/.test(ua) ? 'Apple GPU' : 'ANGLE (Google, Vulkan 1.3.0, SwiftShader)';
        return orig.call(this, param);
      };
    };
    patchGL(WebGLRenderingContext && WebGLRenderingContext.prototype);
    patchGL(window.WebGL2RenderingContext && WebGL2RenderingContext.prototype);
  } catch (e) {}
  try {
    if (navigator.userAgentData) {
      def(navigator, 'userAgentData', {
        brands: [{ brand: 'Google Chrome', version: '131' }, { brand: 'Chromium', version: '131' }, { brand: 'Not_A Brand', version: '24' }],
        mobile: false,
        platform: 'macOS',
        getHighEntropyValues: function() {
          return Promise.resolve({ architecture: 'arm', bitness: '64', brands: [{ brand: 'Google Chrome', version: '131' }], mobile: false, model: '', platform: 'macOS', platformVersion: '14.0.0', uaFullVersion: '131.0.0.0' });
        }
      });
    }
  } catch (e) {}
  try {
    if (navigator.connection) {
      def(navigator.connection, 'rtt', 50);
      def(navigator.connection, 'downlink', 10);
      def(navigator.connection, 'effectiveType', '4g');
    }
  } catch (e) {}
  try {
    Object.defineProperty(window, 'outerWidth', { get: () => window.innerWidth + 16, configurable: true });
    Object.defineProperty(window, 'outerHeight', { get: () => window.innerHeight + 85, configurable: true });
  } catch (e) {}
})();`

func chromeFlags(opt BrowserOptions) []chromedp.ExecAllocatorOption {
	opts := append(chromedp.DefaultExecAllocatorOptions[:],
		chromedp.Flag("enable-automation", false),
		chromedp.Flag("disable-blink-features", "AutomationControlled"),
		chromedp.Flag("mute-audio", true), // never play sound on the server itself
		chromedp.Flag("autoplay-policy", "no-user-gesture-required"),
		// Fewer processes = less RAM. Keeping iframes in the page process also lets
		// the audio tap reach embedded players.
		// keep GPU on so WebGL exists (a missing WebGL context is a headless tell)
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
	if !opt.Engine.Shell { // chrome-headless-shell is always headless and has no "new" mode
		opts = append(opts, chromedp.Flag("headless", "new"))
	}
	if opt.Engine.Path != "" {
		opts = append(opts, chromedp.ExecPath(opt.Engine.Path))
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
	tctx, tcancel := context.WithTimeout(ctx, 30*time.Second) // a browser that never comes up must not hang startup
	defer tcancel()
	v, err := chromedp.CallBrowser(tctx, browser.GetVersion, cdp.Empty{})
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
	b := &Browser{
		hub:       hub,
		opt:       opt,
		wake:      make(chan struct{}, 1),
		view:      View{opt.Width, opt.Height, opt.Quality},
		streams:   map[string]byte{},
		ua:        ua,
		dl:        newDownloads(),
		recovered: map[string]time.Time{},
	}
	for _, d := range []string{opt.DownloadDir, opt.UploadDir, opt.StateDir, opt.FlowDir} {
		if d != "" {
			os.MkdirAll(d, 0o700)
		}
	}
	go b.loop() // before launch: launch may already queue work (tab events)
	if err := b.launch(); err != nil {
		return nil, err
	}
	go b.watchAudio()
	go b.watchHealth()
	return b, nil
}

// launch starts Chrome and opens the start page. Used at startup and by restart.
func (b *Browser) launch() error {
	opts := append(chromeFlags(b.opt), chromedp.UserAgent(b.ua), chromedp.UserDataDir(b.opt.Profile))
	allocCtx, cancelAlloc := chromedp.NewExecAllocator(context.Background(), opts...)
	root, _ := chromedp.NewContext(allocCtx)
	// Starts Chrome and attaches to its first blank tab. Do binds the browser to root,
	// so bound it with a timer instead of a timeout context.
	timer := time.AfterFunc(30*time.Second, cancelAlloc)
	err := chromedp.Do(root)
	if !timer.Stop() && err == nil {
		err = errors.New("the browser took too long to start")
	}
	if err != nil {
		cancelAlloc()
		return err
	}

	b.mu.Lock()
	b.root, b.cancelAlloc = root, cancelAlloc
	b.mu.Unlock()
	firstBlank := chromedp.FromContext(root).Target.TargetID
	b.createMu.Lock()
	b.known = map[string]bool{string(firstBlank): true}
	b.createMu.Unlock()

	if err := chromedp.ListenBrowser(root, b.onBrowserEvent); err != nil {
		cancelAlloc()
		return err
	}
	if b.opt.DownloadDir != "" {
		ev := true
		ctx, cancel := cmdCtx(root)
		if _, err := chromedp.CallBrowser(ctx, browser.SetDownloadBehavior, browser.SetDownloadBehaviorParams{
			Behavior: browser.SetDownloadBehaviorBehaviorAllow, DownloadPath: b.opt.DownloadDir, EventsEnabled: &ev}); err != nil {
			log.Println("chrome: downloads:", err)
		}
		cancel()
	}
	if _, err := b.openTab(b.opt.StartURL); err != nil {
		cancelAlloc()
		return err
	}
	// Our tabs live in the same window, so closing the startup tab keeps Chrome running.
	ctx, cancel := cmdCtx(root)
	chromedp.CallBrowser(ctx, target.CloseTarget, target.CloseTargetParams{TargetID: firstBlank})
	cancel()
	return nil
}

// rootCtx is the context of the running Chrome (it changes when Chrome restarts).
func (b *Browser) rootCtx() context.Context {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.root
}

// cmdTimeout bounds one Chrome command, so a hung page or browser can never stall
// the command queue (and with it every later click, key and agent step).
const cmdTimeout = 15 * time.Second

func cmdCtx(ctx context.Context) (context.Context, context.CancelFunc) {
	return context.WithTimeout(ctx, cmdTimeout)
}

// ---------- Health: recover from a frozen tab or browser ----------

// watchHealth pings Chrome and the active tab. A tab whose page stopped
// responding is reopened; a browser that died or froze is restarted, so nobody
// has to restart the server.
func (b *Browser) watchHealth() {
	const every, limit = 5 * time.Second, 8 * time.Second
	browserFails, tabFails := 0, 0
	var lastTab *Tab
	for range time.Tick(every) {
		b.mu.Lock()
		closed, root, t, loading := b.closed, b.root, b.active, b.active != nil && b.active.loading
		b.mu.Unlock()
		if closed {
			return
		}
		if root.Err() != nil { // Chrome exited or the connection dropped
			b.restart("the browser closed unexpectedly")
			browserFails, tabFails = 0, 0
			continue
		}
		ctx, cancel := context.WithTimeout(root, limit)
		_, err := chromedp.CallBrowser(ctx, browser.GetVersion, cdp.Empty{})
		cancel()
		if err != nil {
			if browserFails++; browserFails >= 2 {
				b.restart("the browser stopped responding")
				browserFails, tabFails = 0, 0
			}
			continue
		}
		browserFails = 0

		if t == nil || t != lastTab {
			lastTab, tabFails = t, 0
		}
		if t == nil || t.ctx.Err() != nil {
			continue
		}
		ctx, cancel = context.WithTimeout(t.ctx, limit)
		_, err = chromedp.Call(ctx, cdpruntime.Evaluate, cdpruntime.EvaluateParams{Expression: "1"})
		cancel()
		// Only a timeout means "hung": errors like "context destroyed" come during normal navigation.
		if err != nil && errors.Is(err, context.DeadlineExceeded) && t.ctx.Err() == nil {
			tabFails++
			if loading { // a slow page load gets more time
				tabFails = min(tabFails, 2)
			}
			if tabFails >= 3 || (tabFails >= 2 && !loading) {
				b.recoverTab(t)
				lastTab, tabFails = nil, 0
			}
			continue
		}
		tabFails = 0
	}
}

// recoverTab replaces a tab whose page froze (endless script, runaway memory…).
func (b *Browser) recoverTab(t *Tab) {
	b.mu.Lock()
	u := t.url
	again := time.Since(b.recovered[u]) < 2*time.Minute
	b.recovered[u] = time.Now()
	b.mu.Unlock()
	log.Println("chrome: tab not responding:", u)
	reopen := !again && (strings.HasPrefix(u, "http://") || strings.HasPrefix(u, "https://"))
	// Open the replacement first: a window whose last tab closes is gone, and
	// headless Chrome then can't open tabs. Tabs are made by the browser process,
	// which still answers while a page is frozen.
	b.mu.Lock()
	last := len(b.tabs) == 1
	b.mu.Unlock()
	var repl *Tab
	if reopen || last {
		var err error
		if repl, err = b.createTab(); err != nil {
			log.Println("chrome: replacement tab:", err)
		}
	}
	t.cancel() // ends every command still waiting on it, and closes the target
	if reopen {
		go b.hub.systemChat("The page stopped responding and was reloaded: " + u)
	} else {
		go b.hub.systemChat("The page stopped responding and was closed: " + u)
	}
	b.enqueue(func() {
		if repl == nil {
			b.closeTab(t.id)
			return
		}
		b.activate(repl)
		b.closeTab(t.id)
		dest := b.opt.StartURL
		if reopen {
			dest = u
		}
		if nu := normalizeURL(dest); nu != "" {
			b.mu.Lock()
			repl.fresh = true
			b.mu.Unlock()
			ctx, cancel := cmdCtx(repl.ctx)
			defer cancel()
			if _, err := chromedp.Call(ctx, page.Navigate, page.NavigateParams{URL: nu}); err != nil {
				log.Println("chrome: reopen tab:", err)
			}
		}
	})
}

// restart kills Chrome and starts a fresh one with the same profile (cookies and logins stay).
func (b *Browser) restart(reason string) {
	if !b.rmu.TryLock() {
		return
	}
	defer b.rmu.Unlock()
	log.Println("chrome: restarting:", reason)
	go b.hub.systemChat("Restarting the browser: " + reason)

	b.mu.Lock()
	if b.closed {
		b.mu.Unlock()
		return
	}
	tabs, root, cancelAlloc := b.tabs, b.root, b.cancelAlloc
	b.tabs, b.active, b.frame, b.gotFrame = nil, nil, nil, false
	clear(b.streams)
	b.mu.Unlock()
	b.notify()

	for _, t := range tabs {
		t.cancel() // unblocks anything still waiting on the old tabs
	}
	done := make(chan struct{})
	go func() { chromedp.Cancel(root); close(done) }()
	select {
	case <-done:
	case <-time.After(3 * time.Second):
	}
	cancelAlloc() // kills the process if it is still there

	for try := 1; ; try++ {
		err := b.launch()
		if err == nil {
			b.hub.systemChat("The browser is back")
			return
		}
		log.Printf("chrome: restart failed (try %d): %v", try, err)
		b.mu.Lock()
		closed := b.closed
		b.mu.Unlock()
		if closed {
			return
		}
		if try == 3 {
			b.hub.systemChat("Could not restart the browser: " + err.Error())
		}
		time.Sleep(min(time.Duration(try)*5*time.Second, time.Minute))
	}
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
			b.runJob(j)
		}
	}
}

// jobLimit is the longest one queued command may hold up the ones behind it.
const jobLimit = 30 * time.Second

// runJob runs one command. Every Chrome call has its own timeout, but if a
// command still hangs, the queue moves on instead of freezing for everyone
// (the hung command finishes or fails on its own later).
func (b *Browser) runJob(j *job) {
	done := make(chan struct{})
	go func() {
		defer close(done)
		if j.kind == jobFunc {
			j.f()
		} else {
			b.dispatchMouse(j.mouse)
		}
	}()
	select {
	case <-done:
	case <-time.After(jobLimit):
		log.Println("chrome: a command took too long; moving on")
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

// runTab serializes work for one stable tab without blocking work on other tabs.
func (b *Browser) runTab(tabID string, f func(t *Tab) error) error {
	b.mu.Lock()
	var t *Tab
	for _, candidate := range b.tabs {
		if candidate.id == tabID {
			t = candidate
			break
		}
	}
	b.mu.Unlock()
	if t == nil {
		return errors.New("tab not found: " + tabID)
	}
	t.lane.Lock()
	defer t.lane.Unlock()
	return f(t)
}

// findTab returns the tab with that id, or the active one when tabID is empty.
func (b *Browser) findTab(tabID string) *Tab {
	b.mu.Lock()
	defer b.mu.Unlock()
	if tabID == "" {
		return b.active
	}
	for _, c := range b.tabs {
		if c.id == tabID {
			return c
		}
	}
	return nil
}

// sessionFree reports whether who may act on the tab now (nobody holds a session, it expired, or who holds it).
// Caller holds t.sessMu.
func (t *Tab) sessionFree(who string) bool {
	return t.owner == "" || t.owner == who || time.Now().After(t.ownerUntil)
}

// claimTab gives who exclusive use of the tab for ttl (a "session": pick a tool, then draw, without another
// agent changing the page state in between). Others' input calls wait until release or expiry.
func (b *Browser) claimTab(tabID, who string, ttl time.Duration) error {
	t := b.findTab(tabID)
	if t == nil {
		return errors.New("tab not found")
	}
	for end := time.Now().Add(60 * time.Second); ; time.Sleep(50 * time.Millisecond) {
		t.sessMu.Lock()
		if t.sessionFree(who) {
			t.owner, t.ownerUntil = who, time.Now().Add(ttl)
			t.sessMu.Unlock()
			return nil
		}
		holder := t.owner
		t.sessMu.Unlock()
		if time.Now().After(end) {
			return fmt.Errorf("the tab is in use by %q; try again later", holder)
		}
	}
}

func (b *Browser) releaseTab(tabID, who string) {
	if t := b.findTab(tabID); t != nil {
		t.sessMu.Lock()
		if t.owner == who {
			t.owner = ""
		}
		t.sessMu.Unlock()
	}
}

// releaseOwner frees every tab locked by this agent or one of its sub-agents (who/…).
func (b *Browser) releaseOwner(who string) {
	b.mu.Lock()
	tabs := slices.Clone(b.tabs)
	b.mu.Unlock()
	for _, t := range tabs {
		t.sessMu.Lock()
		if t.owner == who || strings.HasPrefix(t.owner, who+"/") {
			t.owner = ""
		}
		t.sessMu.Unlock()
	}
}

// lockGesture waits for any other agent's session on the tab, then takes the tab's gesture lock;
// it returns the unlock. Input tools hold it for the whole call.
func (b *Browser) lockGesture(tabID, who string) (func(), error) {
	t := b.findTab(tabID)
	if t == nil {
		return func() {}, nil
	}
	for end := time.Now().Add(60 * time.Second); ; time.Sleep(50 * time.Millisecond) {
		t.sessMu.Lock()
		free, holder := t.sessionFree(who), t.owner
		t.sessMu.Unlock()
		if free {
			t.gesture.Lock()
			t.sessMu.Lock()
			still := t.sessionFree(who)
			t.sessMu.Unlock()
			if still {
				return t.gesture.Unlock, nil
			}
			t.gesture.Unlock() // someone claimed the tab while we waited for the lock
		}
		if time.Now().After(end) {
			return nil, fmt.Errorf("the tab is in use by %q (browser_lock_tab); try again later", holder)
		}
	}
}

// busy reports whether the active tab is still loading.
func (b *Browser) busy() bool {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.active != nil && b.active.loading
}

// busyTab reports whether a specific tab is still loading without changing the active tab.
func (b *Browser) busyTab(tabID string) bool {
	b.mu.Lock()
	defer b.mu.Unlock()
	for _, t := range b.tabs {
		if t.id == tabID {
			return t.loading
		}
	}
	return false
}

func (b *Browser) viewSize() View {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.view
}

// Close shuts Chrome down, so no process is left behind when we exit.
func (b *Browser) Close() {
	b.mu.Lock()
	b.closed = true
	root, cancelAlloc := b.root, b.cancelAlloc
	b.mu.Unlock()
	done := make(chan struct{})
	go func() { chromedp.Cancel(root); close(done) }()
	select {
	case <-done:
	case <-time.After(3 * time.Second):
	}
	cancelAlloc()
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
	ctx, cancel := cmdCtx(b.rootCtx())
	res, err := chromedp.CallBrowser(ctx, target.CreateTarget, target.CreateTargetParams{URL: "about:blank", NewWindow: &nw})
	if err != nil && strings.Contains(err.Error(), "no browser is open") {
		nw = true // the window closed with its last tab: open a new one
		res, err = chromedp.CallBrowser(ctx, target.CreateTarget, target.CreateTargetParams{URL: "about:blank", NewWindow: &nw})
	}
	cancel()
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
		ctx, cancel := cmdCtx(t.ctx)
		_, err = chromedp.Call(ctx, page.Navigate, page.NavigateParams{URL: u})
		cancel()
	}
	return t, err
}

// attach connects to a tab, prepares it (screen size, audio tap) and adds it to the tab strip.
func (b *Browser) attach(id, openerID string, opened bool) (*Tab, error) {
	t := &Tab{id: id, opener: openerID, title: "New Tab", url: "about:blank", obs: newObserver(id)}
	tabCtx, cancel := chromedp.NewContext(b.rootCtx(), chromedp.WithTargetID(target.ID(id)), chromedp.WithListener(b.tabListener(t)))
	t.ctx, t.cancel = tabCtx, cancel
	// Do binds the tab to its context, so bound it with a timer instead of a timeout context.
	timer := time.AfterFunc(cmdTimeout, cancel)
	err := chromedp.Do(tabCtx)
	if !timer.Stop() && err == nil {
		err = errors.New("the tab did not respond")
	}
	if err != nil {
		cancel()
		return nil, err
	}
	ctx, cancelSetup := context.WithTimeout(tabCtx, cmdTimeout) // all setup steps together
	defer cancelSetup()

	b.mu.Lock()
	v, emu := b.view, b.emu
	b.mu.Unlock()
	run := true
	steps := []func() error{
		func() error { // a background tab must still behave as focused, so parallel sub-agents can use it
			_, err := chromedp.Call(ctx, emulation.SetFocusEmulationEnabled, emulation.SetFocusEmulationEnabledParams{Enabled: true})
			return err
		},
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
			_, err := chromedp.Call(ctx, page.AddScriptToEvaluateOnNewDocument, page.AddScriptToEvaluateOnNewDocumentParams{Source: stealthJS, RunImmediately: &run})
			return err
		},
		func() error { // getDisplayMedia gets the shared screen instead of a picker (share.go)
			if _, err := chromedp.Call(ctx, cdpruntime.AddBinding, cdpruntime.AddBindingParams{Name: shareBinding}); err != nil {
				return err
			}
			_, err := chromedp.Call(ctx, page.AddScriptToEvaluateOnNewDocument, page.AddScriptToEvaluateOnNewDocumentParams{Source: shareJS, RunImmediately: &run})
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
	if b.recording() { // a tab opened during a recording is recorded too
		b.recInstall(t)
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
		ctx, cancel := context.WithTimeout(old.ctx, 3*time.Second)
		chromedp.Call(ctx, page.StopScreencast, cdp.Empty{})
		cancel()
	}
	ctx, cancel := cmdCtx(b.rootCtx())
	chromedp.CallBrowser(ctx, target.ActivateTarget, target.ActivateTargetParams{TargetID: target.ID(t.id)})
	cancel()
	b.startScreencast(t, v)
}

func (b *Browser) startScreencast(t *Tab, v View) {
	q := int64(v.Quality)
	// Chrome refuses while a page is between two documents, so retry briefly.
	for try := 0; try < 10; try++ {
		ctx, cancel := context.WithTimeout(t.ctx, 5*time.Second)
		_, err := chromedp.Call(ctx, page.StartScreencast, page.StartScreencastParams{
			Format: page.StartScreencastFormatJpeg, Quality: &q,
			// Allow full device-pixel frames (pages render at up to 3x), so text stays sharp.
			MaxWidth: int64(v.Width) * 3, MaxHeight: int64(v.Height) * 3, EveryNthFrame: 1,
		})
		cancel()
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
		ctx, cancel := cmdCtx(t.ctx)
		shot, err := chromedp.Call(ctx, page.CaptureScreenshot, page.CaptureScreenshotParams{Format: page.CaptureScreenshotFormatJpeg, Quality: &q})
		cancel()
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
				ctx, cancel := cmdCtx(t.ctx)
				chromedp.Call(ctx, page.Reload, page.ReloadParams{})
				cancel()
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
		ctx, cancel := cmdCtx(b.rootCtx())
		chromedp.CallBrowser(ctx, target.CloseTarget, target.CloseTargetParams{TargetID: info.TargetID})
		cancel()
		b.hub.systemChat("Tab limit reached — close a tab first")
		return
	}
	t, err := b.attach(id, string(info.OpenerID), true)
	if err != nil {
		log.Println("chrome: new tab:", err)
		return
	}
	ctx, cancel := cmdCtx(b.rootCtx())
	if res, err := chromedp.CallBrowser(ctx, target.GetTargetInfo, target.GetTargetInfoParams{TargetID: info.TargetID}); err == nil && res.TargetInfo != nil {
		info = res.TargetInfo // the page kept loading while we attached
	}
	cancel()
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
			go func() {
				ctx, cancel := cmdCtx(t.ctx)
				chromedp.Call(ctx, page.ScreencastFrameAck, page.ScreencastFrameAckParams{SessionID: sid})
				cancel()
			}()
			pkt := make([]byte, 1+len(e.Data))
			pkt[0] = pktFrame
			copy(pkt[1:], e.Data)
			b.mu.Lock()
			active := b.active == t
			if active {
				b.frame, b.gotFrame = pkt, true
				b.shareFrameLocked(pkt[1:])
			}
			drift := false
			if active && e.Metadata != nil {
				want := metrics(b.view, b.emu)
				drift = math.Abs(e.Metadata.DeviceWidth-float64(want.Width)) > 1 || math.Abs(e.Metadata.DeviceHeight-float64(want.Height)) > 1
				if drift && time.Since(t.refitAt) > 2*time.Second {
					t.refitAt = time.Now()
				} else {
					drift = false
				}
			}
			b.mu.Unlock()
			if drift {
				b.enqueue(func() { b.refit(t) })
			}
			if active {
				b.hub.broadcastFrame(pkt)
			}

		case *cdpruntime.EventBindingCalled:
			switch e.Name {
			case audioBinding:
				b.onAudio(t, e.Payload)
			case metaBinding:
				b.onMeta(t, e.Payload)
			case shareBinding:
				b.onShare(t, e.Payload)
			case recBinding:
				b.onRec(t, e.Payload)
			}

		case *page.EventNavigatedWithinDocument: // single-page apps change the address without loading
			if string(e.FrameID) == t.id {
				go b.onPageChange(t, e.URL)
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
			go b.onPageChange(t, e.Frame.URL)
			if reset { // a new tab starts at about:blank; going "back" there makes no sense
				go func() {
					ctx, cancel := cmdCtx(t.ctx)
					chromedp.Call(ctx, page.ResetNavigationHistory, cdp.Empty{})
					cancel()
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
			go func() {
				ctx, cancel := cmdCtx(t.ctx)
				chromedp.Call(ctx, page.HandleJavaScriptDialog, page.HandleJavaScriptDialogParams{Accept: true})
				cancel()
			}()
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
	ctx, cancel := cmdCtx(t.ctx)
	h, err := chromedp.Call(ctx, page.GetNavigationHistory, cdp.Empty{})
	cancel()
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
	b.shareAudio(t, rate, parts[2])

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
	v.Quality = min(max(v.Quality, 10), 100)
	b.enqueue(func() {
		b.mu.Lock()
		b.view = v
		emu := b.emu
		tabs := slices.Clone(b.tabs)
		t := b.active
		b.gotFrame = false
		b.mu.Unlock()
		for _, tab := range tabs {
			ctx, cancel := context.WithTimeout(tab.ctx, 5*time.Second)
			chromedp.Call(ctx, emulation.SetDeviceMetricsOverride, metrics(v, emu))
			cancel()
		}
		if t != nil {
			ctx, cancel := context.WithTimeout(t.ctx, 5*time.Second)
			chromedp.Call(ctx, page.StopScreencast, cdp.Empty{})
			cancel()
			b.startScreencast(t, v)
		}
		b.hub.broadcastView(v)
	})
}

// refit restores a tab's screen size after Chrome changed it on its own (the
// frames came out squashed until the size was switched back and forth by hand).
// Setting the same size again is ignored by Chrome, so step off it first.
func (b *Browser) refit(t *Tab) {
	b.mu.Lock()
	v, emu, active := b.view, b.emu, b.active == t
	b.mu.Unlock()
	m := metrics(v, emu)
	nudge := m
	nudge.Height++
	ctx, cancel := context.WithTimeout(t.ctx, 5*time.Second)
	chromedp.Call(ctx, emulation.SetDeviceMetricsOverride, nudge)
	chromedp.Call(ctx, emulation.SetDeviceMetricsOverride, m)
	if active {
		chromedp.Call(ctx, page.StopScreencast, cdp.Empty{})
	}
	cancel()
	if active {
		b.startScreencast(t, v)
	}
}

func (b *Browser) Navigate(raw string) {
	u := normalizeURL(raw)
	if u == "" {
		return
	}
	b.onTab(func(t *Tab) error {
		ctx, cancel := cmdCtx(t.ctx)
		defer cancel()
		_, err := chromedp.Call(ctx, page.Navigate, page.NavigateParams{URL: u})
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
	ctx, cancel := cmdCtx(t.ctx)
	defer cancel()
	h, err := chromedp.Call(ctx, page.GetNavigationHistory, cdp.Empty{})
	if err != nil {
		return false, err
	}
	i := h.CurrentIndex + delta
	if i < 0 || i >= int64(len(h.Entries)) {
		return false, nil
	}
	_, err = chromedp.Call(ctx, page.NavigateToHistoryEntry, page.NavigateToHistoryEntryParams{EntryID: h.Entries[i].ID})
	return true, err
}

func (b *Browser) Reload() {
	b.onTab(func(t *Tab) error {
		ctx, cancel := cmdCtx(t.ctx)
		defer cancel()
		_, err := chromedp.Call(ctx, page.Reload, page.ReloadParams{})
		return err
	})
}

func (b *Browser) Stop() {
	b.onTab(func(t *Tab) error {
		ctx, cancel := cmdCtx(t.ctx)
		defer cancel()
		_, err := chromedp.Call(ctx, page.StopLoading, cdp.Empty{})
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
		ctx, cancel := inputCtx(t)
		defer cancel()
		_, err := chromedp.Call(ctx, dispatchWheel, wheelParams{Type: string(input.DispatchMouseEventTypeMouseWheel), X: x, Y: y, DeltaX: m.DX, DeltaY: m.DY, Modifiers: int64(mods)})
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
	ctx, cancel := inputCtx(t)
	defer cancel()
	_, err := chromedp.Call(ctx, input.DispatchMouseEvent, p)
	return err
}

// inputCtx bounds one input command, so a page that hangs can't stall the queue
// (and every later click and key) forever.
func inputCtx(t *Tab) (context.Context, context.CancelFunc) {
	return context.WithTimeout(t.ctx, 5*time.Second)
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
		ctx, cancel := inputCtx(t)
		defer cancel()
		if wantCopy {
			if s, err := chromedp.Run(ctx, chromedp.Evaluate[string](selectionJS)); err == nil && s != "" {
				onCopy(s)
			}
		}
		_, err := chromedp.Call(ctx, input.DispatchKeyEvent, p)
		return err
	})
}

// Text types a piece of text directly (IME input, paste…).
func (b *Browser) Text(s string) {
	if s == "" {
		return
	}
	b.onTab(func(t *Tab) error {
		ctx, cancel := inputCtx(t)
		defer cancel()
		_, err := chromedp.Call(ctx, input.InsertText, input.InsertTextParams{Text: s})
		return err
	})
}
