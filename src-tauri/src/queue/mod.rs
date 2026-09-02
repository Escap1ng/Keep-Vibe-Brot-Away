//! 问题队列(消费驱动)+ 主题冷却矩阵 + 后台维护任务
//!
//! 队列规则:最多 3 张 pending;队列满 → 一切触发停止(由 G1 保证)。
//! 排泄口:① 回答 ② 手动跳过 ③ 不再问这类(写入冷却矩阵) ④ TTL 2 小时 ⑤ 引用文件又被修改 → stale。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::gate::{Branch, GatePass};
use crate::state::{now_ts, AppState};

pub const MAX_PENDING: usize = 3;
pub const TTL_SECS: i64 = 2 * 3600;
pub const PENDING_OPTIONS: &[&str] = &["满意", "不满意", "自定义…"];

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
pub fn build_card(pass: &GatePass, text: String, ref_files: Vec<String>, ref_event_id: String, project_path: String, severity: &str) -> Card {
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

/// 卡片入队(队列满时:默认丢弃;P0 且设置允许 → 替换最旧一张)
/// 含并发护栏:同指纹卡在 1h 内已存在(队列或历史)则拒绝入队,消除异步管线竞态重复
pub fn insert_card(core: &mut crate::state::Core, card: Card, emit: &dyn Fn(&str, &Card)) -> Option<Card> {
    // 竞态护栏:同指纹去重(与 G6 同规则,但加锁后复查)
    let fp = crate::gate::fingerprint(
        match card.kind.as_str() {
            "review" => Branch::Review,
            _ => Branch::Knowledge,
        },
        &card.theme,
        card.ref_files.first().map(|s| s.as_str()).unwrap_or(""),
    );
    let now = now_ts();
    let dup = core
        .queue
        .iter()
        .filter(|c| c.status == "pending")
        .chain(core.history.iter().filter(|c| now - c.created_at < crate::gate::DEDUP_WINDOW_SECS))
        .any(|c| crate::gate::fingerprint_for_card(c) == fp);
    if dup {
        log::info!("竞态护栏:同指纹卡片已存在,丢弃重复卡(theme={})", card.theme);
        return None;
    }

    let pending_count = core.queue.iter().filter(|c| c.status == "pending").count();
    if pending_count >= MAX_PENDING {
        if card.severity == "P0" && core.settings.p0_replace_queue {
            // 替换最旧的一张 pending
            let oldest = core
                .queue
                .iter()
                .find(|c| c.status == "pending")
                .cloned();
            if let Some(mut old_card) = oldest {
                old_card.status = "dismissed".into();
                emit("card.dismissed", &old_card);
                // 被替换的卡同样归档进历史,避免只改状态留在队列、重启后静默丢失
                archive_card(core, old_card);
            }
        } else {
            return None;
        }
    }

    // 主题冷却记录
    core.cooldowns.touch_asked(&card.theme, now_ts());
    core.last_card_at = Some(now_ts());
    core.stats.roll_to_today();
    core.stats.cards_created += 1;
    core.queue.push_back(card.clone());
    emit("card.new", &card);
    Some(card)
}

/// 归档已退场的卡片:移出队列并计入历史(上限 500,丢最旧)
pub fn archive_card(core: &mut crate::state::Core, card: Card) {
    core.queue.retain(|c| c.id != card.id);
    core.history.push(card);
    if core.history.len() > 500 {
        let overflow = core.history.len() - 500;
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
    std::fs::write(dir.join("history.json"), serde_json::to_string(&core.history)?)?;
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

/// 队列推送:回答 / 跳过 / 不再问这类
fn set_status_and_finish(
    state: &Arc<AppState>,
    card_id: &str,
    status: &str,
    answer: String,
    want_prompt: bool,
) -> anyhow::Result<Option<Card>> {
    let card = {
        let mut core = state.core.blocking_lock();
        let found = core.queue.iter_mut().find(|c| c.id == card_id);
        let Some(c) = found else {
            return Err(anyhow::anyhow!("卡片不存在:{card_id}"));
        };
        if c.status != "pending" {
            return Err(anyhow::anyhow!("卡片已处理:{}", c.status));
        }
        c.status = status.into();
        c.answer = answer.clone();
        if want_prompt {
            c.optimized_prompt = "(生成中)".into();
        }
        let clone = c.clone();
        clone
    };

    let mut core = state.core.blocking_lock();
    core.stats.roll_to_today();
    core.stats.answers += 1;
    if answer.contains("不满意") {
        core.stats.unsatisfied += 1;
    }
    // 冷却矩阵反馈:连续不满意 → 自动静默 1h
    let theme = card.theme.clone();
    let unsatisfied = answer.contains("不满意");
    let now = now_ts();
    core.cooldowns.note_answer(&theme, unsatisfied, now);
    // 知情度回答 → 记入项目事实(用户回答过的知情度事实)
    if card.kind == "awareness" && !answer.is_empty() {
        let key = crate::state::project_key(&card.project_path);
        if let Some(live) = core.projects.get_mut(&key) {
            if let Some(facts) = live.facts.as_mut() {
                let entry = format!("用户回答(知情度):{answer}");
                if !facts.awareness_facts.iter().any(|f| f == &entry) {
                    facts.awareness_facts.push(entry);
                    if let Some(pdir) = state_dir(&state, &card.project_path) {
                        let _ = crate::memory::save_facts(&pdir, facts);
                    }
                }
            }
        }
    }
    // 消费触发增量精炼(每 ≥20 次 → 异步合并)
    if let Some(live) = core.projects.get_mut(&crate::state::project_key(&card.project_path)) {
        live.consumed_since_merge += 1;
        if live.consumed_since_merge >= 20 {
            live.consumed_since_merge = 0;
            let pdir = state_dir(&state, &card.project_path);
            let facts = live.facts.clone();
            drop_facts_kick(state, pdir, facts, card.project_path.clone());
        }
    }
    // 移出队列并入历史(上限 500,丢最旧)
    archive_card(&mut core, card.clone());
    crate::state::save_core_locked(&state.data_dir, &core);
    drop(core);

    crate::state::emit(&state.app, "card.updated", &card);

    Ok(Some(card))
}

fn state_dir(state: &Arc<AppState>, project_path: &str) -> Option<PathBuf> {
    Some(state.project_dir(project_path))
}

fn drop_facts_kick(state: &Arc<AppState>, pdir: Option<PathBuf>, facts: Option<crate::memory::Facts>, project_path: String) {
    // 异步增量精炼(由 memory::merge_kick 完成;避免死锁:加载后脱离锁执行)
    let state = state.clone();
    let Some((pdir, facts)) = pdir.zip(facts) else { return };
    crate::memory::merge_kick(state, pdir, facts, project_path);
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
pub fn queue_dismiss(state: tauri::State<'_, Arc<AppState>>, card_id: String) -> Result<(), String> {
    let state = state.inner().clone();
    set_status_and_finish(&state, &card_id, "dismissed", "手动跳过".into(), false).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn queue_mute_theme(state: tauri::State<'_, Arc<AppState>>, card_id: String) -> Result<(), String> {
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

/// 回答卡片;不满意/自定义(非"满意")时生成下一轮优化提示词
#[tauri::command]
pub fn queue_answer(state: tauri::State<'_, Arc<AppState>>, card_id: String, answer: String) -> Result<Card, String> {
    let state = state.inner().clone();
    let card = {
        let core = state.core.blocking_lock();
        core.queue
            .iter()
            .find(|c| c.id == card_id)
            .cloned()
            .ok_or_else(|| "卡片不存在".to_string())?
    };

    let want_prompt = card.kind == "review"
        && (answer.starts_with("不满意") || answer.starts_with("自定义"))
        && !answer.starts_with("满意");
    let finished = set_status_and_finish(&state, &card_id, "answered", answer.clone(), want_prompt)
        .map_err(|e| e.to_string())?;
    let Some(finished) = finished else {
        return Ok(card);
    };

    if want_prompt {
        // 异步生成优化提示词:写剪贴板 + 追加 .kvba/next-prompt.md
        let state = state.clone();
        let f = finished.clone();
        tauri::async_runtime::spawn(async move {
            if let Ok(prompt) = crate::llm::prompt::generate_optimized(&state, &f).await {
                let mut core = state.core.blocking_lock();
                for c in core.queue.iter_mut() {
                    if c.id == f.id {
                        c.optimized_prompt = prompt.clone();
                        break;
                    }
                }
                for c in core.history.iter_mut() {
                    if c.id == f.id {
                        c.optimized_prompt = prompt.clone();
                        break;
                    }
                }
                crate::state::save_core(&state);
                drop(core);
                let mut updated = f.clone();
                updated.optimized_prompt = prompt;
                crate::state::emit(&state.app, "prompt.generated", &updated);
            }
        });
    }
    Ok(finished)
}

/// LLM 分析通过(G7)后:生成卡片并推送;返回是否成功入队(供"来一道"回传真实结局)
pub fn push_pass_card(app: &tauri::AppHandle, state: &Arc<AppState>, pass: &GatePass, analysis: crate::llm::AnalysisOut, ref_event_id: String) -> bool {
    let project_path = analysis.project_path.clone().unwrap_or_default();
    let core = state.core.blocking_lock();
    let text = analysis.text.clone();
    let ref_files = analysis.ref_files.clone();
    let severity = analysis.severity.clone();
    let kind = analysis.kind.clone();
    let theme = if analysis.theme.is_empty() {
        pass.theme_hint.clone()
    } else {
        analysis.theme.clone()
    };
    let card = build_card(pass, text, ref_files, ref_event_id, project_path, &severity);
    let card = Card {
        kind,
        theme,
        ..card
    };
    drop(core);

    let app2 = app.clone();
    let inserted = insert_card(
        &mut state.core.blocking_lock(),
        card.clone(),
        &move |ev, c| {
            crate::state::emit(&app2, ev, c);
        },
    )
    .is_some();
    crate::state::save_core(state);
    if inserted {
        // 新卡 → 展开胶囊
        crate::windows::show_panel(app);
    }
    inserted
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
            text: "?".into(),
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
        assert!(cd.is_muted("vue-lifecycle", 1100), "连续 2 次不满意应静默 1h");
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
            assert!(insert_card(&mut core, mk_card(&format!("t{i}"), "tips", "info", &format!("f{i}.ts")), &noop_emit()).is_some());
        }
        assert_eq!(core.queue.iter().filter(|c| c.status == "pending").count(), 3);
        // 队列满:默认配置下直接丢弃
        assert!(insert_card(&mut core, mk_card("t3", "tips", "info", "f3.ts"), &noop_emit()).is_none());
        assert_eq!(core.queue.len(), 3);
    }

    #[test]
    fn insert_card_p0_replaces_oldest_when_allowed_and_archives() {
        let mut core = Core::empty(Settings { p0_replace_queue: true, ..Default::default() });
        for i in 0..3 {
            insert_card(&mut core, mk_card(&format!("t{i}"), "tips", "info", &format!("f{i}.ts")), &noop_emit());
        }
        let before_history = core.history.len();
        let r = insert_card(&mut core, mk_card("code-review", "review", "P0", "fx.ts"), &noop_emit());
        assert!(r.is_some(), "P0 允许替换时应入队");
        assert_eq!(core.queue.iter().filter(|c| c.status == "pending").count(), 3, "总数仍 ≤3");
        // 被替换的卡归档进历史而不是留在队列里等重启丢失
        assert_eq!(core.history.len(), before_history + 1);
        assert!(core.history.iter().any(|c| c.status == "dismissed"));
        assert_eq!(core.queue.iter().filter(|c| c.status != "pending").count(), 0);
    }

    #[test]
    fn insert_card_rejects_duplicate_fingerprint() {
        let mut core = Core::empty(Settings::default());
        let c1 = mk_card("vue-lifecycle", "tips", "info", "a.ts");
        assert!(insert_card(&mut core, c1.clone(), &noop_emit()).is_some());
        // 同主题同引用文件 → 同指纹,id 却不同;竞态护栏应拒绝
        let c2 = mk_card("vue-lifecycle", "tips", "info", "a.ts");
        assert_ne!(c1.id, c2.id);
        assert!(insert_card(&mut core, c2, &noop_emit()).is_none());
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
        assert!(core.history.first().unwrap().theme != "tt0", "最旧的应被丢弃");
    }

    #[test]
    fn norm_path_unifies_separators_and_case() {
        assert_eq!(norm_path("C:\\Proj\\SRC\\App.vue"), norm_path("c:/proj/src/app.VUE"));
        assert_ne!(norm_path("src/app.vue"), norm_path("other/src/app.vue"));
    }
}
