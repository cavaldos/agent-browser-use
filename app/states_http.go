package main

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"time"
)

// registerStateAPI wires the saved-session (login state) endpoints used by the Settings panel:
// list, export (download a JSON bundle), import (upload one), load and delete. They operate on
// the states folder on disk; load also drives the shared browser, so it needs the agent.
func registerStateAPI(mux *http.ServeMux, mcp *MCPServer) {
	agent := func(w http.ResponseWriter) *Agent {
		if mcp.agent == nil {
			http.Error(w, "no browser is running", http.StatusServiceUnavailable)
			return nil
		}
		return mcp.agent
	}
	writeJSON := func(w http.ResponseWriter, v any) {
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(v)
	}

	// GET /api/states -> { "states": [ {name, cookies, origin, saved}, ... ] }
	mux.HandleFunc("/api/states", func(w http.ResponseWriter, r *http.Request) {
		a := agent(w)
		if a == nil {
			return
		}
		writeJSON(w, map[string]any{"states": a.listStates()})
	})

	// GET /api/states/export?current=1 -> downloads every login of the running browser right now.
	// GET /api/states/export[?name=foo] -> downloads saved state(s) (all of them if no name).
	mux.HandleFunc("/api/states/export", func(w http.ResponseWriter, r *http.Request) {
		a := agent(w)
		if a == nil {
			return
		}
		q := r.URL.Query()
		var bundle stateBundle
		var err error
		prefix := "sessions-"
		if q.Get("current") != "" {
			bundle, err = a.liveBundle()
			prefix = "browser-sessions-"
		} else {
			bundle, err = a.buildBundle(q.Get("name"))
		}
		if err != nil {
			http.Error(w, err.Error(), http.StatusNotFound)
			return
		}
		a.announce("📤", fmt.Sprintf("Export %d session(s)", len(bundle.States)), nil)
		fname := prefix + time.Now().Format("20060102-150405") + ".json"
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("Content-Disposition", "attachment; filename=\""+fname+"\"")
		enc := json.NewEncoder(w)
		enc.SetIndent("", "  ")
		_ = enc.Encode(bundle)
	})

	// POST /api/states/import -> upload a sessions file (multipart field "file", or a raw JSON body).
	// Every session in it is saved and loaded into the browser right away.
	mux.HandleFunc("/api/states/import", func(w http.ResponseWriter, r *http.Request) {
		a := agent(w)
		if a == nil {
			return
		}
		if r.Method != http.MethodPost {
			http.Error(w, "use POST", http.StatusMethodNotAllowed)
			return
		}
		var data []byte
		fallback := "imported"
		if f, hdr, err := r.FormFile("file"); err == nil {
			defer f.Close()
			data, _ = io.ReadAll(io.LimitReader(f, 8<<20))
			if hdr != nil {
				fallback = trimJSON(hdr.Filename)
			}
		} else {
			data, _ = io.ReadAll(io.LimitReader(r.Body, 8<<20))
		}
		if len(data) == 0 {
			http.Error(w, "no file uploaded", http.StatusBadRequest)
			return
		}
		names, msg, err := a.importAndApply(data, fallback)
		if names == nil {
			http.Error(w, err.Error(), http.StatusBadRequest)
			return
		}
		a.announce("📥", fmt.Sprintf("Import %d session(s)", len(names)), nil)
		if err != nil { // saved, but the browser did not take it
			writeJSON(w, map[string]any{"imported": names, "loaded": false, "error": err.Error()})
			return
		}
		writeJSON(w, map[string]any{"imported": names, "loaded": true, "message": msg})
	})

	// POST /api/states/save {"name":"foo"} -> capture the browser's current login under a name.
	mux.HandleFunc("/api/states/save", func(w http.ResponseWriter, r *http.Request) {
		a := agent(w)
		if a == nil {
			return
		}
		out, err := a.State("save", bodyName(r), "")
		if err != nil {
			http.Error(w, err.Error(), http.StatusBadRequest)
			return
		}
		writeJSON(w, map[string]any{"ok": true, "message": out})
	})

	// POST /api/states/load {"name":"foo"} -> restore a session into the shared browser.
	mux.HandleFunc("/api/states/load", func(w http.ResponseWriter, r *http.Request) {
		a := agent(w)
		if a == nil {
			return
		}
		name := bodyName(r)
		out, err := a.State("load", name, "")
		if err != nil {
			http.Error(w, err.Error(), http.StatusBadRequest)
			return
		}
		writeJSON(w, map[string]any{"ok": true, "message": out})
	})

	// POST /api/states/delete {"name":"foo"} -> remove a saved session file.
	mux.HandleFunc("/api/states/delete", func(w http.ResponseWriter, r *http.Request) {
		a := agent(w)
		if a == nil {
			return
		}
		name := bodyName(r)
		if _, err := a.State("delete", name, ""); err != nil {
			http.Error(w, err.Error(), http.StatusBadRequest)
			return
		}
		writeJSON(w, map[string]any{"ok": true})
	})
}

func bodyName(r *http.Request) string {
	var b struct {
		Name string `json:"name"`
	}
	_ = json.NewDecoder(io.LimitReader(r.Body, 1<<16)).Decode(&b)
	return b.Name
}

func trimJSON(s string) string {
	if len(s) > 5 && s[len(s)-5:] == ".json" {
		return s[:len(s)-5]
	}
	return s
}
