// =====================================================================
// OTHER PEOPLE'S CURSORS (over the shared screen, relative to it)
// =====================================================================
let lastCursor = 0, cursorOut = true;
const agentCursorState = new Map();
const cursorVisibleOnActiveTab = (m) => !m.tabId || m.tabId === state.active;
document.addEventListener("mousemove", (e) => {
  const now = Date.now();
  if (now - lastCursor < 50) return; // max 20 updates per second
  lastCursor = now;
  if (inside(e)) { cursorOut = false; send({ type: "cursor", area: "b", ...pos(e) }); }
  else if (!cursorOut) { cursorOut = true; send({ type: "cursor", area: "" }); }
});
function moveCursor(m) {
  if (m.workerId || (m.id || "").startsWith("agent:")) {
    if (m.workerId && !state.users["agent:" + cursorHandle(m)]) return; // its agent is not in the room (it left)
    agentCursorState.set(m.id, m);
  }
  const u = state.users[m.id] || (m.workerId ? { id: m.id, name: m.workerId, color: "#a78bfa", agent: true } : null);
  if (!u || (!prefs.cursors && !u.agent)) return; // the agent's cursor always shows
  let el = document.getElementById("c-" + m.id);
  if (!el) {
    el = document.createElement("div");
    el.className = "cursor" + (u.agent ? " agent" : "");
    el.id = "c-" + m.id;
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("width", "18"); svg.setAttribute("height", "18"); svg.setAttribute("viewBox", "0 0 24 24");
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path.setAttribute("d", "m3 3 7.07 16.97 2.51-7.39 7.39-2.51L3 3z");
    path.setAttribute("fill", u.color); path.setAttribute("stroke", "#09090b"); path.setAttribute("stroke-width", "1.5"); path.setAttribute("stroke-linejoin", "round");
    svg.appendChild(path);
    const tag = document.createElement("span");
    tag.className = "tag";
    setName(tag, u.name, u.agent ? "🤖 " : "");
    tag.style.background = u.color;
    el.append(svg, tag);
    $("#cursors").appendChild(el);
  }
  if (m.area !== "b" || !cursorVisibleOnActiveTab(m)) { el.style.opacity = "0"; return; }
  const r = scr.getBoundingClientRect(), o = stage.getBoundingClientRect(); // the layer covers the stage
  el.style.opacity = "1";
  el.style.transform = `translate(${r.left - o.left + (m.x || 0) * r.width}px, ${r.top - o.top + (m.y || 0) * r.height}px)`;
}
// The @handle a cursor belongs to: its worker is "opencode#3f9a" or a sub-agent "opencode#3f9a/painter-1".
const cursorHandle = (m) => (m.workerId || "").split("/")[0];
// removeAgentCursors drops the cursors of an agent that left (and of its sub-agents).
function removeAgentCursors(handle) {
  for (const [id, m] of agentCursorState) {
    if (cursorHandle(m) !== handle) continue;
    agentCursorState.delete(id);
    document.getElementById("c-" + id)?.remove();
  }
}
function refreshAgentCursors() {
  for (const m of agentCursorState.values()) moveCursor(m);
}
window.addEventListener("active-tab-changed", refreshAgentCursors);
