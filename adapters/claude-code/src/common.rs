//! KVBA 适配器公共工具:归一化事件构造 + 信号探测(0 token)

use serde_json::json;
use serde_json::Value;

#[derive(Debug, Default, Clone)]
pub struct SeverityFlags {
    pub has_error_path: bool,
    pub has_api_call: bool,
    pub todo_or_fixme: bool,
}

/// 从 intent/diff 文本本地探测信号(不含任何 AI 调用)
pub fn scan_flags(intent: &str, diff: &str) -> SeverityFlags {
    let lower = format!("{} {}", intent.to_lowercase(), diff.to_lowercase());
    SeverityFlags {
        has_api_call: lower.contains("http")
            || lower.contains("curl")
            || lower.contains("fetch(")
            || lower.contains("axios")
            || lower.contains("npm ")
            || lower.contains("pip ")
            || lower.contains("go get")
            || lower.contains("bun add"),
        has_error_path: lower.contains("panic!") || lower.contains("throw"),
        todo_or_fixme: lower.contains("todo") || lower.contains("fixme"),
    }
}

/// 从 tool_input 中收集受影响文件(`file_path` 单值 + `file_paths` 列表)
pub fn collect_files(input: &Value) -> Vec<String> {
    let mut files = Vec::new();
    if let Some(p) = input["file_path"].as_str() {
        files.push(p.to_string());
    }
    if let Some(list) = input["file_paths"].as_array() {
        files.extend(list.iter().filter_map(|p| p.as_str().map(str::to_string)));
    }
    files
}

/// 构造归一化事件所需的字段集合(集中一处,避免十余个位置参数错位)
pub struct EventSpec<'a> {
    pub emitter: &'a str,
    pub event_type: &'a str,
    pub tool_name: &'a str,
    pub files: Vec<String>,
    pub diff_head: &'a str,
    pub intent: &'a str,
    pub flags: SeverityFlags,
    pub line_delta: i64,
    pub project_path: &'a str,
    pub session_id: &'a str,
}

/// 构造归一化事件(schema kvba.event.v1;id=blake3)
pub fn build_event(spec: EventSpec<'_>) -> Value {
    // 同一时间戳同时用于 id 哈希与最终事件,保证二者一致
    let ts = now_ms();
    let stub = json!({
        "emitter": spec.emitter,
        "eventType": spec.event_type,
        "ts": ts,
        "projectPath": spec.project_path,
        "intent": spec.intent,
    });
    let id = blake3::hash(stub.to_string().as_bytes()).to_hex()[..24].to_string();

    json!({
        "schema": "kvba.event.v1",
        "id": id,
        "emitter": spec.emitter,
        "eventType": spec.event_type,
        "toolName": spec.tool_name,
        "ts": ts,
        "projectPath": spec.project_path,
        "sessionId": spec.session_id,
        "excerpt": {
            "files": spec.files,
            "diffHead": spec.diff_head,
            "intent": spec.intent,
            "hasErrorPath": spec.flags.has_error_path,
            "hasApiCall": spec.flags.has_api_call,
            "todoOrFixme": spec.flags.todo_or_fixme,
            "lineDelta": spec.line_delta,
        },
    })
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> EventSpec<'static> {
        EventSpec {
            emitter: "test",
            event_type: "tool.completed",
            tool_name: "edit",
            files: vec!["a.ts".to_string()],
            diff_head: "+x",
            intent: "编辑 a.ts",
            flags: SeverityFlags::default(),
            line_delta: 1,
            project_path: "C:/p",
            session_id: "s1",
        }
    }

    #[test]
    fn build_event_schema_and_fields() {
        let ev = build_event(spec());
        assert_eq!(ev["schema"], "kvba.event.v1");
        assert_eq!(ev["emitter"], "test");
        assert_eq!(ev["projectPath"], "C:/p");
        assert!(ev["ts"].as_u64().unwrap() > 0);
        assert!(!ev["id"].as_str().unwrap().is_empty());
    }

    #[test]
    fn collect_files_merges_single_and_list() {
        let input = json!({ "file_path": "a.ts", "file_paths": ["b.ts", 3, "c.ts"] });
        assert_eq!(collect_files(&input), vec!["a.ts", "b.ts", "c.ts"]);
    }

    #[test]
    fn scan_flags_detects_api_and_todo_and_error() {
        let f = scan_flags("接入 http API", "// TODO: handle throw");
        assert!(f.has_api_call);
        assert!(f.todo_or_fixme);
        assert!(f.has_error_path);
    }
}
