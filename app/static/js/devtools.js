// =====================================================================
// INSPECT: Chrome DevTools for the tab on screen, docked under the shared screen.
// The front end is Chrome's own, served through this server (see devtools.go), so it works
// behind the room's password and through the public link too.
// =====================================================================
let devOn = false, devFor = "";
const devPanel = $("#devPanel"), devFrame = $("#devFrame");

const devUrl = (id) => {
  const sock = location.protocol === "https:" ? "wss" : "ws";
  return `/devtools/inspector.html?${sock}=${location.host}/devtools/page/${encodeURIComponent(id)}`;
};

function devShow() {
  const tab = state.tabs.find((t) => t.id === state.active);
  if (!tab) return;
  $("#devTarget").textContent = tab.title || tab.url || "";
  if (devFor !== tab.id) { devFor = tab.id; devFrame.src = devUrl(tab.id); } // DevTools is bound to one tab: follow the active one
}

async function setInspect(on) {
  if (on) {
    if (!state.enabled) return;
    try {
      const d = await api("/api/devtools");
      if (!d.ok) return toast(`DevTools unavailable: ${d.error}`, "info");
    } catch (e) { return fail("DevTools unavailable")(e); }
    devPanel.style.height = Math.round(+(localStorage.getItem("devh") || 320)) + "px";
  }
  devOn = on;
  devPanel.hidden = !on;
  $("#inspectBtn").classList.toggle("on", on);
  if (on) devShow();
  else { devFor = ""; devFrame.src = "about:blank"; } // frees the DevTools session
}

$("#inspectBtn").onclick = () => setInspect(!devOn);
$("#devClose").onclick = () => setInspect(false);
$("#devPop").onclick = () => { if (devFor) window.open(devUrl(devFor), "_blank", "noopener"); };
window.addEventListener("active-tab-changed", () => { if (devOn) devShow(); });

// Drag the top edge to resize. The iframe would swallow the mouse, so a shield covers it meanwhile.
$("#devResize").onpointerdown = (e) => {
  e.preventDefault();
  const startY = e.clientY, startH = devPanel.offsetHeight, max = innerHeight - 220;
  $("#devShield").hidden = false;
  const move = (ev) => { devPanel.style.height = Math.max(120, Math.min(max, startH + startY - ev.clientY)) + "px"; };
  const up = () => {
    $("#devShield").hidden = true;
    removeEventListener("pointermove", move);
    removeEventListener("pointerup", up);
    try { localStorage.setItem("devh", devPanel.offsetHeight); } catch {}
  };
  addEventListener("pointermove", move);
  addEventListener("pointerup", up);
};
