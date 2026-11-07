# Agent Browser Use

Let AI agents (Claude Code, Cursor, VS Code…) drive a real browser over **MCP**, while you watch every step live in a web page and can take over at any time.

![Demo](docs/images/demo.png)

## Quick start

Requires Go 1.22+ and Chrome (or just Docker).

```sh
make run            # run from source  -> http://localhost:3000
make build          # build bin/agent-browser-use
make docker         # build the Docker image and run it
make docker-logs    # follow container logs
make docker-stop    # stop the container
```

Options: `PORT=8080`, `HOST=0.0.0.0`, `ARGS="-agent-speed 1"`. The scripts live in `scripts/` and can be run directly.

## Connect an agent

```sh
# server already running (HTTP)
claude mcp add --transport http agent-browser http://localhost:3000/mcp

# or let the agent start it (stdio)
claude mcp add agent-browser -- /path/to/bin/agent-browser-use mcp
```

## Project layout

```
app/        Go source + web UI (static/)
docker/     Dockerfile, entrypoint
scripts/    run / build / docker scripts
docs/       detailed documentation
```

## Documentation

See [docs/GUIDE.md](docs/GUIDE.md) for MCP tools, options, the shared browser room, security and limitations.
