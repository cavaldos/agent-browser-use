// Light / dark mode (the initial theme is set by an inline script in index.html)
$("#themeBtn").onclick = () => {
  const t = document.documentElement.dataset.theme === "light" ? "dark" : "light";
  document.documentElement.dataset.theme = t;
  save("theme", t);
  // Share the viewer's mode with the shared browser when we can.
  if (typeof send === "function" && state && state.enabled) send({ type: "color_scheme", colorScheme: t });
};
