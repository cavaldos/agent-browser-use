package main

import (
	"encoding/json"
	"runtime"
	"strings"
	"testing"

	"github.com/chromedp/cdproto/emulation"
)

func TestUAMetadata(t *testing.T) {
	brands := []*emulation.UserAgentBrandVersion{
		{Brand: "Not-A.Brand", Version: "99"}, {Brand: "Google Chrome", Version: "157"}, {Brand: "Chromium", Version: "157"},
	}
	m := uaMetadata("HeadlessChrome/157.0.8103.39", brands, "macOS")
	if m == nil {
		t.Fatal("no metadata")
	}
	if m.FullVersion != "157.0.8103.39" || m.Platform != "macOS" || m.Mobile || m.Bitness == "" {
		t.Errorf("got %+v", m)
	}
	want := map[string]string{"Not-A.Brand": "99.0.0.0", "Google Chrome": "157.0.8103.39", "Chromium": "157.0.8103.39"}
	if len(m.FullVersionList) != 3 {
		t.Fatalf("fullVersionList: %+v", m.FullVersionList)
	}
	for i, b := range m.FullVersionList {
		if b.Brand != brands[i].Brand || b.Version != want[b.Brand] {
			t.Errorf("fullVersionList[%d] = %+v", i, b)
		}
		if m.Brands[i].Version != brands[i].Version { // the low-entropy list keeps major versions
			t.Errorf("brands[%d] = %+v", i, m.Brands[i])
		}
	}
	if arch := map[string]string{"arm64": "arm", "amd64": "x86"}[runtime.GOARCH]; arch != "" && m.Architecture != arch {
		t.Errorf("architecture %q on %s", m.Architecture, runtime.GOARCH)
	}
	// fullVersion must reach Chrome even though cdproto no longer has it.
	j, _ := json.Marshal(userAgentOverrideParams{UserAgent: "x", UserAgentMetadata: m})
	for _, s := range []string{`"fullVersion":"157.0.8103.39"`, `"fullVersionList":`, `"platformVersion":`, `"formFactors":["Desktop"]`} {
		if !strings.Contains(string(j), s) {
			t.Errorf("%s lacks %s", j, s)
		}
	}

	// Nothing reliable to build from: keep Chrome's own hints.
	if uaMetadata("HeadlessChrome/157.0.8103.39", nil, "macOS") != nil || uaMetadata("weird", brands, "macOS") != nil {
		t.Error("expected nil metadata")
	}
}

func TestThreeParts(t *testing.T) {
	for in, want := range map[string]string{"26.2": "26.2.0", "15.6.1": "15.6.1", "6.8.0-45-generic": "6.8.0", "14": "14.0.0", "": "", "x": ""} {
		if got := threeParts(in); got != want {
			t.Errorf("threeParts(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestStealthJSHidesOurGlobals(t *testing.T) {
	for _, s := range []string{"'__tb'", "'__abu'", "getOwnPropertyNames", "Reflect, 'ownKeys'", "[native code]"} {
		if !strings.Contains(stealthJS, s) {
			t.Errorf("stealthJS lacks %s", s)
		}
	}
	// The old fakes contradicted the real browser and got Google sign-in blocked.
	for _, s := range []string{"'webdriver'", "userAgentData", "WebGLRenderingContext", "permissions"} {
		if strings.Contains(stealthJS, s) {
			t.Errorf("stealthJS fakes %s again", s)
		}
	}
}
