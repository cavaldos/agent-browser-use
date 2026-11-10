// Builds the page: injects components/*.html, then loads js/*.js in order (they share one global scope).
const COMPONENTS_READY = (async () => {
  const slots = [...document.querySelectorAll("[data-include]")];
  const html = await Promise.all(slots.map((s) => fetch(s.dataset.include).then((r) => r.text())));
  slots.forEach((s, i) => s.replaceWith(document.createRange().createContextualFragment(html[i])));
})();

const SCRIPTS = ["core", "icons", "helpers", "theme", "join", "socket", "members", "tabs", "toolbar", "screen", "picker", "flows", "sound", "emotes", "sidebar", "tunnel", "mentions", "cursors", "agent", "boot"];

COMPONENTS_READY.then(async () => {
  for (const name of SCRIPTS) {
    await new Promise((ok, fail) => {
      const el = document.createElement("script");
      el.src = `js/${name}.js`;
      el.onload = ok;
      el.onerror = () => fail(new Error("failed to load " + el.src));
      document.body.appendChild(el);
    });
  }
  document.getElementById("nameInput")?.focus();
});
