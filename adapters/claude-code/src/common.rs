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

/// 取 `keys` 中**最长**的字符串字段(内容/差异常分散在多个字段,最长的最接近正文)。
///
/// 两侧命名都得列:Claude Code 用 snake_case(`new_string`),Trae 用 camelCase
/// (`newString`)。旧实现只查 camelCase,于是对 Claude 的 Edit 永远取不到 diff
/// —— `lineDelta` 恒为 0,G5-A 的"大改 +30"这一路直接哑掉。
pub fn longest_str(v: &Value, keys: &[&str]) -> String {
    let mut best = String::new();
    if let Some(obj) = v.as_object() {
        for k in keys {
            if let Some(s) = obj.get(*k).and_then(|x| x.as_str()) {
                if s.len() > best.len() {
                    best = s.to_string();
                }
            }
        }
    }
    best
}

/// 从 tool_input 收集受影响文件(`file_path` 单值 + `file_paths` 列表 + MultiEdit 的 edits 数组)
pub fn collect_files(input: &Value) -> Vec<String> {
    let mut files = Vec::new();
    if let Some(p) = input["file_path"].as_str() {
        files.push(p.to_string());
    }
    if let Some(list) = input["file_paths"].as_array() {
        files.extend(list.iter().filter_map(|p| p.as_str().map(str::to_string)));
    }
    // MultiEdit 的文件藏在 edits 数组里,不看这一路就等于"改了但没留下文件名"
    if let Some(edits) = input["edits"].as_array() {
        for e in edits {
            for key in ["file_path", "filePath"] {
                if let Some(p) = e[key].as_str() {
                    files.push(p.to_string());
                }
            }
        }
    }
    // 按**首次出现顺序**去重。不能排序:`excerpt.files` 的第一个元素参与 G6 主题级指纹,
    // 排序会让两个适配器(bridge / node hook)对同一动作给出不同的"首个文件",
    // 跨 emitter 的内容判重随之失效。
    let mut seen = std::collections::HashSet::new();
    files.retain(|f| seen.insert(f.clone()));
    files
}

/// Claude Code 的工具名是首字母大写的,而门控的 `is_write_event` 只认规范小写名
/// (`edit` / `write` / `patch`)。**不归一化 = 这一路永远产不出审查卡。**
///
/// 这正是本 bridge 的旧缺陷:它把 `Edit` 原样上报,于是 `matches!("edit"|…)` 恒不成立,
/// Claude Code 用户只能拿到知识卡,拿不到审查卡 —— 而且现象是"应用一切正常",无从察觉。
pub fn map_tool(name: &str) -> String {
    let mapped = match name {
        "Write" => "write",
        "Edit" | "MultiEdit" | "NotebookEdit" => "edit",
        "Bash" => "bash",
        "Read" => "read",
        "Glob" => "glob",
        "Grep" => "grep",
        "Task" => "task",
        "WebFetch" => "webfetch",
        "WebSearch" => "websearch",
        "SlashCommand" => "slash",
        // MCP 工具形如 mcp__github__create_issue,以及未知工具:整串小写保留
        _ => return name.to_lowercase(),
    };
    mapped.to_string()
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

    /// 回归:工具名不归一化 → 门控的 is_write_event 恒不命中 → Claude 这一路永远出不了审查卡
    #[test]
    fn map_tool_normalizes_claude_names_to_gate_vocabulary() {
        assert_eq!(map_tool("Edit"), "edit");
        assert_eq!(map_tool("MultiEdit"), "edit");
        assert_eq!(map_tool("NotebookEdit"), "edit");
        assert_eq!(map_tool("Write"), "write");
        // Bash 不是写事件:把跑命令误判成改代码,会凭空造出审查卡
        assert_eq!(map_tool("Bash"), "bash");
        assert_eq!(map_tool("Read"), "read");
        assert_eq!(
            map_tool("mcp__github__create_issue"),
            "mcp__github__create_issue"
        );
        assert_eq!(map_tool(""), "");
    }

    /// 回归:Claude Code 的 Edit 用 snake_case,旧实现只查 camelCase → diff 恒空、lineDelta 恒 0
    #[test]
    fn longest_str_reads_claude_snake_case_and_picks_the_longest() {
        let input =
            json!({ "file_path": "a.ts", "old_string": "x", "new_string": "aaa\nbbb\nccc" });
        let got = longest_str(&input, &["diff", "new_string", "newString", "content"]);
        assert_eq!(got, "aaa\nbbb\nccc");
        assert_eq!(longest_str(&json!(null), &["new_string"]), "");
        assert_eq!(longest_str(&json!({ "a": 1 }), &["a"]), ""); // 非字符串字段不参与
    }

    #[test]
    fn collect_files_keeps_first_seen_order_and_reads_multiedit() {
        let input = json!({
            "file_path": "z.ts",
            "file_paths": ["a.ts", "z.ts"],
            "edits": [{ "file_path": "b.ts" }, { "filePath": "a.ts" }]
        });
        // 顺序有意义:excerpt.files[0] 参与 G6 主题级指纹,排序会让两个适配器对同一动作给出不同键
        assert_eq!(
            collect_files(&input),
            vec!["z.ts".to_string(), "a.ts".to_string(), "b.ts".to_string()]
        );
    }
}
