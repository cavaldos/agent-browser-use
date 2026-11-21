# Agent Browser Use

**Let AI coding agents drive a real browser over MCP, while you watch every step live in a web page and can take over at any time.**

Agent Browser Use is a single Go binary that runs one headless Chrome on your machine and exposes it as a set of
[Model Context Protocol](https://modelcontextprotocol.io) tools in the style of Playwright MCP. Any MCP-capable agent
(Claude Code, Codex, OpenCode, Pi, Cursor, VS Code…) can navigate, read the page, click, type and log in for you, while
a web viewer shows the agent's cursor, the element it is acting on, and a log of every step.

<p align="center">
  <a href="https://github.com/cavaldos/agent-browser-use/releases/latest"><img src="https://img.shields.io/badge/download-latest-4c1" alt="Download latest"></a>
  <a href="https://github.com/cavaldos/agent-browser-use/releases"><img src="https://img.shields.io/github/downloads/cavaldos/agent-browser-use/total?label=downloads&color=4c1" alt="Downloads"></a>
  <a href="https://github.com/cavaldos/agent-browser-use/stargazers"><img src="https://img.shields.io/github/stars/cavaldos/agent-browser-use?label=stars&color=007ec6" alt="Stars"></a>
  <a href="https://github.com/cavaldos/agent-browser-use/network/members"><img src="https://img.shields.io/github/forks/cavaldos/agent-browser-use?label=forks&color=007ec6" alt="Forks"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/cavaldos/agent-browser-use?label=license&color=4c1" alt="License: MIT"></a>
  <a href="https://github.com/sponsors/cavaldos"><img src="https://img.shields.io/badge/Sponsor-%E2%9D%A4%EF%B8%8F-c224f6" alt="Sponsor"></a>
</p>

## Demo

https://github.com/user-attachments/assets/19edb4dd-3e3f-45e3-8809-b27a6c31be58

<details>
<summary>Video not playing? Watch the animated preview</summary>

![Demo](docs/images/demo.gif)

</details>

## Screenshots

<table>
  <tr>
    <td width="50%"><img src="docs/images/agent-control.png" alt="The agent is in control: cursor, captions and step log"><br><b>Watch the agent work.</b> Its cursor, a caption for every step and a log in the chat, with a one-click <i>Take over</i> or <i>Block agent</i>.</td>
    <td width="50%"><img src="docs/images/pick-element.png" alt="Picking an element to share with a coding agent"><br><b>Pick an element and share it.</b> Copy its code for your coding agent, or post it in the room chat.</td>
  </tr>
  <tr>
    <td width="50%"><img src="docs/images/public-link.png" alt="Public link popover with the link and password to copy"><br><b>Share with a public link.</b> One switch opens a built-in tunnel: copy the link (and password) and send it, no router setup. Switch off and the link stops working.</td>
    <td width="50%"></td>
  </tr>
</table>

## Features

- **Playwright-style MCP tools** — `browser_snapshot` returns an accessibility outline with a `ref` for every element, and the action tools (click, type, scroll, drag, select…) use those refs.
- **Live viewer** — watch the page stream in your browser, with the agent's cursor, captions and chat log.
- **Shared browser room** — you and the agent use the same Chrome; take control at any time, or block the agent.
- **Pick an element** — click the pick button, point at anything on the page and get a short code (e.g. `el-1d2a`). Paste it to your coding agent, which looks the element up with `browser_get_element` and can act on it right away.
- **Record a flow** — press the red record button, do a task in the browser, then stop. You get a flow id (e.g. `flow-3fa9c1`): paste just the id to your agent, which looks it up with `browser_get_flow` and replays it with `browser_run_flow` (repeat it N times, or change what gets typed with `inputs`) or adapts it to what you ask. The **Flows** tab in the sidebar lists your flows: view and edit their steps, rename, replay, export/import or delete them. Passwords are never stored: the agent must be given them at replay time.
- **Assets tab** — everything the browser produced while you work is kept in the sidebar's **Assets** tab: screenshots (the agent's `browser_take_screenshot` and the *Capture screen* button), text copied in the shared browser, elements you picked and downloaded files. View, copy, download or remove each one. They survive a restart (`assets` folder next to the profile).
- **Inspect (DevTools)** — press the `</>` button in the bottom bar to dock Chrome's own DevTools under the shared screen (it follows the active tab; drag its top edge to resize, or pop it out into a window). It goes through the server, so it sits behind the password and works over the public link. Pass `-no-devtools` to keep Chrome's local debugging port closed.
- **Persistent logins** — cookies live in a profile folder, so you sign in once and the agent reuses the session.
- **No heavy dependencies** — no Docker, X server or video encoder needed. Docker is available if you prefer it.

## Quick start

1. Download the project and start the server (requires Go 1.22+ and Chrome/Chromium, or just Docker):

   ```sh
   git clone https://github.com/cavaldos/agent-browser-use.git
   cd agent-browser-use
   make run            # starts the server on port 3000
   ```

   Or run it with Docker: `make docker`. If you installed the binary (see [Install](#install)), just run `agent-browser-use`.

2. The server listens on **port 3000**:
   - Viewer (watch the agent): **http://localhost:3000**
   - MCP endpoint (connect agents): **http://localhost:3000/mcp**

Keep the server running while you use your agents.

### Run commands

```sh
agent-browser-use                       # web UI + MCP over HTTP (port 3000)
agent-browser-use -p 8080 -pw secret    # other port, with a password
agent-browser-use mcp                   # MCP over stdio (an agent starts it for you); also serves the web UI
agent-browser-use -version              # print the version
agent-browser-use -update               # update to the latest release
```

From source, use `make`:

| Command | What it does |
|---|---|
| `make run` | Run from source. Short options: `make run p=8080 pw=secret engine=brave` |
| `make run -- -p 8080 -pw secret` | Pass any flag straight to the app |
| `make build` | Build `bin/agent-browser-use` |
| `make install` | Build and install to `~/.local/bin` (`BINDIR=...` to change) |
| `make docker` / `make docker-logs` / `make docker-stop` | Run in Docker, show its logs, stop it |
| `make docker-warp` | Docker with a Cloudflare WARP container as the browser's proxy |

`PORT=8080`, `HOST=0.0.0.0` and `ARGS="-agent-speed 1"` also work with `make`.

### Flags

| Flag | Default | Meaning |
|---|---|---|
| `-port`, `-p` | `3000` | HTTP port |
| `-pw` | none | Password for the web UI and `/mcp` (or env `AGENT_BROWSER_USE_PASSWORD`) |
| `-home` | `log` (current folder) | The one folder where everything is stored (see below; or env `AGENT_BROWSER_USE_HOME`) |
| `-engine` | `auto` | `chromium`, `chrome`, `edge`, `brave`, `vivaldi`, `headless-shell` (lightest) |
| `-chrome` | auto-detect | Path to the browser executable |
| `-width`, `-height` | `1280`, `720` | Shared screen size |
| `-quality` | `92` | Image quality 10-100 (lower = less bandwidth) |
| `-max-tabs` | `12` | Maximum open tabs |
| `-start` | `https://www.google.com` | Page for new tabs |
| `-agent-speed` | `0` | Agent cursor/typing pace: `0` instant, `1` normal, `2` slower |
| `-agent-idle` | `15s` | Give control back after the agent is idle this long (`0` = never) |
| `-open` | `true` | Open the viewer when the agent starts and nobody is watching |
| `-proxy` | none | Upstream SOCKS5 proxy for the browser only (or env `AGENT_BROWSER_USE_PROXY`) |
| `-tunnel` | off | Public https link through a built-in Cloudflare tunnel (sets a password if none) |
| `-subdomain` | random | With `-tunnel`: name for the public URL |
| `-mcp-remote` | off | Allow `/mcp` from other machines (default: this machine only) |
| `-no-mcp` | off | Disable the `/mcp` endpoint |
| `-no-browser` | off | Disable the shared browser |
| `-no-audio` | off | Disable browser audio |
| `-data` | none | File to keep chat history across restarts (default: memory only) |
| `-profile`, `-downloads`, `-uploads`, `-states`, `-flows` | inside `-home` | Override where one specific part is stored |

Run `agent-browser-use -h` for the full list. Set `AGENT_BROWSER_USE_NO_MENU=1` to run in the foreground instead of the terminal menu.

### Where your data goes

Everything the app creates lives in **one folder** named `log/`, created in the folder you start the app from (`make run`: the project root). Change it with `-home <folder>`. With `agent-browser-use mcp` (started by an agent from any folder) it is `~/.agent-browser-use/` instead.

```
log/
  chrome-profile/        browser cookies and logins (delete it to log out of everything)
  downloads/             files the browser downloaded
  uploads/               files the agent may upload from
  states/                saved login sessions (contain cookies: treat like passwords)
  flows/                 recorded flows
  warp.json              built-in WARP account (private key, don't share)
  agent-browser-use.log  server log
```

**Own IP via Cloudflare WARP (browser only):** the globe chip in the header always shows the agent browser's public IP; its switch sends the browser through Cloudflare WARP. WARP runs inside the app (userspace WireGuard, no network interface, no route changes), so your computer keeps its own IP. The first time you switch it on, the app signs up a free WARP account and keeps it in `warp.json` in the home folder (private key, don't share it). To use your own SOCKS5 proxy instead, pass `-proxy socks5://host:port` (or `AGENT_BROWSER_USE_PROXY`); `make docker-warp` does that with a WARP container. DNS and WebRTC also go through the tunnel, so the real IP does not leak.

## Install

macOS / Linux (Windows: run it inside WSL)

```sh
curl -fsSL https://raw.githubusercontent.com/cavaldos/agent-browser-use/main/scripts/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/cavaldos/agent-browser-use/main/scripts/install.ps1 | iex
```

Then run `agent-browser-use`. Viewer: **http://localhost:3000**, MCP endpoint: **http://localhost:3000/mcp**.

Uninstall: `rm ~/.local/bin/agent-browser-use` (Windows: `Remove-Item -Recurse $env:LOCALAPPDATA\agent-browser-use`)

Prefer Docker or need more options? See [docs/INSTALL.md](docs/INSTALL.md).

### Browser engine

```sh
# macOS
brew install --cask chromium
# Debian / Ubuntu
sudo apt install -y chromium
# Fedora
sudo dnf install -y chromium
# Windows (PowerShell)
winget install Hibbiki.Chromium
```

## Connecting a coding agent (MCP)

Start the server with a password (`./agent-browser-use -pw your-password`), then point your agent at
`http://localhost:3000/mcp`. New to MCP, or want the stdio mode? Read the [general guide](docs/MCP.md).

> **No password?** Start the server without `-pw`, then delete the `Authorization` header / token lines in the snippets below.

### Claude Code

```sh
claude mcp add --transport http agent-browser http://localhost:3000/mcp --header "Authorization: Bearer your-password"
```

Or in a project `.mcp.json`:

```json
{
  "mcpServers": {
    "agent-browser": {
      "type": "http",
      "url": "http://localhost:3000/mcp",
      "headers": { "Authorization": "Bearer your-password" }
    }
  }
}
```

Check it with `claude mcp list` or `/mcp`.

### Codex (OpenAI)

`~/.codex/config.toml` (or `.codex/config.toml` in a trusted project). The password stays in an environment variable so it isn't written in the file:

```toml
[mcp_servers.agent-browser]
url = "http://localhost:3000/mcp"
bearer_token_env_var = "AGENT_BROWSER_USE_PASSWORD"
```

```sh
export AGENT_BROWSER_USE_PASSWORD=your-password
```

Restart Codex after editing the file.

### OpenCode

`opencode.json` (project root or global config):

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "agent-browser": {
      "type": "remote",
      "url": "http://localhost:3000/mcp",
      "enabled": true,
      "headers": { "Authorization": "Bearer your-password" }
    }
  }
}
```

### Pi

MCP is not part of every Pi install; it is often added through an extension such as the community `pi-mcp-adapter`.
Once MCP is enabled, use the common `mcpServers` format:

```json
{
  "mcpServers": {
    "agent-browser": {
      "type": "http",
      "url": "http://localhost:3000/mcp",
      "headers": { "Authorization": "Bearer your-password" }
    }
  }
}
```

### Other agents

Cursor, VS Code, Windsurf, Gemini CLI, Cline, Zed, Claude Desktop and more:
**[docs/MCP-CLIENTS.md](docs/MCP-CLIENTS.md)**.

### Verify the connection

Ask your agent something like:

> Go to news.ycombinator.com, open the top story and summarize it.

Then open **http://localhost:3000** to watch the agent work.

## Available tools

| Tool | What it does |
|---|---|
| `browser_navigate` / `browser_navigate_back` / `browser_navigate_forward` / `browser_reload` | Open a URL (non-URL text is searched on Google), or move through history |
| `browser_snapshot` | Accessibility outline of the page, with a `ref` for each actionable element |
| `browser_click` / `browser_hover` / `browser_type` / `browser_press_key` | Click, hover, type into a field, press keys and shortcuts |
| `browser_select_option` / `browser_scroll` / `browser_drag` | Pick `<select>` options, scroll, drag and drop |
| `browser_wait_for` / `browser_wait_for_change` | Wait for text, or for the page to change on its own |
| `browser_take_screenshot` / `browser_evaluate` | Screenshot the tab, or run JavaScript in the page |
| `browser_tabs` / `browser_say` | Manage tabs, or show a message to the people watching |
| `browser_console_messages` / `browser_network_requests` | Console output, exceptions and network requests (status, headers, response body). Action results also list new console errors and failed requests |
| `browser_downloads` / `browser_file_upload` | Wait for and read downloaded files; fill `<input type=file>` from the upload folder |
| `browser_cookies` / `browser_storage` / `browser_state` | Read and change cookies and localStorage/sessionStorage; save and restore a whole login session by name |
| `browser_snapshot_diff` | Only what changed on the page since the last snapshot or a named baseline |
| `browser_emulate` | Phone/tablet presets, screen size, pixel ratio, touch, user agent, locale, time zone, dark mode, GPS location, network speed |
| `browser_get_flow` | Read a flow a person recorded (by its id, e.g. `flow-3fa9c1`), or list all flows |
| `browser_run_flow` | Replay a recorded flow, optionally several times, with changed inputs (`inputs`) |

### Recorded flows

Record a task once, then let the agent repeat it or adapt it:

1. Press the red record button under the screen and use the browser as usual (clicks, typing, choices in lists, keys, links, tabs).
2. Press **Stop & save**. You get an id such as `flow-3fa9c1`.
3. Paste the id to your agent with what you want, e.g. "flow-3fa9c1, do it for these three names". Alone, the id replays the flow once.

The agent finds the flow with `browser_get_flow`, then replays it with `browser_run_flow` or follows its steps when the job needs changes. Actions the agent takes itself are not recorded.

- **Passwords are never stored.** A password step needs its value passed in `inputs` at replay time, or a saved login (`browser_state`).
- **Flows tab** (sidebar): list, search, rename, view and edit steps, remove steps, replay from the viewer, export and import as `.json`, delete.
- Flows are saved in the `flows/` folder inside the home folder (change it with `-flows <folder>`).
- Not recorded: hover-only menus, scrolling, and anything inside an iframe.

See [docs/GUIDE.md](docs/GUIDE.md) for the full tool reference, options, the shared browser room, security and limitations.

## Security

- Uploads only read from the upload folder (and the download folder), never from other places on the machine. Saved sessions (`states/`) contain cookies: treat them like passwords.
- The shared browser keeps the cookies of every account you log into. The agent can read any open page and run JavaScript in it, so only log into accounts you are happy for the agent to use.
- By default `/mcp` only accepts connections from the local machine, and it rejects requests from other websites.
- Without `-pw`, anyone who can reach port 3000 can open the viewer and drive the browser. Keep it on a trusted network, or set a password.

## Project layout

```
app/        Go source (MCP server, browser control, agent, web UI in static/)
docker/     Dockerfile and entrypoint
scripts/    run / build / docker scripts
docs/       full guide and images
bin/        built binary (make build)
```

## Documentation

- [docs/INSTALL.md](docs/INSTALL.md) — Docker, building from source, options.
- [docs/GUIDE.md](docs/GUIDE.md) — full guide: tools, options, shared browser room, data, security and limitations.
- [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md) — contributing and releasing.

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=cavaldos/agent-browser-use&type=Date)](https://star-history.com/#cavaldos/agent-browser-use&Date)

## License

See [LICENSE](LICENSE).
