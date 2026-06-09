# Time Travel Debugging recording

This note records the external behavior dbgflow relies on for TTD recording.

## References

- Microsoft documents `TTD.exe` as the command-line recorder for Time Travel
  Debugging traces. It supports launch, attach, and monitor modes.
- `-out <path>` selects the trace file or output directory. dbgflow always uses
  a controlled artifact directory for this value.
- `-noUI` disables the small recorder UI and is the dbgflow default for
  automation.
- `-accepteula` is available for automation after the user has reviewed and
  accepted the TTD license.
- `-children`, `-cmdLineFilter`, `-ring`, `-maxFile`, `-module`,
  `-recordmode`, and `-replayCpuSupport` are modeled as typed options rather
  than passed through as arbitrary command text.
- Microsoft notes that TTD recording typically requires administrator
  privileges, can significantly slow the target, and can generate large trace
  files.
- TTD trace files use `.run`; `.idx` index files can be re-created and are often
  large. Recorder diagnostics can appear in `.out` / `.err` output files.
- Microsoft warns that recordings may contain memory, file paths, registry
  data, file contents, personally identifiable information, or security-related
  information. dbgflow therefore treats TTD artifacts as sensitive.

## Implementation consequences

- `trace.record_ttd` is a dedicated tool, not a `trace.record_profile`
  collector. TTD launch recording owns target process creation, while
  `trace.record_profile` assumes dbgflow
  owns target launch after collectors start.
- dbgflow config accepts `[tools].ttd_dir` as an explicit directory containing
  `TTD.exe`; when omitted, the runtime derives `<dbgeng_dir>\ttd` from
  `[debugger].dbgeng_dir` if that directory contains `TTD.exe`. Tool requests do
  not accept recorder paths.
- The recorder command is built with `std::process::Command` argv values only;
  no shell command line is constructed.
- All recorder output and generated trace files stay under
  `artifacts\ttd_recordings\<recording_id>`.

## Source links

- <https://learn.microsoft.com/en-us/windows-hardware/drivers/debuggercmds/time-travel-debugging-ttd-exe-command-line-util>
- <https://learn.microsoft.com/en-us/windows-hardware/drivers/debuggercmds/time-travel-debugging-trace-file-information>
