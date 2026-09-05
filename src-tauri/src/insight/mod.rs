//! Evidence-backed Insight domain model and deterministic Phase 1 analyzer.

use serde::{Deserialize, Serialize};

use crate::events::NormalizedEvent;
use crate::workunit::{ChangeScope, FileChange, TestStatus, WorkUnit};

pub const INSIGHT_KEEP: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum InsightType {
    Risk,
    Decision,
    TestGap,
    #[default]
    Explain,
    Question,
}

impl InsightType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Risk => "risk",
            Self::Decision => "decision",
            Self::TestGap => "test-gap",
            Self::Explain => "explain",
            Self::Question => "question",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceKind {
    File,
    Line,
    Diff,
    Test,
    Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RiskLevel {
    #[default]
    #[serde(rename = "info")]
    Info,
    #[serde(rename = "P2")]
    P2,
    #[serde(rename = "P1")]
    P1,
    #[serde(rename = "P0")]
    P0,
}

impl RiskLevel {
    pub fn card_severity(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::P2 | Self::P1 => "P1",
            Self::P0 => "P0",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    pub kind: EvidenceKind,
    #[serde(rename = "ref")]
    pub reference: String,
    pub excerpt: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Insight {
    pub id: String,
    pub work_unit_id: String,
    #[serde(rename = "type")]
    pub kind: InsightType,
    pub conclusion: String,
    pub evidence: Vec<Evidence>,
    pub importance: String,
    pub suggested_action: String,
    pub risk_level: RiskLevel,
    pub confidence: f32,
}

pub fn analyze(work_unit: &WorkUnit, source_events: &[NormalizedEvent]) -> Vec<Insight> {
    let mut insights = Vec::new();
    if let Some(summary) = change_summary(work_unit, source_events) {
        insights.push(summary);
    }
    if let Some(test_gap) = test_gap(work_unit) {
        insights.push(test_gap);
    }
    if let Some(risk) = event_risk(work_unit, source_events) {
        insights.push(risk);
    }
    if let Some(decision) = configuration_decision(work_unit) {
        insights.push(decision);
    }
    insights
}

pub fn primary(insights: &[Insight]) -> Option<&Insight> {
    [
        InsightType::Risk,
        InsightType::Decision,
        InsightType::TestGap,
        InsightType::Explain,
        InsightType::Question,
    ]
    .into_iter()
    .find_map(|kind| insights.iter().find(|insight| insight.kind == kind))
}

fn change_summary(work_unit: &WorkUnit, source_events: &[NormalizedEvent]) -> Option<Insight> {
    if !work_unit.change_set.files.is_empty() {
        let additions: u64 = work_unit
            .change_set
            .files
            .iter()
            .map(|file| file.additions)
            .sum();
        let deletions: u64 = work_unit
            .change_set
            .files
            .iter()
            .map(|file| file.deletions)
            .sum();
        let unmeasured = work_unit
            .change_set
            .files
            .iter()
            .filter(|file| file.additions == 0 && file.deletions == 0)
            .count();
        let names = work_unit
            .change_set
            .files
            .iter()
            .take(3)
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>()
            .join("、");
        let evidence: Vec<Evidence> = work_unit
            .change_set
            .files
            .iter()
            .take(8)
            .map(|file| file_evidence(file, work_unit.change_set.scope))
            .collect();
        let line_summary = if unmeasured == 0 {
            format!("+{additions}/-{deletions}")
        } else {
            format!("已统计 +{additions}/-{deletions}，{unmeasured} 个文件行数未计")
        };
        let subject = match work_unit.change_set.scope {
            ChangeScope::WorktreeAtTurnEnd => "回合结束时 Git 工作区可见",
            ChangeScope::PartialWorktreeAtTurnEnd => "Git 证据未完整采集，当前拿到",
            ChangeScope::AgentEvents => "Agent 写事件声明本回合涉及",
            ChangeScope::Unavailable => "当前拿到未分级证据对应",
        };
        let conclusion = format!(
            "{subject} {} 个文件（{line_summary}）：{names}{}",
            work_unit.change_set.files.len(),
            if work_unit.change_set.files.len() > 3 {
                " 等"
            } else {
                ""
            }
        );
        return Some(build_insight(
            work_unit,
            InsightType::Explain,
            conclusion,
            evidence,
            format!(
                "证据范围是{}；这里只描述收口时可见状态，不作本回合精确归因。",
                work_unit.change_set.scope.description()
            ),
            "先核对文件范围，再查看风险与测试缺口。".into(),
            RiskLevel::Info,
            match work_unit.change_set.scope {
                ChangeScope::WorktreeAtTurnEnd => 0.72,
                ChangeScope::PartialWorktreeAtTurnEnd => 0.5,
                ChangeScope::AgentEvents => 0.55,
                ChangeScope::Unavailable => 0.35,
            },
        ));
    }

    let event = source_events.iter().rev().find(|event| {
        let intent = event.excerpt.intent.trim();
        (event.emitter == "codex-notify" || event.event_type == "message.updated")
            && !intent.is_empty()
            && intent != "回合结束"
            && intent != "本轮处理结束"
            && intent != "Codex 回合结束"
            && !intent.starts_with("会话结束(")
    })?;
    let excerpt = truncate_chars(
        if event.excerpt.intent.trim().is_empty() {
            &event.excerpt.diff_head
        } else {
            &event.excerpt.intent
        },
        220,
    );
    Some(build_insight(
        work_unit,
        InsightType::Explain,
        "本回合已结束，但当前 Git 工作区没有可定位的变更；仅保留 Agent 事件摘要。".into(),
        vec![Evidence {
            kind: EvidenceKind::Event,
            reference: event.id.clone(),
            excerpt,
        }],
        "这条结论只能证明 Agent 报告了什么，不能证明具体代码改动。".into(),
        "如本回合包含提交，请在后续基线能力接入后再做精确归因。".into(),
        RiskLevel::Info,
        0.45,
    ))
}

fn test_gap(work_unit: &WorkUnit) -> Option<Insight> {
    let source_files: Vec<&FileChange> = work_unit
        .change_set
        .files
        .iter()
        .filter(|file| is_source_file(&file.path) && !is_test_file(&file.path))
        .collect();
    if source_files.is_empty() {
        return None;
    }

    if work_unit.tests.is_empty() {
        let evidence = source_files
            .iter()
            .take(5)
            .map(|file| file_evidence(file, work_unit.change_set.scope))
            .collect();
        return Some(build_insight(
            work_unit,
            InsightType::TestGap,
            format!(
                "检测到 {} 个源文件变更，但本 WorkUnit 未捕获测试命令。",
                source_files.len()
            ),
            evidence,
            "没有测试事件不等于测试未运行，但当前证据链无法证明验证结果。".into(),
            "运行与改动相关的最小测试集，并让适配器记录测试结果。".into(),
            RiskLevel::P2,
            0.9,
        ));
    }

    if work_unit
        .tests
        .iter()
        .any(|test| test.status == TestStatus::Unknown)
    {
        let evidence = work_unit
            .tests
            .iter()
            .filter(|test| test.status == TestStatus::Unknown)
            .take(5)
            .map(|test| Evidence {
                kind: EvidenceKind::Test,
                reference: test.evidence.clone(),
                excerpt: test.name.clone(),
            })
            .collect();
        return Some(build_insight(
            work_unit,
            InsightType::TestGap,
            "捕获到测试命令，但 kvba.event.v1 没有退出状态，测试结果仍未验证。".into(),
            evidence,
            "工具调用完成不能作为测试通过的证据。".into(),
            "扩展事件协议记录退出状态，或人工核对本次测试输出。".into(),
            RiskLevel::P2,
            0.98,
        ));
    }
    None
}

fn event_risk(work_unit: &WorkUnit, events: &[NormalizedEvent]) -> Option<Insight> {
    let flagged: Vec<&NormalizedEvent> = events
        .iter()
        .filter(|event| {
            event.excerpt.has_api_call
                || event.excerpt.has_error_path
                || event.excerpt.todo_or_fixme
        })
        .collect();
    if flagged.is_empty() {
        return None;
    }
    let mut signals = Vec::new();
    if flagged.iter().any(|event| event.excerpt.has_api_call) {
        signals.push("外部 API");
    }
    if flagged.iter().any(|event| event.excerpt.has_error_path) {
        signals.push("异常路径");
    }
    if flagged.iter().any(|event| event.excerpt.todo_or_fixme) {
        signals.push("TODO/FIXME");
    }
    let evidence = flagged
        .iter()
        .take(5)
        .map(|event| Evidence {
            kind: EvidenceKind::Event,
            reference: event.id.clone(),
            excerpt: truncate_chars(
                if event.excerpt.intent.is_empty() {
                    &event.excerpt.diff_head
                } else {
                    &event.excerpt.intent
                },
                180,
            ),
        })
        .collect();
    Some(build_insight(
        work_unit,
        InsightType::Risk,
        format!(
            "Agent 事件包含 {} 信号，需要核对对应实现。",
            signals.join("、")
        ),
        evidence,
        "这些是适配器的文本信号，不等同于已经确认的代码缺陷。".into(),
        "定位证据事件对应的文件，检查失败路径与遗留标记。".into(),
        RiskLevel::P2,
        0.68,
    ))
}

fn configuration_decision(work_unit: &WorkUnit) -> Option<Insight> {
    let files: Vec<&FileChange> = work_unit
        .change_set
        .files
        .iter()
        .filter(|file| is_decision_file(&file.path))
        .collect();
    if files.is_empty() {
        return None;
    }
    let evidence = files
        .iter()
        .take(5)
        .map(|file| file_evidence(file, work_unit.change_set.scope))
        .collect();
    Some(build_insight(
        work_unit,
        InsightType::Decision,
        "依赖、构建或运行配置存在变更，需要确认是否接受为项目级选择。".into(),
        evidence,
        "配置类改动通常会影响后续 Agent 与发布环境，不能只当普通文件编辑。".into(),
        "确认变更意图，并把长期约束写入后续 ProjectModel。".into(),
        RiskLevel::P2,
        0.82,
    ))
}

#[allow(clippy::too_many_arguments)]
fn build_insight(
    work_unit: &WorkUnit,
    kind: InsightType,
    conclusion: String,
    evidence: Vec<Evidence>,
    importance: String,
    suggested_action: String,
    risk_level: RiskLevel,
    confidence: f32,
) -> Insight {
    debug_assert!(!evidence.is_empty());
    let refs = evidence
        .iter()
        .map(|item| item.reference.as_str())
        .collect::<Vec<_>>()
        .join("|");
    let seed = format!("{}|{}|{}", work_unit.id, kind.as_str(), refs);
    Insight {
        id: format!("ins_{}", &blake3::hash(seed.as_bytes()).to_hex()[..16]),
        work_unit_id: work_unit.id.clone(),
        kind,
        conclusion: truncate_chars(&conclusion, 240),
        evidence,
        importance,
        suggested_action,
        risk_level,
        confidence: confidence.clamp(0.0, 1.0),
    }
}

fn file_evidence(file: &FileChange, scope: ChangeScope) -> Evidence {
    let change = if file.additions == 0 && file.deletions == 0 {
        "变更行数未采集".to_string()
    } else {
        format!("+{}/-{}", file.additions, file.deletions)
    };
    Evidence {
        kind: EvidenceKind::File,
        reference: file.path.clone(),
        excerpt: format!("{}：{change}", scope.description()),
    }
}

fn is_source_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [
        ".rs", ".ts", ".tsx", ".js", ".jsx", ".vue", ".py", ".go", ".java", ".kt", ".swift", ".cs",
        ".cpp", ".c", ".h",
    ]
    .iter()
    .any(|extension| lower.ends_with(extension))
}

fn is_test_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase().replace('\\', "/");
    lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.contains("__tests__")
        || lower.contains(".test.")
        || lower.contains(".spec.")
}

fn is_decision_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase().replace('\\', "/");
    [
        "cargo.toml",
        "cargo.lock",
        "package.json",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "pyproject.toml",
        "requirements.txt",
        "go.mod",
        "dockerfile",
        "tauri.conf.json",
    ]
    .iter()
    .any(|name| lower.ends_with(name))
        || lower.contains("/.github/workflows/")
        || lower.contains("/migrations/")
}

fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Excerpt, NormalizedEvent, SCHEMA};
    use crate::workunit::{ChangeSet, TestResult, WorkUnitMode};

    fn unit(files: &[&str], tests: bool) -> WorkUnit {
        WorkUnit {
            id: "wu_test".into(),
            project_id: "proj_test".into(),
            session_id: "session".into(),
            goal: "change parser".into(),
            mode: WorkUnitMode::Control,
            start_at: 1,
            end_at: 2,
            events: vec!["event-1".into()],
            change_set: ChangeSet {
                files: files
                    .iter()
                    .map(|path| FileChange {
                        path: path.to_string(),
                        additions: 3,
                        deletions: 1,
                        hunks: 0,
                    })
                    .collect(),
                scope: ChangeScope::WorktreeAtTurnEnd,
                ..Default::default()
            },
            tests: if tests {
                vec![TestResult {
                    name: "cargo test".into(),
                    status: TestStatus::Unknown,
                    evidence: "event-test".into(),
                }]
            } else {
                vec![]
            },
        }
    }

    fn source_event() -> NormalizedEvent {
        NormalizedEvent {
            schema: SCHEMA.into(),
            id: "event-1".into(),
            emitter: "test".into(),
            event_type: "tool.completed".into(),
            tool_name: "edit".into(),
            ts: 1,
            project_path: "C:/repo".into(),
            session_id: "session".into(),
            excerpt: Excerpt::default(),
        }
    }

    #[test]
    fn every_generated_insight_has_local_evidence() {
        let insights = analyze(
            &unit(&["src/lib.rs", "Cargo.toml"], false),
            &[source_event()],
        );
        assert!(insights.len() >= 3);
        assert!(insights.iter().all(|insight| !insight.evidence.is_empty()));
        assert!(insights
            .iter()
            .flat_map(|insight| insight.evidence.iter())
            .all(|evidence| !evidence.reference.is_empty()));
    }

    #[test]
    fn missing_test_event_becomes_scoped_test_gap() {
        let insights = analyze(&unit(&["src/lib.rs"], false), &[source_event()]);
        let gap = insights
            .iter()
            .find(|insight| insight.kind == InsightType::TestGap)
            .unwrap();
        assert!(gap.conclusion.contains("未捕获测试命令"));
        assert_eq!(gap.risk_level, RiskLevel::P2);
    }

    #[test]
    fn manifest_change_becomes_confirmation_candidate() {
        let insights = analyze(&unit(&["package.json"], false), &[source_event()]);
        assert!(insights
            .iter()
            .any(|insight| insight.kind == InsightType::Decision));
    }

    #[test]
    fn insight_contract_round_trips() {
        let insight = analyze(&unit(&["src/lib.rs"], true), &[source_event()])
            .into_iter()
            .next()
            .unwrap();
        let json = serde_json::to_string(&insight).unwrap();
        let decoded: Insight = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, insight);
        assert!(json.contains("\"type\":\"explain\""));
        assert!(json.contains("\"ref\":"));
    }

    #[test]
    fn primary_prefers_actionable_insight_over_explanation() {
        let insights = analyze(
            &unit(&["src/lib.rs", "Cargo.toml"], false),
            &[source_event()],
        );
        assert_eq!(primary(&insights).unwrap().kind, InsightType::Decision);
    }
}
