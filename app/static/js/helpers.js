// =====================================================================
// SMALL HELPERS
// =====================================================================
function initials(name) {
  const p = name.trim().split(/\s+/);
  return ((p[0]?.[0] || "") + (p.length > 1 ? p[p.length - 1][0] : "")).toUpperCase() || "?";
}
// setName writes a name into el, with an agent's "#3f9a" id dimmed (opencode#3f9a).
function setName(el, name, prefix = "") {
  const i = name.lastIndexOf("#");
  el.textContent = prefix + (i > 0 ? name.slice(0, i) : name);
  if (i > 0) {
    const id = document.createElement("span");
    id.className = "agent-id";
    id.textContent = name.slice(i);
    el.append(id);
  }
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
