// Records the WebSocket hello sequence of /ws: connects as two people, prints what the first one receives.
// usage: node ws-hello.mjs <port>   -> JSON on stdout (binary packets: only their kinds are listed)
const port = process.argv[2];
const open = (name) =>
  new Promise((resolve, reject) => {
    const ws = new WebSocket(`ws://127.0.0.1:${port}/ws?name=${encodeURIComponent(name)}`);
    ws.binaryType = "arraybuffer";
    const got = [];
    ws.onmessage = (e) => {
      if (typeof e.data === "string") got.push(JSON.parse(e.data));
      else {
        const u = new Uint8Array(e.data); // frames arrive at a varying rate: only the kinds are recorded
        (got.kinds ??= new Set()).add(u[0]);
      }
    };
    ws.onopen = () => resolve({ ws, got });
    ws.onerror = () => reject(new Error("ws error"));
  });
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
try {
  const a = await open("Alice");
  await wait(800);
  const b = await open("Bob");
  await wait(500);
  a.ws.send(JSON.stringify({ type: "chat", text: "hello" }));
  a.ws.send(JSON.stringify({ type: "emote", text: "👍" }));
  a.ws.send(JSON.stringify({ type: "control_take" }));
  await wait(500);
  b.ws.close();
  await wait(500);
  console.log(JSON.stringify({ alice: a.got, binary_packet_kinds: [...(a.got.kinds ?? [])].sort() }, null, 1));
  a.ws.close();
} catch (e) {
  console.log(JSON.stringify({ error: String(e.message) }));
}
process.exit(0);
