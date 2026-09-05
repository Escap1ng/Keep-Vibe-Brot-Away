# KVBA——Keep-Vibe-Brot-Away 项目伴读

在 VibeCoding 的等待间隙里,把"对项目的掌控感"递回给你。

KVBA 不接管执行。它在 Agent 回合结束后把低层事件聚合为 WorkUnit，用本地 Git 与测试事件形成证据化 Insight，再通过胶囊低打扰地递给用户。问题队列最多 3 张,用户不消费,它就沉默。

> **动手改这个仓库之前,先读 [docs/复盘与工程约定.md](docs/复盘与工程约定.md)。** 那里记着已经付过学费的地方:哪些是铁律、哪些坑别再踩、以及这份文档要求你如何诚实更新它。
>
> 新加入的协作者请从 [docs/HANDOVER.md](docs/HANDOVER.md) 入手——那里是全貌速览与已知问题清单。

## 目录

- [安装](#安装)：环境要求 / 启动 / 配置 LLM / 四种 Agent 联动 / 端点发现
- [使用方式(产品循环)](#使用方式产品循环)
- [架构](#架构)
- [数据目录](#数据目录)
- [目录结构](#目录结构)
- [持续集成与发布](#持续集成与发布)
- [已知边界](#已知边界)
- [文档索引](#文档索引)

```text
CodingAgent(opencode / Claude Code / Trae)
  → 适配器(opencode 插件 / claude-code bridge / jsonl watcher / Trae hook)
  → POST /kvba/events(127.0.0.1 随机端口 + token 鉴权)
  → KVBA(Rust 核心:WorkUnit 聚合 / Git 证据 / Insight / 兼容门控与记忆层)
  → 悬浮胶囊:普通洞察进入摘要队列,仅高置信高风险主动展开
```

---

## 安装

### 环境要求

- Windows 10/11(macOS/Linux 支持正在完善,Wayland 下透明/置顶效果依赖窗口合成器)
- Node.js ≥ 18、Rust ≥ 1.77、WebView2 运行时
- 一个自备的 LLM API Key(OpenAI 兼容 / Gemini / Claude),密钥仅存本机(AES-256-GCM)

### 启动

**开发模式**(vite 热更 + Rust 调试):

```bash
npm install        # 首次
npm run tauri dev  # 或直接双击 dev.cmd
```

**正式使用**(打包安装):

```bash
npm run tauri build   # 产物: src-tauri/target/release/bundle/nsis/*.exe
# 只想要 nsis 安装包(跳过 msi): npm run tauri build -- --bundles nsis
```

安装后从开始菜单启动「KVBA」。启动即后台驻留:系统托盘图标 + 屏幕右缘的红色悬浮胶囊,**不弹任何主窗口**;勾选设置里的「开机自启」后随系统静默启动。退出只能走托盘菜单(窗口 ✕ 一律转为隐藏)。

首次启动请先:右键胶囊 →「打开设置…」→ 填模型名与 API Key。

### 配置 LLM

右键胶囊 →「打开设置…」→ 模型页:

| 项 | 说明 |
|---|---|
| Provider | `openai`(OpenAI 兼容,可换 DeepSeek/中转)/ `google` / `anthropic` |
| 模型名 | 如 `gpt-4o-mini`、`gemini-2.0-flash`、`claude-3-5-haiku` |
| 自定义 BaseURL | 可选,代理/中转地址(留空 = 官方端点) |
| API Key | 本机加密存储(不会上云;解密密钥派生自机器指纹) |
| 每日调用上限 | 默认 80 次/日,超限自动降级为"攒批" |

> 未配置 LLM 时,KVBA 仍会以"确定性模板卡"照常工作,只是内容不再由 AI 生成。
>
> 想确认 LLM 是否真的在工作,看设置面板「运行状态 → **出稿来源**」:`LLM N · 模板兜底 M`。卡片解析失败会静默回填模板文案并照常入队,所以「今日发卡 N」对两种来源一视同仁——**只有「出稿来源」才能证明 LLM 链路正常**。每张卡的具体出处记录在「决策日志」的 G7 行里。

### 与 opencode 联动

插件文件:正本 `adapters/opencode-plugin/kvba-plugin.ts`(副本 `.opencode/plugins/kvba-plugin.ts` 即开即用,两者必须一致)。

```bash
# 全局安装(所有项目)
copy adapters\opencode-plugin\kvba-plugin.ts %USERPROFILE%\.config\opencode\plugins\kvba-plugin.ts
# 或单项目安装
copy adapters\opencode-plugin\kvba-plugin.ts .opencode\plugins\kvba-plugin.ts
```

插件订阅 `tool.execute.before/after` 与 `session.idle`,自动向 KVBA 上报归一化事件(失败重试 3 次后静默丢弃,绝不影响 Agent)。**安装后重启 opencode 会话即生效。**

### 与 Claude Code 联动

两种接法任选其一,可同时开(同报由服务端 5s 内容指纹判重兜底)。

**接法 A:Node 钩子(零编译,推荐)**

```powershell
# 全局生效:把 hooks.sample.json 的 hooks 块合入 %USERPROFILE%\.claude\settings.json
# 单项目生效:合入 <项目>\.claude\settings.json
```

示例片段(必须**新开会话**才生效;`matcher` 用 `"*"` 而不是枚举工具名,枚举表会漏掉 MultiEdit/WebFetch 等):

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [ { "type": "command", "command": "node \"$CLAUDE_PROJECT_DIR/adapters/claude-code-hook/kvba-claude-hook.mjs\"", "timeout": 3, "async": true } ] }
    ],
    "PostToolUse": [
      { "matcher": "*", "hooks": [ { "type": "command", "command": "node \"$CLAUDE_PROJECT_DIR/adapters/claude-code-hook/kvba-claude-hook.mjs\"", "timeout": 3, "async": true } ] }
    ],
    "Stop": [
      { "hooks": [ { "type": "command", "command": "node \"$CLAUDE_PROJECT_DIR/adapters/claude-code-hook/kvba-claude-hook.mjs\"", "timeout": 3, "async": true } ] }
    ]
  }
}
```

脚本装在仓库外时,把 `$CLAUDE_PROJECT_DIR/...` 换成绝对路径。完整配置见 [adapters/claude-code-hook/hooks.sample.json](adapters/claude-code-hook/hooks.sample.json),覆盖 UserPromptSubmit / PostToolUse / Stop / SubagentStop / SessionEnd 五类事件。

**接法 B:Rust 桥(需编译)**

```bash
cd adapters\claude-code
cargo build --release
# 产物:target\release\kvba-claude-bridge.exe 与 kvba-claude-watcher.exe
```

1. 将 `adapters/claude-code/hooks.settings-sample.json` 里的 execute 路径改为你的 `kvba-claude-bridge.exe` 绝对路径;
2. 合入 `~/.claude/settings.json`(示例片段):

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Edit|Write|Patch|Bash|Read|Grep|Glob|Think",
        "hooks": [
          { "type": "command", "command": "<kvba-claude-bridge.exe 绝对路径>", "timeout": 3, "async": true }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          { "type": "command", "command": "<kvba-claude-bridge.exe 绝对路径>", "timeout": 3, "async": true }
        ]
      }
    ]
  }
}
```

3.(可选·兜底,方案 C)后台运行 JSONL watcher:

```powershell
Start-Process -WindowStyle Hidden <kvba-claude-watcher.exe>
```

### 与 Codex 联动

零编译,一行配置:把下面这行**原样追加到 `%USERPROFILE%\.codex\config.toml` 的顶层**(路径换成实际绝对路径),重启 Codex 会话生效:

```toml
notify = ["node", "C:\\Users\\me\\KVBA\\adapters\\codex-notify\\kvba-codex-hook.mjs"]
```

三条会静默吃掉配置的坑(详见 [adapters/codex-notify/config.sample.toml](adapters/codex-notify/config.sample.toml)):

1. `notify` 必须是**顶层键**——粘在 `[tui]` 等段落下面会变成 `[tui].notify`,完全无效且无报错;
2. 不要加引号、不要写 shell 语法——Codex 直接把 argv 交给 exec,不经过 shell(`~`、`$HOME` 不展开;路径带空格也不需要引号);
3. `node` 必须在 Codex 所在环境的 PATH 里(GUI 启动时未必继承 shell 的 PATH,拿不准就写绝对路径)。

能力边界:Codex 目前只有 `agent-turn-complete` 一种 notify 事件,这一路只能产出 `turn.ended` → 知识分支(贴士/破冰/知情度卡);**审查卡需要写事件,Codex 这一路拿不到**,想要审查卡请用 Claude Code 或 Trae。

手工自证(不经 Codex 包装层):

```powershell
$env:KVBA_HOOK_DEBUG = "1"
node adapters\codex-notify\kvba-codex-hook.mjs '{"type":"agent-turn-complete","thread-id":"t1","turn-id":"1","cwd":"'$PWD'","last-assistant-message":"改完 src/lib/api.ts"}'
# 期望 [kvba-hook] codex agent-turn-complete → ok;→ fail/drop 说明 KVBA 没在跑或 endpoint.json 不对
```

### 与 Trae 联动

零编译,装好 Node 即可:

```powershell
md .trae
copy adapters\trae-hook\hooks.sample.json .trae\hooks.json
# 然后编辑 .trae\hooks.json,把 <仓库绝对路径…> 换成实际路径,保留后面的 \adapters\trae-hook\kvba-trae-hook.mjs
```

**三步必做,否则 hook 永不触发**(踩过的坑,别省):

1. Trae 菜单 **设置 → Hooks**,确认项目 Hook 已列出且处于**启用**状态(未启用就点创建/启用);
2. **新开一个会话**再让 Agent 干活 —— Trae 只在创建 Session 时读取 hook 配置,当前会话内改文件不生效。
3. **命令里的路径不能含空格,也不要给路径加引号。** Trae 会把 hook 命令交给 `trae-sandbox.exe exec --command-line <...>` 执行,命令串里的内层引号会**提前闭合外层引号**,于是路径在空格处被劈成两半,报 `error: unexpected argument 'more\KVBA\adapters\...' found`。此时 Hook 状态看着是"已启用",实际一次都没跑起来。

   仓库放在带空格(或中文)的目录下时,改用 **8.3 短路径**,并且**不加引号**:

   ```powershell
   # 查短路径名:Scripting.FileSystemObject 的 ShortPath
   (New-Object -ComObject Scripting.FileSystemObject).GetFolder("C:\Users\me\Desktop\GH more").ShortPath
   # → C:\Users\me\Desktop\GHMORE~1
   ```

   ```json
   { "type": "command", "command": "node C:\\Users\\me\\Desktop\\GHMORE~1\\KVBA\\adapters\\trae-hook\\kvba-trae-hook.mjs", "timeout": 10 }
   ```

   脚本本身只依赖 `node:fs`、端点从 `%APPDATA%\dev.kvba.app\endpoint.json` 读,所以拷到任何无空格目录(或经短路径访问)都能正常工作,`projectPath` 仍取 Trae 传入的 `cwd`。

配置生效与否的判据:打开 KVBA 设置面板「运行状态 → 事件链路」,显示「本次运行 0 条」即未接入;显示条数在增长即已通。也可以先手工验证脚本本身:

```powershell
$env:KVBA_HOOK_DEBUG = "1"
'{"hook_event_name":"Stop","session_id":"t","cwd":"'$PWD'"}' | node adapters\trae-hook\kvba-trae-hook.mjs
# 期望 [kvba-hook] Stop → ok;→ fail/drop 说明 KVBA 没在跑或端点文件不对
```

`.trae/hooks.json` 是机器相关的本地配置(含绝对路径),已在 `.gitignore` 内,不会随仓库分发。脚本在 `PostToolUse` 读 `tool_name/tool_input/tool_response`、在 `Stop` 发 `turn.ended`,并把 Trae 的工具名映射成门控期望的规范小写名(`SearchReplace→edit`、`RunCommand→bash`、`Read→read` …);`RunCommand` 不算写事件,避免把跑命令误判成改代码。全局生效可改用 `%USERPROFILE%\.trae-cn\hooks.json`。

### 端点发现

各适配器统一从 `<数据目录>\endpoint.json` 读取端口与 token(`KVBA_DATA_DIR` 环境变量可覆盖数据目录;Windows 默认为 `%APPDATA%\dev.kvba.app`)。`/kvba/hello` 用主 token 换取事件上报 token,每次重启都会让主进程再次随机化端口与 token。

---

## 使用方式(产品循环)

1. Agent 忙碌时,KVBA 把卡片推送到胶囊(屏幕右上/右下角):
   - **审查卡**:本次 AI 产出有什么不足?→ 满意 / 不满意 / 自定义…
   - **知识贴士**:项目命中 Vue/Nuxt/Rust 等框架时,讲相关知识;
   - **知情度卡**:「知道这个项目现在用什么框架吗?」→ 回答被记入项目记忆。
2. 点「不满意」→ KVBA 生成"下一轮优化提示词"(≤800 token 小调用)→ **自动写入系统剪贴板** + 追加 `<项目>/.kvba/next-prompt.md`,粘贴回 Agent 即完成闭环。
3. 队列最多 3 张,满则一切触发停止(零 LLM 消费);「跳过」「不再问这类」「TTL 2 小时」「引用文件被再次修改→过时退场」都是排泄口。
4. 「来一道」旁路频控,想学就点。**每个会话开场还会自动递一张知识贴士**(整轮对话刚开始、你正在等 Agent 干活的那个空档),不需要先攒够"可审的改动"。
5. 暂停/恢复、隐藏胶囊(仅托盘)、打开设置、退出:右键胶囊或托盘图标随时可用。

## 架构

```text
┌────────────────────────────────────────────────────────────┐
│ CodingAgent 环境                                            │
│  Claude Code: hooks PostToolUse/Stop + JSONL watcher 兜底    │
│  opencode:    TS 插件(事件钩子)                            │
│  Trae:        hooks PostToolUse/Stop -> node 脚本            │
│  输出统一"归一化事件"→ POST http://127.0.0.1:<port>/kvba/events
└──────────────────────────┬─────────────────────────────────┘
                           │ HTTP(握手 token 鉴权,仅 127.0.0.1)
┌──────────────────────────▼─────────────────────────────────┐
│ KVBA(Tauri 2,无主窗口,托盘 + 悬浮胶囊)                      │
│ Rust 核心(gate/events/memory/llm/crypto/queue/http)        │
│ TS 前端(Vue3: 胶囊状态机 idle/active/popup + 设置/历史/决策日志)
└────────────────────────────────────────────────────────────┘
```

- **WorkUnit 与证据化 Insight**(`src-tauri/src/workunit/`、`src-tauri/src/insight/`):自动链路在 `turn.ended` 收口，读取回合结束时 Git 工作区快照与测试事件，生成带结论、证据、建议动作、风险级别和置信度的结构化洞察。普通摘要只更新胶囊角标，高置信 P1/P0 才主动展开。Git 快照只描述收口时可见状态，不冒充本回合精确归因。
- **兼容门控决策树**(`src-tauri/src/gate/`):保留给手动「来一道」和暂时没有回合边界的 Claude JSONL watcher；G1→G7 的旧卡片协议继续兼容已有队列与历史数据。
- **节奏参数(2026-09-03 依据实测放宽)**:频率限制 25s、同主题冷却 10min、正文去重窗口 20min、静默期 1h 保持不变(静默期是用户唯一的否决权,不予放宽)。知识分支会在「项目命中主题 + 通用主题池」中轮换,优先挑最久没问过的,不再反复问同一句话。
- **开场破冰**:每个会话的第一次评估会放行一张知识贴士(绕过频率限制与主题冷却,但不突破队列上限、不占用静默期),让"对话一开始"就有内容可看;它不会出知情度卡——那张卡必须最后生成。
- **记忆层**(`src-tauri/src/memory/`):项目级 facts(扫描 package.json/README/目录树,首版由一次 LLM 调用生成,仅在消费 ≥20 条或会话结束时增量合并)+ 对话滚动窗口(最近 3 条 ≤3KB,零 LLM 消耗)。同一项目的 facts 生成期间只调用一次,不会逐条事件消耗额度。
- **加密存储**(`src-tauri/src/crypto/`):AES-256-GCM,密钥 = HKDF-SHA256(机器指纹,随机 salt);机器指纹来源:Windows MachineGuid / macOS IOPlatformUUID / Linux /etc/machine-id。全程不使用 MD5。
- **防腐不侵入**:适配器失败重试 3 次后丢弃,绝不阻塞 Agent;胶囊圆点是 48×48 无边框透明置顶小窗(不进任务栏、从不抢占焦点);胶囊可随时隐藏,退化为托盘驻留 + 系统通知兜底。

## 数据目录

`%APPDATA%\dev.kvba.app\`(macOS/Linux:`~/.local/share/dev.kvba.app`):

| 文件 | 说明 |
|---|---|
| `endpoint.json` | 本地端点(端口 + token,每次重启随机) |
| `settings.json` | 非机密设置 |
| `secrets.enc` | API Key(AES-256-GCM) |
| `queue.json` / `history.json` | 队列与历史 |
| `cooldown.json` | 主题冷却矩阵 |
| `budget.json` | 每日 LLM 预算快照 |
| `decision-log.jsonl` | 门控决策日志(滚动 200) |
| `projects/<sha256(path)[..16]>/` | 每项目 `events.log`、`facts.json`、`work-units.json`(最近 100 个)、`insights.json`(最近 300 条) |

日志**不在**上面这个目录:`tauri-plugin-log` 默认同时写 stdout 与系统日志目录,打包运行时只写文件。Windows 在 `%LOCALAPPDATA%\dev.kvba.app\logs\KVBA.log`(macOS `~/Library/logs/dev.kvba.app/`,Linux `~/.local/share/dev.kvba.app/logs/`)。排查静默失败(如 facts 生成不落地)先看这个文件,不要只盯着 `tauri dev` 的终端。

## 目录结构

```text
src-tauri/            Rust 核心(workunit/insight/workflow + 兼容 gate/events/memory/llm/queue)
src/                  Vue3 + Vite 前端(胶囊圆点、面板、设置/历史/决策日志)
adapters/
  claude-code/        Rust 桥:kvba-claude-bridge(stdin hook)+kvba-claude-watcher(jsonl)
  opencode-plugin/    TS 插件(kvba-plugin.ts;部署副本 .opencode/plugins/,CI 强制两者一致)
  trae-hook/          Node 脚本适配器(kvba-trae-hook.mjs + hooks.sample.json)
docs/                 规格与文档(改本仓库前必读 复盘与工程约定.md)
.github/workflows/    CI 与三平台发布流水线
```

## 持续集成与发布

- **CI**(`.github/workflows/ci.yml`,每次 push / PR / 三平台矩阵):
  `vue-tsc + vite build` → opencode 插件 `tsc --noEmit`(并校验 `.opencode/plugins/` 副本与源文件一致)→
  `cargo test --locked`(主程序 + claude-code 适配器)。
  `cargo fmt --check` 与 `cargo clippy -- -D warnings` 均为**阻断门禁**(两个 Rust 工程当前零告警)。
- **发布**(`.github/workflows/release.yml`):打 tag 即三平台打包并把产物挂到 GitHub Release。

  ```bash
  git tag v0.2.0 && git push origin v0.2.0
  ```

  产物:Windows `nsis/*.exe`、Ubuntu `deb/*.deb`、macOS `dmg/*.dmg`。tag 去前缀后与 `package.json` 版本不一致会直接失败;手动触发(workflow_dispatch)只构建上传工作流产物,不建 Release。

## 已知边界

- Wayland 下透明/置顶依赖合成器,建议 X11 或 Windows/macOS。
- 本地 HTTP 仅绑定 `127.0.0.1`,token 随机;密钥永不离开本机。
- LLM 分析超时(180s)与重试(指数退避 ×2)后丢弃本次卡片,不影响后续批次。一次分析最多只占一格当日额度(契约解析失败的严格重试不再重复计费)。
- Claude Code 的 bridge(hook)与 watcher(JSONL 兜底)同开时,同一动作会被报两遍。事件 `id` 由各适配器自生成、两边不同,因此服务端按 **emitter 无关的内容指纹**(项目 + 事件类型 + 工具 + 意图 + 文件名 + 事件秒)在 5s 窗口内判重并静默 ACK。若两路边缘字段(如意图文本)不一致,仍可能漏判为两条事件——后果只是信号权重略高,不会丢数据。
- `endpoint.json` 是适配器发现本地端点的唯一入口。端口/token 每次重启随机化,若该文件写入失败(权限/目录被删),适配器将无法配对而静默失效,而 KVBA 本体仍可用——此时看启动日志中的 `写入 ... 失败` 一行。**删除 `endpoint.json` 并重启 KVBA 即可重置配对**(后需重启各 Agent 会话让适配器重读)。

## 文档索引

| 文档 | 内容 | 什么时候读 |
|---|---|---|
| [docs/HANDOVER.md](docs/HANDOVER.md) | 项目交接报告:全貌速览、模块地图、已知问题清单、清理记录 | 新协作者入门第一站 |
| [docs/复盘与工程约定.md](docs/复盘与工程约定.md) | 六轮复盘、铁律、缺陷现场证据、出稿判读表 | **改本仓库前必读** |
| [docs/InitPrompt.md](docs/InitPrompt.md) | 原始技术规格书:问题定义、架构、门控树、里程碑 | 理解设计意图与取舍 |
| [docs/rfcs/RFC-0003-workunit-cognition-ledger.md](docs/rfcs/RFC-0003-workunit-cognition-ledger.md) | 架构升级 RFC: 从事件卡片转向 WorkUnit + Insight 认知账本 | 跟进 #3 的产品方向与目标架构 |
