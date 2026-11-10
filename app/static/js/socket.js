// =====================================================================
// WEBSOCKET (reconnects automatically)
// =====================================================================
let ws;
function connect() {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  ws = new WebSocket(`${proto}://${location.host}/ws?name=${encodeURIComponent(prefs.name)}`);
  ws.binaryType = "arraybuffer";
  ws.onopen = () => setStatus("Live", true);
  ws.onclose = () => {
    setStatus("Reconnecting…", false);
    state.users = {}; renderMembers(); $("#cursors").innerHTML = "";
    setTimeout(connect, 2000);
  };
  ws.onmessage = (e) => {
    if (typeof e.data === "string") return onMessage(JSON.parse(e.data));
    const kind = new Uint8Array(e.data, 0, 1)[0];
    if (kind === 1) drawFrame(e.data);
    else if (kind === 2) playAudio(e.data);
  };
}
const send = (m) => ws && ws.readyState === 1 && ws.send(JSON.stringify(m));
function setStatus(text, ok) {
  const s = $("#status");
  s.classList.toggle("ok", ok);
  $(".dot", s).className = "dot relative inline-flex h-2 w-2 rounded-full " + (ok ? "bg-emerald-400" : "bg-amber-400");
  $(".txt", s).textContent = text;
  s.title = ok ? "Connected" : text;
}

// Every connected coding agent is in the room as its own @handle (e.g. opencode#3f9a).
const agentUserOf = (h) => ({ id: "agent:" + h, name: h, color: "#a78bfa", agent: true });
function setAgents(handles, announce) {
  const now = new Set(handles.map((h) => "agent:" + h));
  for (const [id, u] of Object.entries(state.users)) {
    if (!u.agent || now.has(id)) continue;
    delete state.users[id];
    removeAgentCursors(u.name);
    if (announce) toast(`${u.name} left`, "userOff");
  }
  for (const h of handles) {
    const id = "agent:" + h;
    if (state.users[id]) continue;
    state.users[id] = agentUserOf(h);
    if (announce) toast(`🤖 ${h} joined`, "user");
  }
}

function onMessage(m) {
  switch (m.type) {
    case "init":
      state.me = m.you;
      state.users = {}; (m.users || []).forEach((u) => (state.users[u.id] = u));
      setAgents(m.agents || [], false);
      state.ctrl = m.ctrl || null;
      state.blocked = m.blocked || null;
      state.enabled = !!m.enabled;
      if (m.view) state.view = m.view;
      setTabs(m.tabs || [], m.active || "");
      if (!state.enabled) $("#loading span").textContent = "The shared browser is disabled on this server";
      $("#messages").innerHTML = ""; lastGroup = null;
      (m.chat || []).forEach((c) => addChat(c, false));
      renderChatEmpty();
      scrollChat(true);
      send({ type: "audio", on: prefs.sound });
      if (state.paused) send({ type: "pause", on: true });
      onRec(m.rec, true);
      renderMembers(); renderCtrl(); renderBlock(); renderSettings(); layout();
      break;
    case "agents":
      setAgents(m.agents || [], true); renderMembers();
      break;
    case "user_join":
      state.users[m.user.id] = m.user; renderMembers();
      toast(`${m.user.name} joined`, "user");
      break;
    case "user_leave": {
      const u = state.users[m.id];
      delete state.users[m.id]; renderMembers();
      document.getElementById("c-" + m.id)?.remove();
      if (u) toast(`${u.name} left`, "userOff");
      break;
    }
    case "control": {
      const prev = state.ctrl;
      state.ctrl = m.ctrl || null;
      renderCtrl();
      if (state.ctrl && state.ctrl.id !== prev?.id) {
        if (amCtrl()) toast("You’re in control", "mouse");
        else toast(`${state.ctrl.name} is in control`, "mouse");
      }
      break;
    }
    case "block":
      state.blocked = m.blocked || null;
      renderBlock();
      toast(state.blocked ? `${state.blocked.name} blocked the agent` : "The agent can take control again", state.blocked ? "lock" : "unlock");
      break;
    case "tabs": setTabs(m.tabs || [], m.active || ""); break;
    case "view": state.view = m.view; renderSettings(); layout(); break;
    case "pick": onPick(m); break;
    case "rec": onRec(m.rec); break;
    case "rec_done": onRecDone(m.rec); break;
    case "flows": renderFlows(); break;
    case "flow_run": onFlowRun(m.rec); break;
    case "chat": addChat(m.msg, true); break;
    case "emote": spawnEmote(m.text, m.user); break;
    case "cursor": moveCursor(m); break;
    case "agent": agentEvent(m); break;
    case "clipboard": copyToClipboard(m.text); break;
  }
}
