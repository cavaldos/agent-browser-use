package main

// Asking the person for the password. In `mcp` (stdio) mode stdin/stdout carry MCP messages,
// so the question goes through a native password dialog, or the terminal as a fallback.

import (
	"bufio"
	"fmt"
	"os"
	"os/exec"
	"runtime"
	"strings"
)

const passwordEnv = "AGENT_BROWSER_USE_PASSWORD"

// askPassword returns what the person typed, or "" if they cancelled / no way to ask.
func askPassword(msg string) string {
	var cmd *exec.Cmd
	switch runtime.GOOS {
	case "darwin":
		script := fmt.Sprintf(`text returned of (display dialog %q default answer "" with hidden answer with title "Agent Browser Use" buttons {"Cancel","OK"} default button "OK")`, msg)
		cmd = exec.Command("osascript", "-e", script)
	case "windows":
		ps := fmt.Sprintf(`$c = Get-Credential -UserName 'agent-browser-use' -Message '%s'; if ($c) { $c.GetNetworkCredential().Password }`, strings.ReplaceAll(msg, "'", "''"))
		cmd = exec.Command("powershell", "-NoProfile", "-Command", ps)
	default:
		if _, err := exec.LookPath("zenity"); err == nil {
			cmd = exec.Command("zenity", "--entry", "--hide-text", "--title=Agent Browser Use", "--text="+msg)
		}
	}
	if cmd != nil {
		if out, err := cmd.Output(); err == nil {
			return strings.TrimRight(string(out), "\r\n")
		}
	}
	// Fallback: the controlling terminal (no echo control, but it never touches MCP's stdio).
	if tty, err := os.OpenFile("/dev/tty", os.O_RDWR, 0); err == nil {
		defer tty.Close()
		fmt.Fprint(tty, msg+" ")
		line, _ := bufio.NewReader(tty).ReadString('\n')
		return strings.TrimRight(line, "\r\n")
	}
	return ""
}
