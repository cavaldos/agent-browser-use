//go:build !windows

package main

import (
	"os"
	"os/exec"
	"syscall"
)

// detach lets the child outlive this process (own session, no controlling terminal).
func detach(c *exec.Cmd) { c.SysProcAttr = &syscall.SysProcAttr{Setsid: true} }

func processAlive(pid int) bool {
	p, err := os.FindProcess(pid)
	return err == nil && p.Signal(syscall.Signal(0)) == nil
}

func terminateProcess(pid int) {
	if p, err := os.FindProcess(pid); err == nil {
		p.Signal(syscall.SIGTERM)
	}
}

func killProcess(pid int) {
	if p, err := os.FindProcess(pid); err == nil {
		p.Kill()
	}
}
