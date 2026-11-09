package main

// Public tunnel (-tunnel): same idea as https://github.com/tuanngocptn/nport, built in.
// We ask the nport backend for a Cloudflare tunnel (no account needed), then run the
// cloudflared binary that is embedded in this program. No router/port forwarding needed.

import (
	"bufio"
	"bytes"
	"crypto/rand"
	"embed"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"time"
)

// scripts/fetch-cloudflared.sh puts the cloudflared binary for the build platform here.
//
//go:embed all:cloudflared
var cloudflaredFS embed.FS

const defaultTunnelBackend = "https://api.nport.link"

type Tunnel struct {
	URL       string
	backend   string
	subdomain string
	tunnelID  string
	cmd       *exec.Cmd
}

type tunnelAPIResponse struct {
	Success     bool   `json:"success"`
	Error       string `json:"error"`
	TunnelID    string `json:"tunnelId"`
	TunnelToken string `json:"tunnelToken"`
	URL         string `json:"url"`
}

func randomHex(n int) string {
	b := make([]byte, n)
	rand.Read(b)
	return hex.EncodeToString(b)
}

// extractCloudflared writes the embedded binary to the cache folder and returns its path.
// Falls back to a cloudflared found in PATH when none was embedded.
func extractCloudflared() (string, error) {
	name := "cloudflared"
	if runtime.GOOS == "windows" {
		name += ".exe"
	}
	data, err := cloudflaredFS.ReadFile("cloudflared/" + name)
	if err != nil {
		if p, e := exec.LookPath("cloudflared"); e == nil {
			return p, nil
		}
		return "", errors.New("cloudflared is not embedded in this build: run scripts/fetch-cloudflared.sh and rebuild (or install cloudflared)")
	}
	dir, err := os.UserCacheDir()
	if err != nil {
		dir = os.TempDir()
	}
	dir = filepath.Join(dir, appName)
	if err := os.MkdirAll(dir, 0755); err != nil {
		return "", err
	}
	path := filepath.Join(dir, fmt.Sprintf("%s-%d", name, len(data)))
	if st, err := os.Stat(path); err == nil && st.Size() == int64(len(data)) {
		return path, nil
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, data, 0755); err != nil {
		return "", err
	}
	return path, os.Rename(tmp, path)
}

func tunnelRequest(method, backend string, body any) (*tunnelAPIResponse, error) {
	buf, _ := json.Marshal(body)
	req, _ := http.NewRequest(method, backend, bytes.NewReader(buf))
	req.Header.Set("Content-Type", "application/json")
	resp, err := (&http.Client{Timeout: 30 * time.Second}).Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	var r tunnelAPIResponse
	if err := json.Unmarshal(raw, &r); err != nil {
		return nil, fmt.Errorf("backend answered %d: %s", resp.StatusCode, strings.TrimSpace(string(raw)))
	}
	if resp.StatusCode >= 400 || (method == http.MethodPost && !r.Success) {
		msg := r.Error
		if msg == "" {
			msg = string(raw)
		}
		return nil, fmt.Errorf("backend: %s", msg)
	}
	return &r, nil
}

// startTunnel exposes localhost:port on a public https URL.
func startTunnel(port int, subdomain, backend string) (*Tunnel, error) {
	if backend == "" {
		backend = defaultTunnelBackend
	}
	if subdomain == "" {
		subdomain = "abu-" + randomHex(4)
	}
	bin, err := extractCloudflared()
	if err != nil {
		return nil, err
	}
	r, err := tunnelRequest(http.MethodPost, backend, map[string]string{"subdomain": subdomain})
	if err != nil {
		return nil, err
	}
	t := &Tunnel{backend: backend, subdomain: subdomain, tunnelID: r.TunnelID, URL: r.URL}
	if !strings.Contains(t.URL, "://") {
		t.URL = "https://" + t.URL
	}

	cmd := exec.Command(bin, "tunnel", "--no-autoupdate", "run", "--token", r.TunnelToken, "--url", fmt.Sprintf("http://localhost:%d", port))
	stderr, _ := cmd.StderrPipe()
	if err := cmd.Start(); err != nil {
		t.Close()
		return nil, err
	}
	t.cmd = cmd

	connected := make(chan struct{})
	go func() {
		sc := bufio.NewScanner(stderr)
		sent := false
		for sc.Scan() {
			line := sc.Text()
			if !sent && strings.Contains(line, "Registered tunnel connection") {
				sent = true
				close(connected)
			}
			if strings.Contains(line, "ERR") && !strings.Contains(line, "retrying") {
				fmt.Fprintln(os.Stderr, "[cloudflared]", line)
			}
		}
	}()
	select {
	case <-connected:
	case <-time.After(30 * time.Second):
		t.Close()
		return nil, errors.New("tunnel did not connect within 30s (check your internet connection)")
	}
	return t, nil
}

// Close stops cloudflared and deletes the tunnel from the backend.
func (t *Tunnel) Close() {
	if t.cmd != nil && t.cmd.Process != nil {
		t.cmd.Process.Kill()
		t.cmd.Wait()
	}
	if t.tunnelID != "" {
		tunnelRequest(http.MethodDelete, t.backend, map[string]string{"subdomain": t.subdomain, "tunnelId": t.tunnelID})
	}
}

// ---------- Switch on/off at runtime (the button in the web UI) ----------

var tunnels *TunnelManager

type TunnelStatus struct {
	On        bool   `json:"on"`
	Busy      bool   `json:"busy,omitempty"`
	URL       string `json:"url,omitempty"`
	Password  string `json:"password,omitempty"`  // only when we generated it
	Generated bool   `json:"generated,omitempty"` // we set the password for the tunnel
	Error     string `json:"error,omitempty"`
	CanToggle bool   `json:"canToggle"`
}

type TunnelManager struct {
	port    int
	backend string

	mu        sync.Mutex
	t         *Tunnel
	busy      bool
	generated bool // we created the password and clear it again on Stop
	lastErr   string
}

func (m *TunnelManager) Status() TunnelStatus {
	m.mu.Lock()
	defer m.mu.Unlock()
	s := TunnelStatus{Busy: m.busy, Error: m.lastErr, Generated: m.generated}
	if m.t != nil {
		s.On, s.URL = true, m.t.URL
		if m.generated {
			s.Password = password()
		}
	}
	return s
}

func (m *TunnelManager) Start(subdomain string) error {
	m.mu.Lock()
	if m.t != nil || m.busy {
		m.mu.Unlock()
		return nil
	}
	m.busy, m.lastErr = true, ""
	m.mu.Unlock()

	// A public URL without a password would hand the browser to anyone.
	gen := false
	if password() == "" {
		setPassword(randomHex(8))
		gen = true
	}
	t, err := startTunnel(m.port, subdomain, m.backend)

	m.mu.Lock()
	defer m.mu.Unlock()
	m.busy = false
	if err != nil {
		if gen {
			setPassword("")
		}
		m.lastErr = err.Error()
		return err
	}
	m.t, m.generated = t, gen
	return nil
}

func (m *TunnelManager) Stop() {
	m.mu.Lock()
	t, gen := m.t, m.generated
	m.t, m.generated, m.lastErr = nil, false, ""
	m.mu.Unlock()
	if t == nil {
		return
	}
	t.Close()
	if gen {
		setPassword("")
	}
}

// Requests that came in through the tunnel carry Cloudflare headers. Only people on this
// machine/network may switch the tunnel; someone who got the link cannot switch it off.
func viaTunnel(r *http.Request) bool {
	return r.Header.Get("Cf-Connecting-Ip") != "" || r.Header.Get("Cf-Ray") != "" || r.Header.Get("X-Forwarded-For") != ""
}

// GET /api/tunnel -> status;  POST /api/tunnel {"on":true|false,"subdomain":"x"}
func registerTunnelAPI(mux *http.ServeMux, m *TunnelManager) {
	mux.HandleFunc("/api/tunnel", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		can := !viaTunnel(r)
		if r.Method == http.MethodPost {
			if !can {
				http.Error(w, "the tunnel can only be switched from the machine that runs the app", http.StatusForbidden)
				return
			}
			var req struct {
				On        bool   `json:"on"`
				Subdomain string `json:"subdomain"`
			}
			json.NewDecoder(io.LimitReader(r.Body, 4096)).Decode(&req)
			if req.On {
				m.Start(strings.ToLower(strings.TrimSpace(req.Subdomain)))
				// we may just have set a password: keep this browser logged in
				if p := password(); p != "" {
					http.SetCookie(w, &http.Cookie{Name: authCookie, Value: authToken(), Path: "/", HttpOnly: true,
						SameSite: http.SameSiteLaxMode, MaxAge: 30 * 24 * 3600})
				}
			} else {
				m.Stop()
			}
		}
		s := m.Status()
		s.CanToggle = can
		if !can {
			s.Password = ""
		}
		json.NewEncoder(w).Encode(s)
	})
}
