//! 非机密设置:存储于 dataDir/settings.json;密钥类数据一律进 secrets.enc

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri_plugin_autostart::ManagerExt;

use crate::state::now_ts;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// LLM provider:openai | google | anthropic
    pub provider: String,
    /// 模型名(由用户填写)
    pub model: String,
    /// openai-compatible 自定义 BaseURL(可选,便于代理/中转)
    pub base_url: Option<String>,
    /// 每日 LLM 调用上限(默认 80)
    pub daily_budget: u64,
    /// 全局暂停提问
    pub paused: bool,
    /// 开机自启
    pub autostart: bool,
    /// P0 审查卡允许替换队列最旧卡(默认关)
    pub p0_replace_queue: bool,
    /// 胶囊停靠物理坐标(窗口左上角)
    pub dock_x: Option<i64>,
    pub dock_y: Option<i64>,
    /// 胶囊是否隐藏(仅托盘)
    pub capsule_hidden: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            provider: "openai".into(),
            model: String::new(),
            base_url: None,
            daily_budget: 80,
            paused: false,
            autostart: false,
            p0_replace_queue: false,
            dock_x: None,
            dock_y: None,
            capsule_hidden: false,
        }
    }
}

const SETTINGS_FILE: &str = "settings.json";

pub fn settings_path(dir: &std::path::Path) -> PathBuf {
    dir.join(SETTINGS_FILE)
}

pub fn load_settings(dir: &std::path::Path) -> anyhow::Result<Settings> {
    match std::fs::read_to_string(settings_path(dir)) {
        Ok(raw) => match serde_json::from_str::<Settings>(&raw) {
            Ok(mut s) => {
                // 每次启动记录
                if s.capsule_hidden {
                    // 保持用户上次的隐藏状态
                }
                s.capsule_hidden = false; // 默认启动显示胶囊
                Ok(s)
            }
            Err(_) => Ok(Settings::default()),
        },
        Err(_) => Ok(Settings::default()),
    }
}

pub fn save_settings(dir: &std::path::Path, s: &Settings) -> anyhow::Result<()> {
    let raw = serde_json::to_string_pretty(s)?;
    std::fs::write(settings_path(dir), raw)?;
    Ok(())
}

#[tauri::command]
pub fn settings_get(state: tauri::State<'_, std::sync::Arc<crate::state::AppState>>) -> Settings {
    state.core.blocking_lock().settings.clone()
}

#[tauri::command]
pub fn settings_put(
    state: tauri::State<'_, std::sync::Arc<crate::state::AppState>>,
    patch: serde_json::Value,
) -> Result<(), String> {
    let state = state.inner().clone();
    let dir = state.data_dir.clone();
    let mut core = state.core.blocking_lock();
    let old = core.settings.clone();
    let mut next = old.clone();
    if let Ok(current) = serde_json::from_value::<Settings>(patch.clone()) {
        // 只允许更新白名单字段,保留内部字段
        next.provider = current.provider;
        next.model = current.model;
        next.base_url = current.base_url;
        next.daily_budget = current.daily_budget;
        next.autostart = current.autostart;
        next.p0_replace_queue = current.p0_replace_queue;
    }
    // 单独的字段式补丁(前端的部分更新)
    if let Some(v) = patch.get("paused") {
        if let Some(b) = v.as_bool() {
            next.paused = b;
        }
    }
    if let Some(v) = patch.get("capsuleHidden") {
        if let Some(b) = v.as_bool() {
            next.capsule_hidden = b;
        }
    }

    core.settings = next;
    core.paused = core.settings.paused;
    core.capsule_hidden = core.settings.capsule_hidden;

    // 应用自启动设置
    let autostart_target = core.settings.autostart;
    let app = state.app.clone();
    let _ = app.autolaunch().enable();
    if !autostart_target {
        let _ = app.autolaunch().disable();
    }

    let _ = save_settings(&dir, &core.settings);
    drop(core);
    let _ = now_ts();
    Ok(())
}
