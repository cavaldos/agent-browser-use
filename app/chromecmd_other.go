//go:build !linux

package main

import "os/exec"

// chromeCmdOptions: chromedp's default is empty on this platform.
func chromeCmdOptions(cmd *exec.Cmd) {}
