package main

// The AI agent: drives the shared browser for an MCP client (see mcp.go).
//
// It acts like a person would, so whoever watches the web UI can follow along:
// its own cursor glides to each element, the element is outlined with a caption,
// clicks show a ripple, text is typed key by key, and every step is logged in
// the chat as "AI Agent". A person can click "Take over" at any time; the agent
// then stops and its next step reports that a human is in control.

import (
	"context"
	_ "embed"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"os/exec"
	"regexp"
	"runtime"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/chromedp/cdproto/input"
	"github.com/chromedp/cdproto/page"
	"github.com/chromedp/chromedp"
	"github.com/chromedp/chromedp/kb"
)

//go:embed agent.js
var agentJS string

var agentUser = User{ID: "agent", Name: "AI Agent", Color: "#a78bfa", Agent: true}

type AgentOptions struct {
	Speed      float64       // 0 = no animation (fastest), 1 = normal pace, 2 = twice as slow
	OpenViewer bool          // open the web UI when the agent starts and nobody watches
	ViewerURL  string        //
	Idle       time.Duration // give control back after this long without agent actions
}

type Agent struct {
	hub *Hub
	b   *Browser
	opt AgentOptions

	mu       sync.Mutex // serializes this worker's calls
	tabID    string     // empty means the active tab (legacy behavior)
	workerID string     // the agent's display name (empty: the default agent)
	cursorID string     // id of this agent's cursor on everyone's screen
	x, y     float64    // cursor position, 0..1 of the screen
	opened   bool       // viewer already opened once

	last, lastKey string          // the outline the model saw last, and which tab/URL it was for
	marks         map[string]mark // named baselines for browser_snapshot_diff
	dlSeen        int             // downloads the agent has been told about
	workersMu     sync.Mutex
	workers       map[string]*Agent
}

// worker returns the agent identity for (name, tab): every sub-agent gets its own cursor,
// refs and snapshots, even when several of them use the same tab.
func (a *Agent) worker(name, tabID string) *Agent {
	if name == "" && tabID == "" {
		return a
	}
	key := name + "@" + tabID
	a.workersMu.Lock()
	defer a.workersMu.Unlock()
	if w := a.workers[key]; w != nil {
		return w
	}
	w := &Agent{hub: a.hub, b: a.b, opt: a.opt, tabID: tabID, workerID: cmpOr(name, "agent:"+tabID), cursorID: "agent:" + key, x: 0.5, y: 0.5, workers: a.workers}
	a.workers[key] = w
	return w
}

func newAgent(h *Hub, b *Browser, opt AgentOptions) *Agent {
	a := &Agent{hub: h, b: b, opt: opt, x: 0.5, y: 0.5, workers: map[string]*Agent{}}
	go h.watchAgentIdle(opt.Idle)
	go h.watchSeats()
	return a
}

// ---------- Hub side: presence, control, what viewers see ----------

// agentJoin shows the agent in the room and returns how many people are watching.
func (h *Hub) agentJoin() int {
	h.mu.Lock()
	defer h.mu.Unlock()
	if !h.agentOn {
		h.agentOn = true
	}
	h.agentLast = time.Now()
	h.agentBusy = true
	return len(h.clients)
}

// agentDone marks the end of a tool call: the idle timer starts from here.
func (h *Hub) agentDone() {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.agentLast = time.Now()
	h.agentBusy = false
}

var errBlocked = errors.New("agent blocked")

// humanGrace: the agent won't take control from a person who used the browser this recently.
const humanGrace = 8 * time.Second

// agentTake gives control to the agent, taking it from a person if needed, unless someone blocked the agent.
func (h *Hub) agentTake() error {
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.blocked != nil {
		return fmt.Errorf("%w: %s blocked the agent from using the browser. Wait until they click “Unblock”, or ask them to (browser_say), then try again", errBlocked, h.blocked.Name)
	}
	if h.ctrl != nil && h.ctrl != h.agent && time.Since(h.humanLast) < humanGrace {
		return fmt.Errorf("%w: %s is using the browser right now. Wait a few seconds and try again, or ask them (browser_say)", errBlocked, h.ctrl.user.Name)
	}
	if h.ctrl != h.agent {
		h.agentFrom = h.ctrl // nil when nobody was driving
		if h.ctrl != nil {
			h.addSystemChat("🤖 " + h.agent.user.Name + " took over from " + h.ctrl.user.Name)
		}
		h.setCtrl(h.agent)
	}
	h.agentLast = time.Now()
	return nil
}

// agentRelease gives control back to the person the agent took it from (if still here), or frees it.
// caller holds h.mu
func (h *Hub) agentRelease() {
	back := h.agentFrom
	h.agentFrom = nil
	if back != nil && !h.clients[back] {
		back = nil
	}
	h.setCtrl(back)
}

func (h *Hub) watchAgentIdle(idle time.Duration) {
	if idle <= 0 {
		return
	}
	for range time.Tick(time.Second) {
		h.mu.Lock()
		if h.ctrl == h.agent && !h.agentBusy && time.Since(h.agentLast) > idle {
			h.agentRelease()
		}
		h.mu.Unlock()
	}
}

func (h *Hub) agentSend(m Msg) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.broadcast(m, nil)
}

// agentLog adds a line from the agent to the chat (not saved to disk on its own).
func (h *Hub) agentLog(text string) { h.agentChat("", text) }

// agentChat adds a line to the chat from a coding agent (who = its @handle; empty = the generic "AI Agent").
func (h *Hub) agentChat(who, text string) {
	h.mu.Lock()
	defer h.mu.Unlock()
	cm := ChatMsg{User: h.agent.user.Name, Color: h.agent.user.Color, Text: text, Time: time.Now().UnixMilli(), Agent: true}
	if who != "" {
		cm.User = who
	}
	h.pushChat(cm)
	h.broadcast(Msg{Type: "chat", Msg: &cm}, nil)
}

// ---------- Basics ----------

// begin runs at the start of every tool call.
func (a *Agent) begin() {
	viewers := a.hub.agentJoin()
	if a.opt.OpenViewer && !a.opened && viewers == 0 {
		a.opened = true
		go openInBrowser(a.opt.ViewerURL)
	}
}

func openInBrowser(u string) {
	var cmd *exec.Cmd
	switch runtime.GOOS {
	case "darwin":
		cmd = exec.Command("open", u)
	case "windows":
		cmd = exec.Command("rundll32", "url.dll,FileProtocolHandler", u)
	default:
		cmd = exec.Command("xdg-open", u)
	}
	cmd.Start()
}

// pause waits d scaled by the speed setting, so people can follow.
func (a *Agent) pause(d time.Duration) {
	if a.opt.Speed > 0 {
		time.Sleep(time.Duration(float64(d) * a.opt.Speed))
	}
}

// act runs f on the active tab, as long as the agent may drive.
func (a *Agent) act(f func(t *Tab) error) error {
	if err := a.hub.agentTake(); err != nil {
		return err
	}
	if a.tabID != "" {
		return a.b.runTab(a.tabID, f)
	}
	return a.b.run(f)
}

// look runs f on the active tab without needing control (reading only).
// Reads skip the input queue, so the agent watching or snapshotting the page
// never makes a person's clicks and keys wait.
func (a *Agent) look(f func(t *Tab) error) error {
	if a.tabID != "" {
		return a.b.runTab(a.tabID, f)
	}
	a.b.mu.Lock()
	t := a.b.active
	a.b.mu.Unlock()
	if t == nil {
		return errors.New("no tab is open")
	}
	return f(t)
}

// evalJS runs expr in the page after installing the helpers from agent.js.
func evalJS[T any](t *Tab, expr string) (T, error) {
	ctx, cancel := context.WithTimeout(t.ctx, 30*time.Second) // a hung page must not hang the agent
	defer cancel()
	// Usually the helper is already in the page: skip sending its 20KB source each time.
	v, err := chromedp.Run(ctx, chromedp.Evaluate[T](agentReady+expr, chromedp.EvalAwaitPromise))
	if err != nil && strings.Contains(jsError(err).Error(), agentMissing) {
		v, err = chromedp.Run(ctx, chromedp.Evaluate[T](agentJS+"\n;"+expr, chromedp.EvalAwaitPromise))
	}
	return v, jsError(err)
}

const agentMissing = "__abu_missing__"

// agentReady throws unless the current version of agent.js is loaded in the page.
var agentReady = func() string {
	m := regexp.MustCompile(`const V = (\d+);`).FindStringSubmatch(agentJS)
	if m == nil {
		return "throw new Error('" + agentMissing + "');\n"
	}
	return "if (!(window.__abu && window.__abu.v === " + m[1] + ")) throw new Error('" + agentMissing + "');\n"
}()

// jsError turns a JavaScript exception into its message, without the stack.
func jsError(err error) error {
	var ex *chromedp.ExceptionError
	if errors.As(err, &ex) && ex.Exception != nil && ex.Exception.Description != "" {
		msg, _, _ := strings.Cut(ex.Exception.Description, "\n    at ")
		return errors.New(strings.TrimPrefix(msg, "Error: "))
	}
	return err
}

// evalRetry retries while a page is between two documents ("context destroyed").
func (a *Agent) evalRetry(f func(t *Tab) error) error {
	var err error
	for try := 0; try < 4; try++ {
		if err = a.look(f); err == nil || !strings.Contains(err.Error(), "context") {
			return err
		}
		time.Sleep(400 * time.Millisecond)
	}
	return err
}

// idleFor says how long the page has gone without changing (clocks and timers ticking don't count).
func (a *Agent) idleFor() (time.Duration, error) {
	var ms float64
	err := a.look(func(t *Tab) error {
		var err error
		ms, err = evalJS[float64](t, "__abu.idleFor()")
		return err
	})
	return time.Duration(ms) * time.Millisecond, err
}

// settle waits until the page has reacted to an action: done loading, and still for a moment.
// It returns as soon as that happens, so a quick page costs ~200ms instead of a fixed wait.
func (a *Agent) tabBusy() bool {
	if a.tabID != "" {
		return a.b.busyTab(a.tabID)
	}
	return a.b.busy()
}

func (a *Agent) settle(max time.Duration) {
	const still = 150 * time.Millisecond
	time.Sleep(50 * time.Millisecond) // let a navigation or a new tab start
	end := time.Now().Add(max)
	var giveUp time.Time // some pages never stop animating: don't wait for them forever
	for time.Now().Before(end) {
		if a.tabBusy() {
			giveUp = time.Time{}
			time.Sleep(50 * time.Millisecond)
			continue
		}
		if giveUp.IsZero() {
			giveUp = time.Now().Add(1500 * time.Millisecond)
		} else if time.Now().After(giveUp) {
			return
		}
		if d, err := a.idleFor(); err == nil && d >= still {
			return
		}
		time.Sleep(30 * time.Millisecond)
	}
}

// WaitForChange returns as soon as the page changes on its own (or the element under
// ref / selector does): the other player moved, a reply arrived… Clocks ticking don't count.
// It compares with the outline the model saw last, so a change that came just before the call counts too.
func (a *Agent) WaitForChange(seconds float64, ref, selector string) (bool, error) {
	if seconds <= 0 {
		seconds = 60
	}
	limit := time.Duration(min(seconds, 300) * float64(time.Second))
	if a.last == "" { // nothing to compare with yet
		if _, err := a.Snapshot(); err != nil {
			return false, err
		}
	}
	a.announce("👀", "Wait for the page to change", nil)
	var hit string
	start := fmt.Sprintf("__abu.watchStart(%s, %s, %s)", jsq(a.last), jsq(ref), jsq(selector))
	if err := a.evalRetry(func(t *Tab) error {
		var err error
		hit, err = evalJS[string](t, start)
		return err
	}); err != nil {
		return false, err
	}
	end := time.Now().Add(limit)
	for hit == "" {
		if time.Now().After(end) {
			a.look(func(t *Tab) error { _, err := evalJS[bool](t, "__abu.watchStop()"); return err })
			return false, nil
		}
		time.Sleep(40 * time.Millisecond)
		if a.tabBusy() {
			break // a new page is loading
		}
		err := a.look(func(t *Tab) error {
			var err error
			hit, err = evalJS[string](t, "__abu.watchState()")
			return err
		})
		if err != nil {
			if strings.Contains(err.Error(), "context") {
				break // the page is being replaced
			}
			return false, err
		}
	}
	a.settle(5 * time.Second) // let the change finish (a piece sliding, a list filling…)
	return true, nil
}

func jsq(s string) string {
	b, _ := json.Marshal(s)
	return string(b)
}

// ---------- Showing what the agent does ----------

type box struct {
	X     float64 `json:"x"`
	Y     float64 `json:"y"`
	W     float64 `json:"w"`
	H     float64 `json:"h"`
	CX    float64 `json:"cx"`
	CY    float64 `json:"cy"`
	VW    float64 `json:"vw"`
	VH    float64 `json:"vh"`
	Cover string  `json:"cover"`
	Label string  `json:"label"`
}

// Rect is a box on the shared screen, as fractions of its size.
type Rect struct {
	X float64 `json:"x"`
	Y float64 `json:"y"`
	W float64 `json:"w"`
	H float64 `json:"h"`
}

func (bx box) rect() *Rect {
	if bx.VW <= 0 || bx.VH <= 0 {
		return nil
	}
	return &Rect{bx.X / bx.VW, bx.Y / bx.VH, bx.W / bx.VW, bx.H / bx.VH}
}

func (bx box) center() (float64, float64) {
	if bx.VW <= 0 || bx.VH <= 0 {
		return 0.5, 0.5
	}
	return bx.CX / bx.VW, bx.CY / bx.VH
}

// announce puts a caption (and an outline, if bx is given) on everyone's screen and logs the step in the chat.
func (a *Agent) announce(icon, text string, bx *box) {
	m := Msg{Type: "agent", Act: "step", Text: text, TabID: a.tabID, WorkerID: a.workerID}
	if bx != nil {
		m.Rect = bx.rect()
	}
	a.hub.agentSend(m)
	a.hub.agentLog(icon + " " + text)
}

func quote(s string) string {
	s = strings.TrimSpace(strings.Join(strings.Fields(s), " "))
	if r := []rune(s); len(r) > 80 {
		s = string(r[:79]) + "…"
	}
	return "“" + s + "”"
}

// targetName names the element for captions: the agent's own description if it gave one.
func targetName(element string, bx box) string {
	if strings.TrimSpace(element) != "" {
		return quote(element)
	}
	return bx.Label
}

// locate scrolls the element into view and returns where it is.
func (a *Agent) locate(ref string) (box, error) {
	var bx box
	if ref == "" {
		return bx, errors.New("ref is required: take a snapshot (browser_snapshot) and pass the ref of the element, e.g. \"e12\"")
	}
	err := a.act(func(t *Tab) error {
		var err error
		bx, err = evalJS[box](t, "__abu.locate("+jsq(ref)+")")
		return err
	})
	if err == nil {
		a.pause(80 * time.Millisecond) // let the scroll reach the screen
	}
	return bx, err
}

// ---------- Mouse and keyboard ----------

func (a *Agent) mouse(typ input.DispatchMouseEventType, x, y float64, btn input.MouseButton, buttons, clicks int64) error {
	v := a.b.viewSize()
	p := input.DispatchMouseEventParams{Type: typ, X: x * float64(v.Width), Y: y * float64(v.Height), Button: btn, Buttons: buttons, ClickCount: clicks}
	if btn == "" {
		p.Button = input.MouseButtonNone
	}
	return a.act(func(t *Tab) error {
		ctx, cancel := inputCtx(t)
		defer cancel()
		_, err := chromedp.Call(ctx, input.DispatchMouseEvent, p)
		return err
	})
}

// moveTo glides the cursor to (x, y), sending real mouse moves (hover effects show) and the cursor to viewers.
func (a *Agent) moveTo(x, y float64, held input.MouseButton, buttons int64) error {
	x, y = clamp01(x), clamp01(y)
	v := a.b.viewSize()
	steps := 1
	if a.opt.Speed > 0 {
		d := math.Hypot((x-a.x)*float64(v.Width), (y-a.y)*float64(v.Height))
		steps = int(min(max(d/35, 6), 26))
	}
	if held != "" {
		steps = max(steps, 5) // drag-and-drop code needs a few moves to notice the drag
	}
	sx, sy := a.x, a.y
	for i := 1; i <= steps; i++ {
		k := float64(i) / float64(steps)
		k = k * k * (3 - 2*k) // ease in and out
		cx, cy := sx+(x-sx)*k, sy+(y-sy)*k
		if err := a.mouse(input.DispatchMouseEventTypeMouseMoved, cx, cy, held, buttons, 0); err != nil {
			return err
		}
		a.x, a.y = cx, cy
		a.hub.agentSend(Msg{Type: "cursor", ID: cmpOr(a.cursorID, agentUser.ID), X: cx, Y: cy, Area: "b", TabID: a.tabID, WorkerID: a.workerID})
		if i < steps {
			a.pause(18 * time.Millisecond)
		}
	}
	return nil
}

var mouseButtons = map[string]struct {
	b    input.MouseButton
	mask int64
}{
	"left":   {input.MouseButtonLeft, 1},
	"right":  {input.MouseButtonRight, 2},
	"middle": {input.MouseButtonMiddle, 4},
}

// clickAt presses and releases a button where the cursor is.
func (a *Agent) clickAt(button string, count int) error {
	mb, ok := mouseButtons[button]
	if !ok {
		mb = mouseButtons["left"]
	}
	a.hub.agentSend(Msg{Type: "agent", Act: "click", X: a.x, Y: a.y, TabID: a.tabID, WorkerID: a.workerID})
	for c := 1; c <= count; c++ {
		if err := a.mouse(input.DispatchMouseEventTypeMousePressed, a.x, a.y, mb.b, mb.mask, int64(c)); err != nil {
			return err
		}
		if err := a.mouse(input.DispatchMouseEventTypeMouseReleased, a.x, a.y, mb.b, 0, int64(c)); err != nil {
			return err
		}
	}
	return nil
}

// Keys by name ("Enter", "ArrowDown", "F5"…), built from chromedp's key table.
var keyNames = func() map[string]*kb.Key {
	m := map[string]*kb.Key{}
	for _, k := range kb.Keys {
		if utf8.RuneCountInString(k.Key) > 1 {
			m[strings.ToLower(k.Key)] = k
		}
	}
	alias := map[string]string{"esc": "escape", "return": "enter", "del": "delete", "up": "arrowup", "down": "arrowdown",
		"left": "arrowleft", "right": "arrowright", "pgup": "pageup", "pgdn": "pagedown", "ins": "insert"}
	for a, k := range alias {
		if m[k] != nil {
			m[a] = m[k]
		}
	}
	m["space"] = kb.Keys[' ']
	return m
}()

const modAlt = 1

// parseKey reads "Enter", "a", "Shift+Tab", "Control+A", "Meta+Enter"…
func parseKey(combo string) (*kb.Key, int64, error) {
	parts := strings.Split(combo, "+")
	if strings.HasSuffix(combo, "++") || combo == "+" { // the "+" key itself
		parts = append(parts[:len(parts)-2], "+")
	}
	var mods int64
	for _, p := range parts[:len(parts)-1] {
		switch strings.ToLower(strings.TrimSpace(p)) {
		case "control", "ctrl":
			mods |= modCtrl
		case "alt", "option":
			mods |= modAlt
		case "shift":
			mods |= modShift
		case "meta", "cmd", "command", "super", "win":
			mods |= modMeta
		case "controlormeta", "mod":
			if runtime.GOOS == "darwin" {
				mods |= modMeta
			} else {
				mods |= modCtrl
			}
		default:
			return nil, 0, fmt.Errorf("unknown modifier %q in %q (use Control, Alt, Shift or Meta)", p, combo)
		}
	}
	name := parts[len(parts)-1]
	var k *kb.Key
	if utf8.RuneCountInString(name) == 1 {
		r, _ := utf8.DecodeRuneInString(name)
		if mods&(modCtrl|modMeta|modAlt) != 0 {
			r = []rune(strings.ToLower(name))[0] // shortcuts use the plain key
		}
		k = kb.Keys[r]
	} else {
		k = keyNames[strings.ToLower(strings.TrimSpace(name))]
	}
	if k == nil {
		return nil, 0, fmt.Errorf("unknown key %q (examples: Enter, Tab, Escape, ArrowDown, Backspace, a, Control+A, Shift+Tab)", name)
	}
	if k.Shift {
		mods |= modShift
	}
	return k, int64(fixMods(int(mods))), nil
}

func (a *Agent) pressKey(k *kb.Key, mods int64) error {
	down := input.DispatchKeyEventParams{Key: k.Key, Code: k.Code, Modifiers: mods,
		WindowsVirtualKeyCode: k.Windows, NativeVirtualKeyCode: k.Native}
	if k.Text != "" && mods&(modCtrl|modMeta|modAlt) == 0 {
		down.Type = input.DispatchKeyEventTypeKeyDown
		down.Text, down.UnmodifiedText = k.Text, k.Unmodified
	} else {
		down.Type = input.DispatchKeyEventTypeRawKeyDown
	}
	// Chrome on macOS needs the editing command for Cmd+A/C/X/Z (and Ctrl, which people expect to work the same).
	if runtime.GOOS == "darwin" && mods&(modCtrl|modMeta) != 0 {
		if cmd, ok := macCommands[strings.ToLower(k.Key)]; ok {
			if cmd == "undo" && mods&modShift != 0 {
				cmd = "redo"
			}
			down.Commands = []string{cmd}
		}
	}
	up := input.DispatchKeyEventParams{Type: input.DispatchKeyEventTypeKeyUp, Key: k.Key, Code: k.Code, Modifiers: mods,
		WindowsVirtualKeyCode: k.Windows, NativeVirtualKeyCode: k.Native}
	return a.act(func(t *Tab) error {
		ctx, cancel := inputCtx(t)
		defer cancel()
		if _, err := chromedp.Call(ctx, input.DispatchKeyEvent, down); err != nil {
			return err
		}
		_, err := chromedp.Call(ctx, input.DispatchKeyEvent, up)
		return err
	})
}

// typeText types key by key so viewers see it appear. Characters without a key (é, ộ, emoji…) are inserted as text.
func (a *Agent) typeText(s string) error {
	runes := []rune(s)
	if a.opt.Speed == 0 || len(runes) > 400 {
		return a.act(func(t *Tab) error {
			ctx, cancel := inputCtx(t)
			defer cancel()
			_, err := chromedp.Call(ctx, input.InsertText, input.InsertTextParams{Text: s})
			return err
		})
	}
	for _, r := range runes {
		var err error
		if k := kb.Keys[r]; r < 128 && k != nil && (k.Print || r == '\n') {
			mods := int64(0)
			if k.Shift {
				mods = modShift
			}
			err = a.pressKey(k, mods)
		} else {
			err = a.act(func(t *Tab) error {
				ctx, cancel := inputCtx(t)
				defer cancel()
				_, err := chromedp.Call(ctx, input.InsertText, input.InsertTextParams{Text: string(r)})
				return err
			})
		}
		if err != nil {
			return err
		}
		a.pause(28 * time.Millisecond)
	}
	return nil
}

// ---------- Actions (one per MCP tool) ----------

func (a *Agent) Navigate(raw string) error {
	u := normalizeURL(raw)
	if u == "" {
		return fmt.Errorf("cannot open %q: only http(s) addresses are allowed", raw)
	}
	a.announce("🌐", "Open "+u, nil)
	err := a.act(func(t *Tab) error {
		res, err := chromedp.Call(t.ctx, page.Navigate, page.NavigateParams{URL: u})
		if err == nil && res.ErrorText != "" {
			err = fmt.Errorf("could not open %s: %s", u, res.ErrorText)
		}
		return err
	})
	if err != nil {
		return err
	}
	a.settle(20 * time.Second)
	return nil
}

func (a *Agent) History(delta int64) error {
	word := "back"
	if delta > 0 {
		word = "forward"
	}
	a.announce("↩️", "Go "+word, nil)
	moved := false
	err := a.act(func(t *Tab) error {
		var err error
		moved, err = goHistory(t, delta)
		return err
	})
	if err != nil {
		return err
	}
	if !moved {
		return fmt.Errorf("there is no page to go %s to", word)
	}
	a.settle(15 * time.Second)
	return nil
}

func (a *Agent) Reload() error {
	a.announce("🔄", "Reload the page", nil)
	if err := a.act(func(t *Tab) error {
		_, err := chromedp.Call(t.ctx, page.Reload, page.ReloadParams{})
		return err
	}); err != nil {
		return err
	}
	a.settle(15 * time.Second)
	return nil
}

// Click returns a note when another element sits on top of the target.
func (a *Agent) Click(ref, element, button string, double bool, px, py *float64) (string, error) {
	bx, err := a.locateOrPoint(ref, px, py)
	if err != nil {
		return "", err
	}
	verb := "Click"
	if double {
		verb = "Double-click"
	} else if button == "right" {
		verb = "Right-click"
	}
	a.announce("🖱️", verb+" "+targetName(element, bx), &bx)
	x, y := bx.center()
	if err := a.moveTo(x, y, "", 0); err != nil {
		return "", err
	}
	a.pause(220 * time.Millisecond)
	count := 1
	if double {
		count = 2
	}
	if err := a.clickAt(button, count); err != nil {
		return "", err
	}
	a.settle(8 * time.Second)
	if bx.Cover != "" {
		return fmt.Sprintf("Note: %s was covering the element at the click point, so the click may have hit it instead.", bx.Cover), nil
	}
	return "", nil
}

func (a *Agent) Hover(ref, element string) error {
	bx, err := a.locate(ref)
	if err != nil {
		return err
	}
	a.announce("👆", "Hover "+targetName(element, bx), &bx)
	x, y := bx.center()
	if err := a.moveTo(x, y, "", 0); err != nil {
		return err
	}
	a.settle(time.Second)
	return nil
}

func (a *Agent) Type(ref, element, text string, submit, clear bool) error {
	if ref == "" {
		return a.typeFocused(element, text, submit, clear)
	}
	bx, err := a.locate(ref)
	if err != nil {
		return err
	}
	shown := text
	if hasPassword(element, bx.Label) {
		shown = strings.Repeat("•", min(utf8.RuneCountInString(text), 12))
	}
	a.announce("⌨️", "Type "+quote(shown)+" into "+targetName(element, bx), &bx)
	x, y := bx.center()
	if err := a.moveTo(x, y, "", 0); err != nil {
		return err
	}
	a.pause(150 * time.Millisecond)
	if err := a.clickAt("left", 1); err != nil {
		return err
	}
	hadText := false
	if err := a.act(func(t *Tab) error {
		var err error
		hadText, err = evalJS[bool](t, fmt.Sprintf("__abu.prepareInput(%s, %t)", jsq(ref), clear))
		return err
	}); err != nil {
		return err
	}
	if clear && hadText {
		a.pause(120 * time.Millisecond)
		if err := a.pressKey(keyNames["backspace"], 0); err != nil {
			return err
		}
	}
	if err := a.typeText(text); err != nil {
		return err
	}
	if submit {
		a.pause(200 * time.Millisecond)
		if err := a.pressKey(keyNames["enter"], 0); err != nil {
			return err
		}
	}
	a.settle(8 * time.Second)
	return nil
}

// selectFocusedJS selects the content of the focused field so typing replaces it; says whether it had any.
const selectFocusedJS = `(() => {
  const el = document.activeElement;
  if (!el || el === document.body) return false;
  if (el.tagName === "INPUT" || el.tagName === "TEXTAREA") {
    try { el.select(); } catch (e) { return false; }
    return el.value !== "";
  }
  if (el.isContentEditable) {
    const r = document.createRange(); r.selectNodeContents(el);
    const s = getSelection(); s.removeAllRanges(); s.addRange(r);
    return el.textContent !== "";
  }
  return false;
})()`

// typeFocused types into whatever has focus (after Tab, or a field the snapshot has no ref for).
func (a *Agent) typeFocused(element, text string, submit, clear bool) error {
	shown := text
	if hasPassword(element) {
		shown = strings.Repeat("•", min(utf8.RuneCountInString(text), 12))
	}
	a.announce("⌨️", "Type "+quote(shown)+" into "+cmpOr(element, "the focused field"), nil)
	hadText := false
	if clear {
		if err := a.act(func(t *Tab) error {
			var err error
			hadText, err = evalJS[bool](t, selectFocusedJS)
			return err
		}); err != nil {
			return err
		}
	}
	if hadText {
		if err := a.pressKey(keyNames["backspace"], 0); err != nil {
			return err
		}
	}
	if err := a.typeText(text); err != nil {
		return err
	}
	if submit {
		a.pause(200 * time.Millisecond)
		if err := a.pressKey(keyNames["enter"], 0); err != nil {
			return err
		}
	}
	a.settle(8 * time.Second)
	return nil
}

func hasPassword(s ...string) bool {
	for _, x := range s {
		x = strings.ToLower(x)
		if strings.Contains(x, "password") || strings.Contains(x, "mật khẩu") || strings.Contains(x, "passcode") {
			return true
		}
	}
	return false
}

func (a *Agent) PressKey(combo string) error {
	k, mods, err := parseKey(combo)
	if err != nil {
		return err
	}
	a.announce("⌨️", "Press "+combo, nil)
	a.hub.agentSend(Msg{Type: "agent", Act: "key", Text: combo, TabID: a.tabID, WorkerID: a.workerID})
	if err := a.pressKey(k, mods); err != nil {
		return err
	}
	a.settle(8 * time.Second)
	return nil
}

func (a *Agent) SelectOption(ref, element string, values []string) ([]string, error) {
	if len(values) == 0 {
		return nil, errors.New("values is required: the option values or labels to select")
	}
	bx, err := a.locate(ref)
	if err != nil {
		return nil, err
	}
	a.announce("📋", "Choose "+quote(strings.Join(values, ", "))+" in "+targetName(element, bx), &bx)
	x, y := bx.center()
	if err := a.moveTo(x, y, "", 0); err != nil {
		return nil, err
	}
	a.pause(200 * time.Millisecond)
	a.hub.agentSend(Msg{Type: "agent", Act: "click", X: a.x, Y: a.y, TabID: a.tabID, WorkerID: a.workerID})
	var picked []string
	err = a.act(func(t *Tab) error {
		vals, _ := json.Marshal(values)
		var err error
		picked, err = evalJS[[]string](t, fmt.Sprintf("__abu.selectOption(%s, %s)", jsq(ref), vals))
		return err
	})
	if err != nil {
		return nil, err
	}
	a.settle(5 * time.Second)
	return picked, nil
}

func (a *Agent) Scroll(direction string, amount float64, ref, element string) error {
	v := a.b.viewSize()
	x, y := 0.5, 0.5
	where := "the page"
	if ref != "" {
		bx, err := a.locate(ref)
		if err != nil {
			return err
		}
		x, y = bx.center()
		where = targetName(element, bx)
	}
	dx, dy := 0.0, 0.0
	switch strings.ToLower(direction) {
	case "", "down":
		dy = 1
	case "up":
		dy = -1
	case "right":
		dx = 1
	case "left":
		dx = -1
	default:
		return fmt.Errorf("direction must be up, down, left or right, not %q", direction)
	}
	if amount <= 0 {
		amount = 0.7 * float64(v.Height)
		if dx != 0 {
			amount = 0.7 * float64(v.Width)
		}
	}
	a.announce("🖱️", fmt.Sprintf("Scroll %s %s by %.0fpx", where, strings.ToLower(cmpOr(direction, "down")), amount), nil)
	if err := a.moveTo(x, y, "", 0); err != nil {
		return err
	}
	steps := 1
	if a.opt.Speed > 0 {
		steps = 10
	}
	px, py := a.x*float64(v.Width), a.y*float64(v.Height)
	for i := 0; i < steps; i++ {
		p := wheelParams{Type: string(input.DispatchMouseEventTypeMouseWheel), X: px, Y: py,
			DeltaX: dx * amount / float64(steps), DeltaY: dy * amount / float64(steps)}
		if err := a.act(func(t *Tab) error {
			ctx, cancel := inputCtx(t)
			defer cancel()
			_, err := chromedp.Call(ctx, dispatchWheel, p)
			return err
		}); err != nil {
			return err
		}
		a.pause(30 * time.Millisecond)
	}
	a.settle(2 * time.Second)
	return nil
}

func cmpOr(s, def string) string {
	if s == "" {
		return def
	}
	return s
}

// locateOrPoint finds an element by ref, or a bare point given in page pixels (for canvases).
func (a *Agent) locateOrPoint(ref string, px, py *float64) (box, error) {
	if ref == "" && px != nil && py != nil {
		v := a.b.viewSize()
		if v.Width <= 0 || v.Height <= 0 {
			return box{}, errors.New("the page has no size yet")
		}
		return box{X: *px, Y: *py, CX: *px, CY: *py, VW: float64(v.Width), VH: float64(v.Height), Label: fmt.Sprintf("point (%.0f, %.0f)", *px, *py)}, nil
	}
	return a.locate(ref)
}

func (a *Agent) Drag(startRef, startEl, endRef, endEl string, sx, sy, ex, ey *float64) error {
	from, err := a.locateOrPoint(startRef, sx, sy)
	if err != nil {
		return err
	}
	a.announce("✋", "Drag "+targetName(startEl, from)+" to "+cmpOr(quote(endEl), endRef), &from)
	x, y := from.center()
	if err := a.moveTo(x, y, "", 0); err != nil {
		return err
	}
	a.pause(150 * time.Millisecond)
	if err := a.mouse(input.DispatchMouseEventTypeMousePressed, a.x, a.y, input.MouseButtonLeft, 1, 1); err != nil {
		return err
	}
	to, err := a.locateOrPoint(endRef, ex, ey)
	if err != nil {
		a.mouse(input.DispatchMouseEventTypeMouseReleased, a.x, a.y, input.MouseButtonLeft, 0, 1)
		return err
	}
	a.hub.agentSend(Msg{Type: "agent", Act: "step", Text: "Drop on " + targetName(endEl, to), Rect: to.rect()})
	x, y = to.center()
	if err := a.moveTo(x, y, input.MouseButtonLeft, 1); err != nil {
		return err
	}
	a.pause(150 * time.Millisecond)
	if err := a.mouse(input.DispatchMouseEventTypeMouseReleased, a.x, a.y, input.MouseButtonLeft, 0, 1); err != nil {
		return err
	}
	a.settle(3 * time.Second)
	return nil
}

// WaitFor waits for a time, for text to appear, or for text to go away.
func (a *Agent) WaitFor(seconds float64, text, gone string) error {
	if seconds <= 0 && text == "" && gone == "" {
		return errors.New("give time (seconds), text or textGone")
	}
	if text == "" && gone == "" {
		seconds = min(seconds, 60)
		a.announce("⏳", fmt.Sprintf("Wait %.1fs", seconds), nil)
		time.Sleep(time.Duration(seconds * float64(time.Second)))
		return nil
	}
	limit := 30 * time.Second
	if seconds > 0 {
		limit = time.Duration(min(seconds, 120) * float64(time.Second))
	}
	what, want := text, true
	if text == "" {
		what, want = gone, false
	}
	if want {
		a.announce("⏳", "Wait for "+quote(what), nil)
	} else {
		a.announce("⏳", "Wait until "+quote(what)+" is gone", nil)
	}
	end := time.Now().Add(limit)
	for {
		var has bool
		err := a.look(func(t *Tab) error {
			var err error
			has, err = evalJS[bool](t, "__abu.hasText("+jsq(what)+")")
			return err
		})
		if err == nil && has == want {
			return nil
		}
		if time.Now().After(end) {
			if want {
				return fmt.Errorf("text %s did not appear within %s", quote(what), limit)
			}
			return fmt.Errorf("text %s was still there after %s", quote(what), limit)
		}
		time.Sleep(300 * time.Millisecond)
	}
}

func (a *Agent) Screenshot() (string, error) {
	q := int64(80)
	var data []byte
	err := a.look(func(t *Tab) error {
		shot, err := chromedp.Call(t.ctx, page.CaptureScreenshot, page.CaptureScreenshotParams{Format: page.CaptureScreenshotFormatJpeg, Quality: &q})
		if err == nil {
			data = shot.Data
		}
		return err
	})
	return base64.StdEncoding.EncodeToString(data), err
}

func (a *Agent) Evaluate(fn, ref string) (string, error) {
	if strings.TrimSpace(fn) == "" {
		return "", errors.New("function is required, e.g. \"() => document.title\"")
	}
	a.announce("⚙️", "Run JavaScript on the page", nil)
	arg := "undefined"
	if ref != "" {
		arg = "__abu.get(" + jsq(ref) + ")"
	}
	expr := `(async () => {
  const __f = (` + fn + `);
  const r = await (typeof __f === "function" ? __f(` + arg + `) : __f);
  if (r === undefined) return "undefined";
  if (r instanceof Node) return (r.outerHTML || r.textContent || "").slice(0, 20000);
  try { return JSON.stringify(r, null, 2) ?? String(r); } catch (e) { return String(r); }
})()`
	var out string
	err := a.act(func(t *Tab) error {
		var err error
		out, err = evalJS[string](t, expr)
		return err
	})
	if len(out) > 50000 {
		out = out[:50000] + "\n… (cut)"
	}
	return out, err
}

func (a *Agent) Say(text, who string) {
	a.hub.agentSend(Msg{Type: "agent", Act: "say", Text: text, TabID: a.tabID, WorkerID: a.workerID})
	a.hub.agentChat(who, text)
}

// Tabs lists, opens, selects or closes tabs.
func (a *Agent) Tabs(action string, index int, rawURL, tabID string, background bool) (string, error) {
	tabs, _, _, _ := a.b.State()
	pick := func() (TabInfo, error) {
		if index < 0 || index >= len(tabs) {
			return TabInfo{}, fmt.Errorf("no tab with index %d (there are %d tabs, numbered from 0)", index, len(tabs))
		}
		return tabs[index], nil
	}
	switch action {
	case "", "list":
		return a.tabList(), nil
	case "new":
		u := cmpOr(rawURL, a.b.opt.StartURL)
		_, prior, _, _ := a.b.State()
		newID := ""
		a.announce("➕", "Open a new tab: "+u, nil)
		if err := a.hub.agentTake(); err != nil {
			return "", err
		}
		err := a.b.call(func() error {
			a.b.mu.Lock()
			full := len(a.b.tabs) >= a.b.opt.MaxTabs
			a.b.mu.Unlock()
			if full {
				return fmt.Errorf("tab limit (%d) reached, close a tab first", a.b.opt.MaxTabs)
			}
			t, err := a.b.openTab(u)
			if t != nil {
				newID = t.id
			}
			if background && prior != "" {
				a.b.ActivateTab(prior) // sub-agents work in their own tab; the viewer keeps its tab
			}
			return err
		})
		if err != nil {
			return "", err
		}
		if newID != "" {
			a.worker("", newID).settle(20 * time.Second)
		} else {
			a.settle(20 * time.Second)
		}
		if background {
			return fmt.Sprintf("Opened tab tabId=%s. Pass this tabId to every tool call of the sub-agent that owns it.\n\n%s", newID, a.tabList()), nil
		}
	case "select":
		if tabID != "" {
			for i, candidate := range tabs {
				if candidate.ID == tabID {
					index = i
					break
				}
			}
		}
		t, err := pick()
		if err != nil {
			return "", err
		}
		a.announce("🗂️", "Switch to tab "+quote(t.Title), nil)
		if err := a.hub.agentTake(); err != nil {
			return "", err
		}
		a.b.ActivateTab(t.ID)
		a.settle(2 * time.Second)
	case "close":
		if tabID != "" {
			for i, candidate := range tabs {
				if candidate.ID == tabID {
					index = i
					break
				}
			}
		}
		if index < 0 { // the current one
			_, active, _, _ := a.b.State()
			for i, t := range tabs {
				if t.ID == active {
					index = i
				}
			}
		}
		t, err := pick()
		if err != nil {
			return "", err
		}
		a.announce("✖️", "Close tab "+quote(t.Title), nil)
		if err := a.hub.agentTake(); err != nil {
			return "", err
		}
		a.b.call(func() error { a.b.closeTab(t.ID); return nil })
		a.workersMu.Lock()
		for k := range a.workers {
			if strings.HasSuffix(k, "@"+t.ID) {
				delete(a.workers, k)
			}
		}
		a.workersMu.Unlock()
		a.settle(2 * time.Second)
	default:
		return "", fmt.Errorf("action must be list, new, select or close, not %q", action)
	}
	return "", nil
}

func (a *Agent) tabList() string {
	tabs, active, _, _ := a.b.State()
	var s strings.Builder
	s.WriteString("### Open tabs\n")
	for i, t := range tabs {
		cur := ""
		if t.ID == active {
			cur = " (current)"
		}
		fmt.Fprintf(&s, "- %d:%s [%s](%s) tabId=%s\n", i, cur, t.Title, t.URL, t.ID)
	}
	return s.String()
}

// Snapshot describes the current page for the model: address, title, tabs, and the outline with refs.
func (a *Agent) Snapshot() (string, error) { return a.snapshot(false) }

// Changes describes the page after an action. On the same page it lists only what changed
// since the last snapshot, so the model has much less to read; a new page gets the full outline.
func (a *Agent) Changes() (string, error) { return a.snapshot(true) }

func (a *Agent) snapshot(diff bool) (string, error) {
	var snap string
	err := a.evalRetry(func(t *Tab) error {
		var err error
		snap, err = evalJS[string](t, "__abu.snapshot()")
		return err
	})
	if err != nil {
		return "", err
	}
	tabs, active, _, _ := a.b.State()
	if a.tabID != "" {
		active = a.tabID
	}
	var s strings.Builder
	key := ""
	s.WriteString("### Page state\n")
	for _, t := range tabs {
		if t.ID == active {
			fmt.Fprintf(&s, "- Page URL: %s\n- Page Title: %s\n", t.URL, t.Title)
			key = t.ID + " " + t.URL
		}
	}
	if len(tabs) > 1 {
		s.WriteString(a.tabList())
	}
	prev, prevKey := a.last, a.lastKey
	a.last, a.lastKey = snap, key
	if diff && prev != "" && key == prevKey {
		if d, ok := diffOutline(prev, snap, false); ok {
			s.WriteString(d)
			return s.String(), nil
		}
	}
	s.WriteString("### Page snapshot\n```yaml\n")
	s.WriteString(snap)
	s.WriteString("\n```\n")
	return s.String(), nil
}

// diffOutline lists what changed between two page outlines, with a little context around each change.
// ok is false when so much changed that the full outline is clearer (unless force: an explicit diff is always shown).
func diffOutline(prev, cur string, force bool) (string, bool) {
	ops, ok := diffLines(strings.Split(prev, "\n"), strings.Split(cur, "\n"))
	if !ok {
		return "", false
	}
	const ctx = 2
	keep := make([]bool, len(ops))
	changed := 0
	for i, o := range ops {
		if o.kind != ' ' {
			changed++
			for j := max(0, i-ctx); j <= min(len(ops)-1, i+ctx); j++ {
				keep[j] = true
			}
		}
	}
	if changed == 0 {
		return "### Page snapshot\nNothing changed on the page since the last snapshot.\n", true
	}
	var s strings.Builder
	s.WriteString("### Page changes\nOnly what changed since the earlier snapshot (+ added, - removed, the rest is context). " +
		"Refs from earlier snapshots still work; call browser_snapshot for the whole page.\n```diff\n")
	gap := false
	for i, o := range ops {
		if !keep[i] {
			gap = true
			continue
		}
		if gap {
			s.WriteString("  …\n")
			gap = false
		}
		s.WriteByte(o.kind)
		s.WriteByte(' ')
		s.WriteString(o.text)
		s.WriteByte('\n')
	}
	if gap {
		s.WriteString("  …\n")
	}
	s.WriteString("```\n")
	if !force && s.Len() > len(cur)*2/3 {
		return "", false
	}
	return s.String(), true
}

type lineOp struct {
	kind byte // ' ' same, '-' removed, '+' added
	text string
}

// diffLines is a line diff (longest common subsequence). ok is false when the changed part is too big to compare.
func diffLines(a, b []string) ([]lineOp, bool) {
	p := 0
	for p < len(a) && p < len(b) && a[p] == b[p] {
		p++
	}
	q := 0
	for q < len(a)-p && q < len(b)-p && a[len(a)-1-q] == b[len(b)-1-q] {
		q++
	}
	am, bm := a[p:len(a)-q], b[p:len(b)-q]
	n, m := len(am), len(bm)
	if n*m > 1_000_000 {
		return nil, false
	}
	ops := make([]lineOp, 0, len(a)+len(b))
	for _, l := range a[:p] {
		ops = append(ops, lineOp{' ', l})
	}
	w := m + 1
	lcs := make([]int32, (n+1)*w) // lcs[i*w+j]: common lines of am[i:] and bm[j:]
	for i := n - 1; i >= 0; i-- {
		for j := m - 1; j >= 0; j-- {
			if am[i] == bm[j] {
				lcs[i*w+j] = lcs[(i+1)*w+j+1] + 1
			} else {
				lcs[i*w+j] = max(lcs[(i+1)*w+j], lcs[i*w+j+1])
			}
		}
	}
	for i, j := 0, 0; i < n || j < m; {
		switch {
		case i < n && j < m && am[i] == bm[j]:
			ops = append(ops, lineOp{' ', am[i]})
			i++
			j++
		case i < n && (j == m || lcs[(i+1)*w+j] >= lcs[i*w+j+1]):
			ops = append(ops, lineOp{'-', am[i]})
			i++
		default:
			ops = append(ops, lineOp{'+', bm[j]})
			j++
		}
	}
	for _, l := range a[len(a)-q:] {
		ops = append(ops, lineOp{' ', l})
	}
	return ops, true
}
