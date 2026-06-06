# AGENTS.md

本文件面向参与本项目的 AI agent、开发者和自动化工具。修改本仓库前应先阅读本文件，并遵守其中的架构边界、开发约束和安全要求。

项目目标、阶段计划、关键进展和路线图记录在 `GOALS.md`，不要在本文件重复维护。

## 1. 项目定位

dbgflow 是面向 Windows 调试自动化的 MCP server / skills 工具链。

核心职责：

* 管理调试 session 生命周期。
* 对外暴露安全、结构化的调试工具接口。
* 对内适配 Windows 调试后端。
* 支持受控文本调试命令、结构化调试 API 和未来高级调试语义。
* 管理调试日志、输出和 artifacts。

本项目不是 shell wrapper，也不是无限制本地命令执行器。

## 2. 架构边界

保持以下分层：

```text
MCP Tool Layer
  -> Session Manager
  -> Policy Layer
  -> Artifact Manager
  -> DebugBackend
```

要求：

* MCP tool 表达调试意图，不直接绑定具体后端。
* 调试后端必须通过 `DebugBackend` 抽象接入。
* backend 选择属于内部实现细节，不作为常规公开 tool 暴露。
* tool handler 保持薄层，业务逻辑放入 session / backend / policy / artifacts 等核心层。
* 新增能力优先扩展抽象层和结构化工具，不在 tool handler 中堆积逻辑。

## 3. Tool API 原则

公开 tool 名称应处于调试 MCP 语境下，避免无意义的 `debug_` 前缀。

当前基础 session tools：

```text
create_session
list_sessions
close_session
```

`create_session` 采用 get-or-create 语义：同一 target 已存在 active session 时返回现有详情，否则创建新 session 并返回相同详情结构。

后续 tool 应优先表达调试目标或调试动作，例如：

```text
open_dump
analyze
get_stack
list_modules
continue_until_event
```

谨慎开放文本命令接口，例如：

```text
execute
```

文本命令接口必须经过 policy 检查。不得默认提供无限制 debugger command、shell、脚本或文件访问能力。

工具返回应尽量结构化，包含 session 状态、结果、warnings 和 artifact 引用。除原始输出查看接口外，不要只返回不可解析的大段文本。

## 4. Session 规则

基本规则：

* 每个调试目标对应独立 session。
* 同一 session 内操作必须串行化。
* 不同 session 可以并发。
* session 必须有明确状态，并支持显式关闭。
* 后端异常时，session 应进入错误状态。
* 不得静默重试可能破坏调试现场的操作。
* 长时间运行或阻塞操作必须支持 timeout 或 cancellation。

推荐状态集合：

```text
Created
Starting
Ready
Break
Running
Closing
Closed
Error
```

运行控制类操作应单独建模，例如 `continue_until_event` 和 `break_execution`。不要把 `g`、`p`、`t` 等运行类命令当成普通查询命令处理。

## 5. 安全规则

本项目会访问进程、dump、trace、符号、扩展 DLL 和调试输出，默认按敏感能力处理。

默认禁止：

```text
.shell
任意 .load
任意 .scriptload
任意脚本文件执行
任意 dump 写出
任意内存写出
任意外部进程执行
任意未授权路径访问
```

要求：

* 外部输入路径必须校验。
* 不允许路径穿越。
* 不允许默认访问任意用户目录。
* artifacts 必须位于受控 workspace。
* dump、TTD trace、transcript 和内存输出均视为敏感数据。
* 如需支持扩展加载，必须通过专门接口，并限制为 allowlisted extension。

## 6. 文本命令规则

文本命令能力用于兼容 WinDbg 命令生态，但必须受控。

要求：

* 默认使用 allowlist。
* 对危险命令使用 denylist。
* 区分查询命令与运行控制命令。
* 记录原始命令、输出、状态变化和错误。
* 对输出大小设置限制。
* 完整输出写入 artifact，工具响应只返回摘要、截断输出和 artifact 引用。

允许命令优先是查询类，例如：

```text
!analyze -v
k
kb
kv
~* k
lm
r
.ecxr
.exr
.cxr
.reload
.sympath
dx
```

危险命令不得通过普通文本接口开放。

## 7. Artifacts 与日志

每个 session 应有独立 artifact 目录：

```text
artifacts/
  sessions/
    <session_id>/
      transcript.log
      events.jsonl
      commands.jsonl
      outputs/
      dumps/
      traces/
```

必须记录：

* session 创建参数
* backend 类型
* tool 调用
* 调试命令
* 状态变化
* 后端事件
* 错误
* 关键输出摘要

不要默认记录完整内存内容。日志和报告应支持后续 redaction。

## 8. 代码组织

当前 workspace 以 crate 分层为主：

```text
crates/
  dbgflow-core/
  dbgflow-mcp/
```

要求：

* `dbgflow-core` 承载 session、backend、policy、artifacts、error 等核心逻辑。
* `dbgflow-mcp` 承载 MCP-facing tool facade，不污染核心层。
* backend 实现不得污染 MCP schema。
* policy 逻辑集中管理。
* path 处理集中管理。
* error type 应明确，不滥用字符串错误。
* 异步任务必须有关闭和清理路径。

## 9. 测试要求

至少覆盖：

* session 创建、查询、列出与关闭
* session 状态转换
* 同 session 命令串行化
* 多 session 并发
* command policy
* path policy
* artifact 写入
* timeout
* 后端错误处理
* mock backend 行为

涉及真实调试器的测试应与普通单元测试分离，避免 CI 环境不稳定。

修改后运行最相关、范围最小的检查。优先使用：

```text
cargo fmt --all -- --check
cargo test
```

若无法运行检查，应说明原因和已完成的替代验证。

本项目交互式 MCP 入口使用本地 HTTP transport，开发时从仓库根目录传入受控数据目录：

```text
cargo run -p dbgflow-mcp -- http --bind 127.0.0.1:7331 --data-dir D:\Repos\Project\dbgflow\var
```

从仓库根目录运行时，也可使用等价的 `--data-dir .\var`。`var/` 用于本地开发 artifacts 和 logs，已加入 `.gitignore`。HTTP transport 仅用于本机调试，必须绑定 loopback 地址；`/mcp` 不需要 bearer token 认证。不要使用无 `--data-dir` 的 HTTP 运行方式；stdio MCP transport 不作为公开运行入口。内部 session worker 使用标准子命令 `dbgflow-mcp worker session`，仅由主进程启动。

## 10. 文档维护

`AGENTS.md` 只记录协作规则、架构边界和安全要求。

`GOALS.md` 记录：

* 项目目标
* 当前阶段
* 关键进展
* 设计决策
* 里程碑
* 待办事项
* 风险列表

重要开发、架构调整或调试能力扩展后，应更新 `GOALS.md`。

完成 feature 后，应同步更新 `README.md` 和 `README.zh-CN.md`，确保中英文 README 反映当前可用能力、入口和限制。

## 11. 禁止事项

除非已有明确设计决策，否则不要：

* 移除 backend abstraction。
* 让 MCP tool 直接操作具体后端实现。
* 默认开放任意调试命令。
* 默认开放任意本地路径。
* 把本项目变成通用 shell runner。
* 将 dump、trace 或 transcript 当成普通非敏感文件。
* 在没有状态机的情况下实现复杂运行控制。
* 用不稳定文本解析结果冒充可靠结构化数据。
* 静默吞掉调试器错误。
* 自动删除用户 dump、trace 或日志文件。
