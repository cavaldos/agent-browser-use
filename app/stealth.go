package main

// Stealth: make the headless, CDP-driven Chrome look like the Chrome a person uses.
//
// The approach follows Obscura (github.com/h4ckf0r0day/obscura): one consistent
// identity instead of a pile of fakes. Bot checks (Google sign-in among them)
// compare values with each other, so a faked value that disagrees with the
// real browser is worse than no fake at all. Hence:
//
//  1. Leave alone what Chrome already reports correctly (webdriver is false thanks
//     to --disable-blink-features=AutomationControlled; WebGL, plugins, languages,
//     permissions and userAgentData are the real ones).
//  2. Fix the headless leaks natively over CDP where possible: the user agent
//     without "HeadlessChrome" now comes with its full client hints
//     (userAgentMetadata), taken from the running browser.
//  3. For what only JavaScript can fix, patch as little as possible and make each
//     patch indistinguishable from the built-in: functions print as
//     "[native code]", nothing is added to navigator itself, and our own globals
//     (bindings, helpers) are hidden from Object.keys / getOwnPropertyNames /
//     Reflect.ownKeys.

import (
	"os/exec"
	"regexp"
	"runtime"
	"strings"

	"github.com/chromedp/cdproto/cdp"
	"github.com/chromedp/cdproto/emulation"
)

// hiddenGlobalPrefixes are the window properties this program adds (DevTools
// bindings, page helpers). Pages must not see them when they list window's keys.
var hiddenGlobalPrefixes = []string{"__tb", "__abu"}

// stealthJS runs first in every frame, before the page's own scripts.
var stealthJS = `(() => {
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
  const prefixes = ` + jsStrings(hiddenGlobalPrefixes) + `;
  const isOurs = (k) => typeof k === 'string' && prefixes.some((p) => k.startsWith(p));
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
})();`

// jsStrings writes a Go string list as a JavaScript array literal.
func jsStrings(l []string) string {
	q := make([]string, len(l))
	for i, s := range l {
		q[i] = "'" + s + "'"
	}
	return "[" + strings.Join(q, ", ") + "]"
}

// userAgentMetadata is emulation.UserAgentMetadata plus fullVersion, which cdproto
// dropped but Chrome still uses for navigator.userAgentData's uaFullVersion.
type userAgentMetadata struct {
	Brands          []*emulation.UserAgentBrandVersion `json:"brands,omitempty"`
	FullVersionList []*emulation.UserAgentBrandVersion `json:"fullVersionList,omitempty"`
	FullVersion     string                             `json:"fullVersion,omitempty"`
	Platform        string                             `json:"platform"`
	PlatformVersion string                             `json:"platformVersion"`
	Architecture    string                             `json:"architecture"`
	Model           string                             `json:"model"`
	Mobile          bool                               `json:"mobile"`
	Bitness         string                             `json:"bitness,omitempty"`
	Wow64           bool                               `json:"wow64"`
	FormFactors     []string                           `json:"formFactors,omitempty"`
}

type userAgentOverrideParams struct {
	UserAgent         string             `json:"userAgent"`
	AcceptLanguage    string             `json:"acceptLanguage,omitempty"`
	UserAgentMetadata *userAgentMetadata `json:"userAgentMetadata,omitempty"`
}

// setUserAgentOverride is Emulation.setUserAgentOverride with the complete metadata.
var setUserAgentOverride = cdp.Command[userAgentOverrideParams, cdp.Empty]{Method: emulation.CommandSetUserAgentOverride}

// uaMetadata builds the client hints (navigator.userAgentData and the Sec-CH-UA-*
// headers) for the user agent the shared browser presents. Overriding the user
// agent empties Chrome's high-entropy hints (full version, platform version,
// architecture); a real Chrome always has them, so we fill them in from the
// running browser: product is Browser.getVersion's "HeadlessChrome/157.0.1.2",
// brands and platform what navigator.userAgentData reports.
func uaMetadata(product string, brands []*emulation.UserAgentBrandVersion, platform string) *userAgentMetadata {
	full := product[strings.LastIndex(product, "/")+1:]
	if len(brands) == 0 || !regexp.MustCompile(`^\d+(\.\d+){3}$`).MatchString(full) {
		return nil // nothing reliable to build from: keep Chrome's own
	}
	major := full[:strings.Index(full, ".")]
	m := &userAgentMetadata{
		FullVersion:     full,
		Platform:        platform,
		PlatformVersion: osPlatformVersion(),
		Architecture:    "x86",
		Bitness:         "64",
		FormFactors:     []string{"Desktop"},
	}
	switch runtime.GOARCH {
	case "arm64", "arm":
		m.Architecture = "arm"
	}
	if runtime.GOARCH == "386" || runtime.GOARCH == "arm" {
		m.Bitness = "32"
	}
	for _, b := range brands {
		m.Brands = append(m.Brands, &emulation.UserAgentBrandVersion{Brand: b.Brand, Version: b.Version})
		v := b.Version + ".0.0.0" // the GREASE brand ("Not-A.Brand") has no real version
		if b.Version == major {
			v = full
		}
		m.FullVersionList = append(m.FullVersionList, &emulation.UserAgentBrandVersion{Brand: b.Brand, Version: v})
	}
	return m
}

// osPlatformVersion is Sec-CH-UA-Platform-Version as Chrome reports it on this machine.
func osPlatformVersion() string {
	out := func(name string, args ...string) string {
		b, err := exec.Command(name, args...).Output()
		if err != nil {
			return ""
		}
		return strings.TrimSpace(string(b))
	}
	switch runtime.GOOS {
	case "darwin": // macOS 26.2 -> "26.2.0"
		return threeParts(out("sw_vers", "-productVersion"))
	case "windows": // Chrome: "10.0.0" on Windows 10, "13.0.0"+ on Windows 11
		if m := regexp.MustCompile(`\d+\.\d+\.(\d+)`).FindStringSubmatch(out("cmd", "/c", "ver")); m != nil && len(m[1]) >= 5 && m[1] >= "22000" {
			return "15.0.0"
		}
		return "10.0.0"
	case "linux": // the kernel version, e.g. "6.8.0"
		return threeParts(out("uname", "-r"))
	}
	return ""
}

// threeParts turns "26.2" or "6.8.0-45-generic" into "26.2.0" / "6.8.0".
func threeParts(v string) string {
	m := regexp.MustCompile(`^(\d+)(?:\.(\d+))?(?:\.(\d+))?`).FindStringSubmatch(v)
	if m == nil {
		return ""
	}
	for i := 2; i <= 3; i++ {
		if m[i] == "" {
			m[i] = "0"
		}
	}
	return m[1] + "." + m[2] + "." + m[3]
}
