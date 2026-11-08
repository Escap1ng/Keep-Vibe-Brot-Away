# KVBA（Keep-Vibe-Brot-Away）

KVBA 是一个本地优先的跨 Coding Agent 项目认知层。它在 Agent 回合结束后，将工具事件、Git 变更和测试命令聚合为工作单元（`WorkUnit`），再生成带证据的洞察（`Insight`）。

KVBA 不接管 Agent，也不会因分析失败阻塞开发流程。每个 `WorkUnit` 只把一条主洞察投影为队列卡片；交付策略只允许高置信度的 P0 或 P1 风险主动展开胶囊。当前确定性分析器只生成 Info 或 P2，尚不能自动展开高风险洞察，详情见[当前状态](docs/status.md)。

项目数据和模型密钥持久化在本机。启用 LLM 后，KVBA 会把分析所需的上下文和鉴权信息发送给用户配置的模型提供商；未配置 LLM 时，确定性分析与模板卡片仍可工作。

- 想立即运行：从[快速开始](#快速开始)开始。
- 想接入 Agent：阅读[适配器指南](docs/adapters.md)。
- 准备贡献代码：先读[当前架构](docs/architecture.md)和[工程指南](docs/engineering.md)。

## 当前成熟度

当前源码清单中的版本号为 `0.2.0`。项目已完成 `Event → WorkUnit → Insight → Delivery` 的首个纵向切片，但还不是完整的“项目认知层”，源码版本号也不等同于已发布二进制的版本：

| 能力 | 当前状态 |
|---|---|
| 本地事件摄入 | 已实现；随机端口与鉴权令牌仅绑定 `127.0.0.1` |
| 回合聚合 | 已实现；在 `turn.ended` 或有效的 `session.ended` 收口 |
| Git 证据 | 已实现回合结束时相对 `HEAD` 的工作区快照；不能精确归因本回合 |
| 测试证据 | 能识别测试命令，但事件协议没有可靠的执行结果，状态记为 `Unknown` |
| 洞察 | 已实现带结论、证据、建议动作、风险级别和置信度的结构化输出 |
| 高风险主动提醒 | 主动展开策略已实现；生产分析器尚不生成 P0 或 P1，因此当前不会自动触发 |
| 认知持久化 | `WorkUnit` 与 `Insight` 统一写入原子快照 `cognition.json` |

完整限制与后续方向见[当前状态](docs/status.md)。产品方向与阶段决策见 [RFC-0003](docs/rfcs/RFC-0003-workunit-cognition-ledger.md)。

## 快速开始

### 环境

- Node.js 20（与 CI 一致）。
- 当前稳定版 Rust 工具链。
- Tauri 2 对应平台的系统依赖；Windows 还需要 WebView2。

Windows 是当前主要验证平台。Linux 与 macOS 会在 CI 中完成编译、Clippy 和单元测试，但桌面窗口行为及适配器端点发现仍需实机验证。

### 开发模式

```powershell
npm install
npm run tauri dev
```

也可以在 Windows 上运行 `dev.cmd`。

### 构建安装包

```powershell
npm run tauri build
```

Windows 只构建 NSIS 安装包：

```powershell
npm run tauri build -- --bundles nsis
```

### 配置 LLM（可选）

启动后右键胶囊，选择“打开设置…”，再填写模型配置：

| 设置 | 说明 |
|---|---|
| 模型提供商 | `openai`、`google` 或 `anthropic` |
| 模型名 | 由对应提供商支持的模型标识 |
| Base URL | 可选，用于 OpenAI 兼容服务或代理 |
| API Key | 加密后持久化在本机；调用时作为鉴权凭据发送给所选提供商 |
| 每日调用上限 | 默认 80 次；旧卡片链路耗尽后把已通过门控的上下文写入批处理，等待额度恢复 |

设置面板中的“出稿来源”会分别统计本地证据、LLM 与模板兜底。只有其中的 LLM 计数能证明当前实例实际走过模型出稿链路。

## 接入 Agent

| Agent | 推荐入口 |
|---|---|
| opencode | TypeScript 插件 |
| Claude Code | Node 钩子；Rust bridge 与 JSONL watcher 为可选方案 |
| Codex | `notify` 脚本 |
| Trae | Node 钩子 |

各适配器能看到的信号并不相同。安装、能力矩阵、验证和故障排查统一维护在[适配器指南](docs/adapters.md)。适配器都从 KVBA 运行时写出的 `endpoint.json` 发现本地端点；上报失败会重试，最终丢弃本次事件，但不会阻塞 Agent。

## 工作方式

```text
Coding Agent
  → 适配器上报归一化事件
  → KVBA 在回合边界聚合 WorkUnit
  → Git 快照与测试命令形成证据
  → 确定性分析器生成 Insight
  → 主洞察投影为队列卡片；其余洞察只保留在认知账本
```

用户可以在胶囊中查看或跳过主洞察投影。旧链路的审查卡在用户反馈“不满意”时会生成下一轮优化提示词，写入剪贴板及项目下的 `.kvba/next-prompt.md`；新的 Insight 投影尚未接入这条动作闭环。队列最多保留 3 张卡片；暂停、隐藏胶囊、打开设置和退出均可通过胶囊或托盘菜单完成。

旧门控仍服务于手动“来一道”、知识卡片，以及缺少回合边界的 Claude JSONL watcher。它是兼容路径，不再是新架构的领域核心。

## 架构摘要

```text
adapters/
  └─ POST /kvba/events（本机回环 + 鉴权）
src-tauri/src/
  ├─ events/       事件校验、判重与滚动日志
  ├─ workunit/     回合聚合与 Git/测试证据
  ├─ insight/      确定性洞察生成与主洞察选择
  ├─ cognition_store.rs  WorkUnit 与 Insight 原子快照
  ├─ workflow.rs   自动链路编排
  └─ gate/queue/llm/memory/ 兼容门控、投递、模型与记忆
src/
  └─ Vue 3 胶囊、设置、历史与决策日志
```

设计边界、故障语义和源码映射见[当前架构](docs/architecture.md)。

## 本地数据与隐私

Windows 默认数据目录为 `%APPDATA%\dev.kvba.app`：

| 文件 | 用途 |
|---|---|
| `endpoint.json` | 当前进程的端口与鉴权信息 |
| `settings.json` / `secrets.enc` | 普通设置与加密后的模型密钥 |
| `queue.json` / `history.json` | 待消费队列与历史记录 |
| `cooldown.json` / `budget.json` / `stats.json` | 节奏、预算与运行统计 |
| `decision-log.jsonl` | 旧门控决策日志 |
| `projects/<project-key>/events.log` | 最近 200 条归一化事件 |
| `projects/<project-key>/facts.json` | 兼容记忆层的项目事实 |
| `projects/<project-key>/cognition.json` | `WorkUnit` 与 `Insight` 的单一原子快照 |

`cognition.json` 不存在时，KVBA 按以下规则读取旧数据：

1. `work-units.json` 与 `insights.json` 都有效时，立即生成新快照。
2. 只存在一份时，把有效侧恢复到内存，但不立即迁移。
3. 任一旧文件无法解析，或现有 `cognition.json` 损坏时，阻止后续覆盖。

旧文件始终保留，给人工恢复留出机会。

Windows 日志位于 `%LOCALAPPDATA%\dev.kvba.app\logs\KVBA.log`。Linux/macOS 的实际目录以 Tauri 返回的应用数据目录和日志目录为准；当前适配器在 macOS 上的默认发现路径存在不一致，见[当前状态](docs/status.md)。

## 开发与文档

```powershell
npm run build
Push-Location src-tauri
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
Pop-Location
```

完整 CI、适配器验证和发布规则见[工程指南](docs/engineering.md)。

| 文档 | 作用 |
|---|---|
| [文档导航](docs/README.md) | 文档职责、权威层级与阅读路径 |
| [适配器指南](docs/adapters.md) | 安装、验证、故障排查和能力矩阵 |
| [当前架构](docs/architecture.md) | 已实现的数据流、存储和边界 |
| [当前状态](docs/status.md) | 已实现能力、已知限制与下一步 |
| [工程指南](docs/engineering.md) | 贡献规则、检查命令、CI 与发布 |
| [RFC-0003](docs/rfcs/RFC-0003-workunit-cognition-ledger.md) | WorkUnit 中心架构的方向决策 |
