// =====================================================================
// PICK TOOL: point at an element in the shared browser, get a code for it.
// Paste the code to a coding agent (it calls browser_get_element), or post it in the chat.
// =====================================================================
let picking = false, pickShown = null, lastHover = 0;
const pickBox = document.createElement("div");
pickBox.id = "pickBox";
pickBox.innerHTML = '<span class="lbl"></span>';
const pickPop = document.createElement("div");
pickPop.id = "pickPop";
pickPop.className = "pop rounded-xl border border-blue-400/60 bg-zinc-900/95 p-3 text-[13px] text-zinc-200 shadow-2xl backdrop-blur";
stage.append(pickBox, pickPop);

function pickLabel(p) {
  const t = (p.name || p.text || "").slice(0, 40);
  return `${p.role || p.tag}${t ? " “" + t + "”" : ""}`;
}
// Last few parts of a CSS path, e.g. "… > div > span".
function shortSel(sel) {
  const parts = sel.split(" > ");
  return parts.length > 3 ? "… > " + parts.slice(-3).join(" > ") : sel;
}

function setPicking(on) {
  picking = on;
  $("#pickBtn").classList.toggle("on", on);
  scr.classList.toggle("picking", on);
  if (!on) { pickBox.style.display = "none"; hidePickPop(); }
  else toast("Click an element to get its code (Esc to cancel)", "pick");
}
$("#pickBtn").onclick = () => setPicking(!picking);

// Show a rectangle (viewport pixels of the shared browser) over the screen.
function drawPickBox(p) {
  const r = scr.getBoundingClientRect(), o = stage.getBoundingClientRect();
  const kx = r.width / p.view.width, ky = r.height / p.view.height;
  Object.assign(pickBox.style, {
    display: "block", left: r.left - o.left + p.rect.x * kx + "px", top: r.top - o.top + p.rect.y * ky + "px",
    width: Math.max(p.rect.w * kx, 2) + "px", height: Math.max(p.rect.h * ky, 2) + "px",
  });
  $(".lbl", pickBox).textContent = pickLabel(p);
}
function hidePickPop() { pickPop.style.display = "none"; pickShown = null; }

// While picking, the screen must not forward the pointer to the page.
for (const ev of ["mousedown", "mouseup", "contextmenu"]) {
  scr.addEventListener(ev, (e) => { if (picking) { e.stopImmediatePropagation(); e.preventDefault(); } }, true);
}
addEventListener("mouseup", (e) => { if (picking) e.stopImmediatePropagation(); }, true);
addEventListener("mousemove", (e) => {
  if (!picking) return;
  e.stopImmediatePropagation(); // don't move the page's mouse either
  if (!inside(e) || pickShown) return;
  const now = Date.now();
  if (now - lastHover < 80) return;
  lastHover = now;
  send({ type: "pick", event: "hover", ...pos(e) });
}, true);
scr.addEventListener("click", (e) => {
  if (!picking) return;
  e.stopImmediatePropagation();
  e.preventDefault();
  if (pickShown) hidePickPop();
  send({ type: "pick", event: "click", ...pos(e) });
}, true);
addEventListener("keydown", (e) => { if (picking && e.key === "Escape") setPicking(false); }, true);

function onPick(m) {
  if (!picking) return;
  if (!m.pick) { if (m.event === "hover" && !pickShown) pickBox.style.display = "none"; return; }
  if (m.event === "hover") { if (!pickShown) drawPickBox(m.pick); return; }
  drawPickBox(m.pick);
  showPickPop(m.pick);
}

function agentPrompt(p) {
  return `Look at the element I picked in the browser: ${p.code}. Use the browser_get_element tool with code "${p.code}".`;
}
function showPickPop(p) {
  pickShown = p;
  pickPop.innerHTML = `
    <div class="mb-2 flex items-center gap-2"><span class="rounded-full bg-blue-500/20 px-2.5 py-1 font-mono text-[12px] font-semibold text-blue-300"></span>
      <span class="min-w-0 flex-1 truncate text-zinc-400" title=""></span>
      <button data-a="close" class="icon-btn h-6 w-6" title="Close">${svg("x", "h-3.5 w-3.5")}</button></div>
    <div class="mb-3 truncate font-mono text-[11px] text-zinc-500"></div>
    <div class="flex gap-2">
      <button data-a="copy" class="btn-primary h-8 flex-1 whitespace-nowrap px-2 text-[12px]">${svg("copy", "h-3.5 w-3.5 shrink-0")} Copy for agent</button>
      <button data-a="chat" class="btn-ghost h-8 flex-1 whitespace-nowrap px-2 text-[12px]">${svg("chat", "h-3.5 w-3.5 shrink-0")} Post in chat</button>
    </div>`;
  const [code, label] = pickPop.querySelectorAll("span");
  code.textContent = p.code; label.textContent = pickLabel(p);
  pickPop.children[1].textContent = shortSel(p.selector);
  pickPop.children[1].title = p.selector;
  pickPop.onclick = (e) => {
    const a = e.target.closest("button")?.dataset.a;
    if (a === "copy") copyToClipboard(agentPrompt(p));
    else if (a === "chat") { send({ type: "chat", text: `📍 ${p.code} · ${pickLabel(p)}` }); toast("Posted in the chat", "chat"); }
    else if (a !== "close") return;
    hidePickPop(); pickBox.style.display = "none";
  };
  const r = scr.getBoundingClientRect(), o = stage.getBoundingClientRect();
  const k = r.width / p.view.width;
  const left = Math.min(Math.max(r.left - o.left + (p.rect.x + p.rect.w) * k + 8, 8), o.width - 348);
  const top = Math.min(Math.max(r.top - o.top + p.rect.y * k, 8), o.height - 140);
  Object.assign(pickPop.style, { display: "block", left: left + "px", top: top + "px" });
}
