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
$("#urlForm").onsubmit = (e) => {
  e.preventDefault();
  if (!guard()) return;
  send({ type: "nav", url: urlInput.value });
  urlInput.blur();
  kbd.focus({ preventScroll: true });
};
$("#backBtn").onclick = () => guard() && send({ type: "back" });
$("#fwdBtn").onclick = () => guard() && send({ type: "forward" });
$("#reloadBtn").onclick = () => guard() && send({ type: activeTab()?.loading ? "stop" : "reload" });
