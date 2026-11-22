//! Process helpers. `detach` lets the child outlive this process (own session, no controlling
//! terminal). Unix: setsid; Windows: no-op. Also `process_alive`, `terminate_process`, `kill_process`.
//! The unix and windows versions are kept as #[cfg] blocks.
//!
//! What main.rs / menu.rs call: only menu.rs uses these: `detach` before spawning the
//! server child, `process_alive` / `terminate_process` / `kill_process` to stop a server left in the
//! background by an earlier run. Pids are i32 (int); callers pass pids > 0.

use std::process::Command;

/// proc_unix.rs: `detach(c)` sets `SysProcAttr{Setsid: true}`. Here setsid runs in the child
/// between fork and exec, which is what forkAndExec does.
#[cfg(unix)]
pub fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure only calls setsid(2), which is async-signal-safe, and allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(std::io::Error::from)
        });
    }
}

/// Windows: `detach(c)` is an empty function, so it is empty here too.
#[cfg(windows)]
pub fn detach(_cmd: &mut Command) {}

/// proc_unix.rs `processAlive`: `kill(pid, 0)` succeeds. also reports EPERM as not alive.
#[cfg(unix)]
pub fn process_alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

/// proc_windows.rs `processAlive`: the process can be opened.
#[cfg(windows)]
pub fn process_alive(pid: i32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: plain Win32 call; the returned handle is closed right away.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if h.is_null() {
            return false;
        }
        CloseHandle(h);
        true
    }
}

/// proc_unix.rs `terminateProcess`: SIGTERM.
#[cfg(unix)]
pub fn terminate_process(pid: i32) {
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    );
}

/// proc_windows.rs `terminateProcess`: calls killProcess here, because Windows has no SIGTERM.
#[cfg(windows)]
pub fn terminate_process(pid: i32) {
    kill_process(pid);
}

/// proc_unix.rs `killProcess`: SIGKILL (`Process.Kill`).
#[cfg(unix)]
pub fn kill_process(pid: i32) {
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    );
}

/// proc_windows.rs `killProcess`: TerminateProcess with exit code 1 (what Kill does).
#[cfg(windows)]
pub fn kill_process(pid: i32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    // SAFETY: plain Win32 calls; the handle is closed after use and null-checked.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid as u32);
        if h.is_null() {
            return;
        }
        TerminateProcess(h, 1);
        CloseHandle(h);
    }
}
