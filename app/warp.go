package main

import (
	"bytes"
	"context"
	"crypto/ecdh"
	"crypto/rand"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/netip"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"golang.zx2c4.com/wireguard/conn"
	"golang.zx2c4.com/wireguard/device"
	"golang.zx2c4.com/wireguard/tun/netstack"
)

// Built-in Cloudflare WARP: a WireGuard tunnel that lives entirely inside this process
// (userspace network stack). It creates no network interface and changes no route, so
// the machine keeps its own IP; only connections dialled through it (the agent
// browser's, via ProxySwitch) leave with a Cloudflare IP.
type warpTunnel struct {
	file string // saved account + keys (warp.json, private)

	mu   sync.Mutex
	tnet *netstack.Net
	dev  *device.Device
}

type warpAccount struct {
	PrivateKey string `json:"private_key"` // base64
	PeerKey    string `json:"peer_key"`    // base64
	Endpoint   string `json:"endpoint"`    // ip:port
	AddrV4     string `json:"addr_v4"`
	AddrV6     string `json:"addr_v6"`
}

const warpPeerKey = "bmXOC+F1FxEMF9dyiK2H5/1SUtzH0JuVo51h2wPfgyo="

func (w *warpTunnel) load() (*warpAccount, error) {
	b, err := os.ReadFile(w.file)
	if err != nil {
		return nil, err
	}
	var a warpAccount
	if err := json.Unmarshal(b, &a); err != nil {
		return nil, err
	}
	if a.PrivateKey == "" || a.AddrV4 == "" {
		return nil, errors.New("incomplete warp account")
	}
	return &a, nil
}

func (w *warpTunnel) save(a *warpAccount) error {
	os.MkdirAll(filepath.Dir(w.file), 0o755)
	b, _ := json.MarshalIndent(a, "", "  ")
	return os.WriteFile(w.file, b, 0o600)
}

// register creates a free WARP account for a new WireGuard key.
func warpRegister() (*warpAccount, error) {
	key, err := ecdh.X25519().GenerateKey(rand.Reader)
	if err != nil {
		return nil, err
	}
	body, _ := json.Marshal(map[string]any{
		"key": base64.StdEncoding.EncodeToString(key.PublicKey().Bytes()), "install_id": "", "fcm_token": "",
		"tos": time.Now().UTC().Format(time.RFC3339), "model": "PC", "serial_number": "", "locale": "en_US",
	})
	req, _ := http.NewRequest(http.MethodPost, "https://api.cloudflareclient.com/v0a2158/reg", bytes.NewReader(body))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("User-Agent", "okhttp/3.12.1")
	req.Header.Set("CF-Client-Version", "a-6.10-2158")
	resp, err := (&http.Client{Timeout: 20 * time.Second}).Do(req)
	if err != nil {
		return nil, fmt.Errorf("WARP sign-up: %w", err)
	}
	defer resp.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<16))
	if resp.StatusCode/100 != 2 {
		return nil, fmt.Errorf("WARP sign-up: HTTP %d %s", resp.StatusCode, strings.TrimSpace(string(b[:min(len(b), 200)])))
	}
	var r struct {
		Config struct {
			Peers []struct {
				PublicKey string `json:"public_key"`
				Endpoint  struct {
					V4   string `json:"v4"`
					Host string `json:"host"`
				} `json:"endpoint"`
			} `json:"peers"`
			Interface struct {
				Addresses struct {
					V4 string `json:"v4"`
					V6 string `json:"v6"`
				} `json:"addresses"`
			} `json:"interface"`
		} `json:"config"`
	}
	if err := json.Unmarshal(b, &r); err != nil || len(r.Config.Peers) == 0 || r.Config.Interface.Addresses.V4 == "" {
		return nil, errors.New("WARP sign-up: unexpected reply")
	}
	p := r.Config.Peers[0]
	a := &warpAccount{
		PrivateKey: base64.StdEncoding.EncodeToString(key.Bytes()),
		PeerKey:    p.PublicKey,
		AddrV4:     r.Config.Interface.Addresses.V4,
		AddrV6:     r.Config.Interface.Addresses.V6,
		Endpoint:   "162.159.192.1:2408",
	}
	if a.PeerKey == "" {
		a.PeerKey = warpPeerKey
	}
	if h, _, err := net.SplitHostPort(p.Endpoint.V4); err == nil && h != "" {
		a.Endpoint = net.JoinHostPort(h, "2408") // the API reports port 0
	}
	return a, nil
}

// net returns the running tunnel, signing up and starting it on first use.
func (w *warpTunnel) net() (*netstack.Net, error) {
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.tnet != nil {
		return w.tnet, nil
	}
	a, err := w.load()
	if err != nil {
		if a, err = warpRegister(); err != nil {
			return nil, err
		}
		if err := w.save(a); err != nil {
			return nil, err
		}
	}
	addrs := []netip.Addr{}
	for _, s := range []string{a.AddrV4, a.AddrV6} {
		if ip, err := netip.ParseAddr(strings.Split(s, "/")[0]); err == nil {
			addrs = append(addrs, ip)
		}
	}
	dns := []netip.Addr{netip.MustParseAddr("1.1.1.1"), netip.MustParseAddr("1.0.0.1")}
	tun, tnet, err := netstack.CreateNetTUN(addrs, dns, 1280)
	if err != nil {
		return nil, err
	}
	priv, err1 := base64.StdEncoding.DecodeString(a.PrivateKey)
	peer, err2 := base64.StdEncoding.DecodeString(a.PeerKey)
	if err1 != nil || err2 != nil {
		return nil, errors.New("warp.json: bad keys (delete it to sign up again)")
	}
	dev := device.NewDevice(tun, conn.NewDefaultBind(), device.NewLogger(device.LogLevelError, "warp: "))
	cfg := fmt.Sprintf("private_key=%s\npublic_key=%s\nendpoint=%s\npersistent_keepalive_interval=25\nallowed_ip=0.0.0.0/0\nallowed_ip=::/0\n",
		hex.EncodeToString(priv), hex.EncodeToString(peer), a.Endpoint)
	if err := dev.IpcSet(cfg); err != nil {
		dev.Close()
		return nil, err
	}
	if err := dev.Up(); err != nil {
		dev.Close()
		return nil, err
	}
	w.tnet, w.dev = tnet, dev
	return tnet, nil
}

func (w *warpTunnel) dial(ctx context.Context, addr string) (net.Conn, error) {
	tn, err := w.net()
	if err != nil {
		return nil, err
	}
	// The host name is resolved inside the tunnel (Cloudflare DNS), not by this machine.
	return tn.DialContext(ctx, "tcp", addr)
}
