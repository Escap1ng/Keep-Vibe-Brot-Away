# KVBA 代码库研究报告(基于实际代码的再评估)

> ## ⚠️ 历史快照 —— 请先读 §3
>
> §1、§2 与「摘要」是对提交 `2394475` 的**当时快照**,原 §3/§4 列出的 B1-B3、D1-D8 与大部分 P2 项**已在后续清理中修复**;继续按原文行动会重复劳动、甚至改坏已修好的逻辑。
>
> 本文现在可信的部分是 **§3(逐条对账)** 与 **§4(当前基线与剩余动作)** —— 它们已按工作区现行代码重写,每条都标了复核证据位置。
>
> 快照日期:2026-08;对账日期:2026-09-03。

## 摘要(快照原文,未更新)

远程仓库已含一套**完整可运行的首个版本**(提交 `2394475`),规格书 M1–M7 的主干功能基本落地:门控决策树有纯函数实现与 16 个夹具单测、三窗口胶囊交互、双 token 事件接入、AES-256-GCM 密钥加密、每日预算均已就位。此前评审的 8 处缺口中 **3 处已被代码解决、2 处被巧妙规避、1 处部分缓解,仅剩 2 处真正缺失**;5 大风险中 2 个消解、1 个降级为注意点。但深读发现了 **3 个 P0 级正确性 Bug(watcher 项目路径推导错误、半行数据丢失、新卡秒变"已过时")**和一批规格偏差与死代码。本报告给出全部证据与修复优先级。

---

## 1. 现状概览(快照原文)

### 1.1 代码结构(实际)

| 层 | 位置 | 状态 |
|---|---|---|
| Rust 核心 | [src-tauri/src](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src) | gate/events/queue/memory/llm/crypto/http/tray/windows/settings/state 全部存在 |
| Vue3 前端 | [src](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src) | App.vue + CapsuleDot/CapsulePanel/SettingsView + tokens.css 设计令牌 |
| Claude Code 适配器 | [adapters/claude-code](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/adapters/claude-code) | bridge + watcher 双 bin,hooks 样例齐全 |
| opencode 插件 | [adapters/opencode-plugin/kvba-plugin.ts](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/adapters/opencode-plugin/kvba-plugin.ts) | 三钩子完整;`.opencode/plugins/` 下有同步副本 |
| 文档 | [README.md](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/README.md) | 安装、hooks 配置、插件安装三节齐全 |

### 1.2 里程碑完成度判定(快照原文,现状态见 §3)

| 里程碑 | 当时判定 | 依据 |
|---|---|---|
| M1 骨架 | ✅ 完成 | 托盘 + 48px 透明置顶圆点、拖拽、24px 边缘吸附、隐藏为托盘([windows.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/windows.rs)、[tauri.conf.json](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/tauri.conf.json)) |
| M2 事件摄入 | ⚠️ 完成但有 2 个 P0 Bug | HTTP 端点、双适配器、events.log 滚动落盘都在 |
| M3 决策树 | ✅ 完成 | G1–G7 纯函数 `evaluate` + 16 个夹具单测([gate/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/gate/mod.rs)) |
| M4 记忆层 | ⚠️ 完成但有缺陷 | facts 首版/精炼/50KB 上限齐备;收缩丢数据、跨项目混采见 §3 |
| M5 LLM 分析器 | ✅ 基本完成 | 三 provider、SSE、AES-GCM 加密、每日预算 80;"攒批"名存实亡见 §3 |
| M6 胶囊交互 | ⚠️ 完成但有 1 个 P0 Bug | 三态、回答/跳过/静默、TTL、next-prompt 闭环齐备;stale 误判见 §3 |
| M7 打包 | 🟡 大部分完成 | 右键菜单/暂停/设置/历史/单实例齐;打包产物未验证,托盘文案小疵 |

---

## 2. 此前评审结论的核实(快照原文,现状态见 §3)

### 2.1 五大风险

| 风险 | 当时现状 | 证据 |
|---|---|---|
| R1 点击穿透悖论 | ✅ **被设计规避**:圆点窗口缩到 48×48≈圆点本身,非交互区≈0,从根上取消穿透需求;穿透权限已声明但全仓无调用 | [CapsuleDot.vue](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src/components/CapsuleDot.vue)、[capabilities/default.json](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/capabilities/default.json) |
| R2 窗口形态 | 🟡 **选择了多窗口**(3 个:圆点/面板/设置) | [tauri.conf.json](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/tauri.conf.json) |
| R3 diff 提取 | ✅ **采用轻量方案**:不跑 `git diff`,`diffHead` 取 tool_input 中 diff/patch/newString/content 最长者 | [bridge.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/adapters/claude-code/src/bin/bridge.rs) |
| R4 G5-A 激进启发式 | ❌ **仍为激进实现**:纯文本判断,误报风险仍在 | [gate/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/gate/mod.rs) `signal_score` |
| R5 成功度量 | ❌ **仍不可验收**:Stats 仅内存三计数、未序列化、不落盘 | [state.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/state.rs) |

### 2.2 八处缺口(当时)

| # | 缺口 | 当时现状 |
|---|---|---|
| 1 | 框架矛盾 | ✅ Vue3 + `.vue`,无 TSX |
| 2 | 评估触发时机 | ✅ 每条事件即时触发管线,节流靠 G2(60s)+ G3(90s) |
| 3 | 端口/配对发现 | ✅ 随机端口 + `<dataDir>/endpoint.json`,双 token |
| 4 | 攒批出口 | ❌ 预算耗尽直接丢弃,无缓冲无补发 |
| 5 | 无 Key 冷启动 | 🟡 确定性模板卡,但无主动引导通知 |
| 6 | salt 位置 | ✅ 随信封内嵌 `secrets.enc` |
| 7 | 哈希混用 | ✅ blake3(事件/指纹)+ sha256(项目目录) |
| 8 | 面板置顶偏差 | ❌ 面板声明了 `alwaysOnTop: true`,与规格 §9.1 相反 |

### 2.3 悬空引用复核

- `.opencode/skills` 动效哲学文件**依然不存在**;但 [tokens.css](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src/styles/tokens.css) 已自成一套动效令牌(150/220/300ms、强 ease-out、reduced-motion 降级),事实上形成标准。

---

## 3. 逐条对账(2026-09-03 复核,以代码为准)

### 3.1 P0 三项 —— 全部已修复

| # | 原问题 | 现状态 | 复核证据 |
|---|---|---|---|
| B1 | watcher 项目路径推导错误 | ✅ **已修复**(实现换了方案) | 不再做 `parent().parent()`,直接读 JSONL 转录行自带的 `cwd` 字段并按文件缓存;两者都拿不到就**丢弃该事件**而非猜路径 —— [watcher.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/adapters/claude-code/src/bin/watcher.rs) `handle_line` |
| B2 | 半行数据丢失 | ✅ **已修复** | 行尾无 `\n` 时在累加 `consumed` 之前 `break`,偏移不提交,下次轮询重读 —— [watcher.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/adapters/claude-code/src/bin/watcher.rs) 读取循环 |
| B3 | stale 检测误判新卡 | ✅ **已修复** | 加了 `e.ts > c.created_at` 时间校验;文件比对改为 `norm_path` 归一后**精确相等**,注释明确"不用后缀匹配" —— [queue/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/queue/mod.rs) 维护任务 |

### 3.2 P1 八项 —— 七项已修复,一项转为规格差额

| # | 原问题 | 现状态 | 复核证据 |
|---|---|---|---|
| D1 | 攒批无出口 | ✅ 已修复 | `pending_batch` 落盘/载入,预算耗尽走 `push_batch`(同主题去重、上限 20、保鲜 24h),维护任务每 60s `try_drain_batch` 补发;腾额时再撞预算会 `insert(0, item)` 放回 —— [budget.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/llm/budget.rs) |
| D2 | 每事件全量落盘 5 文件 | ✅ 已修复(方案不同) | 摄入热路径只 `append` 一行决策日志 + 重写本项目 `events.log`;`save_core_locked` 只在真正改状态的点调用(入队、作答、攒批、维护)。无计时去抖,也不需要了 —— [http/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/http/mod.rs) `log_decision` |
| D3 | TTL/stale 卡不进 history | ✅ 已修复 | 两类卡均 `archive_card()`(移出 queue + push history,上限 `HISTORY_MAX=500`) |
| D4 | stats 不落盘、死字段 | ✅ 已修复 | `Stats` 派生 `Serialize` 并落 `stats.json`,`roll_to_today()` 跨日并入累计,载入与查询两侧都会归并;`total_today` 死字段已不存在 —— [state.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/state.rs) |
| D5 | facts 收缩清空 awareness_facts;合并跨项目污染 | ✅ 已修复 | 收缩先裁 `decisions`(保留最近 4)再裁 `awareness_facts`(保留最近 20),**绝不整体清空用户事实**,之后才对 `dir_summary` 逐半压缩;合并信号 `core.projects.get(project_key(...))` 只取本项目最近 20 条 —— [memory/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/memory/mod.rs) |
| D6 | 决策日志提前记 Pass | ✅ 已修复 | LLM 执行后按真实结局回写,三结局(`Inserted`/`QueueFull`/`Duplicate`)分别陈述,拒绝原因带真实错误(截 180 字) |
| D7 | 插件端点只读一次 | ✅ 已修复 | `ensureEndpoint()` 惰性重读(≥30s 重试一次),KVBA 晚启动不再整会话静默失效 —— [kvba-plugin.ts](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/adapters/opencode-plugin/kvba-plugin.ts) |
| D8 | G5-A 激进启发式 | 🟡 **转为规格差额** | 「注释密度骤降 +15」**实际未实现**(反而降低了误报面);其余文本启发式与阈值 60 保留。已记入任务书 §13 待决事项 4 |

### 3.3 P2 清单 —— 逐条清算

**原报告有 4 条断言在现行代码中不成立(名称对不上,属误报):**

- ❌ "`gate::dedup_check` 死函数无调用点":源码里没有 `dedup_check`,实际函数是 `dedup_hit`,且有 Review / Knowledge **两个调用点**。
- ❌ "`core.recently_asked` 只写不读":`Core` 无此字段;`recently_asked` 是 `CooldownMap` 的方法,被 G2 冷却判断读两处。
- ❌ "前端 `capsule:panel-head` 死监听":全仓无此事件名;面板标题栏是普通 DOM `@pointerdown` → `startDragging()`。
- ❌ "`blake16` 名不副实(实为 FNV)":函数已改名 `fnv16`,名实相符。

**其余各条现状:**

| 项 | 现状态 |
|---|---|
| `extract_json` 非平衡括号、注释与实现不符 | ✅ 已改为**平衡括号扫描**(带字符串状态与转义感知),注释与实现一致 |
| 正文兜底截 140 字与契约 ≤120 不一致 | ✅ 统一为 **120 字符** |
| 严格重试再占一格当日预算(最坏 1 次分析 2 格) | ✅ 已修复:`chat_complete` 拆为计预算与不计预算两条通道,重试走后者 |
| macOS 指纹注释写 IOPlatformUUID 实读 `/etc/hostname` | ✅ 已改为主路径 `ioreg -rd1 -c IOPlatformExpertDevice` 解析 `IOPlatformUUID`,`/etc/hostname` 仅作回退 |
| bridge `stop_hook_active` 按字符串读、Stop intent 恒空 | ✅ 已改 `as_bool()`,并给出稳定 intent 文案供门控与去重使用 |
| `common.rs` 中 id 哈希 ts 与上报 ts 可能不同步 | ✅ `build_event` 内取一次 `now_ms()` 共用,并有单测锁定 schema |
| `dev.cmd` / hooks 样本含他人机器硬编码路径 | ✅ 全仓已无 `D:\nodejs`、`C:\Users\admin` 之类残留(样本用 `<替换为你的绝对路径>` 占位);`dev.cmd` 另修复了 LF-only 导致的 cmd 字节错位,并加 `.gitattributes` 锁 CRLF |
| 托盘"暂停提问"文案不翻转 | ✅ 已修复(持有菜单引用项本身做 `set_text`) |
| `endpoint.json` 建议注明"删除即重置配对" | ✅ README「已知边界」已补,并补了写失败留痕与诊断口径 |
| 单测仅覆盖 gate 模块 | ✅ 现为 **51 例 / 7 个模块**(gate 17、queue 10、windows 7、llm 7、memory 4、events 4、state 2) |
| watcher 安全阀注释"200 条"实为 1MB | ✅ 注释与代码一致:`lines >= 200 \|\| consumed > 1_000_000` |
| watcher 只发 `tool.completed`,与 bridge 并存双报 | ✅ **本轮修复**:摄入层新增 emitter 无关内容指纹 + 5s 判重窗口,重复事件静默 ACK 且不刷新活性;详见 §4.2 |
| opencode 插件 `lastArgs` 以工具名为键并发覆盖 | ✅ **本轮修复**:键优先用 `callID`,并加上限 64 与"用后即删" |
| opencode 插件 `sessionId` 恒空 | ⚠️ **仍在**:插件钩子入参未提供会话标识,留空是诚实做法;若需要需从 `client` 侧取 |
| 空内容 `lineDelta` 算 1 | ✅ 已修复为 0 |
| `capsule_set_state` 只写不读 | ✅ **本轮修复**:命令保留取值校验,`StatsInfo.capsuleState` 成为真实读方,设置面板「运行状态」显示胶囊态 |

### 3.4 本轮(2026-09)新发现并已修复的问题

原报告未覆盖,来自"用户点来一道却始终不出卡"的运行时排查:

1. **G6 与入队护栏用了两套指纹口径** —— G6 用「主题 + 近 90s 事件首文件」(批为空时退化成空串),入队护栏用「主题 + LLM 挑的文件」,导致 G6 放行、护栏拒绝,而提示却写「队列已满或同指纹重复」的假二选一,把用户引向一条不存在的死路。修法是指纹拆两层(`fingerprint` 近似 / `fingerprint_text` 出稿真去重),全链路统一调用同一函数,并把三结局分别陈述。
2. **facts 首版逐条事件重复触发** —— `kick_facts_if_needed` 无 in-flight 标记,每条事件都烧一格额度(现场证据:预算 used=28 而卡片仅 2 张)。已加 `facts_inflight` 集合,成功失败都释放。
3. **`Facts` / `AnalysisOut` 严格解析导致整份分析作废** —— LLM 少回一个字段或改用 snake_case 就全案失败,退化为空白模板卡。已改为容器级 `default` + 字段级 `alias` 宽容解析,并对空正文兜底为模板文案。
4. **`shownCards` 非响应式** —— `const shownCards = cards.value.slice(0,3)` 只在 setup 时取一次快照,新卡永远不显示。已改 `computed`。
5. **`settings_put` 部分补丁重置未提交字段** —— 已引入全 `Option` 的 `SettingsPatch`(`baseUrl: Option<Option<String>>` 承载"清空"与"不改"两种语义)。
6. **端点信息只信内存** —— 原来会拿磁盘上的旧 `endpoint.json` 兜底,把"适配器连不通"误导成"端点正常"。
7. **面板可被拖到屏外 / dock 坐标越界后无法唤回** —— 增加 `clamp_dock` / `pick_dock` / `keep_panel_on_screen` 三层几何自愈(含 7 个几何单测)。⚠️ 注意:此功能上线过程中曾发生**误判**——用 WinForms `Screen.Bounds`(DPI 虚拟化逻辑值 1707×1067)去核对 Tauri 物理坐标,得出"用户 dockX=2358 在屏外"的错误结论;改用 DPI 感知 API 重测得物理 2560×1600,该坐标从未出界。几何层保留作防御,但在该机器上实为空转,真实故障原因是 `baseUrl` 配置。

---

## 4. 当前基线与剩余动作

### 4.1 门禁(全部为阻断式,当前全绿)

| 门禁 | 命令 | 当前结果 |
|---|---|---|
| Rust 单测 | `cargo test --locked`(主程序 + claude-code 适配器) | **51 passed / 0 failed** |
| Clippy | `cargo clippy --all-targets --locked -- -D warnings` × 两个 Rust 工程 | 0 告警 |
| 格式 | `cargo fmt --check` | 0 差异 |
| 前端 | `vue-tsc --noEmit` + `vite build` | 通过(`noUnusedLocals`/`noUnusedParameters`/`noFallthroughCasesInSwitch` 已开) |
| 插件类型 | `tsc --noEmit --strict` 单独 job | 通过 |
| 副本一致 | `cmp adapters/opencode-plugin/kvba-plugin.ts .opencode/plugins/kvba-plugin.ts` | 一致 |
| Trae 适配器 | `node --check` + `hooks.sample.json` JSON 校验 | 通过 |

### 4.2 真实遗留(按价值排序)

| # | 事项 | 性质 |
|---|---|---|
| 1 | §1.5「提示词粘回率」本地无信号源,指标不可自动验收 | 产品待决,见任务书 §13.2 |
| 2 | §9.1 面板 `alwaysOnTop` 与规格相反 | 产品待决,见任务书 §13.3 |
| 3 | G5-A 文本启发式误报率未做过灰度观测 | 数据待决,见任务书 §13.4 |
| 4 | 打包产物(三平台 installer)仍未实机冒烟 | 验证缺口 |
| 5 | opencode 插件 `sessionId` 恒空 | 小疵,需 SDK 侧支持 |

> 勘误:本表曾列「§5.1 P0 替换队列是假开关」一条。复核后确认 `Settings.p0_replace_queue` → `SettingsPatch` → `insert_card` 替换分支 → 单测**全部存在**,该条作废。错误源于一处检索方式:用前端驼峰名 `p0Replace` 去 grep Rust 蛇形字段 `p0_replace_queue`,零命中即断言"不存在"。**教训:跨语言契约核对必须两侧各自查规范名,不能拿一侧的序列化名字去猜另一侧。**

### 4.3 第二轮故障定位(2026-09-03,现场证据)

用户报告"仍然无法自动触发"。这次先把运行时证据全部捞出来,再谈代码:

| 证据 | 取值 | 结论 |
|---|---|---|
| 最后一次事件摄入 | `events.log` 内最大 ts = 2026-09-02 15:27 | **约 25 小时零事件流入** |
| 决策日志(今日 15:45–16:16) | 11 条全为 `G7` 之后被拦,原因为新增的 `入队被拒:正文与 1h 内已递出的卡片雷同` | 门控与 LLM 都在正常工作,拦点在**出稿后的正文去重** |
| `budget.json` vs `stats.json` | used=21 / 今日发卡=1 | 20 次 LLM 调用白烧,产出 1 张卡 |
| `projects/` 目录 | 两个目录 `054cd4064e7550fb`、`09c188826a349779`,分别对应 `C:\Users\...\KVBA` 与 `c:/Users/.../KVBA` | **同一项目裂成两套记忆**(`project_key` 未归一化路径) |
| `projects/*/` 内容 | 两个目录都**只有 `events.log`,没有 `facts.json`** | facts 从未成功生成 |
| `.trae/hooks.json` 与 `~/.trae-cn/hooks.json` | **两个文件都不存在** | Trae 适配器从未安装 —— 这才是零事件流入的直接原因 |
| 端口 50479(`endpoint.json` 所记) | 无任何进程监听,`Get-Process` 也查不到 kvba | 取证时 KVBA 未在运行 |

因果链(每一环都独立致命):

```
.trae/hooks.json 从未创建  →  Trae 侧零上报  →  25h 无事件
  → kick_facts_if_needed 只在事件流入时调用  →  facts.json 始终不存在
  → 主题恒为 framework-unknown  →  知识分支每次让 LLM 写同一句 awareness 文案
  → 正文指纹与 1h 内已递出的那张完全相同  →  insert_card 返回 Duplicate
  → 用户点"来一道"只看到"这个话题刚递过同款内容",队列永远 0/3
```

本轮据此修复:

1. **安装 Trae 项目级 Hook**(写 `.trae/hooks.json`,绝对路径指向 `adapters/trae-hook/kvba-trae-hook.mjs`)—— 需用户在 设置 > Hooks 确认启用并新开会话。
2. **`project_key` 增加 `normalize_project_path`**:统一分隔符、折叠重复斜杠、去尾部斜杠,Windows 下再统一小写。补 2 个单测。此前 `C:\a\b` 与 `c:/a/b/` 会裂成两个项目,记忆隔离被路径拼写打穿。
3. **启动时为已恢复的活动项目补生成 facts**(`kick_facts_for_active`):打断"facts 恒空 → 文案恒定 → 被去重挡死"这条链。此前 facts 只在 `events_handler` 里 kick,Agent 一闲置就永不重试。
4. **设置面板「事件链路」行改为可诊断形态**:显示本次进程摄入条数 + 相对时间,并区分三种状态(0 条 = 适配器未接入 / 有历史但 >10min 静默 = 链路曾通后断 / 正常)。原来只有一句"链路正常",只要落过一条事件就永远显示正常,把 25 小时的断流掩盖了。

**仍未解决、且必须用户侧动作的**:KVBA 当前未运行;`.trae/hooks.json` 需经 设置 > Hooks 确认启用;启用后需新开会话(Trae 在会话创建时读取 hook 配置)。

## 5. 结论(对账后)

架构判断(纯函数门控、双 token、信封加密、模板兜底、消费驱动队列)依然成立,是这份代码的主要价值。快照里 3 个 P0、8 个 P1 已**全部关闭**(其中 D8 转为规格差额),P2 有 4 条属误报、其余已修或已澄清。代码库已从"能跑但带病"推进到"门禁全绿、故障链可解释";剩下的风险集中在**产品决策未定**(4 项)与**打包实机验证缺失**,而不是正确性缺陷。
