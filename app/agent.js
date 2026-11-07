// Helpers the AI agent runs inside the page (main world). Installed on demand
// before every call, so it survives navigations. Exposes window.__abu:
//   snapshot()            accessibility-style outline of the page, with [ref=eN] on things you can act on
//   locate(ref)           scrolls the element into view, returns its box in viewport pixels
//   prepareInput(ref, c)  focuses a field; c=true selects its content so typing replaces it
//   selectOption(ref, v)  picks options of a <select>
//   get(ref)              the element itself (for browser_evaluate)
//   hasText(s)            whether the page shows the text
//   idleFor()             ms since the page last changed (clocks and timers ticking don't count)
//   fileInput(ref)        the <input type=file> that belongs to the element (itself, inside it, or its label)
//   watchStart/State/Stop wait for the page (or one element) to change on its own
(() => {
  const V = 5;
  if (window.__abu && window.__abu.v === V) return;

  // Refs stay the same for the same element across snapshots.
  const old = window.__abu;
  const refs = old?.refs || new Map(); // "e12" -> WeakRef(element)
  const ids = old?.ids || new WeakMap(); // element -> "e12"
  let n = old?.n || 0;
  const refOf = (el) => {
    let r = ids.get(el);
    if (!r) { r = "e" + ++n; ids.set(el, r); }
    refs.set(r, new WeakRef(el));
    return r;
  };
  const get = (r) => {
    const el = refs.get(r)?.deref();
    if (!el || !el.isConnected) throw new Error(`Ref ${r} not found on the current page. Take a new snapshot (browser_snapshot) and use a ref from it.`);
    return el;
  };

  const clean = (s, max = 200) => {
    s = (s || "").replace(/\s+/g, " ").trim();
    return s.length > max ? s.slice(0, max - 1) + "…" : s;
  };
  const q = (s) => JSON.stringify(s);

  // Text that is only digits (a chess clock, a countdown, "12:04") changes all the time
  // without the page really changing, so mutations there are ignored.
  const timerLike = (node) => {
    const el = node?.nodeType === 1 ? node : node?.parentElement;
    const t = el?.textContent ?? "";
    return t.length < 40 && /^[\s\d:.,]*$/.test(t);
  };

  const SKIP = new Set(["SCRIPT", "STYLE", "NOSCRIPT", "TEMPLATE", "META", "LINK", "HEAD", "TITLE", "BASE"]);
  const INTERACTIVE = new Set(["link", "button", "checkbox", "radio", "textbox", "searchbox", "combobox", "listbox", "option",
    "slider", "spinbutton", "switch", "tab", "menuitem", "menuitemcheckbox", "menuitemradio", "treeitem", "clickable"]);
  // Roles whose name is their text: printed as one line, no children (unless they hold controls).
  const NAME_FROM_TEXT = new Set(["link", "button", "heading", "tab", "menuitem", "menuitemcheckbox", "menuitemradio", "option",
    "treeitem", "cell", "columnheader", "rowheader", "checkbox", "radio", "switch", "clickable", "listitem", "paragraph", "code", "label"]);
  const INPUT_ROLE = { button: "button", submit: "button", reset: "button", image: "button", file: "button", checkbox: "checkbox",
    radio: "radio", range: "slider", number: "spinbutton", search: "searchbox", color: "button" };

  function roleOf(el) {
    const r = el.getAttribute("role");
    if (r && r !== "presentation" && r !== "none") return r.split(/\s+/)[0];
    switch (el.tagName) {
      case "A": return el.hasAttribute("href") ? "link" : null;
      case "BUTTON": case "SUMMARY": return "button";
      case "INPUT": {
        const t = (el.getAttribute("type") || "text").toLowerCase();
        if (t === "hidden") return null;
        return INPUT_ROLE[t] || "textbox";
      }
      case "TEXTAREA": return "textbox";
      case "SELECT": return el.multiple || el.size > 1 ? "listbox" : "combobox";
      case "IMG": return el.getAttribute("alt") === "" ? null : "img";
      case "H1": case "H2": case "H3": case "H4": case "H5": case "H6": return "heading";
      case "UL": case "OL": case "MENU": return "list";
      case "LI": return "listitem";
      case "NAV": return "navigation";
      case "MAIN": return "main";
      case "HEADER": return "banner";
      case "FOOTER": return "contentinfo";
      case "ASIDE": return "complementary";
      case "FORM": return "form";
      case "DIALOG": return "dialog";
      case "TABLE": return "table";
      case "TR": return "row";
      case "TD": return "cell";
      case "TH": return "columnheader";
      case "P": return "paragraph";
      case "PRE": return "code";
      case "LABEL": return "label";
      case "IFRAME": return "iframe";
      case "VIDEO": return "video";
      case "HR": return "separator";
      case "BLOCKQUOTE": return "blockquote";
    }
    if (el.isContentEditable && !el.parentElement?.isContentEditable) return "textbox";
    return null;
  }

  function nameOf(el, role) {
    let s = el.getAttribute("aria-label");
    if (s && s.trim()) return clean(s);
    const by = el.getAttribute("aria-labelledby");
    if (by) {
      s = by.split(/\s+/).map((id) => el.ownerDocument.getElementById(id)?.innerText || "").join(" ");
      if (s.trim()) return clean(s);
    }
    const tag = el.tagName;
    if (tag === "IMG") return clean(el.getAttribute("alt") || el.getAttribute("title"));
    if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") {
      const t = (el.type || "").toLowerCase();
      if (["button", "submit", "reset"].includes(t)) return clean(el.value || (t === "submit" ? "Submit" : t === "reset" ? "Reset" : ""));
      if (t === "image") return clean(el.alt || el.value);
      if (el.labels?.length) return clean([...el.labels].map((l) => l.innerText).join(" "));
      return clean(el.getAttribute("placeholder") || el.getAttribute("title") || el.getAttribute("name"));
    }
    if (tag === "IFRAME") return clean(el.getAttribute("title") || el.getAttribute("name"));
    if (NAME_FROM_TEXT.has(role)) {
      s = el.innerText;
      if (!clean(s)) { // icon-only controls
        const img = el.querySelector("img[alt],svg[aria-label],[title]");
        s = img?.getAttribute("alt") || img?.getAttribute("aria-label") || img?.getAttribute("title") || el.getAttribute("title") || "";
      }
      return clean(s, role === "paragraph" || role === "code" || role === "listitem" || role === "cell" ? 500 : 120);
    }
    return clean(el.getAttribute("title"));
  }

  function visible(el) {
    if (el.getAttribute("aria-hidden") === "true") return false;
    if (el.checkVisibility && !el.checkVisibility({ visibilityProperty: true, checkVisibilityCSS: true })) {
      return getComputedStyle(el).display === "contents"; // no box of its own, children may show
    }
    return true;
  }

  function childNodes(el) {
    if (el.shadowRoot) return [...el.shadowRoot.childNodes];
    if (el.tagName === "SLOT") {
      const a = el.assignedNodes({ flatten: true });
      return a.length ? a : [...el.childNodes];
    }
    return [...el.childNodes];
  }

  // Elements without a role that still react to clicks (div with onclick, cursor: pointer…).
  function clickable(el) {
    if (el.hasAttribute("onclick") || (el.hasAttribute("tabindex") && el.tabIndex >= 0)) return true;
    const c = getComputedStyle(el).cursor;
    if (c !== "pointer") return false;
    const p = el.parentElement;
    return !p || getComputedStyle(p).cursor !== "pointer"; // only the outermost pointer element
  }

  function attrs(el, role) {
    const a = [];
    const t = (el.type || "").toLowerCase();
    if (role === "heading") a.push(`level=${el.getAttribute("aria-level") || el.tagName.slice(1) || 2}`);
    if (role === "checkbox" || role === "radio" || role === "switch" || role === "menuitemcheckbox" || role === "menuitemradio") {
      const c = el.tagName === "INPUT" ? el.checked : el.getAttribute("aria-checked") === "true";
      if (c) a.push("checked");
      if (el.indeterminate || el.getAttribute("aria-checked") === "mixed") a.push("mixed");
    }
    if (el.disabled || el.getAttribute("aria-disabled") === "true") a.push("disabled");
    const ex = el.getAttribute("aria-expanded");
    if (ex) a.push(ex === "true" ? "expanded" : "collapsed");
    if (el.getAttribute("aria-selected") === "true") a.push("selected");
    if (el.getAttribute("aria-pressed") === "true") a.push("pressed");
    if (el.getAttribute("aria-current") && el.getAttribute("aria-current") !== "false") a.push("current");
    if (el.required) a.push("required");
    if (el.readOnly && (el.tagName === "INPUT" || el.tagName === "TEXTAREA")) a.push("readonly");
    if (role === "link") {
      const h = el.getAttribute("href");
      if (h && !h.startsWith("javascript:") && h.length < 150) a.push(`url=${h}`);
    }
    if (t && el.tagName === "INPUT" && !["text", "checkbox", "radio", "submit", "button"].includes(t) && role === "textbox") a.push(`type=${t}`);
    return a;
  }

  function valueOf(el, role) {
    if (el.tagName === "SELECT") {
      const sel = [...el.selectedOptions].map((o) => clean(o.label, 80)).join(", ");
      const all = [...el.options].slice(0, 25).map((o) => clean(o.label, 60));
      return `${sel || "(none)"} [options: ${all.join(" | ")}${el.options.length > 25 ? " | …" : ""}]`;
    }
    if (el.tagName === "INPUT" || el.tagName === "TEXTAREA") {
      if (["checkbox", "radio", "button", "submit", "reset", "image", "file"].includes((el.type || "").toLowerCase())) return "";
      if (el.type === "password") return el.value ? "•".repeat(Math.min(el.value.length, 12)) : "";
      return clean(el.value, 300);
    }
    if (role === "textbox" && el.isContentEditable) return clean(el.innerText, 300);
    if (role === "slider" || role === "spinbutton") return el.getAttribute("aria-valuenow") || "";
    return "";
  }

  const MAX = 60000;
  function snapshot() {
    let size = 0, truncated = false;
    const out = [];
    const line = (depth, s) => {
      if (truncated) return;
      if (size > MAX) { truncated = true; out.push("- text: … (page is long, the rest was cut. Scroll or use browser_evaluate to read more)"); return; }
      const l = "  ".repeat(depth) + s;
      out.push(l);
      size += l.length + 1;
    };

    // Returns lines for the children of `parent` at `depth` into `out`; text pieces are merged.
    function walkChildren(parent, depth) {
      let text = "";
      const flush = () => { const t = clean(text, 500); if (t) line(depth, "- text: " + t); text = ""; };
      for (const node of childNodes(parent)) {
        if (truncated) return;
        if (node.nodeType === 3) { text += " " + node.nodeValue; continue; }
        if (node.nodeType !== 1) continue;
        const el = node;
        if (SKIP.has(el.tagName)) continue;
        if (el.tagName === "BR") { text += " "; continue; }
        const role = el.tagName === "svg" || el.tagName === "SVG" ? (el.getAttribute("aria-label") ? "img" : null) : roleOf(el);
        if ((el.tagName === "svg" || el.tagName === "SVG") && !role) continue;
        if (!visible(el)) continue;
        const inline = !role && /^(SPAN|B|I|EM|STRONG|SMALL|MARK|ABBR|CITE|Q|S|U|SUB|SUP|TIME|FONT|BDI|BDO|DFN|KBD|SAMP|VAR|DATA)$/.test(el.tagName) && !el.shadowRoot && !clickable(el);
        if (inline && !el.querySelector("a,button,input,select,textarea,[role],[onclick],[tabindex],img,iframe")) {
          text += " " + el.innerText; // plain inline formatting: keep it in the running text
          continue;
        }
        flush();
        walkEl(el, role, depth);
      }
      flush();
    }

    function walkEl(el, role, depth) {
      if (!role && clickable(el) && clean(el.innerText || el.getAttribute("title"))) role = "clickable";
      if (!role) return walkChildren(el, depth); // generic container: flatten

      if (role === "iframe") {
        let doc = null;
        try { doc = el.contentDocument; } catch {}
        const name = nameOf(el, role);
        if (!doc?.body) return line(depth, `- iframe${name ? " " + q(name) : ""} (cross-origin, content not available)`);
        line(depth, `- iframe${name ? " " + q(name) : ""}:`);
        return walkChildren(doc.body, depth + 1);
      }

      const name = nameOf(el, role);
      const a = attrs(el, role);
      if (INTERACTIVE.has(role) || el.tagName === "SELECT") a.push("ref=" + refOf(el));
      const value = valueOf(el, role);
      let head = "- " + role + (name ? " " + q(name) : "") + a.map((x) => ` [${x}]`).join("");

      if (el.tagName === "SELECT" || el.tagName === "INPUT" || el.tagName === "TEXTAREA" || role === "img" || role === "separator") {
        return line(depth, head + (value ? ": " + value : ""));
      }
      const leaf = NAME_FROM_TEXT.has(role) &&
        !el.querySelector("a[href],button,input,select,textarea,[role=button],[role=link],[role=checkbox],[role=tab],[role=menuitem],[onclick],iframe,img[alt]:not([alt=''])") &&
        !el.shadowRoot;
      if (leaf) {
        if (role === "paragraph" || role === "code" || role === "listitem" || role === "cell" || role === "label") {
          return name ? line(depth, `- ${role}${a.map((x) => ` [${x}]`).join("")}: ${name}`) : undefined;
        }
        return line(depth, head + (value ? ": " + value : ""));
      }
      if (value) head += ": " + value;
      if (role === "paragraph" || role === "listitem" || role === "cell" || role === "label") head = "- " + role + a.map((x) => ` [${x}]`).join("");
      const start = out.length, startSize = size;
      line(depth, head + ":");
      const before = out.length;
      walkChildren(el, depth + 1);
      if (out.length === before && !INTERACTIVE.has(role) && !name) { out.length = start; size = startSize; } // empty landmark
      else if (out.length === before) out[start] = out[start].replace(/:$/, "");
    }

    walkChildren(document.body || document.documentElement, 0);
    return out.join("\n") || "- text: (empty page)";
  }

  // Box of the element in top-level viewport pixels, after scrolling it into view.
  function locate(r) {
    const el = get(r);
    if (el.scrollIntoViewIfNeeded) el.scrollIntoViewIfNeeded(true);
    else el.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
    let rect = el.getBoundingClientRect();
    if (rect.width === 0 && rect.height === 0 && el.getClientRects().length) rect = el.getClientRects()[0];
    let x = rect.left, y = rect.top;
    for (let w = el.ownerDocument.defaultView; w && w !== w.top && w.frameElement; w = w.parent) {
      const fr = w.frameElement.getBoundingClientRect();
      x += fr.left + w.frameElement.clientLeft;
      y += fr.top + w.frameElement.clientTop;
    }
    if (rect.width === 0 && rect.height === 0) throw new Error(`Element ${r} is not visible on the page.`);
    const vw = innerWidth, vh = innerHeight;
    // Click point: the center, kept inside the viewport.
    const cx = Math.min(Math.max(x + rect.width / 2, 1), vw - 1), cy = Math.min(Math.max(y + rect.height / 2, 1), vh - 1);
    let cover = "";
    if (el.ownerDocument === document) {
      const hit = document.elementFromPoint(cx, cy);
      if (hit && hit !== el && !el.contains(hit) && !(hit.tagName === "LABEL" && hit.control === el) && !(el.tagName === "INPUT" && el.labels && [...el.labels].some((l) => l.contains(hit)))) {
        cover = "<" + hit.tagName.toLowerCase() + (hit.id ? "#" + hit.id : "") + (typeof hit.className === "string" && hit.className ? "." + hit.className.trim().split(/\s+/).slice(0, 2).join(".") : "") + ">";
      }
    }
    const role = roleOf(el) || "element";
    return { x, y, w: rect.width, h: rect.height, cx, cy, vw, vh, cover, label: `${role}${nameOf(el, role) ? " " + q(nameOf(el, role)) : ""}` };
  }

  // Focuses a field; clear=true selects its content (typing replaces it), otherwise puts the caret at the end.
  function prepareInput(r, clear) {
    const el = get(r);
    el.focus();
    if (el.tagName === "INPUT" || el.tagName === "TEXTAREA") {
      try {
        if (clear) el.select();
        else el.setSelectionRange(el.value.length, el.value.length);
      } catch {}
      return !!el.value;
    }
    if (el.isContentEditable) {
      const s = getSelection(), range = document.createRange();
      range.selectNodeContents(el);
      if (!clear) range.collapse(false);
      s.removeAllRanges(); s.addRange(range);
      return !!el.textContent;
    }
    throw new Error(`Element ${r} is not a text field (${roleOf(el) || el.tagName.toLowerCase()}).`);
  }

  function selectOption(r, values) {
    const el = get(r);
    if (el.tagName !== "SELECT") throw new Error(`Element ${r} is not a <select>. Click it and pick the option from the snapshot instead.`);
    const opts = [...el.options], picked = [];
    const match = (o) => values.some((v) => v === o.value || v === o.label || v === o.text.trim());
    for (const o of opts) {
      const m = match(o);
      if (el.multiple) o.selected = m;
      else if (m && !picked.length) el.value = o.value;
      if (m && (el.multiple || !picked.length)) picked.push(o.label);
    }
    if (!picked.length) throw new Error(`No option matches ${values.join(", ")}. Options: ${opts.map((o) => o.label).join(" | ")}`);
    el.dispatchEvent(new Event("input", { bubbles: true }));
    el.dispatchEvent(new Event("change", { bubbles: true }));
    return picked;
  }

  const hasText = (s) => (document.body?.innerText || "").includes(s);

  // ---- Watching the page ----
  // busyAt: when the page last changed. watch: an ongoing wait for the page to change
  // ({ base: outline it is compared with, scope: element to watch or null, hit: what changed }).
  old?.observer?.disconnect();
  let busyAt = performance.now(), watch = null;
  const norm = (s) => s.replace(/\d+/g, "#"); // numbers alone (clocks, counters) don't count as a change

  function checkWatch() {
    if (!watch) return;
    watch.timer = 0;
    if (!watch.hit && norm(snapshot()) !== watch.base) watch.hit = "page";
  }
  // Starts waiting; base is the outline the agent last saw. Returns what already changed, or "".
  function watchStart(base, ref, selector) {
    watchStop();
    let scope = null;
    if (ref) scope = get(ref);
    else if (selector) {
      scope = document.querySelector(selector);
      if (!scope) throw new Error(`No element matches the selector ${selector}.`);
    }
    watch = { base: norm(base), scope, hit: "", timer: 0 };
    checkWatch(); // it may have changed before the wait started
    return watch.hit;
  }
  function watchState() {
    if (!watch) return "gone";
    if (!watch.hit && watch.scope && !watch.scope.isConnected) watch.hit = "element";
    return watch.hit;
  }
  function watchStop() {
    if (watch) clearTimeout(watch.timer);
    watch = null;
    return true;
  }
  function fileInput(r) {
    const el = get(r);
    const isFile = (x) => x && x.tagName === "INPUT" && x.type === "file";
    const found = isFile(el) ? el
      : el.querySelector?.("input[type=file]")
      || (isFile(el.control) ? el.control : null)
      || el.closest?.("label")?.control
      || el.closest?.("form")?.querySelector("input[type=file]")
      || (document.querySelectorAll("input[type=file]").length === 1 ? document.querySelector("input[type=file]") : null);
    if (!isFile(found)) throw new Error(`Ref ${r} is not a file input and no file input belongs to it. Snapshot the page and pick the upload control.`);
    return found;
  }
  const idleFor = () => Math.round(performance.now() - busyAt);

  const observer = new MutationObserver((ms) => {
    if (watch && !watch.hit && watch.scope && ms.some((m) => watch.scope.contains(m.target))) watch.hit = "element";
    if (!ms.some((m) => !timerLike(m.target))) return;
    busyAt = performance.now();
    if (watch && !watch.hit && !watch.timer) watch.timer = setTimeout(checkWatch, 80);
  });
  observer.observe(document, { subtree: true, childList: true, attributes: true, characterData: true });

  Object.defineProperty(window, "__abu", {
    value: { v: V, refs, ids, get n() { return n; }, get, snapshot, locate, prepareInput, selectOption, hasText,
      idleFor, watchStart, watchState, watchStop, observer, fileInput },
    configurable: true, enumerable: false, writable: true,
  });
})();
