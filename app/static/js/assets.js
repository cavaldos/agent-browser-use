// =====================================================================
// ASSETS: what the browser produced while people and agents worked: screenshots, text copied in
// the shared browser, elements picked with the Pick tool, downloaded files. They are kept on the
// server (see app/src/assets.rs) and listed in the sidebar's Assets tab, newest first.
// =====================================================================
let assets = [], assetFilter = "all", assetSeq = 0, assetNew = 0;
const freshAssets = new Set(); // ids that arrived while the tab was open: they flash once

const KIND_LABEL = { screenshot: "Screenshot", copy: "Copied text", element: "Element", download: "Download" };
const KIND_ICON_A = { screenshot: "image", copy: "text", element: "pick", download: "download" };
const assetGroup = (a) => (a.kind === "screenshot" ? "screenshot" : a.kind === "download" ? "download" : "text");
const assetUrl = (a, dl) => `/api/assets/file?id=${encodeURIComponent(a.id)}${dl ? "&download=1" : ""}`;
const sizeOf = (n) => (n >= 1 << 20 ? (n / (1 << 20)).toFixed(1) + " MB" : n >= 1 << 10 ? (n / (1 << 10)).toFixed(1) + " KB" : n + " B");

// A new asset was made (by anyone): refresh the list, and count it on the tab when it is not showing.
function onAssets(m) {
  if (m.id && !(sidebarOpen() && pane === "assets")) {
    assetNew++;
    $("#assetsBadge").textContent = assetNew > 9 ? "9+" : assetNew;
    $("#assetsBadge").hidden = false;
  }
  if (m.id) freshAssets.add(m.id);
  if (pane === "assets") renderAssets();
}

async function renderAssets() {
  assetNew = 0;
  $("#assetsBadge").hidden = true;
  const seq = ++assetSeq;
  try { assets = (await api("/api/assets")).assets || []; } catch (e) { return fail("Couldn’t load the assets")(e); }
  if (seq !== assetSeq) return;
  const box = $("#assetList");
  const shown = assets.filter((a) => assetFilter === "all" || assetGroup(a) === assetFilter);
  const top = box.scrollTop;
  box.replaceChildren();
  $("#assetClearBtn").disabled = !assets.length;
  if (!shown.length) {
    box.innerHTML = `<div class="flex flex-col items-center gap-1 px-6 py-10 text-center">
      <div class="mb-2 grid h-12 w-12 place-items-center rounded-2xl bg-white/[.04] text-zinc-400 ring-1 ring-inset ring-white/[.06]">${svg("layers", "h-5 w-5")}</div>
      <p class="text-sm font-medium text-zinc-300">${assets.length ? "Nothing of this kind" : "No assets yet"}</p>
      <p class="text-xs text-zinc-500">${assets.length ? "Try another filter." : "Screenshots the agent takes, text you copy, elements you pick and files the browser downloads show up here."}</p></div>`;
    return;
  }
  for (const a of shown) box.appendChild(assetCard(a));
  box.scrollTop = top;
  freshAssets.clear();
}

function assetCard(a) {
  const card = document.createElement("div");
  card.className = "asset-card" + (freshAssets.has(a.id) ? " fresh" : "");
  const host = hostOf(a.url);
  card.innerHTML = `
    ${a.kind === "screenshot" ? `<img class="asset-thumb" loading="lazy" alt="">` : ""}
    <div class="space-y-2 p-2.5">
      <div class="flex items-center gap-2">
        <span class="asset-kind ${a.kind}">${svg(KIND_ICON_A[a.kind] || "file", "h-3 w-3")}${KIND_LABEL[a.kind] || a.kind}</span>
        <span class="min-w-0 flex-1 truncate text-[11.5px] text-zinc-500"></span>
      </div>
      ${a.kind === "copy" || a.kind === "element" ? `<pre class="asset-pre"></pre>` : ""}
      ${a.kind === "download" ? `<p class="dl-name truncate text-[13px] font-medium"></p>` : ""}
      <p class="from truncate text-xs text-zinc-500" ${host ? "" : "hidden"}></p>
      <div class="acts flex items-center justify-end gap-0.5"></div>
    </div>`;
  const by = a.by === "agent" ? "🤖 agent" : a.by === "browser" ? "" : a.by;
  $(".flex span:last-child", card).textContent = [by, ago(a.time), a.size ? sizeOf(a.size) : ""].filter(Boolean).join(" · ");
  if (a.kind === "screenshot") {
    const img = $("img", card);
    img.src = assetUrl(a);
    img.onclick = () => openLightbox(a);
  }
  if ($(".asset-pre", card)) {
    $(".asset-pre", card).textContent = a.text;
    $(".asset-pre", card).title = a.kind === "element" ? "Picked element" : "Copied text";
  }
  if ($(".dl-name", card)) { $(".dl-name", card).textContent = a.name; $(".dl-name", card).title = a.text || a.name; }
  const from = $(".from", card);
  from.textContent = a.title ? `${a.title} · ${host}` : host;
  from.title = a.url || "";
  const acts = $(".acts", card);
  const btn = (icon, title, fn) => {
    const b = document.createElement("button");
    b.className = "icon-btn h-7 w-7";
    b.title = title;
    b.innerHTML = svg(icon, "h-3.5 w-3.5");
    b.onclick = (e) => { e.stopPropagation(); fn(); };
    acts.appendChild(b);
    return b;
  };
  if (a.kind === "copy") btn("copy", "Copy the text", () => copyToClipboard(a.text));
  if (a.kind === "element") {
    btn("copy", "Copy the element id for an agent", () => { copyToClipboard(a.name); toast(`Copied ${a.name}: paste it to your agent`, "copy"); });
    btn("code", "Copy the HTML", () => copyToClipboard(a.text));
  }
  if (a.kind === "screenshot") {
    btn("external", "Open the picture", () => openLightbox(a));
    btn("copy", "Copy the picture", () => copyImage(a));
  }
  if (a.kind !== "element") btn("download", "Download", () => downloadAsset(a));
  btn("trash", "Remove from this list", () => removeAsset(a));
  return card;
}

function downloadAsset(a) {
  const l = document.createElement("a");
  l.href = assetUrl(a, true);
  l.download = a.name;
  l.click();
}
async function copyImage(a) {
  try {
    const blob = await (await fetch(assetUrl(a))).blob();
    // The clipboard takes PNG only: draw the JPEG on a canvas first.
    const bmp = await createImageBitmap(blob);
    const c = Object.assign(document.createElement("canvas"), { width: bmp.width, height: bmp.height });
    c.getContext("2d").drawImage(bmp, 0, 0);
    const png = await new Promise((ok) => c.toBlob(ok, "image/png"));
    await navigator.clipboard.write([new ClipboardItem({ "image/png": png })]);
    toast("Picture copied", "copy");
  } catch { toast("Your browser wouldn’t copy the picture: use Download instead", "info"); }
}
async function removeAsset(a) {
  try { await api("/api/assets/delete?id=" + encodeURIComponent(a.id), { method: "POST" }); } catch (e) { fail("Couldn’t remove it")(e); }
}

function openLightbox(a) {
  closeLightbox();
  const el = document.createElement("div");
  el.id = "assetLightbox";
  el.innerHTML = `
    <div class="flex h-12 shrink-0 items-center gap-2 px-4 text-sm text-zinc-200">
      <span class="min-w-0 flex-1 truncate"></span>
      <button data-a="copy" class="icon-btn h-8 w-8" title="Copy the picture">${svg("copy", "h-4 w-4")}</button>
      <button data-a="download" class="icon-btn h-8 w-8" title="Download">${svg("download", "h-4 w-4")}</button>
      <button data-a="close" class="icon-btn h-8 w-8" title="Close (Esc)">${svg("x", "h-4 w-4")}</button>
    </div>
    <img alt="">`;
  $("span", el).textContent = [a.name, a.title || hostOf(a.url)].filter(Boolean).join(" · ");
  $("img", el).src = assetUrl(a);
  el.onclick = (e) => {
    const act = e.target.closest("button")?.dataset.a;
    if (act === "copy") copyImage(a);
    else if (act === "download") downloadAsset(a);
    else if (act === "close" || e.target === el || e.target.tagName === "IMG") closeLightbox();
  };
  document.body.appendChild(el);
}
const closeLightbox = () => $("#assetLightbox")?.remove();
document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeLightbox(); });

$("#assetFilters").onclick = (e) => {
  const f = e.target.closest("button")?.dataset.f;
  if (!f) return;
  assetFilter = f;
  $$("#assetFilters .asset-chip").forEach((b) => b.classList.toggle("active", b.dataset.f === f));
  renderAssets();
};
$("#assetCaptureBtn").onclick = async () => {
  if (!state.enabled) return;
  try { await api("/api/assets/capture?by=" + encodeURIComponent(prefs.name || ""), { method: "POST" }); toast("Screenshot saved in Assets", "camera"); }
  catch (e) { fail("Couldn’t capture")(e); }
};
$("#assetClearBtn").onclick = async () => {
  if (!assets.length || !confirm("Remove all assets from this list? Downloaded files stay in the download folder.")) return;
  try { await api("/api/assets/clear", { method: "POST" }); } catch (e) { fail("Couldn’t clear")(e); }
};
