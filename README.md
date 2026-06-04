# dbgflow

[中文](README.zh-CN.md)

dbgflow is an early-stage Windows debugging automation MCP server and skills toolchain.

The current implementation includes the initial skeleton plus a Windows-only
DbgEng dump-analysis / process-debugging MVP, a stdio MCP server, a local
Streamable HTTP MCP endpoint, and Windows service scripts:

- backend abstraction
- mock backend
- session lifecycle management
- command policy
- artifact manager
- DbgEng backend for dump targets
- DbgEng backend for process attach and launch targets
- denylist-protected `execute` command support
- MCP-facing tool facade
- stdio JSON-RPC MCP entrypoint with tool schemas
- Streamable HTTP MCP endpoint at `/mcp` with resource update SSE
- native Windows service mode
- PowerShell install / uninstall service scripts

Initial tool names:

- `create_session`
- `get_session`
- `list_sessions`
- `close_session`
- `execute`
- `set_symbols`

`create_session` uses get-or-create semantics and returns quickly with a
`Starting` session while the backend opens the target in the background. Use
`get_session`, `list_sessions`, or the HTTP resource update stream to observe
the transition to `Ready`, `Break`, `Closed`, or `Error`.

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
`execute` no longer uses an allowlist, but dangerous commands such as shell
execution, script loading, extension loading, dump writing, and memory writing
remain denied by policy. Run-control commands update session state separately
from ordinary queries.

Run the MCP server over stdio:

```text
cargo run -p dbgflow-mcp
```

Run the MCP server over local Streamable HTTP:

```text
cargo run -p dbgflow-mcp -- http --bind 127.0.0.1:7331
```

To enable the installed-style data layout in HTTP mode, pass one data
directory; dbgflow will use `<data-dir>\artifacts` and `<data-dir>\logs`:

```text
cargo run -p dbgflow-mcp -- http --bind 127.0.0.1:7331 --data-dir C:\dbgflow\var
```

The HTTP endpoint is `http://127.0.0.1:7331/mcp`. `POST /mcp` returns JSON
responses. `GET /mcp` opens a server-sent event stream for MCP notifications,
including `notifications/resources/updated` for session state changes. `GET
/healthz` returns a simple health response.

The server supports `initialize`, `notifications/initialized`, `ping`,
`tools/list`, `tools/call`, `resources/list`, and `resources/read`. Tool
results are returned as JSON text content. Debugger command output is returned
in full and also written to session artifacts.

By default, the MCP server writes artifacts under the workspace-level
`artifacts/` directory. Set `DBGFLOW_ARTIFACT_ROOT` to override that location
when `--data-dir` is not used.

When `--data-dir` is used, runtime logs are written as daily JSONL files under
`<data-dir>\logs` and artifacts under `<data-dir>\artifacts`. Runtime logs are
retained for 7 days; artifacts are not automatically removed.

Install or uninstall the Windows service from an elevated PowerShell session:

```text
.\scripts\install-service.ps1
.\scripts\uninstall-service.ps1
```

The install script builds the release binary, replaces an existing
`dbgflow-mcp` service if present, copies the executable to
`%LOCALAPPDATA%\dbgflow\bin`, installs it as LocalSystem with
`--data-dir %LOCALAPPDATA%\dbgflow\var`, starts it, and checks `/healthz`.
Service artifacts and logs are written under `%LOCALAPPDATA%\dbgflow\var`;
uninstall does not delete artifacts or logs by default.

Live process DbgEng integration tests are ignored by default because attach and
launch behavior depends on local debugger permissions and target process state.
Run them explicitly when validating live process support.
