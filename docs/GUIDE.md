# Agent Browser Use — full guide

Let AI agents (Claude Code, Claude Desktop, Cursor, VS Code…) use a real browser over **MCP**, in the style of
[Playwright MCP](https://github.com/microsoft/playwright-mcp), while **you watch every step live** in a web page.
The agent's cursor glides to each element, the element gets outlined with a caption, clicks show a ripple, text is typed
key by key, and every step is logged in the chat.

It is a single Go binary. You don't need Docker, an X server or a video encoder. One headless Chrome runs on your machine,
driven over the DevTools Protocol and streamed to the web viewer.

## What you see while the agent works

| | |
|---|---|
| 🤖 **Agent cursor** | A purple "AI Agent" cursor moves to the element before acting. It sends real mouse events, so the page's hover effects show too |
| 🟪 **Highlight** | The element the agent is about to use is outlined and pulses |
| 💬 **Caption** | A line at the top of the screen shows the current step: `Click “Sign in”`, `Type “…” into “Email”`… |
| 💥 **Click ripple** | A ripple where the agent clicks |
| ⌨️ **Live typing** | Text appears key by key, like a person typing (any language, including accents and emoji). Passwords show as `•••` in captions and the log |
| 📜 **Activity log** | Each step is a chat line from "AI Agent" |
| ✋ **Take over** | The agent takes control by itself when it acts, even from a person, and hands it back to whoever had it after 15 idle seconds. Click **Block agent** to stop it from taking control (for example to log in or solve a captcha), and **Unblock agent** to let it again |

The agent can also talk to you with the `browser_say` tool, for example "Please log in for me".

## Install

Requires Go 1.22+ and Google Chrome.

    make build        # -> bin/agent-browser-use
    make run          # run from source

### Docker

Builds an image with the server and headless Chromium (amd64 and arm64), then runs it:

    make docker            # build the image and start it on http://localhost:3000 (MCP: /mcp)
    make docker-logs       # follow the logs
    make docker-stop       # stop and remove the container

Cookies/logins stay in the `agent-browser-use-data` volume, so rebuilding keeps them.
The port is published on `127.0.0.1` only. `make docker HOST=0.0.0.0` shares the viewer on your network
(the MCP endpoint is then reachable from it too). Other options: `PORT=8080`, `ARGS="-quality 60 -agent-speed 1"`.

## Connect an agent

There are two ways to connect. Both give the same tools.

### Option 1: stdio (the agent starts it)

The agent runs `agent-browser-use mcp` itself. The first time the agent uses the browser while nobody is watching,
the viewer at **http://localhost:3000** opens in your default browser.

**Claude Code**

    claude mcp add agent-browser -- /path/to/agent-browser-use mcp

**Claude Desktop / Cursor / other clients** (`claude_desktop_config.json`, `~/.cursor/mcp.json`…)

```json
{
  "mcpServers": {
    "agent-browser": {
      "command": "/path/to/agent-browser-use",
      "args": ["mcp"]
    }
  }
}
```

In this mode, cookies/logins and chat history are stored in `~/.agent-browser-use/`.
Sometimes a server is already running on the port, for example because you started Option 2 or another agent
session is open. Then `mcp` just forwards to that server, so everyone shares one browser.

### Option 2: HTTP (you start the server)

    ./agent-browser-use                 # viewer: http://localhost:3000, MCP: http://localhost:3000/mcp

**Claude Code**

    claude mcp add --transport http agent-browser http://localhost:3000/mcp

**Other clients**

```json
{
  "mcpServers": {
    "agent-browser": { "url": "http://localhost:3000/mcp" }
  }
}
```

(VS Code uses the `"servers"` key and needs `"type": "http"`.)

Then ask your agent something like *"Go to news.ycombinator.com, open the top story and summarize it"*, and open
http://localhost:3000 to watch.

## MCP tools

These tools work like Playwright MCP. Call `browser_snapshot` to get an outline of the page. Every element you can act
on has a `ref` (for example `e12`). Pass that `ref` to the action tools. Every action tool returns a fresh snapshot.

| Tool | What it does |
|---|---|
| `browser_navigate` | Open a URL in the current tab (text that isn't an address is searched on Google) |
| `browser_navigate_back` / `browser_navigate_forward` | Go back / forward in history |
| `browser_reload` | Reload the page |
| `browser_snapshot` | Accessibility-style outline of the page, with a `ref` for each actionable element |
| `browser_click` | Click (`doubleClick`, `button`: left/right/middle) |
| `browser_hover` | Move the mouse over an element (opens menus, tooltips…) |
| `browser_type` | Click a text field and type. `clear` (default true) replaces the old content; `submit` presses Enter |
| `browser_press_key` | Press a key or shortcut: `Enter`, `Escape`, `ArrowDown`, `Tab`, `Control+A`, `Shift+Tab`… |
| `browser_select_option` | Pick options in a `<select>` by value or label |
| `browser_scroll` | Scroll the page (or inside an element) with the mouse wheel |
| `browser_drag` | Drag one element onto another |
| `browser_wait_for` | Wait for text to appear or disappear, or for a number of seconds |
| `browser_wait_for_change` | Wait until the page changes on its own (the other player's move, a reply…) and return right away with what changed. Clocks ticking don't count; `ref`/`selector` watches one element |
| `browser_take_screenshot` | Screenshot of the current tab (JPEG) |
| `browser_evaluate` | Run a JavaScript function in the page and return the result as JSON |
| `browser_tabs` | List / open / switch / close tabs. Tabs a page opens (`target=_blank`) become the current tab automatically |
| `browser_say` | Show a message to the people watching (on-screen caption + chat line) |

### Parallel tab workers

**Multi-agent recipe:** the main coding agent calls `browser_tabs {"action":"new","background":true,"url":"…"}` once per sub-agent. Each call returns a `tabId`. Hand one `tabId` to each sub-agent; the sub-agent passes it on every `browser_*` call. Each tab then has its own cursor, refs and snapshots, so sub-agents never click on each other's pages. Never share one `tabId` between two sub-agents.

Every browser tool accepts an optional `tabId`. Omit it for the existing active-tab behavior; when supplied, the MCP agent uses an independent serialized worker lane and cursor/snapshot state for that tab. Calls for different tabs can run concurrently, while calls targeting the same tab remain ordered. Tab creation, selection, and closing still use the shared browser lifecycle. Background-tab operations are parallel, but the visible video stream follows the human-selected active tab (only one tab can be streamed).

### See what the page is doing

| Tool | What it does |
|---|---|
| `browser_console_messages` | `console.*` calls, uncaught exceptions and browser warnings of the current page. `level` (error, warning, info, debug, all), `limit`, `clear`, `history` (also earlier pages) |
| `browser_network_requests` | Requests of the current page: method, URL, status, type, time, size. `filter` (text or regex), `method`, `failedOnly`, `includeStatic`. `id` shows the headers of one request and `body: true` its response body (sensitive headers such as `Cookie` and `Authorization` are hidden) |

Every action that returns a page snapshot (click, type, navigate…) also lists **Page problems since the last step**: new console errors/warnings and failed requests (4xx/5xx or network errors). The viewers see a `⚠️` line in the chat when that happens.

### Files

| Tool | What it does |
|---|---|
| `browser_downloads` | `list` downloaded files (with their path on the server), `wait` until a download finishes, `read` a text file |
| `browser_file_upload` | Fill a file input (give `ref` of the input, or of its button/label). `paths` are names of files in the upload folder, or `files` (`name` + base64 `data`) are written there first |

Downloads go to `downloads/` and uploads are read from `uploads/` (both next to the profile folder; change with `-downloads` and `-uploads`). Uploading refuses any file outside those two folders.

### Cookies, storage and sessions

| Tool | What it does |
|---|---|
| `browser_cookies` | `list` (current page, or `all`), `set`, `delete`, `clear` (everything, or one `domain`) |
| `browser_storage` | `list`, `get`, `set`, `remove`, `clear` on localStorage (`type: local`) or sessionStorage (`type: session`) |
| `browser_state` | `save` all cookies plus the current site's localStorage/sessionStorage under a `name`; `load`, `list`, `delete` them later (kept in `states/`, change with `-states`) |

### Snapshot diff and device emulation

| Tool | What it does |
|---|---|
| `browser_snapshot_diff` | What changed since the last snapshot you saw, or since a baseline saved earlier with `save: "name"` and used with `baseline: "name"` |
| `browser_emulate` | `device`: iPhone 15, iPhone SE, iPad, Pixel 8, Galaxy S23, Galaxy Tab S9, Laptop, Desktop HD, or `reset`. Or set `width`/`height`/`scale`/`mobile`/`touch`/`userAgent`. Also `locale`, `timezone`, `colorScheme`, `latitude`/`longitude`, `network` (online, offline, slow-3g, fast-3g, 4g). Applies to every tab, resizes the shared screen to the device and reloads the page. No arguments shows the current settings |

`alert`/`confirm` dialogs are accepted automatically, and their text appears in the chat.

## Options

    ./agent-browser-use -port 8080              # another port
    ./agent-browser-use -agent-speed 1          # animate the agent's cursor and typing so people can follow (default 0 = instant)
    ./agent-browser-use -open=false             # don't open the viewer automatically
    ./agent-browser-use -agent-idle 1m          # agent hands back control after 1 idle minute (default 15s, 0 = never)
    ./agent-browser-use -width 1600 -height 900 # screen size
    ./agent-browser-use -quality 50             # lighter images for slow networks
    ./agent-browser-use -start https://duckduckgo.com   # page for new tabs
    ./agent-browser-use -max-tabs 8             # cap open tabs (default 12)
    ./agent-browser-use -no-audio               # don't capture sound
    ./agent-browser-use -no-mcp                 # turn off /mcp, use it only as a shared browser room
    ./agent-browser-use -mcp-remote             # allow other machines on the LAN to call /mcp

The same options work with `agent-browser-use mcp …`. See them all with `./agent-browser-use -h`.

## Shared browser room

Besides the agent, the viewer is a shared browser room. Several people can watch at once (also through the LAN link
printed at startup). They hear the page's audio, chat, send emoji reactions, and take turns driving.

- **Real tabs**: open, close, switch, drag to reorder, mute a tab.
- **Address bar**: back/forward/reload/stop, search from the address bar.
- **Control**: one person (or the agent) drives at a time, using **Take control** / **Take over** / **Release** in the bottom bar. **Block agent** keeps the agent from taking control.
- **Settings**: resolution and image quality for the whole room; scroll speed, other people's cursors and reactions per device.

### Shortcuts (while the screen is focused)
| Keys | Action |
|---|---|
| Alt+T / Alt+W | New tab / close tab |
| Alt+1…9 | Switch tab |
| Ctrl/⌘+L | Address bar |
| Ctrl/⌘+R, F5 | Reload |
| Alt+← / Alt+→ | Back / forward |

## How it works

| | |
|---|---|
| Video | The active tab sends a JPEG frame whenever it changes (screencast) → WebSocket → `<canvas>` |
| Audio | A script injected into each page taps `<audio>`/`<video>`/Web Audio and sends PCM → WebSocket → Web Audio API |
| Agent | MCP (JSON-RPC over stdio or HTTP) → `agent.go` sends real mouse/keyboard events through DevTools to the active tab. `agent.js` runs in the page to build snapshots and locate elements. Every step is broadcast to viewers (cursor, outline, caption, log) |
| Control | The agent takes control when it acts and gives it back to the person it took it from after being idle (`-agent-idle`). While someone has clicked **Block agent**, it can't take control and is told so |

| File | |
|---|---|
| `app/main.go` | Web server, the room (viewers, chat, control), startup |
| `app/browser.go` | Headless Chrome: tabs, screencast, audio, people's mouse/keyboard |
| `app/agent.go` | The agent's actions and what viewers see of them |
| `app/agent.js` | Runs in the page: snapshot, `ref`s, element positions |
| `app/mcp.go` | MCP protocol, tool list, stdio/HTTP transports |
| `app/static/` | Web UI |

## Data

- `./agent-browser-use` keeps `chrome-profile/` (cookies/logins) in the current folder; `agent-browser-use mcp` keeps it in `~/.agent-browser-use/`.
- Chat history is kept in memory only and is gone after a restart. Pass `-data chat.json` to keep it in a file.
- Downloads, uploads and saved sessions live in `downloads/`, `uploads/` and `states/` next to the profile folder (`-downloads`, `-uploads`, `-states` to change).
- Change the profile folder with `-profile`. Delete `chrome-profile/` to log out of everything.

## Security

- The shared browser keeps the cookies of every account you log into. The agent can read any open page and run
  JavaScript in it, so only log into accounts you are fine with the agent using.
- By default `/mcp` only accepts connections from this machine. It also rejects requests from other websites (checks `Origin`).
- Anyone on your LAN with the link can open the viewer (port 3000) and drive the browser, so keep it on a trusted network.
- `browser_file_upload` can only send files from the upload and download folders. Saved sessions in `states/` hold cookies and storage in plain files (mode 600): treat them like passwords.
- Only `http(s)` addresses can be opened. `file://`, `chrome://` and similar are blocked.

## Limitations

- No file upload/download, no browser extensions.
- Native `<select>` popups don't show in the stream. The agent picks options with `browser_select_option`.
- Snapshots can't read the content of cross-origin iframes.
- Sound from cross-origin media without CORS headers can't be captured.
