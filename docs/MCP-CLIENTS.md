# Connecting other coding agents

Basics (HTTP vs stdio, password) are in [MCP.md](MCP.md). Below, each client has a plain setup and a **with password**
setup. Replace `your-password` with the value you passed to `-pw`. Examples assume the server is at `http://localhost:3000/mcp`.

Claude Code, Codex, OpenCode and Pi are in the [README](../README.md#connecting-a-coding-agent-mcp).

Config formats change between client versions, so check the client's own docs if something doesn't work.

## Cursor

`~/.cursor/mcp.json` (all projects) or `.cursor/mcp.json` (one project):

```json
{ "mcpServers": { "agent-browser": { "url": "http://localhost:3000/mcp" } } }
```

With password:

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

## VS Code (GitHub Copilot)

`.vscode/mcp.json` (VS Code uses `servers` and needs `type`):

```json
{ "servers": { "agent-browser": { "type": "http", "url": "http://localhost:3000/mcp" } } }
```

With password. VS Code can ask you for it instead of storing it in the file:

```json
{
  "inputs": [
    { "type": "promptString", "id": "abu-password", "description": "Agent Browser Use password", "password": true }
  ],
  "servers": {
    "agent-browser": {
      "type": "http",
      "url": "http://localhost:3000/mcp",
      "headers": { "Authorization": "Bearer ${input:abu-password}" }
    }
  }
}
```

## Windsurf

`~/.codeium/windsurf/mcp_config.json` (the URL field is `serverUrl`):

```json
{ "mcpServers": { "agent-browser": { "serverUrl": "http://localhost:3000/mcp" } } }
```

With password:

```json
{
  "mcpServers": {
    "agent-browser": {
      "serverUrl": "http://localhost:3000/mcp",
      "headers": { "Authorization": "Bearer your-password" }
    }
  }
}
```

## Gemini CLI

```sh
gemini mcp add --transport http agent-browser http://localhost:3000/mcp
```

With password:

```sh
gemini mcp add --transport http --header "Authorization: Bearer your-password" agent-browser http://localhost:3000/mcp
```

Or in `~/.gemini/settings.json` (the URL field is `httpUrl`):

```json
{
  "mcpServers": {
    "agent-browser": {
      "httpUrl": "http://localhost:3000/mcp",
      "headers": { "Authorization": "Bearer your-password" }
    }
  }
}
```

## Cline / Roo Code

Extension's **MCP Servers** panel → **Configure MCP Servers**:

```json
{ "mcpServers": { "agent-browser": { "type": "streamableHttp", "url": "http://localhost:3000/mcp" } } }
```

With password:

```json
{
  "mcpServers": {
    "agent-browser": {
      "type": "streamableHttp",
      "url": "http://localhost:3000/mcp",
      "headers": { "Authorization": "Bearer your-password" }
    }
  }
}
```

## Zed

`settings.json`:

```json
{ "context_servers": { "agent-browser": { "url": "http://localhost:3000/mcp" } } }
```

With password:

```json
{
  "context_servers": {
    "agent-browser": {
      "url": "http://localhost:3000/mcp",
      "headers": { "Authorization": "Bearer your-password" }
    }
  }
}
```

## Claude Desktop

Claude Desktop only launches local programs, so use stdio (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "agent-browser": { "command": "/path/to/agent-browser-use", "args": ["mcp"] }
  }
}
```

With password:

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

Leave `env` out and a password dialog opens when the server needs one.

## Any other client

- **HTTP:** add a streamable HTTP server with the URL, and a header `Authorization: Bearer your-password`.
- **stdio:** use the command `/path/to/agent-browser-use mcp` and set the environment variable `AGENT_BROWSER_USE_PASSWORD`.

The URL field is called `url`, `serverUrl` or `httpUrl` depending on the client.
