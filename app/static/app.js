const $ = (s, root = document) => root.querySelector(s);
const $$ = (s, root = document) => [...root.querySelectorAll(s)];

// Room state (from the server) and preferences of this device.
const state = { me: null, users: {}, ctrl: null, blocked: null, tabs: [], active: "", view: { width: 1280, height: 720, quality: 70 }, enabled: false, paused: false };
const prefs = {
  name: localStorage.getItem("name") || "",
  sound: localStorage.getItem("sound") !== "off",
  volume: +(localStorage.getItem("volume") ?? 0.8),
  scroll: +(localStorage.getItem("scroll") ?? 1),
  cursors: localStorage.getItem("cursors") !== "off",
  emotes: localStorage.getItem("emotes") !== "off",
  sidebar: localStorage.getItem("sidebar") ?? (innerWidth >= 1100 ? "open" : "closed"),
};
const save = (k, v) => { try { localStorage.setItem(k, v); } catch {} };
const narrow = matchMedia("(max-width: 899px)"); // the sidebar becomes a drawer (keep in sync with style.css)
const EMOTES = ["😍", "😴", "🤣", "🎉", "😱", "😄", "👍", "❤️"];

// =====================================================================
// ICONS (inline SVG, Lucide style)
// =====================================================================
const ICONS = {
  logo: '<rect x="2" y="3" width="20" height="14" rx="2"/><path d="M8 21h8M12 17v4"/><path d="m9 10 2 2 4-4"/>',
  globe: '<circle cx="12" cy="12" r="10"/><path d="M2 12h20"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/>',
  back: '<path d="m12 19-7-7 7-7"/><path d="M19 12H5"/>',
  forward: '<path d="M5 12h14"/><path d="m12 5 7 7-7 7"/>',
  reload: '<path d="M21 12a9 9 0 1 1-9-9c2.52 0 4.93 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/>',
  x: '<path d="M18 6 6 18M6 6l12 12"/>',
  search: '<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/>',
  lock: '<rect x="3" y="11" width="18" height="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>',
  unlock: '<rect x="3" y="11" width="18" height="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 9.9-1"/>',
  info: '<circle cx="12" cy="12" r="10"/><path d="M12 16v-4M12 8h.01"/>',
  volume: '<path d="M11 5 6 9H2v6h4l5 4V5z"/><path d="M15.54 8.46a5 5 0 0 1 0 7.07"/><path d="M19.07 4.93a10 10 0 0 1 0 14.14"/>',
  volumeX: '<path d="M11 5 6 9H2v6h4l5 4V5z"/><path d="m22 9-6 6M16 9l6 6"/>',
  play: '<path d="M6 4l14 8-14 8z" fill="currentColor"/>',
  pause: '<rect x="6" y="4" width="4" height="16" rx="1" fill="currentColor"/><rect x="14" y="4" width="4" height="16" rx="1" fill="currentColor"/>',
  expand: '<path d="M8 3H5a2 2 0 0 0-2 2v3M21 8V5a2 2 0 0 0-2-2h-3M16 21h3a2 2 0 0 0 2-2v-3M3 16v3a2 2 0 0 0 2 2h3"/>',
  shrink: '<path d="M8 3v3a2 2 0 0 1-2 2H3M21 8h-3a2 2 0 0 1-2-2V3M3 16h3a2 2 0 0 1 2 2v3M16 21v-3a2 2 0 0 1 2-2h3"/>',
  pip: '<rect x="2" y="4" width="20" height="16" rx="2"/><rect x="12" y="11" width="7" height="6" rx="1" fill="currentColor"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  send: '<path d="m22 2-7 20-4-9-9-4z"/><path d="M22 2 11 13"/>',
  arrowRight: '<path d="M5 12h14"/><path d="m12 5 7 7-7 7"/>',
  mouse: '<rect x="6" y="3" width="12" height="18" rx="6"/><path d="M12 7v4"/>',
  panel: '<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M15 3v18"/>',
  help: '<circle cx="12" cy="12" r="10"/><path d="M9.09 9a3 3 0 0 1 5.83 1c0 2-3 3-3 3M12 17h.01"/>',
  user: '<path d="M19 21v-2a4 4 0 0 0-4-4H9a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/>',
  userOff: '<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="m17 8 5 5M22 8l-5 5"/>',
  copy: '<rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>',
  chat: '<path d="M7.9 20A9 9 0 1 0 4 16.1L2 22Z"/>',
  settings: '<path d="M20 7h-9M14 17H5"/><circle cx="17" cy="17" r="3"/><circle cx="7" cy="7" r="3"/>',
  download: '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="M7 10l5 5 5-5"/><path d="M12 15V3"/>',
  upload: '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="M17 8l-5-5-5 5"/><path d="M12 3v12"/>',
  trash: '<path d="M3 6h18"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>',
  sun: '<circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41"/>',
  moon: '<path d="M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9z"/>',
  key: '<circle cx="7.5" cy="15.5" r="4.5"/><path d="m21 2-9.6 9.6"/><path d="m15.5 7.5 3 3L22 7l-3-3"/>',
};
function svg(name, cls = "") {
  return `<svg class="${cls}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICONS[name] || ""}</svg>`;
}
$$("i[data-icon]").forEach((i) => (i.outerHTML = svg(i.dataset.icon, i.className)));
$("#themeBtn").onclick = () => {
  const t = document.documentElement.dataset.theme === "light" ? "dark" : "light";
  document.documentElement.dataset.theme = t;
  save("theme", t);
};
const setIcon = (el, name, cls) => { if (el.dataset.icon !== name) { el.dataset.icon = name; el.innerHTML = svg(name, cls); } };

// =====================================================================
// SMALL HELPERS
// =====================================================================
function initials(name) {
  const p = name.trim().split(/\s+/);
  return ((p[0]?.[0] || "") + (p.length > 1 ? p[p.length - 1][0] : "")).toUpperCase() || "?";
}
function avatar(u, size = "h-8 w-8 text-[11px]") {
  const el = document.createElement("div");
  el.className = `grid ${size} shrink-0 select-none place-items-center rounded-full font-semibold text-zinc-950`;
  el.style.background = u.color;
  el.textContent = u.agent ? "🤖" : initials(u.name);
  el.title = u.name;
  return el;
}
function toast(text, icon = "info") {
  const box = $("#toasts");
  const el = document.createElement("div");
  el.className = "pop flex items-center gap-2.5 rounded-xl border border-white/10 bg-zinc-900/95 px-4 py-2.5 text-sm text-zinc-200 shadow-2xl shadow-black/50 backdrop-blur";
  el.innerHTML = svg(icon, "h-4 w-4 shrink-0 text-indigo-300") + "<span></span>";
  el.lastChild.textContent = text;
  box.appendChild(el);
  while (box.children.length > 3) box.firstChild.remove();
  setTimeout(() => {
    el.style.transition = "opacity .3s, transform .3s";
    el.style.opacity = "0";
    el.style.transform = "translateY(6px)";
    setTimeout(() => el.remove(), 300);
  }, 2400);
}
const timeOf = (t) => new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

// =====================================================================
// JOIN
// =====================================================================
$("#nameInput").value = prefs.name;
$("#loginForm").onsubmit = (e) => {
  e.preventDefault();
  prefs.name = $("#nameInput").value.trim();
  if (!prefs.name) return;
  save("name", prefs.name);
  ensureAudio(); // this click allows the page to play sound
  $("#login").hidden = true;
  $("#app").hidden = false;
  setSidebar(prefs.sidebar === "open" && !narrow.matches); // on phones the drawer starts closed
  renderSound(); renderPlay(); renderSettings(); renderCtrl(); layout();
  connect();
};
$("#leaveBtn").onclick = () => { localStorage.removeItem("name"); location.reload(); };

// =====================================================================
// WEBSOCKET (reconnects automatically)
// =====================================================================
let ws;
function connect() {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  ws = new WebSocket(`${proto}://${location.host}/ws?name=${encodeURIComponent(prefs.name)}`);
  ws.binaryType = "arraybuffer";
  ws.onopen = () => setStatus("Live", true);
  ws.onclose = () => {
    setStatus("Reconnecting…", false);
    state.users = {}; renderMembers(); $("#cursors").innerHTML = "";
    setTimeout(connect, 2000);
  };
  ws.onmessage = (e) => {
    if (typeof e.data === "string") return onMessage(JSON.parse(e.data));
    const kind = new Uint8Array(e.data, 0, 1)[0];
    if (kind === 1) drawFrame(e.data);
    else if (kind === 2) playAudio(e.data);
  };
}
const send = (m) => ws && ws.readyState === 1 && ws.send(JSON.stringify(m));
function setStatus(text, ok) {
  const s = $("#status");
  s.classList.toggle("ok", ok);
  $(".dot", s).className = "dot relative inline-flex h-2 w-2 rounded-full " + (ok ? "bg-emerald-400" : "bg-amber-400");
  $(".txt", s).textContent = text;
  s.title = ok ? "Connected" : text;
}

function onMessage(m) {
  switch (m.type) {
    case "init":
      state.me = m.you;
      state.users = {}; (m.users || []).forEach((u) => (state.users[u.id] = u));
      state.ctrl = m.ctrl || null;
      state.blocked = m.blocked || null;
      state.enabled = !!m.enabled;
      if (m.view) state.view = m.view;
      setTabs(m.tabs || [], m.active || "");
      if (!state.enabled) $("#loading span").textContent = "The shared browser is disabled on this server";
      $("#messages").innerHTML = ""; lastGroup = null;
      (m.chat || []).forEach((c) => addChat(c, false));
      renderChatEmpty();
      scrollChat(true);
      send({ type: "audio", on: prefs.sound });
      if (state.paused) send({ type: "pause", on: true });
      renderMembers(); renderCtrl(); renderBlock(); renderSettings(); layout();
      break;
    case "user_join":
      state.users[m.user.id] = m.user; renderMembers();
      toast(`${m.user.name} joined`, "user");
      break;
    case "user_leave": {
      const u = state.users[m.id];
      delete state.users[m.id]; renderMembers();
      document.getElementById("c-" + m.id)?.remove();
      if (u) toast(`${u.name} left`, "userOff");
      break;
    }
    case "control": {
      const prev = state.ctrl;
      state.ctrl = m.ctrl || null;
      renderCtrl();
      if (state.ctrl && state.ctrl.id !== prev?.id) {
        if (amCtrl()) toast("You’re in control", "mouse");
        else toast(`${state.ctrl.name} is in control`, "mouse");
      }
      break;
    }
    case "block":
      state.blocked = m.blocked || null;
      renderBlock();
      toast(state.blocked ? `${state.blocked.name} blocked the agent` : "The agent can take control again", state.blocked ? "lock" : "unlock");
      break;
    case "tabs": setTabs(m.tabs || [], m.active || ""); break;
    case "view": state.view = m.view; renderSettings(); layout(); break;
    case "chat": addChat(m.msg, true); break;
    case "emote": spawnEmote(m.text, m.user); break;
    case "cursor": moveCursor(m); break;
    case "agent": agentEvent(m); break;
    case "clipboard": copyToClipboard(m.text); break;
  }
}

// =====================================================================
// MEMBERS & CONTROL
// =====================================================================
const amCtrl = () => !!(state.ctrl && state.me && state.ctrl.id === state.me.id);
const canControl = () => amCtrl() || !state.ctrl; // nobody in control: acting takes control
const lockText = () => state.ctrl
  ? `${state.ctrl.name} is in control — click “Take over” below to drive`
  : "Click “Take control” below to drive";
function guard() {
  if (canControl()) return true;
  toast(lockText(), "lock");
  return false;
}

// Header: overlapping avatars (you first, then whoever drives) + a popover with everyone.
function renderMembers() {
  const me = state.me?.id, ctrl = state.ctrl?.id;
  const rank = (u) => (u.id === me ? 0 : u.id === ctrl ? 1 : 2);
  const list = Object.values(state.users).sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name));
  const MAX = narrow.matches ? 3 : 5;

  const stack = $("#members");
  stack.innerHTML = "";
  list.slice(0, MAX).forEach((u) => {
    const wrap = document.createElement("span");
    wrap.className = "member" + (u.id === ctrl ? " ctrl" : "");
    wrap.appendChild(avatar(u, "h-7 w-7 text-[10px]"));
    if (u.id === ctrl) {
      const badge = document.createElement("span");
      badge.className = "badge";
      badge.innerHTML = svg("mouse", "h-2 w-2");
      wrap.appendChild(badge);
    }
    stack.appendChild(wrap);
  });
  if (list.length > MAX) {
    const more = document.createElement("span");
    more.className = "member grid h-7 w-7 place-items-center rounded-full bg-zinc-800 text-[10px] font-semibold text-zinc-300";
    more.textContent = "+" + (list.length - MAX);
    stack.appendChild(more);
  }
  $("#memberCount").textContent = list.length;
  $("#membersBtn").title = `${list.length} ${list.length === 1 ? "person" : "people"} in the room`;

  const ul = $("#memberList");
  ul.innerHTML = "";
  list.forEach((u) => {
    const li = document.createElement("li");
    li.className = "flex items-center gap-2.5 rounded-lg px-2 py-1.5";
    li.appendChild(avatar(u, "h-7 w-7 text-[10px]"));
    const name = document.createElement("span");
    name.className = "min-w-0 flex-1 truncate text-[13px] text-zinc-200";
    name.textContent = u.name;
    if (u.id === me) name.insertAdjacentHTML("beforeend", ' <span class="text-zinc-500">(you)</span>');
    li.appendChild(name);
    if (u.id === ctrl) li.insertAdjacentHTML("beforeend", `<span class="flex items-center gap-1 rounded-full bg-emerald-500/15 px-2 py-0.5 text-[11px] font-medium text-emerald-300">${svg("mouse", "h-3 w-3")}In control</span>`);
    ul.appendChild(li);
  });
}

function renderCtrl() {
  const btn = $("#ctrlBtn"), icon = $(".ctrl-icon", btn);
  const mine = amCtrl(), other = state.ctrl && !mine;
  btn.className = "ctrl-btn " + (mine ? "mine" : other ? "other" : "free");
  if (other) {
    delete icon.dataset.icon; // so setIcon redraws the mouse later
    icon.textContent = state.ctrl.agent ? "🤖" : initials(state.ctrl.name);
    icon.style.background = state.ctrl.color;
  } else {
    icon.style.background = "";
    setIcon(icon, "mouse", "h-3.5 w-3.5");
  }
  $(".ctrl-status", btn).textContent = mine ? "You’re in control" : other ? `${state.ctrl.name} is in control` : "";
  $(".ctrl-action", btn).textContent = mine ? "Release" : other ? "Take over" : "Take control";
  btn.title = mine ? "Release control so someone else can drive" : other ? `${state.ctrl.name} is in control — click to take over` : "Take control of the shared browser";
  $("#stage").classList.toggle("locked", !canControl());
  $("#lockHint").textContent = mine ? "" : lockText();
  renderMembers();
}
$("#ctrlBtn").onclick = () => send({ type: amCtrl() ? "control_release" : "control_take" });

// The agent takes control by itself when it acts (and gives it back when idle), unless blocked here.
function renderBlock() {
  const btn = $("#blockBtn"), on = !!state.blocked;
  btn.classList.toggle("on", on);
  setIcon($(".block-icon", btn), on ? "lock" : "unlock", "h-3.5 w-3.5");
  $(".block-label", btn).textContent = on ? "Unblock agent" : "Block agent";
  btn.title = on
    ? `${state.blocked.name} blocked the AI agent: it can't take control. Click to let it again`
    : "Stop the AI agent from taking control (it takes control by itself when it acts)";
}
$("#blockBtn").onclick = () => send({ type: "agent_block", on: !state.blocked });
renderBlock();

// =====================================================================
// TABS
// =====================================================================
const tabEls = new Map();
let dragId = null, dropIndex = 0, focusOmniOnNewTab = false;
const activeTab = () => state.tabs.find((t) => t.id === state.active);
function tabTitle(t) {
  if (!t) return "";
  if (!t.url || t.url === "about:blank") return t.title && t.title !== "about:blank" ? t.title : "New Tab";
  return t.title || t.url;
}

function setTabs(tabs, active) {
  const changedActive = active !== state.active;
  state.tabs = tabs;
  state.active = active;
  renderTabs();
  renderToolbar();
  if (changedActive && focusOmniOnNewTab) {
    focusOmniOnNewTab = false;
    focusOmni();
  }
}

function renderTabs() {
  const list = $("#tablist");
  const ids = new Set(state.tabs.map((t) => t.id));
  for (const [id, el] of tabEls) {
    if (!ids.has(id)) {
      tabEls.delete(id);
      el.classList.add("closing");
      setTimeout(() => el.remove(), 200);
    }
  }
  const live = state.tabs.map((t) => {
    let el = tabEls.get(t.id);
    if (!el) { el = makeTab(t.id); tabEls.set(t.id, el); list.appendChild(el); }
    updateTab(el, t);
    return el;
  });
  // Reorder only when needed (moving a node restarts its animations).
  const current = [...list.children].filter((el) => !el.classList.contains("closing"));
  if (current.some((el, i) => el !== live[i])) {
    let prev = null;
    for (const el of live) {
      const want = prev ? prev.nextElementSibling : list.firstElementChild;
      if (want !== el) list.insertBefore(el, want);
      prev = el;
    }
  }
}

function makeTab(id) {
  const el = document.createElement("div");
  el.className = "tab opening";
  el.dataset.id = id;
  el.draggable = true;
  el.innerHTML = `<span class="tab-bg"></span><span class="sep"></span><span class="tab-icon"></span><span class="tab-title"></span>` +
    `<button class="tab-btn tab-audio" hidden></button><button class="tab-btn tab-close" title="Close tab (Alt+W)">${svg("x", "h-3.5 w-3.5")}</button>`;
  el.addEventListener("animationend", () => el.classList.remove("opening"), { once: true });
  el.addEventListener("mousedown", (e) => {
    if (e.button === 1) e.preventDefault(); // no auto-scroll
    if (e.button === 0 && !e.target.closest(".tab-btn")) activateTab(id);
  });
  el.addEventListener("auxclick", (e) => { if (e.button === 1) { e.preventDefault(); closeTab(id); } });
  $(".tab-close", el).onclick = (e) => { e.stopPropagation(); closeTab(id); };
  $(".tab-audio", el).onclick = (e) => { e.stopPropagation(); guard() && send({ type: "tab_mute", id }); };
  el.addEventListener("dragstart", (e) => {
    dragId = id;
    e.dataTransfer.effectAllowed = "move";
    e.dataTransfer.setData("text/plain", id);
    el.classList.add("dragging");
  });
  el.addEventListener("dragend", () => { dragId = null; el.classList.remove("dragging"); clearDrop(); });
  return el;
}

function updateTab(el, t) {
  el.classList.toggle("active", t.id === state.active);
  const title = tabTitle(t);
  el.title = title;
  const te = $(".tab-title", el);
  if (te.textContent !== title) te.textContent = title;

  const icon = $(".tab-icon", el);
  const key = t.loading ? "spin" : t.favicon || "globe";
  if (icon.dataset.key !== key) {
    icon.dataset.key = key;
    if (t.loading) icon.innerHTML = '<span class="tab-spin"></span>';
    else if (t.favicon) {
      const img = new Image();
      img.referrerPolicy = "no-referrer";
      img.onerror = () => { icon.innerHTML = svg("globe", "h-4 w-4 text-zinc-500"); };
      img.src = t.favicon;
      icon.replaceChildren(img);
    } else icon.innerHTML = svg("globe", "h-4 w-4 text-zinc-500");
  }

  const a = $(".tab-audio", el);
  a.hidden = !(t.audible || t.muted);
  if (!a.hidden) {
    setIcon(a, t.muted ? "volumeX" : "volume", "h-3.5 w-3.5");
    a.classList.toggle("muted", !!t.muted);
    a.title = t.muted ? "Unmute site" : "Mute site";
  }
}

function activateTab(id) { if (id !== state.active && guard()) { state.active = id; renderTabs(); renderToolbar(); send({ type: "tab_activate", id }); } }
function closeTab(id) {
  if (!id || !guard()) return;
  tabEls.get(id)?.classList.add("closing");
  send({ type: "tab_close", id });
}
function newTab() {
  if (!guard()) return;
  focusOmniOnNewTab = true;
  send({ type: "tab_new" });
}
$("#newTabBtn").onclick = newTab;
$("#tabstrip").addEventListener("dblclick", (e) => { if (!e.target.closest(".tab, .newtab")) newTab(); });

// Drag tabs to reorder.
function clearDrop() { $$(".drop-left, .drop-right").forEach((el) => el.classList.remove("drop-left", "drop-right")); }
$("#tablist").addEventListener("dragover", (e) => {
  if (!dragId) return;
  e.preventDefault();
  clearDrop();
  const others = state.tabs.filter((t) => t.id !== dragId).map((t) => tabEls.get(t.id));
  dropIndex = others.length;
  for (let i = 0; i < others.length; i++) {
    const r = others[i].getBoundingClientRect();
    if (e.clientX < r.left + r.width / 2) { dropIndex = i; others[i].classList.add("drop-left"); return; }
  }
  others.at(-1)?.classList.add("drop-right");
});
$("#tablist").addEventListener("drop", (e) => {
  e.preventDefault();
  clearDrop();
  if (!dragId || !guard()) return;
  const t = state.tabs.find((x) => x.id === dragId);
  const rest = state.tabs.filter((x) => x.id !== dragId);
  rest.splice(dropIndex, 0, t);
  state.tabs = rest; // show the new order right away
  renderTabs();
  send({ type: "tab_move", id: dragId, index: dropIndex });
});

// =====================================================================
// TOOLBAR & ADDRESS BAR
// =====================================================================
const urlInput = $("#urlInput");
let omniDirty = false; // the user typed in the address bar
urlInput.addEventListener("input", () => (omniDirty = true));
function displayURL(u) {
  if (!u || u === "about:blank") return "";
  const m = u.match(/^https?:\/\/(?:www\.)?(.*)$/);
  if (!m) return u;
  return m[1].replace(/^([^/?#]*)\/$/, "$1"); // "example.com/" -> "example.com"
}
function renderToolbar() {
  const t = activeTab();
  $("#backBtn").disabled = !t?.canBack;
  $("#fwdBtn").disabled = !t?.canFwd;
  const loading = !!t?.loading;
  setIcon($("#reloadBtn"), loading ? "x" : "reload", loading ? "h-[18px] w-[18px]" : "h-[16px] w-[16px]");
  $("#reloadBtn").title = loading ? "Stop" : "Reload (Ctrl+R)";
  if (document.activeElement !== urlInput) urlInput.value = displayURL(t?.url);
  else if (!omniDirty) { urlInput.value = t?.url && t.url !== "about:blank" ? t.url : ""; urlInput.select(); }
  renderUrlIcon(document.activeElement === urlInput ? "search" : null);
  document.title = t ? `${tabTitle(t)} · Agent Browser Use` : "Agent Browser Use";
}
function renderUrlIcon(force) {
  const u = activeTab()?.url || "";
  const kind = force || (u.startsWith("https://") ? "lock" : u.startsWith("http://") ? "info" : "search");
  const el = $("#urlIcon");
  setIcon(el, kind, "h-3.5 w-3.5");
  el.title = kind === "lock" ? "Connection is secure" : kind === "info" ? "Not secure" : "";
  el.className = "grid h-6 w-6 shrink-0 place-items-center rounded-full " + (kind === "info" ? "text-amber-400/90" : "text-zinc-400");
}
function focusOmni() {
  urlInput.focus();
  urlInput.select();
}
urlInput.addEventListener("focus", () => {
  omniDirty = false;
  const u = activeTab()?.url;
  urlInput.value = u && u !== "about:blank" ? u : "";
  urlInput.select();
  renderUrlIcon("search");
});
urlInput.addEventListener("blur", () => renderToolbar());
urlInput.addEventListener("keydown", (e) => {
  if (e.key === "Escape") { urlInput.blur(); kbd.focus({ preventScroll: true }); }
});
$("#urlForm").onsubmit = (e) => {
  e.preventDefault();
  if (!guard()) return;
  send({ type: "nav", url: urlInput.value });
  urlInput.blur();
  kbd.focus({ preventScroll: true });
};
$("#backBtn").onclick = () => guard() && send({ type: "back" });
$("#fwdBtn").onclick = () => guard() && send({ type: "forward" });
$("#reloadBtn").onclick = () => guard() && send({ type: activeTab()?.loading ? "stop" : "reload" });

// =====================================================================
// SHARED SCREEN
// =====================================================================
const stage = $("#stage"), win = $("#window"), scr = $("#screen"), kbd = $("#kbd");
const g = scr.getContext("2d", { alpha: false }); // opaque: cheaper to composite

// Decode one frame at a time. Frames that arrive meanwhile replace each other,
// so a slow device skips to the newest picture instead of falling behind.
let nextFrame = null, decoding = false;
function drawFrame(buf) {
  nextFrame = buf;
  if (!decoding) decodeFrames();
}
async function decodeFrames() {
  decoding = true;
  while (nextFrame) {
    const buf = nextFrame;
    nextFrame = null;
    try {
      const bmp = await createImageBitmap(new Blob([new Uint8Array(buf, 1)], { type: "image/jpeg" }));
      if (scr.width !== bmp.width || scr.height !== bmp.height) { scr.width = bmp.width; scr.height = bmp.height; }
      g.drawImage(bmp, 0, 0);
      bmp.close();
      if (!$("#loading").hidden) $("#loading").hidden = true;
    } catch {}
  }
  decoding = false;
}

// Size the browser window to fit the stage, keeping the screen's aspect ratio.
function layout() {
  const full = document.fullscreenElement === stage;
  const pad = full ? 0 : stage.clientWidth < 640 ? 8 : 20;
  const W = stage.clientWidth - pad * 2, H = stage.clientHeight - pad * 2;
  const chromeH = $("#tabstrip").offsetHeight + $("#toolbar").offsetHeight;
  if (W <= 0 || H <= chromeH) return;
  const k = Math.min(W / state.view.width, (H - chromeH) / state.view.height);
  const w = Math.floor(state.view.width * k), h = Math.floor(state.view.height * k);
  win.style.width = w + "px";
  scr.style.width = w + "px";
  scr.style.height = h + "px";
}
new ResizeObserver(layout).observe(stage);

// --- Mouse ---
const mods = (e) => (e.altKey ? 1 : 0) | (e.ctrlKey ? 2 : 0) | (e.metaKey ? 4 : 0) | (e.shiftKey ? 8 : 0);
function pos(e) {
  const r = scr.getBoundingClientRect();
  return { x: (e.clientX - r.left) / r.width, y: (e.clientY - r.top) / r.height };
}
function inside(e) {
  const r = scr.getBoundingClientRect();
  return e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom;
}

let mouseDown = false;
scr.addEventListener("mousedown", (e) => {
  e.preventDefault(); // keep focus on the keyboard catcher
  kbd.focus({ preventScroll: true });
  if (!guard()) return;
  mouseDown = true;
  send({ type: "mouse", event: "down", ...pos(e), button: e.button, buttons: e.buttons, count: e.detail || 1, mods: mods(e) });
});
addEventListener("mouseup", (e) => {
  if (!mouseDown) return;
  mouseDown = false;
  send({ type: "mouse", event: "up", ...pos(e), button: e.button, buttons: e.buttons, count: e.detail || 1, mods: mods(e) });
});
let pendingMove = null;
addEventListener("mousemove", (e) => {
  if (!canControl() || !(mouseDown || inside(e))) return;
  if (!pendingMove) requestAnimationFrame(() => { send(pendingMove); pendingMove = null; });
  pendingMove = { type: "mouse", event: "move", ...pos(e), buttons: e.buttons, mods: mods(e) };
});
let wheel = null;
scr.addEventListener("wheel", (e) => {
  e.preventDefault();
  if (!canControl()) return;
  const k = (e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 800 : 1) * prefs.scroll;
  if (!wheel) {
    wheel = { type: "mouse", event: "wheel", ...pos(e), dx: 0, dy: 0, mods: mods(e) };
    requestAnimationFrame(() => { send(wheel); wheel = null; });
  }
  wheel.dx += e.deltaX * k; wheel.dy += e.deltaY * k;
}, { passive: false });
scr.addEventListener("contextmenu", (e) => e.preventDefault());
scr.addEventListener("mouseenter", () => !canControl() && $("#lockHint").classList.add("show"));
scr.addEventListener("mouseleave", () => $("#lockHint").classList.remove("show"));

// --- Keyboard ---
kbd.addEventListener("focus", () => stage.classList.add("typing"));
kbd.addEventListener("blur", () => stage.classList.remove("typing"));

// Browser shortcuts handled here instead of being sent to the page.
// (Ctrl+T / Ctrl+W belong to your own browser, so the room uses Alt+T / Alt+W.)
function shortcut(e) {
  const mod = e.ctrlKey || e.metaKey;
  if (e.altKey && !mod && e.code === "KeyT") return newTab(), true;
  if (e.altKey && !mod && e.code === "KeyW") return closeTab(state.active), true;
  if (e.altKey && !mod && /^Digit[1-9]$/.test(e.code)) {
    const n = +e.code.slice(5);
    const t = n === 9 ? state.tabs.at(-1) : state.tabs[n - 1];
    if (t) activateTab(t.id);
    return true;
  }
  if (mod && !e.altKey && e.code === "KeyL") return focusOmni(), true;
  if ((mod && !e.shiftKey && e.code === "KeyR") || e.key === "F5") return guard() && send({ type: "reload" }), true;
  if ((e.altKey && e.key === "ArrowLeft") || (e.metaKey && e.code === "BracketLeft")) return guard() && send({ type: "back" }), true;
  if ((e.altKey && e.key === "ArrowRight") || (e.metaKey && e.code === "BracketRight")) return guard() && send({ type: "forward" }), true;
  return false;
}

const swallowed = new Set();
const isPaste = (e) => (e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "v";
kbd.addEventListener("keydown", (e) => {
  if (e.isComposing || e.keyCode === 229) return; // an IME (e.g. Vietnamese Telex) is composing
  if (shortcut(e)) { e.preventDefault(); swallowed.add(e.code); return; }
  if (isPaste(e)) return; // handled by the paste event
  e.preventDefault();
  if (!canControl()) return;
  send({ type: "key", event: "down", key: e.key, code: e.code, keyCode: e.keyCode, mods: mods(e) });
});
kbd.addEventListener("keyup", (e) => {
  if (e.isComposing || e.keyCode === 229) return;
  if (swallowed.delete(e.code) || isPaste(e)) return;
  e.preventDefault();
  if (!canControl()) return;
  send({ type: "key", event: "up", key: e.key, code: e.code, keyCode: e.keyCode, mods: mods(e) });
});
const sendText = (t) => t && canControl() && send({ type: "text", text: t });
kbd.addEventListener("compositionend", (e) => { sendText(e.data); kbd.value = ""; });
kbd.addEventListener("input", (e) => {
  if (e.isComposing) return;
  if (e.inputType === "insertText" && e.data) sendText(e.data);
  kbd.value = "";
});
kbd.addEventListener("paste", (e) => { e.preventDefault(); sendText(e.clipboardData.getData("text/plain")); });

// Text copied inside the shared browser -> this computer's clipboard.
function copyToClipboard(text) {
  const fallback = () => {
    const t = document.createElement("textarea");
    t.value = text; document.body.appendChild(t); t.select();
    try { document.execCommand("copy"); } catch {}
    t.remove(); kbd.focus({ preventScroll: true });
  };
  if (navigator.clipboard && window.isSecureContext) navigator.clipboard.writeText(text).catch(fallback);
  else fallback();
  toast("Copied to your clipboard", "copy");
}

// --- Fullscreen & picture-in-picture ---
$("#fullBtn").onclick = () => (document.fullscreenElement ? document.exitFullscreen() : stage.requestFullscreen());
document.addEventListener("fullscreenchange", () => {
  setIcon($("#fullBtn"), document.fullscreenElement ? "shrink" : "expand", "h-[18px] w-[18px]");
  $("#fullBtn").title = document.fullscreenElement ? "Exit fullscreen" : "Fullscreen";
  layout();
});
const pipVideo = $("#pipVideo");
if (document.pictureInPictureEnabled && scr.captureStream) $("#pipBtn").hidden = false;
$("#pipBtn").onclick = async () => {
  try {
    if (document.pictureInPictureElement) return await document.exitPictureInPicture();
    if (!pipVideo.srcObject) pipVideo.srcObject = scr.captureStream(30);
    await pipVideo.play();
    await pipVideo.requestPictureInPicture();
  } catch { toast("Picture-in-picture isn’t available here"); }
};

// =====================================================================
// SOUND & PAUSE
// Audio packet: [2][stream id][sample rate uint32 LE][int16 stereo PCM]. Each stream
// gets a small jitter buffer so its chunks play back to back.
// =====================================================================
let actx = null, gainNode = null;
const streams = {};
function ensureAudio() {
  if (!actx) {
    actx = new AudioContext();
    gainNode = actx.createGain();
    gainNode.gain.value = prefs.volume;
    gainNode.connect(actx.destination);
  }
  if (actx.state !== "running") actx.resume();
}
function playAudio(buf) {
  if (!prefs.sound || state.paused || !actx || actx.state !== "running") return;
  const dv = new DataView(buf);
  const id = dv.getUint8(1), rate = dv.getUint32(2, true);
  const pcm = new Int16Array(buf, 6, (buf.byteLength - 6) >> 1);
  const n = pcm.length >> 1;
  if (!n) return;
  const ab = actx.createBuffer(2, n, rate);
  const L = ab.getChannelData(0), R = ab.getChannelData(1);
  for (let i = 0; i < n; i++) { L[i] = pcm[2 * i] / 32768; R[i] = pcm[2 * i + 1] / 32768; }
  const s = (streams[id] ||= { t: 0 });
  const now = actx.currentTime;
  if (s.t < now + 0.02 || s.t > now + 1) s.t = now + 0.12; // (re)start with a small buffer
  const src = actx.createBufferSource();
  src.buffer = ab;
  src.connect(gainNode);
  src.start(s.t);
  s.t += ab.duration;
}
function renderSound() {
  setIcon($("#soundBtn"), prefs.sound && prefs.volume > 0 ? "volume" : "volumeX", "h-[18px] w-[18px]");
  $("#soundBtn").title = prefs.sound ? "Mute" : "Unmute";
  $("#soundBtn").classList.toggle("text-rose-400", !prefs.sound);
  $("#volume").value = prefs.volume;
  $("#volume").style.background = `linear-gradient(90deg, #818cf8 ${prefs.volume * 100}%, rgb(255 255 255 / .12) ${prefs.volume * 100}%)`;
}
$("#soundBtn").onclick = () => {
  prefs.sound = !prefs.sound;
  save("sound", prefs.sound ? "on" : "off");
  ensureAudio();
  send({ type: "audio", on: prefs.sound });
  renderSound();
};
$("#volume").oninput = (e) => {
  prefs.volume = +e.target.value;
  save("volume", prefs.volume);
  if (gainNode) gainNode.gain.setTargetAtTime(prefs.volume, actx.currentTime, 0.02);
  if (!prefs.sound && prefs.volume > 0) { prefs.sound = true; save("sound", "on"); send({ type: "audio", on: true }); }
  renderSound();
};

function setPaused(p) {
  state.paused = p;
  send({ type: "pause", on: p });
  if (!p) ensureAudio();
  renderPlay();
}
function renderPlay() {
  setIcon($("#playBtn"), state.paused ? "play" : "pause", "h-4 w-4" + (state.paused ? " ml-0.5" : ""));
  $("#playBtn").title = state.paused ? "Resume stream" : "Pause stream";
  $("#paused").hidden = !state.paused;
}
$("#playBtn").onclick = () => setPaused(!state.paused);
$("#resumeBtn").onclick = () => setPaused(false);

// =====================================================================
// EMOJI REACTIONS
// =====================================================================
for (const box of [$("#emoteList"), $("#emotePop")]) {
  EMOTES.forEach((em) => {
    const b = document.createElement("button");
    b.className = "emote-btn";
    b.textContent = em;
    b.onclick = () => sendEmote(em);
    box.appendChild(b);
  });
}
$("#emoteToggle").onclick = (e) => togglePop("#emotePop", e);
let lastEmote = 0;
function sendEmote(em) {
  if (Date.now() - lastEmote < 300) return;
  lastEmote = Date.now();
  send({ type: "emote", text: em });
}
function spawnEmote(em, user) {
  if (!prefs.emotes) return;
  const layer = $("#emoteLayer");
  const el = document.createElement("div");
  el.className = "emote";
  el.style.left = 10 + Math.random() * 80 + "%";
  el.style.setProperty("--sway", (Math.random() * 80 - 40).toFixed(0) + "px");
  el.style.setProperty("--rise", -(layer.clientHeight * (0.45 + Math.random() * 0.25)).toFixed(0) + "px");
  el.innerHTML = '<span class="e"></span><span class="n"></span>';
  $(".e", el).textContent = em;
  if (user) { $(".n", el).textContent = user.name; $(".n", el).style.background = user.color; }
  el.addEventListener("animationend", () => el.remove());
  layer.appendChild(el);
  while (layer.children.length > 40) layer.firstChild.remove();
}

// =====================================================================
// SIDEBAR: CHAT & SETTINGS
// =====================================================================
let pane = "chat", unread = 0;
const sidebarOpen = () => !$("#sidebar").classList.contains("collapsed");
function setSidebar(open, focus = false) {
  $("#sidebar").classList.toggle("collapsed", !open);
  $("#scrim").hidden = !open; // only shown on narrow screens (see style.css)
  if (!narrow.matches) { prefs.sidebar = open ? "open" : "closed"; save("sidebar", prefs.sidebar); }
  if (open && pane === "chat") { setUnread(0); scrollChat(true); if (focus && !narrow.matches) $("#chatInput").focus(); }
}
$("#sidebarToggle").onclick = () => setSidebar(!sidebarOpen(), true);
$("#sidebarClose").onclick = () => setSidebar(false);
$("#scrim").onclick = () => setSidebar(false);
$("#sidebar").addEventListener("keydown", (e) => { if (e.key === "Escape" && narrow.matches) setSidebar(false); });
narrow.addEventListener("change", () => { $("#scrim").hidden = !sidebarOpen(); renderMembers(); });

$$(".side-tab").forEach((b) => (b.onclick = () => showPane(b.dataset.pane)));
function showPane(p) {
  pane = p;
  $$(".side-tab").forEach((b) => b.classList.toggle("active", b.dataset.pane === p));
  $("#chatPane").hidden = p !== "chat";
  $("#settingsPane").hidden = p !== "settings";
  if (p === "settings") renderStates();
  if (p === "chat") { setUnread(0); scrollChat(true); }
}
const chatVisible = () => sidebarOpen() && pane === "chat";
function setUnread(n) {
  unread = n;
  $("#unread").hidden = !n;
  $("#unread").textContent = n > 9 ? "9+" : n;
}
function renderChatEmpty() {
  const empty = !$("#messages").children.length;
  $("#chatEmpty").hidden = !empty;
  $("#messages").hidden = empty;
}

let lastGroup = null; // consecutive messages from the same person are grouped
function scrollChat(force) {
  const box = $("#messages");
  if (force || box.scrollHeight - box.scrollTop - box.clientHeight < 120) box.scrollTop = box.scrollHeight;
}
function addChat(c, live) {
  const box = $("#messages");
  const stick = box.scrollHeight - box.scrollTop - box.clientHeight < 120;
  if (!c.user) {
    const el = document.createElement("div");
    el.className = "my-3 flex items-center gap-3 px-1 text-[11.5px] text-zinc-500" + (live ? " pop" : "");
    el.innerHTML = '<span class="h-px flex-1 bg-white/[.06]"></span><span class="max-w-[80%] text-center"></span><span class="h-px flex-1 bg-white/[.06]"></span>';
    el.children[1].textContent = `${c.text} · ${timeOf(c.time)}`;
    box.appendChild(el);
    lastGroup = null;
  } else {
    const line = document.createElement("p");
    line.className = "whitespace-pre-wrap break-words text-sm leading-relaxed text-zinc-200" + (live ? " fade-in" : "");
    line.textContent = c.text;
    if (lastGroup && lastGroup.user === c.user && c.time - lastGroup.time < 3 * 60 * 1000) {
      lastGroup.lines.appendChild(line);
      lastGroup.time = c.time;
    } else {
      const el = document.createElement("div");
      el.className = "mt-4 flex gap-2.5 first:mt-0" + (live ? " pop" : "");
      el.appendChild(avatar({ name: c.user, color: c.color || "#71717a", agent: c.agent }, "h-7 w-7 text-[10px] mt-0.5"));
      const body = document.createElement("div");
      body.className = "min-w-0 flex-1";
      body.innerHTML = '<div class="flex items-baseline gap-2"><span class="text-[13px] font-semibold"></span><span class="text-[11px] text-zinc-500"></span></div><div class="space-y-0.5"></div>';
      body.children[0].children[0].textContent = c.user;
      body.children[0].children[0].style.color = c.color;
      body.children[0].children[1].textContent = timeOf(c.time);
      body.children[1].appendChild(line);
      el.appendChild(body);
      box.appendChild(el);
      lastGroup = { user: c.user, time: c.time, lines: body.children[1] };
    }
    if (live && c.user !== prefs.name && !chatVisible()) setUnread(unread + 1);
  }
  if (live) renderChatEmpty();
  if (live && (stick || c.user === prefs.name)) scrollChat(true);
}
$("#chatForm").onsubmit = (e) => {
  e.preventDefault();
  const text = $("#chatInput").value.trim();
  if (text) send({ type: "chat", text });
  $("#chatInput").value = "";
};

// --- Settings ---
function renderSettings() {
  if (state.me) {
    $("#meAvatar").replaceChildren(avatar(state.me, "h-9 w-9 text-xs"));
    $("#meName").textContent = state.me.name;
  }
  const res = `${state.view.width}x${state.view.height}`;
  if (![...$("#resSelect").options].some((o) => o.value === res)) $("#resSelect").add(new Option(`${state.view.width} × ${state.view.height}`, res));
  $("#resSelect").value = res;
  const q = [...$("#qualSelect").options].reduce((a, o) => (Math.abs(o.value - state.view.quality) < Math.abs(a.value - state.view.quality) ? o : a));
  $("#qualSelect").value = q.value;
  $("#viewInfo").textContent = `${state.view.width}×${state.view.height}`;
  $("#scrollSpeed").value = prefs.scroll;
  $("#scrollVal").textContent = prefs.scroll + "×";
  $("#optCursors").checked = prefs.cursors;
  $("#optEmotes").checked = prefs.emotes;
}
function sendView(v) {
  if (!guard()) return renderSettings();
  send({ type: "view", view: { ...state.view, ...v } });
}
$("#resSelect").onchange = (e) => { const [w, h] = e.target.value.split("x").map(Number); sendView({ width: w, height: h }); };
$("#qualSelect").onchange = (e) => sendView({ quality: +e.target.value });
$("#scrollSpeed").oninput = (e) => { prefs.scroll = +e.target.value; save("scroll", prefs.scroll); renderSettings(); };
$("#optCursors").onchange = (e) => { prefs.cursors = e.target.checked; save("cursors", prefs.cursors ? "on" : "off"); if (!prefs.cursors) $("#cursors").innerHTML = ""; };
$("#optEmotes").onchange = (e) => { prefs.emotes = e.target.checked; save("emotes", prefs.emotes ? "on" : "off"); };

// ---- Saved logins (export / import of cookies + sessions) ----
async function renderStates() {
  const box = $("#stateList");
  if (!box) return;
  let states = [];
  try {
    const r = await fetch("/api/states");
    if (r.ok) states = (await r.json()).states || [];
  } catch { /* server may not be up */ }
  box.replaceChildren();
  if (!states.length) {
    const p = document.createElement("p");
    p.className = "text-xs text-zinc-500";
    p.textContent = "No saved logins yet. The agent saves one with browser_state after you log in.";
    box.appendChild(p);
    return;
  }
  for (const s of states) {
    const row = document.createElement("div");
    row.className = "flex items-center gap-2 rounded-xl border border-white/[.06] bg-white/[.02] p-2.5";
    const info = document.createElement("div");
    info.className = "min-w-0 flex-1";
    info.innerHTML = `<p class="truncate text-[13px] font-medium">${esc(s.name)}</p><p class="truncate text-xs text-zinc-500">${s.cookies} cookies · ${esc(s.origin || "—")}</p>`;
    row.appendChild(info);
    row.appendChild(stateBtn("key", "Load into the browser", () => loadState(s.name)));
    row.appendChild(stateBtn("download", "Export this login", () => { location.href = "/api/states/export?name=" + encodeURIComponent(s.name); }));
    row.appendChild(stateBtn("trash", "Delete", () => deleteState(s.name)));
    box.appendChild(row);
  }
}
function stateBtn(icon, title, onclick) {
  const b = document.createElement("button");
  b.className = "icon-btn h-7 w-7 shrink-0";
  b.title = title;
  b.innerHTML = svg(icon, "h-3.5 w-3.5");
  b.onclick = onclick;
  return b;
}
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
async function loadState(name) {
  try {
    const r = await fetch("/api/states/load", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ name }) });
    if (!r.ok) throw new Error(await r.text());
    toast(`Loaded “${name}” — you’re signed in`, "key");
  } catch (e) { toast(`Couldn’t load: ${String(e.message || e).trim()}`, "info"); }
}
async function deleteState(name) {
  if (!confirm(`Delete saved login “${name}”? This cannot be undone.`)) return;
  try {
    const r = await fetch("/api/states/delete", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ name }) });
    if (!r.ok) throw new Error(await r.text());
    toast(`Deleted “${name}”`, "trash");
    renderStates();
  } catch (e) { toast(`Couldn’t delete: ${String(e.message || e).trim()}`, "info"); }
}
$("#saveStateForm").onsubmit = async (e) => {
  e.preventDefault();
  const name = $("#saveStateName").value.trim();
  if (!name) return;
  try {
    const r = await fetch("/api/states/save", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ name }) });
    if (!r.ok) throw new Error(await r.text());
    $("#saveStateName").value = "";
    toast(`Saved “${name}”`, "key");
    renderStates();
  } catch (err) { toast(`Couldn’t save: ${String(err.message || err).trim()}`, "info"); }
};
$("#exportAllBtn").onclick = () => { location.href = "/api/states/export"; };
$("#importBtn").onclick = () => $("#importFile").click();
$("#importFile").onchange = async (e) => {
  const file = e.target.files[0];
  e.target.value = "";
  if (!file) return;
  const fd = new FormData();
  fd.append("file", file);
  try {
    const r = await fetch("/api/states/import", { method: "POST", body: fd });
    if (!r.ok) throw new Error(await r.text());
    const got = (await r.json()).imported || [];
    toast(`Imported ${got.length} login(s)`, "upload");
    renderStates();
  } catch (err) { toast(`Import failed: ${String(err.message || err).trim()}`, "info"); }
};

// Popovers: one open at a time, a click anywhere else closes them.
const POPS = ["#helpPop", "#emotePop", "#membersPop"];
function togglePop(sel, e) {
  e.stopPropagation();
  const show = $(sel).hidden;
  POPS.forEach((p) => ($(p).hidden = true));
  $(sel).hidden = !show;
}
$("#helpBtn").onclick = (e) => togglePop("#helpPop", e);
$("#membersBtn").onclick = (e) => togglePop("#membersPop", e);
document.addEventListener("click", (e) => {
  POPS.forEach((p) => { if (!e.target.closest(p)) $(p).hidden = true; });
});

// =====================================================================
// OTHER PEOPLE'S CURSORS (over the shared screen, relative to it)
// =====================================================================
let lastCursor = 0, cursorOut = true;
document.addEventListener("mousemove", (e) => {
  const now = Date.now();
  if (now - lastCursor < 50) return; // max 20 updates per second
  lastCursor = now;
  if (inside(e)) { cursorOut = false; send({ type: "cursor", area: "b", ...pos(e) }); }
  else if (!cursorOut) { cursorOut = true; send({ type: "cursor", area: "" }); }
});
function moveCursor(m) {
  const u = state.users[m.id];
  if (!u || (!prefs.cursors && !u.agent)) return; // the agent's cursor always shows
  let el = document.getElementById("c-" + m.id);
  if (!el) {
    el = document.createElement("div");
    el.className = "cursor" + (u.agent ? " agent" : "");
    el.id = "c-" + m.id;
    el.innerHTML = `<svg width="18" height="18" viewBox="0 0 24 24"><path d="m3 3 7.07 16.97 2.51-7.39 7.39-2.51L3 3z" fill="${u.color}" stroke="#09090b" stroke-width="1.5" stroke-linejoin="round"/></svg><span class="tag"></span>`;
    $(".tag", el).textContent = u.agent ? "🤖 " + u.name : u.name;
    $(".tag", el).style.background = u.color;
    $("#cursors").appendChild(el);
  }
  if (m.area !== "b") { el.style.opacity = "0"; return; }
  const r = scr.getBoundingClientRect(), o = stage.getBoundingClientRect(); // the layer covers the stage
  el.style.opacity = "1";
  el.style.transform = `translate(${r.left - o.left + (m.x || 0) * r.width}px, ${r.top - o.top + (m.y || 0) * r.height}px)`;
}

// =====================================================================
// AI AGENT: what it is doing, drawn over the shared screen
// (its cursor is a normal room cursor; see moveCursor)
// =====================================================================
function screenBox() {
  const r = scr.getBoundingClientRect(), o = stage.getBoundingClientRect();
  return { l: r.left - o.left, t: r.top - o.top, w: r.width, h: r.height };
}
let agentTimer = 0;
function agentEvent(m) {
  const s = screenBox();
  if (m.act === "click") {
    const el = document.createElement("div");
    el.className = "agent-ripple";
    el.style.left = s.l + (m.x || 0) * s.w + "px";
    el.style.top = s.t + (m.y || 0) * s.h + "px";
    $("#agentLayer").appendChild(el);
    setTimeout(() => el.remove(), 700);
    return;
  }
  if (m.act !== "step" && m.act !== "say") return;

  const cap = $("#agentCaption"), box = $("#agentBox");
  $(".txt", cap).textContent = m.text || "";
  cap.classList.toggle("say", m.act === "say");
  cap.style.left = s.l + s.w / 2 + "px";
  cap.style.top = s.t + 12 + "px";
  cap.style.maxWidth = Math.max(160, s.w - 24) + "px";
  cap.classList.add("show");

  if (m.rect) { // outline the element, clipped to the screen
    const x1 = Math.max(0, m.rect.x), y1 = Math.max(0, m.rect.y);
    const x2 = Math.min(1, m.rect.x + m.rect.w), y2 = Math.min(1, m.rect.y + m.rect.h);
    const pad = 3;
    box.style.left = s.l + x1 * s.w - pad + "px";
    box.style.top = s.t + y1 * s.h - pad + "px";
    box.style.width = Math.max(0, x2 - x1) * s.w + pad * 2 + "px";
    box.style.height = Math.max(0, y2 - y1) * s.h + pad * 2 + "px";
    box.classList.add("show");
  } else if (m.act === "step") {
    box.classList.remove("show");
  }
  clearTimeout(agentTimer);
  agentTimer = setTimeout(() => { cap.classList.remove("show"); box.classList.remove("show"); }, m.act === "say" ? 8000 : 4000);
}

// Opened as /?join (the agent opens the viewer this way): join right away.
if (new URLSearchParams(location.search).has("join")) {
  history.replaceState(null, "", location.pathname);
  $("#nameInput").value = prefs.name || "Viewer";
  $("#loginForm").requestSubmit();
}
