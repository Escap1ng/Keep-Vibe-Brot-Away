# 工程指南

> 状态：Current ｜ 受众：贡献者与维护者 ｜ 权威范围：长期工程规则、验证命令、CI 与发布 ｜ 最近核对：2026-11-08

## 开始修改前

1. 先读[当前架构](architecture.md)和[当前状态](status.md)，确认修改落在新 WorkUnit/Insight 链路还是旧门控兼容路径。
2. 用代码和可执行测试验证假设。结论分为“静态审计”“单元测试”“端到端实测”，不要跨级表述。
3. 工作区可能包含他人的修改；只编辑任务相关文件，不清理不属于本次任务的变更。

历史事故与被否证假设保留在[复盘记录](复盘与工程约定.md)，不再由它承担当前规范和待办管理。

## 长期规则

1. **绝不阻塞 Agent。** 适配器、分析器、持久化与 UI 的失败都必须可降级；若数据安全要求停止写入，应明确记录并保留内存结果。
2. **证据先于叙事。** “没有搜到”不等于“不存在”；重要否定结论至少用两种独立方式复验。
3. **错误必须可证伪。** 日志说明发生在哪一层、观察到了什么、下一步如何区分原因，避免永远为绿的状态灯。
4. **跨文件契约一次完成。** 修改函数签名、事件字段、Serde 别名或命令注册时，同一次编辑覆盖定义、调用点和测试，避免热重载宿主运行中间态。
5. **每次保存都可能部署。** 在运行 `tauri dev` 时，落盘前先保证 Rust 代码至少能通过 `cargo check`。
6. **一个事实只维护一处。** 当前状态写在 `status.md`，当前架构写在 `architecture.md`，决策原因写在 RFC。
7. **代码与文档同提交。** 改常量、路径、命令、协议或平台支持时同步更新对应权威文档。
8. **关键修改完成后重新检索。** 对字段名、命令名、文件路径和旧术语做全仓复验，不依赖编辑工具的成功提示。
9. **提交信息使用英文 Conventional Commits。** 例如 `fix: ...`、`docs: ...`、`refactor: ...`；PR 标题与正文按项目协作语境使用中文。

## 仓库地图

| 路径 | 作用 |
|---|---|
| `src-tauri/` | Tauri 2 与 Rust 核心 |
| `src/` | Vue 3 胶囊与设置界面 |
| `adapters/` | Agent 适配器正本 |
| `docs/` | 当前文档、RFC 与历史材料 |
| `.github/workflows/` | CI 与发布流水线 |

## 本地验证

前端：

```powershell
npm install
npm run build
```

Rust 主程序：

```powershell
Push-Location src-tauri
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
Pop-Location
```

Claude Code Rust 适配器：

```powershell
Push-Location adapters\claude-code
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
Pop-Location
```

Node 适配器至少做语法与 JSON 校验：

```powershell
node --check adapters\claude-code-hook\kvba-claude-hook.mjs
node --check adapters\codex-notify\kvba-codex-hook.mjs
node --check adapters\qoder-hook\kvba-qoder-hook.mjs
node --check adapters\qoder-hook\install-settings.mjs
Get-Content adapters\qoder-hook\hooks.sample.json -Raw | ConvertFrom-Json | Out-Null
```

只改 Markdown 时，至少执行：

```powershell
git diff --check
rg -n "work-units\.json|insights\.json|P0|P1|KVBA_DATA_DIR|72 项|三 Agent" README.md docs
```

检索结果不是自动失败；它用于确认旧术语是否被明确标成历史或限制。

## CI

`.github/workflows/ci.yml` 在以下场景运行：

- 向 `main` 推送。
- Pull Request。
- 手动触发。

检查项包括：

- Ubuntu 上的前端类型检查与 Vite 构建。
- opencode 插件独立严格 TypeScript 检查。
- Windows、Ubuntu、macOS 上两个 Rust 工程的 rustfmt、Clippy `-D warnings` 和单元测试。

PR 交付前应等待所有必需检查完成，不能用单个平台本地通过代替矩阵结果。

## 发布

`.github/workflows/release.yml` 在版本标签上构建 Windows、Ubuntu 与 macOS 产物。标签去掉 `v` 后必须与 `package.json` 的版本一致。

不要复用已经存在的 `v0.2.0` 标签。发布前应先：

1. 确定下一个语义化版本。
2. 同步 `package.json`、`package-lock.json`、`src-tauri/Cargo.toml` 和 Tauri 配置中的版本。
3. 合并并验证 CI。
4. 创建新的版本标签，再观察发布流水线和产物。

手动触发发布工作流只生成工作流产物，不创建 GitHub Release。

## 文档检查清单

- 链接目标存在，标题锚点可读。
- 当前事实没有复制到多个文件。
- 愿景、已实现和已知限制分别表述。
- 平台支持区分“CI 编译通过”和“实机端到端验证”。
- 隐私文案说明启用 LLM 后会向所选提供商发送上下文与鉴权信息。
- 旧规格和复盘明确标为历史材料。
