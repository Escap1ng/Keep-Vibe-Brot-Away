//! LLM 分析器:Provider 抽象(openai-compatible / google / anthropic,均 SSE 流式)
//! + 每日预算 + 单次 ≤5k token 预算裁剪 + 契约 JSON 解析与重试。

pub mod budget;
pub mod prompt;
pub mod providers;

use std::sync::Arc;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::gate::GatePass;
use crate::state::AppState;

/// 分析输出契约(规格书 7.4)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisOut {
    #[serde(rename = "type")]
    pub kind: String,
    pub severity: String,
    pub theme: String,
    pub text: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub ref_files: Vec<String>,
    #[serde(default)]
    pub optimized_prompt: Option<String>,
    #[serde(default)]
    pub project_path: Option<String>,
}

impl Default for AnalysisOut {
    fn default() -> Self {
        Self {
            kind: "tips".into(),
            severity: "info".into(),
            theme: String::new(),
            text: String::new(),
            options: crate::queue::PENDING_OPTIONS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            ref_files: vec![],
            optimized_prompt: None,
            project_path: None,
        }
    }
}

impl AnalysisOut {
    /// 无 LLM 环境下的确定性降级卡(仍可形成认知价值)
    pub fn fallback(pass: &GatePass, project_path: &str) -> Self {
        let mut out = AnalysisOut::default();
        out.project_path = Some(project_path.to_string());
        match pass.branch {
            crate::gate::Branch::Review => {
                out.kind = "review".into();
                out.severity = if pass.p0 { "P0".into() } else { "P1".into() };
                out.text = "本次 AI 产出有多处值得复查:关键代码块是否有注释、网络调用是否有异常处理?请快速浏览 diff 后作答。".into();
                out.theme = pass.theme_hint.clone();
            }
            crate::gate::Branch::Knowledge if pass.theme_hint == "framework-unknown" => {
                out.kind = "awareness".into();
                out.severity = "info".into();
                out.theme = "framework-unknown".into();
                out.text = "你知道这个项目现在用什么框架吗?回答后我会记住,并在之后按此讲解相关知识点。".into();
            }
            crate::gate::Branch::Knowledge => {
                out.kind = "tips".into();
                out.severity = "info".into();
                out.theme = pass.theme_hint.clone();
                out.text = "间隙小贴士:趁 Agent 干活,回顾一下这个主题的知识点(详见下次贴士)。".into();
            }
        }
        out
    }
}

/// 估算字符对应的 token 数(保守:中文按 1 字符 ≈ 1 token 取样,ASCII 4 字符 ≈ 1)
pub fn estimate_tokens(s: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for c in s.chars() {
        if (c as u32) > 0x2E7F {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other / 4 + 1
}

/// 按 token 预算裁剪字符串(保持 ≤ max_tokens 的估计)
pub fn fit_tokens(s: &str, max_tokens: usize) -> String {
    if estimate_tokens(s) <= max_tokens {
        return s.to_string();
    }
    // 逐段衰减:按 1/2 比例截断再估算
    let mut len = s.len();
    while len > 0 {
        let tmp: String = s.chars().take(len).collect();
        if estimate_tokens(&tmp) <= max_tokens {
            return format!("{}…(截断)", tmp);
        }
        len = len / 2;
    }
    String::new()
}

pub fn is_configured(state: &Arc<AppState>) -> bool {
    let core = state.core.blocking_lock();
    let settings = core.settings.clone();
    drop(core);
    if settings.model.trim().is_empty() {
        return false;
    }
    let ring = crate::crypto::Keyring::new(&state.data_dir);
    ring.get(&settings.provider).map(|k| !k.is_empty()).unwrap_or(false)
}

/// 收集并校验当前 LLM 配置
pub fn config_checked(state: &Arc<AppState>) -> Result<(String, String, Option<String>, String), String> {
    let core = state.core.blocking_lock();
    let settings = core.settings.clone();
    drop(core);
    if settings.model.trim().is_empty() {
        return Err("未配置模型名,请在设置面板填写".into());
    }
    let ring = crate::crypto::Keyring::new(&state.data_dir);
    let key = ring
        .get(&settings.provider)
        .ok_or_else(|| format!("未配置 {} 的 API Key", settings.provider))?;
    Ok((settings.provider, settings.model, settings.base_url, key))
}

/// 通用流式对话:聚合全部文本并返回
pub async fn chat_complete(
    state: &Arc<AppState>,
    system: &str,
    user: &str,
    max_output_tokens: u32,
) -> Result<String, String> {
    let (provider, model, base_url, key) = config_checked(state)?;
    let budget_ok = budget::check_then_inc(state);
    if let Err(budget::BudgetErr::Exceeded) = budget_ok {
        budget_toast_once(state);
        return Err(format!("{} 今日预算已用尽,进入攒批(仅累计信号,不调用 LLM)", budget::BUDGET_EXCEEDED_PREFIX));
    }

    let req = providers::ChatRequest {
        system: system.to_string(),
        user: user.to_string(),
        max_output_tokens,
    };

    // 指数退避重试 ≤2 次(共 3 次尝试)
    let mut last_err = String::new();
    for attempt in 0..3 {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| e.to_string())?;
        match providers::stream_chat(&client, &provider, &model, base_url.as_deref(), &key, &req).await {
            Ok(mut stream) => {
                let mut text = String::new();
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(t) => text.push_str(&t),
                        Err(e) => {
                            last_err = e.to_string();
                            break;
                        }
                    }
                }
                if !text.is_empty() {
                    return Ok(text);
                }
            }
            Err(e) => last_err = e.to_string(),
        }
        if attempt < 2 {
            let wait_ms = 800u64 << attempt;
            tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
        }
    }
    Err(format!("LLM 调用失败(已重试):{last_err}"))
}

/// 从文本中提取首个"括号平衡"的 JSON 对象
/// (带字符串状态与转义感知;避免贪婪匹配到尾部杂散 `}` 或误吞多个对象)
pub fn extract_json(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let rest = &text[start..];
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, ch) in rest.char_indices() {
        if in_str {
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_str = false;
            }
        } else {
            match ch {
                '"' => in_str = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&rest[..i + ch.len_utf8()]);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// 从 LLM 文本中稳健提取 JSON 契约(平衡括号扫描)
pub fn parse_contract(text: &str) -> Option<AnalysisOut> {
    serde_json::from_str(extract_json(text)?).ok()
}

/// 从 LLM 文本解析 facts 结构(facts 初始生成/合并)
pub fn parse_contract_facts(text: &str) -> Option<crate::memory::Facts> {
    serde_json::from_str(extract_json(text)?).ok()
}

/// G7 执行入口:分析门控通过的一次事件批 → 分析卡(取当前活动项目的上下文)
pub async fn analyze_pass(state: &Arc<AppState>, pass: &GatePass) -> Result<AnalysisOut, String> {
    analyze_pass_for_project(state, pass, "").await
}

/// G7 执行入口(指定项目版):供攒批消费时使用——攒批项可能在活动项目切换后才被消费
pub async fn analyze_pass_for_project(state: &Arc<AppState>, pass: &GatePass, project_path: &str) -> Result<AnalysisOut, String> {
    let snapshot = snapshot_for_analysis(state, pass, project_path);

    let system = prompt::system_role();
    let user = prompt::build_analysis_user(&snapshot);
    // 预算:system 0.6k + user ≤3.5k(逐字段裁剪) + 输出 0.8k → 单次总 ≤5k
    let text = chat_complete(state, &system, &user, 800).await?;

    if let Some(out) = parse_contract(&text) {
        return Ok(normalize_out(out, pass, &snapshot));
    }
    // 解析失败:重试一次(更严格提示)。注意:重试会再占一格当日预算。
    let strict = format!("{user}\n\n注意:请只返回一个 JSON 对象,不要包含任何其他文字、不要用 markdown 代码块包裹。");
    let text2 = chat_complete(state, &system, &strict, 800).await?;
    if let Some(out) = parse_contract(&text2) {
        return Ok(normalize_out(out, pass, &snapshot));
    }
    Ok(normalize_out(AnalysisOut::default(), pass, &snapshot))
}

fn normalize_out(mut out: AnalysisOut, pass: &GatePass, snap: &AnalysisSnapshot) -> AnalysisOut {
    let branch = pass.branch;
    let p0 = pass.p0;

    // 投影路径/类型/严重性兜底
    out.project_path = Some(snap.project_path.clone());
    if out.kind.is_empty() {
        out.kind = match branch {
            crate::gate::Branch::Review => "review".into(),
            crate::gate::Branch::Knowledge if pass.theme_hint == "framework-unknown" => "awareness".into(),
            crate::gate::Branch::Knowledge => "tips".into(),
        };
    }
    if out.theme.is_empty() {
        out.theme = pass.theme_hint.clone();
    }
    if out.options.is_empty() {
        out.options = crate::queue::PENDING_OPTIONS.iter().map(|s| s.to_string()).collect();
    }
    if out.ref_files.is_empty() {
        out.ref_files = snap.ref_files.clone();
    }
    if out.severity.is_empty() {
        out.severity = if p0 { "P0".into() } else { "info".into() };
    }
    if out.text.chars().count() > 120 {
        // 契约声明 ≤120 字,截断口径与之一致
        out.text = out.text.chars().take(120).collect();
    }
    out
}

/// 分析快照(规格书 7.3:固定结构,各字段字符预算裁剪,绝不超 ≤5k token)
pub struct AnalysisSnapshot {
    pub project_path: String,
    pub facts_text: String,
    pub dialog_text: String,
    pub diff_text: String,
    pub gate_text: String,
    pub ref_files: Vec<String>,
}

fn snapshot_for_analysis(state: &Arc<AppState>, pass: &GatePass, project_path: &str) -> AnalysisSnapshot {
    let mut snap = AnalysisSnapshot {
        project_path: String::new(),
        facts_text: String::new(),
        dialog_text: String::new(),
        diff_text: String::new(),
        gate_text: String::new(),
        ref_files: vec![],
    };
    let core = state.core.blocking_lock();
    // 指定项目(攒批消费)优先;空则取当前活动项目
    let key = if project_path.is_empty() {
        core.active_project.clone()
    } else {
        Some(crate::state::project_key(project_path))
    };
    let Some(key) = key else { return snap };
    let live = core.projects.get(&key);
    let Some(live) = live else { return snap };
    let path = live
        .events
        .front()
        .map(|e| e.project_path.clone())
        .unwrap_or_default();
    snap.project_path = path;
    snap.ref_files = live
        .events
        .iter()
        .flat_map(|e| e.excerpt.files.iter().cloned())
        .take(5)
        .collect();
    if let Some(f) = &live.facts {
        snap.facts_text = crate::memory::facts_prompt_text(f);
    }
    // 对话记忆:最近 3 条
    snap.dialog_text = live.dialog.iter().cloned().collect::<Vec<_>>().join("\n---\n");
    // diff 摘录:最近事件
    snap.diff_text = live
        .events
        .iter()
        .rev()
        .take(2)
        .map(|e| format!("[{}:{}]\n{}", e.tool_name, e.event_type, e.excerpt.diff_head))
        .collect::<Vec<_>>()
        .join("\n");
    drop(core);

    snap.gate_text = format!(
        "分支:{:?};信号分:{};P0:{};主题:{}",
        pass.branch, pass.score, pass.p0, pass.theme_hint
    );

    // 字符预算裁剪(≈5k token 上限内)
    snap.facts_text = fit_tokens(&snap.facts_text, 1200);
    snap.dialog_text = fit_tokens(&snap.dialog_text, 900);
    snap.diff_text = fit_tokens(&snap.diff_text, 1100);
    snap.gate_text = fit_tokens(&snap.gate_text, 300);
    snap
}

/// 设置面板「测试连接」:一次极小调用(不计重试),把 model/base/key 组合问题当场暴露
#[tauri::command]
pub async fn llm_test_connection(
    state: tauri::State<'_, std::sync::Arc<AppState>>,
) -> Result<String, String> {
    let state = state.inner().clone();
    let (provider, model, base, key) = config_checked(&state)?;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let req = providers::ChatRequest {
        system: "你是回声机".into(),
        user: "只回复两个字母:OK".into(),
        max_output_tokens: 8,
    };
    let mut stream = providers::stream_chat(&client, &provider, &model, base.as_deref(), &key, &req)
        .await
        .map_err(|e| e.to_string())?;
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(t) => text.push_str(&t),
            Err(e) => return Err(format!("流中断:{e}")),
        }
    }
    if text.trim().is_empty() {
        Err("端点连通但返回为空:请核对模型名是否存在于该端点".into())
    } else {
        Ok(format!(
            "连接成功:{} / {}({})",
            provider,
            model,
            base.as_deref().unwrap_or("官方端点")
        ))
    }
}

/// 攒批提示(托盘气泡/系统通知,每日一次)
pub fn budget_toast_once(state: &Arc<AppState>) {
    use tauri_plugin_notification::NotificationExt;
    let app = state.app.clone();
    let mut core = state.core.blocking_lock();
    if core.budget_toast_shown {
        return;
    }
    core.budget_toast_shown = true;
    drop(core);
    let _ = app
        .notification()
        .builder()
        .title("KVBA 预算已用尽")
        .body("今日 LLM 调用上限已达。进入攒批模式:只累计信号,不调用 LLM。明日自动恢复。")
        .show();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_contract_with_markdown_fence() {
        let text = "好的:\n```json\n{\"type\":\"tips\",\"severity\":\"info\",\"theme\":\"vue-lifecycle\",\"text\":\"贴士正文\"}\n```";
        let out = parse_contract(text).expect("代码块包裹的 JSON 应可解析");
        assert_eq!(out.theme, "vue-lifecycle");
    }

    #[test]
    fn parse_contract_string_with_braces_and_escapes() {
        // 字符串值内含 } 与转义引号 \"(JSON 合法转义);后面还跟着残缺的干扰对象
        let text = r#"prefix {"type":"tips","severity":"info","theme":"","text":"a } b \"quoted\""} trailing {"type":"junk"}"#;
        let out = parse_contract(text).expect("应取首个平衡对象");
        assert_eq!(out.text, "a } b \"quoted\"");
        assert!(out.theme.is_empty());
    }

    #[test]
    fn parse_contract_rejects_unbalanced() {
        assert!(parse_contract("{\"text\":\"no close").is_none());
        assert!(parse_contract("no json at all").is_none());
    }

    #[test]
    fn estimate_and_fit_tokens() {
        let s = "中".repeat(100) + &"a".repeat(400); // ≈ 100 + 100 = 200 token
        assert!((190..=210).contains(&estimate_tokens(&s)));
        let fitted = fit_tokens(&s, 50);
        // 含"…(截断)"后缀,允许小幅超出但必须远小于原文
        assert!(estimate_tokens(&fitted) <= 60);
        assert!(estimate_tokens(&fitted) < estimate_tokens(&s));
    }

    #[test]
    fn fit_tokens_noop_when_within_budget() {
        assert_eq!(fit_tokens("短文本", 100), "短文本");
    }
}
