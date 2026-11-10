// =====================================================================
// SHARED SCREEN
// =====================================================================
const stage = $("#stage"), win = $("#window"), scr = $("#screen"), kbd = $("#kbd");
const g = scr.getContext("2d", { alpha: false }); // opaque: cheaper to composite
g.imageSmoothingQuality = "high";

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
      if (scr.width !== bmp.width || scr.height !== bmp.height) { scr.width = bmp.width; scr.height = bmp.height; g.imageSmoothingQuality = "high"; }
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
