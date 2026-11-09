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
