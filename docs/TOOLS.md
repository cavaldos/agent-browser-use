# Available tools

| Tool | What it does |
|---|---|
| `browser_navigate` / `browser_navigate_back` / `browser_navigate_forward` / `browser_reload` | Open a URL (non-URL text is searched on Google), or move through history |
| `browser_snapshot` | Accessibility outline of the page, with a `ref` for each actionable element |
| `browser_click` / `browser_hover` / `browser_type` / `browser_press_key` | Click, hover, type into a field, press keys and shortcuts |
| `browser_select_option` / `browser_scroll` / `browser_drag` | Pick `<select>` options, scroll, drag and drop |
| `browser_wait_for` / `browser_wait_for_change` | Wait for text, or for the page to change on its own |
| `browser_take_screenshot` / `browser_evaluate` | Screenshot the tab, or run JavaScript in the page |
| `browser_add_asset` | Send an image or file from the coding agent to the shared browser's Assets tab (and the upload folder) |
| `browser_tabs` / `browser_say` | Manage tabs, or show a message to the people watching |
| `browser_console_messages` / `browser_network_requests` | Console output, exceptions and network requests (status, headers, response body). Action results also list new console errors and failed requests |
| `browser_downloads` / `browser_file_upload` | Wait for and read downloaded files; fill `<input type=file>` from the upload folder |
| `browser_cookies` / `browser_storage` / `browser_state` | Read and change cookies and localStorage/sessionStorage; save and restore a whole login session by name |
| `browser_snapshot_diff` | Only what changed on the page since the last snapshot or a named baseline |
| `browser_emulate` | Phone/tablet presets, screen size, pixel ratio, touch, user agent, locale, time zone, dark mode, GPS location, network speed |
| `browser_get_flow` | Read a flow a person recorded (by its id, e.g. `flow-3fa9c1`), or list all flows |
| `browser_run_flow` | Replay a recorded flow, optionally several times, with changed inputs (`inputs`) |

## Recorded flows

Record a task once, then let the agent repeat it or adapt it:

1. Press the red record button under the screen and use the browser as usual (clicks, typing, choices in lists, keys, links, tabs).
2. Press **Stop & save**. You get an id such as `flow-3fa9c1`.
3. Paste the id to your agent with what you want, e.g. "flow-3fa9c1, do it for these three names". Alone, the id replays the flow once.

The agent finds the flow with `browser_get_flow`, then replays it with `browser_run_flow` or follows its steps when the job needs changes. Actions the agent takes itself are not recorded.

- **Passwords are never stored.** A password step needs its value passed in `inputs` at replay time, or a saved login (`browser_state`).
- **Flows tab** (sidebar): list, search, rename, view and edit steps, remove steps, replay from the viewer, export and import as `.json`, delete.
- Flows are saved in the `flows/` folder inside the home folder (change it with `-flows <folder>`).
- Not recorded: hover-only menus, scrolling, and anything inside an iframe.

See [docs/GUIDE.md](GUIDE.md) for the full tool reference, options, the shared browser room, security and limitations.
