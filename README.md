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
- main-service proxy configuration for session workers and SymSrv symbol downloads
- launch-only profiling with native ETW and optional Sysinternals Procmon collectors

Initial tool names:

- `create_session`
- `get_session`
- `list_sessions`
- `close_session`
- `eval`
- `set_symbols`
- `run_profile`

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

`run_profile` launches a local executable and records one or more profiling
collectors around the same target lifetime. The default collector is
`native_etw/system_overview`, which writes a standard `.etl` trace. The tool
also accepts `collectors[]` for parallel collection; the legacy single
`collector` field remains accepted for compatibility. Collection stops when the
target exits or when `timeout_ms` expires. Timeout stops collection but does not
terminate the target process by default. Profile metadata, lifecycle events,
collector artifacts, and captured target stdout/stderr are written under
`artifacts\profiles\<profile_id>`.

Example profile request:

```json
{
  "target": {
    "kind": "launch",
    "executable": "C:\\Windows\\System32\\cmd.exe",
    "args": ["/C", "echo dbgflow"]
  },
  "timeout_ms": 10000,
  "collector": {
    "kind": "native_etw",
    "preset": "system_overview"
  }
}
```

Parallel collectors example:

```json
{
  "target": {
    "kind": "launch",
    "executable": "C:\\app\\read-file.exe",
    "args": ["C:\\data\\large_input.bin"]
  },
  "timeout_ms": 10000,
  "collectors": [
    {
      "kind": "native_etw",
      "preset": "system_overview"
    },
    {
      "kind": "procmon",
      "capture_stacks": true,
      "filters": {
        "operations": ["CreateFile", "ReadFile", "WriteFile"],
        "paths": ["C:\\data\\large_input.bin"]
      }
    }
  ]
}
```

The `procmon` collector is optional and depends on Sysinternals Process Monitor.
Configure the service or HTTP runtime with `--sysinternals-dir <path>`; dbgflow
derives `Procmon64.exe` or `Procmon.exe` from that directory. If the option is
not configured, Sysinternals-dependent features return a clear error and the
target is not launched. `run_profile` requests do not accept a Sysinternals path;
this is server runtime configuration. dbgflow does not download Procmon, does
not scan the whole machine, and does not accept a standalone Procmon executable path.
Procmon writes `capture.pml` as the authoritative artifact and exports
`events.csv` plus a best-effort target PID / operation / path filtered
`events.jsonl`; when stack capture is requested, dbgflow also requests an XML
export with stack data.

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
cargo run -p dbgflow-mcp -- http --bind 127.0.0.1:7331 --data-dir .\var --proxy-url http://127.0.0.1:7897
```

The HTTP endpoint is `http://127.0.0.1:7331/mcp`. `POST /mcp` returns JSON
responses. `GET /mcp` opens a server-sent event stream for MCP notifications,
including `notifications/resources/updated` for session state changes. `GET
/healthz` returns a simple health response.

The HTTP transport is local-only: dbgflow only accepts loopback bind addresses
and rejects non-localhost `Origin` headers. `/mcp` does not require bearer token
authentication. HTTP request bodies are limited to 16 MiB.

Proxy configuration is service-wide. Pass `--proxy-url http://127.0.0.1:7897`
to set `_NT_SYMBOL_PROXY=127.0.0.1:7897` for DbgEng/SymSrv symbol downloads
and `HTTP_PROXY` / `HTTPS_PROXY` plus lowercase equivalents for session workers
and launched debuggees. Pass `--no-proxy` to clear known proxy variables for
session workers. If neither option is passed, dbgflow reads `_NT_SYMBOL_PROXY`,
`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`, and lowercase equivalents
from its process environment.

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

The install script only builds `dbgflow-mcp` in release mode and invokes the
built `target\release\dbgflow-mcp.exe service install`. The install subcommand
owns the rest of the flow: it prompts for service settings, detects local
DbgEng in WinDbg Store packages first, Windows Kits / WDK Debuggers second, and
the Windows directory last, detects Sysinternals from environment variables,
`PATH`, and common Sysinternals directories, requests UAC elevation when needed,
copies its current executable to
`%LOCALAPPDATA%\dbgflow\bin`, installs it as LocalSystem with `service run
--data-dir %LOCALAPPDATA%\dbgflow\var`, writes the selected service
environment, starts the service, and checks `/healthz`. Service artifacts and
logs are written under `%LOCALAPPDATA%\dbgflow\var`. Uninstall does not delete
artifacts or logs by default.

During installation, accept the detected DbgEng directory or enter another
directory containing `dbgeng.dll`. The selected path is written as
`DBGFLOW_DBGENG_DIR` in the service environment and is preferred by the DbgEng
resolver. Sysinternals is optional; if no Sysinternals directory is configured,
the service still installs and runs, but Procmon-based profiling is
unavailable.

The interactive installer uses `-ProxyUrl <url>` or existing proxy environment
variables as the displayed proxy default. If neither is present, proxy is left
unconfigured by default. Use `-NoProxy`, or type `none` at the prompt, to clear
known service proxy keys. Use `-NonInteractive` to use provided/default CLI
values without prompts.

Live process DbgEng integration tests are ignored by default because attach and
launch behavior depends on local debugger permissions and target process state.
Run them explicitly when validating live process support. The live HTTP E2E
tests start `dbgflow-mcp http`, call `/mcp`, and cover the worker subprocess
path for attach and launch.
