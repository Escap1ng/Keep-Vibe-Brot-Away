//! JSONL 兜底 watcher(方案 C):监听 ~/.claude/projects/**/*.jsonl 增量,
//! 解析 assistant 消息中的 tool_use(edit/write/bash/read/think…),
//! 归一化后上报 KVBA。输出文件路径含空格时由脚本注入 JSON 配置。
//!
//! 通过环境变量配置:
//! - KVBA_CLAUDEDIR:Claude Code 数据根(default: ~/.claude)
//! - KVBA_WATCH_INTERVAL_MS:轮询周期(default 5000)

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use kvba_adapters::common::{build_event, scan_flags};
use kvba_adapters::post_event;

fn main() {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_else(|_| ".".into());
    let claude_dir = std::env::var("KVBA_CLAUDEDIR").unwrap_or_else(|_| format!("{home}/.claude"));
    let interval_ms: u64 = std::env::var("KVBA_WATCH_INTERVAL_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(5000);

    let projects_dir = PathBuf::from(&claude_dir).join("projects");
    // 每文件独立偏移:path → offset
    let mut offsets: HashMap<PathBuf, u64> = HashMap::new();
    // 每文件项目路径缓存:path → cwd(取自转录行的 cwd 字段)
    let mut cwd_cache: HashMap<PathBuf, String> = HashMap::new();

    loop {
        if let Ok(entries) = fs::read_dir(&projects_dir) {
            for dir in entries.flatten() {
                if !dir.path().is_dir() {
                    continue;
                }
                if let Ok(files) = fs::read_dir(dir.path()) {
                    for f in files.flatten() {
                        let p = f.path();
                        if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                            tail_file(&p, &mut offsets, &mut cwd_cache);
                        }
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(interval_ms));
    }
}

fn tail_file(path: &Path, offsets: &mut HashMap<PathBuf, u64>, cwd_cache: &mut HashMap<PathBuf, String>) {
    let Ok(meta) = fs::metadata(path) else { return };
    let len = meta.len();
    let offset = offsets.entry(path.to_path_buf()).or_insert(0);
    if *offset > len {
        // 文件被轮转/截断 → 从头
        *offset = 0;
    }
    if len <= *offset {
        return;
    }
    let Ok(f) = fs::File::open(path) else { return };
    use std::io::{BufRead, BufReader, Seek, SeekFrom};
    let mut reader = BufReader::new(f);
    if reader.seek(SeekFrom::Start(*offset)).is_err() {
        return;
    }
    let mut line = String::new();
    let mut consumed: u64 = 0;
    let mut lines: u32 = 0;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(n) => {
                // 半行保护:文件正在被写入、行尾尚无换行符时不提交偏移,
                // 等待下次轮询该行补全后再读,避免残行被永久跳过。
                if !line.ends_with('\n') {
                    break;
                }
                consumed += n as u64;
                lines += 1;
                handle_line(&line, path, cwd_cache);
                // 安全阀:单次最多 200 条(或 1MB)
                if lines >= 200 || consumed > 1_000_000 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    *offset += consumed;
}

fn handle_line(line: &str, jsonl_path: &Path, cwd_cache: &mut HashMap<PathBuf, String>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return };
    let Some(msg) = v["message"].as_object() else { return };
    let session_id = v["sessionId"].as_str().unwrap_or("").to_string();
    // 项目路径:转录行自带 cwd 字段(权威来源);缺失时用本文件已缓存的 cwd;
    // 两者都没有则放弃该事件,绝不猜测路径(避免把事件归错项目)。
    let cwd = match v["cwd"].as_str() {
        Some(c) if !c.is_empty() => {
            cwd_cache.insert(jsonl_path.to_path_buf(), c.to_string());
            c.to_string()
        }
        _ => cwd_cache.get(jsonl_path).cloned().unwrap_or_default(),
    };
    if cwd.is_empty() {
        return;
    }

    let Some(content) = msg["content"].as_array() else { return };
    for part in content {
        if part["type"] == "tool_use" {
            let tool_name = part["name"].as_str().unwrap_or("").to_string();
            if tool_name.is_empty() {
                continue;
            }
            let input = part["input"].clone();
            let mut files: Vec<String> = Vec::new();
            if let Some(fp) = input["file_path"].as_str() {
                files.push(fp.to_string());
            }
            if let Some(list) = input["file_paths"].as_array() {
                for p in list {
                    if let Some(p) = p.as_str() {
                        files.push(p.to_string());
                    }
                }
            }
            let mut intent = String::new();
            if let Some(cmd) = input["command"].as_str() {
                intent = format!("$ {cmd}");
            } else if let Some(file) = files.first() {
                intent = format!("编辑 {file}");
            } else if tool_name == "think" {
                intent = input["content"]
                    .as_str()
                    .map(|s| s.chars().take(300).collect())
                    .unwrap_or_default();
            }
            if intent.is_empty() {
                intent = format!("执行 {tool_name}");
            }
            let mut diff = String::new();
            for key in ["diff", "patch", "content"] {
                if let Some(s) = input[key].as_str() {
                    if s.len() > diff.len() {
                        diff = s.to_string();
                    }
                }
            }
            let flags = scan_flags(&intent, &diff);
            let ev = build_event(
                "claude-code-watcher",
                "tool.completed",
                &tool_name,
                files,
                &diff,
                &intent,
                flags.has_api_call,
                flags.has_error_path,
                flags.todo_or_fixme,
                diff.lines().count() as i64,
                &cwd,
                &session_id,
            );
            let _ = post_event(&ev);
        }
    }
}
