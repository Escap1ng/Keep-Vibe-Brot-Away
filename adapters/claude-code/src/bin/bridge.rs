//! Claude Code hooks 桥(stdin 收 hook JSON → 归一化事件 → POST KVBA)
//! 使用方式见 README:在 ~/.claude/settings.json 的 hooks 中配置本二进制路径。
//! 失败静默,始终 0 退出,绝不阻塞 Agent。

use kvba_adapters::common::{
    build_event, collect_files, longest_str, map_tool, scan_flags, EventSpec, SeverityFlags,
};
use kvba_adapters::post_event;

const MAX_STDIN_BYTES: usize = 1024 * 1024;

fn main() {
    // 读全量 stdin(超限即截断)
    let mut raw = String::new();
    {
        use std::io::Read;
        let _ = std::io::stdin()
            .lock()
            .take(MAX_STDIN_BYTES as u64)
            .read_to_string(&mut raw);
    }
    let hook: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return, // 非 JSON 输入(如暂停输出),忽略
    };

    let hook_event_name = hook["hook_event_name"].as_str().unwrap_or("");
    // Stop / SessionEnd / UserPromptSubmit 的 payload 里没有 cwd 字段,
    // 回退到 Claude Code 注入的 CLAUDE_PROJECT_DIR,再退到进程 cwd。
    let cwd = [
        hook["cwd"].as_str(),
        std::env::var("CLAUDE_PROJECT_DIR").ok().as_deref(),
        Some(""),
    ]
    .into_iter()
    .flatten()
    .find(|s| !s.is_empty())
    .unwrap_or("")
    .to_string();
    let session_id = hook["session_id"].as_str().unwrap_or("").to_string();

    let ev = match hook_event_name {
        // 用户刚提交提示 = 一整轮的开始。KVBA 的「会话开场破冰」正是挂在这个时刻,
        // 不接它就等于第一轮对话进行中永远不出卡。
        "UserPromptSubmit" => {
            let prompt = hook["prompt"].as_str().unwrap_or("").to_string();
            let flags = scan_flags(&prompt, "");
            build_event(EventSpec {
                emitter: "claude-code-bridge",
                event_type: "message.updated",
                tool_name: "prompt",
                files: vec![],
                diff_head: &prompt,
                intent: &format!("用户提示:{}", prompt.chars().take(200).collect::<String>()),
                flags,
                line_delta: 0,
                project_path: &cwd,
                session_id: &session_id,
            })
        }
        "PostToolUse" => {
            let raw_tool = hook["tool_name"].as_str().unwrap_or("").to_string();
            let tool = map_tool(&raw_tool);
            if tool == "task" {
                // Task 内联子任务与主 Agent 语义重复:降为 think 类弱信号
                return;
            }
            let tool_input = hook["tool_input"].clone();
            let tool_response = hook["tool_response"].clone();

            let files = collect_files(&tool_input);

            // diff/内容摘录:优先取 input 中最长的一段,皆空时回退 tool_response
            let mut diff = longest_str(
                &tool_input,
                &["diff", "patch", "content", "new_string", "newString", "replacement"],
            );
            if diff.is_empty() {
                if let Some(edits) = tool_input["edits"].as_array() {
                    diff = edits
                        .iter()
                        .map(|e| {
                            longest_str(e, &["new_string", "newString", "replacement"])
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                }
            }
            if diff.is_empty() {
                diff = longest_str(&tool_response, &["diff", "patch"]);
            }

            let command = longest_str(&tool_input, &["command", "cmd"]);
            let mut intent = String::new();
            if !command.is_empty() {
                intent = format!("$ {command}");
            } else if let Some(file) = files.first() {
                intent = format!("编辑 {file}");
                if let Some(from) = tool_input["old_string"]
                    .as_str()
                    .or_else(|| tool_input["oldString"].as_str())
                {
                    intent = format!("修改 {file}:{}", from.chars().take(80).collect::<String>());
                }
            }
            if intent.is_empty() {
                intent = format!("执行 {raw_tool}");
            }

            build_event(EventSpec {
                emitter: "claude-code-bridge",
                event_type: "tool.completed",
                tool_name: &tool,
                files,
                diff_head: &diff,
                intent: &intent,
                flags: scan_flags(&intent, &diff),
                // bash 的"行数"是输出行数,不是代码变更量 —— 伪造它会污染 G5-A 打分
                line_delta: if tool == "bash" {
                    0
                } else {
                    diff.lines().count() as i64
                },
                project_path: &cwd,
                session_id: &session_id,
            })
        }
        "Stop" | "SubagentStop" => build_event(EventSpec {
            emitter: "claude-code-bridge",
            event_type: "turn.ended",
            tool_name: "",
            files: vec![],
            diff_head: "",
            // stop_hook_active 是布尔;给出稳定 intent 便于门控与去重
            intent: if hook_event_name == "SubagentStop" {
                "子代理回合结束"
            } else if hook["stop_hook_active"].as_bool().unwrap_or(false) {
                "回合结束(停止钩子应答)"
            } else {
                "回合结束"
            },
            flags: SeverityFlags::default(),
            line_delta: 0,
            project_path: &cwd,
            session_id: &session_id,
        }),
        // 会话结束 → 服务端做一次增量记忆合并
        "SessionEnd" => {
            let reason = hook["reason"].as_str().unwrap_or("other");
            build_event(EventSpec {
                emitter: "claude-code-bridge",
                event_type: "session.ended",
                tool_name: "",
                files: vec![],
                diff_head: "",
                intent: &format!("会话结束({reason})"),
                flags: SeverityFlags::default(),
                line_delta: 0,
                project_path: &cwd,
                session_id: &session_id,
            })
        }
        // Notification 有意不接:它覆盖"Claude 需要审批"(用户正被阻塞,递卡是干扰)
        // 与"输入框闲置 60s"(没有可递的内容,纯烧额度)两种场景。
        _ => return, // 其它 hook 事件不处理
    };

    let _ = post_event(&ev);
    // 结束后由 stdout 回显空,保证 hook 不被打断
}
