//! 归一化事件 Schema + 摄入 + 滚动落盘(每项目 events.log JSONL,最近 200 条)

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::state::DIALOG_WINDOW;

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

/// **emitter 无关**的内容指纹:project|eventType|tool|intent|文件名|事件秒。
/// 刻意不含 id / emitter / session_id / diff 正文 —— bridge 与 watcher 同开时,
/// 同一动作两边给出的这些字段各不相同(或一边为空),只有"哪个项目、什么动作、
/// 碰了哪些文件、发生在哪一秒"是双方都能对上的稳定事实。
/// 文件名只取 basename,避免一边给绝对路径、一边给相对路径。
pub fn content_fingerprint(e: &NormalizedEvent) -> String {
    let mut files: Vec<&str> = e.excerpt.files.iter().map(|f| base_name(f)).collect();
    files.sort_unstable();
    files.dedup();
    let seed = format!(
        "{}|{}|{}|{}|{}|{}",
        e.project_path,
        e.event_type,
        e.tool_name,
        e.excerpt.intent,
        files.join(","),
        e.ts
    );
    blake3::hash(seed.as_bytes()).to_hex()[..16].to_string()
}

fn base_name(p: &str) -> &str {
    p.rsplit(['/', '\\']).next().unwrap_or(p)
}

/// 内容判重缓冲的保鲜期(秒)与硬上限
pub const CONTENT_DEDUP_SECS: i64 = 5;
pub const CONTENT_DEDUP_MAX: usize = 512;

/// 短期判重:命中返回 true;未命中则登记。
/// 纯函数(不依赖 AppState),事件到达顺序可能轻微乱序,故按时间窗整体清理。
pub fn seen_or_record(buf: &mut VecDeque<(i64, String)>, ts: i64, fp: &str) -> bool {
    buf.retain(|(t, _)| *t >= ts - CONTENT_DEDUP_SECS);
    if buf.iter().any(|(_, f)| f == fp) {
        return true;
    }
    while buf.len() >= CONTENT_DEDUP_MAX {
        buf.pop_front();
    }
    buf.push_back((ts, fp.to_string()));
    false
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// 事件存储:内存窗口 + JSONL 滚动落盘(无实例状态,全部为关联函数)
pub struct EventStore;

impl EventStore {
    /// 摄入事件:清洗 → 内容判重 → 入内存窗口 → 滚动落盘。
    /// 返回 `None` 表示与 5s 内已摄入的事件内容重复(被静默丢弃,调用方仍应 ACK 200)。
    pub fn ingest(
        state: &std::sync::Arc<crate::state::AppState>,
        ev: NormalizedEvent,
    ) -> anyhow::Result<Option<NormalizedEvent>> {
        let ev = ev.sanitize().map_err(anyhow::Error::msg)?;
        let key = crate::state::project_key(&ev.project_path);
        let fp = content_fingerprint(&ev);
        {
            let mut core = state.core.blocking_lock();
            if seen_or_record(&mut core.recent_event_fp, ev.ts, &fp) {
                log::debug!("忽略重复事件(指纹={fp}):emitter={}", ev.emitter);
                return Ok(None);
            }
            core.active_project = Some(key.clone());
            core.last_event_in = Some(crate::state::now_ts());
            core.events_ingested += 1;
            let live = core.projects.entry(key).or_default();

            // 对话窗口:最近 DIALOG_WINDOW 条原始 JSON 截断副本,纯滚动替换(零 LLM 成本)
            let raw: String = serde_json::to_string(&ev)
                .unwrap_or_default()
                .chars()
                .take(crate::state::DIALOG_MAX_CHARS)
                .collect();
            live.dialog.push_back(raw);
            while live.dialog.len() > DIALOG_WINDOW {
                live.dialog.pop_front();
            }

            // 事件窗口(ROLLING_LIMIT 条)
            live.events.push_back(ev.clone());
            while live.events.len() > ROLLING_LIMIT {
                live.events.pop_front();
            }
        }

        // 落盘失败不影响内存窗口(下次摄入会整体重写)
        let dir = state.ensure_project_dir(&ev.project_path);
        let _ = Self::persist_rolling(&dir, &ev.project_path, state);
        Ok(Some(ev))
    }

    /// 整体重写 events.log(JSONL,最多 ROLLING_LIMIT 条)
    fn persist_rolling(
        dir: &std::path::Path,
        project_path: &str,
        state: &std::sync::Arc<crate::state::AppState>,
    ) -> anyhow::Result<()> {
        let key = crate::state::project_key(project_path);
        let text = {
            let core = state.core.blocking_lock();
            let mut text = String::new();
            if let Some(live) = core.projects.get(&key) {
                for e in live.events.iter() {
                    if let Ok(line) = serde_json::to_string(e) {
                        text.push_str(&line);
                        text.push('\n');
                    }
                }
            }
            text
        };
        std::fs::write(dir.join("events.log"), text)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(
        emitter: &str,
        id: &str,
        tool: &str,
        intent: &str,
        files: &[&str],
        ts: i64,
    ) -> NormalizedEvent {
        NormalizedEvent {
            schema: SCHEMA.into(),
            id: id.into(),
            emitter: emitter.into(),
            event_type: "tool.completed".into(),
            tool_name: tool.into(),
            ts,
            project_path: "C:/repo/app".into(),
            session_id: format!("sess-{emitter}"),
            excerpt: Excerpt {
                files: files.iter().map(|s| s.to_string()).collect(),
                diff_head: format!("diff-of-{emitter}"),
                intent: intent.into(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn content_fingerprint_is_emitter_agnostic() {
        // 同一动作被 bridge 与 watcher 各报一遍:id / emitter / session / diff 全不同
        let a = ev(
            "bridge",
            "id-1",
            "edit",
            "改登录",
            &["C:/repo/app/src/a.ts"],
            1000,
        );
        let b = ev(
            "watcher",
            "id-2",
            "edit",
            "改登录",
            &["C:/repo/app/src/a.ts"],
            1000,
        );
        assert_eq!(content_fingerprint(&a), content_fingerprint(&b));
    }

    #[test]
    fn path_style_and_file_order_do_not_change_fingerprint() {
        let a = ev(
            "bridge",
            "1",
            "edit",
            "",
            &["C:/repo/app/src/a.ts", "src/b.ts"],
            1000,
        );
        let b = ev("watcher", "2", "edit", "", &["b.ts", "app/src/a.ts"], 1000);
        assert_eq!(content_fingerprint(&a), content_fingerprint(&b));
    }

    #[test]
    fn different_files_or_intents_are_not_duplicates() {
        let base = ev("bridge", "1", "edit", "改登录", &["src/a.ts"], 1000);
        let other_file = ev("bridge", "2", "edit", "改登录", &["src/z.ts"], 1000);
        let other_intent = ev("bridge", "3", "edit", "改支付", &["src/a.ts"], 1000);
        let other_tool = ev("bridge", "4", "read", "改登录", &["src/a.ts"], 1000);
        let fp = content_fingerprint(&base);
        for other in [&other_file, &other_intent, &other_tool] {
            assert_ne!(fp, content_fingerprint(other));
        }
    }

    #[test]
    fn seen_or_record_dedups_within_window_then_forgets() {
        let mut buf = VecDeque::new();
        let fp = content_fingerprint(&ev("bridge", "1", "edit", "x", &["a.ts"], 1000));
        assert!(!seen_or_record(&mut buf, 1000, &fp));
        assert!(seen_or_record(&mut buf, 1000, &fp), "同一秒内应判重");
        assert!(
            seen_or_record(&mut buf, 1000 + CONTENT_DEDUP_SECS, &fp),
            "保鲜期边界内仍判重"
        );
        assert!(
            !seen_or_record(&mut buf, 1000 + CONTENT_DEDUP_SECS + 1, &fp),
            "超窗后应重新放行"
        );
        assert_eq!(buf.len(), 1, "过期条目应被清掉");
    }
}
