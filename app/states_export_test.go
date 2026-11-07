package main

import (
	"encoding/json"
	"os"
	"testing"
)

// testAgent builds a minimal Agent whose only dependency is a StateDir on disk, enough to exercise
// the save/export/import file helpers without a running browser.
func testAgent(t *testing.T) *Agent {
	t.Helper()
	dir := t.TempDir()
	return &Agent{b: &Browser{opt: BrowserOptions{StateDir: dir}}}
}

func writeState(t *testing.T, a *Agent, name string, st savedState) {
	t.Helper()
	p, err := a.statePath(name)
	if err != nil {
		t.Fatalf("statePath(%q): %v", name, err)
	}
	data, _ := json.MarshalIndent(st, "", "  ")
	if err := os.WriteFile(p, data, 0o600); err != nil {
		t.Fatal(err)
	}
}

func TestExportImportRoundTrip(t *testing.T) {
	src := testAgent(t)
	writeState(t, src, "github", savedState{
		Origin:  "https://github.com",
		Cookies: []stCookie{{Name: "user_session", Value: "abc", Domain: "github.com", Path: "/"}},
		Local:   map[string]string{"theme": "dark"},
	})
	writeState(t, src, "gmail", savedState{
		Origin:  "https://mail.google.com",
		Cookies: []stCookie{{Name: "SID", Value: "xyz", Domain: ".google.com", Path: "/"}},
	})

	// Build a bundle of everything (what the export endpoint streams).
	b, err := src.buildBundle("")
	if err != nil {
		t.Fatalf("buildBundle all: %v", err)
	}
	if len(b.States) != 2 {
		t.Fatalf("want 2 states in bundle, got %d", len(b.States))
	}
	raw, _ := json.MarshalIndent(b, "", "  ")

	// Import the bundle into a fresh (empty) agent — simulating another machine.
	dst := testAgent(t)
	names, err := dst.saveBundle(raw, "fallback")
	if err != nil {
		t.Fatalf("import: %v", err)
	}
	if len(names) != 2 {
		t.Fatalf("want 2 imported, got %d: %v", len(names), names)
	}
	got, err := dst.readState("github")
	if err != nil {
		t.Fatalf("read imported github: %v", err)
	}
	if got.Origin != "https://github.com" || len(got.Cookies) != 1 || got.Cookies[0].Value != "abc" {
		t.Fatalf("imported github state mangled: %+v", got)
	}
	if got.Local["theme"] != "dark" {
		t.Fatalf("localStorage lost: %+v", got.Local)
	}
	if infos := dst.listStates(); len(infos) != 2 {
		t.Fatalf("listStates want 2, got %d", len(infos))
	}
}

func TestExportSingleAndBareImport(t *testing.T) {
	a := testAgent(t)
	writeState(t, a, "gmail", savedState{Origin: "https://mail.google.com", Cookies: []stCookie{{Name: "SID", Value: "x"}}})

	one, err := a.buildBundle("gmail")
	if err != nil {
		t.Fatalf("buildBundle one: %v", err)
	}
	if len(one.States) != 1 {
		t.Fatalf("single export should hold 1 state, got %d", len(one.States))
	}

	// A bare single savedState file (not a bundle) should import under its fallback name.
	data, _ := json.Marshal(savedState{Cookies: []stCookie{{Name: "k", Value: "v"}}})
	names, err := a.saveBundle(data, "legacy")
	if err != nil {
		t.Fatalf("import bare: %v", err)
	}
	if len(names) != 1 || names[0] != "legacy" {
		t.Fatalf("bare import names = %v", names)
	}
}

func TestImportRejectsJunk(t *testing.T) {
	a := testAgent(t)
	if _, err := a.saveBundle([]byte("not json"), "x"); err == nil {
		t.Fatal("expected error for junk input")
	}
}
