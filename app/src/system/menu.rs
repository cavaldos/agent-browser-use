//! Interactive terminal mode. When started from a terminal, the binary runs the real server as a
//! child process (log in ~/.agent-browser-use/) and shows a menu: Restart / Run in background /
//! Terminate. Ctrl+C terminates. Set AGENT_BROWSER_USE_NO_MENU=1 to run in the foreground instead.
//!
//! What main.rs must call (in main.rs):
//! - `menu_wanted`: `if !stdio && menu::menu_wanted { ... }`.
//! - `state_dir(home_flag, &argv)`: `stateDir`. `argv` is the full process argv (os.Args,
//!   including the program name). Also used by main for the home folder (main.rs).
//! - `run_menu(&args, port, tunnel_on, &dir)`. `args` are the
//!   arguments (without the program name) that the child server gets, and `dir` is the value of `state_dir`. It returns an
//!   exit code: 0 means "return from main", any other value means main must call
//!   `std::process::exit(code)` (exit(1) in the error paths).

use std::fs;
use std::io::{self, BufReader, Read, Stdin, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use signal_hook::consts::SIGHUP;
use signal_hook::consts::{SIGINT, SIGTERM};

use crate::proc::{detach, kill_process, process_alive, terminate_process};
use crate::APP_NAME;

/// menu.rs: `const childEnv`.
const CHILD_ENV: &str = "AGENT_BROWSER_USE_CHILD";

/// menu.rs `menuItems`.
const MENU_ITEMS: [&str; 3] = ["Restart", "Run in background", "Terminate"];

/// menu.rs `menuWanted`. Stdin and stdout must both be terminals, and neither the no-menu env var
/// nor the child marker may be set. (checks ModeCharDevice; `is_terminal` is the stricter check.)
pub fn menu_wanted() -> bool {
    env_is_empty("AGENT_BROWSER_USE_NO_MENU")
        && env_is_empty(CHILD_ENV)
        && io::IsTerminal::is_terminal(&io::stdin())
        && io::IsTerminal::is_terminal(&io::stdout())
}

fn env_is_empty(key: &str) -> bool {
    std::env::var(key).unwrap_or_default().is_empty()
}

/// menu.rs `stateDir`. `home_flag` is the `-home` value (homeFlag). `argv` is the full argv.
/// The folder is made absolute and created.
pub fn state_dir(home_flag: &str, argv: &[String]) -> PathBuf {
    let mut dir = home_flag.to_string();
    if dir.is_empty() {
        dir = std::env::var("AGENT_BROWSER_USE_HOME").unwrap_or_default();
    }
    // Default: one folder under ~/.config, except for the `mcp` command.
    if dir.is_empty() && !(argv.len() > 1 && argv[1] == "mcp") {
        let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let home = std::env::var_os(home_var)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        dir = home
            .join(".config")
            .join(APP_NAME)
            .to_string_lossy()
            .into_owned();
    }
    if dir.is_empty() {
        // stdio mode: an MCP client starts us from any folder.
        let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let home = std::env::var_os(home_var)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        dir = home
            .join(format!(".{APP_NAME}"))
            .to_string_lossy()
            .into_owned();
    }
    let abs = std::path::absolute(&dir).unwrap_or_else(|_| PathBuf::from(&dir));
    let _ = fs::create_dir_all(&abs);
    abs
}

/// Events the menu loop waits on. used one channel per source (keys, signals, child done).
enum Event {
    Key(Key),
    Signal,
    /// The child with this generation exited.
    Exited(u64),
}

/// Key reading: `key_up` ... `key_other`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Key {
    Up,
    Down,
    Enter,
    Quit,
    Other,
}

/// menu.rs `supervisor`. `child` is `cmd`; `exited` is `done` channel.
struct Supervisor {
    args: Vec<String>,
    log: PathBuf,
    pid_file: PathBuf,
    child: Option<ChildHandle>,
    gen: u64,
    events: Sender<Event>,
}

struct ChildHandle {
    pid: u32,
    gen: u64,
    exited: Arc<AtomicBool>,
}

impl Supervisor {
    /// menu.rs `stopPrevious`: stop a server left running in the background by an earlier run.
    fn stop_previous(&self) {
        let Ok(text) = fs::read_to_string(&self.pid_file) else {
            return;
        };
        let pid: i32 = text.trim().parse().unwrap_or(0);
        let _ = fs::remove_file(&self.pid_file);
        if pid > 0 && pid as u32 != std::process::id() && process_alive(pid) {
            terminate_process(pid);
            wait_gone(pid, Duration::from_secs(5));
        }
    }

    /// menu.rs `start`: run `<this exe> <args>` with the log file as stdout/stderr, detached.
    fn start(&mut self) -> io::Result<()> {
        let exe = std::env::current_exe()?;
        let lf = fs::File::create(&self.log)?;
        let lf2 = lf.try_clone()?;
        let mut c = Command::new(exe);
        c.args(&self.args)
            .env(CHILD_ENV, "1")
            .stdout(Stdio::from(lf))
            .stderr(Stdio::from(lf2));
        detach(&mut c);
        let child = c.spawn()?;
        let pid = child.id();
        self.gen += 1;
        let gen = self.gen;
        let exited = Arc::new(AtomicBool::new(false));
        let (flag, tx) = (exited.clone(), self.events.clone());
        thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
            flag.store(true, Ordering::SeqCst);
            let _ = tx.send(Event::Exited(gen));
        });
        self.child = Some(ChildHandle { pid, gen, exited });
        let _ = fs::write(&self.pid_file, pid.to_string());
        Ok(())
    }

    /// menu.rs `stop`: SIGTERM, wait up to 5 s, then kill and wait for the exit.
    fn stop(&mut self) {
        let Some(h) = self.child.take() else {
            return;
        };
        let pid = h.pid as i32;
        terminate_process(pid);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !h.exited.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                kill_process(pid);
                while !h.exited.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(20));
                }
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = fs::remove_file(&self.pid_file);
    }

    /// `case <-s.done` (non-blocking): has the current child exited?
    fn exited(&self) -> bool {
        self.child
            .as_ref()
            .is_some_and(|c| c.exited.load(Ordering::SeqCst))
    }

    fn is_current(&self, gen: u64) -> bool {
        self.child.as_ref().is_some_and(|c| c.gen == gen)
    }

    /// `wait_ready`: print the child's startup output once it is serving (and the tunnel is
    /// up, if asked). Returns false if the child exited first.
    fn wait_ready(&self, want_tunnel: bool) -> bool {
        for _ in 0..240 {
            let txt = read_log(&self.log);
            if self.exited() {
                say(&txt);
                println!("The app exited.");
                return false;
            }
            if txt.contains("is running")
                && (!want_tunnel || txt.contains("Public URL") || txt.contains("Tunnel failed"))
            {
                say(&txt);
                println!();
                return true;
            }
            thread::sleep(Duration::from_millis(250));
        }
        say(&read_log(&self.log));
        true
    }
}

/// `wait_gone`: poll until the pid is gone (5 s budget), then kill it.
fn wait_gone(pid: i32, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if !process_alive(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    kill_process(pid);
    thread::sleep(Duration::from_millis(300));
}

/// menu.rs `stty(args...)`: run stty against the terminal and return its trimmed stdout.
fn stty(args: &[&str]) -> String {
    let mut c = Command::new("stty");
    c.args(args);
    match fs::File::open("/dev/tty") {
        Ok(tty) => {
            c.stdin(Stdio::from(tty));
        }
        Err(_) => {
            c.stdin(Stdio::inherit());
        }
    }
    match c.output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Err(_) => String::new(),
    }
}

/// menu.rs `readKey`: read one key from `r`. Esc sequences are parsed for the arrow keys.
fn read_key(r: &mut BufReader<Stdin>) -> Key {
    let Some(b) = read_byte(r) else {
        return Key::Quit;
    };
    match b {
        b'\r' | b'\n' | 4 => Key::Enter,
        3 => Key::Quit,
        b'k' => Key::Up,
        b'j' => Key::Down,
        27 => {
            if r.buffer().is_empty() {
                thread::sleep(Duration::from_millis(30));
            }
            if r.buffer().is_empty() {
                return Key::Other;
            }
            match read_byte(r) {
                Some(b'[') | Some(b'O') => {}
                _ => return Key::Other,
            }
            match read_byte(r) {
                Some(b'A') => Key::Up,
                Some(b'B') => Key::Down,
                _ => Key::Other,
            }
        }
        _ => Key::Other,
    }
}

fn read_byte(r: &mut BufReader<Stdin>) -> Option<u8> {
    let mut b = [0u8; 1];
    match r.read(&mut b) {
        Ok(1) => Some(b[0]),
        _ => None,
    }
}

/// menu.rs `drawMenu`. `redraw` moves the cursor back up over the previous menu first.
fn draw_menu(sel: usize, port: u16, redraw: bool) {
    if redraw {
        print!("\x1b[{}A", MENU_ITEMS.len() + 1);
    }
    print!("\x1b[2K\x1b[1m\x1b[36mServer is running\x1b[0m \x1b[2m(port {port})\x1b[0m\r\n");
    for (i, l) in MENU_ITEMS.iter().enumerate() {
        if i == sel {
            print!("\x1b[2K  \x1b[1m\x1b[32m❯ {:<18}\x1b[0m\r\n", l);
        } else {
            print!("\x1b[2K    {:<18}\r\n", l);
        }
    }
    say("\x1b[2K\x1b[2m  ↑↓ move · Enter select · Ctrl+C terminate\x1b[0m");
}

/// Print without a newline and flush, like fmt.Print on an unbuffered stdout.
fn say(s: &str) {
    print!("{s}");
    let _ = io::stdout().flush();
}

/// menu.rs `readLog`.
fn read_log(path: &Path) -> String {
    fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// Forward OS signals into the event channel (`signal.Notify`). Uses signal-hook's flag API,
/// which works on every platform; a small poller turns the flags into events.
fn forward_signals(tx: Sender<Event>, sigs: &[i32]) {
    let flags: Vec<Arc<AtomicBool>> = sigs
        .iter()
        .filter_map(|&sig| {
            let flag = Arc::new(AtomicBool::new(false));
            signal_hook::flag::register(sig, flag.clone())
                .ok()
                .map(|_| flag)
        })
        .collect();
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(50));
        for flag in &flags {
            if flag.swap(false, Ordering::SeqCst) && tx.send(Event::Signal).is_err() {
                return;
            }
        }
    });
}

/// Restores the terminal (echo, canonical mode, cursor) when dropped. Calls `restore` on
/// every return path and before its os.Exit calls; a Drop guard covers the normal returns.
struct Terminal {
    saved: String,
}

impl Terminal {
    fn restore(&self) {
        stty(&[self.saved.as_str()]);
        say("\x1b[?25h");
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.restore();
    }
}

/// menu.rs `runMenu`: the terminal front end. Returns when the user quits; see the module docs for
/// the exit code.
pub fn run_menu(args: &[String], port: u16, want_tunnel: bool, dir: &Path) -> i32 {
    let (tx, rx) = mpsc::channel::<Event>();
    let mut s = Supervisor {
        args: args.to_vec(),
        log: dir.join(format!("{APP_NAME}.log")),
        pid_file: dir.join("pid"),
        child: None,
        gen: 0,
        events: tx.clone(),
    };
    s.stop_previous();
    if let Err(e) = s.start() {
        eprintln!("Could not start: {e}");
        return 1;
    }
    if !s.wait_ready(want_tunnel) {
        return 1;
    }

    let saved = stty(&["-g"]);
    if saved.is_empty() {
        // No usable terminal control (e.g. Windows): just stay in the foreground.
        forward_signals(tx.clone(), &[SIGINT, SIGTERM]);
        loop {
            match rx.recv() {
                Ok(Event::Signal) => break,
                Ok(Event::Exited(g)) if s.is_current(g) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        s.stop();
        return 0;
    }
    let term = Terminal { saved };
    stty(&["-echo", "-icanon", "min", "1", "time", "0"]);
    say("\x1b[?25l");

    let key_tx = tx.clone();
    thread::spawn(move || {
        let mut r = BufReader::new(io::stdin());
        loop {
            let k = read_key(&mut r);
            if key_tx.send(Event::Key(k)).is_err() || k == Key::Quit {
                return;
            }
        }
    });
    #[cfg(unix)]
    forward_signals(tx.clone(), &[SIGINT, SIGTERM, SIGHUP]);
    #[cfg(not(unix))]
    forward_signals(tx.clone(), &[SIGINT, SIGTERM]);

    let mut sel = 0usize;
    let mut redraw = false;
    loop {
        draw_menu(sel, port, redraw);
        redraw = true;
        let key = match rx.recv() {
            Ok(Event::Key(k)) => k,
            Ok(Event::Signal) => Key::Quit,
            Ok(Event::Exited(g)) => {
                if !s.is_current(g) {
                    continue;
                }
                // The server died by itself: leave the menu instead of hanging.
                term.restore();
                say("\r\n");
                say(&read_log(&s.log));
                println!("The app stopped.");
                let _ = fs::remove_file(&s.pid_file);
                return 0;
            }
            Err(_) => return 0,
        };
        match key {
            Key::Up => sel = (sel + MENU_ITEMS.len() - 1) % MENU_ITEMS.len(),
            Key::Down => sel = (sel + 1) % MENU_ITEMS.len(),
            Key::Quit => {
                say("\r\n");
                s.stop();
                println!("Terminated.");
                return 0;
            }
            Key::Enter => {
                print!("\x1b[{}A\x1b[J", MENU_ITEMS.len() + 1);
                let _ = io::stdout().flush();
                match sel {
                    0 => {
                        s.stop();
                        if let Err(e) = s.start() {
                            term.restore();
                            eprintln!("Could not start: {e}");
                            return 1;
                        }
                        if !s.wait_ready(want_tunnel) {
                            term.restore();
                            return 1;
                        }
                        redraw = false;
                    }
                    1 => {
                        let pid = s.child.as_ref().map_or(0, |c| c.pid);
                        println!(
                            "Running in background (log: {}, pid {}). Stop it by running {} again and choosing Terminate.",
                            s.log.display(),
                            pid,
                            APP_NAME
                        );
                        return 0;
                    }
                    _ => {
                        s.stop();
                        println!("Terminated the server on port {port}.");
                        return 0;
                    }
                }
            }
            Key::Other => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_uses_the_home_flag_and_creates_it() {
        let base = std::env::temp_dir().join(format!("abu-menu-test-{}", std::process::id()));
        let dir = base.join("home");
        let got = state_dir(dir.to_str().unwrap(), &["agent-browser-use".to_string()]);
        assert_eq!(got, dir);
        assert!(dir.is_dir());
        let _ = fs::remove_dir_all(&base);
    }
}
