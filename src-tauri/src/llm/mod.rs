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

/// 分析输出契约(规格书 7.4)。
/// 与 `Facts` 同理:允许 LLM 少回字段或用 snake_case,否则整份分析作废、退化成模板卡。
/// 容器级 `default` 复用下方手写 `Default`(它会填好默认按钮)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AnalysisOut {
    #[serde(rename = "type", alias = "kind")]
    pub kind: String,
    pub severity: String,
    pub theme: String,
    pub text: String,
    pub options: Vec<String>,
    #[serde(alias = "ref_files")]
    pub ref_files: Vec<String>,
    #[serde(alias = "optimized_prompt")]
    pub optimized_prompt: Option<String>,
    #[serde(alias = "project_path")]
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
        let mut out = Self {
            project_path: Some(project_path.to_string()),
            ..Default::default()
        };
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
                out.text =
                    "你知道这个项目现在用什么框架吗?回答后我会记住,并在之后按此讲解相关知识点。"
                        .into();
            }
            crate::gate::Branch::Knowledge => {
                out.kind = "tips".into();
                out.severity = "info".into();
                out.theme = pass.theme_hint.clone();
                out.text =
                    "间隙小贴士:趁 Agent 干活,回顾一下这个主题的知识点(详见下次贴士)。".into();
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
        len /= 2;
    }
    String::new()
}

/// 是否已具备调用 LLM 的最小配置(模型名 + 对应 provider 的 key)
pub fn is_configured(state: &Arc<AppState>) -> bool {
    config_checked(state).is_ok()
}

/// 收集并校验当前 LLM 配置
pub fn config_checked(
    state: &Arc<AppState>,
) -> Result<(String, String, Option<String>, String), String> {
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

/// 统一的 LLM HTTP 客户端(仅连接超时;整体超时由单次请求设定)
fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())
}

/// 通用流式对话:聚合全部文本并返回(**计入一格当日预算**)
pub async fn chat_complete(
    state: &Arc<AppState>,
    system: &str,
    user: &str,
    max_output_tokens: u32,
) -> Result<String, String> {
    let cfg = config_checked(state)?;
    if let Err(budget::BudgetErr::Exceeded) = budget::check_then_inc(state) {
        budget_toast_once(state);
        return Err(format!(
            "{} 今日预算已用尽,进入攒批(仅累计信号,不调用 LLM)",
            budget::BUDGET_EXCEEDED_PREFIX
        ));
    }
    stream_text(&cfg, system, user, max_output_tokens).await
}

/// 通用流式对话(**不计预算**):供"一次任务只预留一格额度"的调用方复用。
/// 典型场景是契约解析失败后的严格重试——额度已在首次调用时扣过,
/// 重试再扣会让一次分析吃掉两格当日预算。
pub async fn chat_complete_unbudgeted(
    state: &Arc<AppState>,
    system: &str,
    user: &str,
    max_output_tokens: u32,
) -> Result<String, String> {
    let cfg = config_checked(state)?;
    stream_text(&cfg, system, user, max_output_tokens).await
}

/// 真正的 HTTP 侧:指数退避重试 ≤2 次,聚合文本增量
async fn stream_text(
    cfg: &(String, String, Option<String>, String),
    system: &str,
    user: &str,
    max_output_tokens: u32,
) -> Result<String, String> {
    let (provider, model, base_url, key) = cfg;
    let req = providers::ChatRequest {
        system: system.to_string(),
        user: user.to_string(),
        max_output_tokens,
    };
    let client = http_client()?;

    // 指数退避重试 ≤2 次(共 3 次尝试)
    let mut last_err = "端点连通但返回为空(无有效内容增量)".to_string();
    for attempt in 0..3 {
        match providers::stream_chat(&client, provider, model, base_url.as_deref(), key, &req).await
        {
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
            tokio::time::sleep(std::time::Duration::from_millis(800u64 << attempt)).await;
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

/// facts 解析失败的**形态**。两种故障的修法完全相反,压成一个 `None` 就只能靠猜:
/// `Unclosed` 要提 `max_output_tokens`,`Shape` 要改契约或放宽结构体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactsParseErr {
    /// 扫不到配对的 `}` —— 几乎总是输出被 max_output_tokens 截断
    Unclosed,
    /// JSON 完整,但字段类型与契约不符(带 serde 原文)
    Shape(String),
}

impl FactsParseErr {
    pub fn as_str(&self) -> String {
        match self {
            Self::Unclosed => "扫不到闭合的 `}`(输出被 max_output_tokens 截断)".into(),
            Self::Shape(m) => format!("JSON 完整但契约不符:{m}"),
        }
    }
}

/// 从 LLM 文本解析 facts 结构(facts 初始生成/合并)
pub fn parse_contract_facts(text: &str) -> Result<crate::memory::Facts, FactsParseErr> {
    let Some(s) = extract_json(text) else {
        return Err(FactsParseErr::Unclosed);
    };
    serde_json::from_str(s).map_err(|e| FactsParseErr::Shape(e.to_string()))
}

/// 供 LLM 解析失败时打印的诊断串:长度 + 首尾片段。
/// 只留开头 240 字无法区分"被截断"和"格式不对"——**尾部才是判据**:
/// 截断的稿子必然停在数组或字符串中间,格式错的稿子则是完整闭合的。
pub fn text_probe(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n <= 200 {
        return format!("长度 {n} 字,全文:{}", chars.iter().collect::<String>());
    }
    let head: String = chars[..120].iter().collect();
    let tail: String = chars[n - 60..].iter().collect();
    format!("长度 {n} 字,首部:{head} …… 尾部:{tail}")
}

/// G7 执行入口:分析门控通过的一次事件批 → 分析卡(取当前活动项目的上下文)
pub async fn analyze_pass(state: &Arc<AppState>, pass: &GatePass) -> Result<AnalysisOut, String> {
    analyze_pass_for_project(state, pass, "").await
}

/// G7 执行入口(指定项目版):供攒批消费时使用——攒批项可能在活动项目切换后才被消费
pub async fn analyze_pass_for_project(
    state: &Arc<AppState>,
    pass: &GatePass,
    project_path: &str,
) -> Result<AnalysisOut, String> {
    let snapshot = snapshot_for_analysis(state, pass, project_path);

    let system = prompt::system_role();
    let user = prompt::build_analysis_user(&snapshot);
    // 预算:system 0.6k + user ≤3.5k(逐字段裁剪) + 输出 0.8k → 单次总 ≤5k
    let text = chat_complete(state, &system, &user, 800).await?;

    if let Some(out) = parse_contract(&text) {
        return Ok(normalize_out(out, pass, &snapshot));
    }
    // 解析失败:重试一次(更严格提示)。额度已由上面的 chat_complete 预留,
    // 重试走不计预算版本 —— 一次分析最多只吃一格当日额度。
    let strict = format!(
        "{user}\n\n注意:请只返回一个 JSON 对象,不要包含任何其他文字、不要用 markdown 代码块包裹。"
    );
    let text2 = chat_complete_unbudgeted(state, &system, &strict, 800).await?;
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
            crate::gate::Branch::Knowledge if pass.theme_hint == "framework-unknown" => {
                "awareness".into()
            }
            crate::gate::Branch::Knowledge => "tips".into(),
        };
    }
    if out.theme.is_empty() {
        out.theme = pass.theme_hint.clone();
    }
    if out.options.is_empty() {
        out.options = crate::queue::PENDING_OPTIONS
            .iter()
            .map(|s| s.to_string())
            .collect();
    }
    if out.ref_files.is_empty() {
        out.ref_files = snap.ref_files.clone();
    }
    if out.severity.is_empty() {
        out.severity = if p0 { "P0".into() } else { "info".into() };
    }
    if out.text.trim().is_empty() {
        // 正文为空会产出一张没有任何认知价值的空白卡,宁可退回模板文案
        out.text = AnalysisOut::fallback(pass, &snap.project_path).text;
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

fn snapshot_for_analysis(
    state: &Arc<AppState>,
    pass: &GatePass,
    project_path: &str,
) -> AnalysisSnapshot {
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
    snap.dialog_text = live
        .dialog
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n---\n");
    // diff 摘录:最近事件
    snap.diff_text = live
        .events
        .iter()
        .rev()
        .take(2)
        .map(|e| {
            format!(
                "[{}:{}]\n{}",
                e.tool_name, e.event_type, e.excerpt.diff_head
            )
        })
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
    let client = http_client()?;
    let req = providers::ChatRequest {
        system: "你是回声机".into(),
        user: "只回复两个字母:OK".into(),
        max_output_tokens: 8,
    };
    let mut stream =
        providers::stream_chat(&client, &provider, &model, base.as_deref(), &key, &req)
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

    /// 截断与类型不符的修法完全相反(提 max_tokens vs 改契约),
    /// 压成同一个 None 就等于让排查者重新猜一遍 —— 这里把两态钉住
    #[test]
    fn facts_parse_error_distinguishes_truncation_from_shape() {
        let truncated = "{\"framework\":\"Tauri + Vue 3\",\"techStack\":[\"Vue 3\",\"TypeScript\",\"Vite\",\"Tauri 2\",\"Rust\",\"OpenAI/G";
        assert_eq!(
            parse_contract_facts(truncated).unwrap_err(),
            FactsParseErr::Unclosed
        );

        let bad_shape = "{\"framework\":\"Vue 3\",\"techStack\":\"Vue 3\"}";
        match parse_contract_facts(bad_shape).unwrap_err() {
            // 注意:serde 只给类型不符与列号,**不给字段名**。
            // 所以调用方必须把原文一并打进日志(text_probe),否则排查者只能对着列号数。
            FactsParseErr::Shape(m) => assert!(
                m.contains("invalid type") && m.contains("expected a sequence"),
                "应带出 serde 的类型不符原文,实际:{m}"
            ),
            other => panic!("数组字段收到字符串应判为 Shape,实际:{other:?}"),
        }

        let ok = "{\"framework\":\"Vue 3\",\"techStack\":[\"Vite\"],\"dir_summary\":\"两目录\"}";
        let facts = parse_contract_facts(ok).expect("缺字段 + snake_case 别名应宽容通过");
        assert_eq!(facts.dir_summary, "两目录");
    }

    /// 诊断串必须带尾部:截断的稿子只有尾部能看出停在数组中间
    #[test]
    fn text_probe_keeps_tail_for_spotting_truncation() {
        let long = format!("{}{}", "x".repeat(300), "[\"tail-marker\"");
        let probe = text_probe(&long);
        assert!(probe.contains("长度 314 字"), "{probe}");
        assert!(
            probe.contains("尾部:") && probe.contains("tail-marker"),
            "{probe}"
        );
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

    /// LLM 少给字段 / 改用 snake_case 时不得整份作废(否则只会静默退化成模板卡)
    #[test]
    fn parse_contract_tolerates_partial_and_snake_case_payload() {
        let out = parse_contract(r#"{"type":"review","text":"这条 diff 里 fetch 没有异常处理"}"#)
            .expect("缺 severity/theme 应解析成功而非整份作废");
        assert_eq!(out.kind, "review");
        // 缺字段由手写 Default 兜底:severity 落到 "info",options 落到默认按钮
        assert_eq!(out.severity, "info");
        assert_eq!(out.options.len(), 3);

        let g = parse_contract(
            r#"{"kind":"tips","severity":"P1","theme":"rust-ownership","text":"正文","ref_files":["src/a.rs"]}"#,
        )
        .expect("type 的别名 kind 与 ref_files 的 snake_case 都应被接受");
        assert_eq!(g.kind, "tips");
        assert_eq!(g.ref_files, vec!["src/a.rs"]);
    }

    /// 空正文会产出一张毫无价值的空白卡,必须回落到模板文案
    #[test]
    fn normalize_out_replaces_empty_text_with_template() {
        let pass = GatePass {
            branch: crate::gate::Branch::Review,
            score: 90,
            p0: false,
            theme_hint: "code-review".into(),
            reason: "审查分支".into(),
        };
        let snap = AnalysisSnapshot {
            project_path: "C:/proj/foo".into(),
            facts_text: String::new(),
            dialog_text: String::new(),
            diff_text: String::new(),
            gate_text: String::new(),
            ref_files: vec![],
        };
        let out = normalize_out(
            AnalysisOut {
                kind: "review".into(),
                text: "   ".into(),
                ..Default::default()
            },
            &pass,
            &snap,
        );
        assert!(!out.text.trim().is_empty(), "空正文应被模板文案替换");
        assert_eq!(out.theme, "code-review");
        assert_eq!(out.project_path.as_deref(), Some("C:/proj/foo"));
    }
}
