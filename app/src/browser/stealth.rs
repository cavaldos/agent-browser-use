//! Stealth: make the headless, CDP-driven Chrome look like the Chrome a person uses.
//!
//! The approach follows Obscura (github.com/h4ckf0r0day/obscura): one consistent
//! identity instead of a pile of fakes. Bot checks (Google sign-in among them)
//! compare values with each other, so a faked value that disagrees with the
//! real browser is worse than no fake at all. Hence:
//!
//!  1. Leave alone what Chrome already reports correctly (webdriver is false thanks
//!     to --disable-blink-features=AutomationControlled; WebGL, plugins, languages,
//!     permissions and userAgentData are the real ones).
//!  2. Fix the headless leaks natively over CDP where possible: the user agent
//!     without "HeadlessChrome" now comes with its full client hints
//!     (userAgentMetadata), taken from the running browser.
//!  3. For what only JavaScript can fix, patch as little as possible and make each
//!     patch indistinguishable from the built-in: functions print as
//!     "[native code]", nothing is added to navigator itself, and our own globals
//!     (bindings, helpers) are hidden from Object.keys / getOwnPropertyNames /
//!     Reflect.ownKeys.

use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

/// hiddenGlobalPrefixes are the window properties this program adds (DevTools
/// bindings, page helpers). Pages must not see them when they list window's keys.
pub const HIDDEN_GLOBAL_PREFIXES: [&str; 2] = ["__tb", "__abu"];

/// stealthJS runs first in every frame, before the page's own scripts.
pub static STEALTH_JS: LazyLock<String> = LazyLock::new(|| {
    let p1 = r####"(() => {
  const w = window;
  if (Object.getOwnPropertyDescriptor(w, '__abuStealth')) return;
  Object.defineProperty(w, '__abuStealth', { value: true });

  // Native-looking functions. Every function we put in place of a built-in is
  // registered here, so Function.prototype.toString prints it like the original.
  const nativeText = new WeakMap();
  const fnToString = Function.prototype.toString;
  const toString = { toString() {
    const s = nativeText.get(this);
    return s !== undefined ? s : Reflect.apply(fnToString, this, []);
  } }.toString;
  nativeText.set(toString, 'function toString() { [native code] }');
  const asNative = (fn, name) => { nativeText.set(fn, 'function ' + name + '() { [native code] }'); return fn; };
  Object.defineProperty(Function.prototype, 'toString', { value: toString, writable: true, enumerable: false, configurable: true });
  // For our other page scripts that replace built-ins (audio tap, screen share).
  Object.defineProperty(w, '__abuNative', { value: asNative });
  const setMethod = (obj, name, fn) =>
    Object.defineProperty(obj, name, { value: asNative(fn, name), writable: true, enumerable: false, configurable: true });

  // Hide our own globals from the reflection APIs (only when they list window).
"####;
    let p2 = r####"  const isOurs = (k) => typeof k === 'string' && prefixes.some((p) => k.startsWith(p));
  const strip = (o, keys) => (o === w ? keys.filter((k) => !isOurs(k)) : keys);
  const gopn = Object.getOwnPropertyNames, keys = Object.keys, ownKeys = Reflect.ownKeys, gopds = Object.getOwnPropertyDescriptors;
  setMethod(Object, 'getOwnPropertyNames', { getOwnPropertyNames(o) { return strip(o, gopn(o)); } }.getOwnPropertyNames);
  setMethod(Object, 'keys', { keys(o) { return strip(o, keys(o)); } }.keys);
  setMethod(Reflect, 'ownKeys', { ownKeys(o) { return strip(o, ownKeys(o)); } }.ownKeys);
  setMethod(Object, 'getOwnPropertyDescriptors', { getOwnPropertyDescriptors(o) {
    const d = gopds(o);
    if (o === w) for (const k of ownKeys(d)) if (isOurs(k)) delete d[k];
    return d;
  } }.getOwnPropertyDescriptors);
  // DevTools bindings are already installed and enumerable: keep them out of for…in.
  for (const k of gopn(w)) {
    if (!isOurs(k)) continue;
    const d = Object.getOwnPropertyDescriptor(w, k);
    if (d && d.enumerable && d.configurable) { d.enumerable = false; Object.defineProperty(w, k, d); }
  }

  // Window frame: a headless window has no tab strip or toolbar, so its outer size
  // equals the page's. Add a desktop browser's frame, within the screen.
  const ua = navigator.userAgent, plat = navigator.platform;
  const mobile = /Mobile|Android|iPhone|iPad/.test(ua) || navigator.maxTouchPoints > 0;
  if (!mobile) {
    const mac = /Mac/.test(plat), win = /Win/.test(plat);
    const frame = { outerWidth: win ? 16 : 0, outerHeight: mac ? 87 : win ? 88 : 85 };
    const screenGet = (n) => Reflect.apply(Object.getOwnPropertyDescriptor(Screen.prototype, n).get, w.screen, []);
    const limit = { outerWidth: 'availWidth', outerHeight: 'availHeight' };
    for (const n of ['outerWidth', 'outerHeight']) {
      const d = Object.getOwnPropertyDescriptor(w, n);
      if (!d || !d.get) continue;
      const orig = d.get;
      const get = Object.getOwnPropertyDescriptor({ get [n]() {
        const v = Reflect.apply(orig, this, []);
        return Math.max(v, Math.min(v + frame[n], screenGet(limit[n])));
      } }, n).get;
      Object.defineProperty(w, n, { ...d, get: asNative(get, 'get ' + n) });
    }
    // A desktop screen keeps room for the menu bar (macOS) or the taskbar (Windows).
    const bar = mac ? 25 : win ? 48 : 0;
    const sp = Screen.prototype;
    const ah = Object.getOwnPropertyDescriptor(sp, 'availHeight'), at = Object.getOwnPropertyDescriptor(sp, 'availTop');
    if (bar && ah && ah.get) {
      const origH = ah.get, origTop = at && at.get;
      const full = (s) => Reflect.apply(origH, s, []) === Reflect.apply(Object.getOwnPropertyDescriptor(sp, 'height').get, s, []);
      Object.defineProperty(sp, 'availHeight', { ...ah, get: asNative(Object.getOwnPropertyDescriptor({ get availHeight() {
        const v = Reflect.apply(origH, this, []);
        return full(this) ? v - bar : v;
      } }, 'availHeight').get, 'get availHeight') });
      if (mac && origTop) {
        Object.defineProperty(sp, 'availTop', { ...at, get: asNative(Object.getOwnPropertyDescriptor({ get availTop() {
          const v = Reflect.apply(origTop, this, []);
          return v === 0 && full(this) ? bar : v;
        } }, 'availTop').get, 'get availTop') });
      }
    }
  }
})();"####;
    format!(
        "{p1}  const prefixes = {};\n{p2}",
        js_strings(&HIDDEN_GLOBAL_PREFIXES)
    )
});

/// jsStrings writes a string list as a JavaScript array literal.
pub fn js_strings(l: &[&str]) -> String {
    let q: Vec<String> = l.iter().map(|s| format!("'{s}'")).collect();
    format!("[{}]", q.join(", "))
}

/// emulation.UserAgentBrandVersion
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserAgentBrandVersion {
    pub brand: String,
    pub version: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// userAgentMetadata is emulation.UserAgentMetadata plus fullVersion, which cdproto
/// dropped but Chrome still uses for navigator.userAgentData's uaFullVersion.
/// (chromiumoxide_cdp's type lacks it too: this is the custom params struct.)
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct UserAgentMetadata {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub brands: Vec<UserAgentBrandVersion>,
    #[serde(rename = "fullVersionList", skip_serializing_if = "Vec::is_empty")]
    pub full_version_list: Vec<UserAgentBrandVersion>,
    #[serde(rename = "fullVersion", skip_serializing_if = "String::is_empty")]
    pub full_version: String,
    pub platform: String,
    #[serde(rename = "platformVersion")]
    pub platform_version: String,
    pub architecture: String,
    pub model: String,
    pub mobile: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub bitness: String,
    pub wow64: bool,
    #[serde(rename = "formFactors", skip_serializing_if = "Vec::is_empty")]
    pub form_factors: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct UserAgentOverrideParams {
    #[serde(rename = "userAgent")]
    pub user_agent: String,
    #[serde(rename = "acceptLanguage", skip_serializing_if = "String::is_empty")]
    pub accept_language: String,
    #[serde(rename = "userAgentMetadata", skip_serializing_if = "Option::is_none")]
    pub user_agent_metadata: Option<UserAgentMetadata>,
}

/// `setUserAgentOverride` is Emulation.setUserAgentOverride with the complete metadata.
pub const SET_USER_AGENT_OVERRIDE: &str = "Emulation.setUserAgentOverride";

/// uaMetadata builds the client hints (navigator.userAgentData and the Sec-CH-UA-*
/// headers) for the user agent the shared browser presents. Overriding the user
/// agent empties Chrome's high-entropy hints (full version, platform version,
/// architecture); a real Chrome always has them, so we fill them in from the
/// running browser: product is Browser.getVersion's "HeadlessChrome/157.0.1.2",
/// brands and platform what navigator.userAgentData reports.
pub fn ua_metadata(
    product: &str,
    brands: &[UserAgentBrandVersion],
    platform: &str,
) -> Option<UserAgentMetadata> {
    let full = &product[product.rfind('/').map_or(0, |i| i + 1)..];
    // ^\d+(\.\d+){3}$
    let parts: Vec<&str> = full.split('.').collect();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if brands.is_empty() || parts.len() != 4 || !parts.iter().all(|p| digits(p)) {
        return None; // nothing reliable to build from: keep Chrome's own
    }
    let major = parts[0];
    let arch = std::env::consts::ARCH;
    let mut m = UserAgentMetadata {
        full_version: full.to_string(),
        platform: platform.to_string(),
        platform_version: os_platform_version(),
        architecture: "x86".into(),
        bitness: "64".into(),
        form_factors: vec!["Desktop".into()],
        ..Default::default()
    };
    if arch == "aarch64" || arch == "arm" {
        m.architecture = "arm".into();
    }
    if arch == "x86" || arch == "arm" {
        m.bitness = "32".into();
    }
    for b in brands {
        m.brands.push(UserAgentBrandVersion {
            brand: b.brand.clone(),
            version: b.version.clone(),
        });
        // the GREASE brand ("Not-A.Brand") has no real version
        let v = if b.version == major {
            full.to_string()
        } else {
            format!("{}.0.0.0", b.version)
        };
        m.full_version_list.push(UserAgentBrandVersion {
            brand: b.brand.clone(),
            version: v,
        });
    }
    Some(m)
}

/// osPlatformVersion is Sec-CH-UA-Platform-Version as Chrome reports it on this machine.
pub fn os_platform_version() -> String {
    let out = |name: &str, args: &[&str]| -> String {
        std::process::Command::new(name)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    };
    match std::env::consts::OS {
        // macOS 26.2 -> "26.2.0"
        "macos" => three_parts(&out("sw_vers", &["-productVersion"])),
        // Chrome: "10.0.0" on Windows 10, "13.0.0"+ on Windows 11
        "windows" => {
            // \d+\.\d+\.(\d+) in `cmd /c ver`
            let v = out("cmd", &["/c", "ver"]);
            let build = v
                .split(|c: char| !(c.is_ascii_digit() || c == '.'))
                .find_map(|w| {
                    let p: Vec<&str> = w.split('.').collect();
                    (p.len() >= 3 && p[..3].iter().all(|x| !x.is_empty())).then(|| p[2].to_string())
                });
            match build {
                Some(b) if b.len() >= 5 && b.as_str() >= "22000" => "15.0.0".into(),
                _ => "10.0.0".into(),
            }
        }
        // the kernel version, e.g. "6.8.0"
        "linux" => three_parts(&out("uname", &["-r"])),
        _ => String::new(),
    }
}

/// threeParts turns "26.2" or "6.8.0-45-generic" into "26.2.0" / "6.8.0".
pub fn three_parts(v: &str) -> String {
    // ^(\d+)(?:\.(\d+))?(?:\.(\d+))?
    fn num(s: &str) -> (&str, &str) {
        let n = s.bytes().take_while(u8::is_ascii_digit).count();
        s.split_at(n)
    }
    let (a, mut rest) = num(v);
    if a.is_empty() {
        return String::new();
    }
    let mut parts = [a, "0", "0"];
    for slot in parts.iter_mut().skip(1) {
        let Some(r) = rest.strip_prefix('.') else {
            break;
        };
        let (n, r2) = num(r);
        if n.is_empty() {
            break;
        }
        *slot = n;
        rest = r2;
    }
    format!("{}.{}.{}", parts[0], parts[1], parts[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brand(b: &str, v: &str) -> UserAgentBrandVersion {
        UserAgentBrandVersion {
            brand: b.into(),
            version: v.into(),
        }
    }

    #[test]
    fn ua_metadata_builds_client_hints() {
        let brands = vec![
            brand("Not-A.Brand", "99"),
            brand("Google Chrome", "157"),
            brand("Chromium", "157"),
        ];
        let m = ua_metadata("HeadlessChrome/157.0.8103.39", &brands, "macOS").expect("no metadata");
        assert!(
            m.full_version == "157.0.8103.39"
                && m.platform == "macOS"
                && !m.mobile
                && !m.bitness.is_empty(),
            "got {m:?}"
        );
        let want = |b: &str| match b {
            "Not-A.Brand" => "99.0.0.0",
            _ => "157.0.8103.39",
        };
        assert_eq!(
            m.full_version_list.len(),
            3,
            "fullVersionList: {:?}",
            m.full_version_list
        );
        for (i, b) in m.full_version_list.iter().enumerate() {
            assert!(
                b.brand == brands[i].brand && b.version == want(&b.brand),
                "fullVersionList[{i}] = {b:?}"
            );
            // the low-entropy list keeps major versions
            assert_eq!(
                m.brands[i].version, brands[i].version,
                "brands[{i}] = {:?}",
                m.brands[i]
            );
        }
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm",
            "x86_64" => "x86",
            _ => "",
        };
        if !arch.is_empty() {
            assert_eq!(m.architecture, arch);
        }
        // fullVersion must reach Chrome even though cdproto no longer has it.
        let j = serde_json::to_string(&UserAgentOverrideParams {
            user_agent: "x".into(),
            user_agent_metadata: Some(m),
            ..Default::default()
        })
        .unwrap();
        for s in [
            r#""fullVersion":"157.0.8103.39""#,
            r#""fullVersionList":"#,
            r#""platformVersion":"#,
            r#""formFactors":["Desktop"]"#,
        ] {
            assert!(j.contains(s), "{j} lacks {s}");
        }

        // Nothing reliable to build from: keep Chrome's own hints.
        assert!(
            ua_metadata("HeadlessChrome/157.0.8103.39", &[], "macOS").is_none()
                && ua_metadata("weird", &brands, "macOS").is_none(),
            "expected nil metadata"
        );
    }

    #[test]
    fn three_parts_cases() {
        for (inp, want) in [
            ("26.2", "26.2.0"),
            ("15.6.1", "15.6.1"),
            ("6.8.0-45-generic", "6.8.0"),
            ("14", "14.0.0"),
            ("", ""),
            ("x", ""),
        ] {
            assert_eq!(three_parts(inp), want, "three_parts({inp:?})");
        }
    }

    #[test]
    fn stealth_js_hides_our_globals() {
        for s in [
            "'__tb'",
            "'__abu'",
            "getOwnPropertyNames",
            "Reflect, 'ownKeys'",
            "[native code]",
        ] {
            assert!(STEALTH_JS.contains(s), "stealthJS lacks {s}");
        }
        // The old fakes contradicted the real browser and got Google sign-in blocked.
        for s in [
            "'webdriver'",
            "userAgentData",
            "WebGLRenderingContext",
            "permissions",
        ] {
            assert!(!STEALTH_JS.contains(s), "stealthJS fakes {s} again");
        }
        assert!(STEALTH_JS.contains("const prefixes = ['__tb', '__abu'];"));
    }
}
