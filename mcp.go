package main

// MCP (Model Context Protocol) server, so AI agents (Claude Code, Claude Desktop,
// Cursor, VS Code…) can drive the shared browser with tools in the style of
// Playwright MCP: browser_navigate, browser_snapshot, browser_click…
//
// Two transports, same tools:
//   - Streamable HTTP at /mcp on the web server (plain JSON responses).
//   - stdio: `agent-browser-use mcp`, one JSON-RPC message per line. If a server
//     is already running on the port, it just forwards to that one.

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"net/url"
	"os"
	"slices"
	"strings"
	"sync"
	"time"
)

const appName = "agent-browser-use"
const appVersion = "1.0.0"

var protocolVersions = []string{"2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"}

const serverInstructions = `This browser is watched live by people in a web UI: they see your cursor, the element you act on, and a caption for every step.
Work like Playwright MCP: call browser_snapshot to get the page outline, then act on elements by their ref (e.g. ref "e12"). Action tools return a fresh snapshot.
To react to the page (the other player's move in a game, a chat reply, a result appearing), call browser_wait_for_change: it returns as soon as the page changes, with what changed.
Use browser_say to tell the people watching what you are doing or what you need from them (for example, to log in for you).
You take control by yourself when you act, even from a person, and give it back after you have been idle a little while.
If a tool says the agent is blocked, a person clicked "Block agent": wait, or ask them (browser_say) to click "Unblock".`

// ---------- JSON-RPC ----------

type rpcRequest struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id,omitempty"`
	Method  string          `json:"method"`
	Params  json.RawMessage `json:"params,omitempty"`
}

type rpcError struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
}

type rpcResponse struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id"`
	Result  any             `json:"result,omitempty"`
	Error   *rpcError       `json:"error,omitempty"`
}

type content struct {
	Type     string `json:"type"`
	Text     string `json:"text,omitempty"`
	Data     string `json:"data,omitempty"`
	MimeType string `json:"mimeType,omitempty"`
}

type toolResult struct {
	Content []content `json:"content"`
	IsError bool      `json:"isError,omitempty"`
}

func textResult(s string) toolResult { return toolResult{Content: []content{{Type: "text", Text: s}}} }
func errorResult(err error) toolResult {
	return toolResult{Content: []content{{Type: "text", Text: "Error: " + err.Error()}}, IsError: true}
}

type MCPServer struct {
	agent *Agent
}

// handle answers one message; nil for notifications.
func (s *MCPServer) handle(raw []byte) *rpcResponse {
	var req rpcRequest
	if err := json.Unmarshal(raw, &req); err != nil {
		return &rpcResponse{JSONRPC: "2.0", ID: json.RawMessage("null"), Error: &rpcError{-32700, "parse error"}}
	}
	if len(req.ID) == 0 || string(req.ID) == "null" {
		return nil // notification (initialized, cancelled…)
	}
	resp := &rpcResponse{JSONRPC: "2.0", ID: req.ID}
	switch req.Method {
	case "initialize":
		var p struct {
			ProtocolVersion string `json:"protocolVersion"`
		}
		json.Unmarshal(req.Params, &p)
		v := protocolVersions[0]
		if slices.Contains(protocolVersions, p.ProtocolVersion) {
			v = p.ProtocolVersion
		}
		resp.Result = map[string]any{
			"protocolVersion": v,
			"capabilities":    map[string]any{"tools": map[string]any{"listChanged": false}},
			"serverInfo":      map[string]any{"name": appName, "title": "Agent Browser Use", "version": appVersion},
			"instructions":    serverInstructions,
		}
	case "ping":
		resp.Result = map[string]any{}
	case "tools/list":
		resp.Result = map[string]any{"tools": tools}
	case "tools/call":
		var p struct {
			Name      string          `json:"name"`
			Arguments json.RawMessage `json:"arguments"`
		}
		if err := json.Unmarshal(req.Params, &p); err != nil {
			resp.Error = &rpcError{-32602, "invalid params"}
			break
		}
		if !slices.ContainsFunc(tools, func(t tool) bool { return t.Name == p.Name }) {
			resp.Error = &rpcError{-32602, "unknown tool: " + p.Name}
			break
		}
		resp.Result = s.callTool(p.Name, p.Arguments)
	case "resources/list":
		resp.Result = map[string]any{"resources": []any{}}
	case "prompts/list":
		resp.Result = map[string]any{"prompts": []any{}}
	default:
		resp.Error = &rpcError{-32601, "method not found: " + req.Method}
	}
	return resp
}

// ---------- Tools ----------

type tool struct {
	Name        string         `json:"name"`
	Title       string         `json:"title,omitempty"`
	Description string         `json:"description"`
	InputSchema map[string]any `json:"inputSchema"`
	Annotations map[string]any `json:"annotations,omitempty"`
}

func str(desc string) map[string]any { return map[string]any{"type": "string", "description": desc} }

func schema(props map[string]any, required ...string) map[string]any {
	s := map[string]any{"type": "object", "properties": props}
	if len(required) > 0 {
		s["required"] = required
	}
	return s
}

var (
	elementProp = str("Human-readable description of the element (shown to the people watching), e.g. \"Sign in button\"")
	refProp     = str("Exact element ref from the page snapshot, e.g. \"e12\"")
	readOnly    = map[string]any{"readOnlyHint": true}
)

var tools = []tool{
	{Name: "browser_navigate", Title: "Navigate to a URL", Description: "Open a URL in the current tab. Text that is not an address is searched on Google.",
		InputSchema: schema(map[string]any{"url": str("The URL to open")}, "url")},
	{Name: "browser_navigate_back", Title: "Go back", Description: "Go back to the previous page.", InputSchema: schema(map[string]any{})},
	{Name: "browser_navigate_forward", Title: "Go forward", Description: "Go forward to the next page.", InputSchema: schema(map[string]any{})},
	{Name: "browser_reload", Title: "Reload", Description: "Reload the current page.", InputSchema: schema(map[string]any{})},
	{Name: "browser_snapshot", Title: "Page snapshot", Annotations: readOnly,
		Description: "Capture an accessibility outline of the current page, with a ref for each element you can act on. Better than a screenshot for deciding what to do.",
		InputSchema: schema(map[string]any{})},
	{Name: "browser_click", Title: "Click", Description: "Click an element on the page.",
		InputSchema: schema(map[string]any{
			"element":     elementProp,
			"ref":         refProp,
			"doubleClick": map[string]any{"type": "boolean", "description": "Double-click instead of a single click"},
			"button":      map[string]any{"type": "string", "enum": []string{"left", "right", "middle"}, "description": "Mouse button (default left)"},
		}, "element", "ref")},
	{Name: "browser_hover", Title: "Hover", Description: "Move the mouse over an element (opens hover menus, tooltips…).",
		InputSchema: schema(map[string]any{"element": elementProp, "ref": refProp}, "element", "ref")},
	{Name: "browser_type", Title: "Type text", Description: "Click a text field and type into it, key by key. Replaces the current content unless clear is false.",
		InputSchema: schema(map[string]any{
			"element": elementProp,
			"ref":     refProp,
			"text":    str("Text to type"),
			"submit":  map[string]any{"type": "boolean", "description": "Press Enter afterwards"},
			"clear":   map[string]any{"type": "boolean", "description": "Replace the field's content (default true). false appends at the end"},
		}, "element", "ref", "text")},
	{Name: "browser_press_key", Title: "Press a key", Description: "Press a key or shortcut on the focused element, e.g. \"Enter\", \"Escape\", \"ArrowDown\", \"Tab\", \"a\", \"Control+A\", \"Shift+Tab\".",
		InputSchema: schema(map[string]any{"key": str("Key name or combination, e.g. Enter, ArrowLeft, Control+A")}, "key")},
	{Name: "browser_select_option", Title: "Select option", Description: "Select one or more options in a <select> drop-down.",
		InputSchema: schema(map[string]any{
			"element": elementProp,
			"ref":     refProp,
			"values":  map[string]any{"type": "array", "items": map[string]any{"type": "string"}, "description": "Values or visible labels of the options"},
		}, "element", "ref", "values")},
	{Name: "browser_scroll", Title: "Scroll", Description: "Scroll the page (or the element under ref) with the mouse wheel.",
		InputSchema: schema(map[string]any{
			"direction": map[string]any{"type": "string", "enum": []string{"down", "up", "left", "right"}, "description": "Default down"},
			"amount":    map[string]any{"type": "number", "description": "Pixels to scroll (default: most of a screen)"},
			"element":   str("Description of the scrollable element (optional)"),
			"ref":       str("Ref of a scrollable element to scroll inside (optional; default the page)"),
		})},
	{Name: "browser_drag", Title: "Drag and drop", Description: "Drag one element and drop it onto another.",
		InputSchema: schema(map[string]any{
			"startElement": str("Description of the element to drag"),
			"startRef":     str("Ref of the element to drag"),
			"endElement":   str("Description of the drop target"),
			"endRef":       str("Ref of the drop target"),
		}, "startElement", "startRef", "endElement", "endRef")},
	{Name: "browser_wait_for", Title: "Wait", Annotations: readOnly,
		Description: "Wait for text to appear, for text to disappear, or for a number of seconds. With text, time is the timeout (default 30).",
		InputSchema: schema(map[string]any{
			"text":     str("Text to wait for"),
			"textGone": str("Text to wait to disappear"),
			"time":     map[string]any{"type": "number", "description": "Seconds to wait (max 60), or the timeout when waiting for text"},
		})},
	{Name: "browser_wait_for_change", Title: "Wait for the page to change", Annotations: readOnly,
		Description: "Wait until the page changes on its own and return right away with what changed: the other player's move in a game, a chat reply, a search result… " +
			"Clocks and timers ticking don't count. Pass ref or selector to watch only one element (e.g. the game board).",
		InputSchema: schema(map[string]any{
			"ref":      str("Ref of the element to watch (optional)"),
			"selector": str("CSS selector of the element to watch, when it has no ref (optional)"),
			"time":     map[string]any{"type": "number", "description": "Most seconds to wait (default 60, max 300)"},
		})},
	{Name: "browser_take_screenshot", Title: "Screenshot", Annotations: readOnly,
		Description: "Take a screenshot of what is on screen. Use browser_snapshot to act on elements; use this to check how things look.",
		InputSchema: schema(map[string]any{})},
	{Name: "browser_evaluate", Title: "Evaluate JavaScript", Description: "Run a JavaScript function in the page and return its result as JSON.",
		InputSchema: schema(map[string]any{
			"function": str("Function source, e.g. \"() => document.title\" or \"(element) => element.innerText\" when ref is given. May be async"),
			"element":  str("Description of the element passed to the function (optional)"),
			"ref":      str("Ref of an element to pass to the function (optional)"),
		}, "function")},
	{Name: "browser_tabs", Title: "Manage tabs", Description: "List, open, switch to or close tabs. Tabs that pages open (target=_blank links) become the current tab automatically.",
		InputSchema: schema(map[string]any{
			"action": map[string]any{"type": "string", "enum": []string{"list", "new", "select", "close"}},
			"index":  map[string]any{"type": "number", "description": "Tab index for select/close (close defaults to the current tab)"},
			"url":    str("URL for a new tab (optional)"),
		}, "action")},
	{Name: "browser_say", Title: "Tell the people watching", Annotations: readOnly,
		Description: "Show a short message to the people watching the browser (a caption on the screen and a line in the room chat). Use it to explain your plan or to ask them for help, e.g. to log in or solve a captcha.",
		InputSchema: schema(map[string]any{"message": str("What to tell them")}, "message")},
}

type toolArgs struct {
	URL          string   `json:"url"`
	Element      string   `json:"element"`
	Ref          string   `json:"ref"`
	DoubleClick  bool     `json:"doubleClick"`
	Button       string   `json:"button"`
	Text         string   `json:"text"`
	Submit       bool     `json:"submit"`
	Clear        *bool    `json:"clear"`
	Key          string   `json:"key"`
	Values       []string `json:"values"`
	Direction    string   `json:"direction"`
	Amount       float64  `json:"amount"`
	StartElement string   `json:"startElement"`
	StartRef     string   `json:"startRef"`
	EndElement   string   `json:"endElement"`
	EndRef       string   `json:"endRef"`
	TextGone     string   `json:"textGone"`
	Time         float64  `json:"time"`
	Function     string   `json:"function"`
	Action       string   `json:"action"`
	Index        *float64 `json:"index"`
	Message      string   `json:"message"`
	Selector     string   `json:"selector"`
}

func (s *MCPServer) callTool(name string, raw json.RawMessage) toolResult {
	a := s.agent
	if a == nil {
		return errorResult(errors.New("the shared browser is disabled on this server (started with -no-browser)"))
	}
	var args toolArgs
	if len(raw) > 0 && string(raw) != "null" {
		if err := json.Unmarshal(raw, &args); err != nil {
			return errorResult(fmt.Errorf("invalid arguments: %v", err))
		}
	}

	a.mu.Lock()
	defer a.mu.Unlock()
	a.begin()
	defer a.hub.agentDone()

	// withSnapshot runs an action and returns its note plus the page afterwards.
	withSnapshot := func(done string, note string, err error) toolResult {
		if err != nil {
			return errorResult(err)
		}
		snap, serr := a.Changes()
		if serr != nil {
			snap = "(could not read the page: " + serr.Error() + ")"
		}
		out := done + "\n"
		if note != "" {
			out += note + "\n"
		}
		return textResult(out + "\n" + snap)
	}

	switch name {
	case "browser_navigate":
		return withSnapshot("Opened "+args.URL, "", a.Navigate(args.URL))
	case "browser_navigate_back":
		return withSnapshot("Went back", "", a.History(-1))
	case "browser_navigate_forward":
		return withSnapshot("Went forward", "", a.History(1))
	case "browser_reload":
		return withSnapshot("Reloaded", "", a.Reload())
	case "browser_snapshot":
		snap, err := a.Snapshot()
		if err != nil {
			return errorResult(err)
		}
		return textResult(snap)
	case "browser_click":
		note, err := a.Click(args.Ref, args.Element, args.Button, args.DoubleClick)
		return withSnapshot("Clicked "+cmpOr(args.Element, args.Ref), note, err)
	case "browser_hover":
		return withSnapshot("Hovered "+cmpOr(args.Element, args.Ref), "", a.Hover(args.Ref, args.Element))
	case "browser_type":
		clear := args.Clear == nil || *args.Clear
		return withSnapshot("Typed into "+cmpOr(args.Element, args.Ref), "", a.Type(args.Ref, args.Element, args.Text, args.Submit, clear))
	case "browser_press_key":
		return withSnapshot("Pressed "+args.Key, "", a.PressKey(args.Key))
	case "browser_select_option":
		picked, err := a.SelectOption(args.Ref, args.Element, args.Values)
		return withSnapshot("Selected "+strings.Join(picked, ", "), "", err)
	case "browser_scroll":
		return withSnapshot("Scrolled "+cmpOr(args.Direction, "down"), "", a.Scroll(args.Direction, args.Amount, args.Ref, args.Element))
	case "browser_drag":
		return withSnapshot("Dragged "+args.StartElement+" to "+args.EndElement, "", a.Drag(args.StartRef, args.StartElement, args.EndRef, args.EndElement))
	case "browser_wait_for":
		return withSnapshot("Done waiting", "", a.WaitFor(args.Time, args.Text, args.TextGone))
	case "browser_wait_for_change":
		changed, err := a.WaitForChange(args.Time, args.Ref, args.Selector)
		done := "The page changed"
		if !changed {
			done = "Nothing changed yet (timed out). Call browser_wait_for_change again to keep waiting"
		}
		return withSnapshot(done, "", err)
	case "browser_take_screenshot":
		data, err := a.Screenshot()
		if err != nil {
			return errorResult(err)
		}
		return toolResult{Content: []content{{Type: "image", Data: data, MimeType: "image/jpeg"}, {Type: "text", Text: "Screenshot of the current tab."}}}
	case "browser_evaluate":
		out, err := a.Evaluate(args.Function, args.Ref)
		if err != nil {
			return errorResult(err)
		}
		return textResult(out)
	case "browser_tabs":
		index := -1
		if args.Index != nil {
			index = int(*args.Index)
		}
		if (args.Action == "select") && index < 0 {
			return errorResult(errors.New("index is required to select a tab"))
		}
		out, err := a.Tabs(args.Action, index, args.URL)
		if err != nil {
			return errorResult(err)
		}
		if out != "" {
			return textResult(out)
		}
		return withSnapshot("Done", "", nil)
	case "browser_say":
		if strings.TrimSpace(args.Message) == "" {
			return errorResult(errors.New("message is required"))
		}
		a.Say(args.Message)
		return textResult("Shown to the people watching.")
	}
	return errorResult(errors.New("unknown tool " + name))
}

// ---------- Streamable HTTP transport (/mcp) ----------

func isLoopback(addr string) bool {
	host, _, err := net.SplitHostPort(addr)
	if err != nil {
		host = addr
	}
	ip := net.ParseIP(host)
	return ip != nil && ip.IsLoopback()
}

func (s *MCPServer) ServeHTTP(allowRemote bool) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		if !allowRemote && !isLoopback(r.RemoteAddr) {
			http.Error(w, "MCP is only available from this machine (start with -mcp-remote to allow other machines)", http.StatusForbidden)
			return
		}
		// Web pages must not reach the agent (DNS rebinding): only local origins, or none (non-browser clients).
		if o := r.Header.Get("Origin"); o != "" {
			u, err := url.Parse(o)
			if err != nil || (u.Hostname() != "localhost" && !isLoopback(u.Hostname()) && (!allowRemote || u.Host != r.Host)) {
				http.Error(w, "origin not allowed", http.StatusForbidden)
				return
			}
		}
		switch r.Method {
		case http.MethodPost:
		case http.MethodDelete:
			w.WriteHeader(http.StatusOK) // no sessions to end
			return
		default: // no server-initiated stream
			w.Header().Set("Allow", "POST, DELETE")
			http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
			return
		}
		body, err := io.ReadAll(io.LimitReader(r.Body, 4<<20))
		if err != nil {
			http.Error(w, "bad request", http.StatusBadRequest)
			return
		}
		body = bytes.TrimSpace(body)
		var out any
		if len(body) > 0 && body[0] == '[' { // batch (older protocol versions)
			var batch []json.RawMessage
			if json.Unmarshal(body, &batch) != nil {
				http.Error(w, "bad request", http.StatusBadRequest)
				return
			}
			var resps []*rpcResponse
			for _, m := range batch {
				if resp := s.handle(m); resp != nil {
					resps = append(resps, resp)
				}
			}
			if len(resps) > 0 {
				out = resps
			}
		} else if resp := s.handle(body); resp != nil {
			out = resp
		}
		if out == nil {
			w.WriteHeader(http.StatusAccepted)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(out)
	}
}

// ---------- stdio transport ----------

// serveStdio reads one message per line from stdin and answers on stdout until stdin closes.
func (s *MCPServer) serveStdio() {
	var wmu sync.Mutex
	write := func(resp *rpcResponse) {
		if resp == nil {
			return
		}
		b, _ := json.Marshal(resp)
		wmu.Lock()
		os.Stdout.Write(append(b, '\n'))
		wmu.Unlock()
	}
	dispatchLines(os.Stdin, func(line []byte) { write(s.handle(line)) })
}

// dispatchLines calls f for each line of r. Tool calls run one after another in the
// order they came in; everything else (ping, tools/list…) is answered right away.
func dispatchLines(r io.Reader, f func([]byte)) {
	calls := make(chan []byte, 256)
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		for line := range calls {
			f(line)
		}
	}()
	readLines(r, func(line []byte) {
		var m struct {
			Method string `json:"method"`
		}
		json.Unmarshal(line, &m)
		if m.Method == "tools/call" {
			calls <- line
			return
		}
		wg.Add(1)
		go func() { defer wg.Done(); f(line) }()
	})
	close(calls)
	wg.Wait()
}

func readLines(r io.Reader, f func([]byte)) {
	sc := bufio.NewScanner(r)
	sc.Buffer(make([]byte, 64*1024), 16<<20)
	for sc.Scan() {
		if line := bytes.TrimSpace(sc.Bytes()); len(line) > 0 {
			f(slices.Clone(line))
		}
	}
}

// probeMCP reports whether an agent-browser-use server already answers at u.
func probeMCP(u string) bool {
	c := http.Client{Timeout: 2 * time.Second}
	resp, err := c.Post(u, "application/json", strings.NewReader(`{"jsonrpc":"2.0","id":1,"method":"ping"}`))
	if err != nil {
		return false
	}
	defer resp.Body.Close()
	var r rpcResponse
	return resp.StatusCode == http.StatusOK && json.NewDecoder(resp.Body).Decode(&r) == nil && r.Error == nil
}

// proxyStdio forwards stdin messages to a running server and writes its answers to stdout.
func proxyStdio(u string) {
	log.Printf("%s is already running, forwarding MCP to %s", appName, u)
	c := http.Client{Timeout: 10 * time.Minute}
	var wmu sync.Mutex
	dispatchLines(os.Stdin, func(line []byte) {
		req, _ := http.NewRequest(http.MethodPost, u, bytes.NewReader(line))
		req.Header.Set("Content-Type", "application/json")
		req.Header.Set("Accept", "application/json, text/event-stream")
		resp, err := c.Do(req)
		var out []byte
		if err != nil {
			var m rpcRequest
			if json.Unmarshal(line, &m) == nil && len(m.ID) > 0 {
				out, _ = json.Marshal(rpcResponse{JSONRPC: "2.0", ID: m.ID, Error: &rpcError{-32603, "server not reachable: " + err.Error()}})
			}
		} else {
			out, _ = io.ReadAll(resp.Body)
			resp.Body.Close()
			if resp.StatusCode != http.StatusOK {
				out = nil
			}
		}
		if out = bytes.TrimSpace(out); len(out) > 0 {
			wmu.Lock()
			os.Stdout.Write(append(out, '\n'))
			wmu.Unlock()
		}
	})
}
