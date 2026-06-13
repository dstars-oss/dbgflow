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

构建一套面向 Windows 的自动化调试、trace 采集和 headless 逆向分析
MCP server / skills 工具链，使 AI agent 能够安全、稳定、可追踪地执行
调试和二进制分析任务，包括 dump 分析、进程 attach、程序 launch、异常分析、
栈分析、模块分析、符号分析、反汇编、反编译、IDB 标注管理，以及未来的
TTD 时间旅行调试。

项目目标不是简单封装命令行调试器或逆向工具，而是构建一个可持续演进的
调试、trace 和逆向分析编排平台。

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
`trace.record_profile` 工具，支持 launch-only、native ETW
`process` / `file_io` event sets、目标退出或 timeout 自动停止采集，
并对 target PID 产出后处理后的 `process.jsonl`、`file_io.jsonl`
和 `summary.json`。

### G7. idalib 反汇编与逆向分析

基于 Hex-Rays idalib 构建 headless 逆向分析能力，将 IDA engine 作为受控
本机分析后端接入 dbgflow，而不是通过任意 IDA 命令或脚本执行实现。

目标能力：

* 支持 binary / IDB analysis session。
* 支持函数、反汇编、字符串、imports / exports、xrefs、类型和伪代码查询。
* 支持受控 IDB 标注修改能力，例如 rename、comment 和 type，并完整记录审计。
* 公开 tool 使用 `ida.*` namespace，首版面向 IDA / idalib 语义。
* binary、IDB / i64、伪代码、反汇编和分析输出均按敏感 artifact 管理。

相关依据：

* Hex-Rays IDA 9.0 文档说明 idalib 支持在 IDA 外部通过 C++ / Python API 使用
  IDA engine：https://docs.hex-rays.com/release-notes/9_0
* Hex-Rays 9.2 feature overview 继续描述 idalib headless library 能力：
  https://hex-rays.com/feature-overview-ida-8.5-vs-9.2
* Rust 生态已有 idalib 绑定探索；dbgflow 当前采用 Rust 主体、runtime-only direct
  IDA DLL binding 的受控 worker 路线：https://github.com/binarly-io/idalib

## 4. 非目标

当前阶段不追求：

* 完整替代 WinDbg GUI。
* 完整替代 IDA GUI 或交互式逆向工作台。
* 支持 kernel debugging。
* 自动修复所有 bug。
* 对未校验或未记录的任意本地路径开放调试器能力。
* 对外开放任意 IDA / OS 命令执行能力。
* 默认上传 dump、trace 或内存内容到外部服务。

这些能力如需引入，必须单独设计和评审。

## 5. 当前架构方向

当前推荐架构：

```text
crates/
  dbgflow-common
    error / logging / proxy / ids / validation / artifacts / job guards
  dbgflow-debug
    session manager / session worker / DebugBackend / DbgEngBackend
  dbgflow-trace
    profile manager / native ETW / TTD recording
  dbgflow-reverse
    reserved boundary for future idalib reverse analysis
  dbgflow-core
    compatibility facade for existing Rust paths
  dbgflow-mcp
    HTTP transport / MCP protocol / tool registry and schemas

Runtime flow:

dbgflow-mcp
  |
  |-- dbg.* MCP Tool Layer -> dbgflow-debug
  |     |-- Session Manager
  |     |-- Target Validation
  |     |-- Artifact Manager from dbgflow-common
  |     |-- SessionWorker launcher
  |           |
  |           v
  |         Per-session worker process
  |           |
  |           v
  |         DebugBackend trait
  |           |
  |           |-- DbgEngBackend
  |           |     |-- execute
  |           |     |-- event callbacks
  |           |
  |           |-- TTD backend optional
  |-- trace.* MCP Tool Layer -> dbgflow-trace
  |     |-- ProfileManager / native ETW
  |     |-- TtdRecordingManager / TTD.exe
  |     |-- Artifact Manager from dbgflow-common
  |
  |-- ida.* MCP Tool Layer -> dbgflow-reverse
        |
        |-- ReverseSessionManager
        |-- ReverseTarget Validation
        |-- Artifact Manager from dbgflow-common
        |-- ReverseWorker launcher
        |
        v
      Per-analysis worker process
        |
        v
      IDA dynamic binding worker
        |
        |-- runtime load idalib.dll / ida.dll
        |-- minimal C ABI
        |-- no build-time IDA SDK dependency
```

推荐优先级：

```text
1. 先定义 DebugBackend 抽象
2. 先实现测试用 worker launcher 用于快速验证
3. 再实现 DbgEngBackend MVP
4. 每个真实 session 通过独立 worker 子进程隔离
5. 保持 MCP API 稳定
6. 后端能力逐步增强
7. 将 idalib 逆向能力作为独立 `ida.*` tool family 设计
8. 先实现 ReverseBackend / worker spike，再扩展查询和标注工具
9. 新增跨领域基础设施优先放入 `dbgflow-common`，避免 debug / trace / reverse 重复实现
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

### M3. TTD Trace 分析工作流

支持：

* 验证并文档化 DbgEng / WinDbg 对 `.run` trace 的打开路径。
* 优先通过 `dbg.eval` 复用 WinDbg TTD 原生命令查询事件、异常、位置和内存访问。
* 避免新增一套覆盖 WinDbg TTD 命令体系的并行 analyzer API。
* 只在 dbgflow 需要管理 session、artifact、索引缓存或高层报告时增加很薄的 helper。

### M4. 报告生成

支持生成调试报告：

* crash summary
* suspected root cause
* thread summary
* stack highlights
* module and symbol health
* artifact links

### M5. IdaLib Reverse Worker

实现基于 idalib 的 headless reverse worker：

* 支持 runtime 配置探测或指定 IDA install 与 SDK / idalib 运行环境。
* 支持每个 reverse analysis session 使用独立 worker 子进程。
* 支持 `ida.*` tool descriptor 与 reverse session lifecycle MVP。
* 支持只读查询能力：函数、反汇编、字符串、imports / exports、xrefs、类型和伪代码。
* 支持受控标注修改能力：rename、comment 和 type。
* 支持 reverse artifacts、审计日志和错误记录。

当前 MVP tool 名称：

```text
ida.create_session
ida.get_session
ida.close_session
ida.get_metadata
ida.list_segments
ida.list_functions
ida.list_strings
ida.list_imports
ida.list_exports
ida.lookup_functions
ida.disassemble
ida.decompile
ida.list_xrefs
ida.rename
ida.set_comment
ida.set_type
```

`ida.close_session` 默认请求保存打开的数据库；`.idb` / `.i64` target 默认原地操作。
当前基础 idalib close ABI 不返回保存成功与否，事件和 session warning 会把保存结果
记录为 `unknown`。rich reverse tools 通过 Rust direct binding 直接加载官方
`ida.dll` / `idalib.dll` runtime symbols；缺少 symbol、license 或 processor 支持时
返回明确 unsupported error，基础 session / metadata / segment / function 查询仍可用。
qstring 和 xrefblk_t 依赖能力会在 database 打开后用当前 IDB 中的安全样本做 runtime
validation；未通过时 metadata warning 和 unsupported error 会记录具体原因。分页 rich
tools 在 Rust 层统一应用默认 limit 100、最大 limit 10000。`ida.decompile` 保持明确
unsupported，直到 Hex-Rays dispatcher 完成验证。

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
* `dbg.eval` 是调试主入口，运行控制也优先使用 WinDbg 原生命令；如未来确需新增
  typed run-control tool，也必须复用同一 backend 状态通道，而不是解析命令文本。

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

### D-008: `trace.record_profile` 使用 native ETW 而不是 WPR 命令行

决定：

第一版 profiling 能力直接在代码层面控制 ETW session，输出标准
`.etl`，不通过 `wpr.exe` 命令行编排。

原因：

* 避免把 dbgflow 变成 shell wrapper。
* 保持采集生命周期、artifact、错误和审计由核心层统一控制。
* 为后续 debugger-gated profiling 和更轻量的 provider preset 留出空间。

### D-009: 删除 Procmon profile collector，record 以 ETW 为主

决定：

`trace.record_profile` 继续保留 `collectors[]` 结构，但当前只接受
`native_etw` collector。`native_etw` 默认 collector 已由 D-015/D-016
改为 `target_process + process + file_io + stacks`。原可选 Sysinternals
Process Monitor / `procmon` collector 已删除，运行时不再读取
`tools.sysinternals_dir`，安装脚本也不再探测或写入 Sysinternals 配置。

后续非 ETW record 能力优先以内部 hook-based collector 等受控实现扩展，
不再为 profiling 引入外部采集工具。

原因：

* record 的主线目标是稳定、可自动化、可审计的 ETW trace。
* Procmon 依赖外部 GUI/CLI 工具、EULA、权限和全局采集状态，增加运行时与测试复杂度。
* dbgflow 不应为了 profiling 退化成外部工具编排器。
* `collectors[]` 结构仍为未来内部 collector 扩展保留。

### D-010: runtime 配置统一收敛到 TOML 文件

决定：

HTTP runtime、Windows service install / run / uninstall 均以 `--config <path>`
读取 `config.toml`。安装脚本负责本机探测 DbgEng、TTD 和 proxy，并写入
配置文件；Rust service install 只负责读取配置、校验、提权、复制 exe、创建服务、
启动和健康检查。

卸载时不假设当前执行的 exe 就是已安装服务 exe。默认按 service name 从 Windows
Service Control Manager 查询已安装服务命令行，解析其中的 `service run --config
<path>`，再读取配置并删除整个 install root。服务不存在时，可显式传入 config 作为
fallback。

原因：

* 避免 bind、data dir、DbgEng、TTD、proxy 分散在命令行、service
  Environment 和安装脚本临时状态中。
* 配置文件更易审计、备份和后续扩展。
* 卸载必须以已安装服务的真实命令行为准，避免从另一个工作区或另一个 exe 运行
  uninstall 时删错路径。

### D-011: 初始 DbgEng symbol path 由 runtime 配置承载并通过 API 应用

决定：

`config.toml` 的 `[debugger].symbol_path` 记录安装时确定的初始符号路径。
安装脚本支持显式 `-SymbolPath`；未传时继承当前 `_NT_ALT_SYMBOL_PATH` 和
`_NT_SYMBOL_PATH`，不存在则不写入。真实 DbgEng session 在打开 target 前通过
`IDebugSymbols3::SetSymbolPathWide` 应用该路径。

原因：

* symbol path 是调试后端配置，不应依赖 worker 环境变量作为标准化生效机制。
* 保留原生 WinDbg symbol path 字符串，兼容 `srv*`、`cache*`、UNC 和分号语义。
* 不默认写入 Microsoft public symbol server，避免默认网络下载和隐藏磁盘占用。

### D-012: SymSrv proxy 可由网络代理配置派生

决定：

`_NT_SYMBOL_PROXY` 仍是 SymSrv 符号下载代理的最终环境变量入口。若 proxy 配置来自
环境变量且未显式提供 `_NT_SYMBOL_PROXY`，runtime 可从 `HTTPS_PROXY`、
`HTTP_PROXY` 或 `ALL_PROXY` 派生 SymSrv 所需的 `host:port` 值；无法派生时保留
原网络代理变量但不设置 `_NT_SYMBOL_PROXY`。

原因：

* 安装脚本可继续持久化现有网络代理环境，不需要新增配置项。
* 符号下载代理行为对用户更符合预期，同时保留显式 `_NT_SYMBOL_PROXY` 的优先级。
* 避免把 socks、带认证信息或带路径的通用代理值错误传给 SymSrv。

### D-013: TTD recording 使用独立 `trace.record_ttd` tool

决定：

第一版 TTD recording 通过独立 `trace.record_ttd` tool 编排 Microsoft `TTD.exe`，
支持 launch、attach 和有界 monitor recording。TTD recorder 目录来自
`config.toml` 的 `[tools].ttd_dir`、`[debugger].dbgeng_dir\ttd` 推导目录或
Windows `PATH`，不接受请求内传入 recorder 路径，不下载或安装 TTD，也不暴露任意
recorder command line。生成的 `.run`、`.out`、`.err` 和 `.idx` 文件写入
`artifacts\ttd_recordings\<recording_id>`，并按敏感 artifact 管理。

原因：

* TTD launch recording 必须由 `TTD.exe` 启动目标，不符合 `trace.record_profile`
  “先启动 collector、再由 dbgflow 启动 target”的生命周期模型。
* 独立 tool 能清晰表达 TTD 的 admin 权限、性能开销、文件大小和敏感数据边界。
* typed options 比透传命令行更容易审计，也避免把 dbgflow 扩展成通用 shell wrapper。

### D-014: MCP tool 名称采用 dot namespace

决定：

公开 MCP tool 名称采用 `dbg.*` 和 `trace.*` namespace。当前名称为
`dbg.create_session`、`dbg.get_session`、`dbg.list_sessions`、`dbg.close_session`、
`dbg.eval`、`dbg.add_symbols`、`trace.record_profile` 和 `trace.record_ttd`。
旧的 flat tool names 不再作为 alias 暴露。

`dbg.add_symbols` 只追加 symbol path，不提供替换语义；每个 session 的初始
symbol path 由 runtime config 负责。

原因：

* dot namespace 保持调试 MCP 语境清晰，同时符合 MCP tool name 推荐字符集。
* append-only symbol tool 避免意外覆盖安装时配置或空默认的全局符号路径。
* `trace.record_profile` 和 `trace.record_ttd` 都产出敏感 trace artifact，但保留
  独立 schema，避免把 ETW profiling 与 TTD recording 生命周期混合。

### D-015: Native ETW collector 使用 scope / event_sets / stacks schema

决定：

`native_etw` collector 不再使用 `system_overview` preset。公开 schema 改为
`scope`、`event_sets` 和 `stacks`。当前只支持 `scope.kind=target_process`、
`event_sets=["process", "file_io"]`，默认开启 stack capture。

`process` 采集侧启用 process、thread 和 image load kernel flags。
`file_io` 采集侧启用 disk/file name、file I/O init 和 op-end 相关 kernel flags，
覆盖常用 FileIo name/rundown、create、read、write、cleanup、flush、
delete、rename、query、set、directory enumeration/notification、file-system
control；close 事件在后处理中视为 cleanup 之后的冗余句柄关闭信号，不输出为独立
JSONL 行。原始 `.etl` 仍可能包含系统范围事件；PID 精确性由 ETL 后处理产物
`process.jsonl`、`file_io.jsonl` 和 `summary.json` 保证。文件路径通过直接路径字段
以及 `FileObject` / `FileKey` best-effort 关联解析；无法解析时保留 raw pointer
字段。stack 输出为紧凑字符串数组，解析成功时使用 `module+0xoffset`，未解析时保留
raw address；展示顺序接近 WinDbg stack，不做符号解析。默认文件 I/O 采集会增加 trace
体积和敏感路径暴露，可通过显式 `event_sets` 排除。

原因：

* 避免旧 `system_overview` 采集过宽，难以归因到目标进程。
* event set schema 能自然扩展到后续 disk I/O、registry、CPU sample 等能力。
* launch 场景 target PID 只有进程创建后才可靠获得，先做事件类别减法，再在后处理层
  严格按 target PID 过滤，可避免复杂 suspended launch 或平台受限 provider filter。

### D-016: Native ETW process 命名与 FileIo completion 合并

决定：

公开 event set `process_lifecycle` 破坏式改名为 `process`，不保留 alias。
Native ETW 默认 event sets 为 `["process", "file_io"]`；后处理 artifact、summary key
和事件 JSON `event_set` 同步使用 `process`，对应 artifact 为 `process.jsonl`。

`file_io.jsonl` 输出 begin-side I/O 事件。FileIo `OpEnd` completion 通过 `IrpPtr`
合并回对应 begin 事件，补充 `nt_status`、`extra_info`、`completion_pid`、
`completion_tid` 和 `completion_sequence`。未匹配的 OpEnd 不输出独立事件，只进入
summary 计数和 warning；未完成的 begin 事件仍保留原始字段。`close` 事件不输出，
其 `OpEnd` 只用于内部忽略匹配，避免产生无意义的 unmatched completion 噪声。

stack frame 在解析和统计后、写入 JSON 前反转，使展示顺序更接近 WinDbg `k`/`kb`。

原因：

* `process` 比 `process_lifecycle` 更短，且对用户而言足以表达该 event set。
* `file_io` 用户更需要一条完整 I/O 事件，而不是 begin / OpEnd 分散在两行。
* `completion_sequence` 保留 completion 的相对顺序，不提前承诺 duration 或 timestamp
  单位语义。
* WinDbg-style stack 顺序更符合调试人员阅读习惯。

### D-017: `ida.*` 作为 idalib 逆向 tool namespace

决定：

公开 MCP tool 名称新增 `ida.*` namespace。首版 reverse analysis 能力明确基于
IDA / idalib，公开名称直接表达后端语义，而不是抽象为 `rev.*` 或 `bin.*`。

原因：

* idalib 的函数、类型、xrefs、伪代码和 IDB 标注语义与 IDA 强绑定。
* 明确 namespace 能降低调用方对后端能力、授权和 artifact 敏感性的误解。
* 如果未来增加其他 reverse backend，可在内部保留 `ReverseBackend` 抽象，再单独设计
  兼容或迁移策略。

### D-018: idalib worker 采用 Rust-first runtime direct binding

决定：

idalib reverse worker 的首选实现路线是 Rust 主体开发，并通过 `libloading` 在运行时
直接加载官方 `idalib.dll` / `ida.dll` symbols。默认构建不依赖 IDA SDK headers、
C++ 编译器、bindgen、autocxx 或 cxx。Python idalib 可用于探索或验证 API 行为，
但不作为默认生产实现路线。

原因：

* Rust 主进程、配置、validation、artifacts、日志和 MCP facade 可沿用现有工程边界。
* direct binding 删除额外部署 native bridge DLL 的峰值，保持默认构建
  runtime-only；C++ ABI 风险集中在 `dbgflow-reverse::ida` 私有 wrapper 和 real IDA
  smoke test 中验证。
* 不把 Python 运行时、虚拟环境和模块搜索路径作为首版生产依赖，可以降低部署和服务化
  复杂度。

### D-019: reverse session 与 debug session 分离

决定：

静态逆向分析使用独立的 `ReverseSessionManager`、`ReverseTarget Validation`、
`ReverseBackend` 和 reverse worker，不复用 `DebugBackend` 或现有 debug session
状态机。

原因：

* 调试 session 面向运行态目标、debug event 和执行状态；reverse session 面向
  binary / IDB、自动分析状态、数据库变更和静态查询。
* 分离后可以保持 `dbg.*`、`trace.*` 和 `ida.*` 的行为边界清晰。
* IDB 标注修改需要独立审计和 artifact 策略，不能混入调试 transcript 语义。

### D-020: idalib 能力只暴露 typed reverse-analysis tools

决定：

`ida.*` 不提供任意 IDA 脚本、任意 OS 命令或通用 shell runner。所有能力通过
typed tools 暴露，例如 session lifecycle、list functions、disassemble、
decompile、list xrefs、rename 和 set comment。

原因：

* IDA 数据库、伪代码、字符串、路径和分析输出都可能包含敏感信息。
* 任意脚本执行会绕过 dbgflow 的 target validation、artifact 管理和审计边界。
* typed tools 更容易为 AI agent 提供稳定、可测试、可回放的逆向分析接口。

### D-021: workspace 按 common/debug/trace/reverse 分层

决定：

workspace 拆分为 `dbgflow-common`、`dbgflow-debug`、`dbgflow-trace`、
`dbgflow-reverse`、`dbgflow-core` 和 `dbgflow-mcp`。`dbgflow-core` 保留为
compatibility facade，继续 re-export 旧 Rust module 路径；`dbgflow-mcp` 直接依赖
领域 crate。`dbgflow-reverse` 暂只作为 crate 边界，不暴露 `ida.*` tool 或未定型
reverse API。

原因：

* debug session、trace recording 和未来 reverse analysis 的生命周期相似但语义不同，
  需要独立领域边界。
* artifacts、logging、proxy、typed ids、validation 和 single-active-job guard 属于
  跨领域基础设施，集中到 `dbgflow-common` 可避免未来 `ida.*` 复制现有 profile / TTD 模式。
* facade 保持现有 Rust 调用路径和测试兼容，让内部架构可以演进而不破坏 MCP wire API。

### D-022: IDA MVP 采用运行时动态绑定而非构建期 SDK 绑定

决定：

`ida.*` session MVP 使用 Rust worker 在运行时动态加载 `idalib.dll` 和 `ida.dll`，
手写最小 C ABI 绑定。dbgflow 源码编译不依赖 IDA SDK、Clang、bindgen、
`idalib-rs` 或 IDA 安装目录。首版只绑定 session 打开 / 关闭、IDA 版本查询、
segment 列表和 function 列表，不返回 qstring/name，不暴露 IDAPython、任意 eval、
decompile、xref、rename、comment 或 patch。

原因：

* 源码使用者即使没有 IDA/SDK/Clang 也应能编译和运行非 IDA 能力。
* IDA runtime 只在真实调用 `ida.create_session` 的机器上成为必要依赖。
* 最小只读 ABI 降低 C++ layout 和 IDA SDK 版本漂移风险，为后续增量能力保留空间。
* session worker 子进程隔离能在 IDA 加载失败、阻塞或崩溃时保护主 MCP server。

### D-023: 子进程默认使用 MCP loopback peer 所在交互会话

决定：

安装脚本生成的配置显式写入 `[process]`，默认
`child_identity = "mcp_peer_session"`、`fallback_child_identity = "active_interactive_session"`、
`elevate_if_admin = true`。该策略统一用于 debug session worker、IDA reverse worker、
DbgEng launch target、profile launch target 和 TTD recorder process。未配置 `[process]`
的旧配置继续使用 `current_process`，避免改变既有部署行为。

HTTP `/mcp` 当前没有 bearer token、Windows Integrated Auth、named-pipe impersonation
或其他强客户端身份。dbgflow 只在本机 loopback 场景中通过 TCP owner table 推断 peer
PID，再用 `ProcessIdToSessionId` 得到 peer session id；如果解析失败，则按配置回退到
active interactive session，再失败则回退当前进程并记录 warning。若目标用户 token 有
linked elevated token 且配置允许，则优先使用 elevated token。

原因：

* Windows service 默认 LocalSystem 时，直接用服务身份启动 IDA/TTD/debuggee 容易遇到
  桌面会话、license、UAC 和用户环境不匹配。
* loopback peer PID/session 不是强认证身份，但足以覆盖当前可信本机 MCP bridge 场景，
  且所有解析结果、fallback reason 和 elevated 状态都会进入审计日志。
* 将 launcher 放入 `dbgflow-common` 可避免 debug / trace / reverse 各自重复实现
  token、stdio、artifact 输出和审计逻辑。

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
* [x] 将 workspace 重分层为 common / debug / trace / reverse / core facade / MCP。

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
* [x] 支持安装配置中的初始 DbgEng symbol path，并通过 DbgEng symbols API 应用。
* [x] 将 Windows service 交互式安装旅程、依赖探测、提权和 service 环境写入收敛到主程序。
* [x] 支持安装配置写入统一子进程身份策略，并在运行时解析 `[process]`。
* [x] 移除依赖提权或本机 live 调试环境的 ignored 测试，真实环境验证改用受控 smoke。
* [x] 补齐 `transcript.log` 和 `events.jsonl` 审计链路。

### P3

* [x] 明确本地 HTTP 不使用 token / auth，依赖 loopback-only bind、Origin 限制和可信本机环境。
* [x] 支持 Streamable HTTP SSE stream。
* [x] 支持 native ETW launch-only `trace.record_profile` MVP。
* [x] 删除可选 Procmon collector，record profile 收敛为 ETW-first。
* [x] 支持 TTD recording。
* [x] 支持 TTD trace artifact。
* [x] 支持 native ETW process lifecycle 采集、target PID 后处理过滤和模块偏移 stack 输出。
* [ ] 验证 `.run` trace 是否可通过现有 DbgEng file target 打开，并补充 smoke 测试与文档。
* [ ] 支持 debugger-gated profiling。
* [ ] 支持更完整 ETL 后处理和 profiling 报告生成。
* [ ] 支持调试报告生成。

### P4

* [x] 完成 IDA runtime 探测和 `[reverse.ida] install_dir` 配置设计。
* [x] 安装脚本探测常见 IDA 安装目录，并在有效时写入 `[reverse.ida].install_dir`。
* [x] 定义 `ReverseSessionManager` 和 reverse IDA worker 协议。
* [x] 实现 Rust-first、no-SDK build-time dependency 的 IDA dynamic binding worker MVP。
* [x] 设计 `ida.*` MCP tool schema 和 session lifecycle。
* [x] 支持 headless binary / IDB analysis session MVP。
* [x] 支持只读 segment / function 列表 MVP；direct rich binding 可返回 name / segment 等增强字段。
* [x] 暴露函数、反汇编、字符串、imports / exports、xrefs、类型和伪代码 typed tool surface，并通过 Rust direct binding dispatch；移除未验证的一函数一块控制流占位工具。
* [x] 暴露受控 IDB 标注修改 tool surface：rename、comment 和 type，默认 close 时保存。
* [x] 支持 reverse query / mutation artifacts、审计日志和敏感输出记录。
* [x] reverse outputs 使用唯一 artifact 文件名，避免重复查询或修改覆盖旧审计结果。
* [ ] 继续强化 Rust direct IDA binding 的版本升级流程，并在未来补齐 Hex-Rays dispatcher；qstring / xrefblk runtime validation 已完成 MVP。
* [ ] 增强 close/save 结果来源；基础 idalib ABI 只能记录保存请求和 unknown 结果。
* [ ] 支持后续 redaction 策略。

## 9. 近期开发计划

下一轮建议执行顺序：

```text
1. 统一文档和 tool 描述口径：`dbg.eval` + WinDbg / DbgEng 原生命令是调试主入口
2. 验证 `.run` trace 打开路径，明确 TTD 分析通过 WinDbg TTD 命令完成还是需要极薄 helper
3. 完成 idalib / IDA SDK 架构设计、runtime 配置和安全边界设计
4. 实现 ReverseBackend / ReverseSessionManager / IdaLibBackend worker spike
5. 设计 `ida.*` tool schema、artifacts 和审计格式
6. 支持 headless reverse session MVP：加载 binary / IDB 并查询函数、反汇编、xrefs 和伪代码
7. 支持受控 IDB 标注修改 MVP：rename、comment 和 type
8. 继续增强 target/path validation、redaction、报告消费格式和真实环境验证
```

优先保持架构清晰，而不是过早追求完整调试能力。
