package main

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"time"
)

// ProxySwitch lets the shared browser change its public IP while it runs. Chrome is
// pointed at a tiny SOCKS5 server on 127.0.0.1 (this process); each connection it
// opens is dialled either directly or through the upstream proxy (e.g. Cloudflare
// WARP). Switching only changes where new connections go, so no browser restart.
// Nothing outside the browser is touched.
type ProxySwitch struct {
	upstream string      // host:port of an upstream SOCKS5 proxy (-proxy); "" = built-in WARP
	warp     *warpTunnel // built-in WARP, used when there is no upstream

	mu       sync.Mutex
	on       bool
	ln       net.Listener
	conns    map[net.Conn]bool
	gen      int
	ip       string
	loc      string
	warpOn   string // "on"/"plus"/"off" as Cloudflare sees the browser
	warpErr  string // why the built-in WARP could not start
	checking bool
	err      string
}

// newProxySwitch parses an upstream like socks5://warp:1080. Empty = built-in WARP,
// whose account lives in warpFile ("" = no WARP: the switch is not offered).
func newProxySwitch(raw, warpFile string) (*ProxySwitch, error) {
	p := &ProxySwitch{conns: map[net.Conn]bool{}}
	if raw == "" {
		if warpFile != "" {
			p.warp = &warpTunnel{file: warpFile}
			p.on = p.warp.wasOn()
		}
		return p, nil
	}
	if !strings.Contains(raw, "://") {
		raw = "socks5://" + raw
	}
	u, err := url.Parse(raw)
	if err != nil || u.Host == "" {
		return nil, fmt.Errorf("bad -proxy %q (want socks5://host:port)", raw)
	}
	if u.Scheme != "socks5" && u.Scheme != "socks5h" {
		return nil, fmt.Errorf("-proxy %q: only socks5:// is supported (Cloudflare WARP exposes SOCKS5)", raw)
	}
	p.upstream = u.Host
	p.on = true
	return p, nil
}

func (p *ProxySwitch) Configured() bool { return p.upstream != "" || p.warp != nil }

// Listen starts the local SOCKS5 server and returns the address Chrome should use.
func (p *ProxySwitch) Listen() (string, error) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return "", err
	}
	p.ln = ln
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				return
			}
			go p.serve(c)
		}
	}()
	return "socks5://" + ln.Addr().String(), nil
}

func (p *ProxySwitch) isOn() bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.on
}

func (p *ProxySwitch) track(c net.Conn, add bool) {
	p.mu.Lock()
	if add {
		p.conns[c] = true
	} else {
		delete(p.conns, c)
	}
	p.mu.Unlock()
}

// dial opens a connection the way the browser's traffic currently goes.
func (p *ProxySwitch) dial(ctx context.Context, addr string) (net.Conn, error) {
	d := net.Dialer{Timeout: 15 * time.Second}
	if p.isOn() && p.warp != nil {
		return p.warp.dial(ctx, addr)
	}
	if !p.isOn() || p.upstream == "" {
		// IPv4 first, so sites see the same IPv4 the header shows; IPv6-only hosts still work.
		if c, err := d.DialContext(ctx, "tcp4", addr); err == nil {
			return c, nil
		}
		return d.DialContext(ctx, "tcp", addr)
	}
	c, err := d.DialContext(ctx, "tcp", p.upstream)
	if err != nil {
		return nil, fmt.Errorf("proxy %s: %w", p.upstream, err)
	}
	if err := socks5Connect(c, addr); err != nil {
		c.Close()
		return nil, err
	}
	return c, nil
}

// socks5Connect performs the client side of a SOCKS5 CONNECT (no auth). The host is sent
// as a name, so DNS is resolved by the proxy, not by this machine.
func socks5Connect(c net.Conn, addr string) error {
	host, portS, err := net.SplitHostPort(addr)
	if err != nil {
		return err
	}
	port, err := strconv.Atoi(portS)
	if err != nil {
		return err
	}
	c.SetDeadline(time.Now().Add(15 * time.Second))
	defer c.SetDeadline(time.Time{})
	if _, err := c.Write([]byte{5, 1, 0}); err != nil {
		return err
	}
	var rep [2]byte
	if _, err := io.ReadFull(c, rep[:]); err != nil {
		return err
	}
	if rep[0] != 5 || rep[1] != 0 {
		return errors.New("socks5: proxy refused the handshake")
	}
	req := []byte{5, 1, 0}
	if ip := net.ParseIP(host); ip != nil && ip.To4() != nil {
		req = append(req, 1)
		req = append(req, ip.To4()...)
	} else if ip != nil {
		req = append(req, 4)
		req = append(req, ip.To16()...)
	} else {
		if len(host) > 255 {
			return errors.New("socks5: host name too long")
		}
		req = append(req, 3, byte(len(host)))
		req = append(req, host...)
	}
	req = binary.BigEndian.AppendUint16(req, uint16(port))
	if _, err := c.Write(req); err != nil {
		return err
	}
	var h [4]byte
	if _, err := io.ReadFull(c, h[:]); err != nil {
		return err
	}
	if h[1] != 0 {
		return fmt.Errorf("socks5: connect to %s failed (code %d)", addr, h[1])
	}
	n := 0
	switch h[3] {
	case 1:
		n = 4
	case 4:
		n = 16
	case 3:
		var l [1]byte
		if _, err := io.ReadFull(c, l[:]); err != nil {
			return err
		}
		n = int(l[0])
	default:
		return errors.New("socks5: bad reply")
	}
	_, err = io.CopyN(io.Discard, c, int64(n+2))
	return err
}

// serve handles one connection from Chrome: SOCKS5 CONNECT only.
func (p *ProxySwitch) serve(c net.Conn) {
	p.track(c, true)
	defer func() { c.Close(); p.track(c, false) }()
	c.SetDeadline(time.Now().Add(15 * time.Second))
	var h [2]byte
	if _, err := io.ReadFull(c, h[:]); err != nil || h[0] != 5 {
		return
	}
	if _, err := io.CopyN(io.Discard, c, int64(h[1])); err != nil {
		return
	}
	if _, err := c.Write([]byte{5, 0}); err != nil {
		return
	}
	var r [4]byte
	if _, err := io.ReadFull(c, r[:]); err != nil || r[0] != 5 || r[1] != 1 {
		c.Write([]byte{5, 7, 0, 1, 0, 0, 0, 0, 0, 0})
		return
	}
	var host string
	switch r[3] {
	case 1:
		var b [4]byte
		if _, err := io.ReadFull(c, b[:]); err != nil {
			return
		}
		host = net.IP(b[:]).String()
	case 4:
		var b [16]byte
		if _, err := io.ReadFull(c, b[:]); err != nil {
			return
		}
		host = net.IP(b[:]).String()
	case 3:
		var l [1]byte
		if _, err := io.ReadFull(c, l[:]); err != nil {
			return
		}
		b := make([]byte, l[0])
		if _, err := io.ReadFull(c, b); err != nil {
			return
		}
		host = string(b)
	default:
		return
	}
	var pb [2]byte
	if _, err := io.ReadFull(c, pb[:]); err != nil {
		return
	}
	addr := net.JoinHostPort(host, strconv.Itoa(int(binary.BigEndian.Uint16(pb[:]))))
	out, err := p.dial(context.Background(), addr)
	if err != nil {
		c.Write([]byte{5, 4, 0, 1, 0, 0, 0, 0, 0, 0})
		return
	}
	p.track(out, true)
	defer func() { out.Close(); p.track(out, false) }()
	c.Write([]byte{5, 0, 0, 1, 0, 0, 0, 0, 0, 0})
	c.SetDeadline(time.Time{})
	done := make(chan struct{}, 2)
	go func() { io.Copy(out, c); done <- struct{}{} }()
	go func() { io.Copy(c, out); done <- struct{}{} }()
	<-done
}

// Set switches the browser between its own IP route and the upstream proxy. Open
// connections are closed so the next requests use the new route.
func (p *ProxySwitch) Set(on bool) {
	p.mu.Lock()
	if !p.Configured() || p.on == on {
		p.mu.Unlock()
		return
	}
	p.on = on
	p.gen++
	p.ip, p.loc, p.warpOn, p.err, p.warpErr = "", "", "", "", ""
	p.checking = true
	for c := range p.conns {
		c.Close()
	}
	p.mu.Unlock()
	go func() {
		if p.warp != nil {
			if on {
				if _, err := p.warp.net(); err != nil { // first time: sign up + start the tunnel
					p.mu.Lock()
					p.on, p.warpErr = false, err.Error()
					p.gen++
					p.mu.Unlock()
					log.Println("warp:", err)
					go p.Refresh()
					return
				}
			}
			p.warp.remember(on)
		}
		p.Refresh()
	}()
}

// Refresh looks up the public IP the browser currently has (through the same route).
func (p *ProxySwitch) Refresh() {
	p.mu.Lock()
	gen := p.gen
	p.checking = true
	p.mu.Unlock()
	ip, loc, warp, err := p.lookup()
	p.mu.Lock()
	defer p.mu.Unlock()
	if gen != p.gen { // switched meanwhile: that switch starts its own lookup
		return
	}
	p.checking = false
	if err != nil {
		p.ip, p.loc, p.warpOn, p.err = "", "", "", err.Error()
		return
	}
	p.ip, p.loc, p.warpOn, p.err = ip, loc, warp, ""
}

func (p *ProxySwitch) lookup() (ip, loc, warp string, err error) {
	cl := &http.Client{
		Timeout: 12 * time.Second,
		Transport: &http.Transport{
			DialContext:       func(ctx context.Context, _, addr string) (net.Conn, error) { return p.dial(ctx, addr) },
			DisableKeepAlives: true,
		},
	}
	// 1.1.1.1 is an IPv4 address, so the reply shows the IPv4 the browser goes out with.
	resp, err := cl.Get("https://1.1.1.1/cdn-cgi/trace")
	if err != nil {
		resp, err = cl.Get("https://www.cloudflare.com/cdn-cgi/trace")
	}
	if err != nil {
		return "", "", "", err
	}
	defer resp.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(resp.Body, 8192))
	for _, line := range strings.Split(string(b), "\n") {
		k, v, ok := strings.Cut(strings.TrimSpace(line), "=")
		if !ok {
			continue
		}
		switch k {
		case "ip":
			ip = v
		case "loc":
			loc = v
		case "warp":
			warp = v
		}
	}
	if ip == "" {
		return "", "", "", errors.New("no IP in reply")
	}
	return ip, loc, warp, nil
}

// Watch keeps the IP fresh so the header always shows the current one.
func (p *ProxySwitch) Watch() {
	p.Refresh()
	for range time.Tick(30 * time.Second) {
		p.Refresh()
	}
}

type proxyStatus struct {
	Configured bool   `json:"configured"`
	On         bool   `json:"on"`
	IP         string `json:"ip"`
	Loc        string `json:"loc"`
	Warp       string `json:"warp"`
	Checking   bool   `json:"checking"`
	Error      string `json:"error"`
	WarpError  string `json:"warpError"`
	CanToggle  bool   `json:"canToggle"`
}

func (p *ProxySwitch) Status() proxyStatus {
	p.mu.Lock()
	defer p.mu.Unlock()
	return proxyStatus{Configured: p.Configured(), On: p.on, IP: p.ip, Loc: p.loc, Warp: p.warpOn, Checking: p.checking, Error: p.err, WarpError: p.warpErr}
}

// GET /api/proxy -> status;  POST /api/proxy {"on":true|false}
func registerProxyAPI(mux *http.ServeMux, p *ProxySwitch) {
	mux.HandleFunc("/api/proxy", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		can := !viaTunnel(r)
		if r.Method == http.MethodPost {
			if !can {
				http.Error(w, "the IP can only be switched from the machine that runs the app", http.StatusForbidden)
				return
			}
			var req struct {
				On bool `json:"on"`
			}
			json.NewDecoder(io.LimitReader(r.Body, 4096)).Decode(&req)
			p.Set(req.On)
		}
		s := p.Status()
		s.CanToggle = can
		json.NewEncoder(w).Encode(s)
	})
}

func startProxy(raw, warpFile string) (*ProxySwitch, string) {
	p, err := newProxySwitch(raw, warpFile)
	if err != nil {
		log.Fatal(err)
	}
	addr := ""
	if p.Configured() {
		if addr, err = p.Listen(); err != nil {
			log.Fatalf("proxy: %v", err)
		}
	}
	go p.Watch()
	return p, addr
}
