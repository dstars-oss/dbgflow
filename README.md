# dbgflow

[中文](README.zh-CN.md)

dbgflow is an early-stage Windows debugging automation MCP server and skills toolchain.

The current implementation includes the initial skeleton plus a Windows-only
DbgEng dump-analysis MVP and a minimal stdio MCP server:

- backend abstraction
- mock backend
- session lifecycle management
- command policy
- artifact manager
- DbgEng backend for dump targets
- allowlisted `execute` command support
- MCP-facing tool facade
- stdio JSON-RPC MCP entrypoint with tool schemas

Initial tool names:

- `create_session`
- `list_sessions`
- `close_session`
- `execute`

`create_session` uses get-or-create semantics: if an active session already
exists for the same target, it returns that session's details; otherwise, it
creates a new session and returns the same detail shape.

On Windows, dump sessions use `dbgeng.dll` resolved in this order: WinDbg /
WinDbg Preview app package, Windows SDK Debuggers, then System32 fallback.
Dump targets may point to any existing local dump file with a supported dump
extension; command output and logs are still written under the controlled
artifact root.

Run the MCP server over stdio:

```text
cargo run -p dbgflow-mcp
```

The server supports `initialize`, `notifications/initialized`, `ping`,
`tools/list`, and `tools/call`. Tool results are returned as JSON text content;
raw debugger command output is still written to session artifacts.

By default, the MCP server writes artifacts under the workspace-level
`artifacts/` directory. Set `DBGFLOW_ARTIFACT_ROOT` to override that location.
