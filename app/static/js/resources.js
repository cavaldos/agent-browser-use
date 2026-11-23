// Performance limits: how much CPU / RAM of the machine the shared browser may use.
// 100% = no limit. CPU is enforced by pausing the browser now and then, RAM by asking it to free memory.
let resState = { available: false };
let resBusy = false; // a slider is being dragged or a save is in flight: don't overwrite it

function resFmtMem(mb) { return mb >= 1024 ? (mb / 1024).toFixed(1) + " GB" : mb + " MB"; }

function renderResources() {
  const s = resState;
  $("#resMenu").hidden = !s.available;
  if (!s.available) return;
  const canEdit = !!s.canToggle;
  $("#resCpu").disabled = !canEdit;
  $("#resMem").disabled = !canEdit;
  if (!resBusy) {
    $("#resCpu").value = s.cpuLimit;
    $("#resMem").value = s.memLimit;
  }
  resLabels();
  // The button shows live use; "/15" is the CPU cap when one is set, the dot turns red when RAM is over.
  $("#resText").textContent = "CPU " + s.cpuPct.toFixed(0) + "%" + (s.cpuLimit < 100 ? "/" + s.cpuLimit : "");
  $("#resText").className = "tabular-nums " + (s.memOver ? "text-red-400" : s.throttled ? "text-amber-400" : "");
  $("#resCpuUse").textContent = "using " + s.cpuPct.toFixed(0) + "%";
  $("#resMemUse").textContent = "using " + resFmtMem(s.memMB) + " (" + s.memPct.toFixed(0) + "%)";
  const cpuCap = s.cpuLimit < 100 ? s.cpuLimit : 100, memCap = s.memLimit < 100 ? s.memLimit : 100;
  $("#resCpuBar").style.width = Math.min(100, (s.cpuPct / cpuCap) * 100) + "%";
  $("#resMemBar").style.width = Math.min(100, (s.memPct / memCap) * 100) + "%";
  $("#resMemBar").className = "h-full rounded-full transition-all " + (s.memOver ? "bg-red-500" : "bg-indigo-500");
  $("#resNote").textContent = s.memOver ? "RAM over limit" : s.throttled ? "slowed to hold the CPU limit" : "";
  $("#resNote").className = "ml-1 font-normal " + (s.memOver ? "text-red-400" : "text-zinc-500");
  $("#resHint").textContent = canEdit
    ? "100% means no limit. A CPU limit slows pages down instead of closing them. The RAM limit is soft: the browser is asked to free memory, nothing is closed."
    : "Limits can only be changed from the machine that runs the app.";
}

// The numbers next to the sliders follow the slider while it moves.
function resLabels() {
  const cpu = +$("#resCpu").value, mem = +$("#resMem").value, s = resState;
  $("#resCpuVal").textContent = cpu >= 100 ? "No limit" : "max " + cpu + "% (" + (cpu / 100 * s.cores).toFixed(1) + " of " + s.cores + " cores)";
  $("#resMemVal").textContent = mem >= 100 ? "No limit" : "max " + mem + "% (" + resFmtMem(Math.round(mem / 100 * s.totalMemMB)) + ")";
}

async function resCall(body) {
  try {
    const r = await fetch("/api/resources", body ? { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) } : {});
    if (!r.ok) throw new Error(await r.text());
    resState = await r.json();
  } catch (err) { toast("Performance: " + String(err.message || err).trim(), "error"); }
  renderResources();
}

for (const id of ["#resCpu", "#resMem"]) {
  $(id).oninput = () => { resBusy = true; resLabels(); };
  $(id).onchange = async () => {
    await resCall({ cpuLimit: +$("#resCpu").value, memLimit: +$("#resMem").value });
    resBusy = false;
    renderResources();
  };
}

resCall();
// Live numbers every 2 s while the menu is open, every 15 s otherwise (the server stops sampling when nobody asks).
setInterval(() => { if (!$("#resPop").hidden && !resBusy) resCall(); }, 2000);
setInterval(() => { if ($("#resPop").hidden) resCall(); }, 15000);
$("#resBtn").onclick = (e) => { togglePop("#resPop", e); if (!$("#resPop").hidden) resCall(); };
$("#resPop").onclick = (e) => e.stopPropagation();
