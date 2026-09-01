//! 项目伴读(KVBA)适配器集合:Claude Code hooks 桥 + JSONL 兜底 watcher
//!
//! 两个二进制:
//! - kvba-claude-bridge:stdin 读 hook JSON(PostToolUse/Stop)→ POST 归一化事件
//! - kvba-claude-watcher:~/.claude/projects/**/*.jsonl 增量兜底 watcher(方案 C)
//!
//! 铁律:上报失败重试 3 次后丢弃,绝不影响 Agent(始终以 0 退出)。

pub mod common;

use std::env;

/// KVBA 主程序数据目录(跨平台默认 + KVBA_DATA_DIR 覆盖)
pub fn kvba_data_dir() -> String {
    if let Ok(dir) = env::var("KVBA_DATA_DIR") {
        if !dir.is_empty() {
            return dir;
        }
    }
    if let Ok(appdata) = env::var("APPDATA") {
        return format!("{}\\dev.kvba.app", appdata);
    }
    if let Ok(home) = env::var("HOME") {
        return format!("{}/.local/share/dev.kvba.app", home);
    }
    if let Ok(profile) = env::var("USERPROFILE") {
        return format!("{}\\AppData\\Roaming\\dev.kvba.app", profile);
    }
    ".".into()
}

/// 端点信息(endpoint.json)
pub fn endpoint_path(data_dir: &str) -> String {
    format!("{}/endpoint.json", data_dir.trim_end_matches(['/', '\\']))
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub port: u16,
    pub hello_token: String,
    pub event_token: String,
}

/// 读取端点信息(KVBA 未启动 → Err)
pub fn load_endpoint() -> anyhow::Result<Endpoint> {
    let dir = kvba_data_dir();
    let raw = std::fs::read_to_string(endpoint_path(&dir))?;
    let v: serde_json::Value = serde_json::from_str(&raw)?;
    Ok(Endpoint {
        port: v["port"].as_u64().unwrap_or(0) as u16,
        hello_token: v["helloToken"].as_str().unwrap_or("").to_string(),
        event_token: v["eventToken"].as_str().unwrap_or("").to_string(),
    })
}

/// 上报事件:hello 换 token → POST(kvba/events),失败指数重试 ≤3 次
pub fn post_event(ev: &serde_json::Value) -> anyhow::Result<bool> {
    let ep = load_endpoint()?;
    if ep.port == 0 {
        return Ok(false);
    }
    let base = format!("http://127.0.0.1:{}", ep.port);
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()?;

    let mut event_token = ep.event_token.clone();

    // 用主 token 换取上报 token(KVBA 重启后 eventToken 仍有效,失败即回退)
    if let Ok(resp) = client
        .post(format!("{base}/kvba/hello"))
        .header("X-KVBA-Token", &ep.hello_token)
        .send()
    {
        if let Ok(v) = resp.json::<serde_json::Value>() {
            if let Some(t) = v["token"].as_str() {
                event_token = t.to_string();
            }
        }
    }

    let mut last_err = String::new();
    for attempt in 0..3 {
        match client
            .post(format!("{base}/kvba/events"))
            .header("X-KVBA-Token", event_token.clone())
            .json(ev)
            .send()
        {
            Ok(_) => return Ok(true),
            Err(e) => last_err = e.to_string(),
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(200 * (attempt as u64 + 1)));
        }
    }
    eprintln!("[kvba] 事件上报失败(已重试):{last_err}");
    Ok(false)
}
