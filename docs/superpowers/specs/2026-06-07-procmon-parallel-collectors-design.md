# Procmon Parallel Collectors Design

日期：2026-06-07

## 背景

`run_profile` 现有 `native_etw/system_overview` collector 适合系统概览：进程生命周期、线程、镜像加载、CPU sampling、context switch、磁盘概况和文件名 rundown。实际验证中，目标进程读取 `large_input.bin` 的行为可以通过 stdout、进程命令行和 `FileIo FileRundown` 交叉确认，但没有稳定产生可归因到该文件的直接 `FileIo Read` 或 `DiskIo Read` 事件。

这不是 native ETW collector 的 stop/trace 完整性问题，而是 `system_overview` 的语义边界：它不是精确文件 I/O 审计 collector。若要记录更准确的文件/注册表 I/O 操作、结果和调用栈，应接入 Sysinternals Process Monitor 作为可选 collector，并允许它与 native ETW 在同一次 profile 中并行采集。

## 目标

- `run_profile` 支持多个 collector 并行运行。
- 保持旧版单数 `collector` 输入兼容，并内部归一化为 `collectors[]`。
- 新增 `procmon` collector，用于精确文件/注册表 I/O 事件和可选堆栈采集。
- Procmon 只从主服务配置的 `--sysinternals-dir` 中派生，不暴露单独 `procmon_path`。
- 安装脚本交互式识别 Sysinternals 目录；未配置时，依赖 Sysinternals 的能力不可用。
- 所有 collector artifact 均写入受控 profile artifact 目录。
- MCP handler 保持薄层，collector 编排和 Procmon 逻辑放在 `dbgflow-core`。

## 非目标

- 不自动下载 Sysinternals 工具。
- 不扫描全盘寻找 Procmon。
- 不从用户级环境变量隐式读取 Procmon 路径。
- 不支持 `--procmon-path`。
- 不允许 `run_profile` 请求中传入 Procmon exe 路径。
- 不把 Procmon 设为默认 collector。
- 不在第一版实现复杂 Procmon 过滤 DSL。
- 不保证 Procmon 栈符号在所有环境都已完整解析。

## Runtime 配置

主服务入口新增可选参数：

```text
dbgflow-mcp http --bind 127.0.0.1:7331 --data-dir .\var --sysinternals-dir C:\Tools\Sysinternals
dbgflow-mcp service run --bind 127.0.0.1:7331 --data-dir C:\dbgflow\var --sysinternals-dir C:\Tools\Sysinternals
```

参数语义：

- `--sysinternals-dir <dir>`：配置 Sysinternals 工具目录。
- 目录必须存在且必须是目录。
- `procmon` collector 从该目录内部派生 Procmon exe 路径。
- 优先查找 `Procmon64.exe`，其次查找 `Procmon.exe`。
- 如果目录存在但找不到 Procmon，服务仍可启动，但 `procmon` collector 不可用，并应在 runtime capability 中记录 warning。
- 未传 `--sysinternals-dir` 时，所有依赖 Sysinternals 的 collector/tool 均不可用。

Procmon 路径不接受其他来源。这个限制让服务行为可审计，也避免 service 账号和交互式用户环境不一致。

## 安装脚本

`scripts/install-service.ps1` 增加交互式 Sysinternals 检测。

脚本流程：

1. 接受可选参数 `-SysinternalsDir <dir>`。
2. 如果用户显式传入，校验目录存在，并检查其中是否包含 `Procmon64.exe` 或 `Procmon.exe`。
3. 如果用户未传，自动检查常见目录，例如：
   - 当前仓库相邻或工具目录中的 `Sysinternals`
   - `C:\Tools\Sysinternals`
   - `C:\Sysinternals`
   - `C:\Program Files\Sysinternals`
4. 如果识别到候选目录，交互式询问是否写入 service command line 的 `--sysinternals-dir`。
5. 如果未识别到或用户拒绝，继续安装服务，但不配置 `--sysinternals-dir`。

安装脚本不下载 Procmon，不修改用户环境变量，不写入单独 Procmon 路径。

## Tool API

新增复数 collector 输入：

```json
{
  "target": {
    "kind": "launch",
    "executable": "C:\\app\\app.exe",
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

旧输入保持兼容：

```json
{
  "collector": {
    "kind": "native_etw",
    "preset": "system_overview"
  }
}
```

归一化规则：

- 只传 `collector`：转换为单元素 `collectors[]`。
- 只传 `collectors`：按数组执行。
- 同时传 `collector` 和 `collectors`：拒绝，避免歧义。
- 未传 collector：使用默认 `native_etw/system_overview`。
- `collectors[]` 为空：拒绝。

## Procmon Collector

`procmon` collector 第一版支持：

- launch target profile 场景。
- 文件 I/O 操作过滤。
- 可选 registry I/O 操作过滤。
- 可选栈采集开关 `capture_stacks`。
- 保存原始 `.pml`。
- 导出可读事件摘要 artifact。

输入字段：

```text
kind: "procmon"
capture_stacks: bool
filters.operations: string[]
filters.paths: string[]
```

第一版过滤保持简单：

- `operations` 只接受 Procmon 已知操作名，例如 `CreateFile`、`ReadFile`、`WriteFile`、`CloseFile`、`RegOpenKey`。
- `paths` 是精确路径或前缀匹配路径。
- 运行时仍必须按目标 PID post-filter，避免把系统级采集结果误归因给目标。

## 并行生命周期

`run_profile` 生命周期调整为：

```text
create profile artifact directory
resolve and validate collectors
start all collectors
launch target process
wait for target exit or timeout_ms
stop all collectors
export collector summaries
write profile.json and events.jsonl
return profile result
```

启动顺序建议：

1. 先启动 `procmon` 等外部 collector。
2. 再启动 `native_etw`。
3. 所有 collector 确认开始后启动目标进程。

停止顺序建议与启动顺序相反，尽量覆盖目标退出尾部事件。

同一次 profile 内 collector 并行采集，但同一个 profile 的 start/stop/export 操作必须由 profile manager 统一串行编排，避免 artifact 和状态竞争。

## Artifacts

profile artifact 目录调整为：

```text
artifacts/
  profiles/
    <profile_id>/
      profile.json
      events.jsonl
      target/
        stdout.txt
        stderr.txt
      collectors/
        native_etw/
          trace.etl
          summary.json
        procmon/
          capture.pml
          events.csv
          events.jsonl
          summary.json
```

`profile.json` 汇总：

```text
profile_id
target metadata
target_pid
completion_reason
target_exit_code
duration_ms
collector_results[]
warnings
```

每个 `collector_results[]` 包含：

```text
kind
status
start_time
end_time
artifacts
warnings
error
```

Procmon artifact 视为敏感调试数据，必须位于受控 data-dir，不能写入 Sysinternals 目录或用户目录。

## 错误处理

collector 解析失败：

- 请求 `procmon` 但未配置 `--sysinternals-dir`：返回明确错误，目标不启动。
- 配置了 `--sysinternals-dir` 但找不到 Procmon：返回明确错误，目标不启动。
- `collectors[]` 中存在未知 collector：返回 schema/validation error。

采集中失败：

- target 启动前 collector start 失败：停止已启动 collector，返回 failed profile。
- target 已启动后某 collector stop/export 失败：保留其他 collector 结果，并在 `collector_results[]` 与 `warnings` 中记录。
- target 启动失败：停止所有已启动 collector，返回 failed profile 和 artifact 引用。
- timeout：停止所有 collector，返回 timed_out，不默认终止 target。

结果状态先沿用现有 profile 状态：

```text
completed
timed_out
failed
```

collector 级别错误通过 `collector_results[]` 表达，不先新增 `completed_with_errors`。

## 安全

- Procmon 是系统级采集工具，默认不启用。
- 只有显式配置 `--sysinternals-dir` 后才能请求 `procmon`。
- 不自动接受未知下载来源。
- 不将 Procmon 命令行暴露为通用 shell 执行能力。
- 所有 Procmon 输出写入受控 artifact 目录。
- 返回结果不内联大体积事件流，只返回摘要和 artifact 引用。
- 堆栈、路径、模块名、注册表键均视为敏感数据。

## 测试

核心单元测试：

- `collector` 单数输入归一化为 `collectors[]`。
- `collectors[]` 多 collector 输入被接受。
- 同时传 `collector` 和 `collectors` 被拒绝。
- 空 `collectors[]` 被拒绝。
- 未传 collector 时使用默认 `native_etw/system_overview`。
- 未配置 `--sysinternals-dir` 时 `procmon` collector 不可用。
- `--sysinternals-dir` 只接受存在的目录。
- Procmon 路径只从 Sysinternals 目录派生，优先 `Procmon64.exe`。
- collector start 失败会停止已启动 collector。
- target 启动失败会停止所有已启动 collector。

安装脚本测试：

- 显式 `-SysinternalsDir` 会写入 service args。
- 未配置 Sysinternals 时 service 仍可安装。
- 安装脚本不生成 `--procmon-path`。
- 无 Procmon 的目录不会被当作可用 Procmon 配置。

真实 Procmon 集成测试：

- Windows-only。
- 需要本机 Sysinternals 目录和管理员/service 权限。
- 默认 ignored。
- 验证目标读取文件时，Procmon artifact 中可找到目标 PID、路径、操作名和可选栈信息。

## 文档

实现后更新：

- `README.md`
- `README.zh-CN.md`
- `GOALS.md`

文档应说明：

- `run_profile` 支持 `collectors[]` 并行采集。
- `collector` 单数格式仅为兼容入口。
- `native_etw/system_overview` 是默认概览 collector。
- `procmon` 是显式启用的精确 I/O collector。
- Procmon 依赖服务启动参数 `--sysinternals-dir`。
- 安装脚本会交互式识别 Sysinternals 目录。
- 未配置 Sysinternals 时，依赖 Sysinternals 的能力不可用。

## 开放假设

- 第一版 Procmon collector 只支持 launch profile，不支持 attach。
- Procmon 命令行能力足以完成 start、stop、保存 PML 和导出摘要。
- 栈采集可能受系统配置、符号路径和 Procmon 版本影响；第一版只保证保存可用 artifact，不保证所有事件都有完整符号化栈。
- 多 collector 并行采集的 target 生命周期由现有 profile manager 扩展，不新增独立 profile session tool。
