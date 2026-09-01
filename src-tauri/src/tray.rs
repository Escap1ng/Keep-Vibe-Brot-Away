//! 系统托盘:常驻 + 菜单(来一道 / 显示隐藏胶囊 / 暂停恢复 / 设置 / 退出)
//! 胶囊被隐藏时,系统通知仅作为兜底(预算用尽也走通知提醒一次)。

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::Manager;

/// 退出应用(前端菜单入口)
#[tauri::command]
pub fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

pub fn build_tray(app: &tauri::AppHandle) -> anyhow::Result<()> {
    let menu = Menu::new(app)?;
    let tip = MenuItem::with_id(app, "tray-tip", "来一道", true, None::<&str>)?;
    let toggle = MenuItem::with_id(app, "tray-toggle", "隐藏胶囊", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "tray-pause", "暂停提问", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "tray-settings", "打开设置…", true, None::<&str>)?;
    let exit = MenuItem::with_id(app, "tray-exit", "退出", true, None::<&str>)?;
    menu.append_items(&[&tip, &toggle, &pause, &settings, &exit])?;

    TrayIconBuilder::with_id("kvba-tray")
        .icon(Image::from_bytes(include_bytes!("../icons/tray.png"))?)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("KVBA 项目伴读")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "tray-tip" => {
                let state = app.state::<std::sync::Arc<crate::state::AppState>>();
                crate::http::pipeline_run(&state, true);
            }
            "tray-toggle" => {
                let state = app.state::<std::sync::Arc<crate::state::AppState>>();
                let hidden = state.core.blocking_lock().capsule_hidden;
                if hidden {
                    crate::windows::show_capsule(app);
                } else {
                    crate::windows::hide_capsule(app);
                }
            }
            "tray-pause" => {
                let state = app.state::<std::sync::Arc<crate::state::AppState>>();
                let mut core = state.core.blocking_lock();
                core.settings.paused = !core.settings.paused;
                core.paused = core.settings.paused;
                let _ = crate::settings::save_settings(&state.data_dir, &core.settings);
                drop(core);
            }
            "tray-settings" => crate::windows::open_settings(app),
            "tray-exit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            let app = tray.app_handle();
            match event {
                tauri::tray::TrayIconEvent::Click { button: tauri::tray::MouseButton::Left, .. } => {
                    let state = app.state::<std::sync::Arc<crate::state::AppState>>();
                    if state.core.blocking_lock().capsule_hidden {
                        crate::windows::show_capsule(app);
                    } else {
                        crate::windows::show_panel_mode(app, "popup");
                    }
                }
                _ => {}
            }
        })
        .build(app)?;
    Ok(())
}
