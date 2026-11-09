# 当前架构

> 状态：Current ｜ 受众：贡献者与维护者 ｜ 权威范围：当前已实现的系统边界、数据流与故障语义 ｜ 代码基线：`a34c13b` ｜ 最近核对：2026-11-08

## 系统边界

KVBA 是本地常驻的项目认知辅助工具，不是 Agent 编排器。它接收 Agent 暴露的事件，在可用的回合边界形成证据化洞察，再通过托盘和悬浮胶囊交付。

明确的非目标：

- 不接管 Agent 执行，不因分析或上报失败阻塞 Agent。
- 不把洞察当作测试、静态分析或完整代码评审的替代品。
- 不在证据不足时把共享工作区变化归因给单个 Agent。
- 不把胶囊 UI 当作领域核心；它只是一个投递端。

方向决策见 [RFC-0003](rfcs/RFC-0003-workunit-cognition-ledger.md)。

## 当前数据流

```text
Agent signal
  → NormalizedEvent (kvba.event.v1)
  → events.log + in-memory event window
  → turn/session boundary
  → WorkUnit + ChangeSet + TestResult
  → deterministic Insight[]
  → cognition.json
  → primary Insight projection
  → 队列 / 胶囊
```

### 1. 事件摄入

适配器向 `POST /kvba/events` 发送 `NormalizedEvent`。服务端负责：

- 校验事件版本、类型、项目路径和必要字段。
- 按事件 ID 与 5 秒内容指纹做短窗判重。
- 将每项目最近 200 条事件保存在内存并重写 `events.log`。
- 在边界事件到达时触发自动工作流。

本地 HTTP 仅绑定 `127.0.0.1`，端口和鉴权令牌每次启动随机化。

### 2. WorkUnit 聚合

`turn.ended` 和满足条件的 `session.ended` 会按摄入顺序收口当前窗口。聚合器生成：

- 会话、目标、时间范围和来源事件。
- 变更集（`ChangeSet`）及作用域。
- 从命令文本识别的测试记录。

缺少边界的 Claude JSONL watcher 不进入这条链路，继续走旧门控。

### 3. 证据边界

Git 证据是“边界到达时，工作区相对 `HEAD` 的快照”，不是“本回合开始到结束的差异”：

- 能描述当时可见的未提交变化。
- 看不到本回合内已经提交后又变干净的变化。
- 共享工作区中的变化不能可靠归因给当前 Agent。
- Git 采集失败或工作区干净时，可退回 Agent 声明的文件证据，并显式保留不完整状态。

测试证据现在有两条路径：

- `kvba.event.v1` 的可选 `execution` 字段携带执行 ID、命令、cwd、起止时间、退出码、结果来源与输出定位。每条执行独立保留，按执行 ID 去重。
- 只有来源可信（`transcript` / `adapter`）且退出码存在时，`TestResult.status` 才会是 `Passed` / `Failed`；缺失来源或退出码时保持 `Unknown`。
- 没有 `execution` 的旧事件仍按命令文本识别测试，状态为 `Unknown`。当前 Qoder 适配器提供该字段，其余来源显式保持未知。

### 4. Insight 生成

确定性分析器从一个已完成的 `WorkUnit` 生成 `explain`、`test-gap`、`risk` 或 `decision` 洞察。每条洞察都包含：

- 结论。
- 可定位证据。
- 重要性说明。
- 建议动作。
- 风险级别与置信度。

当前生产分析器只会生成 Info 或 P2。队列已经实现“P0 或 P1 且置信度至少 0.85 才主动展开”的策略，但自动链路尚无可产生 P0 或 P1 的来源。

主洞察先按风险级别 `P0 → P1 → P2 → Info` 排序，同级再按类型 `risk → decision → test-gap → explain → question` 选择。前端只负责展示完整领域数据，不重新推理。

### 5. 投递与兼容路径

每个 WorkUnit 只选择一条主洞察投影为卡片，进入最多 3 项的队列；其余洞察只保存在 `cognition.json`。用户可以跳过卡片或隐藏胶囊；只有达到主动展开条件的主洞察才允许展开胶囊。

旧 `gate/` 决策树仍用于：

- 手动“来一道”。
- 知识与学习类卡片。
- 缺少回合边界的 Claude JSONL watcher。
- 生成兼容旧卡片协议的决策结果。

它是兼容路径，不是 `WorkUnit` 与 `Insight` 的领域核心。

## 持久化与恢复

每项目主要文件：

| 文件 | 语义 |
|---|---|
| `events.log` | 最近 200 条事件的 JSONL 窗口 |
| `facts.json` | 旧记忆层的项目事实 |
| `cognition.json` | 同一版本的 `WorkUnit[]` 与 `Insight[]` |

`cognition.json` 使用临时文件、刷新和同目录替换完成原子更新。Windows 使用带替换与写穿语义的文件移动，并对短暂占用做有限重试。

恢复规则：

1. `cognition.json` 有效时直接加载；`events.log` 缺失不会阻止认知恢复。
2. `cognition.json` 不存在，且旧 `work-units.json`、`insights.json` 均完整有效时，生成新快照并保留旧文件。
3. 只存在一份旧文件时，恢复有效侧到内存并记录警告，但不立即生成迁移快照。
4. 任一旧文件无法解析，或 `cognition.json` 损坏、版本不支持、项目键不匹配时，记录错误并阻止后续覆盖；本次新结果只保留在内存，等待人工备份和修复。

这组规则优先保证“不把可恢复数据静默覆盖为空”。

## 组件映射

| 目录/文件 | 职责 |
|---|---|
| `src-tauri/src/events/` | 事件协议、校验、判重和滚动日志 |
| `src-tauri/src/workunit/` | 回合窗口、Git 采集、测试命令与工作单元 |
| `src-tauri/src/insight/` | 洞察生成、排序与主洞察选择 |
| `src-tauri/src/cognition_store.rs` | 认知快照迁移、原子写入与恢复 |
| `src-tauri/src/workflow.rs` | 自动链路编排 |
| `src-tauri/src/gate/` | 旧卡片门控兼容路径 |
| `src-tauri/src/queue/` | 卡片投递、冷却、过期与反馈 |
| `src-tauri/src/memory/` | `facts.json` 与会话记忆 |
| `src-tauri/src/llm/` | 模型提供商、预算、提示词与契约解析 |
| `src-tauri/src/http/` | 本地 HTTP、握手和命令接口 |
| `src/` | Vue 3 胶囊、设置、历史和决策日志 |
| `adapters/` | 各 Agent 的事件归一化与上报 |

## 安全、隐私与降级

- 模型密钥用 AES-256-GCM 加密后持久化；加密密钥由机器标识、随机盐和 HKDF-SHA256 派生。
- 启用 LLM 时，分析上下文与 API Key 会发送给用户配置的模型提供商。KVBA 不提供“完全离线但仍调用云模型”的保证。
- `WorkUnit` 与 `Insight` 自动链路不依赖 LLM。旧卡片链路未配置模型时可使用模板；预算耗尽时进入 `batch.json`，提供商请求失败时拒绝本次出稿，只有契约解析失败才回填模板文案。
- 当前事件日志按条数而非总字节限制；最多 200 条长事件仍可能让单文件异常偏大。

会变化的实现进度与缺陷统一维护在[当前状态](status.md)。
