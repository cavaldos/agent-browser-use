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
