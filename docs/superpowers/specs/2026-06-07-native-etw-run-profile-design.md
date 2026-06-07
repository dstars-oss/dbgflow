# Native ETW `run_profile` V1 Design

## Summary

Add a first profiling capability to dbgflow through a single MCP tool named
`run_profile`. V1 runs a target process, records a native ETW trace directly
from code, writes a standard `.etl` artifact, and returns profile metadata.

The first version is intentionally artifact-first. It does not call `wpr.exe`,
does not use Procmon, does not inject hooks, does not support attach, and does
not parse the ETL into a full report. The ETL file is the canonical capture
output so existing Windows tools such as WPA, xperf, TraceProcessor, and related
ETW tooling can consume it.

## Goals

- Provide one reliable profile job tool that starts and stops collection within
  a single MCP call.
- Generate a standard `.etl` trace as the primary output.
- Launch a target process and stop collection when the target exits or when the
  requested timeout is reached.
- Keep all profile outputs under controlled dbgflow artifacts.
- Keep MCP handlers thin and place profiling logic in `dbgflow-core`.
- Preserve room for future debugger-gated profiling and richer collectors.

## Non-Goals

- No WPR command-line orchestration.
- No Procmon collection in V1.
- No API hooking or injection in V1.
- No attach-to-existing-process support in V1.
- No custom provider configuration in V1.
- No complete ETL parsing or automated performance report in V1.
- No automatic privilege elevation.

## Tool API

Expose one primary MCP tool:

```text
run_profile
```

Example input:

```json
{
  "target": {
    "kind": "launch",
    "executable": "C:\\app\\app.exe",
    "args": ["--case", "1"]
  },
  "timeout_ms": 300000,
  "collector": {
    "kind": "native_etw",
    "preset": "system_overview"
  }
}
```

V1 accepts only `target.kind = "launch"`, `collector.kind = "native_etw"`, and
`collector.preset = "system_overview"`.

`timeout_ms` bounds the collection window. The profile stops when the target
process exits or when `timeout_ms` expires, whichever happens first. On timeout,
V1 stops ETW collection but does not terminate the target process by default.

Example output shape:

```json
{
  "profile_id": "...",
  "status": "completed",
  "completion_reason": "target_exited",
  "target_pid": 1234,
  "target_exit_code": 0,
  "duration_ms": 120000,
  "artifacts": {
    "trace": "artifacts/profiles/.../trace.etl",
    "profile": "artifacts/profiles/.../profile.json",
    "events": "artifacts/profiles/.../events.jsonl",
    "stdout": "artifacts/profiles/.../target/stdout.txt",
    "stderr": "artifacts/profiles/.../target/stderr.txt"
  },
  "warnings": []
}
```

V1 result statuses:

```text
completed
timed_out
failed
```

Failures before a profile artifact directory is created may return a normal tool
execution error. Failures after collection has started should return a
`failed` profile result with artifact references whenever possible.

## Lifecycle

`run_profile` is a single-call lifecycle:

```text
create profile artifact directory
start native ETW session -> trace.etl
launch target process
wait for target exit or timeout_ms
stop native ETW session
write profile.json and events.jsonl
return artifact references, final status, and warnings
```

Normal users and agents should not need separate `start_profile` and
`stop_profile` calls. A future management tool may stop a stuck or orphaned
profile job, but it is not part of the normal V1 workflow.

## Architecture

Add profiling as a core subsystem, separate from `DebugBackend`:

```text
dbgflow-mcp
  -> run_profile tool facade

dbgflow-core
  -> profile::manager
  -> profile::worker
  -> profile::target
  -> profile::state
  -> profile::collector::ProfileCollector
  -> profile::collector::native_etw::NativeEtwCollector
  -> artifacts
```

Profiling is not a debugger backend. It uses the same project principles:
validated targets, controlled artifacts, explicit state, worker isolation,
auditable events, and thin MCP handlers.

The collector boundary should remain explicit even though V1 has only one
collector. A future collector can reuse the same orchestrator lifecycle:

```text
ProfileCollector::start(output_dir)
ProfileCollector::stop()
ProfileCollector::cleanup()
```

## Native ETW Collector

`NativeEtwCollector` controls ETW directly from code. It should use Windows ETW
controller APIs such as `StartTrace`, `EnableTraceEx2`, and `ControlTrace` to
write `trace.etl`.

V1 should fail early if required ETW privileges are missing. dbgflow normally
runs as a Windows service, so the tool should return a clear error and record
available ETW failure details rather than attempting automatic elevation.

The collector must use a unique session name per profile job and must attempt
cleanup on all failure paths after the ETW session has started.

## `system_overview` Preset

V1 supports one built-in preset:

```text
system_overview
```

The preset should prioritize broad system observability for a launched target:

```text
process and thread lifecycle
image load
CPU sampling
context switch
disk and file IO
registry activity
exceptions
stackwalk where supported
```

Stack collection is important but should be treated by capability:

```text
CPU sampling stack        preferred
context switch stack      preferred
file / registry stack     best effort
exception stack           best effort
heap stack                future work
```

The main V1 guarantee is that a standard ETL trace is produced when the ETW
session starts and stops successfully. Missing stacks for some event classes,
provider enable failures, dropped events, or unsupported OS behavior should be
reported as warnings unless they prevent the ETW session itself from running.

## Artifacts

Each profile job gets an independent artifact directory:

```text
artifacts/
  profiles/
    <profile_id>/
      profile.json
      events.jsonl
      trace.etl
      target/
        stdout.txt
        stderr.txt
```

`profile.json` records:

```text
profile_id
target executable and args
target_pid
start_time, end_time, duration_ms
timeout_ms
completion_reason
target_exit_code
collector kind and preset
trace artifact path
warnings
```

`events.jsonl` records lifecycle events:

```text
profile_created
collector_starting
collector_started
target_launching
target_started
target_exited
timeout_reached
collector_stopping
collector_stopped
profile_completed
profile_error
```

The ETL file is sensitive debug artifact data and must remain under the
controlled runtime data directory.

## Error Handling

- ETW start failure: return an error and do not launch the target.
- Target launch failure after ETW start: stop ETW, keep any generated trace, and
  return a `failed` profile result with artifact references.
- Timeout: stop ETW, do not terminate the target by default, and return a
  `timed_out` result.
- ETW stop failure: return an error, record the ETW session name, and include
  cleanup guidance in warnings when possible.
- Permission failure: return an error without attempting elevation.
- Partial provider or stackwalk failure: record warnings if the ETW session can
  still produce a trace.

All paths after successful ETW start must try to stop collection. This is the
reason V1 uses one `run_profile` call instead of a split start/stop workflow.

## Concurrency

V1 should allow at most one native ETW profile job at a time. ETW kernel/system
collection and provider state can interfere across simultaneous jobs. The
manager should reject a second profile job with a clear error while one is
active.

Future versions may relax this after the collector distinguishes kernel logger
state, user provider sessions, and independent collection windows.

## Testing

Unit and integration coverage should include:

- MCP schema accepts only launch target and `native_etw/system_overview`.
- Target path validation rejects missing or invalid executables.
- ETW start failure does not launch the target.
- Target launch failure stops an already-started collector.
- Target exit stops collection and writes expected artifacts.
- Timeout stops collection and reports timeout without killing the target.
- Concurrent `run_profile` calls are rejected.
- Artifact records include profile metadata, lifecycle events, and trace path.

Real ETW integration tests should be Windows-only and ignored by default if they
depend on local privileges or service context. Non-Windows builds should compile
with a clear unsupported error path for `run_profile`.

## Future Work

- Add a management-only stop tool for orphaned or stuck profile jobs.
- Add attach profiling after launch-only behavior is stable.
- Add debugger-gated profiling for precise collection windows.
- Add optional custom provider configuration.
- Add ETL post-processing with TraceProcessor or TDH.
- Add Procmon or high-level API correlation as separate collectors or analyzers.
- Add hook-based instrumentation only for targeted APIs and explicit scenarios.
