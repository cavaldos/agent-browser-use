// =====================================================================
// MEMBERS & CONTROL
// =====================================================================
const amCtrl = () => !!(state.ctrl && state.me && state.ctrl.id === state.me.id);
const canControl = () => amCtrl() || !state.ctrl; // nobody in control: acting takes control
const lockText = () => state.ctrl
  ? `${state.ctrl.name} is in control — click “Take over” below to drive`
  : "Click “Take control” below to drive";
function guard() {
  if (canControl()) return true;
  toast(lockText(), "lock");
  return false;
}

// Header: overlapping avatars (you first, then whoever drives) + a popover with everyone.
function renderMembers() {
  const me = state.me?.id, ctrl = state.ctrl?.id;
  const isCtrl = (u) => u.id === ctrl;
  const rank = (u) => (u.id === me ? 0 : isCtrl(u) ? 1 : 2);
  const list = Object.values(state.users).sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name));
  const MAX = narrow.matches ? 3 : 5;

  const stack = $("#members");
  stack.innerHTML = "";
  list.slice(0, MAX).forEach((u) => {
    const wrap = document.createElement("span");
    wrap.className = "member" + (isCtrl(u) ? " ctrl" : "");
    wrap.appendChild(avatar(u, "h-7 w-7 text-[10px]"));
    if (isCtrl(u)) {
      const badge = document.createElement("span");
      badge.className = "badge";
      badge.innerHTML = svg("mouse", "h-2 w-2");
      wrap.appendChild(badge);
    }
    stack.appendChild(wrap);
  });
  if (list.length > MAX) {
    const more = document.createElement("span");
    more.className = "member grid h-7 w-7 place-items-center rounded-full bg-zinc-800 text-[10px] font-semibold text-zinc-300";
    more.textContent = "+" + (list.length - MAX);
    stack.appendChild(more);
  }
  $("#memberCount").textContent = list.length;
  $("#membersBtn").title = `${list.length} ${list.length === 1 ? "person" : "people"} in the room`;

  const ul = $("#memberList");
  ul.innerHTML = "";
  list.forEach((u) => {
    const li = document.createElement("li");
    li.className = "flex items-center gap-2.5 rounded-lg px-2 py-1.5";
    li.appendChild(avatar(u, "h-7 w-7 text-[10px]"));
    const name = document.createElement("span");
    name.className = "min-w-0 flex-1 truncate text-[13px] text-zinc-200";
    setName(name, u.name);
    if (u.id === me) name.insertAdjacentHTML("beforeend", ' <span class="text-zinc-500">(you)</span>');
    li.appendChild(name);
    if (isCtrl(u)) li.insertAdjacentHTML("beforeend", `<span class="flex items-center gap-1 rounded-full bg-emerald-500/15 px-2 py-0.5 text-[11px] font-medium text-emerald-300">${svg("mouse", "h-3 w-3")}In control</span>`);
    if (u.agent) li.appendChild(kickButton(u));
    ul.appendChild(li);
  });
}

// Disconnect a coding agent: it leaves the room, gives back control and its tabs, and can't use the
// browser again until it is reconnected. Two clicks (the first one asks), so it can't happen by accident.
function kickButton(u) {
  const b = document.createElement("button");
  b.type = "button";
  b.className = "agent-kick";
  b.title = `Disconnect ${u.name} from the browser`;
  b.innerHTML = svg("x", "h-3.5 w-3.5");
  let armed;
  b.onclick = (e) => {
    e.stopPropagation();
    if (!b.classList.contains("armed")) {
      b.classList.add("armed");
      b.innerHTML = svg("x", "h-3 w-3") + "<span>Disconnect?</span>";
      clearTimeout(armed);
      armed = setTimeout(() => { b.classList.remove("armed"); b.innerHTML = svg("x", "h-3.5 w-3.5"); }, 3000);
      return;
    }
    clearTimeout(armed);
    send({ type: "agent_kick", id: u.name });
  };
  return b;
}

function renderCtrl() {
  const btn = $("#ctrlBtn"), icon = $(".ctrl-icon", btn);
  const mine = amCtrl(), other = state.ctrl && !mine;
  btn.className = "ctrl-btn " + (mine ? "mine" : other ? "other" : "free");
  if (other) {
    delete icon.dataset.icon; // so setIcon redraws the mouse later
    icon.textContent = state.ctrl.agent ? "🤖" : initials(state.ctrl.name);
    icon.style.background = state.ctrl.color;
  } else {
    icon.style.background = "";
    setIcon(icon, "mouse", "h-3.5 w-3.5");
  }
  $(".ctrl-status", btn).textContent = mine ? "You’re in control" : other ? `${state.ctrl.name} is in control` : "";
  $(".ctrl-action", btn).textContent = mine ? "Release" : other ? "Take over" : "Take control";
  btn.title = mine ? "Release control so someone else can drive" : other ? `${state.ctrl.name} is in control — click to take over` : "Take control of the shared browser";
  $("#stage").classList.toggle("locked", !canControl());
  $("#lockHint").textContent = mine ? "" : lockText();
  renderMembers();
}
$("#ctrlBtn").onclick = () => send({ type: amCtrl() ? "control_release" : "control_take" });

// The agent takes control by itself when it acts (and gives it back when idle), unless blocked here.
function renderBlock() {
  const btn = $("#blockBtn"), on = !!state.blocked;
  btn.classList.toggle("on", on);
  setIcon($(".block-icon", btn), on ? "lock" : "unlock", "h-3.5 w-3.5");
  $(".block-label", btn).textContent = on ? "Unblock agent" : "Block agent";
  btn.title = on
    ? `${state.blocked.name} blocked the AI agent: it can't take control. Click to let it again`
    : "Stop the AI agent from taking control (it takes control by itself when it acts)";
}
$("#blockBtn").onclick = () => send({ type: "agent_block", on: !state.blocked });
renderBlock();
