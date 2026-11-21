//go:build linux

package main

import (
	"os"
	"os/exec"
	"syscall"
)

// chromeCmdOptions keeps what chromedp does by default on Linux, which a ModifyCmdFunc replaces:
// when this program dies, Chrome dies with it.
func chromeCmdOptions(cmd *exec.Cmd) {
	if _, ok := os.LookupEnv("LAMBDA_TASK_ROOT"); ok {
		return
	}
	if cmd.SysProcAttr == nil {
		cmd.SysProcAttr = new(syscall.SysProcAttr)
	}
	cmd.SysProcAttr.Pdeathsig = syscall.SIGKILL
}
