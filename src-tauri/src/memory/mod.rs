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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Facts {
    pub framework: String,
    pub tech_stack: Vec<String>,
    pub dir_summary: String,
    pub key_entry_points: Vec<String>,
    pub decisions: Vec<String>,
    #[serde(default)]
    pub awareness_facts: Vec<String>,
    #[serde(default)]
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

/// 由摄入管线调用:新项目且无 facts → 调度异步生成首版(扫描 + 一次 LLM 调用)
pub fn kick_facts_if_needed(state: &Arc<AppState>, project_path: &str) {
    let project = project_path.to_string();
    {
        let core = state.core.blocking_lock();
        let key = crate::state::project_key(&project);
        if let Some(live) = core.projects.get(&key) {
            if live.facts.is_some() {
                return;
            }
        }
    }
    // 重启后已存在 facts.json → 直接载入,不再花一次 LLM 调用
    let pdir = state.project_dir(&project);
    if let Some(facts) = load_facts(&pdir) {
        let mut core = state.core.blocking_lock();
        core.project_live(&project).facts = Some(facts);
        return;
    }

    let state = state.clone();
    tauri::async_runtime::spawn(async move {
        let snapshot = scan_project(std::path::Path::new(&project));
        let user = crate::llm::prompt::facts_v1_user(&crate::llm::fit_tokens(&snapshot, 2000));
        match crate::llm::chat_complete(
            &state,
            "你是项目架构记录助手。按 JSON 契约输出,不要多余文字。",
            &user,
            400,
        )
        .await
        {
            Ok(text) => match crate::llm::parse_contract_facts(&text) {
                Some(facts) => {
                    let pdir = state.project_dir(&project);
                    publish_facts(&state, &pdir, &project, facts);
                }
                None => log::warn!("facts 首版返回不可解析的 JSON,已跳过"),
            },
            Err(e) => {
                log::warn!("facts 首版生成失败(不影响主流程):{e}");
            }
        }
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
        match crate::llm::chat_complete(
            &state,
            "你是项目记忆精炼助手。只返回 JSON,不要多余文字。",
            &user,
            500,
        )
        .await
        {
            Ok(text) => match crate::llm::parse_contract_facts(&text) {
                Some(merged) => publish_facts(&state, &pdir, &project_path, merged),
                None => log::warn!("facts 增量合并返回不可解析的 JSON,已跳过"),
            },
            Err(e) => {
                log::warn!("facts 增量合并失败:{e}");
            }
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
