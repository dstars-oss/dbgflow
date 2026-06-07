# DbgEng Symbol Path Reference

This note records how symbol paths are configured for DbgEng-backed sessions.
It is a reference for implementation decisions, not user-facing product
documentation.

## Primary Conclusion

DbgEng has explicit symbol path APIs. `_NT_SYMBOL_PATH` and
`_NT_ALT_SYMBOL_PATH` are useful startup defaults, but they should not be the
only configuration mechanism for dbgflow sessions.

Use environment variables only as inherited initial state for a worker process.
For a live session-level configuration path, prefer the DbgEng symbol APIs:

- `IDebugSymbols::SetSymbolPath(PCSTR Path)`
- `IDebugSymbols2/3::AppendSymbolPath(PCSTR Addition)`
- `IDebugSymbols3::SetSymbolPathWide(PCWSTR Path)`
- `IDebugSymbols3::AppendSymbolPathWide(PCWSTR Addition)`
- `IDebugSymbols::GetSymbolPath` / `IDebugSymbols3::GetSymbolPathWide`
- `IDebugSymbols::Reload` / `IDebugSymbols3::ReloadWide`

For dbgflow, the long-term backend implementation should call the wide
DbgEng methods where available. Text commands such as `.sympath` remain valid
through `eval` for trusted local debugging compatibility, but they should not
be the preferred internal implementation of a standardized symbol tool.

## Environment Variables

Windows debuggers can read `_NT_SYMBOL_PATH` and `_NT_ALT_SYMBOL_PATH` before
the debugger starts. Microsoft documents that the effective debugger symbol path
is created by appending `_NT_SYMBOL_PATH` after `_NT_ALT_SYMBOL_PATH`.

This makes environment variables appropriate for process-wide defaults, service
configuration, and worker startup state. They are weaker for per-session dynamic
updates because the intended DbgEng surface already exposes session APIs.

The debugger command-line option `-sins` ignores the symbol path environment
variables, so environment inheritance is not a complete contract.

## Symbol Path Syntax

A symbol path is a semicolon-separated list of elements. Elements may be normal
directories, cache entries, or symbol server entries.

Common examples:

```text
C:\symbols
srv*C:\symbols*https://msdl.microsoft.com/download/symbols
cache*C:\symbols;srv*https://msdl.microsoft.com/download/symbols
```

Relative paths are accepted by Windows debuggers, but dbgflow should prefer
absolute paths or explicit symbol server syntax for reproducibility and audit
quality.

## Reload Behavior

Changing the symbol path does not guarantee that all desired symbol information
has been loaded immediately. The WinDbg `.sympath` documentation directs users
to reload symbols after changing the path. The DbgEng API equivalent is
`IDebugSymbols::Reload`, whose parameter is interpreted like `.reload`
arguments.

Implementation options:

- Set or append the path only, and rely on lazy symbol loading.
- Set or append the path, then call `Reload("")` or a targeted reload when the
  caller requests refresh.
- Use forced reload semantics only when explicitly requested, because eager
  symbol loading may block on disk, network, symbol server, or proxy behavior.

## dbgflow Guidance

Standardized symbol path support should remain a debugging configuration tool,
not a general shell or environment mutator.

Recommended behavior:

- Keep the public tool name in debugging language, for example `set_symbols`.
- Accept native WinDbg symbol path strings so `srv*` and `cache*` syntax works.
- Store install-time defaults as `[debugger].symbol_path` and apply them with
  `IDebugSymbols3::SetSymbolPathWide` before opening a target.
- Reject empty path entries and control characters that can break command or log
  boundaries, especially while any command fallback exists.
- Record the requested path, append/replace mode, backend result, warnings, and
  any reload operation in artifacts and audit logs.
- Treat symbol cache contents and symbol-server downloads as local debugging
  artifacts that may reveal target module identity.
- Keep proxy handling separate. `_NT_SYMBOL_PROXY` controls SymSrv proxy
  behavior; it does not replace the symbol path. dbgflow may derive
  `_NT_SYMBOL_PROXY` from a configured HTTP/HTTPS proxy when the explicit symbol
  proxy is absent.

## Sources

- Microsoft Learn, Symbol path for Windows debuggers:
  <https://learn.microsoft.com/en-us/windows-hardware/drivers/debugger/symbol-path>
- Microsoft Learn, `IDebugSymbols::SetSymbolPath`:
  <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/dbgeng/nf-dbgeng-idebugsymbols-setsymbolpath>
- Microsoft Learn, `IDebugSymbols2::AppendSymbolPath`:
  <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/dbgeng/nf-dbgeng-idebugsymbols2-appendsymbolpath>
- Microsoft Learn, `IDebugSymbols3` method list:
  <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/dbgeng/nn-dbgeng-idebugsymbols3>
- Microsoft Learn, `.sympath` command:
  <https://learn.microsoft.com/en-us/windows-hardware/drivers/debuggercmds/-sympath--set-symbol-path->
- Microsoft Learn, `IDebugSymbols::Reload`:
  <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/dbgeng/nf-dbgeng-idebugsymbols-reload>
- Microsoft Learn, `.reload` command:
  <https://learn.microsoft.com/en-us/windows-hardware/drivers/debuggercmds/-reload--reload-module->
