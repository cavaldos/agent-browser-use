package main

// HTTP API behind the viewer's Flows tab: list, read, rename, edit, delete, replay, export and
// import recorded flows (see flow.go). Every change is announced to all viewers with a "flows"
// message, so their lists stay current; a replay is announced with "flow_run".

import (
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"os"
	"slices"
	"strconv"
	"strings"
	"sync/atomic"
	"time"
)

var (
	flowRunning atomic.Bool // one replay from the viewer at a time
	flowStop    atomic.Bool // a person asked the running replay to stop (checked between steps)
)

// flowRun tells every viewer that a flow replay started or ended (they show it, with a Stop button).
func (h *Hub) flowRun(id string, on bool) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.runningFlow = map[bool]string{true: id}[on]
	h.broadcast(Msg{Type: "flow_run", Rec: &FlowInfo{ID: id, On: on}}, nil)
}

// flowsChanged tells every viewer to reload its flow list.
func (h *Hub) flowsChanged() {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.broadcast(Msg{Type: "flows"}, nil)
}

// flowStepView is a step as the Flows tab shows it.
type flowStepView struct {
	FlowStep
	Label string `json:"label"`
	Hints string `json:"hints,omitempty"`
}

// validFlow checks a flow that came from outside (an import), and bounds what it holds.
func validFlow(f *Flow) error {
	kinds := []string{"goto", "click", "dblclick", "rclick", "fill", "select", "key", "drag", "back", "forward", "reload", "tab_new", "tab_switch", "tab_close", "page"}
	if len(f.Steps) == 0 {
		return errors.New("the flow has no steps")
	}
	if len(f.Steps) > maxFlowSteps {
		return errors.New("the flow has too many steps")
	}
	for i := range f.Steps {
		s := &f.Steps[i]
		if !slices.Contains(kinds, s.Kind) {
			return errors.New("unknown step kind " + strconv.Quote(s.Kind))
		}
		s.Target, s.To = s.Target.tidy(), s.To.tidy()
		needs := map[string]bool{"click": true, "dblclick": true, "rclick": true, "fill": true, "select": true, "drag": true}
		if needs[s.Kind] && s.Target == nil || s.Kind == "drag" && s.To == nil {
			return errors.New("step " + strconv.Itoa(i+1) + " has no element")
		}
		if s.Kind == "key" {
			if _, _, err := parseKey(s.Value); err != nil {
				return err
			}
		}
		if (s.Kind == "goto" || s.Kind == "page") && normalizeURL(s.URL) == "" {
			return errors.New("step " + strconv.Itoa(i+1) + " has no valid address")
		}
		s.Value, s.URL = cut(s.Value, 5000), cut(s.URL, 2000)
		if s.Secret {
			s.Value = ""
		}
	}
	f.Name = strings.TrimSpace(cut(f.Name, 80))
	return nil
}

func registerFlowAPI(mux *http.ServeMux, h *Hub, mcp *MCPServer) {
	dir := func(w http.ResponseWriter) string {
		if h.browser == nil {
			http.Error(w, "no browser is running", http.StatusServiceUnavailable)
			return ""
		}
		return h.browser.opt.FlowDir
	}
	writeJSON := func(w http.ResponseWriter, v any) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(v)
	}
	post := func(w http.ResponseWriter, r *http.Request) bool {
		if r.Method != http.MethodPost {
			http.Error(w, "POST only", http.StatusMethodNotAllowed)
			return false
		}
		return true
	}
	// load reads the flow named by ?id=, answering 404 itself when there is none.
	load := func(w http.ResponseWriter, r *http.Request) (string, *Flow) {
		d := dir(w)
		if d == "" {
			return "", nil
		}
		f, err := loadFlow(d, r.URL.Query().Get("id"))
		if err != nil {
			http.Error(w, err.Error(), http.StatusNotFound)
			return "", nil
		}
		return d, f
	}

	// GET /api/flows -> { flows: [...], running: "flow-…" }
	mux.HandleFunc("/api/flows", func(w http.ResponseWriter, r *http.Request) {
		d := dir(w)
		if d == "" {
			return
		}
		h.mu.Lock()
		running := h.runningFlow
		h.mu.Unlock()
		writeJSON(w, map[string]any{"flows": listFlows(d), "running": running})
	})

	// GET /api/flows/get?id=… -> the whole flow, with a readable label for every step.
	mux.HandleFunc("/api/flows/get", func(w http.ResponseWriter, r *http.Request) {
		_, f := load(w, r)
		if f == nil {
			return
		}
		steps := make([]flowStepView, len(f.Steps))
		for i, s := range f.Steps {
			v := flowStepView{FlowStep: s, Label: s.label()}
			if s.Kind != "page" {
				v.Hints = strings.TrimSpace(strings.Trim(strings.TrimSpace(s.Target.hints()), "[]"))
			}
			steps[i] = v
		}
		writeJSON(w, map[string]any{"flow": f.info(), "startTitle": f.StartTitle, "by": f.By, "steps": steps})
	})

	// POST /api/flows/rename?id=…&name=…
	mux.HandleFunc("/api/flows/rename", func(w http.ResponseWriter, r *http.Request) {
		if !post(w, r) {
			return
		}
		d, f := load(w, r)
		if f == nil {
			return
		}
		name := strings.TrimSpace(cut(r.URL.Query().Get("name"), 80))
		if name == "" {
			http.Error(w, "name is required", http.StatusBadRequest)
			return
		}
		f.Name = name
		if err := saveFlow(d, f); err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		h.flowsChanged()
		w.WriteHeader(http.StatusNoContent)
	})

	// POST /api/flows/step?id=…&n=3&action=delete|value  (value: JSON body {"value": "…"})
	mux.HandleFunc("/api/flows/step", func(w http.ResponseWriter, r *http.Request) {
		if !post(w, r) {
			return
		}
		d, f := load(w, r)
		if f == nil {
			return
		}
		n, err := strconv.Atoi(r.URL.Query().Get("n"))
		if err != nil || n < 1 || n > len(f.Steps) {
			http.Error(w, "no such step", http.StatusBadRequest)
			return
		}
		s := &f.Steps[n-1]
		switch r.URL.Query().Get("action") {
		case "delete":
			if len(f.Steps) == 1 {
				http.Error(w, "a flow needs at least one step: delete the flow instead", http.StatusBadRequest)
				return
			}
			f.Steps = slices.Delete(f.Steps, n-1, n)
		case "value":
			var body struct {
				Value string `json:"value"`
			}
			if json.NewDecoder(io.LimitReader(r.Body, 64<<10)).Decode(&body) != nil {
				http.Error(w, "send {\"value\": \"…\"}", http.StatusBadRequest)
				return
			}
			switch {
			case s.Kind == "fill" && !s.Secret:
				s.Value = cut(body.Value, 5000)
			case s.Kind == "select" && strings.TrimSpace(body.Value) != "":
				s.Values = []string{cut(body.Value, 200)}
			default:
				http.Error(w, "only typed text (not passwords) and choices can be edited", http.StatusBadRequest)
				return
			}
		default:
			http.Error(w, "action must be delete or value", http.StatusBadRequest)
			return
		}
		if err := saveFlow(d, f); err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		h.flowsChanged()
		w.WriteHeader(http.StatusNoContent)
	})

	// POST /api/flows/delete?id=…
	mux.HandleFunc("/api/flows/delete", func(w http.ResponseWriter, r *http.Request) {
		if !post(w, r) {
			return
		}
		d := dir(w)
		if d == "" {
			return
		}
		p, err := flowPath(d, r.URL.Query().Get("id"))
		if err == nil && os.Remove(p) != nil {
			err = errors.New("no such flow")
		}
		if err != nil {
			http.Error(w, err.Error(), http.StatusNotFound)
			return
		}
		h.flowsChanged()
		w.WriteHeader(http.StatusNoContent)
	})

	// GET /api/flows/export?id=… -> downloads the flow as a JSON file.
	mux.HandleFunc("/api/flows/export", func(w http.ResponseWriter, r *http.Request) {
		_, f := load(w, r)
		if f == nil {
			return
		}
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("Content-Disposition", "attachment; filename=\""+f.ID+".json\"")
		enc := json.NewEncoder(w)
		enc.SetIndent("", "  ")
		enc.Encode(f)
	})

	// POST /api/flows/import (multipart field "file", or a raw JSON body) -> {id}. Always gets a new id.
	mux.HandleFunc("/api/flows/import", func(w http.ResponseWriter, r *http.Request) {
		if !post(w, r) {
			return
		}
		d := dir(w)
		if d == "" {
			return
		}
		var data []byte
		if strings.HasPrefix(r.Header.Get("Content-Type"), "multipart/") { // an uploaded file; else the body is the JSON
			file, _, err := r.FormFile("file")
			if err != nil {
				http.Error(w, "send the flow file in the field \"file\"", http.StatusBadRequest)
				return
			}
			defer file.Close()
			data, _ = io.ReadAll(io.LimitReader(file, 8<<20))
		} else {
			data, _ = io.ReadAll(io.LimitReader(r.Body, 8<<20))
		}
		var f Flow
		if err := json.Unmarshal(data, &f); err != nil {
			http.Error(w, "not a flow file: "+err.Error(), http.StatusBadRequest)
			return
		}
		if err := validFlow(&f); err != nil {
			http.Error(w, "not a usable flow: "+err.Error(), http.StatusBadRequest)
			return
		}
		f.ID = newFlowID(d)
		if f.Created.IsZero() {
			f.Created = time.Now()
		}
		if f.Name == "" {
			f.Name = defaultFlowName(&f)
		}
		if err := saveFlow(d, &f); err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		h.flowsChanged()
		writeJSON(w, map[string]string{"id": f.ID})
	})

	// POST /api/flows/run {id, repeat, inputs} -> {log} once the replay is over (the agent does it, everyone watches).
	mux.HandleFunc("/api/flows/run", func(w http.ResponseWriter, r *http.Request) {
		if !post(w, r) {
			return
		}
		a := mcp.agent
		if a == nil {
			http.Error(w, "no browser is running", http.StatusServiceUnavailable)
			return
		}
		var body struct {
			ID     string            `json:"id"`
			Repeat int               `json:"repeat"`
			Inputs map[string]string `json:"inputs"`
		}
		if json.NewDecoder(io.LimitReader(r.Body, 256<<10)).Decode(&body) != nil {
			http.Error(w, "send {\"id\": \"flow-…\"}", http.StatusBadRequest)
			return
		}
		if !flowRunning.CompareAndSwap(false, true) {
			http.Error(w, "a flow is already running: wait for it or stop it", http.StatusConflict)
			return
		}
		defer flowRunning.Store(false)
		h.mu.Lock()
		h.humanLast = time.Time{} // the person asked for it: the agent may take control right away
		h.mu.Unlock()
		log, err := runFlowFromViewer(a, body.ID, body.Repeat, body.Inputs)
		if err != nil {
			writeJSON(w, map[string]any{"ok": false, "log": err.Error()})
			return
		}
		writeJSON(w, map[string]any{"ok": true, "log": log})
	})

	// POST /api/flows/stop -> the running replay stops after its current step.
	mux.HandleFunc("/api/flows/stop", func(w http.ResponseWriter, r *http.Request) {
		if !post(w, r) {
			return
		}
		flowStop.Store(true)
		w.WriteHeader(http.StatusNoContent)
	})
}

// runFlowFromViewer replays a flow as the agent, the way an MCP tool call would.
func runFlowFromViewer(a *Agent, id string, repeat int, inputs map[string]string) (string, error) {
	unlock, err := a.b.lockGesture("", "viewer")
	if err != nil {
		return "", err
	}
	defer unlock()
	a.mu.Lock()
	defer a.mu.Unlock()
	a.begin()
	defer a.hub.agentDone()
	return a.RunFlow(id, repeat, inputs, false)
}
