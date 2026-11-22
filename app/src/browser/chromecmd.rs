//! chrome_cmd_options: the process settings applied on Linux, which replace the defaults:
//! when this program dies, Chrome dies with it (Pdeathsig SIGKILL, unless LAMBDA_TASK_ROOT is set).
//! On other platforms there are no extra settings.
//!
//! (chromecmd_linux.rs and chromecmd_other.rs, as two `cfg` blocks.)
//!
//! Pdeathsig fires when the *thread* that forked the child exits, not the process (the process is started
//! on a thread that lives for the whole run). Here Chrome is always spawned from the main thread or a
//! tokio worker thread, and both live as long as the runtime, i.e. as long as this program.

#[cfg(target_os = "linux")]
pub fn chrome_cmd_options(cmd: &mut tokio::process::Command) {
    if std::env::var_os("LAMBDA_TASK_ROOT").is_some() {
        return;
    }
    // SAFETY: prctl(2) is async-signal-safe and allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
pub fn chrome_cmd_options(_cmd: &mut tokio::process::Command) {}
