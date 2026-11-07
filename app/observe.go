package main

// What the page is doing behind the scenes: console messages, page errors and network
// requests of every tab. Chrome reports them as events; they are kept in small ring
// buffers so the agent (MCP tools browser_console_messages / browser_network_requests)
// and the people watching can see why a page misbehaves.

import (
	"encoding/json"
	"fmt"
	"regexp"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/chromedp/cdproto/log"
	"github.com/chromedp/cdproto/network"
	cdpruntime "github.com/chromedp/cdproto/runtime"
)

const (
	maxConsole = 500
	maxNet     = 1000
)

type consoleEntry struct {
	ID    int
	Time  time.Time
	Level string // error, warning, info, debug
	Text  string
	Where string // url:line, when known
}

type netEntry struct {
	ID         int
	reqID      network.RequestID
	Time       time.Time
	Method     string
	URL        string
	Type       string // Document, XHR, Fetch, Script, Image…
	Status     int
	StatusText string
	MIME       string
	Failed     string // network error, e.g. net::ERR_CONNECTION_REFUSED
	Size       int64
	Duration   time.Duration
	Done       bool
	ReqHeaders map[string]any
	ResHeaders map[string]any
	HasBody    bool
	PostData   bool
	FromCache  bool
	told       bool // the agent has been told this request failed
}

func (n *netEntry) isError() bool { return n.Failed != "" || n.Status >= 400 }

// observer holds what one tab reported. Safe for concurrent use; event handlers only take its lock.
type observer struct {
	mu      sync.Mutex
	console []consoleEntry
	net     []*netEntry
	byReq   map[network.RequestID]*netEntry
	nextC   int
	nextN   int

	toldConsole int // console messages up to this id were already reported (see alerts)

	mainFrame    string // the tab's top frame: a document request for it starts a new page
	pageC, pageN int    // entries up to these ids belong to earlier pages
}

func newObserver(mainFrame string) *observer {
	return &observer{byReq: map[network.RequestID]*netEntry{}, mainFrame: mainFrame}
}

func (o *observer) addConsole(level, text, where string) {
	if r := []rune(text); len(r) > 2000 {
		text = string(r[:2000]) + "…"
	}
	o.mu.Lock()
	defer o.mu.Unlock()
	o.nextC++
	o.console = append(o.console, consoleEntry{ID: o.nextC, Time: time.Now(), Level: level, Text: text, Where: where})
	if len(o.console) > maxConsole {
		o.console = append([]consoleEntry(nil), o.console[len(o.console)-maxConsole:]...)
	}
}

func (o *observer) addNet(e *netEntry) {
	o.nextN++
	e.ID = o.nextN
	o.net = append(o.net, e)
	o.byReq[e.reqID] = e
	if len(o.net) > maxNet {
		for _, old := range o.net[:len(o.net)-maxNet] {
			if o.byReq[old.reqID] == old {
				delete(o.byReq, old.reqID)
			}
		}
		o.net = append([]*netEntry(nil), o.net[len(o.net)-maxNet:]...)
	}
}

// remoteText turns a console argument into text.
func remoteText(r *cdpruntime.RemoteObject) string {
	if r == nil {
		return ""
	}
	if len(r.Value) > 0 {
		var s string
		if json.Unmarshal(r.Value, &s) == nil {
			return s
		}
		return string(r.Value)
	}
	if r.UnserializableValue != "" {
		return string(r.UnserializableValue)
	}
	if r.Description != "" {
		return r.Description
	}
	return string(r.Type)
}

func consoleLevel(t cdpruntime.ConsoleAPICalledType) string {
	switch t {
	case cdpruntime.ConsoleAPICalledTypeError, cdpruntime.ConsoleAPICalledTypeAssert:
		return "error"
	case cdpruntime.ConsoleAPICalledTypeWarning:
		return "warning"
	case cdpruntime.ConsoleAPICalledTypeDebug, cdpruntime.ConsoleAPICalledTypeTrace:
		return "debug"
	}
	return "info"
}

func frameWhere(st *cdpruntime.StackTrace) string {
	if st != nil && len(st.CallFrames) > 0 {
		f := st.CallFrames[0]
		return fmt.Sprintf("%s:%d", f.URL, f.LineNumber+1)
	}
	return ""
}

// event feeds one Chrome event of the tab into the buffers. Must return fast.
func (o *observer) event(ev any) {
	switch e := ev.(type) {
	case *cdpruntime.EventConsoleAPICalled:
		switch e.Type {
		case cdpruntime.ConsoleAPICalledTypeClear, cdpruntime.ConsoleAPICalledTypeEndGroup:
			return
		}
		parts := make([]string, 0, len(e.Args))
		for _, a := range e.Args {
			parts = append(parts, remoteText(a))
		}
		o.addConsole(consoleLevel(e.Type), strings.Join(parts, " "), frameWhere(e.StackTrace))

	case *cdpruntime.EventExceptionThrown:
		d := e.ExceptionDetails
		if d == nil {
			return
		}
		text := d.Text
		if d.Exception != nil && d.Exception.Description != "" {
			text, _, _ = strings.Cut(d.Exception.Description, "\n    at ")
		}
		where := d.URL
		if d.URL != "" {
			where = fmt.Sprintf("%s:%d", d.URL, d.LineNumber+1)
		}
		o.addConsole("error", "Uncaught "+text, where)

	case *log.EventEntryAdded:
		en := e.Entry
		if en == nil || en.Source == log.EntrySourceNetwork {
			return // network errors are shown as requests
		}
		level := string(en.Level)
		if level == "verbose" {
			level = "debug"
		}
		where := en.URL
		if en.URL != "" && en.LineNumber > 0 {
			where = fmt.Sprintf("%s:%d", en.URL, en.LineNumber)
		}
		o.addConsole(level, "["+string(en.Source)+"] "+en.Text, where)

	case *network.EventRequestWillBeSent:
		if e.Request == nil {
			return
		}
		o.mu.Lock()
		defer o.mu.Unlock()
		if prev := o.byReq[e.RequestID]; prev != nil && e.RedirectResponse != nil { // a redirect reuses the request id
			prev.Status, prev.StatusText = int(e.RedirectResponse.Status), e.RedirectResponse.StatusText
			prev.ResHeaders = e.RedirectResponse.Headers
			prev.Done = true
			prev.Duration = time.Since(prev.Time)
		}
		if e.Type == network.ResourceTypeDocument && string(e.FrameID) == o.mainFrame && e.RedirectResponse == nil {
			o.pageC, o.pageN = o.nextC, o.nextN // a new page starts: earlier entries are history
		}
		o.addNet(&netEntry{reqID: e.RequestID, Time: time.Now(), Method: e.Request.Method, URL: e.Request.URL,
			Type: string(e.Type), ReqHeaders: e.Request.Headers, PostData: e.Request.HasPostData})

	case *network.EventResponseReceived:
		if e.Response == nil {
			return
		}
		o.mu.Lock()
		defer o.mu.Unlock()
		if n := o.byReq[e.RequestID]; n != nil {
			n.Status, n.StatusText, n.MIME = int(e.Response.Status), e.Response.StatusText, e.Response.MimeType
			n.ResHeaders, n.FromCache = e.Response.Headers, e.Response.FromDiskCache
			if e.Type != "" {
				n.Type = string(e.Type)
			}
		}

	case *network.EventLoadingFinished:
		o.mu.Lock()
		defer o.mu.Unlock()
		if n := o.byReq[e.RequestID]; n != nil {
			n.Done, n.HasBody, n.Size = true, true, int64(e.EncodedDataLength)
			n.Duration = time.Since(n.Time)
		}

	case *network.EventLoadingFailed:
		o.mu.Lock()
		defer o.mu.Unlock()
		if n := o.byReq[e.RequestID]; n != nil {
			n.Done = true
			n.Duration = time.Since(n.Time)
			n.Failed = e.ErrorText
			if e.Canceled && n.Failed == "" {
				n.Failed = "canceled"
			}
			if e.BlockedReason != "" {
				n.Failed += " (blocked: " + string(e.BlockedReason) + ")"
			}
			if e.CorsErrorStatus != nil {
				n.Failed += " (CORS: " + string(e.CorsErrorStatus.CorsError) + ")"
			}
		}
	}
}

// ---------- Reading ----------

var levelRank = map[string]int{"debug": 0, "info": 1, "warning": 2, "error": 3}

func shortURL(u string) string {
	if strings.HasPrefix(u, "data:") && len(u) > 60 {
		return u[:60] + "…"
	}
	if len(u) > 200 {
		return u[:200] + "…"
	}
	return u
}

// consoleReport lists messages of at least the given level ("all" = everything, default "info").
func (o *observer) consoleReport(level string, limit int, clear, history bool) string {
	min := levelRank["info"]
	if r, ok := levelRank[strings.ToLower(level)]; ok {
		min = r
	}
	if strings.EqualFold(level, "all") {
		min = 0
	}
	o.mu.Lock()
	var rows []consoleEntry
	for _, c := range o.console {
		if levelRank[c.Level] >= min && (history || c.ID > o.pageC) {
			rows = append(rows, c)
		}
	}
	total := len(o.console)
	if clear {
		o.console = nil
		o.toldConsole = o.nextC
	}
	o.mu.Unlock()

	if limit <= 0 {
		limit = 100
	}
	cut := 0
	if len(rows) > limit {
		cut = len(rows) - limit
		rows = rows[cut:]
	}
	var s strings.Builder
	fmt.Fprintf(&s, "### Console messages (%d shown, %d kept in total)\n", len(rows), total)
	if !history {
		s.WriteString("(current page only; history: true includes earlier pages)\n")
	}
	if cut > 0 {
		fmt.Fprintf(&s, "(%d older ones left out; use limit to see more)\n", cut)
	}
	if len(rows) == 0 {
		s.WriteString("No messages.\n")
	}
	for _, c := range rows {
		fmt.Fprintf(&s, "- [%s] %s %s", strings.ToUpper(c.Level), c.Time.Format("15:04:05"), c.Text)
		if c.Where != "" {
			fmt.Fprintf(&s, "  (%s)", c.Where)
		}
		s.WriteByte('\n')
	}
	if clear {
		s.WriteString("(cleared)\n")
	}
	return s.String()
}

var staticTypes = map[string]bool{"Image": true, "Font": true, "Stylesheet": true, "Media": true, "Manifest": true, "Ping": true, "Other": false}

type netFilter struct {
	Match      string // regular expression (or plain text) on the URL
	Method     string
	FailedOnly bool
	Static     bool
	History    bool // include requests of earlier pages
	Limit      int
}

func (o *observer) networkReport(f netFilter, clear bool) string {
	var re *regexp.Regexp
	if f.Match != "" {
		var err error
		if re, err = regexp.Compile("(?i)" + f.Match); err != nil {
			re = regexp.MustCompile("(?i)" + regexp.QuoteMeta(f.Match))
		}
	}
	o.mu.Lock()
	var rows []netEntry
	for _, n := range o.net {
		if !f.History && n.ID <= o.pageN {
			continue
		}
		if !f.Static && staticTypes[n.Type] && !n.isError() {
			continue
		}
		if f.FailedOnly && !n.isError() {
			continue
		}
		if f.Method != "" && !strings.EqualFold(n.Method, f.Method) {
			continue
		}
		if re != nil && !re.MatchString(n.URL) {
			continue
		}
		rows = append(rows, *n)
	}
	total := len(o.net)
	if clear {
		o.net, o.byReq = nil, map[network.RequestID]*netEntry{}
	}
	o.mu.Unlock()

	limit := f.Limit
	if limit <= 0 {
		limit = 60
	}
	cut := 0
	if len(rows) > limit {
		cut = len(rows) - limit
		rows = rows[cut:]
	}
	var s strings.Builder
	fmt.Fprintf(&s, "### Network requests (%d shown, %d kept in total)\n", len(rows), total)
	if cut > 0 {
		fmt.Fprintf(&s, "(%d older ones left out; use limit to see more)\n", cut)
	}
	if len(rows) == 0 {
		s.WriteString("No requests")
		if !f.Static {
			s.WriteString(" (images, fonts, stylesheets and media are hidden unless includeStatic is true)")
		}
		s.WriteString(".\n")
	}
	for i := range rows {
		s.WriteString("- " + rows[i].line() + "\n")
	}
	if len(rows) > 0 {
		s.WriteString("\nPass id to see headers (and body with body: true) of one request.\n")
	}
	if clear {
		s.WriteString("(cleared)\n")
	}
	return s.String()
}

func (n *netEntry) line() string {
	st := "pending"
	switch {
	case n.Failed != "":
		st = "FAILED " + n.Failed
	case n.Status > 0:
		st = fmt.Sprintf("%d %s", n.Status, n.StatusText)
		if n.FromCache {
			st += " (cache)"
		}
	}
	extra := ""
	if n.Done && n.Duration > 0 {
		extra += fmt.Sprintf(" %dms", n.Duration.Milliseconds())
	}
	if n.Size > 0 {
		extra += " " + humanSize(n.Size)
	}
	t := n.Type
	if t == "" {
		t = "-"
	}
	return fmt.Sprintf("#%d %s %s → %s [%s]%s", n.ID, n.Method, shortURL(n.URL), strings.TrimSpace(st), t, extra)
}

func humanSize(b int64) string {
	switch {
	case b >= 1<<20:
		return fmt.Sprintf("%.1fMB", float64(b)/(1<<20))
	case b >= 1<<10:
		return fmt.Sprintf("%.1fKB", float64(b)/(1<<10))
	}
	return fmt.Sprintf("%dB", b)
}

// find returns a copy of the request with this id.
func (o *observer) find(id int) (netEntry, bool) {
	o.mu.Lock()
	defer o.mu.Unlock()
	for _, n := range o.net {
		if n.ID == id {
			return *n, true
		}
	}
	return netEntry{}, false
}

var secretHeaders = map[string]bool{"authorization": true, "cookie": true, "set-cookie": true, "proxy-authorization": true, "x-api-key": true}

func writeHeaders(s *strings.Builder, title string, h map[string]any) {
	if len(h) == 0 {
		return
	}
	s.WriteString(title + "\n")
	keys := make([]string, 0, len(h))
	for k := range h {
		keys = append(keys, k)
	}
	slices.Sort(keys)
	for _, k := range keys {
		v := fmt.Sprint(h[k])
		if secretHeaders[strings.ToLower(k)] {
			v = "(hidden)"
		}
		fmt.Fprintf(s, "  %s: %s\n", k, v)
	}
}

func (n netEntry) detail() string {
	var s strings.Builder
	fmt.Fprintf(&s, "### Request #%d\n- %s\n- Type: %s, MIME: %s\n", n.ID, n.line(), cmpOr(n.Type, "-"), cmpOr(n.MIME, "-"))
	if n.PostData {
		s.WriteString("- Has a request body\n")
	}
	writeHeaders(&s, "Request headers:", n.ReqHeaders)
	writeHeaders(&s, "Response headers:", n.ResHeaders)
	return s.String()
}

// alerts lists what went wrong since the last time the agent was told: console errors,
// page exceptions and failed requests. Empty when everything is fine.
func (o *observer) alerts() (string, int) {
	o.mu.Lock()
	defer o.mu.Unlock()
	var lines []string
	for _, c := range o.console {
		if c.ID > o.toldConsole && levelRank[c.Level] >= levelRank["warning"] {
			lines = append(lines, fmt.Sprintf("- console %s: %s", c.Level, oneLine(c.Text, 240)))
		}
	}
	for _, n := range o.net {
		if !n.told && n.isError() && n.Done && !strings.HasSuffix(strings.SplitN(n.URL, "?", 2)[0], "/favicon.ico") {
			n.told = true
			lines = append(lines, "- request "+n.line())
		}
	}
	o.toldConsole = o.nextC
	if len(lines) == 0 {
		return "", 0
	}
	count := len(lines)
	if len(lines) > 8 {
		lines = append(lines[:8], fmt.Sprintf("- … and %d more (see browser_console_messages / browser_network_requests)", len(lines)-8))
	}
	return "### Page problems since the last step\n" + strings.Join(lines, "\n") + "\n", count
}

func oneLine(s string, max int) string {
	s = strings.Join(strings.Fields(s), " ")
	if r := []rune(s); len(r) > max {
		s = string(r[:max-1]) + "…"
	}
	return s
}

// markSeen forgets problems so far, so the next report only covers what comes after
// (used when the agent leaves for another page).
func (o *observer) markSeen() {
	o.mu.Lock()
	defer o.mu.Unlock()
	o.toldConsole = o.nextC
	for _, n := range o.net {
		n.told = true
	}
}
