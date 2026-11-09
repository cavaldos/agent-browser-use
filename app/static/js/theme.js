// Light / dark mode (the initial theme is set by an inline script in index.html)
$("#themeBtn").onclick = () => {
  const t = document.documentElement.dataset.theme === "light" ? "dark" : "light";
  document.documentElement.dataset.theme = t;
  save("theme", t);
};
