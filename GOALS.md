# GOALS.md

本文件记录项目目标、关键进展、设计决策和迭代计划。
每次完成重要开发、架构调整或调试能力扩展后，应更新本文件。

## 1. 项目名称

正式名称：

```text
dbgflow
```

如未来需要更名，应单独记录设计决策。

## 2. 项目愿景

构建一套面向 Windows 的自动化调试 MCP server / skills 工具链，使 AI agent 能够安全、稳定、可追踪地执行调试任务，包括 dump 分析、进程 attach、程序 launch、异常分析、栈分析、模块分析、符号分析、插件调用，以及未来的 TTD 时间旅行调试。

项目目标不是简单封装命令行调试器，而是构建一个可持续演进的调试编排平台。

## 3. 核心目标

### G1. 多会话调试管理

支持同时管理多个独立调试会话：

* 每个 session 对应一个 dump、进程或 trace。
* 每个 session 有独立状态机。
* 每个 session 有独立日志和 artifact。
* 同一 session 内命令串行执行。
* 不同 session 可并发运行。

### G2. DbgEng 作为长期主后端

优先设计并逐步实现 DbgEngBackend：

* 支持文本命令执行。
* 支持 output callback 捕获输出。
* 支持 event callback 捕获调试事件。
* 支持 dump / launch / attach。
* 长期通过 DbgEngBackend 承载真实调试能力。

### G3. 保留文本命令兼容能力

即使使用 DbgEng，也保留 WinDbg / DbgEng 风格命令接口：

```text
!analyze -v
~* k
lm
.reload
dx
!custom_extension.command
```

文本接口用于：

* 复用现有 WinDbg 知识。
* 复用调试器扩展。
* 方便专家介入。
* 支持快速 MVP。
* 支持后续 DSL 编译目标。

### G4. 安全可控的调试执行

所有调试能力必须经过权限策略控制：

* 默认禁止危险命令。
* 外部输入路径必须校验、规范化并记录。
* 插件加载必须 allowlist。
* dump、trace、transcript 视为敏感文件。
* 所有工具调用必须可审计。
* 所有 session 必须可关闭、可清理、可追踪。

### G5. 支持自定义调试 DSL

未来支持高阶语义命令：

```text
analyze crash
stack all
modules suspicious
continue until exception
break on symbol
watch writes address
```

DSL 不直接等于调试器命令字符串，而应编译为内部 debug plan。

### G6. 支持 TTD 时间旅行调试

中后期支持：

* TTD launch recording
* TTD attach recording
* TTD monitor recording
* 打开 `.run` trace
* 查询异常事件
* 查询内存访问
* 支持前后跳转分析

TTD trace 必须作为敏感 artifact 管理。

## 4. 非目标

当前阶段不追求：

* 完整替代 WinDbg GUI。
* 支持 kernel debugging。
* 自动修复所有 bug。
* 对未校验或未记录的任意本地路径开放调试器能力。
* 将 AI agent 暴露为不受限制的 shell。
* 默认上传 dump、trace 或内存内容到外部服务。

这些能力如需引入，必须单独设计和评审。

## 5. 当前架构方向

当前推荐架构：

```text
Rust MCP Server
  |
  |-- MCP Tool Layer
  |-- Session Manager
  |-- Policy Layer
  |-- Artifact Manager
  |-- SessionWorker launcher
        |
        v
      Per-session worker process
        |
        v
      DebugBackend trait
        |
        |-- DbgEngBackend
        |     |-- execute
        |     |-- event callbacks
        |
        |-- TTD backend optional
```

推荐优先级：

```text
1. 先定义 DebugBackend 抽象
2. 先实现测试用 worker launcher 用于快速验证
3. 再实现 DbgEngBackend MVP
4. 每个真实 session 通过独立 worker 子进程隔离
5. 保持 MCP API 稳定
6. 后端能力逐步增强
```

## 6. 初始 MVP 范围

### MVP-1: 基础 MCP server

目标：

* Rust MCP server 可启动。
* 可暴露基本 tool。
* 可返回 JSON。
* 有基础错误处理。

工具：

```text
create_session
list_sessions
close_session
```

验收标准：

* HTTP MCP client 能发现工具。
* 能创建 mock session。
* 能关闭 mock session。
* 有基础日志。

### MVP-2: 调试会话管理

目标：

* 实现 SessionManager。
* 实现 session actor。
* 实现 session 状态机。
* 同 session 命令串行化。
* 支持 session cancellation。

验收标准：

* 多 session 可并发存在。
* 同一 session 不会并发执行多个命令。
* session 状态转换可记录。
* session 关闭后资源释放。

### MVP-3: 文本命令执行

目标：

* 支持 `eval`。
* 支持 command policy。
* 支持输出捕获。
* 支持执行状态观测和 cancellation。
* 支持 raw output artifact。

验收标准：

* 可执行未被 denylist 拒绝的诊断命令。
* 非法命令被拒绝。
* 长时间命令执行期间可查询状态，并可通过关闭 session 请求取消。
* 所有命令有审计记录。

### MVP-4: Dump 分析

目标：

* 支持打开 Windows dump。
* 支持通过 `eval` 执行 `!analyze -v` 等受控诊断命令。
* 支持 raw output artifact。
* 支持 transcript 和审计事件。

工具：

```text
create_session
eval
close_session
```

验收标准：

* 给定 dump 文件，可执行 `!analyze -v`。
* 可返回 raw output。
* 可写 transcript。
* 可关闭 session。

### MVP-5: Launch / Attach

目标：

* 支持 launch process。
* 支持 attach process。
* 支持 continue until event。
* 支持 breakpoint / exception / process exit 的基础事件建模。

工具：

```text
launch_process
attach_process
continue_until_event
break_execution
```

验收标准：

* 可启动进程并进入 break 状态。
* 可 attach 到进程。
* 可继续运行直到异常、断点、退出或超时。
* Running / Break / Closed 状态准确。

## 7. 后续里程碑

### M1. DbgEng Worker

实现 DbgEng worker：

* 初始化 DebugCreate / DebugClient。
* 支持 OpenDump。
* 支持 Execute。
* 支持 OutputCallbacks。
* 支持 WaitForEvent。
* 支持 Close / EndSession。

### M2. Extension 管理

支持受控加载调试扩展：

* extension allowlist
* extension path sandbox
* extension command policy
* extension output capture

### M3. TTD Recorder

支持：

* record launch
* record attach
* record monitor
* trace artifact 管理

### M4. TTD Analyzer

支持：

* open trace
* list events
* query exceptions
* navigate positions
* query memory access

### M5. 自定义 DSL

实现初始 DSL：

```text
analyze crash
stack all
modules suspicious
continue until exception
```

并将 DSL 编译为 debug plan。

### M6. 报告生成

支持生成调试报告：

* crash summary
* suspected root cause
* thread summary
* stack highlights
* module and symbol health
* artifact links

## 8. 关键设计决策

### D-001: 保留文本命令接口

决定：

即使采用 DbgEngBackend，也保留 `eval` 能力。

原因：

* 兼容 WinDbg 命令生态。
* 复用现有扩展。
* 方便专家用户。
* 有利于快速 MVP。
* 可作为 DSL 的底层 fallback。

### D-002: 不默认开放任意命令

决定：

文本命令接口必须经过 policy 检查。

原因：

* 调试器命令能力过强。
* 可能访问文件、加载 DLL、执行脚本或影响进程。
* MCP tool 会被 agent 调用，必须有安全边界。

### D-003: 运行控制单独建模

决定：

`g`、`p`、`t` 等运行类命令不作为普通 query command 处理。

原因：

* 运行命令可能长时间不返回。
* 运行命令会改变 session 状态。
* 更适合通过 `continue_until_event` 表达。

### D-004: 每个 session 独立 artifact 目录

决定：

每个 session 写入独立 artifact 目录。

原因：

* 便于审计。
* 便于复现。
* 避免多会话日志混杂。
* 支持后续报告生成。

### D-005: DbgEng 可通过 C++ worker 封装

决定：

如果 Rust 直接绑定 DbgEng 复杂度过高，允许使用 C++ worker 作为后端。

原因：

* DbgEng 是 COM 风格接口。
* Callback、线程模型和生命周期在 C++ 中更自然。
* Rust 层可保持 MCP、策略和调度职责清晰。

### D-006: 不引入 CdbBackend 中间层

决定：

真实调试后端直接面向 DbgEngBackend 演进，不实现 CdbBackend 或 cdb 子进程 MVP 作为中间环节。

原因：

* 避免同时维护两套真实后端生命周期和状态语义。
* 避免围绕 cdb stdout / sentinel / 进程管道建立临时协议。
* 尽早把复杂度集中到长期目标 DbgEngBackend、session 状态机和 artifact 审计链路上。

### D-007: 每个 session 使用独立 worker 子进程

决定：

每个真实 dbgflow session 创建时启动独立 worker 子进程；worker 内部选择 DbgEng / 未来 TTD 等真实后端。主进程不直接执行 DbgEng COM 调用。

原因：

* DbgEng、符号加载或扩展命令可能长期阻塞且无法可靠中断线程。
* 子进程是更可靠的调试 session 隔离和回收边界。
* 主进程可以继续处理 MCP 消息、维护状态和关闭其他 session。
* artifacts 和日志仍由主进程统一写入，便于审计和后续 redaction。

## 9. 关键进展记录

### 2026-05-31

已明确初始方向：

* 项目定位为 Windows 自动化调试 MCP / skills 工具链。
* 明确 DbgEngBackend 的长期价值：事件驱动、状态管理、安全边界和更高架构上限。
* 明确不引入 CdbBackend 或 cdb 子进程 MVP 作为中间环节。
* 明确即使切换到 DbgEng，也应保留文本命令执行能力。
* 明确以受控文本兼容层作为当前调试命令入口。
* 明确未来可扩展自定义调试 DSL。
* 项目正式命名为 `dbgflow`。
* 文本命令接口命名收敛为 `execute`，不再使用 `execute_text`。

已落地初始工程骨架：

* 创建 Rust workspace。
* 创建 `dbgflow-core` crate，承载 backend abstraction、mock backend、session manager、command policy 和 artifact manager 骨架。
* 创建 `dbgflow-mcp` crate，承载 MCP-facing tool facade。
* 初始公开 tool 命名收敛为 session 语义：

  * `create_session`
  * `list_sessions`
  * `close_session`
* `list_backends` 不作为公开 tool 暴露，backend 选择属于内部实现细节。
* `create_session` 采用 get-or-create 语义：同一 target 已存在 active session 时返回现有详情，否则创建并返回相同详情结构。
* 添加 mock session lifecycle 测试，覆盖 create / query / list / close。

已落地 DbgEng / dump 分析 MVP：

* 实现 Windows-only `DbgEngBackend`，直接接入 DbgEng，不引入 CdbBackend。
* 实现 `dbgeng.dll` resolver，查找顺序为 WinDbg / WinDbg Preview 应用商店版、Windows SDK Debuggers、System32 fallback。
* 通过动态加载 `dbgeng.dll` 和 `DebugCreate` 创建 DbgEng client。
* 支持 `DebugTarget::Dump`，打开 dump 后调用 `WaitForEvent` 进入可分析状态。
* 支持受控 `eval`，当前采用 denylist-only policy，默认允许诊断命令并拒绝危险命令。
* `eval` 输出写入 session artifact，响应返回完整输出和 artifact 引用。
* 添加生成 crash dump fixture 的 Windows integration test，已跑通 `!analyze -v`。
* Dump target 允许指向任意已存在的本地 dump 文件，路径会先校验和规范化；输出和日志仍写入受控 artifact root。

已补齐第一阶段 MCP server 入口：

* `dbgflow-mcp` 从启动信息打印演进为本地 HTTP JSON-RPC MCP server。
* 支持 `initialize`、`notifications/initialized`、`ping`、`tools/list` 和 `tools/call`。
* `create_session`、`get_session`、`list_sessions`、`close_session`、`eval`、`set_symbols` 暴露 MCP `inputSchema`。
* `create_session` 的 MCP 参数采用 `{ "target": { "kind": "mock" } }` / `{ "target": { "kind": "dump", "path": "..." } }` 形态，再转换到 core 层 `DebugTarget`。
* Tool 调用结果以 JSON text content 返回；后端错误作为 MCP tool error 返回。
* MCP server 增加 JSON-RPC envelope 校验、protocolVersion 协商、unknown tool / invalid arguments 的 protocol error 分类。
* HTTP MCP server 通过必填 `--data-dir` 固定 artifacts 和 logs 位置。

已落地最小进程调试 MVP：

* `DebugTarget` 扩展支持 `Attach { pid }` 和 `Launch { executable, args }`。
* MCP `create_session` target schema 支持 `attach` 和 `launch`，不新增公开 tool。
* `DbgEngBackend` 支持通过 `AttachProcess` attach PID；launch 默认关闭，需要 `DBGFLOW_ENABLE_LAUNCH=1` 显式启用，并采用 suspended Win32 process creation、DbgEng attach 后再 resume 的最小实现。
* `eval` 继续作为唯一调试命令入口，并开放精确 `g` 作为最小运行控制命令。
* `g` 在后端通过 `SetExecutionStatus(DEBUG_STATUS_GO) + WaitForEvent` 执行。
* 进程调试集成测试已添加，但默认 ignored；当前本机显式运行 attach / launch 测试已通过。

### 2026-06-04

已落地本地 HTTP / Windows service MVP：

* `dbgflow-mcp` 使用 `http` 子命令作为公开 MCP transport。
* 新增本地 Streamable HTTP MCP endpoint：`POST /mcp` 复用现有 JSON-RPC MCP handler，`GET /healthz` 提供健康检查。
* HTTP `POST /mcp` 返回 `application/json`，`GET /mcp` 提供服务端 SSE stream。
* HTTP 默认绑定 `127.0.0.1:7331`，非空 `Origin` 仅允许 localhost / loopback。
* HTTP transport 仅允许 loopback bind，非空 `Origin` 仅允许 localhost / loopback；`/mcp` 不要求 bearer token 认证。
* 新增原生 Windows service 运行模式，支持 SCM stop / shutdown 控制并有序停止 HTTP listener。
* 新增 `scripts/install-service.ps1`：构建 release binary，替换已有服务，复制 exe 到用户 `%LOCALAPPDATA%\dbgflow\bin`，以 LocalSystem 安装并启动 `dbgflow-mcp` 服务。
* 新增 `scripts/uninstall-service.ps1`：停止并卸载服务；默认保留 artifacts 和 logs，避免误删敏感调试输出。
* 安装 / 卸载入口支持非管理员启动；检测到未提权时会弹出 UAC，并把当前参数转交给提权后的主程序进程。

已落地 dump 异步打开与易用性修复：

* `create_session` 改为异步创建，先返回 `Starting` session，后端启动完成后转为 `Ready` / `Break`，失败转为 `Error`。
* session 状态增加更新时间、当前操作、错误信息和可空 backend session id；新增 `get_session` 工具用于查询单个 session。
* session 间不共享创建阶段阻塞路径；不同 session 可并发，同 session 操作仍串行。
* `close_session` 对 Starting / Running session 采用尽力关闭语义，先返回 `Closed`，后端资源后台释放。
* DbgEng 命令执行和输出 callback 改为 Wide API，提高 Windows Unicode 路径兼容性。
* 新增 `set_symbols` 工具，先验证本地符号目录，再通过 `.sympath` / `.sympath+` 设置符号路径。
* HTTP `GET /mcp` 支持 SSE，session 状态变化通过 `notifications/resources/updated` 推送；同时支持 session resources 的 list/read。
* Windows service 默认 artifacts/logs 目录调整为 `%LOCALAPPDATA%\dbgflow\var`。

已落地 DbgEng 生命周期稳定性与统一运行日志：

* 运行时目录参数收敛为 `--data-dir`；内部固定使用 `<data-dir>\artifacts` 和 `<data-dir>\logs`，不再暴露独立 `--artifact-root` / `--log-dir` CLI 参数。
* service 安装脚本只传 `%LOCALAPPDATA%\dbgflow\var` 作为 data dir。
* 新增按日 JSONL 运行日志，记录 service、session 和 DbgEng open / WaitForEvent / execute / close 等关键阶段。
* 运行日志保留 7 天，仅淘汰 logs，不自动删除 artifacts。
* DbgEng in-process COM 操作增加进程级串行化，降低并行 open / close 与 DbgEng 回调重入导致的状态污染风险。
* `create_session` 不再复用 `Closing` session，避免同 target 重试绑定正在关闭的旧 backend id。

### 2026-06-06

已落地长命令 timeout 策略调整与状态可观测性增强：

* MCP tool schema 不再暴露 `startup_timeout_ms` / `timeout_ms`；旧请求字段仍兼容接收，但会被忽略并记录 warning。
* `create_session` 不再使用 backend startup timeout；后端打开 target 期间保持 `Starting`，由 `get_session` / resource update SSE 观察完成状态。
* `eval` 不再使用 backend reply timeout；长命令执行期间写入 `current_operation`，完成后在 `last_operation` 中记录 status、duration、artifact、error 和 output bytes。
* `close_session` 在存在当前操作时会先调用 backend cancellation；DbgEng backend 通过 `IDebugControl::SetInterrupt(DEBUG_INTERRUPT_EXIT)` 请求中断，再等待 worker 完成关闭。
* DbgEng `g` / open target 的 `WaitForEvent` 改为无限等待，由 session 状态查询和 cancellation 承担长操作控制。

已落地每 session 子进程隔离架构：

* `SessionManager` 不再持有真实 backend map，而是维护 `session_id -> SessionWorker`。
* 默认真实 session 通过内部 `worker session` 子命令启动独立 worker 子进程；worker 内部再选择 DbgEng backend。
* 主进程统一执行 target validation、command policy、session 状态管理、artifact 写入和运行日志记录。
* `close_session` 可终止卡住的 worker 子进程，避免 DbgEng 阻塞拖住 MCP 主进程或其他 session。
* MCP schema 移除 mock target，`create_session` 必须显式传入 dump / attach / launch target。

已收敛公开运行入口：

* 删除公开 stdio MCP transport；内部 `worker session` 子命令仅用于 session worker 通信。
* `dbgflow-mcp http` 必须传 `--data-dir`，开发默认使用仓库内 `.\var`。
* 本地 HTTP transport 不使用 bearer token 认证；安全边界依赖 loopback-only bind、localhost `Origin` 限制和 command/path policy。
* 删除 `MockBackend`，单元测试改用测试专用 `SessionWorkerLauncher` / fake worker。

### 2026-06-07

已移除本地 HTTP bearer token 认证：

* `/mcp` 不再要求 `Authorization: Bearer ...`，Codex 等本地 MCP client 只需配置 `http://127.0.0.1:7331/mcp`。
* 删除 `http-token.txt` 生成、读取和校验逻辑，`--data-dir` 只承载 artifacts 和 logs。
* 继续保留 loopback-only bind 与 localhost / loopback `Origin` 限制；远程 HTTP 访问仍不支持。
* Windows service 安装入口不再输出 token 文件位置，卸载入口仍默认保留 artifacts 和 logs。

已将 Windows service 安装 / 卸载能力迁入主程序：

* 新增 `dbgflow-mcp service run`、`dbgflow-mcp service install` 和 `dbgflow-mcp service uninstall` 子命令。
* 安装子命令负责非管理员入口 UAC 提权、替换已有服务、复制当前 exe 到 `%LOCALAPPDATA%\dbgflow\bin`、授权目录、以 LocalSystem 安装并启动服务、检查 `/healthz`。
* 卸载子命令停止并删除服务，默认保留 `%LOCALAPPDATA%\dbgflow\var` 下的 artifacts 和 logs；`--remove-install-files` 仅删除受控 install root 下的 `bin`。
* `scripts/install-service.ps1` 和 `scripts/uninstall-service.ps1` 负责从仓库构建 release binary，然后调用构建出的 `target\release\dbgflow-mcp.exe service install|uninstall` 并传入安装参数；不在主程序内执行 cargo build。
* service 运行入口严格收敛为 `dbgflow-mcp service run --data-dir <path>`；不再支持裸 `dbgflow-mcp service --data-dir <path>` 形态。

已重命名公开文本命令入口：

* session-facing API 从 `execute` 改为 `eval`。
* MCP tool 名称从 `execute` 改为 `eval`，输入 schema 和返回结构保持不变。
* backend / DbgEng 内部仍保留 `execute` 作为底层调试器命令执行语义。

已补齐 session audit artifact 链路：

* 每个 session 初始化 `transcript.log`、`events.jsonl`、`commands.jsonl` 和 `outputs/`。
* session 创建、worker startup、eval start / finish / reject、close 和 worker 异常会写入 `events.jsonl`。
* `eval` 命令记录写入 `commands.jsonl`，包含 command id、状态、输出路径、耗时、输出大小、错误和 backend session id。
* `transcript.log` 记录 session 生命周期、命令开始/结束和完整命令输出；完整命令输出仍单独写入 `outputs/<command_id>.txt`。

已补齐 live HTTP E2E 验证场景：

* 新增 Windows-only ignored integration tests，通过真实 `dbgflow-mcp http`、HTTP `/mcp`、SessionManager、worker 子进程和 DbgEng 完成端到端验证。
* attach 场景覆盖真实进程 attach、`lm`、`~* k`、resource read、close 和审计 artifacts。
* launch 场景覆盖 `DBGFLOW_ENABLE_LAUNCH=1`、进程 launch、`g` continue、resource read、close 和审计 artifacts。

已收敛路线图目标：

* 当前路线图不再把单独的 dump 查询工具作为目标。
* 当前公开能力聚焦 session lifecycle、受控 `eval`、symbol path 设置、状态可观测性和审计 artifacts。

## 10. 当前待办

### P0

* [x] 确定项目名称。
* [x] 初始化 Rust workspace。
* [x] 定义 MCP tool schema。
* [x] 定义 `DebugBackend` trait。
* [x] 定义 `SessionState`。
* [x] 定义 `SessionManager`。
* [x] 实现测试用 fake worker。
* [x] 实现基础 artifact manager。
* [x] 实现 command policy 框架。

### P1

* [x] 实现 DbgEngBackend 最小版本。
* [x] 支持打开 dump。
* [x] 支持文本命令执行。
* [x] 支持 `!analyze -v`。
* [x] 支持 transcript。
* [x] 支持 close session。
* [x] 增加集成测试。

### P2

* [x] 支持 attach process MVP。
* [x] 支持 launch process MVP。
* [x] 支持最小 continue until event：通过 `eval` 精确命令 `g`，进度由 session 状态和 cancellation 控制。
* [x] 支持本地 Streamable HTTP MCP endpoint。
* [x] 支持 Windows service 安装 / 卸载子命令。
* [x] 补齐更多 live attach / launch HTTP E2E ignored 验证场景。
* [x] 补齐 `transcript.log` 和 `events.jsonl` 审计链路。

### P3

* [x] 明确本地 HTTP 不使用 token / auth，依赖 loopback-only bind、Origin 限制和 policy 层。
* [x] 支持 Streamable HTTP SSE stream。
* [ ] 支持 extension allowlist。
* [ ] 支持 TTD recording。
* [ ] 支持 TTD trace artifact。
* [ ] 支持 DSL prototype。
* [ ] 支持调试报告生成。

## 11. 风险列表

### R1. DbgEng 绑定复杂

风险：

Rust 直接绑定 DbgEng 可能涉及较多 unsafe、COM callback、线程模型和生命周期问题。

缓解：

* 先实现 mock backend。
* 可使用 C++ worker 封装 DbgEng。
* Rust 层通过稳定 IPC 调用 worker。

### R2. 文本命令安全风险

风险：

WinDbg 命令能力过强，可能被滥用。

缓解：

* denylist-only command policy。
* path sandbox。
* extension allowlist。
* 默认禁用危险命令。

### R3. 输出解析不稳定

风险：

WinDbg 文本输出可能因版本、符号状态、语言环境、扩展不同而变化。

缓解：

* raw output 永久保留。
* 如未来引入文本解析，必须标注 confidence。
* 不把文本解析结果伪装成绝对事实。

### R4. 长时间运行命令导致 session 卡死

风险：

运行类命令或扩展命令可能不返回。

缓解：

* query 与 run-control 分离。
* 长操作通过 `current_operation` / `last_operation` 暴露状态。
* session 支持 cancellation。
* 每个真实 session 运行在独立 worker 子进程中，必要时可直接终止该 session worker。
* 必要时将 session 标记为 Error。

### R5. Dump / TTD 敏感数据泄露

风险：

dump、trace、transcript 可能包含内存、路径、注册表、凭据或业务数据。

缓解：

* artifacts 默认本地保存。
* 不自动上传。
* 日志支持 redaction。
* 报告生成时过滤敏感内容。

## 12. 近期开发计划

下一轮建议执行顺序：

```text
1. 增强 command policy 的参数、安全边界和测试覆盖
2. 继续在更多真实目标上验证 live attach / launch ignored integration tests
3. 完善 transcript.log / events.jsonl 的 redaction 与报告消费格式
4. 扩展受控 extension allowlist
5. 在状态机稳定后扩展 breakpoint / step / break_execution 等运行控制
```

优先保持架构清晰，而不是过早追求完整调试能力。

## 13. 完成定义

项目第一阶段完成的最低标准：

* MCP server 可启动。
* 能创建调试 session。
* 能打开一个 dump。
* 能执行受控文本命令。
* 能运行 `!analyze -v`。
* 能返回 raw output 和 artifact 引用。
* 能记录 transcript。
* 能关闭 session。
* 有 command policy。
* 有 session state。
* 有基础测试。
* `GOALS.md` 已更新关键进展。
