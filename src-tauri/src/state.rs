//! 全局状态中心:所有数据都经由此处串行化访问,并负责持久化路径分发。
//!
//! 数据目录布局(KVBA 所有用户数据):
//! ```text
//! <dataDir>/
//! ├─ settings.json        设置(非机密)
//! ├─ secrets.enc          密钥(AES-256-GCM 加密,密钥由 HKDF(机器指纹,salt) 派生)
//! ├─ queue.json           待答队列 + 历史
//! ├─ cooldown.json        主题冷却矩阵
//! ├─ budget.json          当日 LLM 调用预算快照
//! ├─ stats.json           运行统计(当日 + 历史累计)
//! ├─ batch.json           攒批缓冲(预算耗尽时暂存的门控通过项)
//! ├─ decision-log.jsonl   门控决策日志(滚动 200 条)
//! ├─ endpoint.json        本地 HTTP 端点信息(端口 + 握手 token)
//! └─ projects/<sha256(path)[..16]>/
//!    ├─ events.log        归一化事件滚动日志(JSONL,最近 200 条)
//!    └─ facts.json        项目全局记忆(≤50KB)
//! ```

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};

/// 核心状态锁:std 互斥(临界区极短,无 await 依赖),保证异步/同步上下文通用
pub struct CoreLock(std::sync::Mutex<Core>);

impl CoreLock {
    pub fn blocking_lock(&self) -> std::sync::MutexGuard<'_, Core> {
        self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

impl From<Core> for CoreLock {
    fn from(core: Core) -> Self {
        CoreLock(std::sync::Mutex::new(core))
    }
}

use crate::events::NormalizedEvent;
use crate::memory::Facts;
use crate::queue::{Card, CooldownMap};
use crate::settings::Settings;

/// 项目路径归一化。
/// 不同适配器(甚至同一适配器的不同会话)会给出 `C:\repo\app`、`c:/repo/app/` 等
/// 多种写法;不归一就会让**同一个项目裂成两个记忆目录**,事件窗口与 facts 互相
/// 看不见 —— 表现为"记忆总是空的、facts 永远生成不出来"。
pub fn normalize_project_path(p: &str) -> String {
    let mut s = p.trim().replace('\\', "/");
    while s.contains("//") {
        s = s.replace("//", "/");
    }
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    #[cfg(windows)]
    {
        // Windows 路径大小写不敏感
        s = s.to_ascii_lowercase();
    }
    s
}

/// 项目键:由**归一化后**的项目路径计算(sha256 前缀),作为事件/记忆隔离的 key
pub fn project_key(project_path: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(normalize_project_path(project_path).as_bytes());
    let out = format!("{:x}", h.finalize());
    out[..16].to_string()
}

/// 对话窗口条数(仅用于「下一轮优化提示词」的上下文,零 LLM 成本)
pub const DIALOG_WINDOW: usize = 3;
/// 单条对话原始 JSON 的字符上限
pub const DIALOG_MAX_CHARS: usize = 3000;

/// 单项目活数据:滚动事件窗口 + 记忆 + 对话窗口
#[derive(Default)]
pub struct ProjectLive {
    /// 最近 ROLLING_LIMIT 条事件(内存镜像,与 events.log 同步)
    pub events: VecDeque<NormalizedEvent>,
    /// 项目全局记忆(None = 尚未生成)
    pub facts: Option<Facts>,
    /// 对话记忆:最近 DIALOG_WINDOW 条事件原始 JSON 截断副本(每条 ≤3KB)
    pub dialog: VecDeque<String>,
    /// 自上次增量精炼以来被消费的问题数(≥20 触发合并)
    pub consumed_since_merge: u32,
}

/// 本地 HTTP 端点信息(写入 endpoint.json 供适配器读取)
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct EndpointInfo {
    pub port: u16,
    pub hello_token: String,
    pub event_token: String,
}

/// 每日 LLM 预算快照
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DailyBudget {
    pub date: String,
    /// 今日请求已用次数
    pub used: u64,
    /// 今日被预算拒绝的次数(统计用)
    pub rejected: u64,
}

impl Default for DailyBudget {
    fn default() -> Self {
        Self {
            date: local_date(),
            used: 0,
            rejected: 0,
        }
    }
}

/// 今日日期(本地时区 yyyy-mm-dd)
pub fn local_date() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 时间戳(秒)
pub fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// 攒批项:门控通过(G1-G6)但 G7 预算耗尽时暂存的上下文快照,
/// 待额度恢复(次日或当日腾出)后由维护任务补发分析。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchItem {
    pub pass: crate::gate::GatePass,
    pub project_path: String,
    pub ref_event_id: String,
    pub created_at: i64,
}

/// 可串行访问的核心状态
pub struct Core {
    pub settings: Settings,
    pub projects: HashMap<String, ProjectLive>,
    /// 当前活动项目键(最近产生事件的项目)
    pub active_project: Option<String>,
    pub queue: VecDeque<Card>,
    pub history: Vec<Card>,
    pub cooldowns: CooldownMap,
    pub budget: DailyBudget,
    /// 门控决策日志(内存,同时滚动落盘)
    pub decision_log: VecDeque<crate::gate::DecisionEntry>,
    /// 攒批缓冲(预算耗尽时累计,恢复后消费)
    pub pending_batch: Vec<BatchItem>,
    /// 距上次弹卡时间 (ts)
    pub last_card_at: Option<i64>,
    /// 今日总计:派发卡片数 / 用户回答数 / 不满意次数(成功度量,落盘持久)
    pub stats: Stats,
    /// 预算耗尽今日提示过(托盘气泡 once/day)
    pub budget_toast_shown: bool,
    /// 上次摄入事件时间(活性检测)
    pub last_event_in: Option<i64>,
    /// 本次进程累计成功摄入的事件条数(不含被内容判重丢弃的):
    /// 只给 `last_event_in` 无法区分"链路通了但 Agent 闲置"与"一条都没进来"
    pub events_ingested: u64,
    /// 被暂停(设置) / 胶囊隐藏标记
    pub paused: bool,
    pub capsule_hidden: bool,
    /// 胶囊前端状态机(idle / active / popup),由前端上报、设置面板显示
    pub capsule_state: String,
    /// 正在生成 facts 首版的项目键(事件逐条进来,不去重就会每条都烧一次 LLM 预算)
    pub facts_inflight: HashSet<String>,
    /// facts 首版**连败**的项目键 → (失败次数, 冷却截止时间戳)。
    /// `facts_inflight` 只挡并发,挡不住"这次失败了 → 下一条事件又来一遍":
    /// 那是一条注定失败的路径在反复扣预算(实测单日 52 格里 47 格烧在这)。纯瞬态,不落盘。
    pub facts_fail: HashMap<String, (u32, i64)>,
    /// 内容级判重缓冲:(事件秒, emitter 无关指纹)。
    /// bridge 与 watcher 同开时同一动作会被报两遍,而 id 由各适配器自生成、两边不同,
    /// 靠 id 去不了重 —— 只能按内容指纹短期判重。纯瞬态,不落盘。
    pub recent_event_fp: VecDeque<(i64, String)>,
}

/// 运行统计(§1.5 成功度量的本地埋点):当日计数 + 历史累计,持久化到 stats.json
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    /// 当日计数所属日期(跨日 roll 时并入累计并清零)
    pub date: String,
    pub cards_created: u64,
    pub answers: u64,
    pub unsatisfied: u64,
    pub total_cards_created: u64,
    pub total_answers: u64,
    pub total_unsatisfied: u64,
    /// 正文真由 LLM 出的卡数。与 `cards_created` 分开记,是因为解析失败会静默
    /// 回填模板文案并照样入队 —— 只记后者的话,「今日发卡 N 张」这张绿灯
    /// 在 LLM 全挂的情况下依然会正常计数(实测 5 张里 4 张是模板)。
    #[serde(default)]
    pub cards_llm: u64,
    /// 退用模板兜底的卡数。持续走高 = 契约或传输坏了,而界面上一切"正常"
    #[serde(default)]
    pub cards_template: u64,
    #[serde(default)]
    pub total_cards_llm: u64,
    #[serde(default)]
    pub total_cards_template: u64,
}

impl Stats {
    pub fn today() -> Self {
        Self {
            date: local_date(),
            cards_created: 0,
            answers: 0,
            unsatisfied: 0,
            total_cards_created: 0,
            total_answers: 0,
            total_unsatisfied: 0,
            cards_llm: 0,
            cards_template: 0,
            total_cards_llm: 0,
            total_cards_template: 0,
        }
    }

    /// 若日期已变更:当日计数并入累计,当日清零
    pub fn roll_to_today(&mut self) {
        let today = local_date();
        if self.date != today {
            self.total_cards_created += self.cards_created;
            self.total_answers += self.answers;
            self.total_unsatisfied += self.unsatisfied;
            self.total_cards_llm += self.cards_llm;
            self.total_cards_template += self.cards_template;
            self.cards_created = 0;
            self.answers = 0;
            self.unsatisfied = 0;
            self.cards_llm = 0;
            self.cards_template = 0;
            self.date = today;
        }
    }
}

impl Core {
    pub fn empty(settings: Settings) -> Self {
        Self {
            settings,
            projects: HashMap::new(),
            active_project: None,
            queue: VecDeque::new(),
            history: Vec::new(),
            cooldowns: CooldownMap::default(),
            budget: DailyBudget::default(),
            decision_log: VecDeque::new(),
            pending_batch: Vec::new(),
            last_card_at: None,
            stats: Stats::today(),
            budget_toast_shown: false,
            last_event_in: None,
            events_ingested: 0,
            paused: false,
            capsule_hidden: false,
            capsule_state: "idle".into(),
            facts_inflight: HashSet::new(),
            facts_fail: HashMap::new(),
            recent_event_fp: VecDeque::new(),
        }
    }

    pub fn project_live(&mut self, path: &str) -> &mut ProjectLive {
        self.projects.entry(project_key(path)).or_default()
    }
}

/// 全局应用状态(Tauri manage 的是一份 Arc,供命令与 HTTP 任务共享)
pub struct AppState {
    pub core: CoreLock,
    pub data_dir: PathBuf,
    pub app: tauri::AppHandle,
    /// 本次进程的真实端点:HTTP 服务绑定成功后写一次,此前为空
    pub endpoint: std::sync::OnceLock<EndpointInfo>,
}

impl AppState {
    pub fn init(app: &tauri::AppHandle) -> anyhow::Result<Arc<Self>> {
        let data_dir = app.path().app_data_dir()?;
        std::fs::create_dir_all(&data_dir)?;

        let state = Arc::new(Self {
            // 设置与各持久化文件统一由 load_persisted 载入,此处只建空壳
            core: CoreLock::from(Core::empty(Settings::default())),
            data_dir,
            app: app.clone(),
            endpoint: std::sync::OnceLock::new(),
        });

        load_persisted(&state)?;
        Ok(state)
    }

    /// 项目数据子目录
    pub fn project_dir(&self, project_path: &str) -> PathBuf {
        self.data_dir
            .join("projects")
            .join(project_key(project_path))
    }

    pub fn ensure_project_dir(&self, project_path: &str) -> PathBuf {
        let dir = self.project_dir(project_path);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }
}

/// 引擎向窗口广播事件的统一入口(保持事件名集中管理)
pub fn emit(app: &tauri::AppHandle, event: &str, payload: &impl Serialize) {
    let _ = app.emit(event, payload);
}

/// 管线快照(仅在 pipeline 内部使用;字段由同步段填充后逐段消费)
pub struct PipelineSnapshot {
    pub project_path: String,
    pub now: i64,
    pub batch: Vec<NormalizedEvent>,
    pub queue_len: usize,
    pub pending: Vec<crate::queue::Card>,
    pub last_card_at: Option<i64>,
    pub facts: Option<Facts>,
    pub cooldowns: crate::queue::CooldownMap,
    pub history: Vec<crate::queue::Card>,
    pub last_event_id: String,
}

/// 持久化载入:settings/queue/cooldown/budget/stats/facts(项目按需)
pub fn load_persisted(state: &Arc<AppState>) -> anyhow::Result<()> {
    let dir = &state.data_dir;
    let mut core = state.core.blocking_lock();

    core.settings = crate::settings::load_settings(dir)?;
    core.paused = core.settings.paused;

    // 队列与历史:旧版把两者混存于 queue.json,这里按状态分流 + 按 id 去重
    {
        let mut seen: HashSet<String> = HashSet::new();
        if let Ok(raw) = std::fs::read_to_string(dir.join("history.json")) {
            if let Ok(cards) = serde_json::from_str::<Vec<Card>>(&raw) {
                for c in cards {
                    if seen.insert(c.id.clone()) {
                        core.history.push(c);
                    }
                }
            }
        }
        if let Ok(raw) = std::fs::read_to_string(dir.join("queue.json")) {
            if let Ok(list) = serde_json::from_str::<Vec<Card>>(&raw) {
                for c in list {
                    if !seen.insert(c.id.clone()) {
                        continue;
                    }
                    if c.status == "pending" && now_ts() - c.created_at <= crate::queue::TTL_SECS {
                        core.queue.push_back(c);
                    } else if c.status != "pending" {
                        // 兼容旧版混存:非 pending 归档进历史
                        core.history.push(c);
                    }
                }
            }
        }
    }

    // 冷却矩阵
    if let Ok(raw) = std::fs::read_to_string(dir.join("cooldown.json")) {
        if let Ok(map) = serde_json::from_str::<CooldownMap>(&raw) {
            core.cooldowns = map;
        }
    }

    // 预算(跨日期自动归零)
    if let Ok(raw) = std::fs::read_to_string(dir.join("budget.json")) {
        if let Ok(b) = serde_json::from_str::<DailyBudget>(&raw) {
            if b.date == local_date() {
                core.budget = b;
            }
        }
    }

    // 运行统计(载入后先做跨日归并:昨日计数并入累计)
    if let Ok(raw) = std::fs::read_to_string(dir.join("stats.json")) {
        if let Ok(s) = serde_json::from_str::<Stats>(&raw) {
            core.stats = s;
            core.stats.roll_to_today();
        }
    }

    // 攒批缓冲(丢弃超过保鲜期的陈旧项)
    if let Ok(raw) = std::fs::read_to_string(dir.join("batch.json")) {
        if let Ok(list) = serde_json::from_str::<Vec<BatchItem>>(&raw) {
            let now = now_ts();
            core.pending_batch = list
                .into_iter()
                .filter(|b| now - b.created_at < crate::llm::budget::BATCH_TTL_SECS)
                .collect();
        }
    }

    // 决策日志(滚动 DECISION_LOG_KEEP,取最近若干行)
    if let Ok(raw) = std::fs::read_to_string(dir.join("decision-log.jsonl")) {
        let lines: Vec<&str> = raw.lines().collect();
        let start = lines.len().saturating_sub(crate::gate::DECISION_LOG_KEEP);
        for line in &lines[start..] {
            if let Ok(e) = serde_json::from_str::<crate::gate::DecisionEntry>(line) {
                core.decision_log.push_back(e);
            }
        }
    }

    // 项目事件恢复:重启后还原最近事件/对话窗口/facts/活动项目
    // (保证"来一道"与记忆在重启后仍然可用)
    let projects_dir = dir.join("projects");
    if let Ok(entries) = std::fs::read_dir(&projects_dir) {
        let mut newest: Option<(i64, String)> = None;
        for entry in entries.flatten() {
            let log = entry.path().join("events.log");
            let Ok(raw) = std::fs::read_to_string(&log) else {
                continue;
            };
            let mut events: Vec<NormalizedEvent> = Vec::new();
            for line in raw.lines().rev().take(crate::events::ROLLING_LIMIT) {
                if let Ok(ev) = serde_json::from_str::<NormalizedEvent>(line) {
                    events.push(ev);
                }
            }
            events.reverse();
            if events.is_empty() {
                continue;
            }
            let path = events.last().unwrap().project_path.clone();
            let key = project_key(&path);
            let mut live = ProjectLive::default();
            for ev in events.iter().rev().take(DIALOG_WINDOW) {
                let r = serde_json::to_string(ev).unwrap_or_default();
                live.dialog
                    .push_back(r.chars().take(DIALOG_MAX_CHARS).collect());
            }
            live.events = events.into_iter().collect();
            if let Some(facts) = crate::memory::load_facts(&entry.path()) {
                live.facts = Some(facts);
            }
            let ts = live.events.back().map(|e| e.ts).unwrap_or(0);
            core.projects.insert(key, live);
            if newest.as_ref().map(|(t, _)| ts > *t).unwrap_or(true) {
                newest = Some((ts, path));
            }
        }
        if let Some((_, path)) = newest {
            core.active_project = Some(project_key(&path));
        }
    }

    drop(core);
    Ok(())
}

/// 持久化各文件(在变更点调用;自行加锁)
pub fn save_core(state: &Arc<AppState>) {
    let dir = state.data_dir.clone();
    let core = state.core.blocking_lock();
    save_core_locked(&dir, &core);
}

/// 无锁变体:调用方必须已持有 core 锁(否则死锁)
pub fn save_core_locked(dir: &std::path::Path, core: &Core) {
    let _ = crate::settings::save_settings(dir, &core.settings);
    let _ = crate::queue::persist_queue(core, dir);
    let _ = std::fs::write(
        dir.join("cooldown.json"),
        serde_json::to_string(&core.cooldowns).unwrap_or_default(),
    );
    let _ = std::fs::write(
        dir.join("budget.json"),
        serde_json::to_string(&core.budget).unwrap_or_default(),
    );
    let _ = std::fs::write(
        dir.join("stats.json"),
        serde_json::to_string(&core.stats).unwrap_or_default(),
    );
    let _ = std::fs::write(
        dir.join("batch.json"),
        serde_json::to_string(&core.pending_batch).unwrap_or_default(),
    );

    // 决策日志滚动写
    let entries: Vec<_> = core.decision_log.iter().collect();
    let mut text = String::new();
    for e in entries {
        if let Ok(l) = serde_json::to_string(e) {
            text.push_str(&l);
            text.push('\n');
        }
    }
    let _ = std::fs::write(dir.join("decision-log.jsonl"), text);
}

/// 历史卡片列表(设置面板「历史」Tab)
#[tauri::command]
pub fn history_list(state: tauri::State<'_, Arc<AppState>>) -> Vec<Card> {
    state.core.blocking_lock().history.clone()
}

/// 本地端点信息(设置面板诊断)。
/// 只信内存里本次进程的真实值:磁盘上的 endpoint.json 可能属于上一次进程,
/// 拿它兜底会显示一个早已失效的端口,反而把"适配器连不通"误导成"端点正常"。
#[tauri::command]
pub fn endpoint_info(state: tauri::State<'_, Arc<AppState>>) -> EndpointInfo {
    state.endpoint.get().cloned().unwrap_or_default()
}

/// 活动项目的记忆(设置面板展示)
#[tauri::command]
pub fn facts_get(state: tauri::State<'_, Arc<AppState>>) -> Option<Facts> {
    let core = state.core.blocking_lock();
    core.active_project
        .as_ref()
        .and_then(|key| core.projects.get(key))
        .and_then(|live| live.facts.clone())
}

/// 累计口径的成功度量(设置面板「累计」行)
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsTotals {
    pub cards_created: u64,
    pub answers: u64,
    pub unsatisfied: u64,
    pub answer_rate: f64,
    /// 累计里真由 LLM 出的稿数(与 cardsCreated 不等即说明有卡片走了模板兜底)
    pub cards_llm: u64,
    pub cards_template: u64,
}

/// `stats_info` 的响应契约(与前端 lib/types.ts 的 StatsInfo 一一对应)
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsInfo {
    pub last_event_in: Option<i64>,
    /// 本次进程摄入的事件条数(诊断"适配器到底通没通")
    pub events_ingested: u64,
    pub active_project: String,
    /// 胶囊当前态(idle/active/popup):排查"卡片进了队列但看不见"时的第一手线索
    pub capsule_state: String,
    pub cards_created: u64,
    pub answers: u64,
    pub unsatisfied: u64,
    /// 当日 LLM 出稿 / 模板兜底 的分账。卡片数相同,含义完全不同:
    /// 前者说明闭环在工作,后者说明契约或传输坏了而界面上一切"正常"
    pub cards_llm: u64,
    pub cards_template: u64,
    pub today_answer_rate: f64,
    pub total: StatsTotals,
    pub batched: usize,
}

/// 回答率(0–100,分母为 0 时记 0)
fn answer_rate(answers: u64, cards: u64) -> f64 {
    if cards == 0 {
        0.0
    } else {
        answers as f64 * 100.0 / cards as f64
    }
}

/// 运行统计(设置面板诊断:适配器是否在打通 + §1.5 成功度量本地埋点)
#[tauri::command]
pub fn stats_info(state: tauri::State<'_, Arc<AppState>>) -> StatsInfo {
    let mut core = state.core.blocking_lock();
    // 读取时顺带跨日归并(避免隔夜仍显示昨日"当日"计数)
    let rolled = core.stats.date != local_date();
    core.stats.roll_to_today();
    if rolled {
        let _ = std::fs::write(
            state.data_dir.join("stats.json"),
            serde_json::to_string(&core.stats).unwrap_or_default(),
        );
    }
    // 活动项目路径:取该项目窗口内最近事件的原始路径(内存中不单独存,故回查事件)
    let active_project = core
        .active_project
        .as_ref()
        .and_then(|key| core.projects.get(key))
        .and_then(|live| live.events.back())
        .map(|e| e.project_path.clone())
        .unwrap_or_default();
    let s = &core.stats;
    StatsInfo {
        last_event_in: core.last_event_in,
        events_ingested: core.events_ingested,
        active_project,
        capsule_state: core.capsule_state.clone(),
        cards_created: s.cards_created,
        answers: s.answers,
        unsatisfied: s.unsatisfied,
        cards_llm: s.cards_llm,
        cards_template: s.cards_template,
        today_answer_rate: answer_rate(s.answers, s.cards_created),
        total: StatsTotals {
            cards_created: s.total_cards_created,
            answers: s.total_answers,
            unsatisfied: s.total_unsatisfied,
            answer_rate: answer_rate(s.total_answers, s.total_cards_created),
            cards_llm: s.total_cards_llm,
            cards_template: s.total_cards_template,
        },
        batched: core.pending_batch.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_key_unifies_path_spellings() {
        // 同一目录的不同写法必须落到同一个键,否则记忆会裂成两个项目
        assert_eq!(
            project_key(r"C:\repo\app"),
            project_key("C:/repo/app"),
            "分隔符差异必须归一"
        );
        assert_eq!(
            project_key(r"C:\repo\app"),
            project_key(r"C:\repo\app\"),
            "尾部斜杠必须归一"
        );
        assert_eq!(
            project_key(r"C:\repo\app"),
            project_key("C:\\\\repo//app"),
            "重复分隔符必须归一"
        );
        #[cfg(windows)]
        assert_eq!(
            project_key(r"C:\repo\app"),
            project_key(r"c:/REPO/app"),
            "Windows 路径大小写不敏感"
        );
    }

    #[test]
    fn project_key_keeps_different_projects_apart() {
        assert_ne!(project_key(r"C:\repo\app"), project_key(r"C:\repo\other"));
        assert_eq!(normalize_project_path(""), "");
    }

    #[test]
    fn stats_roll_merges_day_change_only_once() {
        let mut s = Stats::today();
        s.cards_created = 5;
        s.answers = 2;
        s.unsatisfied = 1;
        // 伪造为旧日期 → roll 后当日清零、累计并入
        s.date = "2000-01-01".into();
        s.roll_to_today();
        assert_eq!(s.cards_created, 0);
        assert_eq!(s.total_cards_created, 5);
        assert_eq!(s.total_answers, 2);
        assert_eq!(s.total_unsatisfied, 1);
        assert_eq!(s.date, local_date());
        // 同日重复 roll 不重复计数
        s.roll_to_today();
        assert_eq!(s.total_cards_created, 5);
    }

    #[test]
    fn project_key_stable_and_isolating() {
        assert_eq!(project_key("C:/proj/foo"), project_key("C:/proj/foo"));
        assert_ne!(project_key("C:/proj/foo"), project_key("C:/proj/bar"));
        assert_eq!(project_key("x").len(), 16);
    }
}
