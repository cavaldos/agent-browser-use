# Connecting an agent over MCP

Agent Browser Use speaks [MCP](https://modelcontextprotocol.io). Any client that supports MCP can use it in one of two ways.

## 1. HTTP (you start the server)

Start the server, then point the client at its `/mcp` address:

```sh
./agent-browser-use            # viewer: http://localhost:3000, MCP: http://localhost:3000/mcp
```

A typical client config looks like this (the exact field names differ a little per client):

```json
{
  "mcpServers": {
    "agent-browser": { "url": "http://localhost:3000/mcp" }
  }
}
```

## 2. stdio (the client starts the server)

The client launches the program itself, so you don't have to start anything:

```json
{
  "mcpServers": {
    "agent-browser": { "command": "/path/to/agent-browser-use", "args": ["mcp"] }
  }
}
```

If a server is already running on the port, `mcp` forwards to it, so every agent shares one browser.

## Password

Start the server with a password to protect the viewer and `/mcp`:

```sh
./agent-browser-use -pw your-password
```

Then give the same password to the client:

| Connection | Where the password goes |
|---|---|
| HTTP | the client's `headers`: `"Authorization": "Bearer your-password"` |
| stdio | `"env": { "AGENT_BROWSER_USE_PASSWORD": "your-password" }` (or `"args": ["mcp", "-pw", "your-password"]`) |

```json
{
  "mcpServers": {
    "agent-browser": {
      "url": "http://localhost:3000/mcp",
      "headers": { "Authorization": "Bearer your-password" }
    }
  }
}
```

```json
{
  "mcpServers": {
    "agent-browser": {
      "command": "/path/to/agent-browser-use",
      "args": ["mcp"],
      "env": { "AGENT_BROWSER_USE_PASSWORD": "your-password" }
    }
  }
}
```

**No password in the config?** In stdio mode, if the running server needs a password, `mcp` pops up a password dialog
(macOS, Windows, Linux with `zenity`; otherwise it asks in the terminal) and you type it yourself, up to 3 tries.
Nothing is saved.

## Per-client instructions

- Claude Code, Codex, OpenCode, Pi: see the [README](../README.md#connecting-a-coding-agent-mcp).
- Cursor, VS Code, Windsurf, Gemini CLI, Cline, Zed, Claude Desktop and others: see [MCP-CLIENTS.md](MCP-CLIENTS.md).
