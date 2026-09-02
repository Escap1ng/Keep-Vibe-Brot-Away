//! Claude Code hooks 桥(stdin 收 hook JSON → 归一化事件 → POST KVBA)
//! 使用方式见 README:在 ~/.claude/settings.json 的 hooks 中配置本二进制路径。
//! 失败静默,始终 0 退出,绝不阻塞 Agent。

use kvba_adapters::common::{build_event, SeverityFlags};
use kvba_adapters::post_event;

const MAX_STDIN_BYTES: usize = 1024 * 1024;

fn main() {
    // 读全量 stdin(超限即截断)
    let mut raw = String::new();
    {
        use std::io::Read;
        let _ = std::io::stdin().lock().take(MAX_STDIN_BYTES as u64).read_to_string(&mut raw);
    }
    let hook: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return, // 非 JSON 输入(如暂停输出),忽略
    };

    let hook_event_name = hook["hook_event_name"].as_str().unwrap_or("");
    let cwd = hook["cwd"].as_str().unwrap_or("").to_string();
    let session_id = hook["session_id"].as_str().unwrap_or("").to_string();

    let ev = match hook_event_name {
        "PostToolUse" => {
            let tool_name = hook["tool_name"].as_str().unwrap_or("").to_string();
            if tool_name == "Task" {
                // Task 内联子任务与主 Agent 语义重复:降为 think 类弱信号
                return;
            }
            let tool_input = hook["tool_input"].clone();
            let tool_response = hook["tool_response"].clone();

            let mut files: Vec<String> = Vec::new();
            if let Some(p) = tool_input["file_path"].as_str() {
                files.push(p.to_string());
            }
            if let Some(list) = tool_input["file_paths"].as_array() {
                for p in list {
                    if let Some(p) = p.as_str() {
                        files.push(p.to_string());
                    }
                }
            }

            // diff/内容摘录
            let mut diff = String::new();
            for key in ["diff", "patch", "newString", "content"] {
                if let Some(s) = tool_input[key].as_str() {
                    if s.len() > diff.len() {
                        diff = s.to_string();
                    }
                }
            }
            if diff.is_empty() {
                for key in ["diff", "patch"] {
                    if let Some(s) = tool_response[key].as_str() {
                        diff = s.to_string();
                    }
                }
            }

            let mut intent = String::new();
            if let Some(cmd) = tool_input["command"].as_str() {
                intent = format!("$ {cmd}");
            } else if let Some(file) = files.first() {
                intent = format!("编辑 {file}");
                if let Some(from) = tool_input["oldString"].as_str() {
                    intent = format!("修改 {file}:{}", from.chars().take(80).collect::<String>());
                }
            }
            if intent.is_empty() {
                intent = format!("执行 {tool_name}");
            }

            let flags = detect_flags(&intent, &diff);
            let line_delta = diff.lines().count() as i64;

            build_event(
                "claude-code-bridge",
                "tool.completed",
                &tool_name,
                files,
                &diff,
                &intent,
                flags.has_api_call,
                flags.has_error_path,
                flags.todo_or_fixme,
                line_delta,
                &cwd,
                &session_id,
            )
        }
        "Stop" => build_event(
            "claude-code-bridge",
            "turn.ended",
            "",
            vec![],
            "",
            // stop_hook_active 是布尔;给出稳定 intent 便于门控与去重
            if hook["stop_hook_active"].as_bool().unwrap_or(false) {
                "回合结束(停止钩子应答)"
            } else {
                "回合结束"
            },
            false,
            false,
            false,
            0,
            &cwd,
            &session_id,
        ),
        _ => return, // 其它 hook 事件本期不处理
    };

    let _ = post_event(&ev);
    // 结束后由 stdout 回显空,保证 hook 不被打断
}

fn detect_flags(intent: &str, diff: &str) -> SeverityFlags {
    use kvba_adapters::common::scan_flags;
    scan_flags(intent, diff)
}
