# UI HTTP calls (JS → Rust server)

Catalogue of every HTTP/WebSocket call made by the web UI (`app/static/js/*.js`, `app/static/loader.js`, `app/agent.js`),
cross-checked against the handlers in `app/src/*.rs`.

`agent.js` makes no HTTP calls. `tabs.js` only loads external favicons (not this server).

| # | Method | Path | Query params | Request body / payload | Response fields the JS reads | JS file:line | Rust handler module |
|---|--------|------|--------------|------------------------|------------------------------|--------------|----------------------|
| 1 | WS (upgrade) | `/ws` | `name` | WS JSON messages (client→server): `audio`, `pause`, `view`, `rec_start`, `rec_stop`, `rec_discard`, chat/cursor/emote/etc. | Server→client message types: `init` (you, users, agents, ctrl, blocked, enabled, view, tabs, active, chat, rec), `agents`, `user_join` (user), `user_leave` (id), `control` (ctrl), `block` (blocked), `tabs`, `view`, `pick`, `rec`, `rec_done` (rec), `flows`, `assets`, `flow_run` (rec), `chat` (msg), `emote` (text, user), `cursor`, `agent`, `clipboard` (text); binary frames: kind 1 = screen, kind 2 = audio | app/static/js/socket.js:7 | app/src/main.rs |
| 2 | GET (poll) / POST (toggle) | `/api/keepalive` | — | POST: `{ on }` | `on`, `available`, `canToggle` (`last` present, unused) | app/static/js/keepalive.js:22 | app/src/keepalive.rs |
| 3 | GET (poll) / POST (toggle) | `/api/proxy` | — | POST: `{ on }` | `configured`, `on`, `canToggle`, `warp`, `checking`, `error`, `ip`, `loc`, `warpError` | app/static/js/proxy.js:28 | app/src/netproxy.rs |
| 4 | GET (poll) / POST (toggle) | `/api/tunnel` | — | POST: `{ on, subdomain }` | `on`, `busy`, `canToggle`, `error`, `url`, `password`, `generated` | app/static/js/tunnel.js:30 | app/src/tunnel.rs |
| 5 | GET | `/api/states` | — | — | `{ states: [{ name, cookies, sites, origin, saved }] }` → reads `states`, then `name`, `cookies`, `sites`, `origin` | app/static/js/sidebar.js:126 | app/src/states_http.rs |
| 6 | POST | `/api/states/load` | — | `{ name }` | `{ ok, message }` (status checked only) | app/static/js/sidebar.js:164 | app/src/states_http.rs |
| 7 | POST | `/api/states/delete` | — | `{ name }` | `{ ok }` (status checked only) | app/static/js/sidebar.js:172 | app/src/states_http.rs |
| 8 | POST | `/api/states/save` | — | `{ name }` | `{ ok, message }` (status checked only) | app/static/js/sidebar.js:183 | app/src/states_http.rs |
| 9 | GET | `/api/states/export` | `current=1` OR `name` | — | File download; reads `Content-Disposition` filename + blob body | app/static/js/sidebar.js:148,193 | app/src/states_http.rs |
| 10 | POST | `/api/states/import` | — | multipart `file` (or raw JSON body) | `{ imported, loaded, message\|error }` → reads `loaded`, `error` | app/static/js/sidebar.js:213 | app/src/states_http.rs |
| 11 | GET | `/api/flows` | — | — | `{ flows: [FlowInfo], running }` → reads `flows` (id, name, count, created, startUrl, secrets), `running` | app/static/js/flows.js:126 | app/src/flow_http.rs |
| 12 | GET | `/api/flows/get` | `id` | — | `{ flow: FlowInfo, startTitle, by, steps: [flowStepView] }` → reads flow (id, name, count, created, startUrl, duration), startTitle, by, steps (kind, label, hints, secret, value, values) | app/static/js/flows.js:220 | app/src/flow_http.rs |
| 13 | POST | `/api/flows/rename` | `id`, `name` | — | 204 (status only) | app/static/js/flows.js:388 | app/src/flow_http.rs |
| 14 | POST | `/api/flows/step` | `id`, `n`, `action` (`value`\|`delete`) | `{ value }` (action=value only) | 204 (status only) | app/static/js/flows.js:345,358 | app/src/flow_http.rs |
| 15 | POST | `/api/flows/delete` | `id` | — | 204 (status only) | app/static/js/flows.js:395 | app/src/flow_http.rs |
| 16 | GET | `/api/flows/export` | `id` | — | File download triggered via `location.href` (no fields read) | app/static/js/flows.js:319 | app/src/flow_http.rs |
| 17 | POST | `/api/flows/import` | — | multipart `file` (or raw JSON body) | `{ id }` → reads `id` | app/static/js/flows.js:188 | app/src/flow_http.rs |
| 18 | POST | `/api/flows/run` | — | `{ id, repeat, inputs }` | `{ ok, log }` → reads `ok`, `log` | app/static/js/flows.js:368 | app/src/flow_http.rs |
| 19 | POST | `/api/flows/stop` | — | — | 204 (status only) | app/static/js/flows.js:321 | app/src/flow_http.rs |
| 20 | GET | `/api/assets` | — | — | `{ assets: [Asset] }` → reads `assets`, then id, kind, name, text, url, title, by, time, size | app/static/js/assets.js:30 | app/src/assets.rs |
| 21 | GET | `/api/assets/file` | `id`, `download` | — | Binary/text/asset file; loaded as `img.src`, `fetch().blob()`, or download link | app/static/js/assets.js:69,106,112,139 | app/src/assets.rs |
| 22 | POST | `/api/assets/delete` | `id` | — | 204 (status only) | app/static/js/assets.js:123 | app/src/assets.rs |
| 23 | POST | `/api/assets/capture` | `by` | — | Asset JSON (ignored by JS; toast only) | app/static/js/assets.js:160 | app/src/assets.rs |
| 24 | POST | `/api/assets/clear` | — | — | 204 (status only) | app/static/js/assets.js:165 | app/src/assets.rs |
| 25 | GET | `/api/devtools` | — | — | `{ ok, error? }` → reads `ok`, `error` | app/static/js/devtools.js:25 | app/src/devtools.rs |
| 26 | GET / WS (upgrade) | `/devtools/...` (incl. `/devtools/page/{id}`) | `ws`/`wss` host path | — | Proxied Chrome DevTools front end + per-tab WebSocket (no fields parsed by our JS) | app/static/js/devtools.js:18,39 | app/src/devtools.rs |
| 27 | GET | static includes (loader) | path from `data-include` | — | HTML text inserted into DOM | app/static/loader.js:4 | app/src/main.rs |
| 28 | GET | static JS bundles | `js/{name}.js` | — | Script source (loaded via `<script src>`) | app/static/loader.js:14 | app/src/main.rs |

### Notes
- `/mcp` (`app/src/main.rs`) is an HTTP endpoint but is used by external MCP clients, not by the UI; omitted.
- External favicon loads in `app/static/js/tabs.js:95` (`img.src = t.favicon`) are not to this server; omitted.
- WebSocket client→server message types beyond those listed are defined by the hub's `Msg` handling in `app/src/main.rs` and the other `app/src/*.rs` modules; see `app/parity/ws-messages.md`.
