//! Engine picks which browser runs the shared screen. Everything here speaks the
//! Chrome DevTools Protocol (screencast, input, audio bindings…), so the choices
//! are the Chromium family. chrome-headless-shell is the lightest: the same engine
//! without the browser UI, so it starts faster and uses less memory.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Engine {
    /// chrome, chromium, brave, edge, vivaldi, headless-shell
    pub name: String,
    /// for messages
    pub label: String,
    pub path: String,
    /// chrome-headless-shell: always headless, takes no --headless flag
    pub shell: bool,
}

/// engineOrder is what "auto" tries, in order: Chromium first, then whatever else is installed.
pub const ENGINE_ORDER: [&str; 6] = [
    "chromium",
    "chrome",
    "edge",
    "brave",
    "vivaldi",
    "headless-shell",
];

fn engine_label(name: &str) -> Option<&'static str> {
    Some(match name {
        "chrome" => "Google Chrome",
        "chromium" => "Chromium",
        "brave" => "Brave",
        "edge" => "Microsoft Edge",
        "vivaldi" => "Vivaldi",
        "headless-shell" => "chrome-headless-shell",
        _ => return None,
    })
}

fn engine_locations(name: &str) -> Vec<String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let (pf, pf86, local) = (
        env("ProgramFiles"),
        env("ProgramFiles(x86)"),
        env("LOCALAPPDATA"),
    );
    let win = |rel: &str| -> Vec<String> {
        [&pf, &pf86, &local]
            .into_iter()
            .filter(|b| !b.is_empty())
            .map(|b| format!("{b}\\{rel}"))
            .collect()
    };
    let os = std::env::consts::OS;
    let s = |l: &[&str]| l.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    match name {
        "chrome" => match os {
            "macos" => s(&["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"]),
            "windows" => [
                s(&["chrome.exe"]),
                win(r"Google\Chrome\Application\chrome.exe"),
            ]
            .concat(),
            _ => s(&["google-chrome", "google-chrome-stable"]),
        },
        "chromium" => match os {
            "macos" => s(&["/Applications/Chromium.app/Contents/MacOS/Chromium"]),
            "windows" => win(r"Chromium\Application\chrome.exe"),
            _ => s(&["chromium", "chromium-browser"]),
        },
        "brave" => match os {
            "macos" => s(&["/Applications/Brave Browser.app/Contents/MacOS/Brave Browser"]),
            "windows" => win(r"BraveSoftware\Brave-Browser\Application\brave.exe"),
            _ => s(&["brave-browser", "brave"]),
        },
        "edge" => match os {
            "macos" => s(&["/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"]),
            "windows" => [
                s(&["msedge.exe"]),
                win(r"Microsoft\Edge\Application\msedge.exe"),
            ]
            .concat(),
            _ => s(&["microsoft-edge", "microsoft-edge-stable"]),
        },
        "vivaldi" => match os {
            "macos" => s(&["/Applications/Vivaldi.app/Contents/MacOS/Vivaldi"]),
            "windows" => win(r"Vivaldi\Application\vivaldi.exe"),
            _ => s(&["vivaldi", "vivaldi-stable"]),
        },
        "headless-shell" => {
            let mut l = s(&["chrome-headless-shell", "headless_shell", "headless-shell"]);
            // Where `npx @puppeteer/browsers install chrome-headless-shell@stable` puts it.
            let exe = if os == "windows" {
                "chrome-headless-shell.exe"
            } else {
                "chrome-headless-shell"
            };
            let cache = Path::new(&home).join(".cache").join("puppeteer");
            for base in [PathBuf::from("."), cache] {
                let mut m = glob_shell(&base, exe);
                m.sort();
                m.reverse(); // newest version first
                l.extend(m);
            }
            l
        }
        _ => Vec::new(),
    }
}

/// filepath.Glob(base/chrome-headless-shell/*/chrome-headless-shell-*/exe): paths that exist.
fn glob_shell(base: &Path, exe: &str) -> Vec<String> {
    let mut out = Vec::new();
    let root = base.join("chrome-headless-shell");
    let Ok(vers) = std::fs::read_dir(&root) else {
        return out;
    };
    for v in vers.flatten() {
        let Ok(subs) = std::fs::read_dir(v.path()) else {
            continue;
        };
        for sub in subs.flatten() {
            if !sub
                .file_name()
                .to_string_lossy()
                .starts_with("chrome-headless-shell-")
            {
                continue;
            }
            let p = sub.path().join(exe);
            if p.exists() {
                // filepath.Join(".", …) has no "./" prefix
                let s = p.to_string_lossy().into_owned();
                out.push(s.strip_prefix("./").map(str::to_string).unwrap_or(s));
            }
        }
    }
    out
}

/// exec.LookPath: a name with a path separator is checked as is, anything else is searched in PATH.
pub fn look_path(file: &str) -> Option<String> {
    fn is_exec(p: &Path) -> bool {
        let Ok(m) = std::fs::metadata(p) else {
            return false;
        };
        if m.is_dir() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            m.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    }
    let candidates = |p: PathBuf| -> Vec<PathBuf> {
        if cfg!(windows) && p.extension().is_none() {
            vec![p.with_extension("exe"), p.with_extension("cmd"), p]
        } else {
            vec![p]
        }
    };
    if file.contains('/') || (cfg!(windows) && file.contains('\\')) {
        return candidates(PathBuf::from(file))
            .into_iter()
            .find(|p| is_exec(p))
            .map(|p| p.to_string_lossy().into_owned());
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        for p in candidates(dir.join(file)) {
            if is_exec(&p) {
                return Some(p.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn find_engine(name: &str) -> String {
    for p in engine_locations(name) {
        if let Some(found) = look_path(&p) {
            return found;
        }
    }
    String::new()
}

/// resolveEngine turns the -engine and -chrome flags into a browser to start.
pub fn resolve_engine(name: &str, path: &str) -> Result<Engine, String> {
    let mut name = name.trim().to_lowercase();
    match name.as_str() {
        "" | "auto" => name = "auto".into(),
        "shell" | "chrome-headless-shell" | "headless_shell" => name = "headless-shell".into(),
        "msedge" => name = "edge".into(),
        "firefox" | "webkit" | "safari" => {
            return Err(format!(
                "engine {name:?} is not supported: the shared screen needs the Chrome DevTools Protocol \
                 (Firefox dropped it). Use one of: auto, {}",
                ENGINE_ORDER.join(", ")
            ));
        }
        _ => {}
    }
    if !path.is_empty() {
        // an explicit path wins; guess what it is for the label
        if name == "auto" {
            name = "chrome".into();
            let base = Path::new(path)
                .file_name()
                .map(|b| b.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            for n in ENGINE_ORDER {
                if base.contains(&n.replace('-', "")) || base.contains(n) {
                    name = n.to_string();
                }
            }
            if base.contains("msedge") {
                name = "edge".into();
            }
        }
        return Ok(Engine {
            label: engine_label(&name).unwrap_or("").to_string(),
            shell: name == "headless-shell",
            name,
            path: path.to_string(),
        });
    }
    let names: Vec<String> = if name == "auto" {
        ENGINE_ORDER.iter().map(|s| s.to_string()).collect()
    } else if engine_label(&name).is_none() {
        return Err(format!(
            "unknown engine {name:?} (use one of: auto, {})",
            ENGINE_ORDER.join(", ")
        ));
    } else {
        vec![name.clone()]
    };
    for n in &names {
        let p = find_engine(n);
        if !p.is_empty() {
            return Ok(Engine {
                name: n.clone(),
                label: engine_label(n).unwrap_or("").to_string(),
                path: p,
                shell: n == "headless-shell",
            });
        }
    }
    if name == "headless-shell" {
        return Err("chrome-headless-shell not found: install it with `npx @puppeteer/browsers install chrome-headless-shell@stable` or pass -chrome <path>".into());
    }
    if name == "auto" {
        return Err("no Chromium-based browser found: install Chrome, Chromium, Edge or Brave, or pass -chrome <path>".into());
    }
    Err(format!(
        "{} not found: install it or pass -chrome <path>",
        engine_label(&name).unwrap_or("")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_path_guesses_the_engine() {
        let e = resolve_engine("auto", "/x/Brave Browser").unwrap();
        assert_eq!((e.name.as_str(), e.label.as_str()), ("brave", "Brave"));
        let e = resolve_engine("", "/opt/chrome-headless-shell").unwrap();
        assert!(e.shell && e.name == "headless-shell");
        let e = resolve_engine("shell", "/x/y").unwrap();
        assert_eq!(e.name, "headless-shell");
        assert_eq!(
            resolve_engine("auto", "/usr/bin/msedge").unwrap().name,
            "edge"
        );
        assert_eq!(
            resolve_engine("auto", "/usr/bin/foo").unwrap().name,
            "chrome"
        );
    }

    #[test]
    fn unsupported_and_unknown_engines() {
        let e = resolve_engine("Firefox", "").unwrap_err();
        assert!(e.starts_with("engine \"firefox\" is not supported"), "{e}");
        assert!(
            e.ends_with("Use one of: auto, chromium, chrome, edge, brave, vivaldi, headless-shell")
        );
        assert_eq!(
            resolve_engine("nope", "").unwrap_err(),
            "unknown engine \"nope\" (use one of: auto, chromium, chrome, edge, brave, vivaldi, headless-shell)"
        );
    }
}
