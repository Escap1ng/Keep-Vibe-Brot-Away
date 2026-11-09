//! WorkUnit domain model and turn-boundary aggregation.
//!
//! The normalized event log remains the ingestion protocol. This module raises
//! those low-level events into one complete Agent turn.

pub mod git;

use std::collections::{BTreeMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::events::NormalizedEvent;

pub const MAX_WORKUNIT_EVENTS: usize = 200;
pub const WORKUNIT_KEEP: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WorkUnitMode {
    #[default]
    Control,
    Learning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ChangeScope {
    WorktreeAtTurnEnd,
    PartialWorktreeAtTurnEnd,
    AgentEvents,
    #[default]
    Unavailable,
}

impl ChangeScope {
    pub fn description(self) -> &'static str {
        match self {
            Self::WorktreeAtTurnEnd => "回合结束时工作区相对 HEAD 的快照",
            Self::PartialWorktreeAtTurnEnd => "未完整采集的工作区快照",
            Self::AgentEvents => "Agent 事件声明的相关文件",
            Self::Unavailable => "无可用变更证据",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct FileChange {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
    pub hunks: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ChangeSet {
    pub base_ref: Option<String>,
    pub head_ref: Option<String>,
    pub files: Vec<FileChange>,
    pub scope: ChangeScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TestStatus {
    Passed,
    Failed,
    Skipped,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct TestResult {
    pub name: String,
    pub status: TestStatus,
    pub evidence: String,
}

/// 一次命令执行的独立记录，按执行 ID 去重（不是按命令文本）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ExecutionRecord {
    pub id: String,
    pub command: String,
    pub cwd: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub exit_code: Option<i32>,
    pub result_source: String,
    pub output_ref: String,
    pub output_excerpt: String,
    pub event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkUnit {
    pub id: String,
    pub project_id: String,
    pub session_id: String,
    pub goal: String,
    pub mode: WorkUnitMode,
    pub start_at: i64,
    pub end_at: i64,
    pub events: Vec<String>,
    pub change_set: ChangeSet,
    pub tests: Vec<TestResult>,
    /// v1 可选扩展；旧快照没有该字段时按空列表读取。
    #[serde(default)]
    pub executions: Vec<ExecutionRecord>,
}

#[derive(Debug, Clone)]
pub struct CompletedWorkUnit {
    pub work_unit: WorkUnit,
    pub project_path: String,
    pub boundary_event_id: String,
    pub source_events: Vec<NormalizedEvent>,
}

pub fn is_completion_boundary(event: &NormalizedEvent) -> bool {
    match event.event_type.as_str() {
        "turn.ended" => event.excerpt.intent.trim() != "子代理回合结束",
        "session.ended" => true,
        _ => false,
    }
}

fn same_session(event: &NormalizedEvent, boundary: &NormalizedEvent) -> bool {
    if boundary.session_id.is_empty() {
        event.session_id.is_empty() && event.emitter == boundary.emitter
    } else {
        event.session_id == boundary.session_id
            && crate::events::agent_family(&event.emitter)
                == crate::events::agent_family(&boundary.emitter)
    }
}

/// Close one WorkUnit at a turn/session boundary.
///
/// Ingestion order defines turn boundaries. Adapters must preserve delivery
/// order: guessing from second-level timestamps would let one event leak into
/// two adjacent WorkUnits.
pub fn aggregate_at_boundary(
    events: &VecDeque<NormalizedEvent>,
    boundary_event_id: &str,
) -> Option<CompletedWorkUnit> {
    let boundary = events.iter().find(|event| event.id == boundary_event_id)?;
    if !is_completion_boundary(boundary) {
        return None;
    }

    let candidates: Vec<NormalizedEvent> = events
        .iter()
        .filter(|event| same_session(event, boundary))
        .cloned()
        .collect();

    let boundary_index = candidates
        .iter()
        .position(|event| event.id == boundary_event_id)?;
    let start_index = candidates[..boundary_index]
        .iter()
        .rposition(is_completion_boundary)
        .map(|index| index + 1)
        .unwrap_or(0);
    let mut source_events = candidates[start_index..=boundary_index].to_vec();

    // session.ended only flushes a still-open turn. A preceding turn boundary
    // already closed the useful events, so do not create an empty duplicate.
    if boundary.event_type == "session.ended" && source_events.len() == 1 {
        return None;
    }
    if source_events.len() > MAX_WORKUNIT_EVENTS {
        source_events.drain(..source_events.len() - MAX_WORKUNIT_EVENTS);
    }

    let project_id = format!("proj_{}", crate::state::project_key(&boundary.project_path));
    let session_id = if boundary.session_id.is_empty() {
        format!("emitter:{}", boundary.emitter)
    } else {
        boundary.session_id.clone()
    };
    let id_seed = format!(
        "{project_id}|{}|{session_id}|{}",
        crate::events::agent_family(&boundary.emitter),
        boundary.id
    );
    let id = format!("wu_{}", &blake3::hash(id_seed.as_bytes()).to_hex()[..16]);
    let start_at = source_events
        .first()
        .map(|event| event.ts)
        .unwrap_or(boundary.ts);

    Some(CompletedWorkUnit {
        work_unit: WorkUnit {
            id,
            project_id,
            session_id,
            goal: infer_goal(&source_events),
            mode: WorkUnitMode::Control,
            start_at,
            end_at: boundary.ts,
            events: source_events.iter().map(|event| event.id.clone()).collect(),
            change_set: ChangeSet::default(),
            tests: extract_tests(&source_events),
            executions: extract_executions(&source_events),
        },
        project_path: boundary.project_path.clone(),
        boundary_event_id: boundary.id.clone(),
        source_events,
    })
}

fn infer_goal(events: &[NormalizedEvent]) -> String {
    let candidate = events
        .iter()
        .find(|event| {
            event.event_type == "message.updated" && !event.excerpt.intent.trim().is_empty()
        })
        .or_else(|| {
            events.iter().find(|event| {
                let intent = event.excerpt.intent.trim();
                !intent.is_empty() && !is_generic_boundary_intent(intent)
            })
        });
    candidate
        .map(|event| truncate_chars(event.excerpt.intent.trim(), 500))
        .unwrap_or_default()
}

fn is_generic_boundary_intent(intent: &str) -> bool {
    intent == "回合结束"
        || intent == "回合结束(停止钩子应答)"
        || intent == "本轮处理结束"
        || intent == "Codex 回合结束"
        || intent.starts_with("会话结束(")
}

const TEST_COMMAND_MARKERS: &[&str] = &[
    "cargo test",
    "npm test",
    "npm run test",
    "pnpm test",
    "yarn test",
    "pytest",
    "python -m unittest",
    "go test",
    "dotnet test",
    "mvn test",
    "gradle test",
];

fn is_test_command(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    TEST_COMMAND_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// 每条执行记录都保留，只按执行 ID 去重；重传同一执行不会变成两次运行。
fn extract_executions(events: &[NormalizedEvent]) -> Vec<ExecutionRecord> {
    let mut seen = HashSet::new();
    let mut records = Vec::new();
    for event in events {
        let Some(exec) = event.execution.as_ref() else {
            continue;
        };
        if exec.command.trim().is_empty() {
            continue;
        }
        let id = if exec.id.trim().is_empty() {
            event.id.clone()
        } else {
            exec.id.clone()
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        records.push(ExecutionRecord {
            id,
            command: exec.command.clone(),
            cwd: exec.cwd.clone(),
            started_at: exec.started_at,
            ended_at: exec.ended_at,
            exit_code: exec.exit_code,
            result_source: exec.result_source.clone(),
            output_ref: exec.output_ref.clone(),
            output_excerpt: exec.output_excerpt.clone(),
            event_id: event.id.clone(),
        });
    }
    records
}

fn evidence_status(exec: &crate::events::ExecutionEvidence) -> TestStatus {
    if !crate::events::RELIABLE_RESULT_SOURCES.contains(&exec.result_source.as_str()) {
        return TestStatus::Unknown;
    }
    match exec.exit_code {
        Some(0) => TestStatus::Passed,
        Some(_) => TestStatus::Failed,
        None => TestStatus::Unknown,
    }
}

fn extract_tests(events: &[NormalizedEvent]) -> Vec<TestResult> {
    let mut tests = Vec::new();
    let mut seen = HashSet::new();
    for event in events {
        if event.event_type != "tool.completed" {
            continue;
        }
        if let Some(exec) = event.execution.as_ref() {
            if exec.command.trim().is_empty() {
                continue;
            }
            if !seen.insert(exec.id.clone()) {
                continue;
            }
            if !is_test_command(&exec.command) {
                continue;
            }
            tests.push(TestResult {
                name: truncate_chars(exec.command.trim(), 160),
                status: evidence_status(exec),
                evidence: event.id.clone(),
            });
            continue;
        }

        // 旧协议事件没有执行证据：每条事件算一次运行，结果保持 Unknown。
        let text = format!("{} {}", event.excerpt.intent, event.excerpt.diff_head);
        if !is_test_command(&text) {
            continue;
        }
        let name = truncate_chars(event.excerpt.intent.trim(), 160);
        let name = if name.is_empty() {
            format!("{} test command", event.tool_name)
        } else {
            name
        };
        tests.push(TestResult {
            name,
            status: TestStatus::Unknown,
            evidence: event.id.clone(),
        });
    }
    tests
}

pub fn changes_from_events(events: &[NormalizedEvent], git_error: Option<String>) -> ChangeSet {
    let mut files = BTreeMap::<String, FileChange>::new();
    for event in events {
        if !crate::gate::is_write_event(event) {
            continue;
        }
        for path in &event.excerpt.files {
            let path = path.trim();
            if path.is_empty() {
                continue;
            }
            files.entry(path.to_string()).or_insert_with(|| FileChange {
                path: path.to_string(),
                ..Default::default()
            });
        }
    }
    let files: Vec<FileChange> = files.into_values().collect();
    ChangeSet {
        scope: if files.is_empty() {
            ChangeScope::Unavailable
        } else {
            ChangeScope::AgentEvents
        },
        files,
        collection_error: git_error,
        ..Default::default()
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Excerpt, SCHEMA};

    fn event(id: &str, event_type: &str, session: &str, ts: i64, intent: &str) -> NormalizedEvent {
        NormalizedEvent {
            schema: SCHEMA.into(),
            id: id.into(),
            emitter: "test-adapter".into(),
            event_type: event_type.into(),
            tool_name: if event_type == "tool.completed" {
                "bash".into()
            } else {
                String::new()
            },
            ts,
            project_path: "C:/repo/app".into(),
            session_id: session.into(),
            excerpt: Excerpt {
                intent: intent.into(),
                ..Default::default()
            },
            execution: None,
        }
    }

    #[test]
    fn closes_only_current_session_since_previous_turn() {
        let events = VecDeque::from([
            event("a1", "tool.completed", "a", 1, "edit old"),
            event("a-end-1", "turn.ended", "a", 2, "回合结束"),
            event("b1", "tool.completed", "b", 3, "other session"),
            event("a2", "tool.completed", "a", 4, "edit new"),
            event("a-end-2", "turn.ended", "a", 5, "回合结束"),
        ]);
        let completed = aggregate_at_boundary(&events, "a-end-2").unwrap();
        assert_eq!(completed.work_unit.events, vec!["a2", "a-end-2"]);
        assert_eq!(completed.work_unit.goal, "edit new");
    }

    #[test]
    fn same_second_tool_event_before_boundary_is_included() {
        let events = VecDeque::from([
            event("late-tool", "tool.completed", "a", 10, "cargo test"),
            event("end", "turn.ended", "a", 10, "回合结束"),
        ]);
        let completed = aggregate_at_boundary(&events, "end").unwrap();
        assert_eq!(completed.work_unit.events, vec!["late-tool", "end"]);
        assert_eq!(completed.work_unit.tests.len(), 1);
        assert_eq!(completed.work_unit.tests[0].status, TestStatus::Unknown);
    }

    #[test]
    fn later_same_second_boundary_does_not_steal_previous_turn() {
        let events = VecDeque::from([
            event("tool-1", "tool.completed", "a", 10, "first edit"),
            event("end-1", "turn.ended", "a", 10, "回合结束"),
            event("tool-2", "tool.completed", "a", 10, "second edit"),
            event("end-2", "turn.ended", "a", 10, "回合结束"),
        ]);
        let first = aggregate_at_boundary(&events, "end-1").unwrap();
        let second = aggregate_at_boundary(&events, "end-2").unwrap();
        assert_eq!(first.work_unit.events, vec!["tool-1", "end-1"]);
        assert_eq!(second.work_unit.events, vec!["tool-2", "end-2"]);
    }

    #[test]
    fn codex_boundary_can_form_single_event_unit() {
        let events = VecDeque::from([event(
            "codex-end",
            "turn.ended",
            "thread-1",
            10,
            "Codex 收尾:implemented parser",
        )]);
        let completed = aggregate_at_boundary(&events, "codex-end").unwrap();
        assert_eq!(completed.work_unit.events, vec!["codex-end"]);
        assert!(completed.work_unit.goal.contains("implemented parser"));
    }

    #[test]
    fn subagent_stop_does_not_close_main_turn() {
        let subagent = event("sub", "turn.ended", "a", 2, "子代理回合结束");
        assert!(!is_completion_boundary(&subagent));
        let events = VecDeque::from([
            event("tool", "tool.completed", "a", 1, "edit"),
            subagent,
            event("end", "turn.ended", "a", 3, "回合结束"),
        ]);
        let completed = aggregate_at_boundary(&events, "end").unwrap();
        assert_eq!(completed.work_unit.events.len(), 3);
    }

    #[test]
    fn session_end_after_turn_does_not_create_empty_duplicate() {
        let events = VecDeque::from([
            event("tool", "tool.completed", "a", 1, "edit"),
            event("turn", "turn.ended", "a", 2, "回合结束"),
            event("session", "session.ended", "a", 3, "会话结束(other)"),
        ]);
        assert!(aggregate_at_boundary(&events, "session").is_none());
    }

    #[test]
    fn workunit_id_is_deterministic_for_boundary() {
        let events = VecDeque::from([event("end", "turn.ended", "a", 1, "done")]);
        let first = aggregate_at_boundary(&events, "end").unwrap();
        let second = aggregate_at_boundary(&events, "end").unwrap();
        assert_eq!(first.work_unit.id, second.work_unit.id);
    }

    #[test]
    fn event_fallback_only_claims_write_files() {
        let mut read = event("read", "tool.completed", "a", 1, "inspect");
        read.tool_name = "read".into();
        read.excerpt.files = vec!["src/read-only.rs".into()];
        let mut edit = event("edit", "tool.completed", "a", 2, "change");
        edit.tool_name = "edit".into();
        edit.excerpt.files = vec!["src/changed.rs".into()];
        let changes = changes_from_events(&[read, edit], None);
        assert_eq!(changes.scope, ChangeScope::AgentEvents);
        assert_eq!(changes.files.len(), 1);
        assert_eq!(changes.files[0].path, "src/changed.rs");
    }

    #[test]
    fn same_session_id_from_different_agents_does_not_mix() {
        let mut claude = event("claude", "tool.completed", "shared", 1, "edit");
        claude.emitter = "claude-code-bridge".into();
        let mut opencode = event("opencode", "tool.completed", "shared", 2, "edit");
        opencode.emitter = "opencode-plugin".into();
        let mut boundary = event("end", "turn.ended", "shared", 3, "回合结束");
        boundary.emitter = "opencode-plugin".into();
        let completed =
            aggregate_at_boundary(&VecDeque::from([claude, opencode, boundary]), "end").unwrap();
        assert_eq!(completed.work_unit.events, vec!["opencode", "end"]);
    }

    fn exec_event(
        id: &str,
        session: &str,
        ts: i64,
        command: &str,
        exec_id: &str,
        exit_code: Option<i32>,
        source: &str,
    ) -> NormalizedEvent {
        let mut event = event(id, "tool.completed", session, ts, &format!("$ {command}"));
        event.execution = Some(crate::events::ExecutionEvidence {
            id: exec_id.into(),
            command: command.into(),
            cwd: "C:/repo/app".into(),
            started_at: ts,
            ended_at: ts,
            exit_code,
            result_source: source.into(),
            output_ref: format!("transcript#{exec_id}"),
            output_excerpt: "test output".into(),
        });
        event
    }

    #[test]
    fn keeps_every_execution_and_derives_status_from_exit_code() {
        let events = VecDeque::from([
            exec_event("e1", "a", 10, "cargo test", "call-1", Some(1), "transcript"),
            exec_event("e2", "a", 11, "cargo test", "call-2", Some(0), "transcript"),
            event("end", "turn.ended", "a", 12, "回合结束"),
        ]);
        let completed = aggregate_at_boundary(&events, "end").unwrap();
        assert_eq!(completed.work_unit.executions.len(), 2);
        assert_eq!(completed.work_unit.tests.len(), 2);
        assert_eq!(completed.work_unit.tests[0].status, TestStatus::Failed);
        assert_eq!(completed.work_unit.tests[1].status, TestStatus::Passed);
        assert_eq!(completed.work_unit.tests[0].evidence, "e1");
        assert_eq!(
            completed.work_unit.executions[0].output_ref,
            "transcript#call-1"
        );
    }

    #[test]
    fn duplicate_execution_id_is_deduped() {
        let events = VecDeque::from([
            exec_event("e1", "a", 10, "cargo test", "call-1", Some(1), "transcript"),
            exec_event(
                "e1-retry",
                "a",
                11,
                "cargo test",
                "call-1",
                Some(1),
                "transcript",
            ),
            event("end", "turn.ended", "a", 12, "回合结束"),
        ]);
        let completed = aggregate_at_boundary(&events, "end").unwrap();
        assert_eq!(completed.work_unit.executions.len(), 1);
        assert_eq!(completed.work_unit.tests.len(), 1);
    }

    #[test]
    fn unreliable_source_never_claims_a_result() {
        let events = VecDeque::from([
            exec_event("e1", "a", 10, "cargo test", "call-1", Some(0), "unknown"),
            event("end", "turn.ended", "a", 11, "回合结束"),
        ]);
        let completed = aggregate_at_boundary(&events, "end").unwrap();
        assert_eq!(completed.work_unit.executions.len(), 1);
        assert_eq!(completed.work_unit.tests[0].status, TestStatus::Unknown);
    }

    #[test]
    fn legacy_event_without_execution_stays_unknown() {
        let events = VecDeque::from([
            event("t1", "tool.completed", "a", 10, "cargo test"),
            event("end", "turn.ended", "a", 11, "回合结束"),
        ]);
        let completed = aggregate_at_boundary(&events, "end").unwrap();
        assert!(completed.work_unit.executions.is_empty());
        assert_eq!(completed.work_unit.tests.len(), 1);
        assert_eq!(completed.work_unit.tests[0].status, TestStatus::Unknown);
    }

    #[test]
    fn old_workunit_json_without_executions_still_loads() {
        let json = r#"{
            "id": "wu_1", "projectId": "proj_1", "sessionId": "s1", "goal": "g",
            "mode": "control", "startAt": 1, "endAt": 2, "events": [],
            "changeSet": {"files": [], "scope": "unavailable"}, "tests": []
        }"#;
        let unit: WorkUnit = serde_json::from_str(json).unwrap();
        assert!(unit.executions.is_empty());
    }
}
