# dbgflow

[中文](README.zh-CN.md)

dbgflow is an early-stage Windows debugging automation MCP server and skills toolchain.

The current implementation includes the initial skeleton plus a Windows-only
DbgEng dump-analysis / process-debugging MVP and a minimal stdio MCP server:

- backend abstraction
- mock backend
- session lifecycle management
- command policy
- artifact manager
- DbgEng backend for dump targets
- DbgEng backend for process attach and launch targets
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

On Windows, DbgEng sessions use `dbgeng.dll` resolved in this order: WinDbg /
WinDbg Preview app package, Windows SDK Debuggers, then System32 fallback.
DbgEng targets currently support dump files, process attach by PID, and process
launch by executable path plus argument list.

Example targets:

```json
{ "kind": "dump", "path": "C:\\crash.dmp" }
```

```json
{ "kind": "attach", "pid": 1234 }
```

```json
{ "kind": "launch", "executable": "C:\\app\\app.exe", "args": ["--flag"] }
```

Dump targets may point to any existing local dump file with a supported dump
extension. Launch targets are disabled by default; set `DBGFLOW_ENABLE_LAUNCH=1`
only in a trusted local environment to allow controlled process launch. Launch
uses a suspended Win32 process creation path and attaches DbgEng before resuming
the target. The executable must be an existing path; shell invocation, custom
current directories, and custom environments are not part of this MVP.
Command output and logs are still written under the controlled artifact root.
The only run-control command currently allowlisted through `execute` is exact
`g`; other step/breakpoint commands remain denied.

Run the MCP server over stdio:

```text
cargo run -p dbgflow-mcp
```

The server supports `initialize`, `notifications/initialized`, `ping`,
`tools/list`, and `tools/call`. Tool results are returned as JSON text content;
raw debugger command output is still written to session artifacts.

By default, the MCP server writes artifacts under the workspace-level
`artifacts/` directory. Set `DBGFLOW_ARTIFACT_ROOT` to override that location.

Live process DbgEng integration tests are ignored by default because attach and
launch behavior depends on local debugger permissions and target process state.
Run them explicitly when validating live process support.
