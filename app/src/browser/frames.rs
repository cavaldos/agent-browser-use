//! Cross-origin iframes.
//!
//! A page from another site (a Stripe checkout form, an embedded widget) has no
//! contentDocument from the top page, so the page helper (agent.js) can't see
//! inside it: no refs, no values, no select options.
//!
//! Reading it works the way Playwright does it: Page.getFrameTree lists every
//! frame, cross-origin included, and Page.createIsolatedWorld gives each frame an
//! execution context we can evaluate in. GrantUniversalAccess lifts the same-origin
//! check inside that world, so the iframe's DOM is readable even though the page
//! is not ours. Nothing about Chrome's process model has to change.
//!
//! Refs from a frame are prefixed with "f<N>" (f1e12 = ref e12 in iframe 1), so a
//! single snapshot covers the page and every iframe in it.
//!
//! # Public API (what agent.rs / tools.rs use)
//!
//! All functions take the `&Tab` they act on and return `Result<_, String>`.
//!   * `parse_frame_ref(ref) -> FrameRef { frame, rest }`: "f1e12" -> {1, "e12"}; plain "e12" is frame 0.
//!   * `child_frames(t) -> (Vec<ChildFrame>, main_frame_id)`: every sub-frame, depth-first; the
//!     1-based position is the N of f<N>.
//!   * `eval_frame_json::<T>(t, n, expr)`: `evalFrameJSON`, expr runs in frame n's isolated world.
//!   * `eval_in_frame_as::<T>(t, n, expr)`: `evalInFrameAs`, the page helper (agent.js) is installed first,
//!     so `__abu.*` works with that frame's own refs.
//!   * `eval_main_json::<T>(t, expr)`: `evalMainJSON`, expr in the main frame (promises awaited).
//!   * `frame_docs(t, main_body) -> Vec<FrameDoc>` and `join_frame_outlines(&docs) -> String`: one outline
//!     covering the page and every iframe (`prefix_frame_refs` rewrites "e12" to "fNe12").
//!   * `frame_origin(t, n) -> IframeOffset`: where iframe n sits on the page (to place clicks).
//!   * `pick_option_expr(ref, values) -> String`: the expression that drives a dropdown inside a frame
//!     (run it with `eval_in_frame_as`).
//!   * `js_error(details)`, `jsq(s)`: the agent.rs helpers these functions need (kept here so this module
//!     does not depend on agent.rs, which owns the same helpers for its own use).
//!
//! `Agent.frameDocs(t, mainBody)` is the free function `frame_docs(t, main_body)` here.

use std::sync::{LazyLock, Once};

use regex::Regex;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::browser::Tab;
use crate::golog;

/// The embedded page helper (see agent.rs).
fn agent_js() -> &'static str {
    crate::agent::agent_js()
}

/// agent.rs `jsq`: a JSON string literal.
pub fn jsq(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// agent.rs `jsError`: the page's exception as a plain message (stack and "Error: " dropped).
pub fn js_error(details: &Value) -> String {
    let msg = details
        .pointer("/exception/description")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
        .or_else(|| details.get("text").and_then(Value::as_str))
        .unwrap_or("exception");
    let msg = msg.split("\n    at ").next().unwrap_or(msg);
    msg.strip_prefix("Error: ").unwrap_or(msg).to_string()
}

/// frameRef is a parsed ref: which frame it lives in, and the ref inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameRef {
    /// 0 = the main frame, 1.. = iframe 1.. as listed
    pub frame: usize,
    /// the ref inside that frame ("e12"); for frame 0 the whole ref
    pub rest: String,
}

/// parseFrameRef splits "f1e12" into {1, "e12"}. A plain "e12" means the main frame.
pub fn parse_frame_ref(r: &str) -> FrameRef {
    let b = r.as_bytes();
    if b.len() > 2 && b[0] == b'f' {
        let mut i = 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i > 1 && i < b.len() {
            if let Ok(n) = r[1..i].parse::<usize>() {
                if n > 0 {
                    return FrameRef {
                        frame: n,
                        rest: r[i..].to_string(),
                    };
                }
            }
        }
    }
    FrameRef {
        frame: 0,
        rest: r.to_string(),
    }
}

/// childFrame is one iframe of the page: its id, its URL, and the iframe element
/// that hosts it in the parent document (used to place clicks).
#[derive(Debug, Clone)]
pub struct ChildFrame {
    pub frame_id: String,
    pub url: String,
    /// the frame that embeds it; for a top-level iframe that is the main frame.
    pub parent: String,
}

fn walk(n: &Value, parent: &str, main: &str, out: &mut Vec<ChildFrame>) {
    let Some(f) = n.get("frame").filter(|f| f.is_object()) else {
        return;
    };
    let id = f["id"].as_str().unwrap_or_default();
    if id != main {
        out.push(ChildFrame {
            frame_id: id.to_string(),
            url: f["url"].as_str().unwrap_or_default().to_string(),
            parent: parent.to_string(),
        });
    }
    if let Some(cs) = n.get("childFrames").and_then(Value::as_array) {
        for c in cs {
            walk(c, id, main, out);
        }
    }
}

/// childFrames lists every sub-frame of the tab's main frame, in document order,
/// flattened depth-first. The 1-based position is the N in f<N>.
pub async fn child_frames(t: &Tab) -> Result<(Vec<ChildFrame>, String), String> {
    // Page.enable makes the frame tree live: without it a frame added by script
    // after load never shows up as a child.
    t.call("Page.enable", json!({}), None)
        .await
        .map_err(|e| e.to_string())?;
    let res = t
        .call("Page.getFrameTree", json!({}), None)
        .await
        .map_err(|e| e.to_string())?;
    let tree = &res["frameTree"];
    let main = tree["frame"]["id"].as_str().unwrap_or_default().to_string();
    let mut out = Vec::new();
    walk(tree, &main, &main, &mut out);
    Ok((out, main))
}

/// frameWorld creates an isolated world inside frame id and returns its execution
/// context, so JavaScript runs against that frame's DOM.
pub async fn frame_world(t: &Tab, id: &str) -> Result<i64, String> {
    let res = t
        .call(
            "Page.createIsolatedWorld",
            json!({"frameId": id, "worldName": "__abu_frame", "grantUniveralAccess": true}),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
    // Chrome spells the parameter grantUniveralAccess (sic); other CDP clients send that too
    Ok(res["executionContextId"].as_i64().unwrap_or(0))
}

/// evalInWorld evaluates expr in an isolated world of a frame, returning the result as JSON.
pub async fn eval_in_world(t: &Tab, world: i64, expr: &str) -> Result<Value, String> {
    let res = t
        .call(
            "Runtime.evaluate",
            json!({"expression": expr, "contextId": world, "awaitPromise": true, "returnByValue": true}),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
    if let Some(d) = res.get("exceptionDetails").filter(|d| !d.is_null()) {
        return Err(js_error(d));
    }
    Ok(res.pointer("/result/value").cloned().unwrap_or(Value::Null))
}

/// `json.Unmarshal(raw, &zero)`: null leaves the zero value.
fn decode<T: DeserializeOwned + Default>(v: Value, n: usize) -> Result<T, String> {
    if v.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(v).map_err(|e| format!("decode iframe f{n} result: {e}"))
}

fn not_found(n: usize, have: usize) -> String {
    format!("iframe f{n} not found: the page has {have} iframe(s). Take a new snapshot (browser_snapshot) and use a ref from it")
}

/// evalFrameJSON evaluates expr in frame n's world and decodes the JSON result.
pub async fn eval_frame_json<T: DeserializeOwned + Default>(
    t: &Tab,
    n: usize,
    expr: &str,
) -> Result<T, String> {
    let (frames, _) = child_frames(t).await?;
    if n < 1 || n > frames.len() {
        return Err(not_found(n, frames.len()));
    }
    let world = frame_world(t, &frames[n - 1].frame_id)
        .await
        .map_err(|e| format!("open iframe f{n}: {e}"))?;
    decode(eval_in_world(t, world, expr).await?, n)
}

/// frameDoc is one frame's snapshot.
#[derive(Debug, Clone, Default)]
pub struct FrameDoc {
    pub n: usize,
    pub url: String,
    pub title: String,
    pub body: String,
    pub err: String,
}

#[derive(Debug, Default, Deserialize)]
struct FrameSnapOut {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
}

/// frameSnapExpr installs the page helper in the frame's world and snapshots it.
fn frame_snap_expr() -> String {
    format!(
        "{}\n;(async () => ({{ url: location.href, title: document.title, body: __abu.snapshot() }}))()",
        agent_js()
    )
}

/// framesWarned keeps the site-isolation warning to once per process: it is a
/// startup-configuration problem, not something that changes per page.
static FRAMES_WARNED: Once = Once::new();

/// frameDocs snapshots every iframe of the tab. The main frame's body is passed
/// in by the caller (it already has it).
pub async fn frame_docs(t: &Tab, main_body: &str) -> Vec<FrameDoc> {
    let mut docs = vec![FrameDoc {
        n: 0,
        body: main_body.to_string(),
        ..Default::default()
    }];
    let found = child_frames(t).await;
    let (frames, err) = match found {
        Ok((f, _)) => (f, false),
        Err(_) => (Vec::new(), true),
    };
    if err || frames.is_empty() {
        // No child frames is normal on a page with none, and a symptom of site
        // isolation when the page does have them: with --site-per-process a
        // cross-origin iframe runs in its own process and getFrameTree stops
        // listing it, so iframe support silently does nothing. Say so once,
        // instead of leaving someone to wonder why nothing inside an iframe works.
        if !err && page_has_dom_iframes(t).await {
            FRAMES_WARNED.call_once(|| {
                golog("[frames] the page has iframes but Page.getFrameTree lists none. This happens when Chrome runs with site isolation (--site-per-process); cross-origin iframes then cannot be read or driven. Keep --disable-site-isolation-trials in chrome_flags (see browser.rs).");
            });
        }
        return docs;
    }
    for (i, f) in frames.iter().enumerate() {
        let mut d = FrameDoc {
            n: i + 1,
            url: f.url.clone(),
            ..Default::default()
        };
        match eval_frame_json::<FrameSnapOut>(t, i + 1, &frame_snap_expr()).await {
            Ok(out) => {
                d.url = out.url;
                d.title = out.title;
                d.body = out.body;
            }
            Err(e) => d.err = e,
        }
        docs.push(d);
    }
    docs
}

// ---------- Coordinates ----------

/// pageHasDOMIframes reports whether the main document holds any iframe element,
/// which is how we tell "no frames" apart from "frames we cannot see".
async fn page_has_dom_iframes(t: &Tab) -> bool {
    eval_main_json::<i64>(t, r#"document.querySelectorAll("iframe,frame").length"#)
        .await
        .is_ok_and(|n| n > 0)
}

/// iframeOffset is an iframe element's position inside its parent document.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
pub struct IframeOffset {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub w: f64,
    #[serde(default)]
    pub h: f64,
}

/// frameOffsetExpr measures, in the current document, the iframe element shown at
/// the given URL. Matching the src is what works across origins: the element is
/// readable even when its document is not.
pub fn frame_offset_expr(url: &str) -> String {
    format!(
        r##"(() => {{
      const want = {};
      const base = want.split("#")[0];
      const list = [...document.querySelectorAll("iframe,frame")];
      const f = list.find((e) => e.src === want) ||
                list.find((e) => e.src && base && (base.startsWith(e.src.split("#")[0]) || e.src.split("#")[0].startsWith(base.slice(0, 80))));
      if (!f) return null;
      const r = f.getBoundingClientRect();
      return {{ x: r.left, y: r.top, w: r.width, h: r.height }};
    }})()"##,
        jsq(url)
    )
}

/// frameOrigin returns where iframe n sits on the page, summing every nesting
/// level: coordinates measured inside the iframe are relative to it.
pub async fn frame_origin(t: &Tab, n: usize) -> Result<IframeOffset, String> {
    let mut out = IframeOffset::default();
    let (frames, main) = child_frames(t).await?;
    if n < 1 || n > frames.len() {
        return Err(format!("iframe f{n} not found"));
    }
    // Chain from the main frame down to frame n.
    let mut chain: Vec<ChildFrame> = Vec::new();
    let mut cur = frames[n - 1].clone();
    for _ in 0..32 {
        chain.insert(0, cur.clone());
        if cur.parent == main {
            break;
        }
        match frames.iter().find(|f| f.frame_id == cur.parent) {
            Some(parent) => cur = parent.clone(),
            None => break,
        }
    }
    // Measure each iframe element inside its own parent document.
    for (i, f) in chain.iter().enumerate() {
        let off = if i == 0 {
            eval_main_json::<IframeOffset>(t, &frame_offset_expr(&f.url)).await
        } else {
            eval_frame_json::<IframeOffset>(
                t,
                frame_index_of(&frames, &chain[i - 1].frame_id),
                &frame_offset_expr(&f.url),
            )
            .await
        };
        let off = off.map_err(|e| {
            format!(
                "locate iframe f{}: {e}",
                frame_index_of(&frames, &f.frame_id)
            )
        })?;
        out.x += off.x;
        out.y += off.y;
        if i == chain.len() - 1 {
            out.w = off.w;
            out.h = off.h;
        }
    }
    Ok(out)
}

/// evalMainJSON evaluates expr in the main frame and decodes the result.
pub async fn eval_main_json<T: DeserializeOwned + Default>(
    t: &Tab,
    expr: &str,
) -> Result<T, String> {
    let res = t
        .call(
            "Runtime.evaluate",
            json!({"expression": expr, "awaitPromise": true, "returnByValue": true}),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
    if let Some(d) = res.get("exceptionDetails").filter(|d| !d.is_null()) {
        return Err(js_error(d));
    }
    let v = res.pointer("/result/value").cloned().unwrap_or(Value::Null);
    if v.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(v).map_err(|e| e.to_string())
}

fn frame_index_of(frames: &[ChildFrame], id: &str) -> usize {
    frames
        .iter()
        .position(|f| f.frame_id == id)
        .map_or(0, |i| i + 1)
}

// ---------- Outline ----------

/// maxFrameURL caps the URL in a frame header. Stripe-style iframes carry their
/// whole configuration in the query string, which is noise in the outline: the
/// origin plus the start of the path is enough to tell frames apart.
pub const MAX_FRAME_URL: usize = 80;

/// frameURLShort keeps the scheme and host, then as much of the path as fits,
/// and drops the query and fragment.
pub fn frame_url_short(u: &str) -> String {
    let u = match u.find(['?', '#']) {
        Some(i) => &u[..i],
        None => u,
    };
    if u.len() <= MAX_FRAME_URL {
        return u.to_string();
    }
    let mut n = MAX_FRAME_URL - 1;
    while !u.is_char_boundary(n) {
        n -= 1;
    }
    format!("{}…", &u[..n])
}

/// maxFrameOutlines caps how many iframes get an outline in one snapshot. Pages
/// with ad slots can carry a dozen or more; the rest are summarized in one line
/// so the model still knows they exist.
pub const MAX_FRAME_OUTLINES: usize = 10;

/// joinFrameOutlines merges the page outline with the outline of every iframe.
/// Refs inside iframe N are rewritten from "e12" to "fNe12", which is what
/// Click/Type/SelectOption/Evaluate understand.
pub fn join_frame_outlines(docs: &[FrameDoc]) -> String {
    let mut b = String::new();
    b.push_str(&docs[0].body);
    let (mut shown, mut skipped) = (0, 0);
    for d in &docs[1..] {
        if !d.err.is_empty() || d.body.trim().is_empty() {
            continue;
        }
        if shown >= MAX_FRAME_OUTLINES {
            skipped += 1;
            continue;
        }
        let prefix = format!("f{}", d.n);
        b.push_str(&format!(
            "\n\n--- iframe [{prefix}] {} ---\n",
            frame_url_short(&d.url)
        ));
        b.push_str(&prefix_frame_refs(&d.body, &prefix));
        shown += 1;
    }
    for d in &docs[1..] {
        if !d.err.is_empty() {
            b.push_str(&format!(
                "\n\n--- iframe [f{}] unreadable: {} ---\n",
                d.n, d.err
            ));
        }
    }
    if skipped > 0 {
        b.push_str(&format!(
            "\n\n({skipped} more iframe(s) with content not shown: only the first {MAX_FRAME_OUTLINES} are listed, to keep this outline readable.)\n"
        ));
    }
    b
}

static REF_IN_TEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[ref=([A-Za-z]*\d+)\]").expect("static regex"));

pub fn prefix_frame_refs(body: &str, prefix: &str) -> String {
    REF_IN_TEXT
        .replace_all(body, |c: &regex::Captures| {
            format!("[ref={prefix}{}]", &c[1])
        })
        .into_owned()
}

// ---------- Driving a dropdown ----------

/// pickOptionExpr drives a dropdown inside a frame. A real <select> is set
/// directly; a custom combobox (Stripe's country picker, most design systems) is
/// opened and its option clicked, because no native popup is painted in headless.
pub fn pick_option_expr(r: &str, values: &[String]) -> String {
    let vals = serde_json::to_string(values).unwrap_or_default();
    format!(
        "{FRAME_PICK_JS}\n;__abuFrame.pickOption({}, {vals})",
        jsq(r)
    )
}

const FRAME_PICK_JS: &str = r##"(() => {
  if (window.__abuFrame) return;
  const clean = (s) => (s || "").replace(/\s+/g, " ").trim();
  const visible = (el) => {
    const r = el.getBoundingClientRect();
    if (r.width === 0 && r.height === 0) return false;
    const st = getComputedStyle(el);
    return st.visibility !== "hidden" && st.display !== "none";
  };
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const optionEls = () => [...document.querySelectorAll('[role="option"],[role="menuitem"],li[data-value],option')].filter(visible);
  function pickOption(ref, values) {
    return (async () => {
      let el = null;
      try { el = window.__abu && window.__abu.get ? window.__abu.get(ref) : null; } catch (e) { el = null; }
      if (!el) el = document.querySelector('[role="combobox"],[aria-haspopup="listbox"],select');
      if (!el) throw new Error("no dropdown found in this iframe");
      el.scrollIntoView({ block: "center", inline: "center" });

      if (el.tagName === "SELECT") {
        const picked = [];
        for (const o of [...el.options]) {
          if (values.some((v) => v === o.value || v === o.label || clean(o.textContent) === v)) {
            if (!picked.length) el.value = o.value;
            picked.push(clean(o.textContent || o.label));
          }
        }
        if (!picked.length) throw new Error("no option matches " + values.join(", "));
        el.dispatchEvent(new Event("input", { bubbles: true }));
        el.dispatchEvent(new Event("change", { bubbles: true }));
        return picked;
      }

      el.focus();
      el.click();
      let list = optionEls();
      for (let i = 0; i < 30 && !list.length; i++) { await wait(50); list = optionEls(); }
      const want = (t) => values.some((v) => clean(t) === v || clean(t).toLowerCase() === v.toLowerCase());
      let target = list.find((o) => want(o.textContent));
      if (!target) {
        const inp = el.tagName === "INPUT" ? el : (el.querySelector && el.querySelector("input")) || document.querySelector('input[role="combobox"]');
        if (inp) {
          for (const v of values) {
            inp.focus();
            inp.value = v;
            inp.dispatchEvent(new Event("input", { bubbles: true }));
            await wait(200);
            list = optionEls();
            target = list.find((o) => want(o.textContent));
            if (target) break;
          }
        }
      }
      if (!target) {
        const seen = list.map((o) => clean(o.textContent)).slice(0, 30);
        throw new Error("no option matches " + values.join(", ") + (seen.length ? ". Options: " + seen.join(" | ") : ". The dropdown did not open"));
      }
      target.scrollIntoView({ block: "nearest" });
      target.click();
      await wait(60);
      return [clean(target.textContent)];
    })();
  }
  Object.defineProperty(window, "__abuFrame", { value: { pickOption, optionEls }, configurable: true });
})()"##;

/// evalInFrameAs runs expr in iframe n's world and decodes the JSON result into T.
/// It is the frame equivalent of evalJS: the page helper is installed there first,
/// so __abu.* works with that frame's own refs.
pub async fn eval_in_frame_as<T: DeserializeOwned + Default>(
    t: &Tab,
    n: usize,
    expr: &str,
) -> Result<T, String> {
    let (frames, _) = child_frames(t).await?;
    if n < 1 || n > frames.len() {
        return Err(not_found(n, frames.len()));
    }
    let world = frame_world(t, &frames[n - 1].frame_id)
        .await
        .map_err(|e| format!("open iframe f{n}: {e}"))?;
    let raw = eval_in_world(t, world, &format!("{}\n;{expr}", agent_js())).await?;
    decode(raw, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// frames_test.rs: TestParseFrameRef
    #[test]
    fn parse_frame_ref_cases() {
        let cases = [
            ("e12", 0, "e12"),
            ("e1", 0, "e1"),
            ("", 0, ""),
            ("f1e12", 1, "e12"),
            ("f1e1", 1, "e1"),
            ("f2e3", 2, "e3"),
            ("f10e4", 10, "e4"),
            ("f123e999", 123, "e999"),
            ("f1", 0, "f1"),
            ("f12", 0, "f12"),
            ("fe1", 0, "fe1"),
            ("fxe1", 0, "fxe1"),
            ("f0e1", 0, "f0e1"),
        ];
        for (r, frame, rest) in cases {
            let got = parse_frame_ref(r);
            assert_eq!((got.frame, got.rest.as_str()), (frame, rest), "ref {r:?}");
        }
    }

    /// frames_test.rs: TestPrefixFrameRefs
    #[test]
    fn prefix_frame_refs_cases() {
        let cases = [
            (
                "one ref",
                "- textbox \"Email\" [ref=e1]",
                "f2",
                "- textbox \"Email\" [ref=f2e1]",
            ),
            (
                "several refs on separate lines",
                "- combobox [ref=e1]\n- listbox [ref=e2]\n- option \"Japan\" [ref=e3]",
                "f4",
                "- combobox [ref=f4e1]\n- listbox [ref=f4e2]\n- option \"Japan\" [ref=f4e3]",
            ),
            (
                "url attributes are left alone",
                "- link \"Docs\" [url=https://example.com/e12] [ref=e5]",
                "f1",
                "- link \"Docs\" [url=https://example.com/e12] [ref=f1e5]",
            ),
            (
                "text that looks like a ref is left alone",
                "- text: see ref=e7 for details [ref=e8]",
                "f3",
                "- text: see ref=e7 for details [ref=f3e8]",
            ),
            (
                "no refs at all",
                "- heading \"Country\" [level=2]\n- paragraph: hello",
                "f1",
                "- heading \"Country\" [level=2]\n- paragraph: hello",
            ),
            ("empty body", "", "f1", ""),
        ];
        for (name, body, prefix, want) in cases {
            assert_eq!(prefix_frame_refs(body, prefix), want, "{name}");
        }
    }

    /// frames_test.rs: TestFrameURLShort
    #[test]
    fn frame_url_short_cases() {
        let long = format!("https://example.com/{}", "a".repeat(200));
        let want_long = format!(
            "https://example.com/{}…",
            "a".repeat(MAX_FRAME_URL - "https://example.com/".len() - 1)
        );
        let cases = [
            (
                "short url unchanged",
                "https://js.stripe.com/v3/elements-inner-habanero.html".to_string(),
                "https://js.stripe.com/v3/elements-inner-habanero.html".to_string(),
            ),
            (
                "query and fragment dropped",
                "https://js.stripe.com/v3/frame.html?apiKey=pk_live_123&session=cs_live_456#frag"
                    .to_string(),
                "https://js.stripe.com/v3/frame.html".to_string(),
            ),
            ("long path is truncated with an ellipsis", long, want_long),
            ("empty url", String::new(), String::new()),
        ];
        for (name, url, want) in cases {
            let got = frame_url_short(&url);
            assert_eq!(got, want, "{name}");
            assert!(got.chars().count() <= MAX_FRAME_URL, "{name}");
        }
    }

    fn doc(n: usize, url: &str, body: &str, err: &str) -> FrameDoc {
        FrameDoc {
            n,
            url: url.into(),
            body: body.into(),
            err: err.into(),
            ..Default::default()
        }
    }

    /// frames_test.rs: TestJoinFrameOutlines
    #[test]
    fn join_frame_outlines_cases() {
        // main frame only
        let got = join_frame_outlines(&[doc(0, "", "- heading \"Hello\"", "")]);
        assert_eq!(got, "- heading \"Hello\"");

        // frames are prefixed and labelled
        let got = join_frame_outlines(&[
            doc(0, "", "- heading \"Outer\"", ""),
            doc(
                1,
                "https://js.stripe.com/v3/f.html?apiKey=x",
                "- combobox [ref=e1]",
                "",
            ),
        ]);
        assert!(
            got.contains("--- iframe [f1] https://js.stripe.com/v3/f.html ---"),
            "{got}"
        );
        assert!(got.contains("[ref=f1e1]"), "{got}");
        assert!(!got.contains("apiKey"), "{got}");

        // empty frames are skipped
        let got = join_frame_outlines(&[
            doc(0, "", "- heading \"Outer\"", ""),
            doc(1, "https://a.example/", "   \n  ", ""),
            doc(2, "https://b.example/", "- text: hi", ""),
        ]);
        assert!(!got.contains("[f1]"), "{got}");
        assert!(got.contains("[f2]"), "{got}");

        // unreadable frames are reported
        let got = join_frame_outlines(&[
            doc(0, "", "- heading \"Outer\"", ""),
            doc(1, "https://a.example/", "", "detached"),
        ]);
        assert!(got.contains("unreadable: detached"), "{got}");

        // frame count is capped
        let mut docs = vec![doc(0, "", "- heading \"Outer\"", "")];
        for i in 1..=MAX_FRAME_OUTLINES + 3 {
            docs.push(doc(i, "https://f.example/", "- text: x", ""));
        }
        let got = join_frame_outlines(&docs);
        assert_eq!(got.matches("--- iframe [").count(), MAX_FRAME_OUTLINES);
        assert!(
            got.contains("3 more iframe(s) with content not shown"),
            "{got}"
        );
    }
}
