//! 记忆层:项目全局记忆(facts,按项目隔离)+ 对话滚动窗口(在 state.rs 内)
//!
//! - 初始化:项目首次出现事件时本地扫描 → 一次 LLM 调用生成首版 → facts.json(≤50KB);
//! - 增量精炼:每消费 ≥20 次问题 或 项目会话结束时,才调用一次 LLM 增量合并(固定提示词模板);
//! - 禁止对 facts 做"每触发一次"的重写。

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::state::{now_ts, AppState};

/// facts.json 体积上限(超出即分级收缩)
pub const FACTS_MAX_BYTES: usize = 50 * 1024;
/// 收缩时保留的最近决策条数
const KEEP_DECISIONS: usize = 4;
/// 收缩时保留的用户知情度事实条数
const KEEP_AWARENESS: usize = 20;
/// 收缩后目录概览的最小保留字符数
const DIR_SUMMARY_FLOOR: usize = 200;

/// 项目记忆。字段全部可缺省(`default`)并兼容 snake_case 别名:
/// LLM 少回一个字段或换了命名风格是常态,严格反序列化会让整份记忆静默丢失
/// (卡片路径有模板兜底,记忆路径没有,一旦解析失败就再也立不起来)。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Facts {
    pub framework: String,
    #[serde(alias = "tech_stack")]
    pub tech_stack: Vec<String>,
    #[serde(alias = "dir_summary")]
    pub dir_summary: String,
    #[serde(alias = "key_entry_points")]
    pub key_entry_points: Vec<String>,
    pub decisions: Vec<String>,
    #[serde(alias = "awareness_facts")]
    pub awareness_facts: Vec<String>,
    #[serde(alias = "generated_at")]
    pub generated_at: i64,
}

/// facts 的提示词形态(截断)
pub fn facts_prompt_text(f: &Facts) -> String {
    serde_json::to_string(f).unwrap_or_default()
}

pub fn save_facts(pdir: &Path, facts: &Facts) -> anyhow::Result<()> {
    let mut raw = serde_json::to_string_pretty(facts)?;
    if raw.len() > FACTS_MAX_BYTES {
        // 超限收缩顺序:先丢较旧的 decisions(可由事件流重新推导),
        // awareness_facts 是用户亲口回答的知情度事实,仅保留最近若干条;
        // 最后才逐半压缩 dir_summary。绝不整体清空用户事实。
        let mut f = facts.clone();
        let cut = f.decisions.len().saturating_sub(KEEP_DECISIONS);
        f.decisions.drain(0..cut);
        let cut = f.awareness_facts.len().saturating_sub(KEEP_AWARENESS);
        f.awareness_facts.drain(0..cut);
        raw = serde_json::to_string_pretty(&f)?;
        while raw.len() > FACTS_MAX_BYTES && f.dir_summary.chars().count() > DIR_SUMMARY_FLOOR {
            let half = f.dir_summary.chars().count() / 2;
            f.dir_summary = f.dir_summary.chars().take(half).collect();
            raw = serde_json::to_string_pretty(&f)?;
        }
    }
    std::fs::write(pdir.join("facts.json"), raw)?;
    Ok(())
}

pub fn load_facts(pdir: &Path) -> Option<Facts> {
    let raw = std::fs::read_to_string(pdir.join("facts.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

/// 本地扫描:package.json / README / Cargo.toml / 目录树(限量深度) → 摘要文本
pub fn scan_project(root: &Path) -> String {
    const MAX_TREE_ENTRIES: usize = 80;
    let mut out = String::new();

    if let Ok(s) = std::fs::read_to_string(root.join("package.json")) {
        out.push_str("[package.json]\n");
        out.push_str(&s.chars().take(3000).collect::<String>());
        out.push('\n');
    }
    if let Ok(s) = std::fs::read_to_string(root.join("Cargo.toml")) {
        out.push_str("[Cargo.toml]\n");
        out.push_str(&s.chars().take(1500).collect::<String>());
        out.push('\n');
    }
    if let Ok(s) = std::fs::read_to_string(root.join("go.mod")) {
        out.push_str("[go.mod]\n");
        out.push_str(&s.chars().take(1200).collect::<String>());
        out.push('\n');
    }
    if let Ok(s) = std::fs::read_to_string(root.join("pyproject.toml")) {
        out.push_str("[pyproject.toml]\n");
        out.push_str(&s.chars().take(1200).collect::<String>());
        out.push('\n');
    }
    if let Ok(md) = std::fs::read_to_string(root.join("README.md")) {
        out.push_str("[README.md]\n");
        out.push_str(&md.chars().take(1800).collect::<String>());
        out.push('\n');
    }

    let mut tree = String::new();
    let mut count = 0usize;
    if let Ok(entries) = std::fs::read_dir(root) {
        let mut names: Vec<(String, bool)> = entries
            .filter_map(|e| e.ok())
            .map(|e| {
                let ok = e.path().is_dir();
                (e.file_name().to_string_lossy().to_string(), ok)
            })
            .filter(|(n, _)| {
                !n.starts_with('.')
                    && !matches!(
                        n.as_str(),
                        "node_modules" | "target" | "dist" | "build" | "vendor"
                    )
            })
            .collect();
        names.sort();
        for (name, is_dir) in names {
            if count >= MAX_TREE_ENTRIES {
                tree.push_str("…(截断)");
                break;
            }
            tree.push_str(&if is_dir {
                format!("{name}/\n")
            } else {
                format!("{name}\n")
            });
            count += 1;
            if is_dir
                && count < MAX_TREE_ENTRIES
                && matches!(name.as_str(), "src" | "app" | "lib" | "pages")
            {
                if let Ok(sub) = std::fs::read_dir(root.join(&name)) {
                    let mut subnames: Vec<String> = sub
                        .filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .filter(|n| !n.starts_with('.'))
                        .collect();
                    subnames.sort();
                    for s in subnames {
                        if count >= MAX_TREE_ENTRIES {
                            break;
                        }
                        tree.push_str(&format!("  {s}\n"));
                        count += 1;
                    }
                }
            }
        }
    }
    out.push_str("[目录树(≤2层)]\n");
    out.push_str(&tree);
    out
}

/// 会话结束:已生成过 facts 或有未精炼的消费记录时,异步做一次增量合并
pub fn session_merge(state: &Arc<AppState>, project_path: &str) {
    let (pdir, facts) = {
        let core = state.core.blocking_lock();
        match core.projects.get(&crate::state::project_key(project_path)) {
            Some(live) if live.consumed_since_merge > 0 || live.facts.is_some() => {
                (state.project_dir(project_path), live.facts.clone())
            }
            _ => return,
        }
    };
    let Some(facts) = facts else { return };
    {
        let mut core = state.core.blocking_lock();
        if let Some(live) = core
            .projects
            .get_mut(&crate::state::project_key(project_path))
        {
            live.consumed_since_merge = 0;
        }
    }
    merge_kick(state.clone(), pdir, facts, project_path.to_string());
}

/// 精炼结果统一出口:盖时间戳 → 落盘 → 写回内存窗口 → 广播前端
fn publish_facts(state: &Arc<AppState>, pdir: &Path, project_path: &str, mut facts: Facts) {
    facts.generated_at = now_ts();
    let _ = save_facts(pdir, &facts);
    {
        let mut core = state.core.blocking_lock();
        core.project_live(project_path).facts = Some(facts);
    }
    crate::state::emit(
        &state.app,
        "facts.updated",
        &serde_json::json!({ "ok": true }),
    );
}

/// 启动时调用:为已恢复的活动项目补生成 facts(事件窗口已从 events.log 恢复,
/// 但 Agent 若正闲置就再也不会触发 `kick_facts_if_needed`,记忆会一直空着)
pub fn kick_facts_for_active(state: &Arc<AppState>) {
    let path = {
        let core = state.core.blocking_lock();
        core.active_project
            .as_ref()
            .and_then(|key| core.projects.get(key))
            .and_then(|live| live.events.back())
            .map(|e| e.project_path.clone())
    };
    if let Some(p) = path {
        kick_facts_if_needed(state, &p);
    }
}

/// 同一项目 facts 首版连续失败几次后开始冷却
const FACTS_FAIL_LIMIT: u32 = 3;
/// 冷却多久(秒)。事件是逐条进来的,没有这道闸门就会每条事件重跑一次注定失败的调用
const FACTS_FAIL_COOLDOWN: i64 = 6 * 3600;

/// facts 取稿:调用 → 解析;仅当"调用成功但解析失败"才更严格地重试一次。
/// 首次调用计预算、重试不计预算,一次任务最多只吃一格当日额度(与 `analyze_pass` 同一口径)。
/// **传输/预算错误一律 `?` 直接上抛**:重试通道是免预算的,若在这里也重试就等于把预算闸门绕穿。
/// 错误串自带故障形态,日志能直接指出是丢数据还是改契约。
///
/// 输出上限一律走 `crate::llm::MAX_OUTPUT_TOKENS`,**这里不要再留本地数字** ——
/// 上一轮 facts=400 / 卡片=800 的漂移正是我误判根因的起点。
async fn facts_complete(state: &Arc<AppState>, system: &str, user: &str) -> Result<Facts, String> {
    let text =
        crate::llm::chat_complete(state, system, user, crate::llm::MAX_OUTPUT_TOKENS).await?;
    if let Ok(facts) = crate::llm::parse_contract_facts(&text) {
        return Ok(facts);
    }
    let strict_user = format!(
        "{user}\n\n注意:只返回一个完整闭合的 JSON 对象,不要任何其他文字,也不要用 markdown 代码块包裹。"
    );
    let text2 = crate::llm::chat_complete_unbudgeted(
        state,
        system,
        &strict_user,
        crate::llm::MAX_OUTPUT_TOKENS,
    )
    .await?;
    crate::llm::parse_contract_facts(&text2).map_err(|e| {
        format!(
            "两次均未拿到合法 JSON:{};{}",
            e.as_str(),
            crate::llm::text_probe(&text2)
        )
    })
}

/// 该失败是否值得计入冷却。预算耗尽不是"这条路径坏了",而是"现在没额度"——
/// 记进冷却会让额度恢复后仍被挡 6 小时,反而把记忆永久饿死。
fn is_budget_exceeded(err: &str) -> bool {
    err.starts_with(crate::llm::budget::BUDGET_EXCEEDED_PREFIX)
}

/// 由摄入管线调用:新项目且无 facts → 调度异步生成首版(扫描 + 一次 LLM 调用)
pub fn kick_facts_if_needed(state: &Arc<AppState>, project_path: &str) {
    let project = project_path.to_string();
    let key = crate::state::project_key(&project);
    {
        let core = state.core.blocking_lock();
        if let Some(live) = core.projects.get(&key) {
            if live.facts.is_some() {
                return;
            }
        }
        if let Some(&(fails, until)) = core.facts_fail.get(&key) {
            if fails >= FACTS_FAIL_LIMIT && crate::state::now_ts() < until {
                log::debug!("facts 首版已连败 {fails} 次,冷却中,跳过本项目");
                return;
            }
        }
    }
    // 重启后已存在 facts.json → 直接载入,不再花一次 LLM 调用
    let pdir = state.project_dir(&project);
    if let Some(facts) = load_facts(&pdir) {
        let mut core = state.core.blocking_lock();
        core.project_live(&project).facts = Some(facts);
        core.facts_fail.remove(&key);
        return;
    }

    // 同一项目只留一个在途任务:事件是逐条进来的,不去重就会每条事件各烧一次预算
    if !state
        .core
        .blocking_lock()
        .facts_inflight
        .insert(key.clone())
    {
        return;
    }

    let state = state.clone();
    tauri::async_runtime::spawn(async move {
        let snapshot = scan_project(std::path::Path::new(&project));
        let user = crate::llm::prompt::facts_v1_user(&crate::llm::fit_tokens(&snapshot, 2000));
        match facts_complete(
            &state,
            "你是项目架构记录助手。按 JSON 契约输出,不要多余文字。",
            &user,
        )
        .await
        {
            Ok(facts) => {
                let pdir = state.project_dir(&project);
                publish_facts(&state, &pdir, &project, facts);
                state.core.blocking_lock().facts_fail.remove(&key);
                log::info!("facts 首版已生成:{project}");
            }
            Err(reason) => {
                if is_budget_exceeded(&reason) {
                    // 额度耗尽不计入连败:下一条事件仍应照常尝试,否则记忆会被饿死
                    log::info!("facts 首版暂缓(今日预算已用尽):{reason}");
                } else {
                    let fails = {
                        let mut core = state.core.blocking_lock();
                        let next = core.facts_fail.get(&key).map(|(n, _)| n + 1).unwrap_or(1);
                        core.facts_fail.insert(
                            key.clone(),
                            (next, crate::state::now_ts() + FACTS_FAIL_COOLDOWN),
                        );
                        next
                    };
                    log::warn!("facts 首版生成失败(累计 {fails} 次,{FACTS_FAIL_LIMIT} 次后冷却 {FACTS_FAIL_COOLDOWN}s):{reason}");
                }
            }
        }
        state.core.blocking_lock().facts_inflight.remove(&key);
    });
}

/// 增量精炼:每消费 ≥20 次问题 或项目会话结束时调用(固定提示词,<=1 次)
pub fn merge_kick(
    state: Arc<AppState>,
    pdir: std::path::PathBuf,
    facts: Facts,
    project_path: String,
) {
    tauri::async_runtime::spawn(async move {
        // 取增量信号:仅该项目的最近事件意图(避免跨项目混采)
        let signals = {
            let core = state.core.blocking_lock();
            core.projects
                .get(&crate::state::project_key(&project_path))
                .map(|p| {
                    p.events
                        .iter()
                        .rev()
                        .take(20)
                        .map(|e| format!("[{}] {}", e.tool_name, e.excerpt.intent))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default()
        };
        let old_text = serde_json::to_string(&facts).unwrap_or_default();
        let user = crate::llm::prompt::facts_merge_user(
            &crate::llm::fit_tokens(&old_text, 1200),
            &crate::llm::fit_tokens(&signals, 1200),
        );
        match facts_complete(
            &state,
            "你是项目记忆精炼助手。只返回 JSON,不要多余文字。",
            &user,
        )
        .await
        {
            Ok(merged) => publish_facts(&state, &pdir, &project_path, merged),
            Err(reason) => log::warn!("facts 增量合并失败:{reason}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("kvba-test-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tmp_dir("roundtrip");
        let f = Facts {
            framework: "Nuxt3".into(),
            ..Default::default()
        };
        save_facts(&dir, &f).unwrap();
        let got = load_facts(&dir).unwrap();
        assert_eq!(got.framework, "Nuxt3");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// LLM 常少回字段或改用 snake_case;严格反序列化会让整份记忆静默丢失
    #[test]
    fn deserialize_tolerates_missing_and_snake_case_fields() {
        let f: Facts =
            serde_json::from_str(r#"{"framework":"Next.js","tech_stack":["TypeScript"]}"#).unwrap();
        assert_eq!(f.framework, "Next.js");
        assert_eq!(f.tech_stack, vec!["TypeScript"]);
        assert!(f.decisions.is_empty(), "缺字段应为空集合而非整体失败");
        assert!(f.dir_summary.is_empty());

        let g: Facts = serde_json::from_str(
            r#"{"framework":"Vue3","keyEntryPoints":["src/main.ts"],"awareness_facts":["a"]}"#,
        )
        .unwrap();
        assert_eq!(g.key_entry_points, vec!["src/main.ts"]);
        assert_eq!(g.awareness_facts, vec!["a"]);
    }

    /// 契约里的 5 个字段全给时必须一字不丢(别名不能破坏正路)
    #[test]
    fn deserialize_keeps_full_camel_case_payload() {
        let raw = r#"{"framework":"Tauri","techStack":["Rust","Vue"],"dirSummary":"src/ …","keyEntryPoints":["src/lib.rs"],"decisions":["用 axum 做本地摄入"],"awarenessFacts":["用户回答(知情度):Tauri"],"generatedAt":123}"#;
        let f: Facts = serde_json::from_str(raw).unwrap();
        assert_eq!(f.tech_stack, vec!["Rust", "Vue"]);
        assert_eq!(f.dir_summary, "src/ …");
        assert_eq!(f.generated_at, 123);
    }

    #[test]
    fn shrink_on_overflow_keeps_recent_awareness_and_limits_decisions() {
        let dir = tmp_dir("shrink");
        let f = Facts {
            decisions: (0..30)
                .map(|i| format!("决策{i}:{}", "d".repeat(400)))
                .collect(),
            awareness_facts: (0..30)
                .map(|i| format!("用户事实{i}:{}", "a".repeat(400)))
                .collect(),
            dir_summary: "s".repeat(70 * 1024), // 单字段即超限
            ..Default::default()
        };
        save_facts(&dir, &f).unwrap();

        let raw = std::fs::read_to_string(dir.join("facts.json")).unwrap();
        assert!(
            raw.len() <= FACTS_MAX_BYTES + 1024,
            "超限应被收缩到约 50KB 内"
        );
        let got = load_facts(&dir).unwrap();
        // 用户回答的知情度事实必须保留(最多 20 条),决策收缩到 ≤4 条
        assert!(!got.awareness_facts.is_empty(), "知情度事实不得整体清空");
        assert!(got.awareness_facts.len() <= 20);
        assert!(
            got.awareness_facts.last().unwrap().contains("用户事实29"),
            "应保留最新的"
        );
        assert!(got.decisions.len() <= 4);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
