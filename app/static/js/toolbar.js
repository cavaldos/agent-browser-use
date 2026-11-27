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
  storeInstaller.renderHint(t?.url);
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
$("#urlForm").onsubmit = async (e) => {
  e.preventDefault();
  if (!guard()) return;
  const value = urlInput.value;
  const listing = storeListing(value);
  if (listing && await storeInstaller.available) {
    storeInstaller.open(listing);
    return;
  }
  send({ type: "nav", url: value });
  urlInput.blur();
  kbd.focus({ preventScroll: true });
};
$("#backBtn").onclick = () => guard() && send({ type: "back" });
$("#fwdBtn").onclick = () => guard() && send({ type: "forward" });
$("#reloadBtn").onclick = () => guard() && send({ type: activeTab()?.loading ? "stop" : "reload" });

function storeListing(value) {
  const text = value?.trim() || "";
  try {
    const url = new URL(/^chrome(?:webstore)?\.google\.com\//i.test(text) ? `https://${text}` : text);
    if (url.protocol !== "https:" || url.username || url.password || url.port) return "";
    let path = url.pathname;
    if (url.hostname === "chrome.google.com" && path.startsWith("/webstore/")) path = path.slice("/webstore".length);
    else if (url.hostname !== "chromewebstore.google.com") return "";
    return /^\/detail\/(?:[^/]+\/)?[a-p]{32}\/*$/.test(path) ? url.href : "";
  } catch { return ""; }
}

// Chrome's native permission dialog is not part of a tab screencast.
const storeInstaller = (() => {
  const button = $("#storeInstallBtn"), dialog = $("#storeInstallDialog"), form = $("#storeInstallForm");
  const input = $("#storeInstallValue"), status = $("#storeInstallStatus"), img = $("#storeInstallFrame");
  const preview = $("#storeInstallPreview"), cursor = $("#storeInstallCursor");
  const done = $("#storeInstallDone"), cancel = $("#storeInstallCancel");
  const hint = $("#storeInstallHint"), hintText = $("#storeInstallHintText"), installHere = $("#storeInstallHere");
  const local = location.protocol === "http:" && ["localhost", "127.0.0.1", "[::1]"].includes(location.hostname);
  let ws, imageURL, finishing, position, managing, enabled = false;
  const renderHint = (value) => {
    const listing = storeListing(value);
    hint.hidden = !listing;
    if (!listing) return;
    installHere.hidden = !enabled;
    if (enabled) hintText.textContent = "The Store tab cannot show Chrome’s install dialog. Use the viewer installer instead.";
    else if (local) hintText.textContent = "Installing in the viewer requires the macOS build using Google Chrome.";
    else hintText.textContent = "To install, open the viewer at localhost on the machine running Agent Browser Use. The installer is not available on remote links.";
  };
  const post = (message) => { if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify(message)); };
  const finish = (enable) => {
    if (!ws || ws.readyState !== WebSocket.OPEN) { dialog.close(); return; }
    if (finishing) return;
    finishing = true;
    done.disabled = cancel.disabled = true;
    status.textContent = "Returning to the shared browser…";
    post({ type: "finish", enable });
  };
  cancel.onclick = () => finish(false);
  done.onclick = () => finish(true);
  dialog.addEventListener("cancel", (event) => { event.preventDefault(); finish(false); });
  dialog.addEventListener("close", () => {
    ws?.close(); ws = null;
    if (imageURL) URL.revokeObjectURL(imageURL);
    imageURL = null; img.removeAttribute("src");
  });
  const open = (listing) => {
    input.value = listing;
    status.textContent = "";
    preview.hidden = done.hidden = true;
    form.hidden = done.disabled = cancel.disabled = finishing = false;
    position = [0.5, 0.5]; cursor.hidden = true;
    cancel.textContent = "Close";
    dialog.showModal();
    input.focus();
  };
  button.onclick = installHere.onclick = () => open(storeListing(activeTab()?.url));
  const start = (request) => {
    managing = request.type === "manage";
    form.hidden = true;
    cancel.textContent = managing ? "Return to viewer" : "Cancel and return";
    status.textContent = managing ? "Opening Chrome’s extensions manager…" : "Opening Chrome’s native installer…";
    const socket = ws = new WebSocket(`ws://${location.host}/ws/extension-install`);
    const poll = () => { if (ws === socket && !finishing) post({ type: "frame" }); };
    socket.onopen = () => { post(request); poll(); };
    socket.onmessage = ({ data }) => {
      if (ws !== socket) return;
      if (typeof data === "string") {
        const message = JSON.parse(data);
        if (!message.done) { setTimeout(poll, 200); return; }
        finishing = true;
        if (!message.error) { dialog.close(); focusOmni(); return; }
        status.textContent = message.error;
        done.hidden = preview.hidden = true;
        cancel.disabled = false; cancel.textContent = "Close";
        ws = null; socket.close();
        return;
      }
      if (imageURL) URL.revokeObjectURL(imageURL);
      imageURL = URL.createObjectURL(data);
      img.src = imageURL;
      if (preview.hidden) {
        preview.hidden = false;
        done.hidden = managing;
        status.textContent = managing
          ? "Scroll to see your installed extensions. Choose Remove and confirm in Chrome. Returning to the viewer won’t undo a removal."
          : "Choose Add to Chrome, review the permissions, and confirm in the preview below. When finished, return to the viewer.";
      }
      setTimeout(poll, 25);
    };
    socket.onclose = () => {
      if (ws !== socket) return;
      ws = null;
      status.textContent = "The Chrome extensions session disconnected.";
      done.hidden = preview.hidden = true;
      cancel.disabled = false; cancel.textContent = "Close";
    };
  };
  form.onsubmit = (event) => {
    event.preventDefault();
    start({ type: "start", value: input.value.trim() });
  };
  $("#storeManageBtn").onclick = () => start({ type: "manage" });
  const press = (x, y) => { if (!finishing) post({ type: "click", x, y }); };
  const scroll = (delta_y) => { if (managing && !finishing) post({ type: "scroll", delta_y }); };
  img.addEventListener("wheel", (event) => {
    if (!managing) return;
    event.preventDefault();
    scroll(event.deltaY * (event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? 400 : 1));
  }, { passive: false });
  img.onclick = (event) => {
    const r = img.getBoundingClientRect();
    press((event.clientX - r.left) / r.width, (event.clientY - r.top) / r.height);
  };
  img.onkeydown = (event) => {
    if (managing && (event.key === "PageUp" || event.key === "PageDown")) {
      event.preventDefault();
      scroll(event.key === "PageUp" ? -400 : 400);
      return;
    }
    const delta = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] }[event.key];
    if (event.key === "Enter") { event.preventDefault(); press(...position); return; }
    if (!delta) return;
    event.preventDefault();
    position = position.map((v, i) => Math.min(0.999, Math.max(0, v + delta[i] * (event.shiftKey ? 0.005 : 0.02))));
    cursor.style.left = `${position[0] * 100}%`;
    cursor.style.top = `${position[1] * 100}%`;
    cursor.hidden = false;
  };
  const available = local
    ? fetch("/api/extension-install", { signal: AbortSignal.timeout(3000) }).then((res) => res.status === 204, () => false)
    : Promise.resolve(false);
  available.then((ok) => {
    enabled = ok;
    button.hidden = !ok;
    renderHint(activeTab()?.url);
  });
  return { available, open, renderHint };
})();
