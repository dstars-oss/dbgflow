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
- install-time DbgEng symbol path configuration
- launch-only profiling with native ETW and optional Sysinternals Procmon collectors
- Time Travel Debugging recording with `TTD.exe` for launch, attach, and bounded monitor scenarios

Current tool names:

- `dbg.create_session`
- `dbg.get_session`
- `dbg.list_sessions`
- `dbg.close_session`
- `dbg.eval`
- `dbg.add_symbols`
- `trace.record_profile`
- `trace.record_ttd`

`dbg.create_session` uses get-or-create semantics and returns quickly with a
`Starting` session while the backend opens the target in the background. Use
`dbg.get_session`, `dbg.list_sessions`, or the HTTP resource update stream to observe
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
`dbg.eval` passes native debugger commands through to DbgEng except for empty
commands. Use it only in a trusted local environment. Run-control commands
are not detected from command text; session state is updated from backend
execution-status events and final backend status.
`dbg.add_symbols` appends native WinDbg symbol path strings, including symbol
server paths such as `srv*C:\symbols*https://msdl.microsoft.com/download/symbols`.
The runtime config can also provide an initial DbgEng symbol path. dbgflow
applies this through the DbgEng symbols API before opening the target; it is not
implemented by relying on worker environment variables.

`trace.record_profile` launches a local executable and records one or more profiling
collectors around the same target lifetime. The default collector is
`native_etw` with `scope.kind=target_process`, `event_sets=["process_lifecycle"]`,
and stack capture enabled. It writes a standard `.etl` trace plus filtered
`events.jsonl` and `summary.json` artifacts for the target PID. The tool also
accepts `collectors[]` for parallel collection. Collection stops when the
target exits or when `timeout_ms` expires. Timeout stops collection but does not
terminate the target process by default. Profile metadata, lifecycle events,
collector artifacts, and captured target stdout/stderr are written under
`artifacts\profiles\<profile_id>`.

Native ETW `process_lifecycle` captures process start/end, thread start/end, and
image load/unload kernel events. The raw `trace.etl` may still contain
system-wide lifecycle events; dbgflow's `events.jsonl` and `summary.json`
post-processing artifacts are filtered to the launched target PID. Stack frames
are emitted as a compact string array: `module+0xoffset` when the address can be
matched to the target PID's image load interval, otherwise the raw `0x...`
address is kept. Symbols are not resolved.

Example profile request:

```json
{
  "target": {
    "kind": "launch",
    "executable": "C:\\Windows\\System32\\cmd.exe",
    "args": ["/C", "echo dbgflow"]
  },
  "timeout_ms": 10000
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
      "scope": { "kind": "target_process" },
      "event_sets": ["process_lifecycle"],
      "stacks": { "enabled": true }
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
Configure `[tools].sysinternals_dir` in `config.toml`; dbgflow derives
`Procmon64.exe` or `Procmon.exe` from that directory. If it is not configured,
Sysinternals-dependent features return a clear error and the target is not
launched. `trace.record_profile` requests do not accept a Sysinternals path; this is
server runtime configuration. dbgflow does not download Procmon, does not scan
the whole machine, and does not accept a standalone Procmon executable path.
Procmon writes `capture.pml` as the authoritative artifact and exports
`events.csv` plus a best-effort target PID / operation / path filtered
`events.jsonl`; when stack capture is requested, dbgflow also requests an XML
export with stack data.

`trace.record_ttd` records Microsoft Time Travel Debugging traces by running
`TTD.exe` with typed launch, attach, or monitor targets. Configure
`[tools].ttd_dir` to override the recorder location; otherwise dbgflow derives
`TTD.exe` from `[debugger].dbgeng_dir\ttd` when available, then falls back to
`PATH`. dbgflow does not download or install TTD, does not accept arbitrary
recorder command lines, and always writes recorder output and generated `.run`
/ `.out` / `.err` / `.idx` files under
`artifacts\ttd_recordings\<recording_id>`. TTD recording usually
requires administrator privileges, can slow the target significantly, and can
produce large files. Treat TTD artifacts as sensitive because traces can contain
memory, file paths, registry data, and file contents.

Example TTD recording request:

```json
{
  "target": {
    "kind": "launch",
    "executable": "C:\\Windows\\System32\\cmd.exe",
    "args": ["/C", "echo dbgflow"]
  },
  "timeout_ms": 30000,
  "options": {
    "accept_eula": true,
    "max_file_mb": 2048
  }
}
```

`dbg.eval` is synchronous and does not expose per-command timeout knobs. While a
command is running, the session exposes `current_operation` plus a
`last_operation` summary with status, timing, artifact, error, and output-size
fields. If DbgEng reports the target running, the session state becomes
`Running` until the next debug event returns it to `Break` or the backend
reports no debuggee and the session becomes `Closed`. Clients can observe
progress with `dbg.get_session`, `resources/read`, or the HTTP resource update
stream. Legacy timeout fields are accepted for compatibility, ignored, and
logged as warnings. `dbg.close_session` requests backend cancellation before closing
a session that is currently executing a command; if the worker is stuck, the
main process can terminate that session's worker without taking down other
sessions or the MCP server.

Run the MCP server over local Streamable HTTP from the repository root:

```text
cargo run -p dbgflow-mcp -- http --config C:\Users\dstars\AppData\Local\dbgflow\config.toml
```

Runtime configuration is read from TOML:

```toml
version = 1

[service]
name = "dbgflow-mcp"
display_name = "dbgflow MCP Server"
install_root = "C:\\Users\\dstars\\AppData\\Local\\dbgflow"

[server]
bind = "127.0.0.1:7331"
data_dir = "C:\\Users\\dstars\\AppData\\Local\\dbgflow\\var"

[debugger]
dbgeng_dir = "C:\\Program Files\\WindowsApps\\Microsoft.WinDbg_...\\amd64"
symbol_path = "srv*C:\\symbols*https://msdl.microsoft.com/download/symbols"

[tools]
sysinternals_dir = "C:\\Users\\dstars\\Bin\\SysinternalsSuite"
# Optional when debugger.dbgeng_dir\ttd contains TTD.exe.
ttd_dir = "C:\\Users\\dstars\\Bin\\TTD"

[proxy]
mode = "url"
url = "http://127.0.0.1:7897"
```

The HTTP endpoint is `http://127.0.0.1:7331/mcp`. `POST /mcp` returns JSON
responses. `GET /mcp` opens a server-sent event stream for MCP notifications,
including `notifications/resources/updated` for session state changes. `GET
/healthz` returns a simple health response.

The HTTP transport is local-only: dbgflow only accepts loopback bind addresses
and rejects non-localhost `Origin` headers. `/mcp` does not require bearer token
authentication. HTTP request bodies are limited to 16 MiB.

Proxy configuration is service-wide and comes from `[proxy]` in `config.toml`.
Use `mode = "url"` with `url = "http://host:port"` to set
`_NT_SYMBOL_PROXY`, `HTTP_PROXY` / `HTTPS_PROXY`, and lowercase equivalents for
session workers and launched debuggees. Use `mode = "disabled"` to clear known
proxy variables, `mode = "env"` with `[proxy.env]` to persist specific proxy
environment variables, or `mode = "none"` to leave proxy unconfigured. In
`mode = "env"`, `_NT_SYMBOL_PROXY` is derived from `HTTPS_PROXY`, `HTTP_PROXY`,
or `ALL_PROXY` when it is not explicitly configured and the proxy value can be
converted to the SymSrv `host:port` format.

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

For troubleshooting, start with the daily runtime log to correlate HTTP/MCP
requests, tool calls, worker lifecycle, DbgEng operations, profile jobs, service
startup/shutdown, TTD recording jobs, durations, errors, and artifact paths.
Then inspect the session, profile, or TTD recording artifact directory referenced by the log. Runtime logs do not
include debugger command output or full HTTP request bodies; command output is
kept in the per-command output artifacts and session transcript.

Build and install or uninstall the Windows service from the repository scripts:

```text
.\scripts\install-service.ps1
.\scripts\uninstall-service.ps1
```

The install script builds `dbgflow-mcp` in release mode, detects local runtime
dependencies, writes `%LOCALAPPDATA%\dbgflow\config.toml`, shows a final
summary, then invokes `target\release\dbgflow-mcp.exe service install --config
<path>`. The install subcommand validates the config, requests UAC elevation
when needed, copies its current executable to `%LOCALAPPDATA%\dbgflow\bin`,
installs it as LocalSystem with `service run --config <path>`, starts the
service, and checks `/healthz`.

The install script detects DbgEng from Microsoft Store WinDbg packages first,
then Windows Kits / WDK Debuggers, then System32. Sysinternals is optional; if
no Sysinternals directory is configured, the service still installs and runs,
but Procmon-based profiling is unavailable. TTD is also optional; the script
first derives it from the resolved DbgEng directory as `<dbgeng_dir>\ttd`, then
falls back to standalone TTD discovery and `PATH`. Use `-TtdDir <path>` to
override that location. Use `-ProxyUrl <url>`, `-NoProxy`, or existing proxy environment
variables to control the generated `[proxy]` section. Use `-SymbolPath <path>`
to write `[debugger].symbol_path`; if omitted, the install script persists
`_NT_ALT_SYMBOL_PATH` and `_NT_SYMBOL_PATH` from the current environment when
present, and otherwise leaves the field unset. It does not default to
Microsoft's public symbol server. Use `-NonInteractive` to write the
detected/default config without the final confirmation prompt.

Uninstall queries the installed service command line from the Windows Service
Control Manager to recover the installed executable path and `--config` path,
then deletes the service and the entire configured install root, including
`bin`, `config.toml`, logs, and artifacts. If the service is already missing,
pass `-ConfigPath <path>` to `scripts\uninstall-service.ps1` as a fallback.

Live process DbgEng integration tests are ignored by default because attach and
launch behavior depends on local debugger permissions and target process state.
Run them explicitly when validating live process support. The live HTTP E2E
tests start `dbgflow-mcp http`, call `/mcp`, and cover the worker subprocess
path for attach and launch.
