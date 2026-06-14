# dbgflow

[English](README.md)

dbgflow 是一个早期阶段的 Windows 调试自动化 MCP server / skills 工具链。

当前实现包含初始工程骨架、Windows-only 的 DbgEng dump 分析 / 进程调试 MVP、本地 Streamable HTTP MCP endpoint，以及 Windows service 安装 / 卸载子命令：

- backend 抽象
- session 生命周期管理
- 每个调试 session 独立 worker 子进程隔离
- artifact manager
- 每个 session 独立的 transcript、event、command 和 output artifacts
- 面向 dump target 的 DbgEng backend
- 面向进程 attach / launch target 的 DbgEng backend
- 透传原生 WinDbg / DbgEng 命令并审计输出的 `eval`
- 面向 MCP 的 tool facade
- 带 resource update SSE 的 `/mcp` Streamable HTTP MCP endpoint
- 原生 Windows service 运行模式
- 原生 Windows service 安装 / 卸载子命令
- 主服务级代理配置，供 session worker 和 SymSrv 符号下载使用
- 安装时配置 DbgEng 初始 symbol path
- launch-only profiling，支持 native ETW collector
- 通过 `TTD.exe` 录制 Time Travel Debugging trace，支持 launch、attach 和有界 monitor 场景
- 通过 vendored `ida-pro-mcp` headless idalib supervisor 提供 IDA 逆向分析 session

当前公开 tool 名称：

- `dbg.create_session`
- `dbg.get_session`
- `dbg.list_sessions`
- `dbg.close_session`
- `dbg.eval`
- `dbg.add_symbols`
- `trace.record_profile`
- `trace.record_ttd`
- `ida.create_session`
- `ida.get_session`
- `ida.list_sessions`
- `ida.close_session`
- upstream `ida-pro-mcp` 非 debugger tools 会以 `ida.<tool_name>` 暴露，例如
  `ida.server_health`、`ida.list_funcs`、`ida.func_query`、`ida.disasm`、
  `ida.decompile`、`ida.xrefs_to`、`ida.imports`、`ida.idb_save`、
  `ida.rename`、`ida.set_comments`、`ida.set_type` 和 `ida.py_eval`

`dbg.create_session` 采用 get-or-create 语义，并会快速返回 `Starting` session；后端打开 target 在后台完成。调用方可以通过 `dbg.get_session`、`dbg.list_sessions` 或 HTTP resource update stream 观察状态转为 `Ready`、`Break`、`Closed` 或 `Error`。
`target` 现在是必填参数；MCP tool schema 不再暴露 mock target。

当前 backend 选择属于内部实现细节，不作为公开 tool 暴露。调用方只需要描述要调试的 target，后续由内部机制选择合适的 backend。

在 Windows 上，DbgEng session 会按 WinDbg / WinDbg Preview 应用包、Windows SDK Debuggers、System32 fallback 的顺序解析 `dbgeng.dll`。

DbgEng target 当前支持 dump 文件、按 PID attach 进程，以及按 executable path + args launch 进程。
每个真实调试 session 会运行在独立 worker 子进程中；主 MCP 进程负责消息分发、session 状态、target 校验、artifacts、logs 和 worker 生命周期控制。

Target 示例：

```json
{ "kind": "dump", "path": "C:\\crash.dmp" }
```

```json
{ "kind": "attach", "pid": 1234 }
```

```json
{ "kind": "launch", "executable": "C:\\app\\app.exe", "args": ["--flag"] }
```

Dump target 可以指向任意已存在的本地文件；如果文件不是 DbgEng 支持的文件，
由 DbgEng 返回错误。Launch 使用 suspended Win32 process creation 路径，并在
DbgEng attach 后再恢复目标进程。Executable 必须是已存在路径；shell invocation、
自定义 cwd 和自定义 env 不属于当前 MVP。命令输出、transcript、command record、
event record 和日志仍写入受控运行目录。`dbg.eval` 除空命令外会将原生 debugger
command 透传给 DbgEng；请仅在可信本地环境中使用。调试主线以 WinDbg / DbgEng
原生命令为稳定接口，包括运行控制命令；dbgflow 不为常见 WinDbg 命令重复暴露
typed wrapper。session 状态不再从命令文本识别运行控制，而是由 backend execution
status 事件和最终状态更新。`dbg.add_symbols` 追加原生 WinDbg symbol path 字符串，
包括 `srv*C:\symbols*https://msdl.microsoft.com/download/symbols` 这类 symbol server
路径。
运行配置也可以提供初始 DbgEng symbol path。dbgflow 会在打开 target 前通过
DbgEng symbols API 应用该路径，而不是依赖 worker 环境变量生效。

`trace.record_profile` 启动本地可执行文件，并围绕同一个 target 生命周期运行
native ETW collector。默认 collector 是 `native_etw`，使用
`scope.kind=target_process`、`event_sets=["process", "file_io"]`，
并默认开启 stack capture。它会写入标准 `.etl` trace，并为 target PID 产出
过滤后的 `process.jsonl`、`file_io.jsonl` 和 `summary.json`。
工具也接受 `collectors[]`，但当前只接受 `native_etw`；该结构保留给后续
hook-based 等内部 collector 扩展。dbgflow 不再包含 Procmon/Sysinternals
这类外部工具 collector。
采集会在目标进程退出或 `timeout_ms` 到达时停止。
timeout 默认只停止采集，不终止目标进程。profile 元数据、审计生命周期事件、
collector artifacts 以及目标 stdout/stderr 写入
`artifacts\profiles\<profile_id>`。

Native ETW `process` 采集 process start/end、thread start/end 和
image load/unload kernel events。原始 `trace.etl` 仍可能包含系统范围的 lifecycle
事件；dbgflow 后处理输出会严格过滤到本次 launch 的 target PID。Native ETW
`file_io` 采集常用 FileIo events，包括 name/rundown、create、read、write、
cleanup、flush、delete、rename、query、set、directory
enumeration/notification 和 file-system control。FileIo OpEnd completion 会按
`IrpPtr` 合并回匹配的 begin-side 事件，并在可用时补充 `nt_status`、`extra_info`
和 `completion_*` 字段；未匹配的 OpEnd 只进入 `summary.json` 统计和 warning，
不再输出独立行。close 事件视为 cleanup 之后的冗余句柄关闭信号，不再输出为独立
`file_io` 行。文件路径通过直接路径字段以及 `FileObject` / `FileKey`
best-effort 关联解析；无法解析时保留 raw pointer 字段。stack frame 以紧凑字符串
数组输出：能匹配 target PID image load 区间时为 `module+0xoffset`，否则保留
原始 `0x...` 地址；展示顺序接近 WinDbg stack。当前不做符号解析。
文件路径和 trace 内容属于敏感数据，默认加入 `file_io` 也会增加 trace 体积；
需要低容量 trace 时可以显式设置 `event_sets` 排除 `file_io`。

Profile 请求示例：

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

显式 collector 请求示例：

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
      "event_sets": ["process", "file_io"],
      "stacks": { "enabled": true }
    }
  ]
}
```

`trace.record_ttd` 通过 Microsoft `TTD.exe` 录制 Time Travel Debugging trace，支持
typed launch、attach 和 monitor target。可用 `[tools].ttd_dir` 显式覆盖
recorder 位置；若未配置，dbgflow 会先从 `[debugger].dbgeng_dir\ttd` 推导
`TTD.exe`，再回退到 `PATH`。dbgflow 不下载或安装 TTD，不接受任意 recorder
command line，并始终把 recorder 输出以及生成的 `.run` / `.out` / `.err` / `.idx` 文件写入
`artifacts\ttd_recordings\<recording_id>`。TTD recording 通常需要管理员权限，
会显著拖慢目标进程，也可能生成很大的文件。TTD artifact 可能包含内存、路径、
注册表数据和文件内容，应按敏感文件处理。

当前没有单独的 `trace.open_ttd` / `trace.open_run` 或并行 TTD analyzer tool。
生成的 `.run` trace 后续分析应优先复用 WinDbg / DbgEng 的 TTD 原生命令和
`dbg.eval`。现有 `dbg.create_session` 的文件 target 会把本地文件交给 DbgEng；
`.run` 是否可按该路径打开取决于本机 DbgEng/WinDbg 能力，目前需要单独 smoke
验证后再作为公开承诺写入文档。agent 分析 TTD 时应避免长串串行 stepping /
navigation 循环，优先使用 WinDbg TTD 的索引、事件、异常和查询类命令做定点分析。

TTD recording 请求示例：

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

`ida.create_session` 会打开已有 IDA database，或打开 IDA loader 可识别的本地
binary input，并返回一个长期存在的 reverse-analysis session。dbgflow vendor 了
`ida-pro-mcp` 的 Python runtime，并通过 stdio 启动一个 headless idalib supervisor：

```text
python -m ida_pro_mcp.idalib_supervisor --stdio --unsafe --profile <generated-profile> --max-workers <n>
```

supervisor 采用 upstream Headless idalib Session Model，为每个 database 管理独立
idalib worker。dbgflow 对外仍只接受自己的 UUID `session_id`，负责 session 外壳、
artifacts、audit logs、同一 reverse session 内串行化调用，以及不同 IDA sessions
之间可独立执行的 upstream worker 隔离。

可通过 `[reverse.ida]` 配置 IDA runtime：

```toml
[reverse.ida]
install_dir = "C:\\Program Files\\IDA Professional 9.3"
python_executable = "C:\\path\\to\\python.exe"       # optional
vendor_src_dir = "D:\\Repos\\Project\\dbgflow\\vendor\\ida-pro-mcp\\src" # optional
max_workers = 4                                      # optional
```

`install_dir` 也可来自 `DBGFLOW_IDA_DIR`；`python_executable` 也可来自
`DBGFLOW_IDA_PYTHON`；`vendor_src_dir` 也可来自 `DBGFLOW_IDA_PRO_MCP_SRC`。
选中的 Python 环境必须已安装或激活 `idapro>=0.0.9`。配置的 IDA 目录必须包含
`ida.exe`、`ida.dll`、`idalib.dll` 和 `ida.hlp`。编译 dbgflow 源码不需要 IDA SDK、
Clang、bindgen 或 `idalib-rs`。

MCP tool surface 保留 dbgflow 管理工具（`ida.create_session`、`ida.get_session`、
`ida.list_sessions`、`ida.close_session`），并将 upstream 非 debugger tools 暴露为
`ida.<tool_name>`。dbgflow 不公开 upstream `idb_open`、`idb_list`、`dbg_*`
debugger tools，也不默认公开 `py_exec_file`。`ida.py_eval` 默认启用，因为它对本机
逆向自动化很有用；但它会在可信本机 IDA context 中执行 IDAPython，应按敏感能力处理。

upstream tool schema 会尽量保持原样，只把 upstream 的 `database` 参数替换成必填
dbgflow `session_id`。成功响应返回 `{ session_id, tool, result, artifact, warnings }`；
其中 `result` 是 upstream `structuredContent`。每次请求、upstream response、`_meta`
和可下载的大结果都会归档到 `artifacts\reverse_sessions\<session_id>\outputs\`。

`ida.close_session` 默认先通过 `idb_save` 请求保存 upstream database（`save: true`），
随后将 dbgflow session 标记为 `Closed`，并忘记 `session_id` 到 upstream database 的
映射。它不会强杀 headless worker，也不会关闭 GUI IDA instance；worker 资源回收交给
upstream idle TTL。若保存请求失败，dbgflow 会保留 session 并返回错误，调用方可以重试
保存，或显式选择 `save: false` 脱管。只有希望脱管且不发起显式保存请求时，才传
`save: false`。

IDA session 请求示例：

```json
{
  "target": { "kind": "binary", "path": "C:\\samples\\a.exe" },
  "mode": "prefer_headless",
  "run_auto_analysis": true,
  "build_caches": true,
  "init_hexrays": true,
  "idle_ttl_sec": 3600,
  "startup_timeout_ms": 1800000
}
```

`dbg.eval` 保持同步返回，但不再对外暴露单条命令 timeout 设置。命令运行期间，session 会暴露 `current_operation`，并在 `last_operation` 中记录状态、耗时、artifact、错误和输出大小。如果 DbgEng 报告目标正在运行，session state 会变为 `Running`；下一次 debug event 返回后恢复为 `Break`，如果 backend 报告 no debuggee 则变为 `Closed`。调用方可以通过 `dbg.get_session`、`resources/read` 或 HTTP resource update stream 观察进度。旧请求中的 timeout 字段仍兼容接收，但会被忽略并写入 warning 日志。若正在执行命令时调用 `dbg.close_session`，服务会先请求 backend cancellation，再关闭 session。
如果 worker 卡住，主进程可以终止该 session 对应的 worker 子进程，不会拖垮其他 session 或 MCP server。

从仓库根目录通过本地 Streamable HTTP 启动 MCP server：

```text
cargo run -p dbgflow-mcp -- http --config C:\Users\dstars\AppData\Local\dbgflow\config.toml
```

运行配置来自 TOML：

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
# 当 debugger.dbgeng_dir\ttd 包含 TTD.exe 时可省略。
ttd_dir = "C:\\Users\\dstars\\Bin\\TTD"

[reverse.ida]
install_dir = "C:\\Program Files\\IDA Professional 9.3"

[process]
child_identity = "mcp_peer_session"
fallback_child_identity = "active_interactive_session"
elevate_if_admin = true

[proxy]
mode = "url"
url = "http://127.0.0.1:7897"
```

HTTP endpoint 是 `http://127.0.0.1:7331/mcp`。`POST /mcp` 返回 JSON response；`GET /mcp` 打开 server-sent event stream，用于发送 MCP notifications，包括 session 状态变化对应的 `notifications/resources/updated`。`GET /healthz` 返回简单健康检查响应。

HTTP transport 仅用于本机调试：dbgflow 只允许绑定 loopback 地址，并拒绝非 localhost 的 `Origin` header。`/mcp` 不需要 bearer token 认证。HTTP request body 上限为 16 MiB。

`[process]` 控制 dbgflow 启动子进程时使用的身份，覆盖 debug session worker、
IDA reverse worker、DbgEng launch target、profile target 和 TTD recorder。
未配置 `[process]` 时保持兼容的 `current_process` 行为。安装脚本会写入
`child_identity = "mcp_peer_session"`、fallback 为
`active_interactive_session`，并启用 `elevate_if_admin = true`。当前 HTTP `/mcp`
没有强客户端认证或 impersonation；dbgflow 会从本机 loopback TCP owner table
推断 peer PID/session，并在日志中记录最终 policy、session id、是否 elevated
以及 fallback 原因。

代理配置是主服务级别的配置，来自 `config.toml` 的 `[proxy]`。使用
`mode = "url"` 和 `url = "http://host:port"` 会为 DbgEng/SymSrv 符号下载
设置 `_NT_SYMBOL_PROXY`，并为 session worker 和 launch 出来的 debuggee 设置
`HTTP_PROXY` / `HTTPS_PROXY` 及小写等价变量。使用 `mode = "disabled"` 会清除
已知代理变量；使用 `mode = "env"` 和 `[proxy.env]` 可持久化具体代理环境变量；
使用 `mode = "none"` 表示不配置代理。在 `mode = "env"` 中，若未显式配置
`_NT_SYMBOL_PROXY`，运行时会尝试从 `HTTPS_PROXY`、`HTTP_PROXY` 或 `ALL_PROXY`
派生 SymSrv 所需的 `host:port` 形式。

当前 server 支持 `initialize`、`notifications/initialized`、`ping`、`tools/list`、`tools/call`、`resources/list` 和 `resources/read`。Tool 结果以 JSON text content 返回；调试命令输出会完整返回，并同时写入 session artifacts。
最新命令 artifact 也会在 session 的 `last_operation` 中返回引用。

运行日志以按日 JSONL 文件写入 `<data-dir>\logs`，artifacts 写入 `<data-dir>\artifacts`。每个 session 会写入 `sessions\<session_id>\transcript.log`、`events.jsonl`、`commands.jsonl` 和 `outputs\<command_id>.txt`。运行日志保留 7 天；artifacts 不会自动删除。

排障时先查看按日运行日志，用于关联 HTTP/MCP 请求、tool 调用、worker 生命周期、
DbgEng 操作、profile job、TTD recording job、service 启停、耗时、错误和 artifact 路径；再根据日志中的
session/profile/TTD recording artifact 路径查看具体归档。运行日志不写入 debugger command output
或完整 HTTP request body；命令输出保留在每条命令的 output artifact 和 session
transcript 中。

从仓库脚本构建并安装或卸载 Windows service：

```text
.\scripts\install-service.ps1
.\scripts\uninstall-service.ps1
```

安装脚本会构建 release 版 `dbgflow-mcp`，探测本机 runtime 依赖，写入
`%LOCALAPPDATA%\dbgflow\config.toml`，展示最终摘要，然后调用
`target\release\dbgflow-mcp.exe service install --config <path>`。安装子命令会
校验配置，按需请求 UAC 提权，复制当前 exe 到 `%LOCALAPPDATA%\dbgflow\bin`，
以 LocalSystem 和 `service run --config <path>` 安装服务，启动服务并检查
`/healthz`。

安装脚本按 Microsoft Store WinDbg 包优先、Windows Kits / WDK Debuggers 其次、
System32 最后的顺序探测 DbgEng。TTD 是可选依赖；脚本会先从已解析的
DbgEng 目录按 `<dbgeng_dir>\ttd` 推导，
再回退到独立 TTD package 和 `PATH` 探测。可用 `-TtdDir <path>` 显式覆盖该位置。
脚本也会从 `DBGFLOW_IDA_DIR`、Windows uninstall registry 和常见 Program Files
目录探测 IDA 安装目录；当目录包含 `ida.exe`、`ida.dll`、`idalib.dll` 和
`ida.hlp` 时写入 `[reverse.ida].install_dir`。可用 `-IdaInstallDir <path>` 显式覆盖
IDA 探测结果。

使用 `-ProxyUrl <url>`、`-NoProxy` 或现有 proxy 环境变量控制生成的 `[proxy]`
配置。使用 `-SymbolPath <path>` 写入 `[debugger].symbol_path`；如果未传该参数，
安装脚本会在当前环境存在 `_NT_ALT_SYMBOL_PATH` / `_NT_SYMBOL_PATH` 时持久化它们，
否则不写入该字段。脚本不会默认写入 Microsoft public symbol server。使用
`-NonInteractive` 可跳过最终确认，直接写入探测值 / 默认值。

使用安装脚本生成的 `[process]` policy 时，IDA/idalib 通常运行在 MCP loopback peer
所在 Windows session，或 active interactive session fallback；当用户存在 linked
elevated token 时会优先使用提升 token。因此 IDA license 和 license terms 接受状态
需要对最终解析出的用户 token 可见。若 token 解析失败，dbgflow 会 fallback 并记录
原因；这种情况下可能需要让 IDA license 对服务账号（例如 LocalSystem）可见，或使用
机器可见的 license 文件 / floating-license 设置。

卸载会从 Windows Service Control Manager 查询已安装服务命令行，找回已安装 exe
路径和 `--config` 路径，然后删除服务并删除整个配置中的 install root，包括 `bin`、
`config.toml`、logs 和 artifacts。如果服务已经不存在，可向
`scripts\uninstall-service.ps1` 传入 `-ConfigPath <path>` 作为 fallback。

需要验证真实 DbgEng / ETW / TTD 环境时，优先使用受控 smoke 脚本或手动 MCP 调用。
仓库默认测试不包含需要提权或依赖本机 live 调试环境的 ignored 测试。
