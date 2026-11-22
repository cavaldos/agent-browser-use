# WebSocket protocol of `/ws`

Sources: `app/src/main.rs` (Msg struct, Hub.handle, serveWS), `app/src/agent.rs`, `app/src/chat.rs`, `app/src/flow.rs`,
`app/src/flow_http.rs`, `app/src/assets.rs`, `app/src/pick.rs`, `app/src/browser.rs` (binary packets) and the client in
`app/static/js/socket.js` plus the senders in the other `app/static/js/*.js` files.

## Connection

- `GET /ws?name=<display name>` upgraded to WebSocket (any Origin accepted: `CheckOrigin` returns true).
  Missing/empty `name` -> HTTP 400 `name required`. Names longer than 30 runes are truncated to 30 runes.
  When `-pw` is set the usual auth wrapper applies first (cookie, Bearer or Basic).
- The server picks `id = %x of rand.Int63()` and a colour from this list, at random:
  `#f87171 #fb923c #facc15 #4ade80 #2dd4bf #38bdf8 #818cf8 #c084fc #f472b6 #a3e635`.
- Text frames carry one JSON object each, always with a string `type`. Binary frames are screen/audio packets.
- Client -> server read limit 64 KiB, read deadline 60 s (reset by each pong); the server pings every 30 s,
  write deadline 10 s. Per-client queues: `send` 512 messages (JSON + audio, in order), `frame` 1 slot (newest
  frame wins). Best-effort messages (`cursor` broadcasts, audio) are dropped when the queue is half full;
  everything else is "important": if the queue is full the client is disconnected and must reconnect (gets a new `init`).
- The browser client reconnects after 2 s when the socket closes.
- JSON uses `omitempty` on every `Msg` field: zero values (`false`, `0`, `""`, empty slices, nil pointers)
  are **absent** from the wire. Exceptions: `Agents` is a pointer to a slice, so an empty list is sent as `[]`
  (`init` always has it); in `Rect`, `FlowInfo.Count` and `Asset.Size` the fields have no omitempty.

## Hello sequence (server -> client, right after the upgrade)

1. `init` (to this client only), then a binary frame packet if a screenshot is cached (browser enabled).
2. `user_join` is sent to **everyone else**.
3. The web page answers with `audio` (`{on: prefs.sound}`) and, if the stream is paused locally, `pause`.

## The `Msg` envelope

All JSON messages, in both directions, are the `Msg` struct (`app/src/main.rs`). Fields and their JSON names
(all optional on the wire):

| JSON | Type | Used by |
|---|---|---|
| `type` | string | always |
| `id` | string | user id / tab id / agent handle / asset id / cursor id |
| `text` | string | chat text, emote, typed text, clipboard text, agent caption, asset kind |
| `x`, `y` | number | normalised 0..1 position on the shared screen |
| `on` | bool | audio / pause / agent_block switch |
| `you` | User | `init` |
| `user` | User | `user_join`, `emote` |
| `users` | User[] | `init` |
| `chat` | ChatMsg[] | `init` (last 200 messages) |
| `agents` | string[] (pointer: `[]` is kept) | `init`, `agents` (handles of connected coding agents) |
| `msg` | ChatMsg | `chat` |
| `enabled` | bool | `init` (shared browser running) |
| `ctrl` | User | `init`, `control` (who is in control; absent = nobody) |
| `blocked` | User | `init`, `block` (absent = not blocked) |
| `tabs` | TabInfo[] | `init`, `tabs` |
| `active` | string | `init`, `tabs` (active tab id) |
| `view` | View | `init`, `view` |
| `index` | int | `tab_move` |
| `url` | string | `nav`, `tab_new` |
| `area` | string | `cursor` (`"b"` over the browser, `""` outside) |
| `event` | string | `mouse` (`down`/`up`/`move`/`wheel`), `key` (`down`/`up`), `pick` (`hover`/`click`) |
| `button`, `buttons`, `count` | int | `mouse` (DOM button, buttons bitmask, click count) |
| `mods` | int | `mouse`, `key`: bitmask Alt=1 Ctrl=2 Meta=4 Shift=8 (non-darwin servers map Meta -> Ctrl, `fixMods`) |
| `key`, `code`, `keyCode` | string, string, int | `key` |
| `dx`, `dy` | number | `mouse` wheel deltas (pixels) |
| `pick` | Pick | `pick` reply |
| `rec` | FlowInfo | `rec`, `rec_done`, `flow_run` |
| `act` | string | `agent`: `step` / `click` / `key` / `say` |
| `rect` | Rect | `agent` (normalised element outline) |
| `tabId`, `workerId` | string | `agent`, agent `cursor`: which tab / which agent worker |

Nested types:

- `User` `{id, name, color, agent?}`; `agent: true` only for the AI agent.
- `ChatMsg` `{user, color, text, time(ms), agent?, id?(order in room), toAgent?, to?[]}`.
- `TabInfo` `{id, title, url, favicon?, loading?, audible?, muted?, canBack?, canFwd?}`.
- `View` `{width, height, quality}` (all ints, no omitempty).
- `Rect` `{x, y, w, h}` normalised 0..1 (no omitempty).
- `Pick` `{code?, ref, tag, role?, name?, selector, text?, html?, rect{x,y,w,h}, url?, title?, view?}`.
- `FlowInfo` `{id?, name?, on?, by?(User), count, last?, since?(ms), created?, startUrl?, duration?(ms), secrets?}`.

## Server -> client (text)

| type | fields | when | audience |
|---|---|---|---|
| `init` | `you`, `users`, `chat`, `agents`, `enabled?`, `tabs?`, `active?`, `view?`, `ctrl?`, `blocked?`, `rec?` | on connect (`serveWS`). `enabled/tabs/active/view` only with a browser. `users` are the people only (coding agents are in `agents`). `rec` only while a recording runs. | that client |
| `user_join` | `user` | a person connected | everyone except the new client |
| `user_leave` | `id` | a person disconnected | everyone |
| `agents` | `agents` (sorted handles) | the set of coding-agent seats changed (`broadcastSeats`, chat.rs) | everyone |
| `control` | `ctrl?` | control handed over / released (`setCtrl`) | everyone |
| `block` | `blocked?` | someone blocked / unblocked the agent | everyone |
| `tabs` | `tabs`, `active` | tab list or active tab changed | everyone |
| `view` | `view` | screen size / quality changed | everyone |
| `chat` | `msg` | new chat line (person, agent, or system line with empty `user`) | everyone |
| `emote` | `text`, `user` | a person sent an emote | everyone (including sender) |
| `cursor` | `id`, `x`, `y`, `area` (+ `tabId`, `workerId` for agent cursors) | a person moved the pointer (ids = user id); an agent cursor uses `id` `agent` or `agent:<key>` | everyone except the sender; best-effort (dropped when slow) |
| `agent` | `act`, `text?`, `x?`, `y?`, `rect?`, `tabId?`, `workerId?` | `act=step` caption (+outline `rect`), `click` ripple at x,y, `key` key combo shown, `say` agent message | everyone |
| `clipboard` | `text` | the person in control copied (Ctrl/Cmd+C or X) in the page | that client only |
| `pick` | `event`, `pick?` | reply to the client's `pick`; no `pick` field = nothing under the pointer; hover reply is a trimmed Pick (rect, view, tag, role, name) | that client only |
| `rec` | `rec` | recording started (`rec.on=true`, `by`, `count`, `since`...), progress, or ended (`rec` = `{count:0}` i.e. empty) | everyone |
| `rec_done` | `rec` | recording saved (`rec` = FlowInfo of the flow) or nothing recorded (`rec` = `{count:0}`); never sent for a discard | the person who stopped it |
| `flows` | - | the flows list changed (also after recording, rename, delete, import, step edit) | everyone |
| `flow_run` | `rec{id, on, count:0}` | a replay started (`on:true`) / ended (`on` absent) | everyone |
| `assets` | `text?` (kind), `id?` | Assets tab changed; `id` + `text`(kind) present when a new asset was added | everyone |

## Client -> server (text)

`Hub.handle` (main.rs). Types not listed here are ignored. "needs control" = see below.

| type | fields | effect |
|---|---|---|
| `cursor` | `x`, `y`, `area` | broadcast as `cursor` with the sender's user id to all others |
| `audio` | `on` | this client wants (or not) audio packets |
| `pause` | `on` | pause/resume the stream (no frames, no audio); resuming pushes the cached frame |
| `emote` | `text` (one of `😍 😴 🤣 🎉 😱 😄 👍 ❤️`) | broadcast `emote`; rate limit 1 per 250 ms, others ignored |
| `chat` | `text` (<= 1000 runes, non-empty) | routes `@agent` / `@handle` mentions, stores (200 kept), broadcasts `chat`, wakes agents, saves to `-data` |
| `pick` | `event` (`hover`/`click`), `x`, `y` | needs a browser; answers with `pick`; a click stores the pick (code `el-xxxx`) and adds an element asset |
| `rec_start` | - | start flow recording (broadcast `rec`, system chat line) |
| `rec_stop` / `rec_discard` | - | stop (save) or discard the recording |
| `control_take` | - | take control from whoever has it (`control` broadcast) |
| `control_release` | - | give control up if you hold it |
| `agent_kick` | `id` (`@handle` or handle) | disconnect one coding agent |
| `agent_block` | `on` | block / unblock the agent from taking control |
| `nav` | `url` | navigate active tab (needs control) |
| `back`, `forward`, `reload`, `stop` | - | history / reload / stop (needs control) |
| `mouse` | `event`, `x`, `y`, `button`, `buttons`, `count`, `mods`, `dx`, `dy` | pointer input; `move` does not claim control |
| `key` | `event` (`down`/`up`), `key`, `code`, `keyCode`, `mods` | keyboard input; a copy shortcut triggers `clipboard` + a `copy` asset |
| `text` | `text` | insert text (paste / IME) |
| `tab_new` | `url?` | open a tab |
| `tab_close`, `tab_activate`, `tab_mute` | `id` | tab operations |
| `tab_move` | `id`, `index` | reorder tabs |
| `view` | `view{width,height,quality}` | resize screen / quality; only the person in control (never claims control itself) |

"Needs control": when nobody holds control, the first sender of `nav back forward reload stop mouse(non-move) key text tab_*` takes it;
if someone else holds it the message is dropped. All of these also require a browser (silently ignored with `-no-browser`).
Input also updates `humanLast` and is recorded into the flow while recording.

## Binary frames (server -> client only)

First byte is the packet kind (`pktFrame=1`, `pktAudio=2`, `app/src/browser.rs`):

- `[1][JPEG bytes...]` screencast frame of the active tab (sent latest-only; the cached frame is pushed on connect and on un-pause).
- `[2][stream id: u8][sample rate: u32 little endian][int16 stereo PCM, little endian...]` page audio; only when the client sent `audio on`
  and is not paused; best-effort.

Clients never send binary frames.

## Other WebSocket endpoints

`/devtools/` (devtools.rs) proxies Chrome DevTools sockets for tabs owned by the shared browser (raw CDP, not this protocol).
It is covered by the same auth wrapper.
