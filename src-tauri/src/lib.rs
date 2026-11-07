//! KVBA 核心库入口:Tauri 应用装配(托盘、悬浮胶囊窗口、模块装配)

pub mod cognition_store;
pub mod crypto;
pub mod events;
pub mod gate;
pub mod http;
pub mod insight;
pub mod llm;
pub mod memory;
pub mod queue;
pub mod settings;
pub mod state;
pub mod tray;
pub mod windows;
pub mod workflow;
pub mod workunit;

use tauri::Manager;

/// 应用入口:启用插件、初始化状态与后台服务、装配命令
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // 已有实例:唤起胶囊并提示
            let _ = app.get_webview_window("capsule-dot").map(|w| w.show());
        }))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .build(),
        )
        .setup(|app| {
            let handle = app.handle();

            // 初始化全局状态(数据目录、设置、队列、记忆、统计等)
            let app_state = state::AppState::init(handle)?;
            // manage 会移动所有权,这里留一份 Arc 给启动期的 facts 补生成
            let state_arc = app_state.clone();
            app.manage(app_state);

            // 启动本地 HTTP 摄入服务(127.0.0.1 随机端口 + 随机 token)
            crate::http::start_server(handle);

            // 为"重启前已有事件、但 facts 还没生成"的活动项目补一次 facts:
            // 只在事件流入时 kick 会让 Agent 闲置期的项目永远拿不到记忆,
            // 主题恒为 framework-unknown → 知识卡每次同一句话 → 被正文去重全部挡死
            crate::memory::kick_facts_for_active(&state_arc);

            // 定位胶囊窗口(默认右缘中部)
            crate::windows::initial_position(handle);

            // 常驻窗口关闭保护(✕ → 隐藏)
            crate::windows::protect_windows_from_close(handle);

            // 面板被拖出屏幕时整块钳回可见区
            crate::windows::keep_panel_on_screen(handle);

            // 创建系统托盘
            crate::tray::build_tray(handle)?;

            // 后台任务:TTL 过期清扫 / 冷却矩阵衰减
            crate::queue::spawn_maintenance(handle);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            crate::windows::capsule_set_state,
            crate::windows::dock_snap,
            crate::windows::panel_open,
            crate::windows::panel_close,
            crate::windows::capsule_hide,
            crate::windows::capsule_show,
            crate::windows::settings_open,
            crate::settings::settings_get,
            crate::settings::settings_put,
            crate::queue::queue_listen,
            crate::queue::queue_answer,
            crate::queue::queue_dismiss,
            crate::queue::queue_mute_theme,
            crate::gate::gate_tick,
            crate::gate::decision_log_list,
            crate::state::history_list,
            crate::state::endpoint_info,
            crate::state::facts_get,
            crate::state::stats_info,
            crate::workflow::work_units_list,
            crate::workflow::insights_list,
            crate::crypto::secrets_put,
            crate::crypto::secrets_test,
            crate::llm::budget::budget_info,
            crate::llm::llm_test_connection,
            crate::tray::quit_app,
        ])
        .run(tauri::generate_context!())
        .expect("KVBA 启动失败");
}
