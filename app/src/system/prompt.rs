//! Asking the person for the password. In `mcp` (stdio) mode stdin/stdout carry MCP messages,
//! so the question goes through a native password dialog, or the terminal as a fallback.
//!
//! What main.rs must call: when a running copy of the server wants a password
//! we do not have (or the wrong one), `let pw = prompt::ask_password(msg);` then
//! `auth::set_password(&pw)`. `msg` is the text shown (the "Agent Browser Use is password
//! protected..." / "Wrong password..." lines). An empty result means cancelled or nothing to ask
//! with, and main then fails with the `PASSWORD_ENV` message.

use std::process::Command;

/// prompt.rs: the native dialog (macOS osascript, Windows PowerShell credential prompt, Linux
/// zenity when installed), then the controlling terminal. Returns "" if the person cancelled or
/// there is no way to ask.
pub fn ask_password(msg: &str) -> String {
    let mut cmd: Option<Command> = None;
    if cfg!(target_os = "macos") {
        // %q quoting of the message is approximated by go_quote (see below).
        let script = format!(
            "text returned of (display dialog {} default answer \"\" with hidden answer with title \"Agent Browser Use\" buttons {{\"Cancel\",\"OK\"}} default button \"OK\")",
            go_quote(msg)
        );
        let mut c = Command::new("osascript");
        c.args(["-e", &script]);
        cmd = Some(c);
    } else if cfg!(windows) {
        let ps = format!(
            "$c = Get-Credential -UserName 'agent-browser-use' -Message '{}'; if ($c) {{ $c.GetNetworkCredential().Password }}",
            msg.replace('\'', "''")
        );
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-Command", &ps]);
        cmd = Some(c);
    } else if look_path("zenity") {
        let mut c = Command::new("zenity");
        c.args([
            "--entry",
            "--hide-text",
            "--title=Agent Browser Use",
            &format!("--text={msg}"),
        ]);
        cmd = Some(c);
    }
    if let Some(mut c) = cmd {
        if let Ok(out) = c.output() {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                return text.trim_end_matches(['\r', '\n']).to_string();
            }
        }
    }
    // Fallback: the controlling terminal. echoes what is typed; rpassword does not echo,
    // which is the deliberate difference here. It still never touches MCP's stdio.
    match rpassword::prompt_password(format!("{msg} ")) {
        Ok(line) => line.trim_end_matches(['\r', '\n']).to_string(),
        Err(_) => String::new(),
    }
}

/// `%q` for a plain string: wraps in double quotes and escapes `\`, `"` and the common
/// control characters. Non-printable Unicode is not escaped (would), which is harmless here.
fn go_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `exec.LookPath` (Unix flavour): is `name` an existing file in one of the PATH folders?
fn look_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_quote_escapes_like_go_for_plain_messages() {
        assert_eq!(go_quote("Enter \"it\""), r#""Enter \"it\"""#);
        assert_eq!(go_quote("a\\b\nc"), r#""a\\b\nc""#);
    }
}
