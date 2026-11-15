package main

// Keep-alive: while it is on, the shared browser moves the mouse and scrolls a little every
// so often, like a person sitting at the page, so sites that stop on inactivity
// (Google Colab, dashboards, ...) keep the session. Off by default; it runs on the
// server, so it keeps going when nobody has the web page open.

import (
	"encoding/json"
	"io"
	"math/rand"
	"net/http"
	"sync"
	"time"

	"github.com/chromedp/cdproto/input"
	"github.com/chromedp/chromedp"
)

type KeepAlive struct {
	b    *Browser
	mu   sync.Mutex
	on   bool
	stop chan struct{}
	last time.Time
}

type keepAliveStatus struct {
	On        bool  `json:"on"`
	Available bool  `json:"available"`
	CanToggle bool  `json:"canToggle"`
	Last      int64 `json:"last,omitempty"` // unix ms of the last simulated activity
}

func (k *KeepAlive) Status() keepAliveStatus {
	k.mu.Lock()
	defer k.mu.Unlock()
	s := keepAliveStatus{On: k.on, Available: k.b != nil}
	if !k.last.IsZero() {
		s.Last = k.last.UnixMilli()
	}
	return s
}

func (k *KeepAlive) Set(on bool) {
	if k.b == nil {
		return
	}
	k.mu.Lock()
	defer k.mu.Unlock()
	if on == k.on {
		return
	}
	k.on = on
	if !on {
		close(k.stop)
		return
	}
	k.stop = make(chan struct{})
	go k.loop(k.stop)
}

func (k *KeepAlive) loop(stop chan struct{}) {
	for {
		// Colab goes idle after a few minutes, so every 20-45 s is plenty.
		wait := 20*time.Second + time.Duration(rand.Intn(25))*time.Second
		select {
		case <-stop:
			return
		case <-time.After(wait):
		}
		k.tick()
	}
}

// tick nudges every open tab once. A tab that is loading or busy is skipped.
func (k *KeepAlive) tick() {
	b := k.b
	b.mu.Lock()
	ids := make([]string, len(b.tabs))
	for i, t := range b.tabs {
		ids[i] = t.id
	}
	v := b.view
	b.mu.Unlock()
	if v.Width <= 0 || v.Height <= 0 {
		return
	}
	for _, id := range ids {
		if b.busyTab(id) {
			continue
		}
		b.runTab(id, func(t *Tab) error { return humanNudge(t, v) })
	}
	k.mu.Lock()
	k.last = time.Now()
	k.mu.Unlock()
}

// humanNudge: a short curved mouse path, and often a small scroll down and back up.
func humanNudge(t *Tab, v View) error {
	w, h := float64(v.Width), float64(v.Height)
	x, y := w*(0.3+rand.Float64()*0.4), h*(0.3+rand.Float64()*0.4)
	move := func(px, py float64) error {
		ctx, cancel := inputCtx(t)
		defer cancel()
		_, err := chromedp.Call(ctx, input.DispatchMouseEvent, input.DispatchMouseEventParams{
			Type: input.DispatchMouseEventTypeMouseMoved, X: px, Y: py, Button: input.MouseButtonNone})
		return err
	}
	tx, ty := w*(0.2+rand.Float64()*0.6), h*(0.2+rand.Float64()*0.6)
	const steps = 12
	for i := 1; i <= steps; i++ {
		f := float64(i) / steps
		f = f * f * (3 - 2*f) // ease in/out
		jitter := rand.Float64()*4 - 2
		if err := move(x+(tx-x)*f+jitter, y+(ty-y)*f+jitter); err != nil {
			return err
		}
		time.Sleep(time.Duration(15+rand.Intn(25)) * time.Millisecond)
	}
	if rand.Intn(2) == 0 {
		return nil
	}
	d := float64(60 + rand.Intn(120))
	for _, dy := range []float64{d, -d} {
		ctx, cancel := inputCtx(t)
		_, err := chromedp.Call(ctx, dispatchWheel, wheelParams{Type: string(input.DispatchMouseEventTypeMouseWheel), X: tx, Y: ty, DeltaY: dy})
		cancel()
		if err != nil {
			return err
		}
		time.Sleep(time.Duration(300+rand.Intn(500)) * time.Millisecond)
	}
	return nil
}

func registerKeepAliveAPI(mux *http.ServeMux, k *KeepAlive) {
	mux.HandleFunc("/api/keepalive", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		can := !viaTunnel(r)
		if r.Method == http.MethodPost {
			if !can {
				http.Error(w, "keep-alive can only be switched from the machine that runs the app", http.StatusForbidden)
				return
			}
			var req struct {
				On bool `json:"on"`
			}
			json.NewDecoder(io.LimitReader(r.Body, 4096)).Decode(&req)
			k.Set(req.On)
		}
		s := k.Status()
		s.CanToggle = can
		json.NewEncoder(w).Encode(s)
	})
}
