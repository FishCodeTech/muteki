# Muteki Control Agent Plugin

This directory is an Agent Plugins 1.0.0 package. Compatible clients discover
its portable manifest at `plugin.json`, the MCP server at `mcp.json`, and the
Agent Skill under `skills/muteki-control/`.

The package contains no credentials. The stdio MCP bridge reads its connection
from `${PLUGIN_DATA}/connection.json`:

```json
{
  "endpoint": "http://127.0.0.1:8000/api/capability",
  "bearer_token": "<session-scoped Muteki capability token>"
}
```

The hosting Agent Plugins client creates this client-managed file when a
capability grant is materialized. Muteki adapters can use
`muteki.capability_bindings.agent_plugin.write_connection_config`; other
clients use their own credential setup flow. Agent Plugins 1.0.0 intentionally
leaves authentication and secret storage to clients.

If a client supports Agent Skills but not MCP, the bundled skill provides a
stdlib-only command bridge that reads `MUTEKI_CAPABILITY_ENDPOINT` and
`MUTEKI_CAPABILITY_TOKEN` from the session environment.
