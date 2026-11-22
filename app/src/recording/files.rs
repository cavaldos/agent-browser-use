//! Files that go in and out of the shared browser.
//!   - Downloads: Chrome saves them into the download folder; we track progress so the
//!     agent (browser_downloads) can wait for a file and read it.
//!   - Uploads: browser_file_upload only reads from the upload and download folders, so an
//!     agent cannot hand a page any other file of the machine.

use std::sync::Mutex;
use std::time::SystemTime;

use crate::browser::Browser;
use crate::observe::{human_size, short_url};

#[derive(Debug, Clone)]
pub struct Download {
    pub guid: String,
    pub url: String,
    /// suggested file name
    pub name: String,
    pub path: String,
    /// inProgress, completed, canceled
    pub state: String,
    pub received: f64,
    pub total: f64,
    pub started: SystemTime,
}

/// Chrome's `Browser.downloadWillBegin` / `Browser.downloadProgress`.
pub enum DownloadEvent {
    WillBegin {
        guid: String,
        url: String,
        suggested_filename: String,
    },
    Progress {
        guid: String,
        total_bytes: f64,
        received_bytes: f64,
        state: String,
        file_path: String,
    },
}

#[derive(Default)]
pub struct Downloads {
    list: Mutex<Vec<Download>>,
}

/// filepath.Base
fn go_base(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let t = p.trim_end_matches('/');
    if t.is_empty() {
        return "/".into();
    }
    t.rsplit('/').next().unwrap_or(t).to_string()
}

impl Downloads {
    pub fn new() -> Downloads {
        Downloads::default()
    }

    /// event handles Chrome's download events; the return value is set once when a download completes.
    pub fn event(&self, ev: DownloadEvent, dir: &str) -> Option<Download> {
        let mut list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        match ev {
            DownloadEvent::WillBegin {
                guid,
                url,
                suggested_filename,
            } => {
                list.push(Download {
                    guid,
                    url,
                    name: go_base(&suggested_filename),
                    path: String::new(),
                    state: "inProgress".into(),
                    received: 0.0,
                    total: 0.0,
                    started: SystemTime::now(),
                });
                if list.len() > 100 {
                    list.remove(0);
                }
                None
            }
            DownloadEvent::Progress {
                guid,
                total_bytes,
                received_bytes,
                state,
                file_path,
            } => {
                let dl = list.iter_mut().rev().find(|d| d.guid == guid)?;
                let was = std::mem::replace(&mut dl.state, state);
                dl.received = received_bytes;
                dl.total = total_bytes;
                if !file_path.is_empty() {
                    dl.path = file_path;
                }
                if dl.state == "completed" && was != "completed" {
                    if dl.path.is_empty() {
                        dl.path = crate::go_join(dir, &dl.name);
                    }
                    return Some(dl.clone());
                }
                None
            }
        }
    }

    pub fn snapshot(&self) -> Vec<Download> {
        self.list.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Download {
    /// `download.line(i)`
    pub fn line(&self, i: usize) -> String {
        match self.state.as_str() {
            "completed" => {
                let size = match std::fs::metadata(&self.path) {
                    Ok(m) => format!(", {}", human_size(m.len() as i64)),
                    Err(_) => String::new(),
                };
                format!(
                    "- #{i} {}: done{size} → {}\n  from {}",
                    self.name,
                    self.path,
                    short_url(&self.url)
                )
            }
            "canceled" => format!(
                "- #{i} {}: canceled (from {})",
                self.name,
                short_url(&self.url)
            ),
            _ => {
                let mut prog = human_size(self.received as i64);
                if self.total > 0.0 {
                    prog.push_str(&format!(" of {}", human_size(self.total as i64)));
                }
                format!(
                    "- #{i} {}: downloading ({prog}) from {}",
                    self.name,
                    short_url(&self.url)
                )
            }
        }
    }
}

/// within reports whether p (after resolving links) lies inside root.
pub fn within(root: &str, p: &str) -> bool {
    let (Ok(r), Ok(q)) = (std::fs::canonicalize(root), std::fs::canonicalize(p)) else {
        return false;
    };
    q.starts_with(r)
}

/// uploadPaths checks the paths an agent wants to upload: existing files inside the upload or download folder.
pub fn upload_paths(b: &Browser, paths: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for p in paths {
        let mut p = p.clone();
        let abs = p.starts_with('/') || (cfg!(windows) && p.len() > 1 && p.as_bytes()[1] == b':');
        if !abs {
            // a bare name means a file in the upload folder, then the download folder
            let cand = [
                crate::go_join(&b.opt.upload_dir, &p),
                crate::go_join(&b.opt.download_dir, &p),
            ];
            let Some(c) = cand.iter().find(|c| std::fs::metadata(c).is_ok()) else {
                return Err(format!(
                    "file {p:?} not found in the upload folder {} or the download folder {}",
                    b.opt.upload_dir, b.opt.download_dir
                ));
            };
            p = c.clone();
        }
        match std::fs::metadata(&p) {
            Ok(m) if !m.is_dir() => {}
            _ => return Err(format!("file {p:?} does not exist")),
        }
        if !within(&b.opt.upload_dir, &p) && !within(&b.opt.download_dir, &p) {
            return Err(format!(
                "{p:?} is outside the upload folder ({}) and the download folder ({}). Put the file there first",
                b.opt.upload_dir, b.opt.download_dir
            ));
        }
        out.push(p);
    }
    if out.is_empty() {
        return Err(
            "paths is required: file names in the upload folder, or full paths inside it".into(),
        );
    }
    Ok(out)
}

/// safeName reduces a name to a plain file name.
pub fn safe_name(name: &str) -> String {
    let n = go_base(&name.trim().replace('\\', "/"));
    if n == "." || n == "/" || n == ".." {
        return String::new();
    }
    n
}
