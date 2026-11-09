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
    renderMentions(line, c.text);
    if (live && c.user !== prefs.name && mentionsMe(c.text)) { toast(`${c.user} mentioned you`, "chat"); playMentionSound(); }
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
      setName(body.children[0].children[0], c.user);
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
const POPS = ["#helpPop", "#emotePop", "#membersPop", "#tunnelPop"];
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
