// =====================================================================
// SOUND & PAUSE
// Audio packet: [2][stream id][sample rate uint32 LE][int16 stereo PCM]. Each stream
// gets a small jitter buffer so its chunks play back to back.
// =====================================================================
let actx = null, gainNode = null;
const streams = {};
function ensureAudio() {
  if (!actx) {
    actx = new AudioContext();
    gainNode = actx.createGain();
    gainNode.gain.value = prefs.volume;
    gainNode.connect(actx.destination);
  }
  if (actx.state !== "running") actx.resume();
}
function playAudio(buf) {
  if (!prefs.sound || state.paused || !actx || actx.state !== "running") return;
  const dv = new DataView(buf);
  const id = dv.getUint8(1), rate = dv.getUint32(2, true);
  const pcm = new Int16Array(buf, 6, (buf.byteLength - 6) >> 1);
  const n = pcm.length >> 1;
  if (!n) return;
  const ab = actx.createBuffer(2, n, rate);
  const L = ab.getChannelData(0), R = ab.getChannelData(1);
  for (let i = 0; i < n; i++) { L[i] = pcm[2 * i] / 32768; R[i] = pcm[2 * i + 1] / 32768; }
  const s = (streams[id] ||= { t: 0 });
  const now = actx.currentTime;
  if (s.t < now + 0.02 || s.t > now + 1) s.t = now + 0.12; // (re)start with a small buffer
  const src = actx.createBufferSource();
  src.buffer = ab;
  src.connect(gainNode);
  src.start(s.t);
  s.t += ab.duration;
}
function renderSound() {
  setIcon($("#soundBtn"), prefs.sound && prefs.volume > 0 ? "volume" : "volumeX", "h-[18px] w-[18px]");
  $("#soundBtn").title = prefs.sound ? "Mute" : "Unmute";
  $("#soundBtn").classList.toggle("text-rose-400", !prefs.sound);
  $("#volume").value = prefs.volume;
  $("#volume").style.background = `linear-gradient(90deg, #818cf8 ${prefs.volume * 100}%, rgb(255 255 255 / .12) ${prefs.volume * 100}%)`;
}
$("#soundBtn").onclick = () => {
  prefs.sound = !prefs.sound;
  save("sound", prefs.sound ? "on" : "off");
  ensureAudio();
  send({ type: "audio", on: prefs.sound });
  renderSound();
};
$("#volume").oninput = (e) => {
  prefs.volume = +e.target.value;
  save("volume", prefs.volume);
  if (gainNode) gainNode.gain.setTargetAtTime(prefs.volume, actx.currentTime, 0.02);
  if (!prefs.sound && prefs.volume > 0) { prefs.sound = true; save("sound", "on"); send({ type: "audio", on: true }); }
  renderSound();
};

function setPaused(p) {
  state.paused = p;
  send({ type: "pause", on: p });
  if (!p) ensureAudio();
  renderPlay();
}
function renderPlay() {
  setIcon($("#playBtn"), state.paused ? "play" : "pause", "h-4 w-4" + (state.paused ? " ml-0.5" : ""));
  $("#playBtn").title = state.paused ? "Resume stream" : "Pause stream";
  $("#paused").hidden = !state.paused;
}
$("#playBtn").onclick = () => setPaused(!state.paused);
$("#resumeBtn").onclick = () => setPaused(false);
