# 适配器指南

> 状态：Current ｜ 受众：使用者与维护者 ｜ 权威范围：适配器安装、验证、故障排查与能力边界 ｜ 最近核对：2026-11-08

## 共用机制

KVBA 启动后会在应用数据目录写入 `endpoint.json`。适配器读取其中的随机端口和鉴权信息，通过本机回环地址调用 `/kvba/hello` 与 `/kvba/events`。

适配器的设计原则是“失败不阻塞 Agent”：网络错误会重试并最终丢弃本次上报。是否接入成功，应以 KVBA 设置面板“运行状态 → 事件链路”的计数增长为准，不能只看 Agent 侧是否报错。

Windows 默认端点文件为：

```text
%APPDATA%\dev.kvba.app\endpoint.json
```

适配器支持用 `KVBA_DATA_DIR` 改变自己的查找目录，但 KVBA 主程序目前不读取这个环境变量。只有当它明确指向主程序实际使用的数据目录时才能设置，否则会让两端失联。macOS 的默认查找路径与 Tauri 应用数据目录目前可能不一致，见[当前状态](status.md)。

## 能力矩阵

| 适配器 | 工具事件 | 回合边界 | 自动形成 `WorkUnit` | 主要限制 |
|---|---:|---:|---:|---|
| opencode 插件 | 是 | 是 | 是 | 无统一测试退出状态 |
| Claude Code Node 钩子 | 是 | 是 | 是 | 异步钩子不保证严格到达顺序；未接 `PostToolUseFailure` |
| Claude Code Rust bridge | 是 | 是 | 是 | 需要编译；随附样例异步上报且未配置失败/会话结束事件 |
| Claude Code JSONL watcher | 是 | 否 | 否 | 暂走旧门控兼容路径 |
| Trae 钩子 | 是 | 是 | 是 | 工具响应文本不等同可靠退出状态 |
| Codex `notify` | 否 | 是 | 是 | 仅能结合回合结束时 Git 快照，无法提供逐工具证据 |

“自动形成 `WorkUnit`”只表示能够收口回合。当前 `kvba.event.v1` 没有标准执行结果字段，因此识别到的测试命令仍会标记为 `Unknown`。

## opencode

插件正本位于 `adapters/opencode-plugin/kvba-plugin.ts`，仓库内可直接加载的副本位于 `.opencode/plugins/kvba-plugin.ts`。CI 会校验二者完全一致。

全局安装：

```powershell
New-Item -ItemType Directory -Force "$env:USERPROFILE\.config\opencode\plugins"
Copy-Item adapters\opencode-plugin\kvba-plugin.ts "$env:USERPROFILE\.config\opencode\plugins\kvba-plugin.ts"
```

单项目安装：

```powershell
New-Item -ItemType Directory -Force .opencode\plugins
Copy-Item adapters\opencode-plugin\kvba-plugin.ts .opencode\plugins\kvba-plugin.ts
```

重启 opencode 会话后执行一次工具调用，并确认 KVBA 的事件计数增加。插件会串行发送工具事件和 `turn.ended`，避免边界先于最后一个工具结果到达。

## Claude Code

### Node 钩子（推荐）

把 [hooks.sample.json](../adapters/claude-code-hook/hooks.sample.json) 中的 `hooks` 合入以下任一配置：

- 全局：`%USERPROFILE%\.claude\settings.json`。
- 单项目：`<项目>\.claude\settings.json`。

示例使用 `$CLAUDE_PROJECT_DIR/adapters/claude-code-hook/kvba-claude-hook.mjs`。脚本不在当前项目内时，改为脚本的绝对路径。Claude Code 通过 shell 执行命令，路径包含空格时应保留示例中的引号。

修改配置后必须新开会话。当前示例覆盖 `UserPromptSubmit`、`PostToolUse`、`Stop`、`SubagentStop` 和 `SessionEnd`；所有钩子均为异步，因此“不会阻塞 Agent”优先于严格顺序。失败工具尚未通过 `PostToolUseFailure` 单独上报。

手工验证脚本：

```powershell
$env:KVBA_HOOK_DEBUG = "1"
'{"hook_event_name":"Stop","session_id":"manual-test","cwd":"C:\\project"}' |
  node adapters\claude-code-hook\kvba-claude-hook.mjs
```

看到 `→ ok` 表示端点可达；`fail/post` 或 `skip/no-endpoint` 时先检查 KVBA 是否运行以及 `endpoint.json` 是否属于当前进程。

### Rust bridge（可选）

```powershell
Push-Location adapters\claude-code
cargo build --release
Pop-Location
```

构建产物为 `kvba-claude-bridge` 与 `kvba-claude-watcher`。把 [hooks.settings-sample.json](../adapters/claude-code/hooks.settings-sample.json) 中的命令改为 bridge 的绝对路径，再合入 Claude Code 配置。

随附样例存在三项边界：

- 只配置 `PostToolUse` 与 `Stop`。
- 使用异步钩子，不保证工具事件先于回合边界到达。
- 没有配置 `PostToolUseFailure`。

bridge 代码能识别更多事件，不代表样例已经订阅这些事件。

JSONL watcher 只作为兜底。它没有稳定的回合边界，因此不能进入 `WorkUnit` 自动聚合链路。bridge 与 watcher 同开时，服务端会在 5 秒窗口内按内容指纹判重；边缘字段不同仍可能形成两条事件。

## Codex

把下面一行放在 `%USERPROFILE%\.codex\config.toml` 的顶层，并改成真实绝对路径：

```toml
notify = ["node", "C:\\Users\\me\\KVBA\\adapters\\codex-notify\\kvba-codex-hook.mjs"]
```

重启 Codex 会话后完成一次任务，再确认 KVBA 的事件计数增加。若没有事件，依次检查：

1. `notify` 是否位于 TOML 顶层，而不是某个 `[section]` 下。
2. `node` 是否在 Codex 进程的 `PATH` 中；GUI 启动时可改用 Node.js 绝对路径。
3. 脚本路径是否为绝对路径。参数由 Codex 直接传给进程，不经过 shell，不要加入 `~`、`$HOME` 或重定向语法。

手工验证：

```powershell
$env:KVBA_HOOK_DEBUG = "1"
node adapters\codex-notify\kvba-codex-hook.mjs '{"type":"agent-turn-complete","thread-id":"t1","turn-id":"1","cwd":"C:\\project","last-assistant-message":"完成修改"}'
```

Codex 当前只提供回合完成通知。KVBA 会在该边界读取 Git 工作区，因此仍可生成基于快照的洞察；但无法知道本轮逐工具操作、测试退出状态或审批等待。

## Trae

复制示例配置：

```powershell
New-Item -ItemType Directory -Force .trae
Copy-Item adapters\trae-hook\hooks.sample.json .trae\hooks.json
```

编辑 `.trae/hooks.json`，把占位符换成脚本的绝对路径。随后：

1. 在 Trae 的“设置 → Hooks”中确认项目钩子已启用。
2. 新开一个会话；Trae 只在创建会话时读取钩子配置。
3. 确认命令路径不含空格。当前 Trae 沙箱对带空格路径的引号处理不稳定；可将脚本放到无空格目录，或在 Windows 使用 8.3 短路径且不要添加内层引号。

查找 Windows 短路径：

```powershell
(New-Object -ComObject Scripting.FileSystemObject).GetFolder("C:\path with spaces\KVBA").ShortPath
```

手工验证：

```powershell
$env:KVBA_HOOK_DEBUG = "1"
'{"hook_event_name":"Stop","session_id":"manual-test","cwd":"C:\\project"}' |
  node adapters\trae-hook\kvba-trae-hook.mjs
```

## 共用故障排查

| 症状 | 检查 | 修复 |
|---|---|---|
| 事件计数一直为 0 | `endpoint.json` 是否存在、端口是否属于当前 KVBA 进程 | 重启 KVBA，再重启 Agent 会话 |
| 手工脚本显示 `skip/no-endpoint` | 适配器读取的数据目录是否正确 | 清除错误的 `KVBA_DATA_DIR`，或指向主程序真实数据目录 |
| 脚本显示 `fail/post` | KVBA 是否仍在运行、端口是否已随机化 | 重启 KVBA 后重新执行手工验证 |
| Claude Code 没有新事件 | 配置是否已加载到新会话、脚本路径引号是否正确 | 新开会话；路径含空格时保留引号 |
| Trae 显示启用但没执行 | 命令路径是否含空格、是否仍在旧会话 | 使用无空格/短路径并新开会话 |
| Codex 没有通知 | `notify` 是否处于 TOML 顶层、Node.js 是否可见 | 移到文件顶层并改用 Node.js 绝对路径 |

Windows 运行日志位于 `%LOCALAPPDATA%\dev.kvba.app\logs\KVBA.log`。适配器不向 Agent 显示错误时，以手工验证输出和该日志共同判断。
