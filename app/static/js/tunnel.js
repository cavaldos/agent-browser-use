// Public link: the header button switches the built-in tunnel on/off and shows the link to copy.
let tunnelState = { on: false, canToggle: true };

function renderTunnel() {
  const s = tunnelState;
  const sw = $("#tunnelSwitch");
  sw.setAttribute("aria-checked", String(s.on || !!s.busy));
  sw.style.background = s.on || s.busy ? "#10b981" : "";
  sw.disabled = !!s.busy || !s.canToggle;
  $("#tunnelKnob").style.transform = s.on || s.busy ? "translateX(16px)" : "";
  $("#tunnelOn").hidden = !s.on;
  $("#tunnelLabel").textContent = s.busy ? "Connecting…" : s.on ? "Public" : "Share";
  $("#tunnelBtn").classList.toggle("text-emerald-400", !!s.on);
  $("#tunnelHint").textContent = s.busy ? "Opening the tunnel, a few seconds…"
    : s.error ? "Failed: " + s.error
    : s.on ? "Anyone with this link can open the room." + (s.generated ? " Password is required." : "")
    : !s.canToggle ? "Off. Only the machine running the app can turn it on."
    : "Off. Turn on to get a link anyone can open, no router setup.";
  if (s.on) {
    $("#tunnelUrl").value = s.url;
    $("#tunnelPwRow").hidden = !s.password;
    $("#tunnelPw").value = s.password || "";
  }
}

async function tunnelCall(body) {
  try {
    const r = await fetch("/api/tunnel", body ? { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) } : {});
    if (!r.ok) throw new Error(await r.text());
    tunnelState = await r.json();
  } catch (err) { tunnelState = { ...tunnelState, busy: false, error: String(err.message || err).trim() }; }
  renderTunnel();
}

async function copyText(text, what) {
  try { await navigator.clipboard.writeText(text); }
  catch { const i = document.createElement("input"); i.value = text; document.body.appendChild(i); i.select(); document.execCommand("copy"); i.remove(); }
  toast(what + " copied", "info");
}

$("#tunnelBtn").onclick = (e) => togglePop("#tunnelPop", e);
$("#tunnelSwitch").onclick = () => {
  if (tunnelState.on) return tunnelCall({ on: false });
  tunnelState = { ...tunnelState, busy: true, error: "" };
  renderTunnel();
  tunnelCall({ on: true });
};
$("#tunnelCopy").onclick = () => copyText(tunnelState.url, "Link");
$("#tunnelPwCopy").onclick = () => copyText(tunnelState.password, "Password");
$("#tunnelPop").onclick = (e) => e.stopPropagation();
tunnelCall();
