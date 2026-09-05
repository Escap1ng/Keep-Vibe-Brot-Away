# RFC-0003：将 KVBA 从事件触发卡片转向跨 Agent 项目认知层

- 状态：Accepted
- 日期：2026-09-04
- 关联：Closes #3

## 1. 决策摘要

KVBA 从“事件触发卡片工具”升级为“跨 Agent 的本地项目认知账本与变更解释层”。

- **默认模式**：掌控模式（监督 Agent 改动，低打扰）
- **可选模式**：学习模式（用户主动开启，围绕真实认知缺口）
- **核心对象**：从 Event 提升为 WorkUnit/ChangeSet
- **核心产物**：从自由文本卡片提升为带证据的 Insight
- **胶囊定位**：保留为投递入口，不再作为领域核心

## 2. 首要用户与模式边界

### 2.1 首要用户（已决）

首要用户定义为：**需要监督 Agent 的开发者**。

理由：
1. 该用户对“改了什么/为什么这样改/风险是否验证”有刚性需求；
2. 与 KVBA 现有“防腐不侵入”定位一致；
3. 学习目标保留，但改为用户主动开启，避免与自动审查混排。

### 2.2 模式定义（已决）

- **掌控模式（默认）**：输出高价值、可验证、可执行的 Insight；仅高置信风险主动打断。
- **学习模式（可选）**：基于 UserModel 选择单个真实知识缺口解释/提问；不与自动审查流水线混排。

## 3. WorkUnit 权威边界（已决）

Phase 1 采用分层边界：

1. **主边界**：Agent 回合（可稳定跨适配器获取，适合作为聚合窗口）；
2. **权威证据**：Git diff + 测试结果（决定“改动是什么、验证是否充分”）；
3. **升级边界**：若存在提交信息，可将一个或多个回合归并为提交级 WorkUnit。

结论：**回合作为时间/会话边界，diff+测试作为事实权威。**

## 4. 最小 Schema（已决）

```json
{
  "WorkUnit": {
    "id": "wu_<uuid>",
    "projectId": "proj_<hash>",
    "sessionId": "agent_session_id",
    "goal": "string",
    "mode": "control|learning",
    "startAt": 0,
    "endAt": 0,
    "events": ["event_id"],
    "changeSet": {
      "baseRef": "string|null",
      "headRef": "string|null",
      "files": [
        { "path": "string", "additions": 0, "deletions": 0, "hunks": 0 }
      ]
    },
    "tests": [
      { "name": "string", "status": "passed|failed|skipped|unknown", "evidence": "string" }
    ]
  },
  "ProjectModel": {
    "projectId": "proj_<hash>",
    "techStack": ["string"],
    "modules": ["string"],
    "entryPoints": ["string"],
    "decisions": [
      { "id": "dec_<uuid>", "summary": "string", "source": "string", "confirmed": true }
    ],
    "risks": ["string"],
    "updatedAt": 0
  },
  "UserModel": {
    "userId": "local_user",
    "knownConcepts": ["string"],
    "unknownConcepts": ["string"],
    "confirmedDecisions": ["dec_<uuid>"],
    "ownershipDepthByModule": {
      "module_path": "low|medium|high"
    },
    "controlPreference": "strict|balanced|light",
    "interruptPreference": "high-risk-only|normal",
    "updatedAt": 0
  },
  "Insight": {
    "id": "ins_<uuid>",
    "workUnitId": "wu_<uuid>",
    "type": "risk|decision|test-gap|explain|question",
    "conclusion": "string",
    "evidence": [
      {
        "kind": "file|line|diff|test|event",
        "ref": "string",
        "excerpt": "string"
      }
    ],
    "importance": "string",
    "suggestedAction": "string",
    "riskLevel": "info|P2|P1|P0",
    "confidence": 0.0
  },
  "DeliveryPolicy": {
    "mode": "control|learning",
    "minConfidence": 0.0,
    "interruptRiskLevel": "P1|P0",
    "queueBudget": 0,
    "strategy": "push|digest|silent"
  }
}
```

## 5. 交付链路（纵向原型定义）

首条纵向原型统一为：

`事件输入 -> WorkUnit 聚合 -> Insight(含证据) -> 胶囊展示`

原型验收要求：
1. Insight 必须包含 `conclusion + evidence + suggestedAction + confidence`；
2. evidence 至少包含 1 条可定位引用（文件/行号/diff/test 任一）；
3. 胶囊仅作为展示层消费 Insight，不再承载领域推理。

## 6. 20 个真实任务回放评估方案（已固化）

为比较旧方案（逐事件卡片）与新方案（WorkUnit Insight），统一采用：

- 样本：至少 20 个真实任务回放；
- 标注：人工标记“是否值得提醒/解释/提问”；
- 指标：
  - 有证据 Insight 占比；
  - 建议动作采纳率；
  - 无价值打扰率与立即跳过率；
  - 跨 Agent 恢复上下文时间。

该评估作为 Phase 0/1 的阻断门槛，不以发卡数量作为主指标。

## 7. 存储与隐私决策（已决）

1. **存储**：长期从 JSON 迁移到 SQLite 事件库 + 投影模型（兼容本地优先）；
2. **模型出境边界**：
   - 默认只发送最小必要摘要；
   - 源码片段默认本地处理；
   - 允许外发的内容需先经过脱敏/裁剪策略；
3. **主动弹出策略**：仅高置信高风险主动弹出，其余进入摘要队列。

## 8. 分阶段推进

- Phase 0：回放评估与价值验证。
- Phase 1：WorkUnit 聚合与证据链。
- Phase 2：ProjectModel / WorkUnit Context / UserModel 三层记忆拆分。
- Phase 3：核心与 Tauri 展示解耦（预留 CLI/编辑器/日报出口）。
- Phase 4：掌控/学习双模式策略化投递。

### Phase 1 首个纵向切片（2026-09-05）

- `turn.ended`/有效 `session.ended` 严格按摄入顺序收口 WorkUnit；适配器必须串行投递，避免用秒级时间戳猜测相邻回合归属；
- 收口后在后台读取本地 Git 工作区快照；工作区干净或采集失败时，可降级为 Agent 写事件声明的文件证据，失败与不完整状态显式保留且不阻塞上报端；
- 确定性生成 `explain`、`test-gap`、`risk`、`decision` Insight，每条都必须引用 file/test/event 证据；
- 全量领域对象分别滚动保存到 `work-units.json` 与 `insights.json`，胶囊只消费一条按 `risk → decision → test-gap → explain` 选择的主洞察投影；
- 普通摘要仅进入队列，仅高置信 P1/P0 主动展开。

当前限制：Git 证据是“回合结束时工作区相对 HEAD 的快照”，尚未建立回合开始基线，因此不能覆盖本回合内已经提交的变化，也不能将共享工作区中的全部变化归因给当前 Agent。Claude JSONL watcher 尚不产生 `turn.ended`，暂走旧门控兼容路径。ProjectModel/UserModel、SQLite、动作闭环和 20 个真实任务回放仍属于后续阶段。

## 9. 非目标

- 不一次性重写现有系统；
- 不自动接管 Agent 执行；
- 不替代测试、静态分析和完整代码评审；
- 不在 WorkUnit 价值验证前盲目扩展更多适配器。

