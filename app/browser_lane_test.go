package main

import (
	"sync"
	"testing"
	"time"
)

func TestRunTabSerializesEachTabIndependently(t *testing.T) {
	b := &Browser{tabs: []*Tab{{id: "one"}, {id: "two"}}}
	var mu sync.Mutex
	started := map[string]time.Time{}
	release := make(chan struct{})
	oneDone := make(chan error, 1)
	go func() {
		oneDone <- b.runTab("one", func(*Tab) error {
			mu.Lock()
			started["one"] = time.Now()
			mu.Unlock()
			<-release
			return nil
		})
	}()
	for {
		mu.Lock()
		ready := !started["one"].IsZero()
		mu.Unlock()
		if ready {
			break
		}
		time.Sleep(time.Millisecond)
	}
	if err := b.runTab("two", func(*Tab) error {
		mu.Lock()
		started["two"] = time.Now()
		mu.Unlock()
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	mu.Lock()
	parallel := started["two"].Before(started["one"].Add(100 * time.Millisecond))
	mu.Unlock()
	if !parallel {
		t.Fatal("different tab lane did not start while the first tab was blocked")
	}
	close(release)
	if err := <-oneDone; err != nil {
		t.Fatal(err)
	}

	startedSecond := make(chan struct{})
	firstStarted := make(chan struct{})
	finishFirst := make(chan struct{})
	go b.runTab("one", func(*Tab) error { close(firstStarted); <-finishFirst; return nil })
	<-firstStarted
	go b.runTab("one", func(*Tab) error { close(startedSecond); return nil })
	select {
	case <-startedSecond:
		t.Fatal("same tab lane ran callbacks concurrently")
	case <-time.After(20 * time.Millisecond):
	}
	close(finishFirst)
	select {
	case <-startedSecond:
	case <-time.After(time.Second):
		t.Fatal("second same-tab callback did not run after the first finished")
	}
}
