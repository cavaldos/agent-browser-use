package main

// Recorded flows: a person presses Record in the viewer and uses the browser as usual. Every
// click, piece of typed text, choice in a list, key and page change becomes a step. Stopping saves
// the flow on disk under a short id (e.g. flow-3fa9c1). The person gives that id to a coding agent,
// which reads the steps with browser_get_flow and replays them with browser_run_flow, or follows
// them itself with changes (other data, a loop over items…).
//
// What a person does is caught inside the page by agent.js (recStart), which reports each action
// through the __abuRec binding while a recording is on. Navigation from the viewer's toolbar
// (address bar, back, tabs…) is recorded by the hub, where those commands arrive.

import (
	"encoding/json"
	"fmt"
	"math/rand"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/chromedp/cdproto/page"
	cdpruntime "github.com/chromedp/cdproto/runtime"
	"github.com/chromedp/chromedp"
)

const (
	recBinding   = "__abuRec"
	maxFlowSteps = 500
	maxRepeat    = 50
)

// FlowTarget describes an element so it can be found again: role + name (as in browser_snapshot),
// then test id, id, CSS path, text, and the point on the screen as a last resort.
type FlowTarget struct {
	Tag         string  `json:"tag,omitempty"`
	Role        string  `json:"role,omitempty"`
	Name        string  `json:"name,omitempty"`
	Nth         int     `json:"nth,omitempty"`   // which of the elements with the same role and name (0 = first)
	Count       int     `json:"count,omitempty"` // how many had that role and name
	TestAttr    string  `json:"testAttr,omitempty"`
	TestID      string  `json:"testId,omitempty"`
	ID          string  `json:"id,omitempty"`
	Sel         string  `json:"sel,omitempty"`
	Text        string  `json:"text,omitempty"`
	Type        string  `json:"type,omitempty"`
	Placeholder string  `json:"placeholder,omitempty"`
	Field       string  `json:"field,omitempty"`
	Href        string  `json:"href,omitempty"`
	X           float64 `json:"x"`
	Y           float64 `json:"y"`
	VW          float64 `json:"vw,omitempty"`
	VH          float64 `json:"vh,omitempty"`
}

// FlowStep is one thing the person did. Kinds: goto, click, dblclick, rclick, fill, select, key,
// drag, back, forward, reload, tab_new, tab_switch, tab_close, and page (the page address changed:
// not an action, the result of the step before it).
type FlowStep struct {
	Kind   string      `json:"kind"`
	At     int64       `json:"at"`            // ms since the recording started
	URL    string      `json:"url,omitempty"` // goto/page/tab_new: the address; others: the page it happened on
	Target *FlowTarget `json:"target,omitempty"`
	To     *FlowTarget `json:"to,omitempty"` // drag: where it was dropped
	Value  string      `json:"value,omitempty"`
	Values []string    `json:"values,omitempty"`
	Secret bool        `json:"secret,omitempty"` // a password: what was typed is not stored
	Index  int         `json:"index,omitempty"`  // tab_switch / tab_close: tab position
}

type Flow struct {
	ID         string     `json:"id"`
	Name       string     `json:"name"`
	Created    time.Time  `json:"created"`
	By         string     `json:"by,omitempty"`
	StartURL   string     `json:"startUrl,omitempty"`
	StartTitle string     `json:"startTitle,omitempty"`
	Duration   int64      `json:"duration"` // ms
	View       View       `json:"view"`
	Steps      []FlowStep `json:"steps"`
}

// FlowInfo is what the viewer gets: the live recording, a saved flow, or a row of the list.
type FlowInfo struct {
	ID       string    `json:"id,omitempty"`
	Name     string    `json:"name,omitempty"`
	On       bool      `json:"on,omitempty"`
	By       *User     `json:"by,omitempty"`
	Count    int       `json:"count"`
	Last     string    `json:"last,omitempty"`
	Since    int64     `json:"since,omitempty"` // recording started (unix ms)
	Created  time.Time `json:"created,omitzero"`
	StartURL string    `json:"startUrl,omitempty"`
	Duration int64     `json:"duration,omitempty"` // ms
	Secrets  int       `json:"secrets,omitempty"`  // password steps: a replay needs them typed in
}

type recording struct {
	flow     *Flow
	by       *Client
	start    time.Time
	stopping bool
}

// ---------- Storage ----------

var flowIDRe = regexp.MustCompile(`^flow-[0-9a-f]{6}$`)

func normFlowID(id string) string {
	return strings.ToLower(strings.TrimSpace(strings.TrimPrefix(strings.TrimSpace(id), "@")))
}

func flowPath(dir, id string) (string, error) {
	id = normFlowID(id)
	if !flowIDRe.MatchString(id) {
		return "", fmt.Errorf("%q is not a flow id (they look like flow-3fa9c1)", id)
	}
	return filepath.Join(dir, id+".json"), nil
}

func newFlowID(dir string) string {
	for {
		id := fmt.Sprintf("flow-%06x", rand.Intn(1<<24))
		if _, err := os.Stat(filepath.Join(dir, id+".json")); os.IsNotExist(err) {
			return id
		}
	}
}

func saveFlow(dir string, f *Flow) error {
	p, err := flowPath(dir, f.ID)
	if err != nil {
		return err
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return err
	}
	data, _ := json.MarshalIndent(f, "", "  ")
	return os.WriteFile(p, data, 0o600)
}

func loadFlow(dir, id string) (*Flow, error) {
	p, err := flowPath(dir, id)
	if err != nil {
		return nil, err
	}
	data, err := os.ReadFile(p)
	if err != nil {
		return nil, fmt.Errorf("no flow called %s (call browser_get_flow without an id to list them)", normFlowID(id))
	}
	var f Flow
	if err := json.Unmarshal(data, &f); err != nil {
		return nil, fmt.Errorf("flow %s is damaged: %v", normFlowID(id), err)
	}
	return &f, nil
}

func (f *Flow) info() FlowInfo {
	n := 0
	for _, s := range f.Steps {
		if s.Secret {
			n++
		}
	}
	return FlowInfo{ID: f.ID, Name: f.Name, Count: len(f.Steps), Created: f.Created, StartURL: f.StartURL, Duration: f.Duration, Secrets: n}
}

func listFlows(dir string) []FlowInfo {
	out := []FlowInfo{}
	ents, _ := os.ReadDir(dir)
	for _, e := range ents {
		id := strings.TrimSuffix(e.Name(), ".json")
		if e.IsDir() || !flowIDRe.MatchString(id) {
			continue
		}
		if f, err := loadFlow(dir, id); err == nil {
			out = append(out, f.info())
		}
	}
	slices.SortFunc(out, func(a, b FlowInfo) int { return b.Created.Compare(a.Created) })
	return out
}

// ---------- Recording in the tabs ----------

// recScript runs in every new document of a tab while a recording is on.
var recScript = agentJS + "\n;__abu.recStart();"

func (b *Browser) recording() bool {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.recOn
}

// SetRecording turns the recorder on or off in every tab (tabs opened later get it in attach).
func (b *Browser) SetRecording(on bool) {
	b.mu.Lock()
	b.recOn = on
	tabs := slices.Clone(b.tabs)
	b.mu.Unlock()
	for _, t := range tabs {
		if on {
			b.recInstall(t)
		} else {
			b.recUninstall(t)
		}
	}
}

func (b *Browser) recInstall(t *Tab) {
	ctx, cancel := cmdCtx(t.ctx)
	defer cancel()
	if _, err := chromedp.Call(ctx, cdpruntime.AddBinding, cdpruntime.AddBindingParams{Name: recBinding}); err != nil {
		return
	}
	res, err := chromedp.Call(ctx, page.AddScriptToEvaluateOnNewDocument, page.AddScriptToEvaluateOnNewDocumentParams{Source: recScript})
	if err != nil {
		return
	}
	b.mu.Lock()
	t.recScript = res.Identifier
	b.mu.Unlock()
	chromedp.Call(ctx, cdpruntime.Evaluate, cdpruntime.EvaluateParams{Expression: recScript}) // the page that is already open
}

func (b *Browser) recUninstall(t *Tab) {
	b.mu.Lock()
	id := t.recScript
	t.recScript = ""
	b.mu.Unlock()
	ctx, cancel := cmdCtx(t.ctx)
	defer cancel()
	// recStop also reports the text still being typed, so nothing is lost when Stop is pressed mid-word.
	chromedp.Call(ctx, cdpruntime.Evaluate, cdpruntime.EvaluateParams{Expression: "window.__abu && window.__abu.recStop && window.__abu.recStop()"})
	if id != "" {
		chromedp.Call(ctx, page.RemoveScriptToEvaluateOnNewDocument, page.RemoveScriptToEvaluateOnNewDocumentParams{Identifier: id})
	}
	chromedp.Call(ctx, cdpruntime.RemoveBinding, cdpruntime.RemoveBindingParams{Name: recBinding})
}

// onRec gets an action from the recorder in a page. Only the tab people see counts.
func (b *Browser) onRec(t *Tab, payload string) {
	b.mu.Lock()
	active, u := b.active == t, t.url
	b.mu.Unlock()
	if active {
		b.hub.recEvent(u, payload)
	}
}

// onPageChange notes that the tab people see is on a new address (a link, a form, a redirect…).
func (b *Browser) onPageChange(t *Tab, u string) {
	b.mu.Lock()
	active, on := b.active == t, b.recOn
	b.mu.Unlock()
	if active && on && strings.HasPrefix(u, "http") {
		b.hub.recPage(u)
	}
}

// ---------- Hub: the recording ----------

// recLive says whether actions count now: a recording is on and a person (not an agent) drives.
// caller holds h.mu
func (h *Hub) recLive() bool {
	return h.rec != nil && h.ctrl != nil && h.ctrl != h.agent
}

// caller holds h.mu
func (h *Hub) recInfo() *FlowInfo {
	r := h.rec
	if r == nil {
		return &FlowInfo{}
	}
	info := &FlowInfo{On: true, By: &r.by.user, Count: len(r.flow.Steps), Since: r.start.UnixMilli()}
	if n := len(r.flow.Steps); n > 0 {
		info.Last = r.flow.Steps[n-1].label()
	}
	return info
}

// caller holds h.mu
func (h *Hub) recStart(c *Client) {
	if h.rec != nil || h.browser == nil {
		return
	}
	tabs, active, _, v := h.browser.State()
	f := &Flow{Created: time.Now(), By: c.user.Name, View: v, Steps: []FlowStep{}}
	for _, t := range tabs {
		if t.ID == active && strings.HasPrefix(t.URL, "http") {
			f.StartURL, f.StartTitle = t.URL, t.Title
		}
	}
	h.rec = &recording{flow: f, by: c, start: time.Now()}
	go h.browser.SetRecording(true)
	h.broadcast(Msg{Type: "rec", Rec: h.recInfo()}, nil)
	h.addSystemChat("⏺ " + c.user.Name + " started recording a flow")
}

// recStop ends the recording and saves it (unless discard). c gets the id; nil when the person left.
// caller holds h.mu
func (h *Hub) recStop(c *Client, discard bool) {
	r := h.rec
	if r == nil || r.stopping {
		return
	}
	r.stopping = true
	go func() {
		h.browser.SetRecording(false)      // reports text still being typed…
		time.Sleep(150 * time.Millisecond) // …which arrives as an event just after
		h.mu.Lock()
		defer h.mu.Unlock()
		h.rec = nil
		h.broadcast(Msg{Type: "rec", Rec: &FlowInfo{}}, nil)
		h.finishRec(r, c, discard)
	}()
}

// caller holds h.mu
func (h *Hub) finishRec(r *recording, c *Client, discard bool) {
	f := r.flow
	if !slices.ContainsFunc(f.Steps, func(s FlowStep) bool { return s.Kind != "page" }) {
		f.Steps = f.Steps[:0] // only page changes, no action: nothing to replay
	}
	reply := func(info *FlowInfo) {
		if c != nil && h.clients[c] {
			b, _ := json.Marshal(Msg{Type: "rec_done", Rec: info})
			trySend(c, outMsg{data: b})
		}
	}
	if discard {
		h.addSystemChat("⏹ The recording was discarded")
		return
	}
	if len(f.Steps) == 0 {
		reply(&FlowInfo{})
		h.addSystemChat("⏹ Recording stopped: no actions were recorded")
		return
	}
	dir := h.browser.opt.FlowDir
	f.ID = newFlowID(dir)
	f.Duration = time.Since(r.start).Milliseconds()
	f.Name = defaultFlowName(f)
	if err := saveFlow(dir, f); err != nil {
		h.addSystemChat("⚠️ Could not save the flow: " + err.Error())
		return
	}
	info := f.info()
	reply(&info)
	h.broadcast(Msg{Type: "flows"}, nil) // every viewer's flow list
	h.addSystemChat(fmt.Sprintf("🎬 Flow %s saved (%d steps): give this id to an agent, it reads it with browser_get_flow and replays it with browser_run_flow.", f.ID, len(f.Steps)))
}

func defaultFlowName(f *Flow) string {
	where := f.StartTitle
	if where == "" {
		for _, s := range f.Steps {
			if s.URL != "" {
				if u, err := url.Parse(s.URL); err == nil {
					where = u.Hostname()
				}
				break
			}
		}
	}
	if r := []rune(where); len(r) > 40 {
		where = string(r[:39]) + "…"
	}
	return strings.TrimPrefix(where+" · "+f.Created.Format("Jan 2 15:04"), " · ")
}

// recEvent takes an action reported by the recorder in the page (the page could call the binding itself, so validate).
func (h *Hub) recEvent(pageURL, payload string) {
	if len(payload) > 64*1024 {
		return
	}
	var e struct {
		K      string      `json:"k"`
		T      *FlowTarget `json:"t"`
		To     *FlowTarget `json:"to"`
		V      string      `json:"v"`
		Vs     []string    `json:"vs"`
		Secret bool        `json:"secret"`
	}
	if json.Unmarshal([]byte(payload), &e) != nil {
		return
	}
	h.mu.Lock()
	defer h.mu.Unlock()
	if !h.recLive() {
		return
	}
	s := FlowStep{Kind: e.K, URL: pageURL, Target: e.T.tidy(), To: e.To.tidy()}
	switch e.K {
	case "click", "rclick":
		if s.Target == nil {
			return
		}
		s.To = nil
	case "drag":
		if s.Target == nil || s.To == nil {
			return
		}
	case "dblclick":
		h.recDouble()
		return
	case "fill":
		if s.Target == nil {
			return
		}
		s.To, s.Secret = nil, e.Secret
		if !s.Secret {
			s.Value = cut(e.V, 5000)
		}
	case "select":
		if s.Target == nil || len(e.Vs) == 0 {
			return
		}
		s.To = nil
		for _, v := range e.Vs[:min(len(e.Vs), 20)] {
			s.Values = append(s.Values, cut(v, 200))
		}
	case "key":
		if _, _, err := parseKey(e.V); err != nil || len(e.V) > 40 {
			return
		}
		s.Target, s.To, s.Value = nil, nil, e.V
	default:
		return
	}
	h.recAdd(s)
}

// recPage notes a new page address after an action.
func (h *Hub) recPage(u string) {
	h.mu.Lock()
	defer h.mu.Unlock()
	if !h.recLive() {
		return
	}
	h.recAdd(FlowStep{Kind: "page", URL: u})
}

// recInput records a viewer toolbar command (address bar, back, tabs…) from the person in control.
// caller holds h.mu
func (h *Hub) recInput(m Msg) {
	if h.rec == nil || h.ctrl == h.agent {
		return
	}
	tabIndex := func(id string) int {
		tabs, _, _, _ := h.browser.State()
		return slices.IndexFunc(tabs, func(t TabInfo) bool { return t.ID == id })
	}
	switch m.Type {
	case "nav":
		if u := normalizeURL(m.URL); u != "" {
			h.recAdd(FlowStep{Kind: "goto", URL: u})
		}
	case "back", "forward", "reload":
		h.recAdd(FlowStep{Kind: m.Type})
	case "tab_new":
		h.recAdd(FlowStep{Kind: "tab_new", URL: normalizeURL(m.URL)})
	case "tab_activate":
		if i := tabIndex(m.ID); i >= 0 {
			h.recAdd(FlowStep{Kind: "tab_switch", Index: i})
		}
	case "tab_close":
		if i := tabIndex(m.ID); i >= 0 {
			h.recAdd(FlowStep{Kind: "tab_close", Index: i})
		}
	}
}

// recDouble turns the two clicks before a double-click into one double-click step.
// caller holds h.mu
func (h *Hub) recDouble() {
	f := h.rec.flow
	n := len(f.Steps)
	if n == 0 || f.Steps[n-1].Kind != "click" {
		return
	}
	if n >= 2 && f.Steps[n-2].Kind == "click" && f.Steps[n-2].Target.same(f.Steps[n-1].Target) {
		f.Steps = f.Steps[:n-1]
		n--
	}
	f.Steps[n-1].Kind = "dblclick"
	h.recChanged()
}

// recAdd appends a step, merging it with the one before when they are really one action.
// caller holds h.mu
func (h *Hub) recAdd(s FlowStep) {
	r := h.rec
	f := r.flow
	s.At = time.Since(r.start).Milliseconds()
	n := len(f.Steps)
	var last *FlowStep
	if n > 0 {
		last = &f.Steps[n-1]
	}
	switch {
	case last != nil && s.Kind == "fill" && last.Kind == "fill" && last.Target.same(s.Target):
		last.Value, last.Secret, last.At = s.Value, s.Secret, s.At // more typing in the same field
	case last != nil && (s.Kind == "fill" || s.Kind == "select") && last.Kind == "click" && last.Target.same(s.Target):
		*last = s // the click only put the focus there; typing into a field clicks it anyway
	case last != nil && s.Kind == "page" && last.Kind == "page":
		last.URL, last.At = s.URL, s.At // a redirect
	case s.Kind == "page" && (last == nil || slices.Contains([]string{"goto", "back", "forward", "reload", "tab_new", "tab_switch"}, last.Kind)):
		return // the obvious result of that step (or where the recording started)
	case s.Kind == "page" && last != nil && sameURL(last.URL, s.URL):
		return // still the same page
	default:
		if n >= maxFlowSteps {
			return
		}
		f.Steps = append(f.Steps, s)
	}
	h.recChanged()
}

// caller holds h.mu
func (h *Hub) recChanged() {
	h.broadcast(Msg{Type: "rec", Rec: h.recInfo()}, nil)
}

func sameURL(a, b string) bool {
	trim := func(s string) string { s, _, _ = strings.Cut(s, "#"); return strings.TrimSuffix(s, "/") }
	return trim(a) == trim(b)
}

func cut(s string, n int) string {
	if r := []rune(s); len(r) > n {
		return string(r[:n])
	}
	return s
}

// tidy bounds what came from the page.
func (t *FlowTarget) tidy() *FlowTarget {
	if t == nil {
		return nil
	}
	for _, p := range []*string{&t.Tag, &t.Role, &t.TestAttr, &t.Type} {
		*p = cut(*p, 40)
	}
	for _, p := range []*string{&t.Name, &t.TestID, &t.ID, &t.Text, &t.Placeholder, &t.Field} {
		*p = cut(*p, 200)
	}
	t.Sel, t.Href = cut(t.Sel, 500), cut(t.Href, 300)
	if t.TestAttr != "" && !slices.Contains([]string{"data-testid", "data-test-id", "data-test", "data-qa", "data-cy"}, t.TestAttr) {
		t.TestAttr, t.TestID = "", ""
	}
	return t
}

func (t *FlowTarget) same(o *FlowTarget) bool {
	if t == nil || o == nil {
		return false
	}
	return (t.Sel != "" && t.Sel == o.Sel) || (t.Name != "" && t.Role == o.Role && t.Name == o.Name)
}

// positional: nothing to recognize the element by but where it was (a canvas, an empty box…).
func (t *FlowTarget) positional() bool {
	return t.Tag == "canvas" || (t.Role == "" && t.Name == "" && t.Text == "" && t.TestID == "" && t.ID == "" && t.Placeholder == "")
}

// ---------- Describing a flow ----------

// what names the element for people and agents: role "name", else tag "text", else a point.
func (t *FlowTarget) what() string {
	if t == nil {
		return "?"
	}
	switch {
	case t.Role != "" && t.Name != "":
		return t.Role + " " + quote(t.Name)
	case t.Text != "":
		return cmpOr(t.Role, cmpOr(t.Tag, "element")) + " " + quote(t.Text)
	case t.Placeholder != "":
		return cmpOr(t.Role, "textbox") + " with placeholder " + quote(t.Placeholder)
	case t.Role != "":
		return t.Role
	case t.Tag != "":
		return fmt.Sprintf("%s at (%.0f, %.0f)", t.Tag, t.X, t.Y)
	}
	return fmt.Sprintf("point (%.0f, %.0f)", t.X, t.Y)
}

// hints are the other ways to find the element, for an agent that follows the flow itself.
func (t *FlowTarget) hints() string {
	if t == nil {
		return ""
	}
	var h []string
	if t.Count > 1 {
		h = append(h, fmt.Sprintf("#%d of %d with this name", t.Nth+1, t.Count))
	}
	if t.TestID != "" {
		h = append(h, fmt.Sprintf("%s=%q", t.TestAttr, t.TestID))
	}
	if t.ID != "" {
		h = append(h, "id="+t.ID)
	}
	if t.Href != "" {
		h = append(h, "href="+t.Href)
	}
	if t.Sel != "" {
		h = append(h, "css: "+t.Sel)
	}
	if t.positional() {
		h = append(h, fmt.Sprintf("point %.0f,%.0f of a %.0f×%.0f page", t.X, t.Y, t.VW, t.VH))
	}
	if len(h) == 0 {
		return ""
	}
	return "  [" + strings.Join(h, "; ") + "]"
}

// label is one short line for the step.
func (s FlowStep) label() string {
	switch s.Kind {
	case "goto":
		return "Open " + s.URL
	case "click":
		return "Click " + s.Target.what()
	case "dblclick":
		return "Double-click " + s.Target.what()
	case "rclick":
		return "Right-click " + s.Target.what()
	case "fill":
		if s.Secret {
			return "Type the password (not recorded) into " + s.Target.what()
		}
		if s.Value == "" {
			return "Clear " + s.Target.what()
		}
		return "Type " + quote(s.Value) + " into " + s.Target.what()
	case "select":
		return "Choose " + quote(strings.Join(s.Values, ", ")) + " in " + s.Target.what()
	case "key":
		return "Press " + s.Value
	case "drag":
		return "Drag " + s.Target.what() + " onto " + s.To.what()
	case "back":
		return "Go back"
	case "forward":
		return "Go forward"
	case "reload":
		return "Reload the page"
	case "tab_new":
		return "Open a new tab" + map[bool]string{true: ": " + s.URL}[s.URL != ""]
	case "tab_switch":
		return fmt.Sprintf("Switch to tab %d", s.Index)
	case "tab_close":
		return fmt.Sprintf("Close tab %d", s.Index)
	case "page":
		return "→ the page becomes " + s.URL
	}
	return s.Kind
}

func (f *Flow) describe() string {
	var s strings.Builder
	fmt.Fprintf(&s, "### Flow %s %s\n", f.ID, quote(f.Name))
	fmt.Fprintf(&s, "Recorded by %s on %s: %d steps in %s.\n", cmpOr(f.By, "someone"), f.Created.Format("2006-01-02 15:04"), len(f.Steps), (time.Duration(f.Duration) * time.Millisecond).Round(time.Second))
	if f.StartURL != "" {
		fmt.Fprintf(&s, "Start page: %s (%s)\n", f.StartURL, f.StartTitle)
	}
	s.WriteString("\nSteps:\n")
	var secret, typed []string
	for i, st := range f.Steps {
		n := strconv.Itoa(i + 1)
		line := n + ". " + st.label()
		if st.Target != nil && st.Kind != "page" {
			line += st.Target.hints()
		}
		if st.Kind == "drag" {
			line += st.To.hints()
		}
		s.WriteString(line + "\n")
		if st.Kind == "fill" || st.Kind == "select" {
			typed = append(typed, n)
			if st.Secret {
				secret = append(secret, n)
			}
		}
	}
	s.WriteString("\nHow to use it:\n")
	fmt.Fprintf(&s, "- Replay it exactly: browser_run_flow with flowId %q (repeat: N runs it N times, each from the start page).\n", f.ID)
	if len(typed) > 0 {
		fmt.Fprintf(&s, "- Change what is typed or chosen with inputs, by step number: e.g. {%q: \"other text\"} (steps with input: %s).\n", typed[0], strings.Join(typed, ", "))
	}
	if len(secret) > 0 {
		fmt.Fprintf(&s, "- Passwords are not recorded: step(s) %s need inputs. Ask the person (browser_say) or reuse a saved login (browser_state).\n", strings.Join(secret, ", "))
	}
	s.WriteString("- Or follow it yourself to adapt it (other data, a loop, a different order): find each element in browser_snapshot by its role and name; the hints in [ ] are fallbacks. Steps with → show where the page should be after the step before.\n")
	return s.String()
}

// ---------- Agent: reading and replaying ----------

func (a *Agent) GetFlow(id string) (string, error) {
	dir := a.b.opt.FlowDir
	if strings.TrimSpace(id) == "" {
		flows := listFlows(dir)
		var s strings.Builder
		s.WriteString("### Recorded flows\n")
		if len(flows) == 0 {
			s.WriteString("None yet. A person records one with the Record button in the viewer and gives you its id.\n")
		}
		for _, f := range flows {
			fmt.Fprintf(&s, "- %s %s: %d steps, recorded %s%s\n", f.ID, quote(f.Name), f.Count, f.Created.Format("2006-01-02 15:04"), map[bool]string{true: ", starts at " + f.StartURL}[f.StartURL != ""])
		}
		return s.String(), nil
	}
	f, err := loadFlow(dir, id)
	if err != nil {
		return "", err
	}
	a.announce("🎬", "Read flow "+f.ID+" "+quote(f.Name), nil)
	return f.describe(), nil
}

type flowFound struct {
	Ref     string `json:"ref"`
	How     string `json:"how"`
	Checked bool   `json:"checked"`
}

// findTarget looks for a recorded element, waiting for it to show up (the page may still be loading).
func (a *Agent) findTarget(t *FlowTarget, wait time.Duration) (*flowFound, error) {
	js, _ := json.Marshal(t)
	end := time.Now().Add(wait)
	for {
		var res *flowFound
		err := a.evalRetry(func(tab *Tab) error {
			var err error
			res, err = evalJS[*flowFound](tab, "__abu.find("+string(js)+")")
			return err
		})
		if err == nil && res != nil && res.Ref != "" {
			return res, nil
		}
		if time.Now().After(end) {
			if err != nil {
				return nil, err
			}
			return nil, fmt.Errorf("could not find %s on the page (%s)", t.what(), a.pageURL())
		}
		time.Sleep(300 * time.Millisecond)
	}
}

// point turns a recorded screen point into a point on the current screen (its size may differ).
func (a *Agent) point(t *FlowTarget) (*float64, *float64) {
	v := a.b.viewSize()
	x, y := t.X, t.Y
	if t.VW > 0 && t.VH > 0 {
		x, y = x*float64(v.Width)/t.VW, y*float64(v.Height)/t.VH
	}
	return &x, &y
}

// RunFlow replays a recorded flow, repeat times. inputs replaces what steps type or choose, by step number.
func (a *Agent) RunFlow(id string, repeat int, inputs map[string]string, skipStart bool) (string, error) {
	f, err := loadFlow(a.b.opt.FlowDir, id)
	if err != nil {
		return "", err
	}
	repeat = min(max(repeat, 1), maxRepeat)
	for k := range inputs {
		if n, err := strconv.Atoi(k); err != nil || n < 1 || n > len(f.Steps) || (f.Steps[n-1].Kind != "fill" && f.Steps[n-1].Kind != "select") {
			return "", fmt.Errorf("inputs: %q is not the number of a step that types or chooses something (see browser_get_flow)", k)
		}
	}
	var missing []string
	for i, s := range f.Steps {
		if s.Secret {
			if _, ok := inputs[strconv.Itoa(i+1)]; !ok {
				missing = append(missing, strconv.Itoa(i+1))
			}
		}
	}
	if len(missing) > 0 {
		return "", fmt.Errorf("step(s) %s type a password, which is not recorded: pass it in inputs, e.g. {%q: \"…\"}. Ask the person for it with browser_say, or load a saved login with browser_state and start the flow after the login steps", strings.Join(missing, ", "), missing[0])
	}

	flowStop.Store(false)
	a.hub.flowRun(f.ID, true)
	defer a.hub.flowRun(f.ID, false)
	a.announce("🎬", fmt.Sprintf("Run flow %s %s (%d steps%s)", f.ID, quote(f.Name), len(f.Steps), map[bool]string{true: fmt.Sprintf(" × %d", repeat)}[repeat > 1]), nil)
	var log strings.Builder
	for run := 1; run <= repeat; run++ {
		if repeat > 1 {
			fmt.Fprintf(&log, "Run %d of %d:\n", run, repeat)
		}
		if !skipStart && f.StartURL != "" && (run > 1 || !sameURL(a.pageURL(), f.StartURL)) {
			if err := a.Navigate(f.StartURL); err != nil {
				return "", fmt.Errorf("%scould not open the start page %s: %v", log.String(), f.StartURL, err)
			}
			fmt.Fprintf(&log, "- opened the start page %s\n", f.StartURL)
		}
		for i, s := range f.Steps {
			if flowStop.Load() {
				return "", fmt.Errorf("%s⏹ stopped by a person before step %d (%s)", log.String(), i+1, s.label())
			}
			in, changed := inputs[strconv.Itoa(i+1)]
			note, err := a.runStep(s, in, changed)
			if changed && !s.Secret { // the log shows what was really typed or chosen
				s.Value, s.Values = in, []string{in}
			}
			if err != nil {
				return "", fmt.Errorf("%s✗ step %d (%s) failed: %v\nThe steps before it were done. Take a snapshot and finish the flow yourself from step %d (browser_get_flow shows the rest), or ask the person", log.String(), i+1, s.label(), err, i+1)
			}
			fmt.Fprintf(&log, "✓ %d. %s%s\n", i+1, s.label(), note)
		}
	}
	return log.String(), nil
}

func (a *Agent) runStep(s FlowStep, input string, hasInput bool) (string, error) {
	const wait = 15 * time.Second
	switch s.Kind {
	case "goto":
		return "", a.Navigate(s.URL)
	case "back":
		return "", a.History(-1)
	case "forward":
		return "", a.History(1)
	case "reload":
		return "", a.Reload()
	case "tab_new":
		_, err := a.Tabs("new", -1, s.URL, "", false)
		return "", err
	case "tab_switch":
		_, err := a.Tabs("select", s.Index, "", "", false)
		return "", err
	case "tab_close":
		_, err := a.Tabs("close", s.Index, "", "", false)
		return "", err
	case "key":
		return "", a.PressKey(s.Value)
	case "page": // wait for the step before to land on that page; a different page is only a warning
		end := time.Now().Add(10 * time.Second)
		for !sameURL(a.pageURL(), s.URL) {
			if time.Now().After(end) {
				return fmt.Sprintf(" (warning: the page is %s)", a.pageURL()), nil
			}
			time.Sleep(200 * time.Millisecond)
		}
		a.settle(5 * time.Second)
		return "", nil
	case "click", "dblclick", "rclick":
		button := map[bool]string{true: "right"}[s.Kind == "rclick"]
		if s.Target.positional() {
			x, y := a.point(s.Target)
			_, err := a.Click("", s.Target.what(), button, s.Kind == "dblclick", x, y)
			return "", err
		}
		el, err := a.findTarget(s.Target, wait)
		if err != nil {
			return "", err
		}
		_, err = a.Click(el.Ref, s.Target.what(), button, s.Kind == "dblclick", nil, nil)
		return found(el), err
	case "fill":
		text := s.Value
		if hasInput {
			text = input
		}
		el, err := a.findTarget(s.Target, wait)
		if err != nil {
			return "", err
		}
		return found(el), a.Type(el.Ref, s.Target.what(), text, false, true)
	case "select":
		values := s.Values
		if hasInput && input != "" {
			values = []string{input}
		}
		el, err := a.findTarget(s.Target, wait)
		if err != nil {
			return "", err
		}
		_, err = a.SelectOption(el.Ref, s.Target.what(), values)
		return found(el), err
	case "drag":
		var fromRef, toRef string
		var sx, sy, ex, ey *float64
		if s.Target.positional() {
			sx, sy = a.point(s.Target)
		} else if el, err := a.findTarget(s.Target, wait); err != nil {
			return "", err
		} else {
			fromRef = el.Ref
		}
		if s.To.positional() {
			ex, ey = a.point(s.To)
		} else if el, err := a.findTarget(s.To, wait); err != nil {
			return "", err
		} else {
			toRef = el.Ref
		}
		return "", a.Drag(fromRef, s.Target.what(), toRef, s.To.what(), sx, sy, ex, ey)
	}
	return "", fmt.Errorf("unknown step kind %q", s.Kind)
}

func found(el *flowFound) string {
	if el.How == "" || el.How == "role and name" {
		return ""
	}
	return " (found by " + el.How + ")"
}
