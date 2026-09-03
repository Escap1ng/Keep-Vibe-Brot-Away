//! 问题队列(消费驱动)+ 主题冷却矩阵 + 后台维护任务
//!
//! 队列规则:最多 3 张 pending;队列满 → 一切触发停止(由 G1 保证)。
//! 排泄口:① 回答 ② 手动跳过 ③ 不再问这类(写入冷却矩阵) ④ TTL 2 小时 ⑤ 引用文件又被修改 → stale。

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::gate::{Branch, GatePass};
use crate::state::{now_ts, AppState};

pub const MAX_PENDING: usize = 3;
pub const TTL_SECS: i64 = 2 * 3600;
pub const PENDING_OPTIONS: &[&str] = &["满意", "不满意", "自定义…"];
/// 历史卡片归档上限(超出丢最旧)
pub const HISTORY_MAX: usize = 500;
/// 每消费这么多张卡,触发一次 facts 增量精炼
pub const FACTS_MERGE_EVERY: u32 = 20;
/// 优化提示词生成中的占位(前端据此不显示空提示词)
pub const PROMPT_PENDING_MARK: &str = "(生成中)";
/// 判定"不满意"的作答前缀
pub const UNSATISFIED_MARK: &str = "不满意";

/// 卡片 Schema(与规格书 5.2 一致)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Card {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub severity: String,
    pub text: String,
    pub options: Vec<String>,
    pub theme: String,
    pub ref_files: Vec<String>,
    pub ref_event_id: String,
    pub created_at: i64,
    pub status: String,
    #[serde(default)]
    pub answer: String,
    #[serde(default)]
    pub optimized_prompt: String,
    #[serde(default)]
    pub project_path: String,
}

/// 主题冷却矩阵条目
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ThemeCooldown {
    pub last_asked_at: i64,
    pub asked_count: u32,
    /// 连续"不满意"次数(≥2 触发 1h 静默)
    pub unsatisfied_streak: u32,
    pub muted_until: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CooldownMap {
    pub themes: HashMap<String, ThemeCooldown>,
}

impl CooldownMap {
    /// 主题是否处于静默期(手动"不再问这类"或连续 2 次不满意)
    pub fn is_muted(&self, theme: &str, now: i64) -> bool {
        self.themes
            .get(theme)
            .map(|t| t.muted_until > now)
            .unwrap_or(false)
    }

    /// 同主题 30 分钟内是否已问
    pub fn recently_asked(&self, theme: &str, now: i64) -> bool {
        self.themes
            .get(theme)
            .map(|t| now - t.last_asked_at < crate::gate::THEME_COOLDOWN_SECS)
            .unwrap_or(false)
    }

    /// 记录一次问询
    pub fn touch_asked(&mut self, theme: &str, now: i64) {
        let t = self.themes.entry(theme.to_string()).or_default();
        t.last_asked_at = now;
        t.asked_count += 1;
    }

    /// 用户对某主题作答:记录满意/不满意;连续 2 次不满意 → 自动静默 1h
    pub fn note_answer(&mut self, theme: &str, unsatisfied: bool, now: i64) {
        let t = self.themes.entry(theme.to_string()).or_default();
        let streak_window = now - 24 * 3600; // 1 天窗口内的连续计
        if unsatisfied {
            if t.last_asked_at > streak_window {
                t.unsatisfied_streak += 1;
            } else {
                t.unsatisfied_streak = 1;
            }
            if t.unsatisfied_streak >= 2 {
                t.muted_until = now + crate::gate::THEME_MUTE_SECS;
                t.unsatisfied_streak = 0;
            }
        } else {
            t.unsatisfied_streak = 0;
        }
    }

    /// 手动"不再问这类":静默 1h
    pub fn mute(&mut self, theme: &str, now: i64) {
        let t = self.themes.entry(theme.to_string()).or_default();
        t.muted_until = now + crate::gate::THEME_MUTE_SECS;
    }
}

/// 从 GatePass 生成卡片
pub fn build_card(
    pass: &GatePass,
    text: String,
    ref_files: Vec<String>,
    ref_event_id: String,
    project_path: String,
    severity: &str,
) -> Card {
    Card {
        id: uuid::Uuid::new_v4().to_string(),
        kind: match pass.branch {
            Branch::Review => "review".into(),
            Branch::Knowledge if pass.theme_hint == "framework-unknown" => "awareness".into(),
            Branch::Knowledge => "tips".into(),
        },
        severity: if severity.is_empty() {
            if pass.p0 {
                "P0".into()
            } else {
                "info".into()
            }
        } else {
            severity.into()
        },
        text,
        options: PENDING_OPTIONS.iter().map(|s| s.to_string()).collect(),
        theme: pass.theme_hint.clone(),
        ref_files,
        ref_event_id,
        created_at: now_ts(),
        status: "pending".into(),
        answer: String::new(),
        optimized_prompt: String::new(),
        project_path,
    }
}

/// 入队结局。必须区分:把"队列已满"与"内容雷同"混成一条提示,使用者就再也
/// 无从判断该去消费卡片还是该换个主题 —— 这次误诊正是源于此。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted,
    /// 队列已满且不允许替换
    QueueFull,
    /// 正文与在队/近期历史卡片雷同
    Duplicate,
}

/// 卡片入队(队列满时:默认丢弃;P0 且设置允许 → 替换最旧一张)
///
/// 去重口径 = **正文**指纹(出稿后的真判据):同款正文即便主题不同也不该再递一遍。
/// 主题级近似(省钱闸)由 G6 负责,两者职责不重叠,不可混用同一指纹。
pub fn insert_card(
    core: &mut crate::state::Core,
    card: Card,
    emit: &dyn Fn(&str, &Card),
) -> InsertOutcome {
    let now = now_ts();
    let fp = crate::gate::fingerprint_text_for_card(&card);
    let dup = core
        .queue
        .iter()
        .filter(|c| c.status == "pending")
        .chain(
            core.history
                .iter()
                .filter(|c| now - c.created_at < crate::gate::DEDUP_WINDOW_SECS),
        )
        .any(|c| crate::gate::fingerprint_text_for_card(c) == fp);
    if dup {
        log::info!("正文去重:同款卡片 1h 内已递过(theme={})", card.theme);
        return InsertOutcome::Duplicate;
    }

    let pending_count = core.queue.iter().filter(|c| c.status == "pending").count();
    if pending_count >= MAX_PENDING {
        if card.severity == "P0" && core.settings.p0_replace_queue {
            // 替换最旧的一张 pending(队列按创建顺序入队,队首即最旧)
            if let Some(mut old_card) = core.queue.iter().find(|c| c.status == "pending").cloned() {
                old_card.status = "dismissed".into();
                emit("card.dismissed", &old_card);
                // 被替换的卡同样归档进历史,避免只改状态留在队列、重启后静默丢失
                archive_card(core, old_card);
            }
        } else {
            return InsertOutcome::QueueFull;
        }
    }

    // 主题冷却记录
    core.cooldowns.touch_asked(&card.theme, now);
    core.last_card_at = Some(now);
    core.stats.roll_to_today();
    core.stats.cards_created += 1;
    core.queue.push_back(card.clone());
    emit("card.new", &card);
    InsertOutcome::Inserted
}

/// 归档已退场的卡片:移出队列并计入历史(上限 HISTORY_MAX,丢最旧)
pub fn archive_card(core: &mut crate::state::Core, card: Card) {
    core.queue.retain(|c| c.id != card.id);
    core.history.push(card);
    if core.history.len() > HISTORY_MAX {
        let overflow = core.history.len() - HISTORY_MAX;
        core.history.drain(0..overflow);
    }
}

/// 归一化文件路径用于比对:统一分隔符并转小写(Windows 大小写不敏感)
fn norm_path(p: &str) -> String {
    p.replace('\\', "/").to_lowercase()
}

/// 持久化:queue.json 仅存 pending 卡(历史单独存 history.json),避免混存翻倍
pub fn persist_queue(core: &crate::state::Core, dir: &std::path::Path) -> anyhow::Result<()> {
    let pending: Vec<Card> = core
        .queue
        .iter()
        .filter(|c| c.status == "pending")
        .cloned()
        .collect();
    std::fs::write(dir.join("queue.json"), serde_json::to_string(&pending)?)?;
    std::fs::write(
        dir.join("history.json"),
        serde_json::to_string(&core.history)?,
    )?;
    Ok(())
}

/// 后台维护:每 60s 清扫 TTL 过期 + 引用文件再编辑 → stale
pub fn spawn_maintenance(handle: &tauri::AppHandle) {
    let app = handle.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let state_arc: Arc<AppState> = {
                let s = app.state::<Arc<AppState>>();
                s.inner().clone()
            };
            let mut changed = false;
            let now = now_ts();
            {
                let mut core = state_arc.core.blocking_lock();
                // TTL 过期 → dismissed 并归档进历史(避免重启后静默丢失、历史不可见)
                let expired: Vec<Card> = core
                    .queue
                    .iter()
                    .filter(|c| c.status == "pending" && now - c.created_at > TTL_SECS)
                    .cloned()
                    .collect();
                for mut card in expired {
                    card.status = "dismissed".into();
                    changed = true;
                    crate::state::emit(&app, "card.dismissed", &card);
                    archive_card(&mut core, card);
                }
                // 引用文件被"再次"修改 → 已过时。
                // 仅统计卡片创建之后到达的事件(生成卡片的事件本身不算"再次修改");
                // 路径用归一化精确比对,不用后缀匹配(避免误判同名尾缀的不同文件)。
                let stale_cards: Vec<_> = core
                    .queue
                    .iter()
                    .filter(|c| c.status == "pending" && !c.ref_files.is_empty())
                    .map(|c| {
                        let key = crate::state::project_key(&c.project_path);
                        let touched = core
                            .projects
                            .get(&key)
                            .map(|p| {
                                p.events.iter().rev().take(30).any(|e| {
                                    e.ts > c.created_at
                                        && e.excerpt.files.iter().any(|f| {
                                            let nf = norm_path(f);
                                            c.ref_files.iter().any(|r| norm_path(r) == nf)
                                        })
                                })
                            })
                            .unwrap_or(false);
                        (c.clone(), touched)
                    })
                    .filter(|(_, touched)| *touched)
                    .collect();
                for (mut card, _) in stale_cards {
                    card.status = "stale".into();
                    changed = true;
                    crate::state::emit(&app, "card.updated", &card);
                    archive_card(&mut core, card);
                }
                if changed {
                    crate::state::save_core_locked(&state_arc.data_dir, &core);
                }
            }
            // 攒批消费:额度恢复且未撞频控/队列上限时,补发一条暂存的门控通过项
            crate::llm::budget::try_drain_batch(&state_arc);
        }
    });
}

/// 队列推送:回答 / 跳过 / 不再问这类。返回卡片终态(供上层继续处理)。
/// 冷却反馈、事实增量、归档与落盘都在此完成;锁只在同步段内持有。
fn set_status_and_finish(
    state: &Arc<AppState>,
    card_id: &str,
    status: &str,
    answer: String,
    want_prompt: bool,
) -> anyhow::Result<Card> {
    let card = {
        let mut core = state.core.blocking_lock();
        let c = core
            .queue
            .iter_mut()
            .find(|c| c.id == card_id)
            .ok_or_else(|| anyhow::anyhow!("卡片不存在:{card_id}"))?;
        if c.status != "pending" {
            return Err(anyhow::anyhow!("卡片已处理:{}", c.status));
        }
        c.status = status.into();
        c.answer = answer.clone();
        // 只有审查卡值得生成"下一轮优化提示词";上层据占位符判断是否触发异步生成
        if want_prompt && c.kind == "review" {
            c.optimized_prompt = PROMPT_PENDING_MARK.into();
        }
        c.clone()
    };

    let mut core = state.core.blocking_lock();
    core.stats.roll_to_today();
    core.stats.answers += 1;
    // 冷却矩阵反馈:连续不满意 → 自动静默 1h
    let unsatisfied = answer.contains(UNSATISFIED_MARK);
    if unsatisfied {
        core.stats.unsatisfied += 1;
    }
    let now = now_ts();
    core.cooldowns.note_answer(&card.theme, unsatisfied, now);

    if let Some(live) = core
        .projects
        .get_mut(&crate::state::project_key(&card.project_path))
    {
        // 知情度回答 → 记入项目事实(用户亲口回答,优先级最高)
        if card.kind == "awareness" && !answer.is_empty() {
            let entry = format!("用户回答(知情度):{answer}");
            if let Some(facts) = live.facts.as_mut() {
                if !facts.awareness_facts.contains(&entry) {
                    facts.awareness_facts.push(entry);
                    let _ =
                        crate::memory::save_facts(&state.project_dir(&card.project_path), facts);
                }
            }
        }
        // 消费计数驱动增量精炼(每 ≥20 次异步合并一次)
        live.consumed_since_merge += 1;
        if live.consumed_since_merge >= FACTS_MERGE_EVERY {
            live.consumed_since_merge = 0;
            if let Some(facts) = live.facts.clone() {
                let state = state.clone();
                let pdir = state.project_dir(&card.project_path);
                // 脱离锁后再异步精炼,避免后台任务与当前命令争锁死锁
                crate::memory::merge_kick(state, pdir, facts, card.project_path.clone());
            }
        }
    }
    // 移出队列并入历史(上限 HISTORY_MAX,丢最旧)
    archive_card(&mut core, card.clone());
    crate::state::save_core_locked(&state.data_dir, &core);
    drop(core);

    crate::state::emit(&state.app, "card.updated", &card);
    Ok(card)
}

#[tauri::command]
pub fn queue_listen(state: tauri::State<'_, Arc<AppState>>) -> Vec<Card> {
    state
        .core
        .blocking_lock()
        .queue
        .iter()
        .filter(|c| c.status == "pending")
        .cloned()
        .collect()
}

#[tauri::command]
pub fn queue_dismiss(
    state: tauri::State<'_, Arc<AppState>>,
    card_id: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    set_status_and_finish(&state, &card_id, "dismissed", "手动跳过".into(), false)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn queue_mute_theme(
    state: tauri::State<'_, Arc<AppState>>,
    card_id: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    let theme = {
        let core = state.core.blocking_lock();
        core.queue
            .iter()
            .find(|c| c.id == card_id)
            .map(|c| c.theme.clone())
    };
    if let Some(theme) = theme {
        let mut core = state.core.blocking_lock();
        core.cooldowns.mute(&theme, now_ts());
        crate::state::save_core_locked(&state.data_dir, &core);
        drop(core);
    }
    set_status_and_finish(&state, &card_id, "dismissed", "不再问这类".into(), false)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 回答卡片;审查卡被判"不满意/自定义"时,异步生成下一轮优化提示词
#[tauri::command]
pub fn queue_answer(
    state: tauri::State<'_, Arc<AppState>>,
    card_id: String,
    answer: String,
) -> Result<Card, String> {
    let state = state.inner().clone();
    let asked_for_prompt = answer.starts_with(UNSATISFIED_MARK) || answer.starts_with("自定义");
    let card = set_status_and_finish(&state, &card_id, "answered", answer, asked_for_prompt)
        .map_err(|e| e.to_string())?;

    // 占位符仍在,说明本卡确实需要(且已被登记为)生成优化提示词
    if card.optimized_prompt == PROMPT_PENDING_MARK {
        // 异步生成优化提示词:写剪贴板 + 追加 .kvba/next-prompt.md
        let state = state.clone();
        let pending_card = card.clone();
        tauri::async_runtime::spawn(async move {
            if let Ok(prompt) = crate::llm::prompt::generate_optimized(&state, &pending_card).await
            {
                {
                    let mut core = state.core.blocking_lock();
                    let mut hit = |c: &mut Card| {
                        if c.id == pending_card.id {
                            c.optimized_prompt = prompt.clone();
                        }
                    };
                    core.queue.iter_mut().for_each(&mut hit);
                    core.history.iter_mut().for_each(hit);
                    crate::state::save_core_locked(&state.data_dir, &core);
                }
                let mut updated = pending_card.clone();
                updated.optimized_prompt = prompt;
                crate::state::emit(&state.app, "prompt.generated", &updated);
            }
        });
    }
    Ok(card)
}

/// LLM 分析通过(G7)后:生成卡片并推送;返回入队结局(供"来一道"回传真实原因)
pub fn push_pass_card(
    app: &tauri::AppHandle,
    state: &Arc<AppState>,
    pass: &GatePass,
    analysis: crate::llm::AnalysisOut,
    ref_event_id: String,
) -> InsertOutcome {
    let severity = analysis.severity.clone();
    let theme = if analysis.theme.is_empty() {
        pass.theme_hint.clone()
    } else {
        analysis.theme.clone()
    };
    let card = build_card(
        pass,
        analysis.text.clone(),
        analysis.ref_files.clone(),
        ref_event_id,
        analysis.project_path.clone().unwrap_or_default(),
        &severity,
    );
    // LLM 对卡片类型/主题的判断优先于门控预判
    let card = Card {
        kind: analysis.kind.clone(),
        theme,
        ..card
    };

    let app2 = app.clone();
    let outcome = insert_card(
        &mut state.core.blocking_lock(),
        card.clone(),
        &move |ev, c| {
            crate::state::emit(&app2, ev, c);
        },
    );
    crate::state::save_core(state);
    if matches!(outcome, InsertOutcome::Inserted) {
        // 新卡 → 展开胶囊
        crate::windows::show_panel(app);
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use crate::state::Core;

    fn mk_card(theme: &str, kind: &str, severity: &str, ref_file: &str) -> Card {
        Card {
            id: uuid::Uuid::new_v4().to_string(),
            kind: kind.into(),
            severity: severity.into(),
            // 正文必须随主题变化:入队去重的判据是正文,全用同一句会让"队列上限"用例先撞去重
            text: format!("?{theme}"),
            options: PENDING_OPTIONS.iter().map(|s| s.to_string()).collect(),
            theme: theme.into(),
            ref_files: vec![ref_file.to_string()],
            ref_event_id: String::new(),
            created_at: now_ts(),
            status: "pending".into(),
            answer: String::new(),
            optimized_prompt: String::new(),
            project_path: "C:/proj/foo".into(),
        }
    }

    fn noop_emit() -> impl Fn(&str, &Card) {
        |_, _| {}
    }

    #[test]
    fn cooldown_two_unsatisfied_mutes_theme_for_1h() {
        let mut cd = CooldownMap::default();
        cd.note_answer("vue-lifecycle", true, 1000);
        assert!(!cd.is_muted("vue-lifecycle", 1000), "1 次不满意不应静默");
        cd.note_answer("vue-lifecycle", true, 1100);
        assert!(
            cd.is_muted("vue-lifecycle", 1100),
            "连续 2 次不满意应静默 1h"
        );
        assert!(cd.is_muted("vue-lifecycle", 1100 + crate::gate::THEME_MUTE_SECS - 1));
        assert!(!cd.is_muted("vue-lifecycle", 1100 + crate::gate::THEME_MUTE_SECS + 1));
    }

    #[test]
    fn cooldown_satisfied_answer_resets_streak() {
        let mut cd = CooldownMap::default();
        cd.note_answer("t", true, 1000);
        cd.note_answer("t", false, 1100); // 满意 → 清零
        cd.note_answer("t", true, 1200);
        assert!(!cd.is_muted("t", 1200), "中断后的单次不满意不应触发静默");
    }

    #[test]
    fn insert_card_respects_queue_cap_and_default_discards() {
        let mut core = Core::empty(Settings::default());
        for i in 0..3 {
            assert_eq!(
                insert_card(
                    &mut core,
                    mk_card(&format!("t{i}"), "tips", "info", &format!("f{i}.ts")),
                    &noop_emit()
                ),
                InsertOutcome::Inserted
            );
        }
        assert_eq!(
            core.queue.iter().filter(|c| c.status == "pending").count(),
            3
        );
        // 队列满:默认配置下直接丢弃
        assert_eq!(
            insert_card(
                &mut core,
                mk_card("t3", "tips", "info", "f3.ts"),
                &noop_emit()
            ),
            InsertOutcome::QueueFull
        );
        assert_eq!(core.queue.len(), 3);
    }

    #[test]
    fn insert_card_p0_replaces_oldest_when_allowed_and_archives() {
        let mut core = Core::empty(Settings {
            p0_replace_queue: true,
            ..Default::default()
        });
        for i in 0..3 {
            insert_card(
                &mut core,
                mk_card(&format!("t{i}"), "tips", "info", &format!("f{i}.ts")),
                &noop_emit(),
            );
        }
        let before_history = core.history.len();
        let r = insert_card(
            &mut core,
            mk_card("code-review", "review", "P0", "fx.ts"),
            &noop_emit(),
        );
        assert_eq!(r, InsertOutcome::Inserted, "P0 允许替换时应入队");
        assert_eq!(
            core.queue.iter().filter(|c| c.status == "pending").count(),
            3,
            "总数仍 ≤3"
        );
        // 被替换的卡归档进历史而不是留在队列里等重启丢失
        assert_eq!(core.history.len(), before_history + 1);
        assert!(core.history.iter().any(|c| c.status == "dismissed"));
        assert_eq!(
            core.queue.iter().filter(|c| c.status != "pending").count(),
            0
        );
    }

    #[test]
    fn insert_card_rejects_duplicate_text() {
        let mut core = Core::empty(Settings::default());
        let c1 = mk_card("vue-lifecycle", "tips", "info", "a.ts");
        assert_eq!(
            insert_card(&mut core, c1.clone(), &noop_emit()),
            InsertOutcome::Inserted
        );
        // 同主题同引用文件 → 同正文,id 却不同;应判为雷同
        let c2 = mk_card("vue-lifecycle", "tips", "info", "a.ts");
        assert_ne!(c1.id, c2.id);
        assert_eq!(
            insert_card(&mut core, c2, &noop_emit()),
            InsertOutcome::Duplicate
        );
    }

    /// 本次线上故障的反证:入队去重只看正文,不再看"主题+事件首文件"。
    /// 否则 G6(主题级近似)与护栏口径不一致,会出现"门控放行、入队却莫名被拒"。
    #[test]
    fn same_theme_different_text_is_admitted() {
        let mut core = Core::empty(Settings::default());
        let mut a = mk_card("framework-unknown", "tips", "info", "src/App.vue");
        a.text = "这个仓库没有 README,新人怎么知道从哪跑起来?".into();
        let mut b = mk_card("framework-unknown", "tips", "info", "src/App.vue");
        b.text = "同一个 framework-unknown 主题,但这次问的是测试策略。".into();
        assert_eq!(
            insert_card(&mut core, a, &noop_emit()),
            InsertOutcome::Inserted
        );
        assert_eq!(
            insert_card(&mut core, b, &noop_emit()),
            InsertOutcome::Inserted
        );
        assert_eq!(core.queue.len(), 2);
    }

    /// 仍在队未消费的卡片:不受 1h 窗口影响,任何时候都算重复
    /// (否则会把同一句话同时挂两份在胶囊里)
    #[test]
    fn pending_card_blocks_same_text_regardless_of_age() {
        let mut core = Core::empty(Settings::default());
        let c = mk_card("vue-lifecycle", "tips", "info", "a.ts");
        assert_eq!(
            insert_card(&mut core, c.clone(), &noop_emit()),
            InsertOutcome::Inserted
        );
        core.queue.make_contiguous().iter_mut().for_each(|k| {
            k.created_at -= crate::gate::DEDUP_WINDOW_SECS + 10;
        });
        assert_eq!(
            insert_card(&mut core, c, &noop_emit()),
            InsertOutcome::Duplicate,
            "卡片还挂在队里就不该再递同款"
        );
    }

    /// 已归档卡片:1h 窗口内挡、窗口外放行(只防短期复读,不做永久封禁)。
    /// 现场故障即 ②:19:44 那张卡 28 分钟后仍把同款挡在门外。
    #[test]
    fn archived_card_blocks_same_text_only_within_window() {
        let mut core = Core::empty(Settings::default());
        let c = mk_card("vue-lifecycle", "tips", "info", "a.ts");
        assert_eq!(
            insert_card(&mut core, c.clone(), &noop_emit()),
            InsertOutcome::Inserted
        );
        // 消费退场 → 归档进历史(created_at 保持"刚刚")
        let mut pending = std::mem::take(&mut core.queue);
        for card in pending.drain(..) {
            archive_card(&mut core, card);
        }
        assert_eq!(
            insert_card(&mut core, c.clone(), &noop_emit()),
            InsertOutcome::Duplicate,
            "1h 内的同款仍应挡"
        );
        for h in core.history.iter_mut() {
            h.created_at -= crate::gate::DEDUP_WINDOW_SECS + 10;
        }
        assert_eq!(
            insert_card(&mut core, c, &noop_emit()),
            InsertOutcome::Inserted,
            "出窗口后同款可以再递"
        );
    }

    #[test]
    fn archive_card_moves_out_of_queue_and_caps_history_at_500() {
        let mut core = Core::empty(Settings::default());
        let c = mk_card("t", "tips", "info", "f.ts");
        core.queue.push_back(c.clone());
        archive_card(&mut core, c);
        assert!(core.queue.is_empty());
        assert_eq!(core.history.len(), 1);
        // 灌满 600 张 → 上限 500,丢最旧
        for i in 0..600 {
            let c = mk_card(&format!("tt{i}"), "tips", "info", &format!("x{i}.ts"));
            archive_card(&mut core, c);
        }
        assert_eq!(core.history.len(), 500);
        assert!(
            core.history.first().unwrap().theme != "tt0",
            "最旧的应被丢弃"
        );
    }

    #[test]
    fn norm_path_unifies_separators_and_case() {
        assert_eq!(
            norm_path("C:\\Proj\\SRC\\App.vue"),
            norm_path("c:/proj/src/app.VUE")
        );
        assert_ne!(norm_path("src/app.vue"), norm_path("other/src/app.vue"));
    }
}
