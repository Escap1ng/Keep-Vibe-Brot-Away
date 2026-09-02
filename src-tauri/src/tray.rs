//! 系统托盘:常驻 + 菜单(来一道 / 显示隐藏胶囊 / 暂停恢复 / 设置 / 退出)
//! 胶囊被隐藏时,系统通知仅作为兜底(预算用尽也走通知提醒一次)。

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::Manager;

/// 取出全局状态(持有副本,避免 State 守卫在块尾提前失效)
fn app_state(app: &tauri::AppHandle) -> std::sync::Arc<crate::state::AppState> {
    app.state::<std::sync::Arc<crate::state::AppState>>()
        .inner()
        .clone()
}

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

    // 菜单项按引用持有,才能在状态翻转后就地改文案(重新 with_id 只会造一个新条目)
    let (toggle_item, pause_item) = (toggle.clone(), pause.clone());

    TrayIconBuilder::with_id("kvba-tray")
        .icon(Image::from_bytes(include_bytes!("../icons/tray.png"))?)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("KVBA 项目伴读")
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "tray-tip" => {
                let state = app_state(app);
                crate::http::pipeline_run(&state, true);
            }
            "tray-toggle" => {
                let state = app_state(app);
                let hidden = state.core.blocking_lock().capsule_hidden;
                if hidden {
                    crate::windows::show_capsule(app);
                } else {
                    crate::windows::hide_capsule(app);
                }
                let _ = toggle_item.set_text(if hidden {
                    "隐藏胶囊"
                } else {
                    "显示胶囊"
                });
            }
            "tray-pause" => {
                let state = app_state(app);
                let paused = {
                    let mut core = state.core.blocking_lock();
                    core.settings.paused = !core.settings.paused;
                    core.paused = core.settings.paused;
                    let paused = core.paused;
                    let _ = crate::settings::save_settings(&state.data_dir, &core.settings);
                    paused
                };
                // 菜单文案随状态翻转
                let _ = pause_item.set_text(if paused {
                    "恢复提问"
                } else {
                    "暂停提问"
                });
            }
            "tray-settings" => crate::windows::open_settings(app),
            "tray-exit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // 左键单击:胶囊隐藏时唤起,否则展开面板;其余(右键/双击)交给系统菜单
            if let tauri::tray::TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                ..
            } = event
            {
                let app = tray.app_handle();
                let hidden = app_state(app).core.blocking_lock().capsule_hidden;
                if hidden {
                    crate::windows::show_capsule(app);
                } else {
                    crate::windows::show_panel_mode(app, "popup");
                }
            }
        })
        .build(app)?;
    Ok(())
}
