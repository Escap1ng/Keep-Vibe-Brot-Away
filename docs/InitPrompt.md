# 技术规格与实现任务书:Keep-Vibe-Brot-Away (KVBA)

你是一名资深 Tauri 2 + Rust + TypeScript 桌面工程师。本文件是完整的产品规格与实现任务书。请严格遵守其中定义,不要擅自增加/改动需求,按里程碑顺序推进。代码注释与交互文本使用中文,标识符保持英文。

---

## 1. 我们解决的问题

### 1.1 背景:VibeCoding 把"思考节拍"从人身上剥离了

编程 Agent 全面接管"执行"。开发者的工作流被切成两段:**写提示词/验收(几十秒)** 和 **等 Agent 干活的间隙(几分钟到几十分钟,期间 Agent 持续输出)**。

在这段"间隙"里,人通常在做两件事之一:
- 盯着输出滚动,大脑空转——看完即忘,没有形成任何记忆;
- 走神/玩手机——不仅没学,连"项目正在发生什么"都丢了。

长此以往,衰退是确定的:
- 不知道项目用什么框架、状态管理走哪条链路;
- 说不清最近十几次变更各自为了什么;
- 对 Agent 生成的东西失去"审查能力",只会说"OK,继续"。

我们把这种状态称为 **"氛围编程脑腐":执行权转移走了,认知没有跟着转移——人从"负责架构的人"退化成了"发指令按回车的人"。**

### 1.2 为什么没有工具解决它

现有辅助全部站在"执行"一侧:Agent 本体、IDE 补全、代码审查机器人……它们的产出是"代码"。没人站在"认知"一侧生产"理解"。而独立的学习工具(教程/文档)又脱离项目上下文,无法与"此刻正在写的代码"挂钩。

### 1.3 KVBA 是什么

KVBA(Keep-Vibe-Brot-Away)是一个**项目伴读(Project Sidekick)**:
- 它不接管执行,只接管**间隙**——Agent 忙于干活时,把与当前项目相关的小知识、审查质疑、知情度提问"递"给用户;
- 它不是弹窗广告,而是一个**被用户消费的节奏**:问题队列最多 3 个,用户不回答,它就沉默;
- 它消费的是用户原本已被浪费的等待时间,产出的是持续增长的**对项目的掌控感**。

### 1.4 两个产品行为

1. **递知识**:依据项目记忆(框架/选型/目录结构),向用户分享"此刻项目上下文中的知识点"。例:项目命中 Nuxt 时,分享 Vue 生命周期钩子的特性、组合式 API 的适用场景等;
2. **抛问题 + 闭环**:
   - **审查类**:本次 AI 产出有什么不足——缺少关键注释?异常捕捉不足?接入的搜索 API 是否让你满意?→ 用户可选 满意/不满意/自定义答案;
   - **知情度类**:"你知道这个项目现在用什么框架吗?"——主动测试并修复认知缺口;
   - 不满意 → 自动生成一段**"下一轮优化提示词"**,写入剪贴板+项目内 `.kvba/next-prompt.md`,用户粘贴回 CodingAgent 即完成闭环。

### 1.5 成功度量(产品定义)

使用 2 周以上,用户应能:
- 说出项目的框架、核心链路与最近 5 次变更的动机(知情度测试通过率显著提升);
- 主动回答审查问题的比例 > 60%(证明在"参与"而非"刷手机");
- 生成的"下一轮优化提示词"被粘贴回 Agent 的比例 > 30%(证明问题真实且闭环成立)。**可测性说明:该指标本地无法自动测量** —— 粘回动作发生在 Agent 侧输入框,不经过 KVBA。当前只能测到"提示词已生成并写入剪贴板 + `.kvba/next-prompt.md`"这一半(埋点在 `Stats` 之外,走文件计数),真实粘回率需人工回填或后续由适配器上报一次 `message.updated` 内容比对才能得出。前两项已埋点(`stats.json`:cards_created / answers / unsatisfied,含跨日归并)。

---

## 2. 不可妥协的核心原则

1. **非侵入**:绝不阻塞、绝不抢焦点、绝不中断 CodingAgent;只在等待间隙出现,可随时静默/隐藏。
2. **消费驱动**:问题队列(最多 3 个待答)是唯一触发器——用户不消费,就不产生新提问(知识贴士也一样)。
3. **成本受控**:单次分析强制 ≤5000 token;每日 LLM 调用上限可配置,超限自动降级为"攒批"。
4. **纯本机**:所有模型调用直连用户自配的模型 API(OpenAI/Google/Claude),密钥**本地 AES-256-GCM 加密存储**(密钥由 HKDF 从"机器指纹+随机 salt"派生)。**禁止使用 MD5**。
5. **轻量**:内存占用小、数据文件小、**无主窗口、后台驻留**,单实例。
6. **形态**:常驻托盘 + 悬浮胶囊(always-on-top 小圆点/胶囊),唯一的大面积界面是"按需弹出的面板",不是常驻主窗。

---

## 3. 架构总览

```
┌────────────────────────────────────────────────────────────┐
│  CodingAgent 环境                                            │
│  Claude Code: hooks(PostToolUse/Stop)+ 转录 JSONL watcher  │
│  opencode: TS 插件(事件钩子)                                │
│  两者输出统一"归一化事件"→ POST http://127.0.0.1:<port>/kvba/events
└──────────────────────────┬─────────────────────────────────┘
                           │ HTTP(本地随机 token 鉴权)
┌──────────────────────────▼─────────────────────────────────┐
│  KVBA(Tauri 2, 无主窗口, 托盘+悬浮胶囊)                       │
│  Rust 核心: 事件摄入/项目识别/门控决策树(0 token)/记忆层/   │
│             加密存储/LLM 客户端(SSE)/冷却矩阵                 │
│  TS 前端: 胶囊状态机(Capsule)/弹出面板(队列/设置/历史)       │
│  纯 Rust 能力(托盘/透明置顶窗口/单实例)只暴露 command+event  │
└────────────────────────────────────────────────────────────┘
```

职责边界铁律:**确定性、性能敏感、系统、安全相关 → Rust;产品交互、快速迭代 → TS**。两层只通过 Tauri command/event 通信,禁止互相直接触碰存储。

---

## 4. 事件摄入(截流)

### 4.1 适配器(三个,其一为兜底)

| 适配器 | 方式 | 形态 |
|---|---|---|
| Claude Code 主适配器 | 官方 Hooks:PostToolUse/Stop → `kvba-claude-bridge`(Rust 二进制,stdin 收 hook JSON,POST 上报) | Rust |
| Claude Code 兜底 | 监听 `~/.claude/projects/**/*.jsonl` 增量,解析工具调用与消息(方案 C) | Rust watcher |
| opencode 适配器 | 官方插件 API,订阅事件(tool.execute.after、message.updated 等),POST 上报 | TS 插件 |
| Trae 适配器 | Hooks:PostToolUse/Stop → `kvba-trae-hook`(Node 脚本,读 stdin JSON 后 POST 上报;工具名映射为规范小写名,`RunCommand` 不计写事件) | Node 脚本 |

- 适配器向主程序 `POST /kvba/hello` 以本地随机 token 换取事件上报 token;
- 上报失败重试 3 次后丢弃,绝不影响 Agent;
- 预留扩展点(不在本期):本地反向代理模式(把 Agent Base URL 指到 KVBA 旁路完整 SSE 流),架构上留有 HTTP 事件入口即可。

### 4.2 归一化事件 Schema(JSON)

```json
{
  "schema": "kvba.event.v1",
  "id": "blake3哈希",
  "emitter": "claude-code-bridge | claude-code-watcher | opencode-plugin | trae-hook",
  "eventType": "tool.completed | tool.started | turn.ended | message.updated | session.ended",
  "toolName": "edit | write | patch | think | read | bash | grep | 其他",
  "ts": 0,
  "projectPath": "D:\\Laptop\\MyProjects\\foo",
  "sessionId": "",
  "excerpt": {
    "files": [],
    "diffHead": "统一 diff 摘录,≤6000 字符,超出截断",
    "intent": "从 tool call 参数/think 内容推断的意图,≤500 字符",
    "hasErrorPath": false,
    "hasApiCall": false,
    "todoOrFixme": false,
    "lineDelta": 42
  }
}
```

### 4.3 落盘规则

- 每项目一份滚动日志:`<dataDir>/projects/<sha256(projectPath)>/events.log`(JSONL,仅最近 200 条,单条截断,diff 摘录 ≤6KB);
- 全量文本不落地。

---

## 5. 问题队列(消费驱动)

### 5.1 队列规则

- 最多 **3 张 pending**;队列满 → **一切触发停止**(审查问答与知识贴士都不触发),零 LLM 调用;
- 排泄口:① 回答(满意/不满意/自定义) ② 手动跳过 ③ "不再问这类"(写入主题冷却矩阵) ④ TTL 2 小时过期 ⑤ 卡片引用文件后来被 Agent 再次修改 → 自动标记"已过时"退场;
- 可选(设置项,默认关):severity=P0 的审查卡在队列满时可**替换**最旧一张(总数仍 ≤3)。已实现:`Settings.p0_replace_queue` → `insert_card` 满队列时替换最旧 pending 并归档(被替换卡记为 `dismissed`,有单测覆盖)。
- 手动按钮"来一道":旁路 G2 频控、G3 活性、G4 路由与 G6 去重,**不破队列上限(G1)**。
- **开场破冰**:每个会话的首次评估放行一张知识贴士(旁路 G2 间隔/同主题冷却与 G6 近似)。它解决的是"对话一开始必然零输出"——旧口径下 G4 只认写事件与 `turn.ended`,而 Trae/Claude Code 的 `turn.ended` 只在整轮结束时才发,于是第一轮进行中永远不出卡。破冰**不旁路** G1、G3、用户显式静默期与出稿正文护栏,且按 session 记账,一次会话最多一次。

### 5.2 卡片 Schema

```json
{
  "id": "uuid",
  "type": "review | tips | awareness",
  "severity": "info | P1 | P0",
  "text": "问题/贴士正文",
  "options": ["满意", "不满意", "自定义…"],
  "theme": "冷却矩阵主题,如 vue-lifecycle / api-error-handling / framework-unknown",
  "refFiles": [],
  "refEventId": "",
  "createdAt": 0,
  "status": "pending | answered | dismissed | stale",
  "answer": "",
  "optimizedPrompt": "不满意时自动生成的下一轮优化提示词"
}
```

### 5.3 下一轮优化提示词

用户点"不满意"或填入自定义答案时,由 LLM 生成(可与卡片同一次调用,超预算则单独小调用 ≤800 token):
1. 写入系统剪贴板;
2. 追加到项目内 `.kvba/next-prompt.md`;
3. 文案格式:`修复 <引用文件/函数>:<描述问题>。<要求/约束>。`——一段可直接粘贴给 CodingAgent 的指令。

---

## 6. 门控决策树(全部 0 token,确定性,Rust 纯函数)

**必须实现为纯函数**:`fn evaluate(ctx: GateContext) -> GateVerdict`。输入:事件批 + 项目事实 + 队列状态 + 冷却矩阵;输出:通过并给出分支(A=审查 / B=知识)或拒绝(含层级 Gx 与原因)。配事件夹具单元测试。提供**决策日志面板**展示每次评估:Gx 层级、拒绝原因、耗时。

```
输入: adapter 事件批(近 90s)+ 队列状态 + 项目事实 + 冷却矩阵 + 破冰标记
G1 队列硬闸   pending >= 3                       → 拒绝(什么都不生成)
G2 频控       距上次弹卡 < 25s 或同主题冷却中      → 拒绝(force / 破冰旁路这两条)
              主题静默(用户"不再问这类"或连续 2 次不满意)→ 任何路径都拒绝
G3 活性       近 90s 无有效 agent 事件            → 拒绝(防空转烧钱)
G4 路由       写事件且 G5-A 达标 → 分支A;否则含 turn.ended/think 或 force 或破冰 → 分支B;
              写事件达标失败且无 turn.ended 可回落 → G5-A 拒绝
G5-A 审查分支(信号分,**只统计写事件**,阈值 40)
  加分: lineDelta≥20(+30)| lineDelta≥8(+15)| 新增 API/网络调用(URL 或 http 库 import)
        | 新增异常路径未包 try/catch | 含 TODO/FIXME | 删除代码文件 | 裸 throw 错误串
        | 影响 ≥3 个文件(+10)
  减分: <5 行纯格式化/重命名 | 纯 README/文档变更 —— **整批只扣一次**
  得分≥40 → 审查问题;若含"网络调用无异常处理"或"覆盖不完整" → severity=P0
G5-B 知识分支
  知识缺口(框架未知)→ awareness 知情度问题,最高优先(破冰卡**不得**出这一型,见铁律 4)
  命中已有主题 → 在【项目主题 + 通用主题池】里取"最久未问"的那个(轮换)
  冷却: 同主题 10min;同主题连续 2 次"不满意" → 该主题静默 1h(自动的"不再问这类")
G6 去重       blake3(分支+主题+卡片正文) 与队列/历史重复 → 拒绝(force / 破冰豁免这一层近似)
G7 执行       通过 → 仅在此处调用 LLM(单次 ≤5k)→ 生成卡片 → 入队 → 胶囊展开
```

**开场破冰(每个 session 一次)**:某会话首次评估时,即使批内只有读事件、即使刚弹过卡,
也放行一张知识贴士。依据是主动干预研究的共识——投递成本取决于「用户此刻的负载」,
而会话开场正是用户刚提交完提示、必然在等的自然断点。它**不旁路** G1 队列上限、G3 活性、
用户显式静默期与出稿后的正文护栏;判定即记账,所以一个会话最多破冰一次。

### 6.1 实现口径与偏差记录(代码落地后回写,以代码为准)

| 规格条目 | 实现口径 | 偏差说明 |
|---|---|---|
| G1 队列硬闸 | `queue::MAX_PENDING = 3`,判 `pending >= 3` | 无。**`force` 与破冰都不旁路的唯一闸门** |
| G2 频控 | 距上次弹卡 < 25s 拒绝;同主题冷却 10min;静默 1h。`force` 与破冰旁路前两条 | 2026-09-03 由 60s/30min 收紧到 25s/10min。**静默期(用户否决)任何路径都不旁路** |
| G3 活性 | `ACTIVITY_WINDOW_SECS = 90`,以最近一次**成功摄入**的事件为准 | 摄入层判重丢弃的重复事件**不**刷新活性——否则 bridge+watcher 双报会把空闲期伪装成活跃期 |
| G4 路由 | 写事件**且 G5-A 达标** → 分支A;否则 `turn.ended`/`think`/`force`/破冰 → 分支B | 旧实现是「有写事件就先落进审查」,不达标直接 G5-A 拒绝,于是同批的 `turn.ended` 被整批浪费(实测 8 次 G5-A 拒绝时 events.log 里躺着 3 条 `turn.ended`)。现改为**不达标即回落知识分支** |
| 开场破冰 | `GateContext.opener`,由 `ProjectLive.opener_session` 按 session 记账;旁路 G2 间隔/同主题冷却与 G6 近似指纹 | **判定即记账**:一次评估只破冰一次,否则每条事件都拿着旁路令牌,频控形同虚设。破冰卡强制走主题轮换、**不出知情度卡**(铁律 4) |
| G5-A 打分口径 | **只统计写事件**;罚分按批结算(整批最多各扣一次) | 旧实现对批内每条事件打分,"读 4 个 .md"就累出 -100,把同窗口一次真实大改(+82)压到阈值以下 |
| G5-A 加分「注释密度骤降」 | **未实现** | 纯文本启发式跨语言误报率高,且与「异常路径未包裹」重复计分。原评审 R4 已把它列为激进启发式 |
| G5-A 加分「删除被依赖文件」 | 降级为「删除代码文件 +15」 | 0 token 原则下不建依赖图,且调 `git` 属侵入,只能按后缀判定 |
| G5-A 减分「与 10min 内已问事件同 hash」 | **未实现**,意图由两层承担:摄入层 emitter 无关内容判重(5s)+ G6 出稿正文指纹(20min 窗口) | 打分层再放第三套 hash 窗口必然出现口径不一致 —— 「卡片进了队列却看不见」那次故障的根因正是 G6 与入队护栏用了两套指纹 |
| G5-B 主题选择 | `pick_knowledge_theme`:候选 = 项目命中主题 + `TIP_POOL`,取静默期之外"最久未问"的那个 | 旧实现每次只给项目命中主题,同主题冷却与正文去重直接把知识分支堵死(用户点"来一道"只看到"刚递过同款内容") |
| G6 去重 | `blake3(分支 + 主题 + 卡片正文前 120 字小写)`,与攒批、入队护栏**共用同一函数**;历史窗口 20min | 判据从「问题 stub + 相关文件」改为**出稿正文**:文件集合由 LLM 每次挑、不稳定,拿它当键会漏判;用户可感知的重复是"同一句话递两遍"。LLM 之前另有一层近似指纹(`分支+主题+refFile`)仅用于攒批去重 |
| G6 与 force/破冰 | 两者都**豁免 G6** 这层主题级近似 | 用户主动索要、或会话开场第一次递话时以"给得出内容"优先;若正文确实雷同,入队护栏返回 `Duplicate` 并如实记入决策日志 |
| G7 结局回写 | 入队三结局 `Inserted` / `QueueFull` / `Duplicate` 分别回写决策日志与用户提示 | 早期版本合并成"队列已满或同指纹重复"的假二选一,把用户引向一条不存在的出路 |
| G7 预算 | 一次分析最多占 **1 格**当日额度;契约解析失败的严格重试走不计预算通道 | 早期实现最坏一次分析吃 2 格 |
| 决策日志 | 内存 `DECISION_LOG_KEEP = 200`(超 400 修剪)+ JSONL 追加落盘 | 拒绝原因带 LLM 真实错误(截 180 字),不再只写"失败" |

---

## 7. 记忆层(两层)

### 7.1 项目全局记忆(按项目路径隔离)

- 内容:框架、技术选型、目录结构概要、关键入口/数据流、Agent 声明过的重要决策、用户回答过的知情度事实;
- 初始化:项目首次出现事件时,本地扫描(package.json/README/目录树限量深度)→ 一次 LLM 调用生成首版,存 `<dataDir>/projects/<hash>/facts.json`(上限 50KB);
- 增量精炼:**每消费 ≥20 次问题 或 项目会话结束时**,才调用一次 LLM 增量合并(固定提示词模板);
- 禁止对 facts 做"每触发一次"的重写。

### 7.2 对话记忆(滚动窗口)

最近 **3 个事件**的原始截断副本(每条 ≤3KB),纯滚动替换,**零 LLM 成本、不压缩**。

### 7.3 单次分析预算(强制 ≤5k token)

- 固定结构:`system(职责+输出 JSON 契约) + 项目事实(截断) + 最近3事件截断 + 当前 diff 摘录 + 门控上下文(分支、信号分、P0 标识)`;
- 核心层 ≈2k token;仅当 diff 大/信号复杂才弹性补到 ≤5k;各字段按字符预算截断,确保绝不超。

> **实现口径更正(第四轮)**:本条约束的是**实际消耗**,而实际消耗由**输入侧**决定 —— `system` ≈0.6k、`user` 经 `fit_tokens` 裁到 ≤3.5k,这两处裁剪全部保留。
> 输出侧**不再下发 `max_tokens`**(`llm::MAX_OUTPUT_TOKENS = None`,由 provider 取自身上限)。理由:契约答案本身只有 300–600 token,任何手写数字都只是在制造「够不够大」的猜疑 —— 写大了撞各家硬上限换 400,写小了就是静默截断。上一轮 facts=400 / 卡片=800 两个数各自为政,直接导致一次错误归因。
> 输出失控的兜底不靠这个上限,靠解析后的硬截断(`text > 120 字` 裁掉)。

### 7.4 分析输出契约(LLM 返回 JSON)

```json
{
  "type": "review | tips | awareness",
  "severity": "info | P1 | P0",
  "theme": "如 vue-lifecycle / api-error-handling / framework-unknown",
  "text": "≤120字",
  "options": ["满意", "不满意", "自定义…"],
  "refFiles": ["..."],
  "optimizedPrompt": "仅 review+不满意时有值"
}
```

---

## 8. LLM 分析器

- Provider 抽象:一个接口,三种实现:`openai`(openai-compatible)、`google`(gemini streaming)、`anthropic`(claude),均走 SSE 流式;支持自定义 BaseURL(便于代理/中转);
- 密钥管理:设置面板填写;加密写 `<dataDir>/secrets.enc`(AES-256-GCM,密钥由 HKDF(machine_id, salt)派生);机器指纹:Windows 用 MachineGuid,macOS 用 IOPlatformUUID,Linux 用 /etc/machine-id;
- 预算:每日调用次数上限(默认 80 次/日,可配置);超限进入"攒批"降级(只累计信号,不调 LLM),托盘气泡提示一次;
- 容错:分析调用后台执行,失败指数退避重试 ≤2 次,绝不阻塞主流程。

---

## 9. 前端形态:托盘 + 悬浮胶囊(无主窗口)

### 9.1 窗口形态

| 形态 | 说明 |
|---|---|
| 悬浮胶囊(常驻) | 无边框、透明、**always-on-top**、skip taskbar、默认不可聚焦(置顶≠抢焦点)。子状态见 9.2 |
| 弹出面板(按需) | 从胶囊展开的轻量面板:队列列表 + "来一道"按钮;失焦自动收起;非置顶仅临时浮层 |
| 设置/历史(按需) | 打开的小窗口,用完即关;不常驻 |

### 9.2 胶囊状态机(Capsule,TS 前端核心组件)

- **idle(收缩)**:小圆点(约 36px,实现取 48×48 无边框透明窗)默认停靠屏幕右缘中部;可拖动,靠近屏幕边缘(24px 内)自动吸附,多显示器按圆点所在屏计算边界并钳回工作区;静态渲染,内容不变不重绘(低能耗);
- **active(展开)**:新卡到达 → 自动展开为"胶囊横幅"(约 320px 宽),展示当前卡片:text、ref 文件、动作按钮(满意/不满意/自定义…/跳过/不再问这类)、上/下条切换角标(切换查看队列中其余卡);
- **popup(面板)**:用户点胶囊"队列"入口 → 展开队列列表弹层(最多 3 张,可点某张回到 active 态);
- 任何时刻右键菜单:暂停/恢复提问、来一道、打开设置、隐藏胶囊(仅托盘图标)、退出;
- 防侵入设计:
  - 置顶但不抢焦点:窗口 `set_focus` 仅在用户点击后发生;动画/展开不得激活窗口;
  - 点击穿透动态开关:胶囊处于 idle 且在平视区边缘时,非交互区全区域 `set_ignore_cursor_events(true)`,用户靠近(悬停检测)再恢复交互。**实现决策:本项不做**。理由:①悬停检测需要轮询窗口坐标或常驻 mouse-hook,与"非侵入、轻量"原则冲突;②权限已在 `capabilities/default.json` 声明但全仓无调用点,圆点窗口已缩到 48×48≈圆点本体,非交互区接近 0,穿透收益远小于代价;③原评审已指出该开关存在"穿透后收不回自身"的自锁问题。若将来要做,前置条件是给出**无轮询**的靠近检测方案;
  - 遮挡防护:默认停靠避开屏幕中心 2/3;支持"隐藏胶囊"退化为托盘图标;
  - 系统通知仅作为兜底:胶囊被隐藏时才用 toast 提醒,平时**不双通道**;
- 平台注意:Windows 透明+置顶常规实现;macOS 设 window level = floating;Linux X11 正常,Wayland 下透明/置顶依赖合成器,文档标注兼容性限制。

### 9.3 UI风格
- 使用material设计风格，结合包豪斯设计要素（简约连贯），动效设计参考.opencode/skills中给出的animate设计哲学。

---

## 10. 工程结构与目录建议

```
keep-vibe-brot-away/
├─ src-tauri/                  # Rust
│  ├─ src/
│  │  ├─ main.rs / lib.rs      # 单实例、命令注册表、setup 钩子
│  │  ├─ state.rs              # Core 中枢 + 持久化 + stats/endpoint/facts 查询命令
│  │  ├─ windows.rs            # 三窗口、拖拽吸附、工作区钳制、capsule_set_state
│  │  ├─ tray.rs               # 托盘菜单(暂停/恢复文案随状态翻转)
│  │  ├─ settings.rs           # settings_get/settings_put(字段级补丁,不误重置)
│  │  ├─ gate/                 # 决策树纯函数 + 单元测试
│  │  ├─ events/               # 归一化 schema、摄入、内容判重、滚动日志
│  │  ├─ memory/               # facts 读写与精炼、对话窗口
│  │  ├─ llm/                  # provider 抽象、流式解析、预算与攒批
│  │  ├─ crypto/               # AES-GCM + HKDF 机器指纹
│  │  ├─ queue/                # 队列状态机、冷却矩阵、维护任务
│  │  └─ http/                 # 本地 HTTP 端点(tokio/axum)
│  └─ Cargo.toml
├─ src/                        # Vue3 + Vite + TS(无 UI 组件库,令牌见 styles/tokens.css)
│  ├─ components/              # CapsuleDot / CapsulePanel / SettingsView
│  └─ lib/                     # api.ts(invoke 封装)、types.ts(与 Rust 契约对齐)
├─ adapters/
│  ├─ claude-code/             # Rust 工程:src/bin/bridge.rs(hook stdin→POST)
│  │                           #        src/bin/watcher.rs(JSONL 增量兜底,方案C)
│  │                           #   注意:watcher 是同一 Cargo 工程的第二个 bin,不是独立目录
│  ├─ opencode-plugin/         # TS 插件(kvba-plugin.ts);.opencode/plugins/ 下为同步副本,CI 校验一致
│  └─ trae-hook/               # Node 脚本适配器(kvba-trae-hook.mjs + hooks.sample.json)
└─ docs/
```

Tauri command/event 清单(实际注册 24 条,勿改同名语义):

- 队列与门控:`queue_listen()`(返回当前 pending 卡片数组,增量走 `card.new` / `card.updated` / `card.dismissed` 事件)、`queue_answer(cardId, answer)`、`queue_dismiss(cardId)`、`queue_mute_theme(cardId)`、`gate_tick(force?)`(默认 `force=true`,即"来一道");
- **`queue_mute_theme` 与规格草案的 `queue_mute_theme(theme)` 不同**:入参是 **cardId**,服务端从该卡片反查 `theme` 再写入冷却矩阵。理由是 UI 只有卡片句柄,让前端传主题名会把"主题"这个后端私有概念泄漏成第二真源;
- 胶囊与窗口:`capsule_set_state(capsuleState)`(`idle`/`active`/`popup`,前端状态机上报,设置面板「运行状态」读取用于诊断)、`dock_snap`、`panel_open` / `panel_close`、`capsule_hide` / `capsule_show`、`settings_open`;
- 配置与诊断:`settings_get` / `settings_put`(字段级补丁)、`secrets_put` / `secrets_test`、`llm_test_connection`、`budget_info`、`stats_info`、`endpoint_info`、`facts_get`、`decision_log_list`、`history_list`、`quit_app`。

---

## 11. 非功能要求

- 内存:稳态 < 100MB;数据文件:每项目 < 500KB;
- 启动即后台驻留(托盘+胶囊),不弹任何窗口;支持开机自启(可选);
- 单实例(tauri-plugin-single-instance);
- 本地 HTTP 仅绑定 127.0.0.1,请求头带握手随机 token;
- 胶囊 idle 态与 Agent 输出区不重叠;所有对外文案中文。

---

## 12. 里程碑(严格按序,每个里程碑结束后进行最小功能单元测试自检)

- **M1 骨架**:Tauri2 工程;托盘 + 悬浮胶囊 idle 态(透明置顶圆点、拖拽、边缘吸附、可隐藏为托盘)可运行;
- **M2 事件摄入**:本地 HTTP 端点;claude-code bridge(含 hooks 配置说明)、opencode TS 插件、Trae hooks(node 脚本)三路适配器上报;events.log 滚动落盘 + emitter 无关内容判重;验收:真实 Agent 会话可见事件入队日志;
- **M3 决策树**:纯函数+夹具单测(覆盖 G1 满队列、G2 冷却、G4 分支、G5 打分、G6 去重、P0 判定);决策日志面板可看每次评估(含 G7 真实结局);
- **M4 记忆层**:facts 扫描/首版生成/增量合并;对话滚动窗口;验收:同一项目两次会话记忆连续;
- **M5 LLM 分析器**:三 provider + SSE + 密钥加密 + 每日预算;契约输出解析与重试(一次分析最多占 1 格额度)、预算耗尽攒批 + 恢复补发;
- **M6 胶囊交互**:idle/active/popup 状态机全功能;回答/跳过/不再问这类;TTL/过时;"不满意→剪贴板+next-prompt.md";点击穿透保护(**经决策取消**,理由见 §9.2);
- **M7 打磨**:右键菜单、暂停/恢复、设置/历史面板、单实例、Windows/macOS/Linux 打包;README(安装 + 三路适配器配置说明)。

---

## 13. 待决事项(实现与规格的差额,需产品侧拍板)

| # | 事项 | 现状 | 需决定 |
|---|---|---|---|
| 1 | ~~§5.1「P0 可替换队列最旧卡」是假开关~~ | **本条作废**:复核后确认 `Settings.p0_replace_queue`、`SettingsPatch`、`insert_card` 替换分支与单测均在,功能完整。此前判断源于一处错误的检索(用前端驼峰名 `p0Replace` 去 grep Rust 蛇形字段 `p0_replace_queue`) | 无需决定;若要让被替换卡不再复用 `dismissed` 状态,可另立需求 |
| 2 | §1.5 第三项度量(提示词粘回率 > 30%) | 本地无信号源,只能测到"提示词已生成" | 是否接受人工回填,或要求适配器多报一路 `message.updated` 做文本比对(成本:多一条事件 + 一次 hash 比对) |
| 3 | §9.1 面板置顶 | 规格要求面板"非置顶仅临时浮层",实现为 `alwaysOnTop: true` | 保持现状(面板可浮在 IDE 之上)还是回到规格语义。改回需同步处理"面板被 IDE 遮住看不见"的回归 |
| 4 | G5-A 激进启发式 | 「注释密度骤降」未实现;其余文本启发式仍在(阈值 60) | 是否需要灰度观察期,先靠决策日志统计误报率再调分值 |