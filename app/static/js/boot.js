// Opened as /?join (the agent opens the viewer this way): join right away.
if (new URLSearchParams(location.search).has("join")) {
  history.replaceState(null, "", location.pathname);
  $("#nameInput").value = prefs.name || "Viewer";
  $("#loginForm").requestSubmit();
}
