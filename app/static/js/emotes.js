// =====================================================================
// EMOJI REACTIONS
// =====================================================================
for (const box of [$("#emoteList"), $("#emotePop")]) {
  EMOTES.forEach((em) => {
    const b = document.createElement("button");
    b.className = "emote-btn";
    b.textContent = em;
    b.onclick = () => sendEmote(em);
    box.appendChild(b);
  });
}
$("#emoteToggle").onclick = (e) => togglePop("#emotePop", e);
let lastEmote = 0;
function sendEmote(em) {
  if (Date.now() - lastEmote < 300) return;
  lastEmote = Date.now();
  send({ type: "emote", text: em });
}
function spawnEmote(em, user) {
  if (!prefs.emotes) return;
  const layer = $("#emoteLayer");
  const el = document.createElement("div");
  el.className = "emote";
  el.style.left = 10 + Math.random() * 80 + "%";
  el.style.setProperty("--sway", (Math.random() * 80 - 40).toFixed(0) + "px");
  el.style.setProperty("--rise", -(layer.clientHeight * (0.45 + Math.random() * 0.25)).toFixed(0) + "px");
  el.innerHTML = '<span class="e"></span><span class="n"></span>';
  $(".e", el).textContent = em;
  if (user) { $(".n", el).textContent = user.name; $(".n", el).style.background = user.color; }
  el.addEventListener("animationend", () => el.remove());
  layer.appendChild(el);
  while (layer.children.length > 40) layer.firstChild.remove();
}
