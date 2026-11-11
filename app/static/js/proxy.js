// Browser IP: the header always shows the agent browser's public IP; the switch sends it
// through the proxy (WARP) or back to this machine's own connection.
let proxyState = { configured: false, on: false };
let shownWarpError = "";

function renderProxy() {
  const s = proxyState;
  const sw = $("#ipSwitch");
  sw.hidden = !s.configured;
  sw.setAttribute("aria-checked", String(!!s.on));
  sw.style.background = s.on ? "#10b981" : "";
  sw.disabled = !s.canToggle;
  $("#ipKnob").style.transform = s.on ? "translateX(16px)" : "";
  const via = !s.configured ? "" : s.on ? (s.warp === "on" || s.warp === "plus" ? " · WARP" : " · proxy") : " · direct";
  $("#ipText").textContent = s.checking ? (s.on ? "Connecting WARP…" : "IP…")
    : s.error ? "IP unavailable" : (s.ip || "IP…") + (s.loc ? " " + s.loc : "") + via;
  $("#ipChip").classList.toggle("text-emerald-400", !!s.on && !s.error);
  if (s.warpError && s.warpError !== shownWarpError) toast("WARP: " + s.warpError, "error");
  shownWarpError = s.warpError || "";
  $("#ipChip").title = s.warpError ? "WARP could not start: " + s.warpError
    : s.error ? "Could not read the IP: " + s.error
    : !s.configured ? "Public IP of the agent browser (this machine's own). Start with -proxy to get a switch."
    : s.on ? "Agent browser goes out through WARP — this machine's own IP is unchanged" : "Agent browser uses this machine's own IP. Switch on for a WARP IP";
}

async function proxyCall(body) {
  try {
    const r = await fetch("/api/proxy", body ? { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) } : {});
    if (!r.ok) throw new Error(await r.text());
    proxyState = await r.json();
  } catch (err) { proxyState = { ...proxyState, checking: false, error: String(err.message || err).trim() }; }
  renderProxy();
}

$("#ipSwitch").onclick = () => {
  proxyState = { ...proxyState, on: !proxyState.on, checking: true, error: "" };
  renderProxy();
  proxyCall({ on: proxyState.on });
};
proxyCall();
setInterval(proxyCall, 5000);
