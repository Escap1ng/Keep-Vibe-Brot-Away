//! 归一化事件 Schema + 摄入 + 滚动落盘(每项目 events.log JSONL,最近 200 条)

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::state::DIALOG_WINDOW;

pub const SCHEMA: &str = "kvba.event.v1";

pub const MAX_DIFF_CHARS: usize = 6000;
pub const MAX_INTENT_CHARS: usize = 500;
pub const MAX_FILES: usize = 32;
pub const ROLLING_LIMIT: usize = 200;

/// 将同一 Agent 的不同适配器实现归为一个来源，同时隔离不同 Agent 恰好同名的会话。
pub fn agent_family(emitter: &str) -> &str {
    if emitter.starts_with("claude-code") {
        "claude-code"
    } else if emitter.starts_with("opencode") {
        "opencode"
    } else if emitter.starts_with("codex") {
        "codex"
    } else if emitter.starts_with("qoder") {
        "qoder"
    } else {
        emitter
    }
}

pub const EVENT_TYPES: &[&str] = &[
    "tool.completed",
    "tool.started",
    "turn.ended",
    "message.updated",
    "session.ended",
];

pub const MAX_COMMAND_CHARS: usize = 2000;
pub const MAX_OUTPUT_CHARS: usize = 2000;
pub const MAX_OUTPUT_REF_CHARS: usize = 1024;

/// 只有这两种来源的 exit_code 才能推出测试结果;`unknown` 一律保持 Unknown。
pub const RELIABLE_RESULT_SOURCES: &[&str] = &["transcript", "adapter"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
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

/// 一次命令执行的独立证据。`kvba.event.v1` 的可选扩展:旧事件没有该字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ExecutionEvidence {
    /// 幂等键:适配器提供的执行/工具调用 ID;缺失时由服务端生成。
    pub id: String,
    pub command: String,
    pub cwd: String,
    /// 秒级时间戳;拿不到开始时间时等于结束时间。
    pub started_at: i64,
    pub ended_at: i64,
    pub exit_code: Option<i32>,
    /// transcript / adapter / unknown。unknown 不得携带 exit_code。
    pub result_source: String,
    /// 输出证据定位符,例如 `<transcript>#<tool_use_id>`。
    pub output_ref: String,
    pub output_excerpt: String,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionEvidence>,
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

        if let Some(mut exec) = self.execution.take() {
            let fallback_id = hash_event(&self);
            exec.command = truncate_chars(exec.command.trim(), MAX_COMMAND_CHARS);
            exec.cwd = exec.cwd.trim().chars().take(512).collect();
            exec.output_ref = exec.output_ref.chars().take(MAX_OUTPUT_REF_CHARS).collect();
            exec.output_excerpt = truncate_chars(&exec.output_excerpt, MAX_OUTPUT_CHARS);
            exec.started_at = normalize_seconds(exec.started_at);
            exec.ended_at = normalize_seconds(exec.ended_at);
            if exec.ended_at <= 0 {
                exec.ended_at = self.ts;
            }
            if exec.started_at <= 0 {
                exec.started_at = exec.ended_at;
            }
            if !RELIABLE_RESULT_SOURCES.contains(&exec.result_source.as_str()) {
                exec.result_source = "unknown".into();
            }
            // 没有可靠来源就不能携带退出码,否则会把 Unknown 伪装成已判定
            if exec.result_source == "unknown" {
                exec.exit_code = None;
            }
            if exec.id.trim().is_empty() {
                exec.id = format!("exec-{fallback_id}");
            } else {
                exec.id = exec.id.chars().take(96).collect();
            }
            // 没有命令文本不算一次命令执行
            if !exec.command.is_empty() {
                self.execution = Some(exec);
            }
        }

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
        "{}|{}|{}|{}|{}|{}",
        e.emitter, e.event_type, e.ts, e.project_path, e.session_id, e.excerpt.intent
    );
    blake3::hash(seed.as_bytes()).to_hex()[..16].to_string()
}

/// **emitter 无关**的内容指纹:project|eventType|tool|intent|文件名|事件秒|执行ID。
/// 刻意不含 id / emitter / diff 正文，但包含规范化 Agent 家族与 session：同一
/// Claude 会话的 bridge/hook/watcher 仍可合并，不同 Agent 或并发会话不会互相
/// 吞掉工具与测试证据。session 缺失时只能退化到 Agent 家族；边界再使用精确
/// emitter，避免两个无 session 的适配器互相吞掉收口信号。
/// 文件名只取 basename,避免一边给绝对路径、一边给相对路径。
/// 有执行 ID 时把它纳入指纹：同一命令的两次真实执行不再被 5 秒窗口误判为重复，
/// 同一执行的重传仍然合并。
pub fn content_fingerprint(e: &NormalizedEvent) -> String {
    let mut files: Vec<&str> = e.excerpt.files.iter().map(|f| base_name(f)).collect();
    files.sort_unstable();
    files.dedup();
    let event_identity = if e.session_id.is_empty() {
        agent_family(&e.emitter).to_string()
    } else {
        format!("{}|{}", agent_family(&e.emitter), e.session_id)
    };
    let boundary_identity = if matches!(e.event_type.as_str(), "turn.ended" | "session.ended") {
        if e.session_id.is_empty() {
            e.emitter.clone()
        } else {
            format!("{}|{}", agent_family(&e.emitter), e.session_id)
        }
    } else {
        String::new()
    };
    let execution_identity = e
        .execution
        .as_ref()
        .map(|exec| exec.id.as_str())
        .unwrap_or("");
    // 有执行 ID 时不再用事件秒区分：同一执行的重传可能带不同时间戳，仍应合并。
    let timestamp_identity = if execution_identity.is_empty() {
        e.ts.to_string()
    } else {
        String::new()
    };
    let seed = format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}",
        e.project_path,
        e.event_type,
        e.tool_name,
        e.excerpt.intent,
        files.join(","),
        timestamp_identity,
        event_identity,
        boundary_identity,
        execution_identity
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

fn normalize_seconds(ts: i64) -> i64 {
    if ts > 1_000_000_000_000 {
        ts / 1000
    } else {
        ts
    }
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
            execution: None,
        }
    }

    #[test]
    fn qoder_hook_is_its_own_agent_family() {
        assert_eq!(agent_family("qoder-hook"), "qoder");
        assert_ne!(agent_family("qoder-hook"), agent_family("claude-code-hook"));
    }

    #[test]
    fn content_fingerprint_is_emitter_agnostic() {
        // 同一会话动作被 bridge 与 watcher 各报一遍:id / emitter / diff 不同
        let a = ev(
            "claude-code-bridge",
            "id-1",
            "edit",
            "改登录",
            &["C:/repo/app/src/a.ts"],
            1000,
        );
        let mut b = ev(
            "claude-code-watcher",
            "id-2",
            "edit",
            "改登录",
            &["C:/repo/app/src/a.ts"],
            1000,
        );
        b.session_id = a.session_id.clone();
        assert_eq!(content_fingerprint(&a), content_fingerprint(&b));
    }

    #[test]
    fn path_style_and_file_order_do_not_change_fingerprint() {
        let a = ev(
            "claude-code-bridge",
            "1",
            "edit",
            "",
            &["C:/repo/app/src/a.ts", "src/b.ts"],
            1000,
        );
        let mut b = ev(
            "claude-code-watcher",
            "2",
            "edit",
            "",
            &["b.ts", "app/src/a.ts"],
            1000,
        );
        b.session_id = a.session_id.clone();
        assert_eq!(content_fingerprint(&a), content_fingerprint(&b));
    }

    #[test]
    fn tool_fingerprint_separates_agents_and_sessions() {
        let mut first = ev("claude-code-bridge", "1", "bash", "$ cargo test", &[], 1000);
        first.session_id = "session-a".into();
        let mut other_session = first.clone();
        other_session.session_id = "session-b".into();
        assert_ne!(
            content_fingerprint(&first),
            content_fingerprint(&other_session)
        );

        let mut other_agent = first.clone();
        other_agent.emitter = "opencode-plugin".into();
        assert_ne!(
            content_fingerprint(&first),
            content_fingerprint(&other_agent)
        );
    }

    #[test]
    fn boundary_fingerprint_keeps_concurrent_sessions_separate() {
        let mut first = ev("claude-code-bridge", "1", "", "回合结束", &[], 1000);
        first.event_type = "turn.ended".into();
        first.session_id = "session-a".into();
        let mut second = first.clone();
        second.id = "2".into();
        second.emitter = "claude-code-hook".into();
        second.session_id = "session-b".into();
        assert_ne!(content_fingerprint(&first), content_fingerprint(&second));

        second.session_id = first.session_id.clone();
        assert_eq!(content_fingerprint(&first), content_fingerprint(&second));

        first.session_id.clear();
        second.session_id.clear();
        assert_ne!(content_fingerprint(&first), content_fingerprint(&second));

        second.session_id = "session-a".into();
        first.session_id = "session-a".into();
        second.emitter = "opencode-plugin".into();
        assert_ne!(content_fingerprint(&first), content_fingerprint(&second));
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

    #[test]
    fn content_fingerprint_separates_execution_ids() {
        let mut first = ev("bridge", "1", "bash", "cargo test", &[], 1000);
        first.execution = Some(ExecutionEvidence {
            id: "call-1".into(),
            command: "cargo test".into(),
            ..Default::default()
        });
        let retransmitted = first.clone();
        let mut retransmitted_late = first.clone();
        retransmitted_late.ts = 1001;
        let mut second_run = first.clone();
        second_run.execution = Some(ExecutionEvidence {
            id: "call-2".into(),
            command: "cargo test".into(),
            ..Default::default()
        });
        assert_eq!(
            content_fingerprint(&first),
            content_fingerprint(&retransmitted),
            "同一执行的重传应合并"
        );
        assert_eq!(
            content_fingerprint(&first),
            content_fingerprint(&retransmitted_late),
            "重传时间戳不同也应合并"
        );
        assert_ne!(
            content_fingerprint(&first),
            content_fingerprint(&second_run),
            "不同执行 ID 不应被当成重复"
        );
    }

    #[test]
    fn execution_from_unreliable_source_drops_exit_code() {
        let mut event = ev("bridge", "1", "bash", "cargo test", &[], 1000);
        event.execution = Some(ExecutionEvidence {
            id: "call-1".into(),
            command: "cargo test".into(),
            exit_code: Some(1),
            result_source: "guess".into(),
            ..Default::default()
        });
        let exec = event.sanitize().unwrap().execution.unwrap();
        assert_eq!(exec.result_source, "unknown");
        assert_eq!(exec.exit_code, None);
        assert_eq!(exec.id, "call-1");
    }

    #[test]
    fn execution_without_command_is_dropped() {
        let mut event = ev("bridge", "1", "bash", "cargo test", &[], 1000);
        event.execution = Some(ExecutionEvidence {
            id: "call-1".into(),
            command: "  ".into(),
            exit_code: Some(0),
            result_source: "transcript".into(),
            ..Default::default()
        });
        assert!(event.sanitize().unwrap().execution.is_none());
    }

    #[test]
    fn execution_timestamps_are_normalized_and_filled() {
        let mut event = ev("bridge", "1", "bash", "cargo test", &[], 1000);
        event.execution = Some(ExecutionEvidence {
            id: "call-1".into(),
            command: "cargo test".into(),
            started_at: 0,
            ended_at: 1_700_000_000_000,
            exit_code: Some(0),
            result_source: "transcript".into(),
            ..Default::default()
        });
        let exec = event.sanitize().unwrap().execution.unwrap();
        assert_eq!(exec.ended_at, 1_700_000_000);
        assert_eq!(exec.started_at, 1_700_000_000);
    }
}
