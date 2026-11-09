// =====================================================================
// JOIN
// =====================================================================
$("#nameInput").value = prefs.name;
$("#loginForm").onsubmit = (e) => {
  e.preventDefault();
  prefs.name = $("#nameInput").value.trim();
  if (!prefs.name) return;
  save("name", prefs.name);
  ensureAudio(); // this click allows the page to play sound
  $("#login").hidden = true;
  $("#app").hidden = false;
  setSidebar(prefs.sidebar === "open" && !narrow.matches); // on phones the drawer starts closed
  renderSound(); renderPlay(); renderSettings(); renderCtrl(); layout();
  connect();
};
$("#leaveBtn").onclick = () => { localStorage.removeItem("name"); location.reload(); };
