# GOALS.md

本文件记录项目目标、设计决策和迭代计划。
每次完成重要开发、架构调整或调试能力扩展后，应更新本文件。

## 1. 项目名称

正式名称：

```text
dbgflow
```

如未来需要更名，应单独记录设计决策。

## 2. 项目愿景

构建一套面向 Windows 的自动化调试 MCP server / skills 工具链，使 AI agent 能够安全、稳定、可追踪地执行调试任务，包括 dump 分析、进程 attach、程序 launch、异常分析、栈分析、模块分析、符号分析，以及未来的 TTD 时间旅行调试。

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
```

文本接口用于：

* 复用现有 WinDbg 知识。
* 方便专家介入。
* 支持快速 MVP。

### G4. 安全可控的调试执行

调试能力面向可信本机环境，必须保留清晰的运行边界和审计链路：

* 外部输入路径必须校验、规范化并记录。
* dump、trace、transcript 视为敏感文件。
* 所有工具调用必须可审计。
* 所有 session 必须可关闭、可清理、可追踪。
* HTTP transport 必须保持 loopback-only。

### G5. 支持 TTD 时间旅行调试

中后期支持：

* TTD launch recording
* TTD attach recording
* TTD monitor recording
* 打开 `.run` trace
* 查询异常事件
* 查询内存访问
* 支持前后跳转分析

TTD trace 必须作为敏感 artifact 管理。

### G6. Native ETW Profiling

支持通过 dbgflow 直接编排 native ETW 采样，生成标准 `.etl` artifact，
并与调试 session、artifact、审计链路保持一致。第一版采用一次性
`run_profile` 工具，支持 launch-only、内置 `system_overview` preset、
目标退出或 timeout 自动停止采集。

## 4. 非目标

当前阶段不追求：

* 完整替代 WinDbg GUI。
* 支持 kernel debugging。
* 自动修复所有 bug。
* 对未校验或未记录的任意本地路径开放调试器能力。
* 默认上传 dump、trace 或内存内容到外部服务。

这些能力如需引入，必须单独设计和评审。

## 5. 当前架构方向

当前推荐架构：

```text
Rust MCP Server
  |
  |-- MCP Tool Layer
  |-- Session Manager
  |-- Target Validation
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

## 6. 后续里程碑

### M1. DbgEng Worker

实现 DbgEng worker：

* 初始化 DebugCreate / DebugClient。
* 支持 OpenDump。
* 支持 Execute。
* 支持 OutputCallbacks。
* 支持 WaitForEvent。
* 支持 Close / EndSession。

### M2. TTD Recorder

支持：

* record launch
* record attach
* record monitor
* trace artifact 管理

### M3. TTD Analyzer

支持：

* open trace
* list events
* query exceptions
* navigate positions
* query memory access

### M4. 报告生成

支持生成调试报告：

* crash summary
* suspected root cause
* thread summary
* stack highlights
* module and symbol health
* artifact links

## 7. 关键设计决策

### D-001: 保留文本命令接口

决定：

即使采用 DbgEngBackend，也保留 `eval` 能力。

原因：

* 兼容 WinDbg 命令生态。
* 方便专家用户。
* 有利于快速 MVP。

### D-002: eval 透传原生调试命令

决定：

`eval` 除空命令外透传原生 WinDbg / DbgEng 命令。

原因：

* 兼容 WinDbg 命令生态和专家工作流。
* 避免维护不完整 denylist 造成误判。
* 本项目当前定位为可信本机调试工具，主要边界是 loopback HTTP、worker 隔离、显式 data-dir 和 artifacts 审计。

### D-003: 执行状态由 backend 感知

决定：

session 的 `Running` / `Break` / `Closed` 状态由 backend execution status 事件和最终状态更新，不通过 WinDbg 命令文本、前缀或分隔符推断。

原因：

* DbgEng 能感知真实执行状态，文本识别会漏掉别名、复合命令和未来能力。
* `eval` 是原生命令透传接口，不能把命令解析结果冒充调试目标状态。
* 后续 `continue_until_event`、step、breakpoint 等专用 tool 应复用同一 backend 状态通道。

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

* DbgEng、符号加载或运行控制命令可能长期阻塞且无法可靠中断线程。
* 子进程是更可靠的调试 session 隔离和回收边界。
* 主进程可以继续处理 MCP 消息、维护状态和关闭其他 session。
* artifacts 和日志仍由主进程统一写入，便于审计和后续 redaction。

### D-008: `run_profile` 使用 native ETW 而不是 WPR 命令行

决定：

第一版 profiling 能力直接在代码层面控制 ETW session，输出标准
`.etl`，不通过 `wpr.exe` 命令行编排。

原因：

* 避免把 dbgflow 变成 shell wrapper。
* 保持采集生命周期、artifact、错误和审计由核心层统一控制。
* 为后续 debugger-gated profiling 和更轻量的 provider preset 留出空间。

### D-009: Procmon 作为可选并行 profile collector

决定：

`run_profile` 支持 `collectors[]` 并行采集。`native_etw/system_overview`
仍是默认概览 collector；Sysinternals Process Monitor 作为显式启用的
`procmon` collector，用于更精确的文件 / 注册表 I/O 事件和可选堆栈 artifact。
Procmon collector 保留 `capture.pml` 作为权威 artifact，并导出 `events.csv`
以及按 target PID / operation / path best-effort 过滤后的 `events.jsonl`。

Procmon 只从主服务 `--sysinternals-dir` 中派生 `Procmon64.exe` 或
`Procmon.exe`，不接受单独 exe 路径，不自动下载，不扫描全盘。安装脚本可交互
识别 Sysinternals 目录；未配置时，依赖 Sysinternals 的能力不可用。

原因：

* `system_overview` 适合系统概览，不保证稳定产生可按目标文件归因的直接
  `FileIo Read` / `DiskIo Read` 操作。
* Procmon 更适合文件 / 注册表操作审计和调用栈采集。
* 并行 collector 能在同一次 target 生命周期中同时保留 ETW 概览和 Procmon
  精确 I/O 证据。
* 通过 `--sysinternals-dir` 显式配置，保持服务行为可审计且不依赖用户环境。

## 8. 当前待办

### P0

* [x] 确定项目名称。
* [x] 初始化 Rust workspace。
* [x] 定义 MCP tool schema。
* [x] 定义 `DebugBackend` trait。
* [x] 定义 `SessionState`。
* [x] 定义 `SessionManager`。
* [x] 实现测试用 fake worker。
* [x] 实现基础 artifact manager。
* [x] 实现基础命令审计和 artifact 写入。

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
* [x] 支持 backend execution status 驱动的运行状态感知：`eval` 不通过命令文本判断运行控制。
* [x] 支持本地 Streamable HTTP MCP endpoint。
* [x] 支持 Windows service 安装 / 卸载子命令。
* [x] 支持主服务级代理配置，并通过 `_NT_SYMBOL_PROXY` 支持 SymSrv 符号下载代理。
* [x] 补齐更多 live attach / launch HTTP E2E ignored 验证场景。
* [x] 补齐 `transcript.log` 和 `events.jsonl` 审计链路。

### P3

* [x] 明确本地 HTTP 不使用 token / auth，依赖 loopback-only bind、Origin 限制和可信本机环境。
* [x] 支持 Streamable HTTP SSE stream。
* [x] 支持 native ETW launch-only `run_profile` MVP。
* [x] 支持并行 profile collectors 和可选 Procmon collector MVP。
* [ ] 支持 TTD recording。
* [ ] 支持 TTD trace artifact。
* [ ] 支持 debugger-gated profiling。
* [ ] 支持 ETL 后处理和报告生成。
* [ ] 支持调试报告生成。

## 9. 近期开发计划

下一轮建议执行顺序：

```text
1. 增强 target/path validation、安全边界和测试覆盖
2. 继续在更多真实目标上验证 live attach / launch ignored integration tests
3. 完善 transcript.log / events.jsonl 的 redaction 与报告消费格式
4. 支持调试报告生成 MVP
5. 基于 backend execution status 通道扩展 breakpoint / step / break_execution 等运行控制
```

优先保持架构清晰，而不是过早追求完整调试能力。
