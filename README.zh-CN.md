# dbgflow

[English](README.md)

dbgflow 是一个早期阶段的 Windows 调试自动化 MCP server / skills 工具链。

当前实现包含初始工程骨架、Windows-only 的 DbgEng dump 分析 / 进程调试 MVP，以及最小 stdio MCP server：

- backend 抽象
- mock backend
- session 生命周期管理
- command policy
- artifact manager
- 面向 dump target 的 DbgEng backend
- 面向进程 attach / launch target 的 DbgEng backend
- 受 allowlist 控制的 `execute` 命令
- 面向 MCP 的 tool facade
- 带 tool schema 的 stdio JSON-RPC MCP 入口

初始公开 tool 名称：

- `create_session`
- `list_sessions`
- `close_session`
- `execute`

`create_session` 采用 get-or-create 语义：如果同一 target 已存在 active session，则返回该 session 的详情；否则创建新 session，并返回相同结构的详情。

当前 backend 选择属于内部实现细节，不作为公开 tool 暴露。调用方只需要描述要调试的 target，后续由内部机制选择合适的 backend。

在 Windows 上，DbgEng session 会按 WinDbg / WinDbg Preview 应用包、Windows SDK Debuggers、System32 fallback 的顺序解析 `dbgeng.dll`。

DbgEng target 当前支持 dump 文件、按 PID attach 进程，以及按 executable path + args launch 进程。

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

Dump target 可以指向任意已存在的本地 dump 文件，只要扩展名受支持。Launch target 默认关闭；仅在可信本地环境中设置 `DBGFLOW_ENABLE_LAUNCH=1` 后才允许受控启动进程。Launch 使用 suspended Win32 process creation 路径，并在 DbgEng attach 后再恢复目标进程。Executable 必须是已存在路径；shell invocation、自定义 cwd 和自定义 env 不属于当前 MVP。命令输出和日志仍写入受控 artifact root。当前唯一通过 `execute` 开放的运行控制命令是精确的 `g`；step 和 breakpoint 相关命令仍默认拒绝。

通过 stdio 启动 MCP server：

```text
cargo run -p dbgflow-mcp
```

当前 server 支持 `initialize`、`notifications/initialized`、`ping`、`tools/list` 和 `tools/call`。Tool 结果以 JSON text content 返回；原始调试命令输出仍写入 session artifacts。

默认情况下，MCP server 会将 artifacts 写入 workspace 级 `artifacts/` 目录。可通过 `DBGFLOW_ARTIFACT_ROOT` 覆盖该位置。

Live process DbgEng 集成测试默认 ignored，因为 attach / launch 行为依赖本机调试权限和目标进程状态；验证进程调试能力时需要显式运行这些测试。
