# dbgflow

[English](README.md)

dbgflow 是一个早期阶段的 Windows 调试自动化 MCP server / skills 工具链。

当前实现包含初始工程骨架、Windows-only 的 DbgEng dump 分析 MVP，以及最小 stdio MCP server：

- backend 抽象
- mock backend
- session 生命周期管理
- command policy
- artifact manager
- 面向 dump target 的 DbgEng backend
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

在 Windows 上，dump session 会按 WinDbg / WinDbg Preview 应用包、Windows SDK Debuggers、System32 fallback 的顺序解析 `dbgeng.dll`。

Dump target 可以指向任意已存在的本地 dump 文件，只要扩展名受支持；命令输出和日志仍写入受控 artifact root。

通过 stdio 启动 MCP server：

```text
cargo run -p dbgflow-mcp
```

当前 server 支持 `initialize`、`notifications/initialized`、`ping`、`tools/list` 和 `tools/call`。Tool 结果以 JSON text content 返回；原始调试命令输出仍写入 session artifacts。

默认情况下，MCP server 会将 artifacts 写入 workspace 级 `artifacts/` 目录。可通过 `DBGFLOW_ARTIFACT_ROOT` 覆盖该位置。
