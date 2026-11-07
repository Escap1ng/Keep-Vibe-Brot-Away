//! Phase 1 orchestration: boundary event -> WorkUnit -> evidence -> Insight -> capsule.

use crate::events::NormalizedEvent;
use crate::insight::Insight;
use crate::state::AppState;
use crate::workunit::WorkUnit;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomaticRoute {
    WorkUnit,
    LegacyWatcher,
    AwaitBoundary,
}

pub fn automatic_route(event: &NormalizedEvent) -> AutomaticRoute {
    if crate::workunit::is_completion_boundary(event) {
        AutomaticRoute::WorkUnit
    } else if event.emitter == "claude-code-watcher" {
        // The JSONL watcher currently emits tool events only. Keep its previous
        // behavior until the adapter can provide a real turn boundary.
        AutomaticRoute::LegacyWatcher
    } else {
        AutomaticRoute::AwaitBoundary
    }
}

pub fn schedule_boundary(state: &Arc<AppState>, event: &NormalizedEvent) {
    let state = state.clone();
    let project_path = event.project_path.clone();
    let boundary_event_id = event.id.clone();
    tauri::async_runtime::spawn(async move {
        let events = {
            let core = state.core.blocking_lock();
            core.projects
                .get(&crate::state::project_key(&project_path))
                .map(|project| project.events.clone())
        };
        let Some(events) = events else {
            log::warn!("WorkUnit 收口失败:项目事件窗口不存在({project_path})");
            return;
        };
        let Some(completed) = crate::workunit::aggregate_at_boundary(&events, &boundary_event_id)
        else {
            return;
        };
        process_completed(state, completed).await;
    });
}

async fn process_completed(
    state: Arc<AppState>,
    mut completed: crate::workunit::CompletedWorkUnit,
) {
    let git_changes = crate::workunit::git::collect(&completed.project_path).await;
    completed.work_unit.change_set = if git_changes.files.is_empty() {
        let mut fallback = crate::workunit::changes_from_events(
            &completed.source_events,
            git_changes.collection_error.clone(),
        );
        if fallback.files.is_empty() {
            git_changes
        } else {
            fallback.base_ref = git_changes.base_ref;
            fallback
        }
    } else {
        git_changes
    };
    let insights = crate::insight::analyze(&completed.work_unit, &completed.source_events);

    let inserted = {
        let mut core = state.core.blocking_lock();
        let project = core.project_live(&completed.project_path);
        if project
            .work_units
            .iter()
            .any(|unit| unit.id == completed.work_unit.id)
        {
            false
        } else {
            project.work_units.push_back(completed.work_unit.clone());
            while project.work_units.len() > crate::workunit::WORKUNIT_KEEP {
                project.work_units.pop_front();
            }
            for insight in &insights {
                if !project
                    .insights
                    .iter()
                    .any(|existing| existing.id == insight.id)
                {
                    project.insights.push_back(insight.clone());
                }
            }
            while project.insights.len() > crate::insight::INSIGHT_KEEP {
                project.insights.pop_front();
            }
            true
        }
    };
    if !inserted {
        return;
    }

    persist_project_artifacts(&state, &completed.project_path).await;
    crate::state::emit(&state.app, "workunit.completed", &completed.work_unit);
    for insight in &insights {
        crate::state::emit(&state.app, "insight.new", insight);
    }

    if let Some(primary) = crate::insight::primary(&insights) {
        let outcome = crate::queue::push_insight_card(
            &state.app,
            &state,
            primary,
            completed.boundary_event_id,
            completed.project_path,
        );
        log::info!(
            "WorkUnit 已收口:id={} insights={} card={outcome:?}",
            completed.work_unit.id,
            insights.len()
        );
    } else {
        log::info!(
            "WorkUnit 已收口但无可投递证据:id={}",
            completed.work_unit.id
        );
    }
}

async fn persist_project_artifacts(state: &Arc<AppState>, project_path: &str) {
    let _persist_guard = state.artifact_persist.lock().await;
    let project_key = crate::state::project_key(project_path);
    if state.cognition_persistence_blocked(&project_key) {
        log::error!(
            "项目 {project_key} 的认知快照此前加载失败，本轮仅保留在内存；请先备份并修复 cognition.json"
        );
        return;
    }
    let (work_units, insights) = {
        let core = state.core.blocking_lock();
        let Some(project) = core.projects.get(&project_key) else {
            return;
        };
        (
            project.work_units.iter().cloned().collect::<Vec<_>>(),
            project.insights.iter().cloned().collect::<Vec<_>>(),
        )
    };
    let directory = state.project_dir(project_path);
    let snapshot =
        crate::cognition_store::CognitionSnapshot::new(project_key.clone(), work_units, insights);
    if let Err(error) = crate::cognition_store::save(&directory, &project_key, &snapshot) {
        let path = directory.join(crate::cognition_store::COGNITION_FILE);
        match &error {
            crate::cognition_store::CognitionStoreError::Durability { .. } => log::error!(
                "{} 已替换为新快照，但父目录同步失败:{error} —— 当前内容可读，断电耐久性未确认",
                path.display()
            ),
            _ => log::error!(
                "原子写入 {} 失败:{error} —— 本轮认知产物仅保留在内存",
                path.display()
            ),
        }
    }
}

#[tauri::command]
pub fn work_units_list(state: tauri::State<'_, Arc<AppState>>) -> Vec<WorkUnit> {
    let core = state.core.blocking_lock();
    let Some(key) = core.active_project.as_ref() else {
        return vec![];
    };
    core.projects
        .get(key)
        .map(|project| project.work_units.iter().rev().cloned().collect())
        .unwrap_or_default()
}

#[tauri::command]
pub fn insights_list(state: tauri::State<'_, Arc<AppState>>) -> Vec<Insight> {
    let core = state.core.blocking_lock();
    let Some(key) = core.active_project.as_ref() else {
        return vec![];
    };
    core.projects
        .get(key)
        .map(|project| project.insights.iter().rev().cloned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Excerpt, SCHEMA};

    fn event(emitter: &str, event_type: &str, intent: &str) -> NormalizedEvent {
        NormalizedEvent {
            schema: SCHEMA.into(),
            id: "event".into(),
            emitter: emitter.into(),
            event_type: event_type.into(),
            tool_name: String::new(),
            ts: 1,
            project_path: "C:/repo".into(),
            session_id: "session".into(),
            excerpt: Excerpt {
                intent: intent.into(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn automatic_pipeline_waits_for_turn_boundary() {
        assert_eq!(
            automatic_route(&event("opencode", "tool.completed", "edit")),
            AutomaticRoute::AwaitBoundary
        );
        assert_eq!(
            automatic_route(&event("opencode", "turn.ended", "回合结束")),
            AutomaticRoute::WorkUnit
        );
    }

    #[test]
    fn watcher_without_boundary_keeps_legacy_route() {
        assert_eq!(
            automatic_route(&event("claude-code-watcher", "tool.completed", "edit")),
            AutomaticRoute::LegacyWatcher
        );
    }

    #[test]
    fn subagent_stop_does_not_route_as_completed_workunit() {
        assert_eq!(
            automatic_route(&event("claude-code-bridge", "turn.ended", "子代理回合结束")),
            AutomaticRoute::AwaitBoundary
        );
    }
}
