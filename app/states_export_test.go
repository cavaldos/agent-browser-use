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

func TestImportCookieEditorList(t *testing.T) {
	a := testAgent(t)
	data := []byte(`[
	  {"domain": ".facebook.com", "expirationDate": 4102444800, "hostOnly": false, "httpOnly": true, "name": "xs", "path": "/", "sameSite": "no_restriction", "secure": true, "session": false, "value": "abc"},
	  {"domain": "mail.google.com", "hostOnly": true, "httpOnly": false, "name": "GMAIL_AT", "path": "/mail", "sameSite": null, "secure": true, "session": true, "value": "q"}
	]`)
	names, err := a.saveBundle(data, "cookies")
	if err != nil || len(names) != 1 || names[0] != "cookies" {
		t.Fatalf("import cookie list: %v %v", names, err)
	}
	st, _ := a.readState("cookies")
	if len(st.Cookies) != 2 || st.Cookies[0].Expires != 4102444800 || !st.Cookies[1].Session {
		t.Fatalf("cookies mangled: %+v", st.Cookies)
	}
	if p := cookieParam(st.Cookies[0]); p.Domain != ".facebook.com" || p.SameSite != "None" || p.URL != "" {
		t.Fatalf("domain cookie param: %+v", p)
	}
	if p := cookieParam(st.Cookies[1]); p.Domain != "" || p.URL != "https://mail.google.com/mail" {
		t.Fatalf("host-only cookie must be set by URL: %+v", p)
	}
	if got := siteNames(st.Cookies); len(got) != 2 || got[0] != "facebook.com" || got[1] != "google.com" {
		t.Fatalf("siteNames = %v", got)
	}
}

func TestImportPlaywrightStorageState(t *testing.T) {
	a := testAgent(t)
	data := []byte(`{"cookies": [{"name": "sid", "value": "1", "domain": "example.com", "path": "/", "expires": -1, "httpOnly": true, "secure": true, "sameSite": "Lax"}],
	  "origins": [{"origin": "https://example.com", "localStorage": [{"name": "token", "value": "t1"}]}]}`)
	if _, err := a.saveBundle(data, "pw"); err != nil {
		t.Fatalf("import playwright: %v", err)
	}
	st, _ := a.readState("pw")
	sites := st.sites()
	if len(sites) != 1 || sites[0].Origin != "https://example.com" || sites[0].Local["token"] != "t1" {
		t.Fatalf("origins mangled: %+v", sites)
	}
	if p := cookieParam(st.Cookies[0]); p.Expires != 0 {
		t.Fatalf("expires -1 must be a session cookie: %+v", p)
	}
}

func TestSitesMergesLegacyAndOrigins(t *testing.T) {
	st := savedState{
		Origin: "https://a.com", Local: kvMap{"k": "v"},
		Origins: []siteStorage{{Origin: "https://a.com", Local: kvMap{"k": "v"}}, {Origin: "https://b.com", Local: kvMap{"x": "y"}}, {Origin: "https://empty.com"}},
	}
	if got := st.sites(); len(got) != 2 {
		t.Fatalf("want 2 sites (deduped, empty dropped), got %+v", got)
	}
	merged := mergeStates(map[string]savedState{"one": st, "two": {Cookies: []stCookie{{Name: "c", Domain: "b.com"}}}})
	if len(merged.Cookies) != 1 || len(merged.sites()) != 2 {
		t.Fatalf("mergeStates: %+v", merged)
	}
}
