# Model Context Protocol (MCP)

`pi-rs` includes built-in client support for Anthropic's **Model Context Protocol (MCP)**, allowing agents to discover and invoke tools provided by external processes.

---

## MCP Design in pi-rs: Lazy Stdio Client

Many agent runtimes eagerly spin up every declared MCP server at process launch, which degrades cold startup times.

`pi-rs` strictly maintains a **lazy stdio architecture**:
- MCP process handles are spawned only when an external tool is first referenced or explicitly queried.
- Startup latency remains under 1 ms even when complex servers (PostgreSQL, filesystem, browser servers) are configured.

---

## MCP Server Configuration

Configure MCP servers in your `config.json` under `mcp_servers`:

```json
{
  "mcp_servers": {
    "sqlite": {
      "command": "uvx",
      "args": ["mcp-server-sqlite", "--db-path", "app.db"]
    },
    "git": {
      "command": "mcp-server-git",
      "args": ["--repository", "."]
    }
  }
}
```

---

## Tool Discovery & Context Management

Tools exposed by MCP servers are automatically mapped into `pi-rs` tool definitions with distinct namespaces (e.g. `sqlite:query`).

`pi-rs` actively avoids dumping exhaustive tool schemas into the model prompt on turn 1. External tool definitions are exposed cleanly and efficiently to preserve model context headroom.
