//! `agent-browser-use --update`: replace this binary with the latest GitHub release.
//!
//! What main.rs must call (main.rs): when `-update` / `-u` is set,
//! `if let Err(e) = update::self_update.await { eprintln!("Update failed: {e:#}"); exit(1) }`
//! then return. The error texts are kept as the error messages.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::{APP_NAME, VERSION};

/// update.rs: `const releaseRepo`.
const RELEASE_REPO: &str = "cavaldos/agent-browser-use";

/// update.rs `selfUpdate`: download the latest release for this OS/arch and replace the running
/// binary with it. Prints progress lines while it works.
pub async fn self_update() -> Result<()> {
    if cfg!(windows) {
        bail!("on Windows, update with scripts/install.ps1");
    }
    // darwin stays darwin, every other OS is looked up as linux.
    let os_name = if std::env::consts::OS == "macos" {
        "darwin"
    } else {
        "linux"
    };
    let exe = std::env::current_exe().context("locating the running binary")?;
    // filepath.EvalSymlinks(exe); the temp file goes next to the resolved path.
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);

    // /releases/latest redirects to /releases/tag/<tag>. Do not follow the redirect.
    let no_redirect = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let resp = no_redirect
        .head(format!("https://github.com/{RELEASE_REPO}/releases/latest"))
        .send()
        .await?;
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let tag = go_base(location);
    if !tag.starts_with('v') {
        bail!("could not find the latest release");
    }
    if tag == VERSION {
        println!("Already up to date ({VERSION})");
        return Ok(());
    }

    let url = format!(
        "https://github.com/{RELEASE_REPO}/releases/download/{tag}/{APP_NAME}-{os_name}-{}",
        go_arch(std::env::consts::ARCH)
    );
    println!("Downloading {url} ...");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;
    let mut r = client.get(&url).send().await?;
    if r.status().as_u16() != 200 {
        bail!("download failed: {}", r.status());
    }

    let tmp = temp_path(exe.parent().unwrap_or(Path::new(".")));
    let write = async {
        let mut f = std::fs::File::create(&tmp)?;
        while let Some(chunk) = r.chunk().await? {
            f.write_all(&chunk)?;
        }
        f.flush()?;
        drop(f);
        set_executable(&tmp)?;
        // Replaces the running executable (renames over it, or handles Windows' locked file).
        self_replace::self_replace(&tmp)?;
        anyhow::Ok(())
    };
    let result = write.await;
    // defers os.Remove(tmp): a no-op once the file was moved into place.
    let _ = std::fs::remove_file(&tmp);
    result?;
    println!("Updated {VERSION} -> {tag}");
    Ok(())
}

/// update.rs: `os.CreateTemp(dir, "."+appName+"-update-*")`, with a unique suffix.
fn temp_path(dir: &Path) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!(".{APP_NAME}-update-{}-{nanos}", std::process::id()))
}

/// update.rs: `os.Chmod(tmp, 0755)`.
#[cfg(unix)]
fn set_executable(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn set_executable(_p: &Path) -> std::io::Result<()> {
    Ok(())
}

/// `filepath.Base` on a URL path: the last element, ignoring trailing slashes.
/// An empty input gives ".".
fn go_base(s: &str) -> &str {
    if s.is_empty() {
        return ".";
    }
    let t = s.trim_end_matches('/');
    if t.is_empty() {
        return "/";
    }
    t.rsplit('/').next().unwrap_or(t)
}

/// Rust arch names mapped to the GOARCH names that the release assets use.
fn go_arch(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_base_matches_filepath_base() {
        assert_eq!(
            go_base("https://github.com/cavaldos/agent-browser-use/releases/tag/v1.2.3"),
            "v1.2.3"
        );
        assert_eq!(go_base("https://x/y/v2.0.0/"), "v2.0.0");
        assert_eq!(go_base(""), ".");
        assert_eq!(go_base("/"), "/");
    }

    #[test]
    fn go_arch_names() {
        assert_eq!(go_arch("x86_64"), "amd64");
        assert_eq!(go_arch("aarch64"), "arm64");
        assert_eq!(go_arch("riscv64"), "riscv64");
    }
}
