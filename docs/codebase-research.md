# KVBA 代码库研究报告(基于实际代码的再评估)

## 摘要

远程仓库已含一套**完整可运行的首个版本**(提交 `2394475`),规格书 M1–M7 的主干功能基本落地:门控决策树有纯函数实现与 16 个夹具单测、三窗口胶囊交互、双 token 事件接入、AES-256-GCM 密钥加密、每日预算均已就位。此前评审的 8 处缺口中 **3 处已被代码解决、2 处被巧妙规避、1 处部分缓解,仅剩 2 处真正缺失**;5 大风险中 2 个消解、1 个降级为注意点。但深读发现了 **3 个 P0 级正确性 Bug(watcher 项目路径推导错误、半行数据丢失、新卡秒变"已过时")**和一批规格偏差与死代码。本报告给出全部证据与修复优先级。

---

## 1. 现状概览

### 1.1 代码结构(实际)

| 层 | 位置 | 状态 |
|---|---|---|
| Rust 核心 | [src-tauri/src](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src) | gate/events/queue/memory/llm/crypto/http/tray/windows/settings/state 全部存在 |
| Vue3 前端 | [src](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src) | App.vue + CapsuleDot/CapsulePanel/SettingsView + tokens.css 设计令牌 |
| Claude Code 适配器 | [adapters/claude-code](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/adapters/claude-code) | bridge + watcher 双 bin,hooks 样例齐全 |
| opencode 插件 | [adapters/opencode-plugin/kvba-plugin.ts](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/adapters/opencode-plugin/kvba-plugin.ts) | 211 行,三钩子完整;`.opencode/plugins/` 下有同步副本 |
| 文档 | [README.md](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/README.md) | 安装、hooks 配置、插件安装三节齐全 |

### 1.2 里程碑完成度判定

| 里程碑 | 判定 | 依据 |
|---|---|---|
| M1 骨架 | ✅ 完成 | 托盘 + 48px 透明置顶圆点、拖拽、24px 边缘吸附、隐藏为托盘([windows.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/windows.rs)、[tauri.conf.json](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/tauri.conf.json)) |
| M2 事件摄入 | ⚠️ 完成但有 2 个 P0 Bug | HTTP 端点、双适配器、events.log 滚动落盘都在;watcher 路径/半行问题见 §4 |
| M3 决策树 | ✅ 完成 | G1–G7 纯函数 `evaluate` + 16 个夹具单测([gate/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/gate/mod.rs) L514-802) |
| M4 记忆层 | ⚠️ 完成但有缺陷 | facts 首版/精炼/50KB 上限齐备;收缩丢数据、跨项目混采见 §4 |
| M5 LLM 分析器 | ✅ 基本完成 | 三 provider、SSE、AES-GCM 加密、每日预算 80;"攒批"名存实亡见 §3 |
| M6 胶囊交互 | ⚠️ 完成但有 1 个 P0 Bug | 三态、回答/跳过/静默、TTL、next-prompt 闭环齐备;stale 误判见 §4 |
| M7 打包 | 🟡 大部分完成 | 右键菜单/暂停/设置/历史/单实例齐;打包产物未验证,托盘文案小疵 |

**结论:这是一个"能跑、架构忠实于规格、但带病"的版本。工作重心应从"建设"转向"修复 + 验证"。**

---

## 2. 此前评审结论的核实(5 风险 8 缺口)

### 2.1 五大风险

| 风险 | 现状 | 证据 |
|---|---|---|
| R1 点击穿透悖论 | ✅ **被设计规避**:圆点窗口缩到 48×48≈圆点本身,非交互区≈0,从根上取消穿透需求;穿透权限已声明但全仓无调用 | [CapsuleDot.vue](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src/components/CapsuleDot.vue) L2-3 注释;[capabilities/default.json](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/capabilities/default.json) L16 |
| R2 窗口形态 | 🟡 **选择了多窗口**(3 个:圆点/面板/设置),非单窗口变形。透明置顶窗口有两个常驻,内存需实测 | [tauri.conf.json](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/tauri.conf.json) L14-54 |
| R3 diff 提取 | ✅ **采用轻量方案**:不跑 `git diff`,`diffHead` 取 tool_input 中 diff/patch/newString/content 最长者,`lineDelta` 为摘录行数 | [bridge.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/adapters/claude-code/src/bin/bridge.rs) L49-79 |
| R4 G5-A 激进启发式 | ❌ **仍为激进实现**:注释密度骤降(+15)、异常路径未包裹(+25)等纯文本判断全部保留,误报风险仍在 | [gate/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/gate/mod.rs) L134-227 |
| R5 成功度量 | ❌ **仍不可验收**:Stats 仅内存三计数、未序列化、不落盘;`total_today` 是死字段 | [state.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/state.rs) L145-150 |

### 2.2 八处缺口

| # | 缺口 | 现状 | 证据 |
|---|---|---|---|
| 1 | 框架矛盾 | ✅ **已解决**:Vue3 + `.vue` 组件,无 TSX | [package.json](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/package.json) |
| 2 | 评估触发时机 | ✅ **已定**:每条事件即时触发管线,节流靠 G2(60s)+ G3(90s 活性),无去抖 | [http/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/http/mod.rs) L98 |
| 3 | 端口/配对发现 | ✅ **已解决**:随机端口 + `<dataDir>/endpoint.json`(port/helloToken/eventToken),双适配器均读取 | [http/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/http/mod.rs) L34-50 |
| 4 | 攒批出口 | ❌ **仍缺失**:预算耗尽直接丢弃分析,无缓冲、无补发,"攒批"仅是文案 | [llm/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/llm/mod.rs) L150 |
| 5 | 无 Key 冷启动 | 🟡 **部分缓解**:未配置时发确定性模板卡 + 模板版优化提示词,闭环不断;但无主动引导通知 | [llm/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/llm/mod.rs) L54-79 |
| 6 | salt 位置 | ✅ **已定**(方案不同):salt 随信封内嵌于 `secrets.enc`,每次保存重新随机 | [crypto/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/crypto/mod.rs) L66-126 |
| 7 | 哈希混用 | ✅ **与规格一致**:事件指纹/去重/知识池用 blake3,项目目录用 sha256 | [events/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/events/mod.rs) L88-94、[state.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/state.rs) L46-52 |
| 8 | 面板置顶偏差 | ❌ **新偏差**:规格 §9.1 要求面板"非置顶仅临时浮层",实际 `capsule-panel` 声明了 `alwaysOnTop: true` | [tauri.conf.json](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/tauri.conf.json) L31-44 |

### 2.3 悬空引用复核

- `.opencode/skills` 动效哲学文件**依然不存在**(`.opencode/` 下仅有 `plugins/kvba-plugin.ts`);但 [tokens.css](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src/styles/tokens.css) 已自成一套动效令牌(150/220/300ms、强 ease-out、reduced-motion 降级),事实上形成了标准,可视为已落地替代。

---

## 3. 深读新发现的问题清单

### 3.1 P0 —— 功能正确性,必须修

| # | 问题 | 位置 | 影响 |
|---|---|---|---|
| B1 | **watcher 项目路径推导错误**:`cwd` 只做了 `jsonl_path.parent().parent()`,得到 `~/.claude/projects` 而非真实项目;`transcript_path` 未读取、目录名未解码 | [watcher.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/adapters/claude-code/src/bin/watcher.rs) L86-91 | 兜底源事件全部归错项目,记忆隔离失效 |
| B2 | **watcher 半行丢失**:文件写入中 `read_line` 返回无换行残行,偏移照样前移,该行永久丢失 | [watcher.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/adapters/claude-code/src/bin/watcher.rs) L45-83 | 静默数据丢失;需"未遇换行不提交偏移" |
| B3 | **stale 检测无时间戳校验**:只比对最近 30 条事件文件与 `refFiles`,卡片本身由这些事件生成,首次清扫(60s)即可能把新卡标过时;且 `ends_with` 后缀匹配误判同名文件 | [queue/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/queue/mod.rs) L242-274 | 卡片莫名消失,用户困惑 |

### 3.2 P1 —— 规格行为缺失 / 数据正确性

| # | 问题 | 位置 |
|---|---|---|
| D1 | **攒批无出口**:预算耗尽丢弃分析,无信号缓冲与次日补发 | [llm/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/llm/mod.rs) L150 |
| D2 | **每事件全量落盘 5 个文件**(settings/queue/cooldown/budget/decision-log),高频事件下磁盘写入放大,无去抖 | [state.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/state.rs) L357-379、[http/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/http/mod.rs) L219-224 |
| D3 | **TTL/stale 卡不进 history**:只改状态留在 queue,重启后静默丢失,历史面板不可见 | [queue/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/queue/mod.rs) L206-216、L219-240 |
| D4 | **stats 不落盘、`total_today` 死字段**:§1.5 验收指标无从测量 | [state.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/state.rs) L145-150 |
| D5 | **facts 收缩清空 `awareness_facts`**(用户亲口回答的知情度事实永久丢失);合并信号采集了**所有项目**的最近事件,跨项目污染 | [memory/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/memory/mod.rs) L38-44、L210-217 |
| D6 | **决策日志提前记 Pass**:LLM 执行前即记 `"G7"/pass=true`,LLM 失败后不回写,日志与真实结局不一致 | [http/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/http/mod.rs) L198-225 vs L239-245 |
| D7 | **opencode 插件端点只在加载时读一次**:KVBA 晚启动则整个会话静默失效,需重启会话 | [kvba-plugin.ts](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/adapters/opencode-plugin/kvba-plugin.ts) L122 |
| D8 | **G5-A 激进启发式保留**(见 R4),跨语言误报风险未处理 | [gate/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/gate/mod.rs) L134-227 |

### 3.3 P2 —— 一致性 / 小疵 / 死代码

- `parse_contract` 取"首个 `{` 到最后一个 `}`",非平衡括号扫描,注释与实现不符([llm/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/llm/mod.rs) L193-201);text 兜底截 140 字与契约 ≤120 字不一致;严格重试再计一次预算(最坏 1 次分析耗 2 次额度)。
- macOS 机器指纹注释写 IOPlatformUUID,实现读 `/etc/hostname`([crypto/mod.rs](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/src-tauri/src/crypto/mod.rs) L36-39)。
- bridge:`stop_hook_active` 按字符串读(实为布尔),Stop 事件 intent 恒空;`common.rs` 中 id 哈希的 ts 与最终上报 ts 可能不同步。
- opencode 插件:`blake16` 实为 FNV-1a(名不副实但功能可用);`sessionId` 恒空;空内容 `lineDelta` 算 1;`lastArgs` 以工具名为键有并发覆盖风险。
- 死代码:`gate::dedup_check` 无调用点;`core.recently_asked` 只写不读;前端 `capsule:panel-head` 死监听。
- 托盘"暂停提问"文案不随状态翻转为"恢复提问"。
- [dev.cmd](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/dev.cmd) 与 [hooks.settings-sample.json](file:///c:/Users/ycq/Desktop/GH%20more/KVBA/adapters/claude-code/hooks.settings-sample.json) 含他人机器硬编码路径(`D:\nodejs\...`、`C:\Users\admin\...`),README 仅一句带过。
- `endpoint.json` 明文含 eventToken:同机可接受,但建议 README 注明"删除该文件即重置配对"。
- 单元测试仅覆盖 gate 模块(16 例),queue/memory/llm/events 零测试。
- watcher 安全阀注释"200 条"实为 1MB 字节;且只发 `tool.completed`、不发 `turn.ended`,与 bridge 并存时同一调用双报(靠主程序去重兜底)。

---

## 4. 建议的修复优先级(供后续 plan 使用)

**第一批(正确性):** B1 watcher 路径解码(读 `transcript_path` + 目录名反解)→ B2 半行保护(未遇 `\n` 不提交偏移)→ B3 stale 加 `event.ts > card.created_at` 校验并改精确路径比对。

**第二批(规格补齐):** D1 攒批缓冲 + 次日补发、D3 过期/过时卡归档进 history、D4 stats 持久化 + 按日重置、D6 决策日志在 LLM 后回写结局。

**第三批(健壮性/一致性):** D2 落盘去抖、D5 facts 收缩策略改为"先裁 decisions、保 awarenessFacts"+ 合并限定当前项目、D7 插件端点惰性重读、D8 G5-A 信号保守化、P2 项清扫。

**验证手段:** `cargo test`(gate 单测)→ `cargo clippy --all-targets` → `npm run tauri dev` 观察托盘/圆点 → 读取 `endpoint.json` 后用 curl 伪造 `kvba.event.v1` 事件打 `/kvba/hello`、`/kvba/events` 全链路冒烟 → 打包 `npm run tauri build` 冒烟。

## 5. 结论

代码库对规格书的忠实度很高,架构判断(纯函数门控、双 token、信封加密、模板兜底)大多是加分项。当前价值最高的动作不是加功能,而是**修掉 3 个 P0、补齐 4 个规格行为缺失(攒批/归档/统计/日志回写),并给 queue/memory/llm 补最小单测**,即可达到"可长期自用"的质量线。
