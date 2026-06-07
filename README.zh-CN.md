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
- launch-only profiling，支持 native ETW 和可选 Sysinternals Procmon collector

初始公开 tool 名称：

- `create_session`
- `get_session`
- `list_sessions`
- `close_session`
- `eval`
- `set_symbols`
- `run_profile`

`create_session` 采用 get-or-create 语义，并会快速返回 `Starting` session；后端打开 target 在后台完成。调用方可以通过 `get_session`、`list_sessions` 或 HTTP resource update stream 观察状态转为 `Ready`、`Break`、`Closed` 或 `Error`。
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

Dump target 可以指向任意已存在的本地文件；如果文件不是受支持的 dump，由 DbgEng 返回错误。Launch 使用 suspended Win32 process creation 路径，并在 DbgEng attach 后再恢复目标进程。Executable 必须是已存在路径；shell invocation、自定义 cwd 和自定义 env 不属于当前 MVP。命令输出、transcript、command record、event record 和日志仍写入受控运行目录。`eval` 除空命令外会将原生 debugger command 透传给 DbgEng；请仅在可信本地环境中使用。session 状态不再从命令文本识别运行控制，而是由 backend execution status 事件和最终状态更新。`set_symbols` 接受原生 WinDbg symbol path 字符串，包括 `srv*C:\symbols*https://msdl.microsoft.com/download/symbols` 这类 symbol server 路径。

`run_profile` 启动本地可执行文件，并围绕同一个 target 生命周期运行一个
或多个 profiling collector。默认 collector 是 `native_etw/system_overview`，
会写入标准 `.etl` trace。工具也接受 `collectors[]` 做并行采集；旧的单数
`collector` 字段继续兼容。采集会在目标进程退出或 `timeout_ms` 到达时停止。
timeout 默认只停止采集，不终止目标进程。profile 元数据、生命周期事件、
collector artifacts 以及目标 stdout/stderr 写入
`artifacts\profiles\<profile_id>`。

Profile 请求示例：

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

并行 collector 请求示例：

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

`procmon` collector 是可选能力，依赖 Sysinternals Process Monitor。通过
`config.toml` 中的 `[tools].sysinternals_dir` 配置；dbgflow 只会从该目录中
派生 `Procmon64.exe` 或 `Procmon.exe`。如果未配置该路径，依赖 Sysinternals
的能力会返回明确错误，且不会启动目标进程。`run_profile` 请求不接受
Sysinternals 路径；这是 server runtime 配置。dbgflow 不下载 Procmon、不扫描
全盘，也不接受单独的 Procmon exe 路径。Procmon 会写入
权威 artifact `capture.pml`，并导出 `events.csv` 以及按 target PID /
operation / path best-effort 过滤后的 `events.jsonl`；请求 stack capture 时，
dbgflow 也会请求带 stack 数据的 XML 导出。

`eval` 保持同步返回，但不再对外暴露单条命令 timeout 设置。命令运行期间，session 会暴露 `current_operation`，并在 `last_operation` 中记录状态、耗时、artifact、错误和输出大小。如果 DbgEng 报告目标正在运行，session state 会变为 `Running`；下一次 debug event 返回后恢复为 `Break`，如果 backend 报告 no debuggee 则变为 `Closed`。调用方可以通过 `get_session`、`resources/read` 或 HTTP resource update stream 观察进度。旧请求中的 timeout 字段仍兼容接收，但会被忽略并写入 warning 日志。若正在执行命令时调用 `close_session`，服务会先请求 backend cancellation，再关闭 session。
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

[tools]
sysinternals_dir = "C:\\Users\\dstars\\Bin\\SysinternalsSuite"

[proxy]
mode = "url"
url = "http://127.0.0.1:7897"
```

HTTP endpoint 是 `http://127.0.0.1:7331/mcp`。`POST /mcp` 返回 JSON response；`GET /mcp` 打开 server-sent event stream，用于发送 MCP notifications，包括 session 状态变化对应的 `notifications/resources/updated`。`GET /healthz` 返回简单健康检查响应。

HTTP transport 仅用于本机调试：dbgflow 只允许绑定 loopback 地址，并拒绝非 localhost 的 `Origin` header。`/mcp` 不需要 bearer token 认证。HTTP request body 上限为 16 MiB。

代理配置是主服务级别的配置，来自 `config.toml` 的 `[proxy]`。使用
`mode = "url"` 和 `url = "http://host:port"` 会为 DbgEng/SymSrv 符号下载
设置 `_NT_SYMBOL_PROXY`，并为 session worker 和 launch 出来的 debuggee 设置
`HTTP_PROXY` / `HTTPS_PROXY` 及小写等价变量。使用 `mode = "disabled"` 会清除
已知代理变量；使用 `mode = "env"` 和 `[proxy.env]` 可持久化具体代理环境变量；
使用 `mode = "none"` 表示不配置代理。

当前 server 支持 `initialize`、`notifications/initialized`、`ping`、`tools/list`、`tools/call`、`resources/list` 和 `resources/read`。Tool 结果以 JSON text content 返回；调试命令输出会完整返回，并同时写入 session artifacts。
最新命令 artifact 也会在 session 的 `last_operation` 中返回引用。

运行日志以按日 JSONL 文件写入 `<data-dir>\logs`，artifacts 写入 `<data-dir>\artifacts`。每个 session 会写入 `sessions\<session_id>\transcript.log`、`events.jsonl`、`commands.jsonl` 和 `outputs\<command_id>.txt`。运行日志保留 7 天；artifacts 不会自动删除。

排障时先查看按日运行日志，用于关联 HTTP/MCP 请求、tool 调用、worker 生命周期、
DbgEng 操作、profile job、service 启停、耗时、错误和 artifact 路径；再根据日志中的
session/profile artifact 路径查看具体归档。运行日志不写入 debugger command output
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
System32 最后的顺序探测 DbgEng。Sysinternals 仍是可选依赖；如果没有配置
Sysinternals 目录，service 仍会正常安装运行，但 Procmon-based profiling 不可用。
使用 `-ProxyUrl <url>`、`-NoProxy` 或现有 proxy 环境变量控制生成的 `[proxy]`
配置；使用 `-NonInteractive` 可跳过最终确认，直接写入探测值 / 默认值。

卸载会从 Windows Service Control Manager 查询已安装服务命令行，找回已安装 exe
路径和 `--config` 路径，然后删除服务并删除整个配置中的 install root，包括 `bin`、
`config.toml`、logs 和 artifacts。如果服务已经不存在，可向
`scripts\uninstall-service.ps1` 传入 `-ConfigPath <path>` 作为 fallback。

Live process DbgEng 集成测试默认 ignored，因为 attach / launch 行为依赖本机调试权限和目标进程状态；验证进程调试能力时需要显式运行这些测试。live HTTP E2E 测试会启动 `dbgflow-mcp http`、调用 `/mcp`，并覆盖 attach / launch 的 worker 子进程链路。
