# Project layout

This project is a single Rust binary (`agent-browser-use`) that runs one shared Chrome
instance. People watch and drive it through a web UI; AI agents drive the same browser
through MCP. Everything the browser produces (screenshots, downloads, recorded flows,
saved logins) is written to a folder on disk.

The binary is built from `app/`, the browser UI is embedded into it from `app/static/`,
and the scripts in `scripts/` wrap build, run, release and Docker tasks.

## Top level

| Path | What lives there |
| --- | --- |
| `app/` | Rust source, static web UI, golden parity outputs, Cargo manifest |
| `bin/` | Built binary (`bin/agent-browser-use`), produced by `make build` |
| `docker/` | Dockerfile, entrypoint and the Chromium package list |
| `scripts/` | Run / build / release / Docker / install scripts |
| `docs/` | This guide and the other documentation, plus images and a demo video |
| `.github/` | CI and release workflows, funding config |
| `.opencode/` | OpenCode agent configuration for working on this repo |
| `Makefile` | Short commands (`make run`, `make build`, `make release`, `make docker`) |
| `run` | One-line convenience launcher for `scripts/run.sh` |
| `warp.json` | Local WARP configuration used during development |
| `README.md` | Project overview, quick start, security notes, links to `docs/` |
| `LICENSE` | License text |

## `app/` — the Rust application

```
app/Cargo.toml          manifest: deps (axum, tokio, reqwest, boringtun, ...), binary target
app/src/main.rs         entry point: flags, paths, HTTP router, shared state, wiring of modules
app/src/agent/          the AI side: MCP server, agent loop, chat, element picking, extra tools
app/src/browser/        the shared Chrome instance and everything spoken over CDP
app/src/network/        tunnels, proxies and keep-alive
app/src/recording/      flows, screenshots/assets, downloads/uploads, saved sessions
app/src/system/         auth, self-update, terminal menu, process/resource helpers
app/static/             web UI, embedded into the binary at build time (rust-embed)
app/parity/             golden outputs from the former Go version, for behaviour checks
app/cloudflared/        cloudflared binary downloaded at build time (gitignored)
app/target/             Rust build output (gitignored)
```

Modules are declared in `main.rs` with `#[path = "..."] mod name;` rather than through
per-directory `mod.rs` files. `main.rs` is where they plug together: a module implements
`BrowserApi` / `HubHooks` and contributes routes through `build_router`.

### `app/src/agent/` — MCP and the agent

| File | Responsibility |
| --- | --- |
| `mcp.rs` | MCP server over HTTP `/mcp` or stdio. Implements the Playwright-style tools (`browser_navigate`, `browser_snapshot`, `browser_click`, …) that let a coding agent drive the browser. |
| `agent.rs` | The agent loop: interprets a task and drives the shared browser like a person, so watchers can follow along in the UI. |
| `chat.rs` | "@agent" chat: every MCP client that connects is a named "seat"; people write to a seat in the viewer and the agent reads/replies with `browser_read_chat` / `browser_say`. |
| `pick.rs` | The viewer's Pick tool: a person points at an element and gets a short ref like `el-3f9a` to paste into an agent, which resolves it with `browser_get_element`. |
| `tools.rs` | The extra MCP tools: console/network inspection, downloads/uploads, cookies and storage, saved sessions, snapshot diffs and device emulation. |
| `mcp_tools.json` | Tool schema definitions shipped for MCP clients. |

### `app/src/browser/` — Chrome and CDP

| File | Responsibility |
| --- | --- |
| `browser.rs` | The shared browser itself: one headless Chrome with real tabs, screencast frames for the active tab, and audio capture from tabs. |
| `cdp.rs` | Chrome DevTools Protocol transport: starting Chrome over `--remote-debugging-pipe`, flat target sessions, per-session event listeners. |
| `chromecmd.rs` | Platform-specific Chrome process settings (on Linux: kill Chrome with the parent via `PDEATHSIG`). |
| `engine.rs` | Chooses the Chromium-family engine (chrome-headless-shell, Chrome, Brave, …); all speak CDP. |
| `devtools.rs` | Exposes Chrome DevTools for the shared tabs inside the viewer. |
| `emulate.rs` | Device emulation: screen size, pixel ratio, touch, user agent, locale, time zone, dark mode, geolocation, network speed. Settings apply to all tabs, including new ones. |
| `frames.rs` | Cross-origin iframes (e.g. an embedded checkout form), which need separate handling from the main frame. |
| `observe.rs` | Console messages, page errors and network requests, kept in small ring buffers and surfaced as MCP tools. |
| `share.rs` | Screen sharing from inside the browser (Discord, Meet, …) by faking `getDisplayMedia`, since headless Chrome has no real picker. |
| `stealth.rs` | Makes the headless, CDP-driven Chrome look like a regular Chrome session (follows the Obscura approach). |

### `app/src/network/` — connectivity

| File | Responsibility |
| --- | --- |
| `tunnel.rs` | `-tunnel`: requests a Cloudflare tunnel from the nport backend and runs the embedded `cloudflared` binary — no router config or port forwarding needed. |
| `warp.rs` | Built-in Cloudflare WARP: a userspace WireGuard tunnel, so only connections routed through it change IP; the machine keeps its own. |
| `netproxy.rs` | ProxySwitch: points Chrome at a local SOCKS5 proxy so its public IP can be changed while running. |
| `keepalive.rs` | Optional mouse movement and scrolling to keep sessions alive on sites that log out on inactivity. Off by default. |

### `app/src/recording/` — data produced by the browser

| File | Responsibility |
| --- | --- |
| `flow.rs` | Recorded flows: clicking Record and using the browser turns actions into steps saved under a short id (e.g. `flow-3fa9c1`) that an agent can replay. |
| `flow_http.rs` | HTTP API behind the Flows tab: list, read, rename, edit, delete, replay, export, import; changes are broadcast to all viewers. |
| `assets.rs` | The Assets tab: screenshots taken by agents or the Capture button, kept together for later review. |
| `files.rs` | Downloads and uploads: Chrome writes downloads to the download folder and the app tracks progress; the agent may only upload from the upload folder. |
| `states.rs` | Saved sessions (logins): all cookies plus localStorage/sessionStorage per site. Treat these as passwords. |
| `states_http.rs` | HTTP API for saved sessions: list, save, load, delete, export/import as a JSON bundle. |

### `app/src/system/` — process and host concerns

| File | Responsibility |
| --- | --- |
| `auth.rs` | `-pw` password protection: login page and cookie for people, Bearer/Basic auth for MCP clients. |
| `fileserver.rs` | Serves the embedded UI and sanitizes paths in front of routes. |
| `menu.rs` | Interactive terminal mode: runs the real server as a child and offers Restart / background / Terminate. Disable with `AGENT_BROWSER_USE_NO_MENU=1`. |
| `proc.rs` | Process helpers: detach, check alive, terminate, kill (separate Unix/Windows implementations). |
| `prompt.rs` | Asks for the password via a native dialog when stdin/stdout are occupied by MCP. |
| `resources.rs` | Keeps the browser from hogging the machine (CPU duty-cycling with SIGSTOP/SIGCONT, etc.). |
| `update.rs` | `--update` / `-u`: replaces the binary with the latest GitHub release. |

### `app/static/` — the web UI

Embedded into the binary with `rust-embed`, so no separate web server is needed.

```
app/static/index.html            page shell
app/static/loader.js             startup
app/static/components/           markup fragments: header.html, sidebar.html, stage.html,
                                 controlbar.html, login.html
app/static/css/                  base, layout, controls, browser, overlays, theme and
                                 "skin-*" files for the different visual themes
app/static/js/                   one file per concern: boot, core, socket, screen, tabs,
                                 toolbar, sidebar, agent, flows, assets, devtools,
                                 cursors, emotes, icons, join, keepalive, members,
                                 mentions, picker, proxy, resources, sound, theme, tunnel
```

### `app/parity/` — behaviour parity checks

Golden outputs captured from the former Go implementation, used by `diff.sh` / `run.sh`
to confirm the Rust rewrite behaves the same.

```
app/parity/golden/       expected output for help, version, bad flags, unexpected args, env proxy
app/parity/run.sh        runs the comparison
app/parity/diff.sh       reports differences
app/parity/http-calls.md expected HTTP calls
app/parity/ws-*.md, *.mjs expected WebSocket messages
```

## `docker/`

```
docker/Dockerfile       image definition; embeds cloudflared at build time
docker/entrypoint.sh    container startup (flags, profile folders, browser launch)
docker/chromium/        package/source list for installing Chromium in the image
```

## `scripts/`

| Script | Purpose |
| --- | --- |
| `run.sh` | Run from source with flags (`-p`, `-pw`, `-engine`, …). |
| `build.sh` | Build `bin/agent-browser-use` (downloads cloudflared first). |
| `build-release.sh` | Build release binaries for macOS, Linux (arm64/amd64) and Windows (amd64). |
| `release.sh` | Tag and push a `vX.Y.Z` release. |
| `test-release.sh` | Local dry run of the release pipeline (`ARGS=--quick` for a fast check). |
| `fetch-cloudflared.sh` | Download cloudflared into `app/cloudflared/`. |
| `docker-build.sh`, `docker-run.sh`, `docker-warp.sh` | Build/run the image, with or without WARP. |
| `install.sh`, `install.ps1` | Install the binary on Unix / Windows. |

## `docs/`

| File | Contents |
| --- | --- |
| `GUIDE.md` | Full guide: tools, options, shared browser room, data, security, limitations. |
| `INSTALL.md` | Docker, building from source, options. |
| `TOOLS.md` | MCP tools and recorded flows. |
| `MCP.md`, `MCP-CLIENTS.md` | MCP setup and client-specific instructions. |
| `CONTRIBUTING.md` | Contributing and releasing. |
| `PROJECT-LAYOUT.md` | This document. |
| `images/`, `videodemo.mp4` | Screenshots and demo media used by the README. |

## `.github/`

- `workflows/ci.yml` — `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` on Linux and macOS.
- `workflows/release.yml` — builds and publishes the release binaries.
- `FUNDING.yml` — funding links.

## Runtime data (`-home`)

All persistent state lives under one folder: by default `~/.config/agent-browser-use/`,
or `~/.agent-browser-use/` for the `mcp` command. Inside it:

```
chrome-profile*/      browser profiles (one per engine variant)
downloads/            downloaded files
uploads/              the only folder the agent may upload from
states/               saved logins/sessions (cookies — keep private)
flows/                recorded flows
assets/               screenshots and other captured assets
warp.json             WARP configuration
```

For the exact flags and paths, see [INSTALL.md](INSTALL.md) and [GUIDE.md](GUIDE.md).
