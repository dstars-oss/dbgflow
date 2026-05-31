# dbgflow

[中文](README.zh-CN.md)

dbgflow is an early-stage Windows debugging automation MCP server and skills toolchain.

The initial implementation focuses on the smallest useful skeleton:

- backend abstraction
- mock backend
- session lifecycle management
- command policy placeholder
- artifact manager placeholder
- MCP-facing tool facade

Initial tool names:

- `create_session`
- `list_sessions`
- `close_session`

`create_session` uses get-or-create semantics: if an active session already
exists for the same target, it returns that session's details; otherwise, it
creates a new session and returns the same detail shape.
