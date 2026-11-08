package main

// Engine picks which browser runs the shared screen. Everything here speaks the
// Chrome DevTools Protocol (screencast, input, audio bindings…), so the choices
// are the Chromium family. chrome-headless-shell is the lightest: the same engine
// without the browser UI, so it starts faster and uses less memory.

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
)

type Engine struct {
	Name  string // chrome, chromium, brave, edge, vivaldi, headless-shell
	Label string // for messages
	Path  string
	Shell bool // chrome-headless-shell: always headless, takes no --headless flag
}

// engineOrder is what "auto" tries, in order: Chromium first, then whatever else is installed.
var engineOrder = []string{"chromium", "chrome", "edge", "brave", "vivaldi", "headless-shell"}

var engineLabels = map[string]string{
	"chrome": "Google Chrome", "chromium": "Chromium", "brave": "Brave", "edge": "Microsoft Edge",
	"vivaldi": "Vivaldi", "headless-shell": "chrome-headless-shell",
}

func engineLocations(name string) []string {
	home, _ := os.UserHomeDir()
	pf, pf86, local := os.Getenv("ProgramFiles"), os.Getenv("ProgramFiles(x86)"), os.Getenv("LOCALAPPDATA")
	win := func(rel string) []string {
		var l []string
		for _, base := range []string{pf, pf86, local} {
			if base != "" {
				l = append(l, filepath.Join(base, rel))
			}
		}
		return l
	}
	switch name {
	case "chrome":
		switch runtime.GOOS {
		case "darwin":
			return []string{"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"}
		case "windows":
			return append([]string{"chrome.exe"}, win(`Google\Chrome\Application\chrome.exe`)...)
		}
		return []string{"google-chrome", "google-chrome-stable"}
	case "chromium":
		switch runtime.GOOS {
		case "darwin":
			return []string{"/Applications/Chromium.app/Contents/MacOS/Chromium"}
		case "windows":
			return win(`Chromium\Application\chrome.exe`)
		}
		return []string{"chromium", "chromium-browser"}
	case "brave":
		switch runtime.GOOS {
		case "darwin":
			return []string{"/Applications/Brave Browser.app/Contents/MacOS/Brave Browser"}
		case "windows":
			return win(`BraveSoftware\Brave-Browser\Application\brave.exe`)
		}
		return []string{"brave-browser", "brave"}
	case "edge":
		switch runtime.GOOS {
		case "darwin":
			return []string{"/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"}
		case "windows":
			return append([]string{"msedge.exe"}, win(`Microsoft\Edge\Application\msedge.exe`)...)
		}
		return []string{"microsoft-edge", "microsoft-edge-stable"}
	case "vivaldi":
		switch runtime.GOOS {
		case "darwin":
			return []string{"/Applications/Vivaldi.app/Contents/MacOS/Vivaldi"}
		case "windows":
			return win(`Vivaldi\Application\vivaldi.exe`)
		}
		return []string{"vivaldi", "vivaldi-stable"}
	case "headless-shell":
		l := []string{"chrome-headless-shell", "headless_shell", "headless-shell"}
		// Where `npx @puppeteer/browsers install chrome-headless-shell@stable` puts it.
		exe := "chrome-headless-shell"
		if runtime.GOOS == "windows" {
			exe += ".exe"
		}
		for _, base := range []string{".", filepath.Join(home, ".cache", "puppeteer")} {
			m, _ := filepath.Glob(filepath.Join(base, "chrome-headless-shell", "*", "chrome-headless-shell-*", exe))
			slices.Sort(m)
			slices.Reverse(m) // newest version first
			l = append(l, m...)
		}
		return l
	}
	return nil
}

func findEngine(name string) string {
	for _, p := range engineLocations(name) {
		if found, err := exec.LookPath(p); err == nil {
			return found
		}
	}
	return ""
}

// resolveEngine turns the -engine and -chrome flags into a browser to start.
func resolveEngine(name, path string) (Engine, error) {
	name = strings.ToLower(strings.TrimSpace(name))
	switch name {
	case "", "auto":
		name = "auto"
	case "shell", "chrome-headless-shell", "headless_shell":
		name = "headless-shell"
	case "msedge":
		name = "edge"
	case "firefox", "webkit", "safari":
		return Engine{}, fmt.Errorf("engine %q is not supported: the shared screen needs the Chrome DevTools Protocol "+
			"(Firefox dropped it). Use one of: auto, %s", name, strings.Join(engineOrder, ", "))
	}
	if path != "" { // an explicit path wins; guess what it is for the label
		if name == "auto" {
			name = "chrome"
			base := strings.ToLower(filepath.Base(path))
			for _, n := range engineOrder {
				if strings.Contains(base, strings.ReplaceAll(n, "-", "")) || strings.Contains(base, n) {
					name = n
				}
			}
			if strings.Contains(base, "msedge") {
				name = "edge"
			}
		}
		return Engine{Name: name, Label: engineLabels[name], Path: path, Shell: name == "headless-shell"}, nil
	}
	names := []string{name}
	if name == "auto" {
		names = engineOrder
	} else if _, ok := engineLabels[name]; !ok {
		return Engine{}, fmt.Errorf("unknown engine %q (use one of: auto, %s)", name, strings.Join(engineOrder, ", "))
	}
	for _, n := range names {
		if p := findEngine(n); p != "" {
			return Engine{Name: n, Label: engineLabels[n], Path: p, Shell: n == "headless-shell"}, nil
		}
	}
	if name == "headless-shell" {
		return Engine{}, errors.New("chrome-headless-shell not found: install it with `npx @puppeteer/browsers install chrome-headless-shell@stable` or pass -chrome <path>")
	}
	if name == "auto" {
		return Engine{}, errors.New("no Chromium-based browser found: install Chrome, Chromium, Edge or Brave, or pass -chrome <path>")
	}
	return Engine{}, fmt.Errorf("%s not found: install it or pass -chrome <path>", engineLabels[name])
}
