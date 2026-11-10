// =====================================================================
// FLOWS: record what you do in the shared browser as a flow with an id (e.g. flow-3fa9c1),
// and manage the recorded flows in the sidebar's Flows tab (view steps, rename, edit, replay…).
// A coding agent reads a flow with browser_get_flow and replays it with browser_run_flow.
// =====================================================================
let rec = { on: false }, recTimer = 0;
let flows = [], runningFlow = "", openFlow = null, flowLog = null; // openFlow: id shown in the detail view
const recBtn = $("#recBtn"), recBar = $("#recBar"), flowPop = $("#flowPop");

const clock = (ms) => { const s = Math.max(0, Math.floor(ms / 1000)); return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`; };
const steps = (n) => `${n} step${n === 1 ? "" : "s"}`;
// Only the id is copied: agents recognize it and look the flow up themselves (browser_get_flow).
const copyId = (id) => { copyToClipboard(id); toast(`Copied ${id}: paste it to your agent`, "copy"); };
const hostOf = (u) => { try { return new URL(u).host; } catch { return ""; } };
function ago(t) {
  const s = (Date.now() - new Date(t)) / 1000;
  if (s < 60) return "just now";
  if (s < 3600) return Math.floor(s / 60) + " min ago";
  if (s < 86400) return Math.floor(s / 3600) + " h ago";
  return new Date(t).toLocaleDateString([], { month: "short", day: "numeric" });
}
async function api(path, opts) {
  const r = await fetch(path, opts);
  if (!r.ok) throw new Error((await r.text()).trim() || r.statusText);
  return r.status === 204 ? null : r.json();
}
const fail = (what) => (e) => toast(`${what}: ${String(e.message || e).trim()}`, "info");

// ---------- Recording ----------

function onRec(info, quiet) {
  const was = rec.on;
  rec = info || { on: false };
  if (rec.on && !was && !quiet) {
    const mine = rec.by?.id === state.me?.id;
    toast(mine ? "Recording: do the task in the page, then press Stop" : `${rec.by?.name} is recording a flow`, "record");
  }
  renderRec();
}

function renderRec() {
  const on = !!rec.on;
  recBtn.classList.toggle("on", on);
  recBtn.title = on ? "Stop recording and save the flow" : "Record a flow: what you do becomes steps an agent can replay";
  recBar.hidden = !on;
  $("#flowsTabDot").hidden = !on;
  $("#flowRecBtn").classList.toggle("on", on);
  clearInterval(recTimer);
  if (!on) {
    $(".rec-txt", recBtn).textContent = "";
    $("#flowRecBtn .txt").textContent = "Record a new flow";
    return;
  }
  const mine = rec.by?.id === state.me?.id;
  const tick = () => {
    const t = clock(Date.now() - rec.since);
    $(".rec-txt", recBtn).textContent = t;
    $(".rec-meta", recBar).textContent = `${t} · ${steps(rec.count)}`;
    $("#flowRecBtn .txt").textContent = `Stop & save · ${t} · ${steps(rec.count)}`;
  };
  tick();
  recTimer = setInterval(tick, 1000);
  $(".rec-who", recBar).textContent = mine ? "Recording" : `${rec.by?.name || "Someone"} is recording`;
  let hint = rec.last || "Click, type and choose in the page: each action becomes a step";
  if (!rec.last && state.ctrl?.agent) hint = "The agent is in control: its actions are not recorded";
  $(".rec-last", recBar).textContent = hint;
}

function toggleRecording() {
  if (!state.enabled) return;
  if (rec.on) return send({ type: "rec_stop" });
  hideFlowPop();
  send({ type: "rec_start" });
}
recBtn.onclick = toggleRecording;
$("#flowRecBtn").onclick = toggleRecording;
recBar.onclick = (e) => {
  const a = e.target.closest("button")?.dataset.a;
  if (a === "stop") send({ type: "rec_stop" });
  else if (a === "discard" && confirm("Discard this recording? Nothing will be saved.")) send({ type: "rec_discard" });
};

function onRecDone(f) {
  if (!f?.id) return toast("Nothing was recorded, so no flow was saved", "info");
  showFlowPop(f);
  renderFlows();
}

function hideFlowPop() { flowPop.hidden = true; }
function showFlowPop(f) {
  flowPop.innerHTML = `
    <div class="mb-2 flex items-center gap-2">
      <span class="text-[13px] font-semibold">Flow saved</span>
      <span class="text-xs text-zinc-500">${steps(f.count)}</span>
      <span class="flex-1"></span>
      <button data-a="close" class="icon-btn h-6 w-6" title="Close">${svg("x", "h-3.5 w-3.5")}</button>
    </div>
    <div class="mb-2 flex items-center gap-2">
      <span class="flow-id rounded-lg bg-blue-500/20 px-2.5 py-1.5 font-mono text-[15px] font-semibold text-blue-300"></span>
      <input class="flow-name h-8 min-w-0 flex-1 rounded-lg border border-white/[.08] bg-zinc-900/80 px-2.5 text-[13px] outline-none focus:border-indigo-400/50" maxlength="80" title="Name of the flow">
    </div>
    <p class="mb-3 text-xs text-zinc-500">Paste the id to an agent, with what you want (or alone to replay it): it finds the flow by itself.</p>
    <div class="flex gap-2">
      <button data-a="id" class="btn-primary h-8 flex-1 whitespace-nowrap px-2 text-[12px]">${svg("copy", "h-3.5 w-3.5 shrink-0")} Copy id</button>
      <button data-a="open" class="btn-ghost h-8 whitespace-nowrap px-2.5 text-[12px]" title="See and edit its steps in the Flows tab">Steps</button>
    </div>`;
  $(".flow-id", flowPop).textContent = f.id;
  const name = $(".flow-name", flowPop);
  name.value = f.name || "";
  name.onchange = () => renameFlow(f.id, name.value);
  name.onkeydown = (e) => { e.stopPropagation(); if (e.key === "Enter") name.blur(); };
  flowPop.onclick = (e) => {
    const a = e.target.closest("button")?.dataset.a;
    if (a === "id") copyId(f.id);
    else if (a === "open") showFlow(f.id);
    else if (a !== "close") return;
    hideFlowPop();
  };
  flowPop.hidden = false;
}

// ---------- Flows tab: the list ----------

async function renderFlows() {
  try {
    const d = await api("/api/flows");
    flows = d.flows || [];
    runningFlow = d.running || "";
  } catch { return; } // server may not be up
  renderFlowList();
  if (openFlow) {
    if (flows.some((f) => f.id === openFlow)) renderFlowDetail();
    else closeFlow();
  }
}

function renderFlowList() {
  const box = $("#flowList");
  const q = $("#flowSearch").value.trim().toLowerCase();
  const shown = flows.filter((f) => !q || [f.name, f.id, f.startUrl].some((x) => (x || "").toLowerCase().includes(q)));
  box.replaceChildren();
  if (!shown.length) {
    box.innerHTML = `<div class="flex flex-col items-center gap-1 px-6 py-10 text-center">
      <div class="mb-2 grid h-12 w-12 place-items-center rounded-2xl bg-white/[.04] text-red-400 ring-1 ring-inset ring-white/[.06]">${svg("record", "h-5 w-5")}</div>
      <p class="text-sm font-medium text-zinc-300">${q ? "No flow matches" : "No flows yet"}</p>
      <p class="text-xs text-zinc-500">${q ? "Try another word." : "Press “Record a new flow”, do a task in the browser, then stop. You get an id to give to an agent."}</p></div>`;
    return;
  }
  for (const f of shown) {
    const card = document.createElement("div");
    card.className = "flow-card group cursor-pointer rounded-xl border border-white/[.06] bg-white/[.02] p-2.5 transition hover:border-white/[.14]";
    card.innerHTML = `
      <div class="flex items-start gap-2">
        <div class="min-w-0 flex-1">
          <p class="flex items-center gap-1.5 text-[13px] font-medium"><span class="truncate"></span></p>
          <p class="mt-0.5 truncate text-xs text-zinc-500"><span class="font-mono text-zinc-400"></span> · ${steps(f.count)} · ${ago(f.created)}</p>
          <p class="host truncate text-xs text-zinc-500"></p>
        </div>
        <button data-a="copy" class="icon-btn h-7 w-7" title="Copy the id for an agent">${svg("copy", "h-3.5 w-3.5")}</button>
        <button data-a="run" class="icon-btn h-7 w-7" title="Replay now">${svg("play", "h-3 w-3")}</button>
      </div>`;
    const title = $("p span", card);
    title.textContent = f.name || f.id;
    if (f.secrets) title.insertAdjacentHTML("afterend", `<span class="shrink-0 text-zinc-500" title="Has a password step: it must be typed in before a replay">${svg("lock", "h-3 w-3")}</span>`);
    if (f.id === runningFlow) title.insertAdjacentHTML("afterend", `<span class="flow-badge running">running</span>`);
    $(".font-mono", card).textContent = f.id;
    $(".host", card).textContent = hostOf(f.startUrl) || "";
    $(".host", card).hidden = !f.startUrl;
    card.onclick = (e) => {
      const a = e.target.closest("button")?.dataset.a;
      if (a === "copy") return copyId(f.id);
      if (a === "run" && !f.secrets) return runFlow(f.id, 1, {});
      showFlow(f.id); // a flow with a password needs it typed in first: open it
    };
    box.appendChild(card);
  }
}
$("#flowSearch").oninput = renderFlowList;
$("#flowSearch").onkeydown = (e) => e.stopPropagation();
$("#flowImportBtn").onclick = () => $("#flowImportFile").click();
$("#flowImportFile").onchange = async (e) => {
  const file = e.target.files[0];
  e.target.value = "";
  if (!file) return;
  const fd = new FormData();
  fd.append("file", file);
  try {
    const d = await api("/api/flows/import", { method: "POST", body: fd });
    toast(`Imported as ${d.id}`, "upload");
    await renderFlows();
    showFlow(d.id);
  } catch (err) { fail("Import failed")(err); }
};

// ---------- Flows tab: one flow ----------

const KIND_ICON = { goto: "🌐", click: "🖱️", dblclick: "🖱️", rclick: "🖱️", fill: "⌨️", select: "📋", key: "⌨️", drag: "✋",
  back: "↩️", forward: "↪️", reload: "🔄", tab_new: "➕", tab_switch: "🗂️", tab_close: "✖️", page: "→" };

function showFlow(id) {
  if (openFlow !== id) flowLog = null;
  openFlow = id;
  if (!sidebarOpen()) setSidebar(true);
  showPane("flows");
  $("#flowListView").hidden = true;
  $("#flowDetail").hidden = false;
  renderFlowDetail();
}
function closeFlow() {
  openFlow = null;
  flowLog = null;
  $("#flowListView").hidden = false;
  $("#flowDetail").hidden = true;
}

let detailSeq = 0;
async function renderFlowDetail() {
  const id = openFlow, seq = ++detailSeq;
  let d;
  try { d = await api("/api/flows/get?id=" + encodeURIComponent(id)); } catch (e) { fail("Couldn’t open the flow")(e); return closeFlow(); }
  if (seq !== detailSeq || id !== openFlow) return; // another flow was opened meanwhile
  const box = $("#flowDetail");
  const secrets = box.querySelectorAll("input[data-secret]"); // keep passwords typed so far
  const typed = Object.fromEntries([...secrets].map((i) => [i.dataset.secret, i.value]));
  const repeat = $(".flow-repeat", box)?.value || 1;
  const f = d.flow, running = runningFlow === f.id;
  box.innerHTML = `
    <div class="flex shrink-0 items-center gap-1 border-b border-white/[.06] p-2">
      <button data-a="back" class="icon-btn h-8 w-8" title="All flows">${svg("chevronLeft", "h-4 w-4")}</button>
      <input class="flow-title h-8 min-w-0 flex-1 rounded-lg border border-transparent bg-transparent px-2 text-[14px] font-semibold outline-none hover:border-white/[.08] focus:border-indigo-400/50" maxlength="80" title="Rename">
      <button data-a="delete" class="icon-btn h-8 w-8 hover:text-red-400" title="Delete this flow">${svg("trash", "h-4 w-4")}</button>
    </div>
    <div class="min-h-0 flex-1 overflow-y-auto">
      <div class="space-y-3 p-3">
        <div class="flex items-center gap-2">
          <button data-a="id" class="flow-idbtn rounded-lg bg-blue-500/20 px-2.5 py-1 font-mono text-[13px] font-semibold text-blue-300" title="Copy the id"></button>
          <span class="meta min-w-0 flex-1 truncate text-xs text-zinc-500"></span>
        </div>
        <a class="start block truncate text-xs text-indigo-300 hover:underline" target="_blank" rel="noopener"></a>
        <div class="flex gap-2">
          <button data-a="id" class="btn-primary h-8 flex-1 whitespace-nowrap px-2 text-[12px]">${svg("copy", "h-3.5 w-3.5 shrink-0")} Copy id</button>
          <button data-a="export" class="btn-ghost h-8 whitespace-nowrap px-2.5 text-[12px]" title="Download the flow as a file">${svg("download", "h-3.5 w-3.5 shrink-0")} Export</button>
        </div>
        <div class="rounded-xl border border-white/[.06] bg-white/[.02] p-2.5">
          <div class="flex items-center gap-2">
            <span class="flex-1 text-[13px] font-medium" title="The agent does every step again while you watch">Replay</span>
            <label class="flex items-center gap-1 text-xs text-zinc-400">×<input class="flow-repeat h-7 w-12 rounded-md border border-white/[.08] bg-zinc-900/80 px-1.5 text-center text-[12px] outline-none" type="number" min="1" max="50" title="How many times"></label>
            <button data-a="run" class="flow-run btn-primary h-7 px-3 text-[12px]"></button>
          </div>
          <pre class="flow-log mt-2 max-h-48 overflow-auto whitespace-pre-wrap rounded-lg bg-black/30 p-2 font-mono text-[11px] leading-relaxed text-zinc-300" hidden></pre>
        </div>
        <p class="label !mb-0 pt-1">Steps <span class="normal-case tracking-normal text-zinc-600">· click ✎ to change what is typed</span></p>
        <ol class="flow-steps space-y-1"></ol>
      </div>
    </div>`;
  const title = $(".flow-title", box);
  title.value = f.name || f.id;
  title.onchange = () => renameFlow(f.id, title.value);
  title.onkeydown = (e) => { e.stopPropagation(); if (e.key === "Enter") title.blur(); };
  $(".flow-idbtn", box).textContent = f.id;
  $(".meta", box).textContent = [steps(f.count), f.duration ? clock(f.duration) : "", d.by ? "by " + d.by : "", ago(f.created)].filter(Boolean).join(" · ");
  const start = $(".start", box);
  start.hidden = !f.startUrl;
  start.textContent = "Starts at " + (d.startTitle ? `${d.startTitle} — ` : "") + (f.startUrl || "");
  if (f.startUrl) start.href = f.startUrl;
  $(".flow-repeat", box).value = repeat;
  $(".flow-repeat", box).onkeydown = (e) => e.stopPropagation();
  const run = $(".flow-run", box);
  run.innerHTML = running ? "Stop" : `${svg("play", "h-3 w-3")} Run`;
  run.classList.toggle("stop", running);
  run.disabled = !!runningFlow && !running;
  run.title = run.disabled ? "Another flow is running" : "";
  if (flowLog) { $(".flow-log", box).hidden = false; $(".flow-log", box).textContent = flowLog; }

  const list = $(".flow-steps", box);
  d.steps.forEach((s, i) => {
    const n = i + 1, li = document.createElement("li");
    li.className = "flow-step group" + (s.kind === "page" ? " page" : "");
    li.innerHTML = `
      <span class="num">${s.kind === "page" ? "" : n}</span>
      <div class="min-w-0 flex-1">
        <p class="lbl"><span class="ic"></span> <span class="txt"></span></p>
        <p class="hints truncate font-mono text-[10.5px] text-zinc-500"></p>
      </div>
      <div class="acts">
        <button data-a="edit" class="icon-btn h-6 w-6" title="Change what is typed / chosen">${svg("edit", "h-3 w-3")}</button>
        <button data-a="del" class="icon-btn h-6 w-6 hover:text-red-400" title="Remove this step">${svg("x", "h-3 w-3")}</button>
      </div>`;
    $(".ic", li).textContent = KIND_ICON[s.kind] || "•";
    $(".txt", li).textContent = s.kind === "page" ? s.label.replace(/^→ /, "") : s.label;
    $(".hints", li).textContent = s.hints || "";
    $(".hints", li).title = s.hints || "";
    $(".hints", li).hidden = !s.hints;
    const editable = (s.kind === "fill" && !s.secret) || s.kind === "select";
    $('[data-a="edit"]', li).hidden = !editable;
    if (s.secret) { // the password is never stored: type it here for a replay from the viewer
      const pw = document.createElement("input");
      pw.type = "password";
      pw.dataset.secret = n;
      pw.value = typed[n] || "";
      pw.placeholder = "Password for the replay (not saved)";
      pw.className = "mt-1 h-7 w-full rounded-md border border-amber-400/30 bg-zinc-900/80 px-2 text-[12px] outline-none focus:border-amber-400/60";
      pw.onkeydown = (e) => e.stopPropagation();
      $(".min-w-0", li).appendChild(pw);
    }
    li.onclick = (e) => {
      const a = e.target.closest("button")?.dataset.a;
      if (a === "del") deleteStep(f.id, n, s.label);
      else if (a === "edit") editStep(li, f.id, n, s);
    };
    list.appendChild(li);
  });

  box.onclick = (e) => {
    const a = e.target.closest("button")?.dataset.a;
    if (a === "back") closeFlow();
    else if (a === "delete") deleteFlow(f);
    else if (a === "id") copyId(f.id);
    else if (a === "export") location.href = "/api/flows/export?id=" + encodeURIComponent(f.id);
    else if (a === "run") {
      if (runningFlow === f.id) return api("/api/flows/stop", { method: "POST" }).then(() => toast("Stopping after the current step", "info")).catch(fail("Couldn’t stop"));
      const inputs = {};
      for (const i of box.querySelectorAll("input[data-secret]")) {
        if (!i.value) { i.focus(); return toast("Type the password for step " + i.dataset.secret + " first", "lock"); }
        inputs[i.dataset.secret] = i.value;
      }
      runFlow(f.id, +$(".flow-repeat", box).value || 1, inputs);
    }
  };
}

// Inline editor for the text a step types (or the option it chooses).
function editStep(li, id, n, s) {
  if ($(".edit", li)) return;
  const form = document.createElement("form");
  form.className = "edit mt-1 flex gap-1";
  form.innerHTML = `<input class="h-7 min-w-0 flex-1 rounded-md border border-indigo-400/50 bg-zinc-900/80 px-2 text-[12px] outline-none">
    <button class="btn-primary h-7 px-2.5 text-[11px]">Save</button>`;
  const input = $("input", form);
  input.value = s.kind === "select" ? (s.values || []).join(", ") : s.value || "";
  input.onkeydown = (e) => { e.stopPropagation(); if (e.key === "Escape") form.remove(); };
  form.onsubmit = async (e) => {
    e.preventDefault();
    try {
      await api(`/api/flows/step?id=${encodeURIComponent(id)}&n=${n}&action=value`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ value: input.value }) });
      toast(`Step ${n} updated`, "edit");
      renderFlowDetail();
    } catch (err) { fail("Couldn’t save")(err); }
  };
  $(".min-w-0", li).appendChild(form);
  input.focus();
  input.select();
}

async function deleteStep(id, n, label) {
  if (!confirm(`Remove step ${n}?\n\n${label}`)) return;
  try {
    await api(`/api/flows/step?id=${encodeURIComponent(id)}&n=${n}&action=delete`, { method: "POST" });
    renderFlows();
  } catch (e) { fail("Couldn’t remove the step")(e); }
}

async function runFlow(id, repeat, inputs) {
  if (runningFlow) return toast("A flow is already running", "info");
  if (openFlow === id) { flowLog = "Running…"; renderFlowDetail(); }
  toast(`Replaying ${id}: the agent takes control`, "play");
  try {
    const d = await api("/api/flows/run", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ id, repeat, inputs }) });
    flowLog = d.log;
    toast(d.ok ? `${id} finished` : `${id} stopped: see the log`, d.ok ? "play" : "info");
    if (!d.ok && openFlow !== id) showFlow(id);
  } catch (e) {
    flowLog = String(e.message || e);
    fail("Couldn’t run")(e);
  }
  if (openFlow === id) renderFlowDetail();
}

function onFlowRun(r) {
  runningFlow = r?.on ? r.id : "";
  renderFlowList();
  if (openFlow) renderFlowDetail();
}

async function renameFlow(id, name) {
  name = name.trim();
  if (!name) return;
  try { await api(`/api/flows/rename?id=${encodeURIComponent(id)}&name=${encodeURIComponent(name)}`, { method: "POST" }); }
  catch (e) { fail("Couldn’t rename")(e); }
}

async function deleteFlow(f) {
  if (!confirm(`Delete flow “${f.name || f.id}”? Agents can no longer use ${f.id}.`)) return;
  try {
    await api(`/api/flows/delete?id=${encodeURIComponent(f.id)}`, { method: "POST" });
    toast(`Deleted ${f.id}`, "trash");
    if (openFlow === f.id) closeFlow();
  } catch (e) { fail("Couldn’t delete")(e); }
}
