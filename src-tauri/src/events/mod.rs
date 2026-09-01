//! 归一化事件 Schema + 摄入 + 滚动落盘(每项目 events.log JSONL,最近 200 条)

use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = "kvba.event.v1";

pub const MAX_DIFF_CHARS: usize = 6000;
pub const MAX_INTENT_CHARS: usize = 500;
pub const MAX_FILES: usize = 32;
pub const ROLLING_LIMIT: usize = 200;

pub const EVENT_TYPES: &[&str] = &[
    "tool.completed",
    "tool.started",
    "turn.ended",
    "message.updated",
    "session.ended",
];

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Excerpt {
    pub files: Vec<String>,
    pub diff_head: String,
    pub intent: String,
    pub has_error_path: bool,
    pub has_api_call: bool,
    pub todo_or_fixme: bool,
    pub line_delta: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct NormalizedEvent {
    pub schema: String,
    pub id: String,
    pub emitter: String,
    pub event_type: String,
    pub tool_name: String,
    pub ts: i64,
    pub project_path: String,
    pub session_id: String,
    pub excerpt: Excerpt,
}

impl NormalizedEvent {
    /// 校验 + 截断清洗(容错:适配器可能传脏数据)
    pub fn sanitize(mut self) -> Result<Self, String> {
        if self.schema != SCHEMA {
            return Err(format!("未知 schema:{}", self.schema));
        }
        if !EVENT_TYPES.contains(&self.event_type.as_str()) {
            return Err(format!("未知 eventType:{}", self.event_type));
        }
        if self.project_path.trim().is_empty() {
            return Err("缺少 projectPath".into());
        }
        if self.ts <= 0 {
            return Err("缺少或非法时间戳".into());
        }
        // 适配器上报毫秒时间戳 → 归一化为秒(门控/队列内部统一秒)
        if self.ts > 1_000_000_000_000 {
            self.ts /= 1000;
        }
        // 字段截断
        if self.excerpt.diff_head.chars().count() > MAX_DIFF_CHARS {
            self.excerpt.diff_head = truncate_chars(&self.excerpt.diff_head, MAX_DIFF_CHARS);
        }
        if self.excerpt.intent.chars().count() > MAX_INTENT_CHARS {
            self.excerpt.intent = truncate_chars(&self.excerpt.intent, MAX_INTENT_CHARS);
        }
        if self.excerpt.files.len() > MAX_FILES {
            self.excerpt.files.truncate(MAX_FILES);
        }
        self.tool_name = self.tool_name.chars().take(64).collect::<String>();
        self.emitter = self.emitter.chars().take(48).collect();
        self.session_id = self.session_id.chars().take(96).collect();

        // id:缺失时用 blake3 确定性生成
        if self.id.trim().is_empty() {
            self.id = hash_event(&self);
        }
        Ok(self)
    }
}

/// 稳定哈希(用于去重/指纹)
pub fn hash_event(e: &NormalizedEvent) -> String {
    let seed = format!(
        "{}|{}|{}|{}|{}",
        e.emitter, e.event_type, e.ts, e.project_path, e.excerpt.intent
    );
    blake3::hash(seed.as_bytes()).to_hex()[..16].to_string()
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// 事件存储:内存镜像 + JSONL 滚动落盘
pub struct EventStore;

impl EventStore {
    pub fn new() -> Self {
        Self
    }

    /// 摄入事件:入内存窗口 + 重写滚动日志(异步封装由调用方处理)
    pub fn ingest(
        &self,
        state: &std::sync::Arc<crate::state::AppState>,
        ev: NormalizedEvent,
    ) -> anyhow::Result<NormalizedEvent> {
        let ev = ev.sanitize().map_err(|e| anyhow::anyhow!(e))?;
        let state = state.clone();
        let dir = state.ensure_project_dir(&ev.project_path);

        let key = crate::state::project_key(&ev.project_path);
        let mut core = state.core.blocking_lock();
        core.active_project = Some(key.clone());
        core.last_event_in = Some(crate::state::now_ts());
        if !core.projects.contains_key(&key) {
            core.projects.insert(key.clone(), crate::state::ProjectLive::new());
        }
        {
            let live = core.projects.get_mut(&key).expect("已插入");

            // 对话窗口:最近 3 条原始 JSON 截断副本(≤3KB),纯滚动替换(零 LLM 成本)
            let raw = serde_json::to_string(&ev).unwrap_or_default();
            let raw: String = raw.chars().take(3000).collect();
            live.dialog.push_back(raw);
            while live.dialog.len() > 3 {
                live.dialog.pop_front();
            }

            // 事件窗口(200 条)
            live.events.push_back(ev.clone());
            while live.events.len() > ROLLING_LIMIT {
                live.events.pop_front();
            }
        }
        drop(core);

        // 滚动落盘(仅最近 200 条;写入失败不影响内存)
        let _ = self.persist_rolling(&dir, &ev.project_path, state.clone());
        Ok(ev)
    }

    /// 重写 events.log(JSONL,200 条)
    fn persist_rolling(
        &self,
        dir: &std::path::Path,
        project_path: &str,
        state: std::sync::Arc<crate::state::AppState>,
    ) -> anyhow::Result<()> {
        let key = crate::state::project_key(project_path);
        let core = state.core.blocking_lock();
        let events: Vec<_> = core
            .projects
            .get(&key)
            .map(|p| p.events.iter().cloned().collect())
            .unwrap_or_default();
        drop(core);
        let mut text = String::new();
        for e in events {
            if let Ok(line) = serde_json::to_string(&e) {
                text.push_str(&line);
                text.push('\n');
            }
        }
        std::fs::write(dir.join("events.log"), text)?;
        Ok(())
    }

    /// 取项目最近事件(供 gate 与记忆用)
    pub fn recent_events(
        state: &std::sync::Arc<crate::state::AppState>,
        project_path: &str,
    ) -> Vec<NormalizedEvent> {
        let key = crate::state::project_key(project_path);
        let core = state.core.blocking_lock();
        core.projects
            .get(&key)
            .map(|p| p.events.iter().cloned().collect())
            .unwrap_or_default()
    }
}
