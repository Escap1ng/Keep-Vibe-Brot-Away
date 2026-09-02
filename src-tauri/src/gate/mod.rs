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

pub const SCORE_THRESHOLD: i32 = 60;
pub const LAST_CARD_WINDOW_SECS: i64 = 60;
pub const THEME_COOLDOWN_SECS: i64 = 30 * 60;
pub const THEME_MUTE_SECS: i64 = 60 * 60;
pub const DEDUP_WINDOW_SECS: i64 = 60 * 60; // 历史去重窗口(1h)

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
}

/// 事件是否驱动"写分支"
pub fn is_write_event(e: &NormalizedEvent) -> bool {
    matches!(e.tool_name.as_str(), "edit" | "write" | "patch")
        && matches!(e.event_type.as_str(), "tool.completed" | "tool.started")
}

/// 生成指纹(去重用):分支 + 主题 + 首个引用文件(确定性)
pub fn fingerprint(branch: Branch, theme: &str, ref_file: &str) -> String {
    let stub = format!("{branch:?}|{theme}|{ref_file}");
    blake3::hash(stub.as_bytes()).to_hex()[..16].to_string()
}

pub fn fingerprint_for_card(c: &Card) -> String {
    let ref_file = c.ref_files.first().cloned().unwrap_or_default();
    let branch = if c.kind == "review" {
        Branch::Review
    } else {
        Branch::Knowledge
    };
    fingerprint(branch, &c.theme, &ref_file)
}

fn text_lower(s: &str) -> String {
    s.chars().take(600).collect::<String>().to_lowercase()
}

/// G5-A 评分:从事件 excerpt 本地字段提取信号(阈值 60)
pub fn signal_score(events: &[NormalizedEvent]) -> Scoring {
    let mut s = Scoring::default();
    let mut effective_api = false;

    for e in events {
        let ex = &e.excerpt;
        let lower = text_lower(&format!("{} {}", ex.diff_head, ex.intent));

        // lineDelta ≥ 20
        if ex.line_delta >= 20 {
            s.score += 30;
            s.points.push(format!("{}:lineDelta≥20(+30)", e.tool_name));
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
        // 减分:纯格式化/重命名(<5 行)
        let format_only = ex.line_delta < 5
            && (lower.contains("format") || lower.contains("rename") || lower.contains("重构"));
        if format_only {
            s.score -= 30;
            s.penalized.push("纯格式化/重命名(-30)".into());
        }
        // 减分:纯文档变更
        let docs = !ex.files.is_empty()
            && ex.files.iter().all(|f| f.ends_with(".md") || f.contains("README") || f.contains("docs/"));
        if docs {
            s.score -= 30;
            s.penalized.push("纯 README/文档变更(-30)".into());
        }
    }

    s.n_events = events.len();
    s.score = s.score.clamp(-100, 200);

    // P0 判定:网络调用无异常处理 | 覆盖不完整
    let intent_pool: String = events
        .iter()
        .map(|e| e.excerpt.intent.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let network_no_guard = effective_api
        && !events
            .iter()
            .any(|e| e.excerpt.diff_head.to_lowercase().contains("catch"));
    let coverage_short = intent_pool.contains("不完整") || intent_pool.contains("未覆盖") || intent_pool.contains("无覆盖");
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
    if f.contains("nest") || f.contains("express") || f.contains("fastify") || f.contains("laravel") {
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

/// 预判知识主题(G2 冷却判定用)
fn prefer_theme(ctx: &GateContext) -> Option<String> {
    let has_write = ctx.events.iter().any(is_write_event);
    let has_turn_end = ctx.events.iter().any(|e| e.event_type == "turn.ended");
    let has_think = ctx.events.iter().any(|e| e.tool_name == "think");
    if has_write {
        return None;
    }
    if !has_turn_end && !has_think {
        return None;
    }
    match ctx.facts {
        None => Some("framework-unknown".into()),
        Some(f) if f.framework.trim().is_empty() => Some("framework-unknown".into()),
        Some(f) => Some(theme_from_facts(f)),
    }
}

fn dedup_hit(ctx: &GateContext, branch: Branch, theme: &str, ref_file: &str) -> bool {
    let fp = fingerprint(branch, theme, ref_file);
    // 队列重复
    for c in ctx.pending_cards {
        if c.status == "pending" && fingerprint_for_card(c) == fp {
            return true;
        }
    }
    // 1h 内历史重复
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
    if ctx.queue_len >= 3 {
        return (
            GateVerdict::Reject(GateLevel::G1, "队列已满(pending≥3),停止一切触发".into()),
            scoring,
        );
    }
    // G2 频控
    if !ctx.force {
        if let Some(last) = ctx.last_card_at {
            if ctx.now - last < LAST_CARD_WINDOW_SECS {
                return (
                    GateVerdict::Reject(
                        GateLevel::G2,
                        format!("距上次弹卡仅 {}s(<60s 频控)", ctx.now - last),
                    ),
                    scoring,
                );
            }
        }
        if let Some(theme) = prefer_theme(ctx) {
            if ctx.cooldowns.is_muted(&theme, ctx.now) {
                return (
                    GateVerdict::Reject(GateLevel::G2, format!("主题 [{theme}] 处于冷却中")),
                    scoring,
                );
            }
            if !theme.eq("framework-unknown") && ctx.cooldowns.recently_asked(&theme, ctx.now) {
                return (
                    GateVerdict::Reject(GateLevel::G2, format!("同主题 [{theme}] 30min 内已问过")),
                    scoring,
                );
            }
        }
    }
    // G3 活性
    if ctx.events.is_empty() && !ctx.force {
        return (
            GateVerdict::Reject(GateLevel::G3, "近 90s 无有效 agent 事件,防空转烧钱".into()),
            scoring,
        );
    }

    // G4 分支路由
    let has_write = ctx.events.iter().any(is_write_event);
    let has_turn_end = ctx.events.iter().any(|e| e.event_type == "turn.ended");
    let has_think = ctx.events.iter().any(|e| e.tool_name == "think");
    let branch: Branch = if has_write {
        Branch::Review
    } else if has_turn_end {
        Branch::Knowledge
    } else if has_think {
        Branch::Knowledge
    } else if ctx.force {
        // "来一道":用户主动要求,即使无事件也走知识分支
        Branch::Knowledge
    } else {
        return (
            GateVerdict::Reject(GateLevel::G4, "无可路由信号(无写事件/turn.ended/think)".into()),
            scoring,
        );
    };

    match branch {
        Branch::Review => {
            if scoring.score < SCORE_THRESHOLD {
                return (
                    GateVerdict::Reject(
                        GateLevel::G5A,
                        format!("信号分 {} < 阈值 {SCORE_THRESHOLD}", scoring.score),
                    ),
                    scoring,
                );
            }
            let ref_file = ctx
                .events
                .iter()
                .flat_map(|e| e.excerpt.files.iter())
                .next()
                .cloned()
                .unwrap_or_default();
            if dedup_hit(ctx, Branch::Review, "code-review", &ref_file) {
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
            let (theme, is_awareness) = match ctx.facts {
                Some(f) if !f.framework.trim().is_empty() => (theme_from_facts(f), false),
                _ => ("framework-unknown".to_string(), true),
            };
            if !is_awareness && !ctx.force && ctx.cooldowns.recently_asked(&theme, ctx.now) {
                return (
                    GateVerdict::Reject(GateLevel::G2, format!("同主题 [{theme}] 30min 内已问过")),
                    scoring,
                );
            }
            let ref_file = ctx
                .events
                .iter()
                .flat_map(|e| e.excerpt.files.iter())
                .next()
                .cloned()
                .unwrap_or_default();
            if dedup_hit(ctx, Branch::Knowledge, &theme, &ref_file) {
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
            return Err(format!("队列已满({pending}/3):先回答或跳过卡片,再来一道"));
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
        }
    }

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
    fn g2_theme_cooldown_rejects() {
        let mut cd = CooldownMap::default();
        cd.touch_asked("vue-lifecycle", 100);
        let facts = Facts { framework: "Vue3".into(), ..Default::default() };
        let events = vec![ev("turn.ended", "think", 0, vec![], "", "总结", false, false, false)];
        // now=1500,距上次问询 1400s < 30min=1800s → G2 拒绝
        let (v, _) = evaluate(&ctx(events, 0, None, &cd, Some(&facts), &[], &[], 1500));
        assert!(matches!(v, GateVerdict::Reject(GateLevel::G2, _)));
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
            "message.updated", "read", 3, vec![], "", "看文件", false, false, false,
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
        let _ = &events;
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
        let events = vec![ev("turn.ended", "think", 0, vec![], "", "这轮改完了", false, false, false)];
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
        let facts = Facts { framework: "Nuxt3".into(), ..Default::default() };
        let events = vec![ev("turn.ended", "think", 0, vec![], "", "结束", false, false, false)];
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
