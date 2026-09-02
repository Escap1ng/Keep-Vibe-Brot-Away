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

/// 构造归一化事件(schema kvba.event.v1;id=blake3)
pub fn build_event(
    emitter: &str,
    event_type: &str,
    tool_name: &str,
    files: Vec<String>,
    diff_head: &str,
    intent: &str,
    has_api_call: bool,
    has_error_path: bool,
    todo_or_fixme: bool,
    line_delta: i64,
    project_path: &str,
    session_id: &str,
) -> Value {
    let mut excerpt = json!({
        "files": [],
        "diffHead": diff_head,
        "intent": intent,
        "hasErrorPath": has_error_path,
        "hasApiCall": has_api_call,
        "todoOrFixme": todo_or_fixme,
        "lineDelta": line_delta,
    });
    excerpt["files"] = json!(files);

    // 同一时间戳同时用于 id 哈希与最终事件,保证二者一致
    let ts = now_ms();
    let stub = json!({
        "emitter": emitter,
        "eventType": event_type,
        "ts": ts,
        "projectPath": project_path,
        "intent": intent,
    });
    let id = blake3::hash(stub.to_string().as_bytes()).to_hex()[..24].to_string();

    json!({
        "schema": "kvba.event.v1",
        "id": id,
        "emitter": emitter,
        "eventType": event_type,
        "toolName": tool_name,
        "ts": ts,
        "projectPath": project_path,
        "sessionId": session_id,
        "excerpt": excerpt,
    })
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_event_schema_and_fields() {
        let ev = build_event("test", "tool.completed", "edit", vec!["a.ts"], "+x", "编辑 a.ts", false, false, false, 1, "C:/p", "s1");
        assert_eq!(ev["schema"], "kvba.event.v1");
        assert_eq!(ev["emitter"], "test");
        assert_eq!(ev["projectPath"], "C:/p");
        assert!(ev["ts"].as_u64().unwrap() > 0);
        assert!(!ev["id"].as_str().unwrap().is_empty());
    }

    #[test]
    fn scan_flags_detects_api_and_todo_and_error() {
        let f = scan_flags("接入 http API", "// TODO: handle throw");
        assert!(f.has_api_call);
        assert!(f.todo_or_fixme);
        assert!(f.has_error_path);
    }
}
