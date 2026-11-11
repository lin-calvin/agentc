# agentc

npm wrapper for [agentc](https://github.com/lin-calvin/agentc) — a headless Wayland compositor
with an MCP server, built for agentic computer use.

```bash
npx agentc daemon          # start the compositor + MCP + panel
npx agentc status
npx agentc stop
```

Point an MCP client at the relay (it starts the daemon on demand):

```jsonc
{ "mcp": { "agentc": { "type": "local", "command": ["npx", "-y", "agentc", "mcp"] } } }
```

The actual binary lives in a per-platform package (`agentc-linux-x64`, `agentc-linux-arm64`),
so only your platform is downloaded.

**Requirements:** Linux with glibc and `libxkbcommon` (present on any Wayland desktop), and
`/dev/…` access for the applications you want to drive. musl/Alpine is not supported yet.

See the main repository for tools, architecture, limits and measurements.
