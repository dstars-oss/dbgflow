# dbgflow

[中文](README.zh-CN.md)

dbgflow is an early-stage Windows debugging automation MCP server and skills toolchain.

The current implementation includes the initial skeleton plus a Windows-only
DbgEng dump-analysis / process-debugging MVP, a local Streamable HTTP MCP
endpoint, and Windows service install / uninstall subcommands:

- backend abstraction
- session lifecycle management
- per-session debug worker subprocess isolation
- artifact manager
- per-session transcript, event, command, and output artifacts
- DbgEng backend for dump targets
- DbgEng backend for process attach and launch targets
- native WinDbg / DbgEng command passthrough with audited `eval` output
- MCP-facing tool facade
- Streamable HTTP MCP endpoint at `/mcp` with resource update SSE
- native Windows service mode
- native Windows service install / uninstall subcommands

Initial tool names:

- `create_session`
- `get_session`
- `list_sessions`
- `close_session`
- `eval`
- `set_symbols`

`create_session` uses get-or-create semantics and returns quickly with a
`Starting` session while the backend opens the target in the background. Use
`get_session`, `list_sessions`, or the HTTP resource update stream to observe
the transition to `Ready`, `Break`, `Closed`, or `Error`.
`target` is required; dbgflow no longer exposes a mock target in the MCP tool
schema.

On Windows, DbgEng sessions use `dbgeng.dll` resolved in this order: WinDbg /
WinDbg Preview app package, Windows SDK Debuggers, then System32 fallback.
DbgEng targets currently support dump files, process attach by PID, and process
launch by executable path plus argument list. Each real debug session runs in
its own worker subprocess; the main MCP process handles message dispatch,
session state, target validation, artifacts, logs, and worker lifecycle control.

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

Dump targets may point to any existing local file; DbgEng reports an error if
the file is not a supported dump. Launch uses a suspended Win32 process creation
path and attaches DbgEng before resuming the target. The executable must be an
existing path; shell invocation, custom current directories, and custom
environments are not part of this MVP.
Command output, transcripts, command records, event records, and logs are still
written under controlled runtime directories.
`eval` passes native debugger commands through to DbgEng except for empty
commands. Use it only in a trusted local environment. Run-control commands
are not detected from command text; session state is updated from backend
execution-status events and final backend status.
`set_symbols` accepts native WinDbg symbol path strings, including symbol server
paths such as `srv*C:\symbols*https://msdl.microsoft.com/download/symbols`.

`eval` is synchronous and does not expose per-command timeout knobs. While a
command is running, the session exposes `current_operation` plus a
`last_operation` summary with status, timing, artifact, error, and output-size
fields. If DbgEng reports the target running, the session state becomes
`Running` until the next debug event returns it to `Break` or the backend
reports no debuggee and the session becomes `Closed`. Clients can observe
progress with `get_session`, `resources/read`, or the HTTP resource update
stream. Legacy timeout fields are accepted for compatibility, ignored, and
logged as warnings. `close_session` requests backend cancellation before closing
a session that is currently executing a command; if the worker is stuck, the
main process can terminate that session's worker without taking down other
sessions or the MCP server.

Run the MCP server over local Streamable HTTP from the repository root:

```text
cargo run -p dbgflow-mcp -- http --bind 127.0.0.1:7331 --data-dir .\var
```

The HTTP endpoint is `http://127.0.0.1:7331/mcp`. `POST /mcp` returns JSON
responses. `GET /mcp` opens a server-sent event stream for MCP notifications,
including `notifications/resources/updated` for session state changes. `GET
/healthz` returns a simple health response.

The HTTP transport is local-only: dbgflow only accepts loopback bind addresses
and rejects non-localhost `Origin` headers. `/mcp` does not require bearer token
authentication. HTTP request bodies are limited to 16 MiB.

The server supports `initialize`, `notifications/initialized`, `ping`,
`tools/list`, `tools/call`, `resources/list`, and `resources/read`. Tool
results are returned as JSON text content. Debugger command output is returned
in full and also written to session artifacts; the latest command artifact is
also referenced from the session's `last_operation`.

Runtime logs are written as daily JSONL files under `<data-dir>\logs`, and
artifacts under `<data-dir>\artifacts`. Each session writes
`sessions\<session_id>\transcript.log`, `events.jsonl`, `commands.jsonl`, and
`outputs\<command_id>.txt`. Runtime logs are retained for 7 days; artifacts are
not automatically removed.

Build and install or uninstall the Windows service from the repository scripts:

```text
.\scripts\install-service.ps1
.\scripts\uninstall-service.ps1
```

The install script builds `dbgflow-mcp` in release mode and then invokes the
built `target\release\dbgflow-mcp.exe service install` with the selected
service parameters. The install subcommand copies its current executable to
`%LOCALAPPDATA%\dbgflow\bin`, installs it as LocalSystem with `service run
--data-dir %LOCALAPPDATA%\dbgflow\var`, starts it, and checks `/healthz`.
Service artifacts and logs are written under `%LOCALAPPDATA%\dbgflow\var`.
Uninstall does not delete artifacts or logs by default.

Live process DbgEng integration tests are ignored by default because attach and
launch behavior depends on local debugger permissions and target process state.
Run them explicitly when validating live process support. The live HTTP E2E
tests start `dbgflow-mcp http`, call `/mcp`, and cover the worker subprocess
path for attach and launch.
