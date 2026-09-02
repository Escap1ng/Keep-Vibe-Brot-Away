//! 提示词工厂:固定结构、预算裁剪,保证单次分析 ≤5k token;以及下一轮优化提示词生成。

use std::sync::Arc;

use crate::queue::Card;
use crate::state::AppState;

use super::AnalysisSnapshot;

/// system 角色职责 + 输出 JSON 契约(固定模板)
pub fn system_role() -> String {
    "你是一个「项目伴读」助手,产品名 KVBA。你的任务:在用户等待 CodingAgent 编程的间隙, \
     基于给定材料,产出与当前项目上下文相关的一条「审查质疑/知识贴士/知情度问题」,帮助用户保持对项目的掌控感。\
     严格约束:非侵入、简短、具体、中文。\
     必须只返回一个 JSON 对象,字段:\
     {\"type\":\"review|tips|awareness\",\"severity\":\"info|P1|P0\",\"theme\":\"如 vue-lifecycle/api-error-handling/framework-unknown\",\
     \"text\":\"≤120字的问题或贴士正文\",\"options\":[\"满意\",\"不满意\",\"自定义…\"],\
     \"refFiles\":[\"相关文件\"],\"optimizedPrompt\":\"(仅在 review 且预期用户可能不满意时才给:一句可粘贴给 CodingAgent 的修复指令,格式:修复 <文件/函数>:<问题>。<要求>。)\"}。\
     不要输出 JSON 以外的任何文字。"
        .into()
}

/// 单次分析用户提示(字段按 token 预算裁剪,绝不超 ≤5k)
pub fn build_analysis_user(snap: &AnalysisSnapshot) -> String {
    format!(
        "【门控上下文】{};\n\n【项目事实】\n{}\n\n【最近 3 个事件】\n{}\n\n【当前 diff 摘录】\n{}\n\n请给出唯一一条最佳卡片。",
        snap.gate_text, snap.facts_text, snap.dialog_text, snap.diff_text
    )
}

/// facts 初始生成提示(首版,一次 LLM 调用)
pub fn facts_v1_user(snapshot: &str) -> String {
    format!(
        "以下是新项目的本地扫描结果:\n```\n{snapshot}\n```\n\
         请提炼项目全局记忆,只返回 JSON:\
         {{\"framework\":\"框架名(未知写空字符串)\",\"techStack\":[\"技术选型\"],\
         \"dirSummary\":\"目录结构概要(≤80字)\",\"keyEntryPoints\":[\"关键入口文件/函数\"],\
         \"decisions\":[\"Agent 声明过的重要决策(如有)\"]}}。不要多余文字。"
    )
}

/// facts 增量合并提示(每消费 ≥20 次 或 会话结束时)
pub fn facts_merge_user(old_facts: &str, new_signals: &str) -> String {
    format!(
        "现有项目记忆:\n```\n{old_facts}\n```\n新信号(事件意图/用户回答):\n```\n{new_signals}\n```\n\
         增量合并记忆:只返回 JSON(结构同现记忆):\
         {{\"framework\":\"\",\"techStack\":[],\"dirSummary\":\"\",\"keyEntryPoints\":[],\
         \"decisions\":[],\"awarenessFacts\":[\"用户回答过的知情度事实,逐条记录\"]}}。\
         规则:内容有变化才更新,无变化原样返回;只更新确有把握的部分,不要臆造。"
    )
}

/// 下一轮优化提示词生成(≤800 token)
pub fn optimized_prompt_user(card_text: &str, ref_files: &str, diff: &str) -> String {
    format!(
        "用户对审查问题说「不满意」。基于以下卡片、引用文件与 diff,生成一段可直接粘贴给 CodingAgent 的「下一轮优化提示词」。\
         格式严格为:`修复 <引用文件/函数>:<描述问题>。<要求/约束>。` 一句话,中文,≤80字,不要多余文字。\n\
         卡片:{card_text}\n引用文件:{ref_files}\nDIFF:\n{diff}"
    )
}

/// 生成"下一轮优化提示词":写剪贴板 + 追加项目内 .kvba/next-prompt.md
/// 调用预算:独立小调用(输出 ≤240 token,输入裁剪);无 LLM 时用模板兜底。
pub async fn generate_optimized(state: &Arc<AppState>, card: &Card) -> anyhow::Result<String> {
    let diff = snapshot_ref_diff(state, card);

    let ref_files = card.ref_files.join(", ");
    if let Ok(text) = super::chat_complete(
        state,
        "你是项目伴读助手,负责生成「下一轮优化提示词」。只输出一句话指令。",
        &super::fit_tokens(&optimized_prompt_user(&card.text, &ref_files, &diff), 1200),
        240,
    )
    .await
    {
        let prompt = sanitize_prompt(&text);
        write_prompt_outputs(state, card, &prompt);
        return Ok(prompt);
    }

    // 模板兜底(无 LLM / 预算超限):仍完成闭环
    let fallback = fallback_optimized(card);
    write_prompt_outputs(state, card, &fallback);
    Ok(fallback)
}

/// 同步快照:取卡片引用事件的 diff(锁只在同步函数内持有)
fn snapshot_ref_diff(state: &Arc<AppState>, card: &Card) -> String {
    let core = state.core.blocking_lock();
    core.projects
        .values()
        .flat_map(|p| p.events.iter().rev().take(3))
        .find(|e| e.id == card.ref_event_id)
        .map(|e| e.excerpt.diff_head.clone())
        .unwrap_or_default()
}

fn sanitize_prompt(raw: &str) -> String {
    let mut t = raw.trim().to_string();
    // 剥掉可能的引号/代码块
    let t2 = t.trim_start_matches('`').trim_start_matches("修复").trim();
    t = format!("修复 {t2}");
    let t = t.replace("```", "");
    if t.chars().count() > 120 {
        t.chars().take(120).collect()
    } else {
        t
    }
}

fn fallback_optimized(card: &Card) -> String {
    let file = card
        .ref_files
        .first()
        .cloned()
        .unwrap_or_else(|| "相关文件".into());
    format!("修复 {file}:检查本次改动中边界条件与异常处理是否完整;补充关键注释并在接入 API 处添加 try/catch。")
}

/// 写剪贴板 + 追加 .kvba/next-prompt.md(项目根)
fn write_prompt_outputs(state: &Arc<AppState>, card: &Card, prompt: &str) {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    let app = state.app.clone();
    let _ = app.clipboard().write_text(prompt.to_string());

    if !card.project_path.is_empty() {
        let base = std::path::Path::new(&card.project_path);
        let dir = base.join(".kvba");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::warn!("创建 .kvba 目录失败:{e}");
            return;
        }
        let file = base.join(".kvba").join("next-prompt.md");
        let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let line = format!("\n## {stamp}\n{prompt}\n");
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
    crate::state::emit(
        &app,
        "prompt.generated",
        &serde_json::json!({ "prompt": prompt }),
    );
}
