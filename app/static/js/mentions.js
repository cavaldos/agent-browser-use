// =====================================================================
// CHAT MENTIONS: type @ to mention a friend who is in the room.
// The person mentioned gets a toast and a notification sound.
// =====================================================================
const handleOf = (name) => name.replace(/\s+/g, "");

function mentionTargets() {
  const list = [], agents = [];
  for (const u of Object.values(state.users)) {
    if (u.id === state.me?.id) continue;
    if (u.agent) agents.push({ handle: u.name, label: u.name, hint: "agent", user: u });
    else list.push({ handle: handleOf(u.name), label: u.name, hint: "", user: u });
  }
  agents.sort((a, b) => a.handle.localeCompare(b.handle));
  if (agents.length > 1) agents.unshift({ handle: "agent", label: "agent", hint: "every agent", user: { name: "agent", color: "#a78bfa", agent: true } });
  return [...agents, ...list];
}
function mentionsMe(text) {
  return !!state.me && text.toLowerCase().split(/\s+/).includes("@" + handleOf(prefs.name || state.me.name).toLowerCase());
}
// Text with @mentions in a highlight.
function renderMentions(el, text) {
  const me = handleOf(prefs.name || state.me?.name || "").toLowerCase();
  const names = new Set(Object.values(state.users).map((u) => "@" + handleOf(u.name).toLowerCase()));
  names.add("@agent"); names.add("@ai");
  for (const u of Object.values(state.users)) if (u.agent) names.add("@" + u.name.split("#")[0].toLowerCase()); // @opencode = every OpenCode
  el.textContent = "";
  for (const part of text.split(/(@\S+)/)) {
    if (!part.startsWith("@")) { el.append(part); continue; }
    const low = part.toLowerCase().replace(/[.,!?;:]+$/, "");
    const mine = low === "@" + me, known = names.has(low);
    if (!mine && !known) { el.append(part); continue; }
    const sp = document.createElement("span");
    sp.className = "mention" + (mine ? " me" : "");
    setName(sp, part);
    el.append(sp);
  }
}

const mentionBox = $("#mentionList"), chatInput = $("#chatInput");
let mentionSel = 0, mentionItems = [];
function mentionQuery() {
  const v = chatInput.value, i = chatInput.selectionStart ?? v.length;
  const m = v.slice(0, i).match(/(^|\s)@(\S*)$/);
  return m ? { q: m[2].toLowerCase(), start: i - m[2].length - 1, end: i } : null;
}
function closeMentions() { mentionBox.hidden = true; mentionItems = []; }
function renderMentionList() {
  const m = mentionQuery();
  if (!m) return closeMentions();
  mentionItems = mentionTargets().filter((t) => t.handle.toLowerCase().startsWith(m.q) || t.label.toLowerCase().startsWith(m.q));
  if (!mentionItems.length) return closeMentions();
  mentionSel = Math.min(mentionSel, mentionItems.length - 1);
  mentionBox.replaceChildren(...mentionItems.map((t, i) => {
    const b = document.createElement("button");
    b.type = "button";
    b.className = i === mentionSel ? "sel" : "";
    b.append(avatar(t.user, "h-6 w-6 text-[10px]"));
    const n = document.createElement("span");
    setName(n, t.label);
    const h = document.createElement("span");
    h.className = "ml-auto text-xs text-zinc-500";
    h.textContent = t.hint;
    b.append(n, h);
    b.onmousedown = (e) => { e.preventDefault(); pickMention(i); };
    return b;
  }));
  mentionBox.hidden = false;
}
function pickMention(i) {
  const m = mentionQuery(), t = mentionItems[i];
  if (!m || !t) return;
  const v = chatInput.value, ins = "@" + t.handle + " ";
  chatInput.value = v.slice(0, m.start) + ins + v.slice(m.end);
  const pos = m.start + ins.length;
  chatInput.setSelectionRange(pos, pos);
  closeMentions();
  chatInput.focus();
}
chatInput.addEventListener("input", () => { mentionSel = 0; renderMentionList(); });
chatInput.addEventListener("click", renderMentionList);
chatInput.addEventListener("blur", () => setTimeout(closeMentions, 100));
chatInput.addEventListener("keydown", (e) => {
  if (mentionBox.hidden || !mentionItems.length) return;
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    mentionSel = (mentionSel + (e.key === "ArrowDown" ? 1 : -1) + mentionItems.length) % mentionItems.length;
    renderMentionList();
  } else if (e.key === "Enter" || e.key === "Tab") { e.preventDefault(); pickMention(mentionSel); }
  else if (e.key === "Escape") { e.preventDefault(); closeMentions(); }
});
