# dbgflow

[English](README.md)

dbgflow 是一个早期阶段的 Windows 调试自动化 MCP server / skills 工具链。

当前实现包含初始工程骨架、Windows-only 的 DbgEng dump 分析 / 进程调试 MVP、本地 Streamable HTTP MCP endpoint，以及 Windows service 脚本：

- backend 抽象
- session 生命周期管理
- 每个调试 session 独立 worker 子进程隔离
- command policy
- artifact manager
- 面向 dump target 的 DbgEng backend
- 面向进程 attach / launch target 的 DbgEng backend
- 受 denylist 保护的 `execute` 命令
- 面向 MCP 的 tool facade
- 带 resource update SSE 的 `/mcp` Streamable HTTP MCP endpoint
- 原生 Windows service 运行模式
- PowerShell 安装 / 卸载服务脚本

初始公开 tool 名称：

- `create_session`
- `get_session`
- `list_sessions`
- `close_session`
- `execute`
- `set_symbols`

`create_session` 采用 get-or-create 语义，并会快速返回 `Starting` session；后端打开 target 在后台完成。调用方可以通过 `get_session`、`list_sessions` 或 HTTP resource update stream 观察状态转为 `Ready`、`Break`、`Closed` 或 `Error`。
`target` 现在是必填参数；MCP tool schema 不再暴露 mock target。

当前 backend 选择属于内部实现细节，不作为公开 tool 暴露。调用方只需要描述要调试的 target，后续由内部机制选择合适的 backend。

在 Windows 上，DbgEng session 会按 WinDbg / WinDbg Preview 应用包、Windows SDK Debuggers、System32 fallback 的顺序解析 `dbgeng.dll`。

DbgEng target 当前支持 dump 文件、按 PID attach 进程，以及按 executable path + args launch 进程。
每个真实调试 session 会运行在独立 worker 子进程中；主 MCP 进程负责消息分发、session 状态、policy、artifacts、logs 和 worker 生命周期控制。

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

Dump target 可以指向任意已存在的本地 dump 文件，只要扩展名受支持。Launch target 默认关闭；仅在可信本地环境中设置 `DBGFLOW_ENABLE_LAUNCH=1` 后才允许受控启动进程。Launch 使用 suspended Win32 process creation 路径，并在 DbgEng attach 后再恢复目标进程。Executable 必须是已存在路径；shell invocation、自定义 cwd 和自定义 env 不属于当前 MVP。命令输出和日志仍写入受控 artifact root。`execute` 不再使用 allowlist，但 `.shell`、脚本加载、扩展加载、dump 写出和内存写出等危险命令仍会被 policy 拒绝。运行控制命令会单独更新 session 状态。

`execute` 保持同步返回，但不再对外暴露单条命令 timeout 设置。命令运行期间，session 会暴露 `current_operation`，并在 `last_operation` 中记录状态、耗时、artifact、错误和输出大小。调用方可以通过 `get_session`、`resources/read` 或 HTTP resource update stream 观察进度。旧请求中的 timeout 字段仍兼容接收，但会被忽略并写入 warning 日志。若正在执行命令时调用 `close_session`，服务会先请求 backend cancellation，再关闭 session。
如果 worker 卡住，主进程可以终止该 session 对应的 worker 子进程，不会拖垮其他 session 或 MCP server。

从仓库根目录通过本地 Streamable HTTP 启动 MCP server：

```text
cargo run -p dbgflow-mcp -- http --bind 127.0.0.1:7331 --data-dir .\var
```

HTTP endpoint 是 `http://127.0.0.1:7331/mcp`。`POST /mcp` 返回 JSON response；`GET /mcp` 打开 server-sent event stream，用于发送 MCP notifications，包括 session 状态变化对应的 `notifications/resources/updated`。`GET /healthz` 返回简单健康检查响应。

HTTP transport 仅用于本机调试：dbgflow 只允许绑定 loopback 地址，并拒绝非 localhost 的 `Origin` header。`/mcp` 不需要 bearer token 认证。

当前 server 支持 `initialize`、`notifications/initialized`、`ping`、`tools/list`、`tools/call`、`resources/list` 和 `resources/read`。Tool 结果以 JSON text content 返回；调试命令输出会完整返回，并同时写入 session artifacts。
最新命令 artifact 也会在 session 的 `last_operation` 中返回引用。

运行日志以按日 JSONL 文件写入 `<data-dir>\logs`，artifacts 写入 `<data-dir>\artifacts`。运行日志保留 7 天；artifacts 不会自动删除。

在 PowerShell 中安装或卸载 Windows service；如果当前 session 未提权，脚本会弹出 UAC
确认窗口并在确认后继续执行：

```text
.\scripts\install-service.ps1
.\scripts\uninstall-service.ps1
```

安装脚本会构建 release binary；如果已存在 `dbgflow-mcp` 服务，则先停止并卸载；然后将 exe 复制到 `%LOCALAPPDATA%\dbgflow\bin`，以 LocalSystem 和 `--data-dir %LOCALAPPDATA%\dbgflow\var` 安装并启动服务，再检查 `/healthz`。服务 artifacts 和 logs 写入 `%LOCALAPPDATA%\dbgflow\var`；卸载脚本默认不删除 artifacts 或 logs。

Live process DbgEng 集成测试默认 ignored，因为 attach / launch 行为依赖本机调试权限和目标进程状态；验证进程调试能力时需要显式运行这些测试。
