# Model Context Protocol (MCP)

`pi-rs` includes a built-in client for the **Model Context Protocol (MCP)**, allowing agents to discover and invoke tools provided by external processes. Stdio and bounded Streamable HTTP POST transports are supported; long-lived server push remains outside this release.

---

## MCP Design in pi-rs: Lazy Stdio Client

Many agent runtimes eagerly spin up every declared MCP server at process launch, which degrades cold startup times.

`pi-rs` keeps MCP activation lazy:
- Stdio child processes and HTTP connections are created only when a server is explicitly enabled.
- Constructing configuration does not connect to every server or delay the basic startup path.
- Activation discovers that server's tools, which can then participate in the normal tool lifecycle and trace.

---

## MCP Server Configuration

Configure MCP servers in your `config.json` as an array under `mcp_servers`:

```json
{
  "mcp_servers": [
    {
      "name": "sqlite",
      "command": "uvx",
      "args": ["mcp-server-sqlite", "--db-path", "app.db"],
      "enabled": false,
      "read_only_tools": ["query"]
    },
    {
      "name": "git",
      "command": "mcp-server-git",
      "args": ["--repository", "."],
      "enabled": false
    }
  ]
}
```

A network server uses `url` instead of `command`, with optional `headers`. Header values
are redacted when configuration is serialized.

---

## Tool Discovery & Context Management

Tools exposed by MCP servers are automatically mapped into `pi-rs` tool definitions with distinct namespaces (e.g. `sqlite:query`).

Configured servers are not activated merely because they appear in the file. In an
interactive session, inspect and activate one explicitly:

```text
/mcp
/mcp enable sqlite
/mcp disable sqlite
```

Activation runs `initialize` and `tools/list`; the resulting tools are namespaced and
participate in the same lifecycle, approval, output-reduction, and trace rules as native
tools. This avoids dumping every configured catalog into the model context.
