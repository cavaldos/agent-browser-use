// Keep awake: the server moves the mouse and scrolls the browser's pages now and then, so sites
// that stop on inactivity (Google Colab) keep running. Off by default.
let kaState = { available: false, on: false };

function renderKeepAlive() {
  const s = kaState;
  $("#kaChip").hidden = !s.available;
  const sw = $("#kaSwitch");
  sw.setAttribute("aria-checked", String(!!s.on));
  sw.style.background = s.on ? "#10b981" : "";
  sw.disabled = !s.canToggle;
  $("#kaKnob").style.transform = s.on ? "translateX(16px)" : "";
  $("#kaIco").classList.toggle("text-amber-400", !!s.on);
  $("#kaIco").classList.toggle("text-zinc-600", !s.on);
  $("#kaIco").title = "Keep awake: " + (s.on ? "on" : "off");
  $("#kaChip").title = s.on ? "Moving the mouse and scrolling now and then so pages (e.g. Colab) don't go idle"
    : "Switch on to keep pages awake by simulating mouse movement and scrolling";
}

async function kaCall(body) {
  try {
    const r = await fetch("/api/keepalive", body ? { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) } : {});
    if (!r.ok) throw new Error(await r.text());
    kaState = await r.json();
  } catch (err) { toast("Keep awake: " + String(err.message || err).trim(), "error"); }
  renderKeepAlive();
}

$("#kaSwitch").onclick = () => {
  kaState = { ...kaState, on: !kaState.on };
  renderKeepAlive();
  kaCall({ on: kaState.on });
};
kaCall();
setInterval(kaCall, 15000);
