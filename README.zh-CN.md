# dbgflow

[English](README.md)

dbgflow 是一个早期阶段的 Windows 调试自动化 MCP server / skills 工具链。

当前实现聚焦于最小可用工程骨架：

- backend 抽象
- mock backend
- session 生命周期管理
- command policy 占位框架
- artifact manager 占位框架
- 面向 MCP 的 tool facade

初始公开 tool 名称：

- `create_session`
- `list_sessions`
- `close_session`

`create_session` 采用 get-or-create 语义：如果同一 target 已存在 active session，则返回该 session 的详情；否则创建新 session，并返回相同结构的详情。

当前 backend 选择属于内部实现细节，不作为公开 tool 暴露。调用方只需要描述要调试的 target，后续由内部机制选择合适的 backend。
