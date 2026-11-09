package main

// `agent-browser-use --update`: replace this binary with the latest GitHub release.

import (
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"time"
)

const releaseRepo = "cavaldos/agent-browser-use"

func selfUpdate() error {
	if runtime.GOOS == "windows" {
		return fmt.Errorf("on Windows, update with scripts/install.ps1")
	}
	osName := runtime.GOOS
	if osName != "darwin" {
		osName = "linux"
	}
	exe, err := os.Executable()
	if err != nil {
		return err
	}
	if p, err := filepath.EvalSymlinks(exe); err == nil {
		exe = p
	}

	// /releases/latest redirects to /releases/tag/<tag>.
	noRedirect := &http.Client{Timeout: 20 * time.Second,
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	resp, err := noRedirect.Head("https://github.com/" + releaseRepo + "/releases/latest")
	if err != nil {
		return err
	}
	resp.Body.Close()
	tag := filepath.Base(resp.Header.Get("Location"))
	if !strings.HasPrefix(tag, "v") {
		return fmt.Errorf("could not find the latest release")
	}
	if tag == version {
		fmt.Printf("Already up to date (%s)\n", version)
		return nil
	}

	url := fmt.Sprintf("https://github.com/%s/releases/download/%s/%s-%s-%s", releaseRepo, tag, appName, osName, runtime.GOARCH)
	fmt.Printf("Downloading %s ...\n", url)
	r, err := (&http.Client{Timeout: 5 * time.Minute}).Get(url)
	if err != nil {
		return err
	}
	defer r.Body.Close()
	if r.StatusCode != 200 {
		return fmt.Errorf("download failed: %s", r.Status)
	}
	tmp, err := os.CreateTemp(filepath.Dir(exe), "."+appName+"-update-*")
	if err != nil {
		return err
	}
	defer os.Remove(tmp.Name())
	if _, err := io.Copy(tmp, r.Body); err != nil {
		tmp.Close()
		return err
	}
	tmp.Close()
	if err := os.Chmod(tmp.Name(), 0755); err != nil {
		return err
	}
	if err := os.Rename(tmp.Name(), exe); err != nil {
		return err
	}
	fmt.Printf("Updated %s -> %s\n", version, tag)
	return nil
}
