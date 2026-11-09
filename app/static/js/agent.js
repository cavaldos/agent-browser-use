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
  if (m.tabId && m.tabId !== state.active) return;
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
