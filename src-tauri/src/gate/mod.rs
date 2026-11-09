//! 门控决策树:全部 0 token、确定性、纯函数。
//!
//! `evaluate(ctx) -> GateVerdict`:
//! G1 队列硬闸 → G2 频控 → G3 活性 → G4 分支路由 → G5 信号打分/知识分支
//! → G6 去重 → G7 由调用方执行(调用 LLM,≤5k token)。
//!
//! 纯函数约束:不访问持久层、不读时钟内部资源、产生确定性结果
//! (知识点池选择基于 blake3 哈希,可复现)。

use serde::{Deserialize, Serialize};

use crate::events::NormalizedEvent;
use crate::memory::Facts;
use crate::queue::Card;

/// G5-A 审查阈值。60 时一次 8 行的普通重构恒为 0 分,整批信号被白白丢掉;
/// 实测(2026-09-03)读 4 个 .md 就能把分数压到 -100,阈值越高越埋没真实写信号。
pub const SCORE_THRESHOLD: i32 = 40;
/// G2 频控:距上次弹卡的最小间隔
pub const LAST_CARD_WINDOW_SECS: i64 = 25;
/// G2 同主题冷却(轮换后仍有兜底作用)
pub const THEME_COOLDOWN_SECS: i64 = 10 * 60;
/// 用户明确"不再问这类"/连续 2 次不满意后的静默时长。**不放宽**——这是唯一的用户否决权
pub const THEME_MUTE_SECS: i64 = 60 * 60;
/// 出稿正文去重窗口。1h 时同一项目一天最多约 4 张同类卡,实测大量"刚递过同款内容"
pub const DEDUP_WINDOW_SECS: i64 = 20 * 60;
/// G3 活性窗口:近 90s 内无事件即视为"Agent 空闲",不触发评估
pub const ACTIVITY_WINDOW_SECS: i64 = 90;
/// 决策日志内存窗口条数
pub const DECISION_LOG_KEEP: usize = 200;
/// 决策日志落盘超过该行号才整写修剪(append 为主,修剪为辅)
pub const DECISION_LOG_TRIM_AT: usize = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Branch {
    Review,
    Knowledge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateLevel {
    G1,
    G2,
    G3,
    G4,
    G5A,
    G5B,
    G6,
}

impl GateLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            GateLevel::G1 => "G1",
            GateLevel::G2 => "G2",
            GateLevel::G3 => "G3",
            GateLevel::G4 => "G4",
            GateLevel::G5A => "G5-A",
            GateLevel::G5B => "G5-B",
            GateLevel::G6 => "G6",
        }
    }
}

#[derive(Clone, Debug)]
pub enum GateVerdict {
    Pass(GatePass),
    Reject(GateLevel, String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GatePass {
    pub branch: Branch,
    pub score: i32,
    pub p0: bool,
    /// G5-B 命中的主题(如 vue-lifecycle / framework-unknown)
    pub theme_hint: String,
    pub reason: String,
}

/// 打分明细(决策日志面板展示)
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Scoring {
    pub score: i32,
    pub points: Vec<String>,
    pub penalized: Vec<String>,
    pub p0: bool,
    pub n_events: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionEntry {
    pub ts: i64,
    pub level: String,
    pub pass: bool,
    pub reason: String,
    pub branch: Option<String>,
    pub score: Option<i32>,
    pub elapsed_ms: u64,
    pub project: String,
}

/// 输入:事件批(近 90s)+ 队列状态 + 项目事实 + 冷却矩阵 + 待答卡
pub struct GateContext<'a> {
    pub events: Vec<NormalizedEvent>,
    pub now: i64,
    pub queue_len: usize,
    pub last_card_at: Option<i64>,
    pub cooldowns: &'a crate::queue::CooldownMap,
    pub facts: Option<&'a Facts>,
    pub pending_cards: &'a [Card],
    pub history: &'a [Card],
    /// "来一道":旁路频控(G2),不破队列上限
    pub force: bool,
    /// 会话开场破冰(每个 session 首次评估)。旁路 G2 频控/同主题冷却与 G6 近似指纹,
    /// 但**不旁路** G1 队列上限、G3 活性、用户显式"不再问这类"的静默期,
    /// 也不旁路出稿后的正文护栏。目的:让"对话一开始"就有话可递。
    pub opener: bool,
}

/// 事件是否驱动"写分支"
pub fn is_write_event(e: &NormalizedEvent) -> bool {
    matches!(e.tool_name.as_str(), "edit" | "write" | "patch")
        && matches!(e.event_type.as_str(), "tool.completed" | "tool.started")
}

/// 事件是否驱动"知识分支"(一轮结束 = 用户正在等,是最低成本的递话时机)
pub fn has_knowledge_signal(events: &[NormalizedEvent]) -> bool {
    events
        .iter()
        .any(|e| e.event_type == "turn.ended" || e.tool_name == "think")
}

/// 生成指纹(去重用):分支 + 主题 + 首个引用文件(确定性)
///
/// 这是 **LLM 之前**的省钱近似:只有主题与事件文件可比对,因此它只能回答
/// "这一类内容最近是否已花过钱",不能回答"这句话是否说过两遍"。
/// 后者由 `fingerprint_text` 在出稿后负责 —— 两者不可混用同一口径。
pub fn fingerprint(branch: Branch, theme: &str, ref_file: &str) -> String {
    let stub = format!("{branch:?}|{theme}|{ref_file}");
    blake3::hash(stub.as_bytes()).to_hex()[..16].to_string()
}

/// 正文级指纹:出稿后的真去重("同一句话别递两遍")
pub fn fingerprint_text(branch: Branch, theme: &str, text: &str) -> String {
    let head: String = text.chars().take(120).collect::<String>().to_lowercase();
    let stub = format!("{branch:?}|{theme}|{head}");
    blake3::hash(stub.as_bytes()).to_hex()[..16].to_string()
}

pub fn branch_of_kind(kind: &str) -> Branch {
    if kind == "review" {
        Branch::Review
    } else {
        Branch::Knowledge
    }
}

pub fn fingerprint_for_card(c: &Card) -> String {
    let ref_file = c.ref_files.first().cloned().unwrap_or_default();
    fingerprint(branch_of_kind(&c.kind), &c.theme, &ref_file)
}

/// 卡片正文指纹(供入队护栏使用)
pub fn fingerprint_text_for_card(c: &Card) -> String {
    fingerprint_text(branch_of_kind(&c.kind), &c.theme, &c.text)
}

fn text_lower(s: &str) -> String {
    s.chars().take(600).collect::<String>().to_lowercase()
}

/// G5-A 评分:从**写事件**的 excerpt 本地字段提取信号(阈值见 `SCORE_THRESHOLD`)。
///
/// 两条口径是 2026-09-03 实测后钉死的:
/// 1. **只统计写事件。** 旧实现对批内每条事件都打分,于是"读了 4 个 .md"
///    这种纯浏览会话会累出 -100,把同窗口里一次真实大改的信号埋掉。
/// 2. **罚分按批只扣一次。** 逐条累加等于按事件数放大罚分,同样埋没真实信号。
pub fn signal_score(events: &[NormalizedEvent]) -> Scoring {
    let mut s = Scoring::default();
    let mut effective_api = false;
    let mut format_hit = false;
    let mut docs_hit = false;
    let writes: Vec<&NormalizedEvent> = events.iter().filter(|e| is_write_event(e)).collect();

    for e in &writes {
        let ex = &e.excerpt;
        let lower = text_lower(&format!("{} {}", ex.diff_head, ex.intent));

        // 变更规模分档:≥20 行是大改,8–19 行是"值得看一眼"的重构。
        // 只有一档时,一次 8 行的重构恒为 0 分,连阈值都摸不到。
        if ex.line_delta >= 20 {
            s.score += 30;
            s.points.push(format!("{}:lineDelta≥20(+30)", e.tool_name));
        } else if ex.line_delta >= 8 {
            s.score += 15;
            s.points.push(format!("{}:lineDelta≥8(+15)", e.tool_name));
        }
        // 新增 API/网络调用
        let api_hint = ex.has_api_call
            || lower.contains("http")
            || lower.contains("axios")
            || lower.contains("fetch")
            || lower.contains("reqwest")
            || lower.contains("api key");
        if api_hint {
            effective_api = true;
            if !s.points.iter().any(|p| p.contains("网络调用")) {
                s.score += 25;
                s.points.push("新增 API/网络调用(+25)".into());
            }
        }
        // 异常路径未包裹:文本级检测误报率高(跨语言不可靠),降权至 +15,
        // 且仅在大变更(≥10 行)时计入——语义级判断交由 G7 LLM 一次完成。
        let err_hint = ex.has_error_path || lower.contains("panic!");
        let has_guard = lower.contains("catch") || lower.contains("try {");
        if err_hint && !has_guard && ex.line_delta >= 10 {
            s.score += 15;
            s.points.push("新增异常路径未见保护(+15)".into());
        }
        // (评审 R4:移除"注释密度骤降"信号——纯文本无法可靠识别各语言的注释,交由 LLM)
        // TODO/FIXME
        if ex.todo_or_fixme || lower.contains("todo") || lower.contains("fixme") {
            s.score += 15;
            s.points.push("含 TODO/FIXME(+15)".into());
        }
        // 删除代码文件:关键词判断不含依赖分析,降权至 +15
        let deleted = lower.contains("删除") || lower.contains("remove ") || lower.contains("rm ");
        let code_file = ex.files.iter().any(|f| {
            [".ts", ".js", ".rs", ".py", ".vue", ".go"]
                .iter()
                .any(|ext| f.ends_with(ext))
        });
        if deleted && code_file {
            s.score += 15;
            s.points.push("删除代码文件(+15)".into());
        }
        // 裸 throw 错误串:降权至 +12(语言上下文中的合法用法不少)
        if lower.contains("throw") && !lower.contains("catch") {
            s.score += 12;
            s.points.push("裸 throw 错误串(+12)".into());
        }
        // 罚分只记"命中过",扣金额在批末统一结算(见函数注释口径 2)
        if ex.line_delta < 5
            && (lower.contains("format") || lower.contains("rename") || lower.contains("重构"))
        {
            format_hit = true;
        }
        let docs = !ex.files.is_empty()
            && ex
                .files
                .iter()
                .all(|f| f.ends_with(".md") || f.contains("README") || f.contains("docs/"));
        if docs {
            docs_hit = true;
        }
    }

    // 一次改动铺开到 3 个以上文件:影响面本身就是值得审的信号
    let touched: std::collections::HashSet<&str> = writes
        .iter()
        .flat_map(|e| e.excerpt.files.iter().map(|s| s.as_str()))
        .collect();
    if touched.len() >= 3 {
        s.score += 10;
        s.points.push(format!("影响 {} 个文件(+10)", touched.len()));
    }
    if format_hit {
        s.score -= 30;
        s.penalized.push("纯格式化/重命名(-30)".into());
    }
    if docs_hit {
        s.score -= 30;
        s.penalized.push("纯 README/文档变更(-30)".into());
    }

    s.n_events = events.len();
    s.score = s.score.clamp(-100, 200);

    // P0 判定:网络调用无异常处理 | 覆盖不完整(同样只看写事件)
    let intent_pool: String = writes
        .iter()
        .map(|e| e.excerpt.intent.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let network_no_guard = effective_api
        && !writes
            .iter()
            .any(|e| e.excerpt.diff_head.to_lowercase().contains("catch"));
    let coverage_short = intent_pool.contains("不完整")
        || intent_pool.contains("未覆盖")
        || intent_pool.contains("无覆盖");
    let p0 = network_no_guard || coverage_short;
    s.p0 = p0;
    s
}

/// 从项目事实推导知识主题(确定性映射)
pub fn theme_from_facts(facts: &Facts) -> String {
    let f = facts.framework.to_lowercase();
    if f.contains("nuxt") || f.contains("vue") {
        return "vue-lifecycle".into();
    }
    if f.contains("next") || f.contains("react") || f.contains("remix") {
        return "react-lifecycle".into();
    }
    if f.contains("svelte") {
        return "svelte-reactivity".into();
    }
    if f.contains("tauri") || f.contains("rust") {
        return "rust-ownership".into();
    }
    if f.contains("nest") || f.contains("express") || f.contains("fastify") || f.contains("laravel")
    {
        return "backend-async".into();
    }
    pool_pick(facts.framework.as_str())
}

/// 通用知识主题池(按种子确定性伪随机)
pub const TIP_POOL: &[&str] = &[
    "project-mental-model",
    "tooling-tips",
    "api-error-handling",
    "refactor-leverage",
];

pub fn pool_pick(seed: &str) -> String {
    let h = blake3::hash(seed.as_bytes());
    let idx = (h.as_bytes()[0] as usize) % TIP_POOL.len();
    TIP_POOL[idx].to_string()
}

/// 知识分支的候选主题:项目命中主题优先,其后是通用主题池。
///
/// 只给一个主题 = 每次出同一句话。实测:本项目 facts 恒推出 `vue-lifecycle`,
/// 于是同主题冷却与正文去重把知识分支彻底堵死,用户点"来一道"只看到
/// "刚递过同款内容"。轮换是"多输出"的前置条件,不是锦上添花。
pub fn knowledge_candidates(facts: Option<&Facts>) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    if let Some(f) = facts {
        if !f.framework.trim().is_empty() {
            v.push(theme_from_facts(f));
        }
    }
    v.extend(TIP_POOL.iter().map(|s| s.to_string()));
    v.dedup();
    v
}

/// 选主题:先在被用户否决(静默)的主题之外,取"最久未问"的那个;
/// 同样新近时按候选顺序(项目命中主题优先)。纯函数、0 token。
///
/// 全部候选都在静默期时,返回其中"最久未问"的一个 —— 让 G2 给出
/// **可执行的**拒绝原因("主题处于静默期"),而不是静悄悄换个主题硬递。
pub fn pick_knowledge_theme(
    facts: Option<&Facts>,
    cooldowns: &crate::queue::CooldownMap,
    now: i64,
) -> String {
    let cands = knowledge_candidates(facts);
    let best = |allow_muted: bool| {
        cands
            .iter()
            .enumerate()
            .filter(|(_, t)| allow_muted || !cooldowns.is_muted(t, now))
            .min_by_key(|(i, t)| (cooldowns.last_asked_at(t), *i))
            .map(|(_, t)| t.clone())
    };
    best(false)
        .or_else(|| best(true))
        .unwrap_or_else(|| "project-mental-model".into())
}

/// 本批要递的知识主题。
///
/// 正常路径下 facts 缺失 → 出知情度卡(`framework-unknown`)。
/// **破冰卡不允许出知情度卡**:铁律 4 规定知情度卡必须最后生成,
/// 不能靠"会话开场旁路"抢在真正该递的话前面,所以它走主题池轮换。
fn knowledge_theme(ctx: &GateContext) -> String {
    let known = matches!(ctx.facts, Some(f) if !f.framework.trim().is_empty());
    if !known && !ctx.opener {
        return "framework-unknown".into();
    }
    pick_knowledge_theme(ctx.facts, ctx.cooldowns, ctx.now)
}

/// 预判知识主题(G2 冷却判定用):仅在会路由到知识分支时才需要
fn prefer_theme(ctx: &GateContext, review_wins: bool) -> Option<String> {
    if review_wins {
        return None;
    }
    if !has_knowledge_signal(&ctx.events) && !ctx.force && !ctx.opener {
        return None;
    }
    Some(knowledge_theme(ctx))
}

fn dedup_hit(ctx: &GateContext, branch: Branch, theme: &str, ref_file: &str) -> bool {
    let fp = fingerprint(branch, theme, ref_file);
    // 队列重复
    for c in ctx.pending_cards {
        if c.status == "pending" && fingerprint_for_card(c) == fp {
            return true;
        }
    }
    // 正文去重窗口内的历史重复
    for c in ctx.history {
        if ctx.now - c.created_at < DEDUP_WINDOW_SECS && fingerprint_for_card(c) == fp {
            return true;
        }
    }
    false
}

/// 门控主评估(纯函数)
pub fn evaluate(ctx: &GateContext) -> (GateVerdict, Scoring) {
    let scoring = signal_score(&ctx.events);

    // G1 队列硬闸(即使"来一道"强制也不破队列上限)
    if ctx.queue_len >= crate::queue::MAX_PENDING {
        return (
            GateVerdict::Reject(
                GateLevel::G1,
                format!(
                    "队列已满(pending≥{}),停止一切触发",
                    crate::queue::MAX_PENDING
                ),
            ),
            scoring,
        );
    }
    // 路由预判:写信号达标才走审查,否则(有 turn.ended/think、force、破冰)走知识
    let has_write = ctx.events.iter().any(is_write_event);
    let knowledge_signal = has_knowledge_signal(&ctx.events);
    let review_wins = has_write && scoring.score >= SCORE_THRESHOLD;

    // G2 频控。"来一道"与"会话破冰"旁路间隔与同主题冷却;
    // **静默期(用户点过"不再问这类"或连续 2 次不满意)任何路径都不旁路。**
    if !ctx.force && !ctx.opener {
        if let Some(last) = ctx.last_card_at {
            if ctx.now - last < LAST_CARD_WINDOW_SECS {
                return (
                    GateVerdict::Reject(
                        GateLevel::G2,
                        format!(
                            "距上次弹卡仅 {}s(<{}s 频控)",
                            ctx.now - last,
                            LAST_CARD_WINDOW_SECS
                        ),
                    ),
                    scoring,
                );
            }
        }
    }
    if let Some(theme) = prefer_theme(ctx, review_wins) {
        if ctx.cooldowns.is_muted(&theme, ctx.now) {
            return (
                GateVerdict::Reject(
                    GateLevel::G2,
                    format!("主题 [{theme}] 处于静默期(用户否决)"),
                ),
                scoring,
            );
        }
        if !ctx.force
            && !ctx.opener
            && theme != "framework-unknown"
            && ctx.cooldowns.recently_asked(&theme, ctx.now)
        {
            return (
                GateVerdict::Reject(
                    GateLevel::G2,
                    format!("同主题 [{theme}] {}min 内已问过", THEME_COOLDOWN_SECS / 60),
                ),
                scoring,
            );
        }
    }
    // G3 活性
    if ctx.events.is_empty() && !ctx.force {
        return (
            GateVerdict::Reject(GateLevel::G3, "近 90s 无有效 agent 事件,防空转烧钱".into()),
            scoring,
        );
    }

    // G4 分支路由。旧实现是 `if has_write { Review }`(达标与否都要先落进审查),
    // 于是窗口里只要混进一条写事件,同批的 turn.ended 就被彻底浪费 —— 实测
    // 8 次 G5-A 拒绝的同时 events.log 里躺着 3 条 turn.ended。现在不达标就回落知识。
    let branch: Branch = if review_wins {
        Branch::Review
    } else if knowledge_signal || ctx.force || ctx.opener {
        Branch::Knowledge
    } else if has_write {
        return (
            GateVerdict::Reject(
                GateLevel::G5A,
                format!(
                    "信号分 {} < 阈值 {SCORE_THRESHOLD},且无 turn.ended 可回落",
                    scoring.score
                ),
            ),
            scoring,
        );
    } else {
        return (
            GateVerdict::Reject(
                GateLevel::G4,
                "无可路由信号(无写事件/turn.ended/think)".into(),
            ),
            scoring,
        );
    };

    // 首个引用文件:参与 G6 指纹(两分支同规则),故提前取出
    let ref_file = ctx
        .events
        .iter()
        .flat_map(|e| e.excerpt.files.iter())
        .next()
        .cloned()
        .unwrap_or_default();

    match branch {
        Branch::Review => {
            // force/opener 时豁免:此处只是主题级近似,真实去重交给出稿后的正文护栏
            if !ctx.force && !ctx.opener && dedup_hit(ctx, Branch::Review, "code-review", &ref_file)
            {
                return (
                    GateVerdict::Reject(GateLevel::G6, "审查指纹与队列/历史重复".into()),
                    scoring,
                );
            }
            let pass = GatePass {
                branch,
                score: scoring.score,
                p0: scoring.p0,
                theme_hint: "code-review".into(),
                reason: if scoring.p0 {
                    "P0:网络调用无异常处理或覆盖不完整".into()
                } else {
                    "审查分支,信号分达标".into()
                },
            };
            (GateVerdict::Pass(pass), scoring)
        }
        Branch::Knowledge => {
            let theme = knowledge_theme(ctx);
            let is_awareness = theme == "framework-unknown";
            if !ctx.force && !ctx.opener && dedup_hit(ctx, Branch::Knowledge, &theme, &ref_file) {
                return (
                    GateVerdict::Reject(GateLevel::G6, "知识卡指纹与队列/历史重复".into()),
                    scoring,
                );
            }
            let pass = GatePass {
                branch,
                score: scoring.score,
                p0: false,
                theme_hint: theme,
                reason: if is_awareness {
                    "知识缺口:框架未知 → 知情度问题(最高优先)".into()
                } else if ctx.opener {
                    "会话开场破冰:知识贴士".into()
                } else {
                    "知识贴士:命中已有主题".into()
                },
            };
            (GateVerdict::Pass(pass), scoring)
        }
    }
}

// ---------- Tauri 命令 ----------

/// 手动触发评估("来一道":旁路频控,不破队列上限)
/// 前置快检 + 等待管线跑完,按真实结局返回(被拦下时返回 Err 并说明层级与原因)
#[tauri::command]
pub async fn gate_tick(
    state: tauri::State<'_, std::sync::Arc<crate::state::AppState>>,
    force: Option<bool>,
) -> Result<String, String> {
    let state = state.inner().clone();
    let force = force.unwrap_or(true);
    {
        let core = state.core.blocking_lock();
        if core.paused {
            return Err("提问已暂停:托盘或设置中可恢复".into());
        }
        let pending = core.queue.iter().filter(|c| c.status == "pending").count();
        if pending >= crate::queue::MAX_PENDING {
            return Err(format!(
                "队列已满({pending}/{}):先回答或跳过卡片,再来一道",
                crate::queue::MAX_PENDING
            ));
        }
        if core.active_project.is_none() {
            return Err("暂无项目活动:让 Agent 干一点活,或确认适配器已接入".into());
        }
    }
    match crate::http::run_pipeline_body(state, force).await {
        crate::http::PipelineOutcome::CardCreated => Ok("已生成一张新卡,请看胶囊".into()),
        crate::http::PipelineOutcome::Rejected(level, reason) => {
            Err(format!("被门控拦下[{level}]:{reason}"))
        }
        crate::http::PipelineOutcome::Paused => Err("提问已暂停:托盘或设置中可恢复".into()),
        crate::http::PipelineOutcome::NoProject => {
            Err("暂无项目活动:让 Agent 干一点活,或确认适配器已接入".into())
        }
    }
}

/// 决策日志列表(面板展示:Gx 层级、拒绝原因、耗时)
#[tauri::command]
pub fn decision_log_list(
    state: tauri::State<'_, std::sync::Arc<crate::state::AppState>>,
) -> Vec<DecisionEntry> {
    state
        .core
        .blocking_lock()
        .decision_log
        .iter()
        .rev()
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Excerpt, NormalizedEvent};
    use crate::memory::Facts;
    use crate::queue::{Card, CooldownMap};

    /// 事件夹具工厂(测试专用:参数即 Excerpt 字段,展开比 builder 更直观)
    #[allow(clippy::too_many_arguments)]
    fn ev(
        event_type: &str,
        tool: &str,
        line_delta: i64,
        files: Vec<&str>,
        diff: &str,
        intent: &str,
        api: bool,
        err: bool,
        todo: bool,
    ) -> NormalizedEvent {
        let id = format!("{}-{}-{}-{}", event_type, tool, line_delta, diff.len());
        NormalizedEvent {
            schema: crate::events::SCHEMA.into(),
            id: blake3::hash(id.as_bytes()).to_hex()[..16].into(),
            emitter: "test".into(),
            event_type: event_type.into(),
            tool_name: tool.into(),
            ts: 1000,
            project_path: "C:/proj/foo".into(),
            session_id: "s1".into(),
            excerpt: Excerpt {
                files: files.into_iter().map(|s| s.to_string()).collect(),
                diff_head: diff.into(),
                intent: intent.into(),
                has_error_path: err,
                has_api_call: api,
                todo_or_fixme: todo,
                line_delta,
            },
            execution: None,
        }
    }

    /// 门控上下文夹具(测试专用:一次性覆盖全部输入维度)
    #[allow(clippy::too_many_arguments)]
    fn ctx<'a>(
        events: Vec<NormalizedEvent>,
        queue_len: usize,
        last: Option<i64>,
        cooldowns: &'a CooldownMap,
        facts: Option<&'a Facts>,
        pending: &'a [Card],
        history: &'a [Card],
        now: i64,
    ) -> GateContext<'a> {
        GateContext {
            events,
            now,
            queue_len,
            last_card_at: last,
            cooldowns,
            facts,
            pending_cards: pending,
            history,
            force: false,
            opener: false,
        }
    }

    /// 高信号夹具:lineDelta≥20(+30) + 网络调用(+25) + 异常未见保护(+15) + 裸 throw(+12) = 82 分,P0
    fn big_write() -> Vec<NormalizedEvent> {
        vec![ev(
            "tool.completed",
            "edit",
            42,
            vec!["src/app.ts"],
            "+++ b/src/app.ts\n+export async function run() {\n+  const r = await fetch(url);\n+  throw new Error(\"boom\");\n+}",
            "新增执行入口",
            true,
            true,
            false,
        )]
    }

    fn card(theme: &str, kind: &str, ref_file: &str) -> Card {
        Card {
            id: uuid::Uuid::new_v4().to_string(),
            kind: kind.into(),
            severity: "info".into(),
            text: "?".into(),
            options: vec!["满意".into(), "不满意".into()],
            theme: theme.into(),
            ref_files: vec![ref_file.to_string()],
            ref_event_id: String::new(),
            created_at: 1000,
            status: "pending".into(),
            answer: String::new(),
            optimized_prompt: String::new(),
            project_path: "C:/proj/foo".into(),
            insight: None,
        }
    }

    #[test]
    fn g1_full_queue_rejects() {
        let cd = CooldownMap::default();
        let (v, s) = evaluate(&ctx(big_write(), 3, None, &cd, None, &[], &[], 2000));
        assert!(matches!(v, GateVerdict::Reject(GateLevel::G1, _)));
        assert!(!s.points.is_empty());
    }

    #[test]
    fn g2_rate_limit_rejects() {
        let cd = CooldownMap::default();
        // 距上次弹卡 10s < 60s
        let (v, _) = evaluate(&ctx(big_write(), 0, Some(1990), &cd, None, &[], &[], 2000));
        assert!(matches!(v, GateVerdict::Reject(GateLevel::G2, _)));
    }

    #[test]
    fn g2_theme_cooldown_rejects_when_every_candidate_is_cooled() {
        let mut cd = CooldownMap::default();
        let facts = Facts {
            framework: "Vue3".into(),
            ..Default::default()
        };
        // 主题轮换后,单个主题被冷却不再等于知识分支被堵死:必须全部候选都在冷却里才拒
        for t in knowledge_candidates(Some(&facts)) {
            cd.touch_asked(&t, 1400);
        }
        let events = vec![ev(
            "turn.ended",
            "think",
            0,
            vec![],
            "",
            "总结",
            false,
            false,
            false,
        )];
        // now=1500,距上次问询 100s < THEME_COOLDOWN_SECS=600s → G2 拒绝
        let (v, _) = evaluate(&ctx(events, 0, None, &cd, Some(&facts), &[], &[], 1500));
        assert!(matches!(v, GateVerdict::Reject(GateLevel::G2, _)));
    }

    #[test]
    fn knowledge_theme_rotates_to_least_recently_asked() {
        let facts = Facts {
            framework: "Vue3".into(),
            ..Default::default()
        };
        let mut cd = CooldownMap::default();
        // 都没问过 → 项目命中主题优先
        assert_eq!(
            pick_knowledge_theme(Some(&facts), &cd, 2000),
            "vue-lifecycle"
        );
        // 刚问过项目主题 → 轮换到通用池的第一个,而不是死守同一句话
        cd.touch_asked("vue-lifecycle", 1000);
        assert_eq!(
            pick_knowledge_theme(Some(&facts), &cd, 2000),
            "project-mental-model"
        );
        cd.touch_asked("project-mental-model", 1001);
        assert_eq!(
            pick_knowledge_theme(Some(&facts), &cd, 2000),
            "tooling-tips"
        );
        // 用户否决过的主题被绕开,但知识分支仍然出话
        cd.mute("tooling-tips", 2000);
        assert_eq!(
            pick_knowledge_theme(Some(&facts), &cd, 2000),
            "api-error-handling"
        );
    }

    /// 回归:罚分不得作用于读事件。旧实现里"读了 4 个 .md"会累出 -100,
    /// 把同窗口内一次真实大改(+82)压到阈值以下,审查卡就此消失。
    #[test]
    fn doc_reads_do_not_bury_a_real_write_signal() {
        let mut events = vec![
            ev(
                "tool.completed",
                "read",
                0,
                vec!["README.md"],
                "",
                "看文档",
                false,
                false,
                false
            );
            4
        ];
        events.extend(big_write());
        let s = signal_score(&events);
        assert!(
            s.score >= SCORE_THRESHOLD,
            "纯读文档不应给同批的写事件扣分:实际 {s:?}"
        );
    }

    /// 回归:8 行的普通重构在旧口径下恒为 0 分。
    #[test]
    fn small_refactor_with_todo_clears_the_threshold() {
        let events = vec![ev(
            "tool.completed",
            "edit",
            12,
            vec!["src/a.ts", "src/b.ts", "src/c.ts"],
            "+// TODO: 补错误分支",
            "抽取公共校验",
            false,
            false,
            true,
        )];
        let s = signal_score(&events);
        assert!(s.score >= SCORE_THRESHOLD, "实际 {s:?}");
    }

    /// 回归:写事件信号不足时,同批的 turn.ended 必须回落到知识分支,
    /// 而不是整批被 G5-A 打死(实测有 8 次 G5-A 拒绝,同期 events.log 里躺着 3 条 turn.ended)。
    #[test]
    fn weak_review_signal_falls_back_to_knowledge_on_turn_ended() {
        let cd = CooldownMap::default();
        let mut events = vec![ev(
            "tool.completed",
            "edit",
            2,
            vec!["src/app.ts"],
            "rename",
            "纯格式化 rename",
            false,
            false,
            false,
        )];
        events.push(ev(
            "turn.ended",
            "",
            0,
            vec![],
            "",
            "这轮改完了",
            false,
            false,
            false,
        ));
        let (v, s) = evaluate(&ctx(events, 0, None, &cd, None, &[], &[], 2000));
        assert!(s.score < SCORE_THRESHOLD, "前提:审查信号确实不达标");
        match v {
            GateVerdict::Pass(p) => {
                assert_eq!(p.branch, Branch::Knowledge);
                assert_eq!(p.theme_hint, "framework-unknown");
            }
            other => panic!("审查不达标应回落知识分支,实际 {other:?}"),
        }
    }

    /// 破冰:每个 session 的第一次评估即使只有读事件、且刚弹过卡,也应放行一张知识卡。
    #[test]
    fn opener_bypasses_rate_limit_with_read_only_batch() {
        let cd = CooldownMap::default();
        let facts = Facts {
            framework: "Vue3".into(),
            ..Default::default()
        };
        let events = vec![ev(
            "tool.completed",
            "read",
            0,
            vec!["src/main.ts"],
            "",
            "看入口",
            false,
            false,
            false,
        )];
        // 距上次弹卡 3s:非破冰会被 G2 挡,破冰放行
        let mut c = ctx(
            events.clone(),
            0,
            Some(1997),
            &cd,
            Some(&facts),
            &[],
            &[],
            2000,
        );
        let (v, _) = evaluate(&c);
        assert!(
            matches!(v, GateVerdict::Reject(GateLevel::G2, _)),
            "前提:常规路径被频控挡住"
        );
        c.opener = true;
        let (v, _) = evaluate(&c);
        match v {
            GateVerdict::Pass(p) => assert_eq!(p.branch, Branch::Knowledge),
            other => panic!("破冰应放行一张知识卡:{other:?}"),
        }
    }

    /// 铁律 4:破冰不许抢知情度卡(它是唯一能让 Agent 反问"项目用什么框架"的卡型)
    #[test]
    fn opener_never_produces_the_awareness_card() {
        let cd = CooldownMap::default();
        let events = vec![ev(
            "tool.completed",
            "read",
            0,
            vec!["src/main.ts"],
            "",
            "看入口",
            false,
            false,
            false,
        )];
        let mut c = ctx(events, 0, None, &cd, None, &[], &[], 2000);
        c.opener = true;
        let (v, _) = evaluate(&c);
        match v {
            GateVerdict::Pass(p) => assert_ne!(p.theme_hint, "framework-unknown"),
            other => panic!("破冰应出知识贴士:{other:?}"),
        }
    }

    /// 破冰不吃用户否决权:显式"不再问这类"的静默期仍然拦得住
    #[test]
    fn opener_still_respects_theme_mute() {
        let mut cd = CooldownMap::default();
        cd.mute("vue-lifecycle", 2000);
        let facts = Facts {
            framework: "Vue3".into(),
            ..Default::default()
        };
        let events = vec![ev(
            "tool.completed",
            "read",
            0,
            vec!["src/main.ts"],
            "",
            "看入口",
            false,
            false,
            false,
        )];
        let mut c = ctx(events, 0, None, &cd, Some(&facts), &[], &[], 2000);
        c.opener = true;
        let (v, _) = evaluate(&c);
        assert!(
            matches!(v, GateVerdict::Pass(ref p) if p.theme_hint != "vue-lifecycle"),
            "静默主题必须被绕开,但知识分支仍要出话:{v:?}"
        );
    }

    /// 破冰不破队列硬闸(G1 仍是唯一不可旁路的闸门)
    #[test]
    fn opener_still_blocked_by_full_queue() {
        let cd = CooldownMap::default();
        let events = vec![ev(
            "tool.completed",
            "read",
            0,
            vec!["src/main.ts"],
            "",
            "看入口",
            false,
            false,
            false,
        )];
        let mut c = ctx(
            events,
            crate::queue::MAX_PENDING,
            None,
            &cd,
            None,
            &[],
            &[],
            2000,
        );
        c.opener = true;
        let (v, _) = evaluate(&c);
        assert!(matches!(v, GateVerdict::Reject(GateLevel::G1, _)));
    }

    #[test]
    fn g3_no_activity_rejects() {
        let cd = CooldownMap::default();
        let (v, _) = evaluate(&ctx(vec![], 0, None, &cd, None, &[], &[], 2000));
        assert!(matches!(v, GateVerdict::Reject(GateLevel::G3, _)));
    }

    #[test]
    fn g4_no_route_rejects() {
        let cd = CooldownMap::default();
        let events = vec![ev(
            "message.updated",
            "read",
            3,
            vec![],
            "",
            "看文件",
            false,
            false,
            false,
        )];
        let (v, _) = evaluate(&ctx(events, 0, None, &cd, None, &[], &[], 2000));
        assert!(matches!(v, GateVerdict::Reject(GateLevel::G4, _)));
    }

    #[test]
    fn g4_write_routes_to_review() {
        let cd = CooldownMap::default();
        let (v, s) = evaluate(&ctx(big_write(), 0, None, &cd, None, &[], &[], 2000));
        match v {
            GateVerdict::Pass(p) => {
                assert_eq!(p.branch, Branch::Review);
                assert!(p.score >= SCORE_THRESHOLD);
            }
            _ => panic!("应通过审查分支"),
        }
        assert!(s.score >= 60);
    }

    #[test]
    fn g5_a_formatting_low_score_rejects() {
        let cd = CooldownMap::default();
        let events = vec![ev(
            "tool.completed",
            "edit",
            2,
            vec!["src/app.ts"],
            "rename",
            "纯格式化 rename",
            false,
            false,
            false,
        )];
        let (v, _) = evaluate(&ctx(events, 0, None, &cd, None, &[], &[], 2000));
        assert!(matches!(v, GateVerdict::Reject(GateLevel::G5A, _)));
    }

    #[test]
    fn g5_a_p0_api_without_catch() {
        let cd = CooldownMap::default();
        let events = vec![ev(
            "tool.completed",
            "edit",
            40,
            vec!["src/net.rs"],
            "+fn call_api() {\n+  let r = client.fetch(url);\n+}",
            "接入搜索 HTTP API 解析结果",
            true,
            true,
            false,
        )];
        let (v, s) = evaluate(&ctx(events, 0, None, &cd, None, &[], &[], 2000));
        match v {
            GateVerdict::Pass(p) => {
                assert!(p.p0, "应判定 P0(网络调用无 try/catch)");
                assert_eq!(p.branch, Branch::Review);
            }
            other => panic!("应通过:{other:?}"),
        }
        assert!(s.p0);
    }

    #[test]
    fn g5_b_framework_unknown_awareness() {
        let cd = CooldownMap::default();
        let events = vec![ev(
            "turn.ended",
            "think",
            0,
            vec![],
            "",
            "这轮改完了",
            false,
            false,
            false,
        )];
        let (v, _) = evaluate(&ctx(events, 0, None, &cd, None, &[], &[], 2000));
        match v {
            GateVerdict::Pass(p) => {
                assert_eq!(p.branch, Branch::Knowledge);
                assert_eq!(p.theme_hint, "framework-unknown");
            }
            other => panic!("应生成知情度问题:{other:?}"),
        }
    }

    #[test]
    fn g5_b_framework_tip() {
        let cd = CooldownMap::default();
        let facts = Facts {
            framework: "Nuxt3".into(),
            ..Default::default()
        };
        let events = vec![ev(
            "turn.ended",
            "think",
            0,
            vec![],
            "",
            "结束",
            false,
            false,
            false,
        )];
        let (v, _) = evaluate(&ctx(events, 0, None, &cd, Some(&facts), &[], &[], 2000));
        match v {
            GateVerdict::Pass(p) => {
                assert_eq!(p.theme_hint, "vue-lifecycle");
            }
            other => panic!("应出 vue 贴士:{other:?}"),
        }
    }

    #[test]
    fn g6_dedup_queue_rejects() {
        let cd = CooldownMap::default();
        let pending = vec![card("code-review", "review", "src/app.ts")];
        let (v, _) = evaluate(&ctx(big_write(), 0, None, &cd, None, &pending, &[], 2000));
        assert!(matches!(v, GateVerdict::Reject(GateLevel::G6, _)));
    }

    /// "来一道"是用户主动授权:G6 只是主题级近似(拿不到 LLM 正文),
    /// 不该用它把用户的手动请求打死 —— 真实去重由出稿后的正文护栏负责。
    #[test]
    fn force_bypasses_topic_level_g6() {
        let cd = CooldownMap::default();
        let pending = vec![card("code-review", "review", "src/app.ts")];
        let mut c = ctx(big_write(), 0, None, &cd, None, &pending, &[], 2000);
        c.force = true;
        let (v, _) = evaluate(&c);
        assert!(
            matches!(v, GateVerdict::Pass(_)),
            "force 下主题级近似不应拦截:按它会把「来一道」永久打死"
        );
    }

    #[test]
    fn g6_dedup_history_rejects() {
        let cd = CooldownMap::default();
        let history = vec![card("code-review", "review", "src/app.ts")];
        let (v, _) = evaluate(&ctx(big_write(), 0, None, &cd, None, &[], &history, 2100));
        assert!(matches!(v, GateVerdict::Reject(GateLevel::G6, _)));
    }

    #[test]
    fn review_readme_only_penalized() {
        let cd = CooldownMap::default();
        let events = vec![ev(
            "tool.completed",
            "write",
            120,
            vec!["README.md"],
            "+文档",
            "更新 README",
            false,
            false,
            false,
        )];
        let (v, _) = evaluate(&ctx(events, 0, None, &cd, None, &[], &[], 2000));
        assert!(matches!(v, GateVerdict::Reject(GateLevel::G5A, _)));
    }

    #[test]
    fn fingerprint_stable() {
        let a = fingerprint(Branch::Review, "code-review", "src/a.ts");
        let b = fingerprint(Branch::Review, "code-review", "src/a.ts");
        assert_eq!(a, b);
        assert_ne!(a, fingerprint(Branch::Knowledge, "code-review", "src/a.ts"));
    }

    #[test]
    fn g1_full_queue_rejects_even_force() {
        let cd = CooldownMap::default();
        let mut c = ctx(big_write(), 3, None, &cd, None, &[], &[], 2000);
        c.force = true;
        let (v, _) = evaluate(&c);
        assert!(matches!(v, GateVerdict::Reject(GateLevel::G1, _)));
    }

    #[test]
    fn force_with_no_events_routes_knowledge() {
        let cd = CooldownMap::default();
        let mut c = ctx(vec![], 0, None, &cd, None, &[], &[], 2000);
        c.force = true;
        let (v, _) = evaluate(&c);
        match v {
            GateVerdict::Pass(p) => assert_eq!(p.branch, Branch::Knowledge),
            other => panic!("来一道应强制走知识分支:{other:?}"),
        }
    }
}
