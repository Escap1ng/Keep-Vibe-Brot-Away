//! 非机密设置:存储于 dataDir/settings.json;密钥类数据一律进 secrets.enc

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri_plugin_autostart::ManagerExt;

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

/// 读取设置;文件缺失或损坏一律回退默认值(不阻塞启动)。
/// `capsuleHidden` 是运行期状态,不落盘生效——每次启动强制显示胶囊。
pub fn load_settings(dir: &std::path::Path) -> anyhow::Result<Settings> {
    let raw = std::fs::read_to_string(settings_path(dir)).unwrap_or_default();
    let mut s = serde_json::from_str::<Settings>(&raw).unwrap_or_default();
    s.capsule_hidden = false;
    Ok(s)
}

pub fn save_settings(dir: &std::path::Path, s: &Settings) -> anyhow::Result<()> {
    let raw = serde_json::to_string_pretty(s)?;
    std::fs::write(settings_path(dir), raw)?;
    Ok(())
}

/// 前端可补丁的字段:全部可选,字段缺失即"保持原值"。
/// 因此不能直接反序列化成 `Settings`——那会把未提交的字段重置为默认值。
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct SettingsPatch {
    provider: Option<String>,
    model: Option<String>,
    /// 双层 Option:字段缺失 = 保留原值,显式 null = 清空为「官方端点」
    base_url: Option<Option<String>>,
    daily_budget: Option<u64>,
    autostart: Option<bool>,
    p0_replace_queue: Option<bool>,
    paused: Option<bool>,
    capsule_hidden: Option<bool>,
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
    // 只接受白名单内的补丁字段;整体覆盖会把未提交的字段重置为默认值
    let p = serde_json::from_value::<SettingsPatch>(patch)
        .map_err(|e| format!("无效的设置补丁:{e}"))?;
    {
        let s = &mut core.settings;
        if let Some(v) = p.provider {
            s.provider = v;
        }
        if let Some(v) = p.model {
            s.model = v;
        }
        if let Some(v) = p.base_url {
            s.base_url = v;
        }
        if let Some(v) = p.daily_budget {
            s.daily_budget = v.max(1);
        }
        if let Some(v) = p.autostart {
            s.autostart = v;
        }
        if let Some(v) = p.p0_replace_queue {
            s.p0_replace_queue = v;
        }
        if let Some(v) = p.paused {
            s.paused = v;
        }
        if let Some(v) = p.capsule_hidden {
            s.capsule_hidden = v;
        }
    }
    // 运行期镜像:Core 上的标志位始终跟随设置
    core.paused = core.settings.paused;
    core.capsule_hidden = core.settings.capsule_hidden;

    apply_autostart(state.app.clone(), core.settings.autostart);

    let _ = save_settings(&dir, &core.settings);
    Ok(())
}

/// 自启开关(插件提供 enable/disable 两个动作,按目标状态单向调用以免误注册)
fn apply_autostart(app: tauri::AppHandle, enable: bool) {
    let autolaunch = app.autolaunch();
    let _ = if enable {
        autolaunch.enable()
    } else {
        autolaunch.disable()
    };
}
