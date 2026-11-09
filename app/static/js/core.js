const $ = (s, root = document) => root.querySelector(s);
const $$ = (s, root = document) => [...root.querySelectorAll(s)];

// Room state (from the server) and preferences of this device.
const state = { me: null, users: {}, ctrl: null, blocked: null, tabs: [], active: "", view: { width: 1280, height: 720, quality: 70 }, enabled: false, paused: false };
const prefs = {
  name: localStorage.getItem("name") || "",
  sound: localStorage.getItem("sound") !== "off",
  volume: +(localStorage.getItem("volume") ?? 0.8),
  scroll: +(localStorage.getItem("scroll") ?? 1),
  cursors: localStorage.getItem("cursors") !== "off",
  emotes: localStorage.getItem("emotes") !== "off",
  sidebar: localStorage.getItem("sidebar") ?? (innerWidth >= 1100 ? "open" : "closed"),
};
const save = (k, v) => { try { localStorage.setItem(k, v); } catch {} };
const narrow = matchMedia("(max-width: 899px)"); // the sidebar becomes a drawer (keep in sync with style.css)
const EMOTES = ["😍", "😴", "🤣", "🎉", "😱", "😄", "👍", "❤️"];
