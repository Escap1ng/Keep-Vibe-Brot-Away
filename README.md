# KVBA——Keep-Vibe-Brot-Away 项目伴读

在 VibeCoding 的等待间隙里,把"对项目的掌控感"递回给你。

KVBA 不接管执行,只接管间隙:Agent 忙于干活时,把与当前项目相关的知识点、审查质疑、知情度提问"递"给用户。它不是一个弹窗广告,而是一个**被用户消费的节奏**——问题队列最多 3 张,用户不消费,它就沉默。

```text
CodingAgent(opencode / Claude Code)
  → 适配器(opencode 插件 / claude-code bridge / jsonl watcher)
  → POST /kvba/events(127.0.0.1 随机端口 + token 鉴权)
  → KVBA(Rust 核心:门控决策树 0 token / 记忆层 / 队列 / 加密存储 / LLM 客户端)
  → 悬浮胶囊:递知识 + 抛问题,不满意自动生成「下一轮优化提示词」回写 Agent
```

---

## 安装

### 环境要求

- Windows 10/11(macOS/Linux 支持中,Wayland 下透明/置顶依赖合成器)
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

> 未配置 LLM 时,KVBA 仍以"确定性模板卡"照常工作,只是贴士不再是 AI 生成。

### 与 opencode 联动

插件文件:前板 `adapters/opencode-plugin/kvba-plugin.ts`(或本仓库 `.opencode/plugins/kvba-plugin.ts` 即开即用)。

```bash
# 全局安装(所有项目)
copy adapters\opencode-plugin\kvba-plugin.ts %USERPROFILE%\.config\opencode\plugins\kvba-plugin.ts
# 或单项目安装
copy adapters\opencode-plugin\kvba-plugin.ts .opencode\plugins\kvba-plugin.ts
```

插件订阅 `tool.execute.before/after` 与 `session.idle`,自动向 KVBA 上报归一化事件(失败重试 3 次后静默丢弃,绝不影响 Agent)。**安装后重启 opencode 会话即生效。**

### 与 Claude Code 联动

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

### 端点发现

三处适配器统一从 `<数据目录>\endpoint.json` 读取端口与 token(`KVBA_DATA_DIR` 环境变量可覆盖数据目录;Windows 默认为 `%APPDATA%\dev.kvba.app`)。`/kvba/hello` 用主 token 换取事件上报 token,每次重启都会让主进程再次随机化端口与 token。

---

## 使用方式(产品循环)

1. Agent 忙碌时,KVBA 把卡片递到胶囊(右上/右下角):
   - **审查卡**:本次 AI 产出有什么不足?→ 满意 / 不满意 / 自定义…
   - **知识贴士**:项目命中 Vue/Nuxt/Rust 等框架时,讲相关知识;
   - **知情度卡**:「知道这个项目现在用什么框架吗?」→ 回答被记入项目记忆。
2. 点「不满意」→ KVBA 生成"下一轮优化提示词"(≤800 token 小调用)→ **自动写入系统剪贴板** + 追加 `<项目>/.kvba/next-prompt.md`,粘贴回 Agent 即完成闭环。
3. 队列最多 3 张,满则一切触发停止(零 LLM 消费);「跳过」「不再问这类」「TTL 2 小时」「引用文件被再次修改→过时退场」都是排泄口。
4. 「来一道」旁路频控,想学就点。
5. 暂停/恢复、隐藏胶囊(仅托盘)、打开设置、退出:右键胶囊或托盘图标随时可用。

## 架构

```text
┌────────────────────────────────────────────────────────────┐
│ CodingAgent 环境                                            │
│  Claude Code: hooks PostToolUse/Stop + JSONL watcher 兜底   │
│  opencode: TS 插件(事件钩子)                               │
│  输出统一"归一化事件"→ POST http://127.0.0.1:<port>/kvba/events
└──────────────────────────┬─────────────────────────────────┘
                           │ HTTP(握手 token 鉴权,仅 127.0.0.1)
┌──────────────────────────▼─────────────────────────────────┐
│ KVBA(Tauri 2,无主窗口,托盘 + 悬浮胶囊)                      │
│ Rust 核心(gate/events/memory/llm/crypto/queue/http)        │
│ TS 前端(Vue3: 胶囊状态机 idle/active/popup + 设置/历史/决策日志)
└────────────────────────────────────────────────────────────┘
```

- **门控决策树**(`src-tauri/src/gate/`):G1 队列硬闸 → G2 频控/主题冷却 → G3 活性 → G4 分支路由 → G5 信号评分(阈值 60,P0 判定)→ G6 去重 → G7 才允许调用 LLM(单次 ≤5k token)。全部 0 token、纯函数、带 14 项夹具单测。面板内「决策日志」可看每次评估:Gx 层级、拒绝原因、耗时。
- **记忆层**(`src-tauri/src/memory/`):项目级 facts(扫描 package.json/README/目录树,首版一次 LLM 生成,增量合并仅在消费 ≥20 或会话结束时)+ 对话滚动窗口(最近 3 条 ≤3KB,零 LLM)。
- **加密存储**(`src-tauri/src/crypto/`):AES-256-GCM,密钥 = HKDF-SHA256(机器指纹,随机 salt);机器指纹:Windows MachineGuid / macOS IOPlatformUUID / Linux /etc/machine-id。不出现 MD5。
- **防腐拒侵入**:适配器失败重试 3 次后丢弃,永不阻塞 Agent;胶囊 idle 态整窗点击穿透(悬停探测器条恢复交互);置顶从不抢焦点;胶囊可随时隐藏退化为托盘 + 系统通知兜底。

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
| `projects/<sha256(path)[..16]>/` | 每项目 `events.log`(JSONL 200 条)+ `facts.json`(≤50KB) |

## 目录结构

```text
src-tauri/            Rust 核心(gate/events/memory/llm/crypto/queue/http + window/tray)
src/                  Vue3 + Vite 前端(胶囊状态机、面板、设置/历史/决策日志)
adapters/
  claude-code/        Rust 桥:kvba-claude-bridge(stdin hook)+kvba-claude-watcher(jsonl)
  opencode-plugin/    TS 插件(kvba-plugin.ts)
docs/                 规格与文档
```

## 已知边界

- Wayland 下透明/置顶依赖合成器,建议 X11 或 Windows/macOS。
- 本地 HTTP 仅绑定 `127.0.0.1`,token 随机;密钥永不离开本机。
- LLM 分析超时(180s)与重试(指数退避 ×2)后丢弃本次卡片,不影响后续批次。
