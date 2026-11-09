//go:build windows

package main

import (
	"os"
	"os/exec"
)

func detach(c *exec.Cmd) {}

func processAlive(pid int) bool {
	p, err := os.FindProcess(pid)
	if err != nil {
		return false
	}
	p.Release()
	return true
}

func terminateProcess(pid int) { killProcess(pid) }

func killProcess(pid int) {
	if p, err := os.FindProcess(pid); err == nil {
		p.Kill()
	}
}
