//! 记忆层:项目全局记忆(facts,按项目隔离)+ 对话滚动窗口(在 state.rs 内)
//!
//! - 初始化:项目首次出现事件时本地扫描 → 一次 LLM 调用生成首版 → facts.json(≤50KB);
//! - 增量精炼:每消费 ≥20 次问题 或 项目会话结束时,才调用一次 LLM 增量合并(固定提示词模板);
//! - 禁止对 facts 做"每触发一次"的重写。

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::state::{now_ts, AppState};

pub const FACTS_MAX_BYTES: usize = 50 * 1024;

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
    let raw = serde_json::to_string_pretty(facts)?;
    if raw.as_bytes().len() > FACTS_MAX_BYTES {
        // 超限:裁掉可再压缩的决策与贴士字段
        let mut f = facts.clone();
        f.decisions.clear();
        f.awareness_facts.clear();
        let raw2 = serde_json::to_string_pretty(&f)?;
        std::fs::write(pdir.join("facts.json"), raw2)?;
        return Ok(());
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
                !n.starts_with('.') && !matches!(n.as_str(), "node_modules" | "target" | "dist" | "build" | "vendor")
            })
            .collect();
        names.sort();
        for (name, is_dir) in names {
            if count >= MAX_TREE_ENTRIES {
                tree.push_str("…(截断)");
                break;
            }
            tree.push_str(&if is_dir { format!("{name}/\n") } else { format!("{name}\n") });
            count += 1;
            if is_dir && count < MAX_TREE_ENTRIES && matches!(name.as_str(), "src" | "app" | "lib" | "pages") {
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

/// 会话结束:有消费记录(<20 也应触发一次增量合并)时异步精炼
pub fn session_merge(state: &Arc<AppState>, project_path: &str) {
    let (pdir, facts) = {
        let core = state.core.blocking_lock();
        let key = crate::state::project_key(project_path);
        match core.projects.get(&key) {
            Some(live) if live.consumed_since_merge > 0 => {
                (state.project_dir(project_path), live.facts.clone())
            }
            Some(live) if live.facts.is_some() => {
                (state.project_dir(project_path), live.facts.clone())
            }
            _ => return,
        }
    };
    let Some(facts) = facts else { return };
    let mut core = state.core.blocking_lock();
    if let Some(live) = core.projects.get_mut(&crate::state::project_key(project_path)) {
        live.consumed_since_merge = 0;
    }
    drop(core);
    merge_kick(state.clone(), pdir, facts);
}

/// 由摄入管线调用:新项目且无 facts → 调度异步生成首版(扫描 + 一次 LLM 调用)
pub fn kick_facts_if_needed(state: &Arc<AppState>, project_path: &str) {
    let project: String = project_path.to_string();
    {
        let core = state.core.blocking_lock();
        let key = crate::state::project_key(&project);
        if let Some(live) = core.projects.get(&key) {
            if live.facts.is_some() {
                return;
            }
        }
    }
    // 重启后已存在 facts.json → 直接载入
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
            Ok(text) => {
                if let Some(mut facts) = crate::llm::parse_contract_facts(&text) {
                    facts.generated_at = now_ts();
                    let pdir = state.project_dir(&project);
                    let _ = save_facts(&pdir, &facts);
                    let mut core = state.core.blocking_lock();
                    core.project_live(&project).facts = Some(facts);
                    drop(core);
                    crate::state::emit(&state.app, "facts.updated", &serde_json::json!({ "ok": true }));
                }
            }
            Err(e) => {
                log::warn!("facts 首版生成失败(不影响主流程):{e}");
            }
        }
    });
}

/// 增量精炼:每消费 ≥20 次问题 或项目会话结束时调用(固定提示词,<=1 次)
pub fn merge_kick(state: Arc<AppState>, pdir: std::path::PathBuf, facts: Facts) {
    tauri::async_runtime::spawn(async move {
        // 取增量信号:活动项目的最近事件意图
        let signals = {
            let core = state.core.blocking_lock();
            core.projects
                .values()
                .flat_map(|p| p.events.iter().rev().take(20))
                .map(|e| format!("[{}] {}", e.tool_name, e.excerpt.intent))
                .collect::<Vec<_>>()
                .join("\n")
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
            Ok(text) => {
                if let Some(merged) = crate::llm::parse_contract_facts(&text) {
                    let mut m = merged;
                    m.generated_at = now_ts();
                    let _ = save_facts(&pdir, &m);
                    let key = pdir
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let mut core = state.core.blocking_lock();
                    if let Some(live) = core.projects.get_mut(&key) {
                        live.facts = Some(m);
                    }
                    drop(core);
                    crate::state::emit(&state.app, "facts.updated", &serde_json::json!({ "ok": true }));
                }
            }
            Err(e) => {
                log::warn!("facts 增量合并失败:{e}");
            }
        }
    });
}
