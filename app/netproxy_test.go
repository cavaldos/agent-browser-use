package main

import (
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"testing"
	"time"
)

// Chrome -> local SOCKS5 -> (switch on) upstream SOCKS5 -> target, then switched off.
func TestProxySwitchChain(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { io.WriteString(w, "hi") }))
	defer srv.Close()

	up, _ := newProxySwitch("", "") // plays the upstream: a direct SOCKS5 server
	upAddr, err := up.Listen()
	if err != nil {
		t.Fatal(err)
	}
	p, err := newProxySwitch(upAddr, "")
	if err != nil {
		t.Fatal(err)
	}
	local, err := p.Listen()
	if err != nil {
		t.Fatal(err)
	}
	get := func() string {
		cl := &http.Client{Timeout: 5 * time.Second, Transport: &http.Transport{DialContext: nil}}
		cl.Transport.(*http.Transport).Proxy = func(*http.Request) (*url.URL, error) { return url.Parse(local) }
		resp, err := cl.Get(srv.URL)
		if err != nil {
			t.Fatal(err)
		}
		defer resp.Body.Close()
		b, _ := io.ReadAll(resp.Body)
		return string(b)
	}
	if got := get(); got != "hi" {
		t.Fatalf("on: got %q", got)
	}
	p.Set(false)
	if p.Status().On {
		t.Fatal("still on")
	}
	if got := get(); got != "hi" {
		t.Fatalf("off: got %q", got)
	}
}
