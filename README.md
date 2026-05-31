# dbgflow

[中文](README.zh-CN.md)

dbgflow is an early-stage Windows debugging automation MCP server and skills toolchain.

The current implementation includes the initial skeleton plus a Windows-only
DbgEng dump-analysis MVP:

- backend abstraction
- mock backend
- session lifecycle management
- command policy
- artifact manager
- DbgEng backend for dump targets
- allowlisted `execute` command support
- MCP-facing tool facade

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
