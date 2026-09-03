# KVBA 项目交接报告

> 生成日期：2026-09-03 ｜ 基于代码库实际状态（git log `9fd097f`，工作区干净）
> **新成员必读顺序**：本文档 → [README.md](../README.md) → [docs/复盘与工程约定.md](复盘与工程约定.md) → [docs/InitPrompt.md](InitPrompt.md)

---

## 1. 项目背景

**KVBA（Keep-Vibe-Brot-Away，「项目伴读」）** 是一个 Tauri 2 桌面常驻工具：在开发者与 Coding Agent（Claude Code / opencode / Trae）协作的过程中，利用等待间隙递知识卡片、抛思考问题。核心设计哲学是**防腐拒侵入**——它不接管 Agent 的执行，只接管间隙；失败静默降级，永不阻塞 Agent。

## 2. 架构设计

```
CodingAgent 环境（Claude Code hooks/Node 钩子/JSONL watcher、opencode 插件、Trae hooks、Codex notify）
        │  统一「归一化事件」→ POST http://127.0.0.1:<随机端口>/kvba/events
        │  （握手 token 鉴权，仅本机回环）
        ▼
KVBA 主程序（Tauri 2，无主窗口：托盘 + 48×48 悬浮胶囊圆点）
  Rust 核心：events → queue → gate → memory → llm → http/crypto
        ▼
TS 前端（Vue 3）：胶囊状态机 idle/active/popup + 设置/历史/决策日志面板
```

**核心链路**：适配器摄入事件 → 事件队列（`MAX_PENDING = 3`）→ **门控决策树**（G1~G7，全部 0 token 纯函数）→ 通过者才调用 LLM 出稿 → 胶囊弹卡。每次门控评估均写入决策日志，可在面板中查看层级与拒绝原因。

## 3. 技术栈

| 层 | 技术 |
|---|---|
| 桌面框架 | Tauri 2（托盘、自启动、通知、剪贴板插件） |
| 后端 | Rust（Axum 本地 HTTP、Reqwest + SSE 调 LLM、AES-256-GCM 加密、tauri-plugin-log） |
| 前端 | Vue 3 + TypeScript + Vite 6（vue-tsc 类型检查） |
| 适配器 | Rust（claude-code 桥）、TS（opencode 插件）、Node（trae-hook） |
| CI/CD | GitHub Actions：三平台矩阵 CI（fmt/clippy `-D warnings` 阻断门禁）+ tag 触发三平台打包发布 |

## 4. 核心功能模块

| 模块 | 位置 | 职责 |
|---|---|---|
| 门控决策树 | `src-tauri/src/gate/` | G1 队列硬闸 → G2 频控(25s)/主题冷却(10min) → G3 活性 → G4 分支路由 → G5 信号评分(阈值40，只统计写事件) → G6 正文去重(20min 窗口) → G7 LLM 放行。25 项门控夹具单测（全库 67 项） |
| 事件摄入 | `src-tauri/src/events/` + `adapters/` | 三种 Agent 的适配器统一上报；服务端按内容指纹 5s 窗口判重（双路同报场景） |
| 记忆层 | `src-tauri/src/memory/` | 项目级 facts（扫描 package.json/README，一次 LLM 生成 ≤50KB）+ 对话滚动窗口（3 条 ≤3KB，零 LLM） |
| LLM 分析器 | `src-tauri/src/llm/` | providers（SSE 解析）、prompt、budget（每日额度）。**不下发 max_tokens**（Anthropic 除外 `ANTHROPIC_REQUIRED_MAX=8192`），输入侧 `fit_tokens` ≤3.5k 控制 |
| 加密存储 | `src-tauri/src/crypto/` | AES-256-GCM，密钥 = HKDF-SHA256(机器指纹)，API Key 永不离开本机 |
| 队列/HTTP/窗口/托盘 | `queue/` `http/` `windows.rs` `tray.rs` | 队列消费、本机端点、胶囊窗口、托盘驻留 |
| 前端 | `src/components/` | CapsuleDot（圆点状态机）、CapsulePanel（卡片面板）、SettingsView（设置/历史/决策日志） |
| 适配器 | `adapters/` | claude-code（Rust bridge+watcher）、opencode-plugin（TS）、trae-hook（Node mjs） |

**特色机制**：会话开场破冰（每会话首评放行一张知识贴士，旁路频控/冷却/G6，不旁路队列/静默期）；静默期 1h 是用户唯一否决权，永不放宽。

## 5. 开发规范

- **改本仓库前必读 [docs/复盘与工程约定.md](复盘与工程约定.md)**，其中「铁律」优先于任何新特性。关键几条：
  - 每次保存都是部署：改跨文件签名时一次性把签名 + 全部调用点 + 测试写入同一次编辑；落盘前先 `cargo check`。
  - 静默吞错禁止：诊断必须可证伪（二值假诊断已造成过重大事故）。
  - 否定性断言（"肯定没问题/从没运行过"）最危险，结论必须分级：静态审计 < 单测 < 实测。
  - 改完关键契约（字段名、命令注册、serde 别名）必须 `grep` 复验。
- CI 门禁：`cargo fmt --check`、`cargo clippy -- -D warnings`、`vue-tsc`、`cargo test --locked` 全部阻断。
- opencode 插件源文件在 `adapters/opencode-plugin/kvba-plugin.ts`，`.opencode/plugins/` 是部署副本，**CI 校验两者一致**，改一处必须同步另一处。
- 提交信息风格：`feat:` / `fix:` / `refactor:` / `docs:` / `chore:`（见 git log）。

## 6. 当前进度（v0.1.0）

- 里程碑主体完成：事件摄入、队列、七层门控、记忆层、LLM 出稿、加密存储、胶囊 UI、三平台 CI/发布流水线均已落地，全库 70 项单测绿。
- 节奏参数已于 2026-09-03 按实测放宽（频控 25s、主题冷却 10min、去重窗口 20min），并新增开场破冰。
- 适配器矩阵已扩至四种宿主（opencode / Claude Code×2 / Trae / Codex），Node 钩子均零编译、静默失败、重试 3 次后丢弃。
- **端到端 LLM 出稿尚未最终验证闭环**（详见 §7）。

## 7. 已知问题及解决方案

按 [复盘与工程约定.md §八](复盘与工程约定.md) 优先级列出：

1. **端到端出稿验证未完成（最高优先）**：SSE 解析（`sse_lines`）修复后仅有单测证明。验证方法：跑 `npm run tauri dev`，新会话开始 30s 内 `decision-log.jsonl` 应出现 `G7 pass=true …(LLM 出稿)`，`stats.json` 应变为带 `cardsLlm` 的新结构。若 `cardsTemplate > 0` 说明传输层又坏，回查复盘 §三，**不要加阈值**。
2. **`parse_contract`（卡片路径）仍返回 `Option`**：截断与类型不符两种失败不可区分。下次动 `llm/mod.rs` 时对齐 facts 路径的两态错误。
3. **`events.log` 无字节上限**：滚动只按条数(200)，单条 `diffHead` 可达 6000 字符，长期会撑大数据目录。修法：滚动加字节阈值或写入前按字节截断。
4. **`force`（来一道）在空队列时被正文去重挡住**：用户会觉得"点了没反应"。若放开，需给 `insert_card` 传「用户主动索要」标志并记入决策日志，不得做静默特例。
5. **CI 平台风险**：fmt/clippy 门禁仅在 Windows 压到 0 告警；Linux/macOS 编译路径（`crypto::machine_id()`）本地编译不到，runner clippy 更新可能弄红 CI——红了解 lint，别回退门禁。
6. **opencode 插件 `tsc --strict` 仅在 CI 验证**：本地未装 `@opencode-ai/plugin`，新字段写显式 `as` 断言 + 可选链。
7. **双路同报边缘不一致时可能漏判重**：后果仅是信号权重略高，可接受。

## 8. 关键文档位置

| 文档 | 内容 |
|---|---|
| [README.md](../README.md) | 安装运行、三 Agent 联动配置、端点发现、数据目录说明 |
| [docs/InitPrompt.md](InitPrompt.md) | 原始技术规格书：问题定义、架构、门控树、里程碑、待决事项 |
| [docs/复盘与工程约定.md](复盘与工程约定.md) | 六轮复盘、铁律、缺陷现场证据、判读表（**必读**） |
| [docs/HANDOVER.md](HANDOVER.md) | 本交接报告 |
| `.github/workflows/` | ci.yml / release.yml |

**运行时数据目录**（非仓库内）：`%APPDATA%\dev.kvba.app\`（endpoint/settings/queue/history/secrets.enc 等，详见 README）；日志在 `%LOCALAPPDATA%\dev.kvba.app\logs\KVBA.log`——排查静默失败先看它。

## 9. 文件清理记录（2026-09-03）

| 清理对象 | 大小 | 原因 | 恢复方式 |
|---|---|---|---|
| `adapters/claude-code/target/` | 564 MB | Rust 构建产物，已被 .gitignore 忽略，纯可再生 | `cargo build`（在 adapters/claude-code 下） |
| `dist/` | 0.1 MB | Vite 前端构建产物，已被 .gitignore 忽略 | `npm run build` |
| `docs/codebase-research.md` | 0.03 MB | 一次性研究报告，结论已沉淀进本报告与复盘文档，避免双份维护过期 | git 历史（`git log -- '*codebase-research*'`） |

**甄别后保留（非冗余）**：
- `node_modules/`（132 MB）：开发必需，未删除。
- 根 `target/`：含运行时数据（kvba-data、trae-test.json），体积小，保留。
- `.opencode/plugins/kvba-plugin.ts`：与 `adapters/opencode-plugin/` 源文件内容一致（哈希已核对），是 CI 强制校验一致性的部署副本，**不是冗余文件**。
- `.trae/hooks.json`：Trae 适配器的本地 hooks 配置（.gitignore 忽略），引用 `adapters/trae-hook/kvba-trae-hook.mjs`，保留。

清理后释放约 **564 MB**，项目源码体积不受影响。
