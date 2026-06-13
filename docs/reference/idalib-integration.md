# IDA / idalib Integration Reference

Date: 2026-06-13

This document records the pre-implementation research and final MVP decision
for adding IDA-backed static analysis to dbgflow.

## Outcome

dbgflow uses an IDA session model:

- `ida.create_session`
- `ida.get_session`
- `ida.list_sessions`
- `ida.close_session`
- `ida.get_metadata`
- `ida.list_segments`
- `ida.list_functions`
- `ida.list_strings`
- `ida.list_imports`
- `ida.list_exports`
- `ida.lookup_functions`
- `ida.disassemble`
- `ida.decompile`
- `ida.list_xrefs`
- `ida.list_basic_blocks`
- `ida.rename`
- `ida.set_comment`
- `ida.set_type`

The implementation is Rust-first and does not require the IDA SDK, Clang,
bindgen, `idalib-rs`, or an IDA installation to compile dbgflow. The IDA worker
loads `idalib.dll` and `ida.dll` dynamically at runtime. Base session,
metadata, segment, and function enumeration use direct dynamic binding. Rich
reverse-analysis tools also use Rust direct bindings to official IDA runtime
exports. Missing symbols, unavailable Hex-Rays licensing, or unsupported
processor/runtime capabilities return explicit per-tool errors while the
session remains usable for other queries.

## Current Local IDA Runtime

The local IDA installation used for research is:

```text
C:\Program Files\IDA Professional 9.3
```

The required runtime files for the MVP are:

```text
ida.exe
ida.dll
idalib.dll
ida.hlp
```

The MVP validates the required IDA runtime files when an install directory is
configured or when `ida.create_session` resolves the runtime. Rich API symbols
are probed from the loaded official DLLs after the base runtime is initialized.
Probe failures are reported through `ida.get_metadata().rich_api`.

## Runtime Configuration

The intended runtime configuration is:

```toml
[reverse.ida]
install_dir = "C:\\Program Files\\IDA Professional 9.3"
```

Resolution order:

1. `[reverse.ida].install_dir`
2. `DBGFLOW_IDA_DIR`
3. `C:\Program Files\IDA Professional 9.3`

The worker sets:

```text
IDADIR=<install_dir>
PATH=<install_dir>;<existing PATH>
```

## Rust Direct Rich Binding

The worker keeps all IDA C++ ABI handling private to `dbgflow-reverse::ida`.
The public MCP schema and manager protocol remain typed Rust structures; no
arbitrary IDA eval, IDAPython, or runtime script execution is exposed.

Direct rich binding currently targets IDA Professional 9.3 x64. At runtime the
worker probes symbols such as `get_ea_name`, `get_name_ea`,
`generate_disasm_line`, `tag_remove`, `next_head`, `get_strlit_contents`,
`enum_import_names`, `get_entry_name`, `xrefblk_t_first_from`,
`xrefblk_t_first_to`, `set_name`, `set_cmt`, and `apply_cdecl`. The qstring and
xref wrappers are private, non-`Send`/`Sync` values used only inside the worker
operation lock.

`ida.get_metadata` exposes a `rich_api` status object:

```json
{
  "available": true,
  "direct_bindings": true,
  "ida_version_gate": "IDA Professional 9.3 x64",
  "capabilities": {
    "names": false,
    "disassembly": false,
    "strings": false,
    "imports": false,
    "exports": false,
    "xrefs": false,
    "basic_blocks": true,
    "comments": true,
    "types": true,
    "decompiler": false
  },
  "missing_symbols": [],
  "hexrays": "not_loaded",
  "warnings": [
    "IDA direct qstring layout validation has not passed; qstring-dependent tools are disabled",
    "Hex-Rays direct decompiler dispatcher is unavailable in this build"
  ]
}
```

Missing direct symbols disable only the affected capabilities. The worker does
not put the session into `Error` for rich capability failures; the individual
tool returns a clear unsupported error.

## Why Dynamic Binding

The original research considered three routes:

- Python `idapro` worker.
- Native C++/SDK bridge DLL.
- Rust worker with direct runtime DLL binding.

The selected runtime route is the third route. The project intentionally avoids
an additional native bridge DLL deployment layer for the MVP, while keeping
the default Rust source build independent from the SDK and IDA import libraries.

Reasons:

- Source users without IDA, SDK, Clang, or bindgen can still compile dbgflow.
- The IDA dependency is only required on machines that actually call
  `ida.create_session`.
- The exposed capability surface remains typed and auditable.
- The IDA process boundary is a dedicated per-session worker subprocess, which
  protects the main MCP server from IDA load failures, crashes, or stalls.

The Python path remains useful for exploratory API behavior, but it is not the
production default for this MVP because it adds Python environment activation,
`idapro` package management, and import-path complexity.

## Official idalib Model

Hex-Rays describes idalib as using IDA's engine outside the GUI through C++ and
IDA Python APIs. IDA 9.x includes the idalib runtime and Python activation
payloads, but Python integration is not automatically ready in arbitrary Python
environments.

Python integration typically requires:

- IDA Pro 9.0 or newer installed, licensed, and initialized at least once.
- Installing the `idapro` Python package into the worker Python environment.
- Running `py-activate-idalib.py`, using HCLI `--set-default`, or setting
  `IDADIR`.
- Importing `idapro` before other IDA Python modules.

C++ integration points to `idalib.hpp` in the IDA SDK. That route is suitable
for richer native bindings, but it would make build-time SDK/toolchain handling
part of dbgflow's source build story unless isolated behind a separate optional
component.

## SDK and Export Findings

The local IDA 9.3 SDK is at:

```text
D:\Installer\ida93sp1\misc\ida-sdk
```

Relevant idalib declarations in `src\include\idalib.hpp`:

```cpp
int init_library(int argc = 0, char *argv[] = nullptr);
int open_database(const char *file_path, bool run_auto, const char *args = nullptr);
void close_database(bool save);
void enable_console_messages(bool enable);
bool get_library_version(int &major, int &minor, int &build);
```

Relevant IDA SDK declarations:

```cpp
int get_segm_qty(void);
segment_t *getnseg(int n);
size_t get_func_qty(void);
func_t *getn_func(size_t n);
```

The local `idalib.dll` exports the idalib entry points with plain names, and
the local `ida.dll` exports the segment/function query functions with plain
names. This matches the MVP's `libloading` lookups.

Layout assumptions checked against IDA 9.3 SDK:

- `ea_t = u64`
- `range_t` begins with `start_ea`, `end_ea`
- `segment_t : range_t` then `uval_t name`, `uval_t sclass`, `uval_t orgbase`,
  `uchar align`, `uchar comb`, `uchar perm`, `uchar bitness`
- `func_t : range_t` then `uint64 flags`

The direct binding layer contains private qstring/name/xref wrappers, but
qstring-dependent and xrefblk-dependent capabilities remain disabled until
real-runtime layout validation passes for the installed IDA 9.3 x64 runtime.

## ida-pro-mcp Research Summary

The project at:

```text
D:\Repos\OSS\ida-pro-mcp
```

was reviewed as a design reference.

Useful patterns:

- Explicit database/session concept: `idb_open`, `idb_list`, then tools take a
  database id.
- Headless idalib worker processes are preferred over GUI plugin mode.
- The supervisor keeps IDA initialization out of the public MCP process.
- Unsafe arbitrary Python eval/exec is separated and disabled by default.
- Read-only profiles are a good default capability model.
- Host/Origin checks and request body limits are present in the HTTP layer.

Patterns not adopted for dbgflow MVP:

- Arbitrary Python eval/exec.
- GUI IDA plugin installation.
- Unscoped IDB mutation tools without typed request/response and audit records.
- Deleting IDA sidecar files next to user inputs.
- Using Python package activation as the default production dependency path.

The key architectural lesson carried forward is the explicit session/database
id model. dbgflow's tool naming follows project style:

```text
ida.create_session
ida.get_session
ida.list_sessions
ida.close_session
ida.get_metadata
ida.list_segments
ida.list_functions
ida.list_strings
ida.list_imports
ida.list_exports
ida.lookup_functions
ida.disassemble
ida.decompile
ida.list_xrefs
ida.list_basic_blocks
ida.rename
ida.set_comment
ida.set_type
```

## MVP Behavior

`ida.create_session` accepts either:

```json
{
  "target": { "kind": "binary", "path": "C:\\samples\\a.exe" },
  "run_auto_analysis": true,
  "startup_timeout_ms": 60000
}
```

or:

```json
{
  "target": { "kind": "database", "path": "C:\\samples\\a.i64" },
  "run_auto_analysis": false,
  "startup_timeout_ms": 60000
}
```

Target validation:

- Path must be non-empty.
- Path must not contain NUL bytes.
- Path must canonicalize to an existing local file.
- Directories are rejected.
- `database` targets must end in `.idb` or `.i64`.

Session state:

```text
Starting
Ready
Closing
Closed
Error
```

`ida.create_session` uses get-or-create semantics for the same canonical
target while the existing session is reusable.

`ida.close_session` defaults to `save: true`. Existing `.idb` / `.i64` database
targets are operated on in place. Pass `save: false` only when session changes
should be discarded. The base `idalib` `close_database(bool save)` ABI returns
`void`, so dbgflow records save intent and an `unknown` save status when using
the direct binding path instead of claiming a successful save.

Paged tools apply a default limit of 100 and a maximum limit of 10000 before
calling the worker. `ida.list_segments` and `ida.list_functions` return
a page in the MCP response, while their artifacts include the complete filtered
JSON result.

Read-only result examples:

```json
[
  {
    "index": 0,
    "start_ea": "0x140001000",
    "end_ea": "0x140002000",
    "size": "0x1000",
    "name": ".text",
    "class": "CODE",
    "perm": "r-x",
    "bitness": 64
  }
]
```

```json
[
  {
    "index": 0,
    "start_ea": "0x140001100",
    "end_ea": "0x140001240",
    "size": "0x140",
    "name": "main",
    "segment": ".text",
    "prototype": "int main()",
    "flags": "0x1"
  }
]
```

Sanity checks:

- Segment count is capped.
- Function count is capped.
- `start_ea < end_ea`.
- `BADADDR` is rejected.
- Segment bitness must be `0..=2`.
- Null pointers from IDA are rejected.

## Artifact Layout

IDA reverse-analysis artifacts are written under:

```text
artifacts/
  reverse_sessions/
    <session_id>/
      events.jsonl
      worker.log
      request.json
      session.json
      outputs/
        segments-<timestamp>-<seq>.json
        functions-<timestamp>-<seq>.json
        metadata-<timestamp>-<seq>.json
        strings-<timestamp>-<seq>.json
        imports-<timestamp>-<seq>.json
        exports-<timestamp>-<seq>.json
        function_lookup-<timestamp>-<seq>.json
        disassembly-<timestamp>-<seq>.json
        decompile-<timestamp>-<seq>.json
        xrefs-<timestamp>-<seq>.json
        basic_blocks-<timestamp>-<seq>.json
        rename-<timestamp>-<seq>.json
        comments-<timestamp>-<seq>.json
        types-<timestamp>-<seq>.json
```

IDA outputs are sensitive and can include proprietary binary structure, symbols,
paths, and strings in future tools.

## Security Boundary

The MVP deliberately does not expose:

- Arbitrary eval.
- IDA Python.
- IDA debugger integration.
- Shell or external command execution.
- Byte or assembly patching.
- GUI adoption.

Mutation and richer analysis tools remain typed only and preserve session
scoping, artifact audit, output limits, and clear threat-model boundaries.

## Real IDA Smoke Tests

Default unit tests do not require IDA. Real IDA tests are ignored and gated:

```powershell
$env:DBGFLOW_REAL_IDA_TEST=1
$env:DBGFLOW_IDA_DIR="C:\Program Files\IDA Professional 9.3"
cargo test -p dbgflow-reverse real_ida_create_binary_session -- --ignored
cargo test -p dbgflow-reverse real_ida_list_segments_functions -- --ignored
```

Direct rich-tool real smoke is gated separately so base IDA validation remains
independent from qstring/xref/type wrapper validation:

```powershell
$env:DBGFLOW_REAL_IDA_DIRECT_TEST=1
$env:DBGFLOW_IDA_DIR="C:\Program Files\IDA Professional 9.3"
cargo test -p dbgflow-reverse real_ida_rich_tools_direct_bindings -- --ignored
```

Hex-Rays decompiler smoke should use a separate gate once the direct dispatcher
is validated for the installed runtime and license.

## Source Links

Official Hex-Rays references:

- idalib documentation: <https://docs.hex-rays.com/core/idalib>
- IDA 9.0 release notes: <https://docs.hex-rays.com/release-notes/9_0>
- IDA command-line switches: <https://docs.hex-rays.com/core/user-interface/concepts/command-line-switches>
- IDAPython docs: <https://docs.hex-rays.com/developer/idapython>
- IDA installation docs: <https://docs.hex-rays.com/getting-started/install-ida>
- `idapro` package: <https://pypi.org/project/idapro/>
- IDA Domain API: <https://github.com/HexRaysSA/ida-domain>
- IDA SDK repository: <https://github.com/HexRaysSA/ida-sdk>
- HCLI installing IDA: <https://hcli.docs.hex-rays.com/user-guide/installing-ida/>
- HCLI introduction: <https://hex-rays.com/blog/introducing-hcli>
- idalib use cases: <https://hex-rays.com/blog/4-powerful-applications-of-idalib-headless-ida-in-action>
- OEM licensing guidance: <https://hex-rays.com/blog/idalib-powers-products-with-oem-license>

Local references inspected:

- `C:\Program Files\IDA Professional 9.3\idalib\README.txt`
- `C:\Program Files\IDA Professional 9.3\idalib\python\py-activate-idalib.py`
- `C:\Program Files\IDA Professional 9.3\idalib\python\idapro\__init__.py`
- `C:\Program Files\IDA Professional 9.3\idalib\python\idapro\config.py`
- `C:\Program Files\IDA Professional 9.3\idalib\examples\idacli.py`
- `D:\Installer\ida93sp1\misc\ida-sdk\src\include\idalib.hpp`
- `D:\Installer\ida93sp1\misc\ida-sdk\src\include\segment.hpp`
- `D:\Installer\ida93sp1\misc\ida-sdk\src\include\funcs.hpp`

Local `ida-pro-mcp` references:

- `D:\Repos\OSS\ida-pro-mcp\README.md`
- `D:\Repos\OSS\ida-pro-mcp\pyproject.toml`
- `D:\Repos\OSS\ida-pro-mcp\src\ida_pro_mcp\idalib_supervisor.py`
- `D:\Repos\OSS\ida-pro-mcp\src\ida_pro_mcp\idalib_server.py`
- `D:\Repos\OSS\ida-pro-mcp\src\ida_pro_mcp\idalib_session_manager.py`
- `D:\Repos\OSS\ida-pro-mcp\src\ida_pro_mcp\ida_mcp\api_python.py`
