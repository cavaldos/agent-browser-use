package main

// Interactive terminal mode. When started from a terminal, the binary runs the real server as a
// child process (log in ~/.agent-browser-use/) and shows a menu: Restart / Run in background /
// Terminate. Ctrl+C terminates. Set AGENT_BROWSER_USE_NO_MENU=1 to run in the foreground instead.

import (
	"bufio"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"
)

const childEnv = "AGENT_BROWSER_USE_CHILD"

func isTTY(f *os.File) bool {
	st, err := f.Stat()
	return err == nil && st.Mode()&os.ModeCharDevice != 0
}

func menuWanted() bool {
	return os.Getenv("AGENT_BROWSER_USE_NO_MENU") == "" && os.Getenv(childEnv) == "" &&
		isTTY(os.Stdin) && isTTY(os.Stdout)
}

func stateDir() string {
	home, err := os.UserHomeDir()
	if err != nil {
		home = os.TempDir()
	}
	dir := filepath.Join(home, "."+appName)
	os.MkdirAll(dir, 0755)
	return dir
}

type supervisor struct {
	args    []string
	log     string
	pidFile string
	cmd     *exec.Cmd
	done    chan struct{}
}

// stopPrevious stops a server left running in the background by an earlier run.
func (s *supervisor) stopPrevious() {
	b, err := os.ReadFile(s.pidFile)
	if err != nil {
		return
	}
	pid, _ := strconv.Atoi(strings.TrimSpace(string(b)))
	os.Remove(s.pidFile)
	if pid > 0 && pid != os.Getpid() && processAlive(pid) {
		terminateProcess(pid)
		waitGone(pid, 5*time.Second)
	}
}

func waitGone(pid int, d time.Duration) {
	for end := time.Now().Add(d); time.Now().Before(end); time.Sleep(100 * time.Millisecond) {
		if !processAlive(pid) {
			return
		}
	}
	killProcess(pid)
	time.Sleep(300 * time.Millisecond)
}

func (s *supervisor) start() error {
	exe, err := os.Executable()
	if err != nil {
		return err
	}
	lf, err := os.Create(s.log)
	if err != nil {
		return err
	}
	defer lf.Close()
	c := exec.Command(exe, s.args...)
	c.Env = append(os.Environ(), childEnv+"=1")
	c.Stdout, c.Stderr = lf, lf
	detach(c)
	if err := c.Start(); err != nil {
		return err
	}
	s.cmd = c
	s.done = make(chan struct{})
	go func() { c.Wait(); close(s.done) }()
	os.WriteFile(s.pidFile, []byte(strconv.Itoa(c.Process.Pid)), 0644)
	return nil
}

func (s *supervisor) stop() {
	if s.cmd == nil {
		return
	}
	terminateProcess(s.cmd.Process.Pid)
	select {
	case <-s.done:
	case <-time.After(5 * time.Second):
		killProcess(s.cmd.Process.Pid)
		<-s.done
	}
	os.Remove(s.pidFile)
	s.cmd = nil
}

// waitReady prints the child's startup output once it is serving (and the tunnel is up, if asked).
func (s *supervisor) waitReady(wantTunnel bool) bool {
	for i := 0; i < 240; i++ {
		b, _ := os.ReadFile(s.log)
		txt := string(b)
		select {
		case <-s.done:
			fmt.Print(txt)
			fmt.Println("The app exited.")
			return false
		default:
		}
		if strings.Contains(txt, "is running") &&
			(!wantTunnel || strings.Contains(txt, "Public URL") || strings.Contains(txt, "Tunnel failed")) {
			fmt.Print(txt)
			fmt.Println()
			return true
		}
		time.Sleep(250 * time.Millisecond)
	}
	b, _ := os.ReadFile(s.log)
	fmt.Print(string(b))
	return true
}

var menuItems = []string{"Restart", "Run in background", "Terminate"}

func stty(args ...string) string {
	c := exec.Command("stty", args...)
	if tty, err := os.Open("/dev/tty"); err == nil {
		defer tty.Close()
		c.Stdin = tty
	} else {
		c.Stdin = os.Stdin
	}
	out, _ := c.Output()
	return strings.TrimSpace(string(out))
}

const (
	keyUp = iota
	keyDown
	keyEnter
	keyQuit
	keyOther
)

// readKey reads one key from r; Esc sequences are parsed for the arrow keys.
func readKey(r *bufio.Reader) int {
	b, err := r.ReadByte()
	if err != nil {
		return keyQuit
	}
	switch b {
	case '\r', '\n', 4:
		return keyEnter
	case 3:
		return keyQuit
	case 'k':
		return keyUp
	case 'j':
		return keyDown
	case 27:
		if r.Buffered() == 0 {
			time.Sleep(30 * time.Millisecond)
		}
		if r.Buffered() == 0 {
			return keyOther
		}
		if n, _ := r.ReadByte(); n != '[' && n != 'O' {
			return keyOther
		}
		switch k, _ := r.ReadByte(); k {
		case 'A':
			return keyUp
		case 'B':
			return keyDown
		}
	}
	return keyOther
}

func drawMenu(sel, port int, redraw bool) {
	if redraw {
		fmt.Printf("\033[%dA", len(menuItems)+1)
	}
	fmt.Printf("\033[2K\033[1m\033[36mServer is running\033[0m \033[2m(port %d)\033[0m\r\n", port)
	for i, l := range menuItems {
		if i == sel {
			fmt.Printf("\033[2K  \033[1m\033[32m❯ %-18s\033[0m\r\n", l)
		} else {
			fmt.Printf("\033[2K    %-18s\r\n", l)
		}
	}
	fmt.Print("\033[2K\033[2m  ↑↓ move · Enter select · Ctrl+C terminate\033[0m")
}

// runMenu is the terminal front end. It returns when the user quits.
func runMenu(args []string, port int, wantTunnel bool) {
	dir := stateDir()
	s := &supervisor{args: args, log: filepath.Join(dir, appName+".log"), pidFile: filepath.Join(dir, "pid")}
	s.stopPrevious()
	if err := s.start(); err != nil {
		fmt.Fprintln(os.Stderr, "Could not start:", err)
		os.Exit(1)
	}
	if !s.waitReady(wantTunnel) {
		os.Exit(1)
	}

	saved := stty("-g")
	if saved == "" { // no usable terminal control (e.g. Windows): just stay in the foreground
		sig := make(chan os.Signal, 1)
		signal.Notify(sig, os.Interrupt, syscall.SIGTERM)
		select {
		case <-sig:
		case <-s.done:
		}
		s.stop()
		return
	}
	restore := func() { stty(saved); fmt.Print("\033[?25h") }
	stty("-echo", "-icanon", "min", "1", "time", "0")
	fmt.Print("\033[?25l")
	defer restore()

	keys := make(chan int)
	go func() {
		r := bufio.NewReader(os.Stdin)
		for {
			k := readKey(r)
			keys <- k
			if k == keyQuit {
				return
			}
		}
	}()
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt, syscall.SIGTERM, syscall.SIGHUP)

	sel, redraw := 0, false
	for {
		drawMenu(sel, port, redraw)
		redraw = true
		var k int
		select {
		case k = <-keys:
		case <-sig:
			k = keyQuit
		case <-s.done: // the server died by itself: leave the menu instead of hanging
			restore()
			fmt.Print("\r\n")
			fmt.Print(readLog(s.log))
			fmt.Println("The app stopped.")
			os.Remove(s.pidFile)
			return
		}
		switch k {
		case keyUp:
			sel = (sel + len(menuItems) - 1) % len(menuItems)
		case keyDown:
			sel = (sel + 1) % len(menuItems)
		case keyQuit:
			fmt.Print("\r\n")
			s.stop()
			fmt.Println("Terminated.")
			return
		case keyEnter:
			fmt.Printf("\033[%dA\033[J", len(menuItems)+1)
			switch sel {
			case 0:
				s.stop()
				if err := s.start(); err != nil {
					restore()
					fmt.Fprintln(os.Stderr, "Could not start:", err)
					os.Exit(1)
				}
				if !s.waitReady(wantTunnel) {
					restore()
					os.Exit(1)
				}
				redraw = false
			case 1:
				fmt.Printf("Running in background (log: %s, pid %d). Stop it by running %s again and choosing Terminate.\n",
					s.log, s.cmd.Process.Pid, appName)
				return
			case 2:
				s.stop()
				fmt.Printf("Terminated the server on port %d.\n", port)
				return
			}
		}
	}
}

func readLog(path string) string {
	b, _ := os.ReadFile(path)
	return string(b)
}
