package main

// Inspect: Chrome DevTools for the tabs of the shared browser, inside the viewer.
//
// Chrome already serves the DevTools front end and a WebSocket per tab on its debugging port, but
// that port only listens on this machine and has no password. So the viewer never talks to it: it
// goes through /devtools/ here, which sits behind the same login as everything else, and only lets
// through what the front end needs (its own files and the sockets of the tabs we own). The
// browser-wide socket (…/devtools/browser/…) is refused: it could open or close any tab.

import (
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"
)

// debugAddr is host:port of Chrome's debugging server. Chrome picks the port itself and writes
// it to DevToolsActivePort in the profile folder, so read it every time (a restart changes it).
func (b *Browser) debugAddr() (string, error) {
	if b.opt.NoDevTools {
		return "", errors.New("DevTools is turned off on this server (-no-devtools)")
	}
	raw, err := os.ReadFile(filepath.Join(b.opt.Profile, "DevToolsActivePort"))
	if err != nil {
		return "", errors.New("the browser has not published its debugging port")
	}
	port, _, _ := strings.Cut(strings.TrimSpace(string(raw)), "\n")
	if n, err := strconv.Atoi(strings.TrimSpace(port)); err != nil || n <= 0 {
		return "", errors.New("the browser's debugging port is unreadable")
	}
	return "127.0.0.1:" + strings.TrimSpace(port), nil
}

// ownsTab reports whether id is one of the shared browser's tabs.
func (b *Browser) ownsTab(id string) bool {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.findLocked(id) >= 0
}

// devtoolsOK says whether this browser can show DevTools: its debugging server answers and it
// has the front end (a slimmed-down chrome-headless-shell does not).
func (b *Browser) devtoolsOK() error {
	addr, err := b.debugAddr()
	if err != nil {
		return err
	}
	c := http.Client{Timeout: 3 * time.Second}
	resp, err := c.Get("http://" + addr + "/devtools/inspector.html")
	if err != nil {
		return errors.New("the browser's debugging server does not answer")
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("this browser (%s) has no built-in DevTools; use -engine chrome", b.opt.Engine.Label)
	}
	return nil
}

func registerDevtoolsAPI(mux *http.ServeMux, h *Hub) {
	// GET /api/devtools -> { ok: true } or { ok: false, error: "…" }
	mux.HandleFunc("/api/devtools", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		if h.browser == nil {
			json.NewEncoder(w).Encode(map[string]any{"ok": false, "error": "no browser is running"})
			return
		}
		if err := h.browser.devtoolsOK(); err != nil {
			json.NewEncoder(w).Encode(map[string]any{"ok": false, "error": err.Error()})
			return
		}
		json.NewEncoder(w).Encode(map[string]any{"ok": true})
	})

	proxy := &httputil.ReverseProxy{
		Director: func(r *http.Request) {}, // set per request below
		ErrorHandler: func(w http.ResponseWriter, r *http.Request, err error) {
			http.Error(w, "DevTools is not reachable: "+err.Error(), http.StatusBadGateway)
		},
	}

	// /devtools/… -> Chrome's debugging server (the front end's files, and a socket per tab).
	mux.HandleFunc("/devtools/", func(w http.ResponseWriter, r *http.Request) {
		b := h.browser
		if b == nil {
			http.Error(w, "no browser is running", http.StatusServiceUnavailable)
			return
		}
		// Only the front end's files and the sockets of our own tabs.
		if rest, ok := strings.CutPrefix(r.URL.Path, "/devtools/page/"); ok {
			if !b.ownsTab(rest) {
				http.Error(w, "not a tab of the shared browser", http.StatusForbidden)
				return
			}
		} else if strings.HasPrefix(r.URL.Path, "/devtools/browser") || strings.Contains(r.URL.Path, "..") {
			http.Error(w, "forbidden", http.StatusForbidden)
			return
		}
		if r.Method != http.MethodGet && r.Method != http.MethodHead {
			http.Error(w, "GET only", http.StatusMethodNotAllowed)
			return
		}
		// A page on another site must not be able to open a socket to a tab through the visitor's
		// browser (nothing else guards it when the room has no password): same origin only.
		if o := r.Header.Get("Origin"); o != "" {
			if u, err := url.Parse(o); err != nil || u.Host != r.Host {
				http.Error(w, "cross-origin access to DevTools is not allowed", http.StatusForbidden)
				return
			}
		}
		addr, err := b.debugAddr()
		if err != nil {
			http.Error(w, err.Error(), http.StatusServiceUnavailable)
			return
		}
		r2 := r.Clone(r.Context())
		r2.URL = &url.URL{Scheme: "http", Host: addr, Path: r.URL.Path, RawQuery: r.URL.RawQuery}
		// Chrome only talks to callers that look local: it refuses other Host and Origin values.
		r2.Host = addr
		r2.Header.Del("Origin")
		r2.Header.Del("Cookie") // the room's login is not for Chrome
		r2.Header.Del("Authorization")
		proxy.ServeHTTP(w, r2)
	})
}
