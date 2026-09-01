//! 窗口装配:悬浮胶囊(idle 小圆点 / active 横幅面板)、停靠吸附、隐藏胶囊(仅托盘)
//!
//! 坐标纪律:tauri.conf 的 width/height 是逻辑像素,而 PhysicalPosition/PhysicalSize 是物理像素。
//! 所有锚定计算一律读取 `outer_size()`(真实物理尺寸)并钳制在圆点所在显示器的边界内,
//! 保证任意 DPI 缩放(125%/150%)与多显示器下面板不越出屏幕。

use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager, PhysicalPosition};

use crate::state::AppState;

pub const EDGE_MARGIN: i64 = 8;
pub const SNAP_THRESHOLD: i64 = 24;
/// 圆点窗口的逻辑尺寸(与 tauri.conf 一致;仅作回退值)
const DOT_LOGICAL: f64 = 48.0;
/// 面板窗口的逻辑尺寸(与 tauri.conf 一致;仅作回退值)
const PANEL_LOGICAL: (f64, f64) = (344.0, 452.0);

struct WinBox {
    w: i64,
    h: i64,
}

/// 读取窗口真实物理尺寸;显示器边界外仍返回实际值
fn window_box(w: &tauri::WebviewWindow) -> Option<WinBox> {
    let size = w.outer_size().ok()?;
    Some(WinBox {
        w: size.width as i64,
        h: size.height as i64,
    })
}

fn dot_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    app.get_webview_window("capsule-dot")
}

fn panel_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    app.get_webview_window("capsule-panel")
}

/// 圆点所在显示器的物理边界(x, y, w, h);失败回退主显示器
fn dot_monitor_bounds(dot: &tauri::WebviewWindow) -> (i64, i64, i64, i64) {
    let mon = dot.current_monitor().ok().flatten().or_else(|| dot.primary_monitor().ok().flatten());
    if let Some(m) = mon {
        let p = m.position();
        let s = m.size();
        (p.x as i64, p.y as i64, s.width as i64, s.height as i64)
    } else {
        (0, 0, 1920, 1080)
    }
}

/// 读取保存的停靠坐标(找不到 → None)
pub fn saved_dock(state: &Arc<AppState>) -> Option<(i64, i64)> {
    let core = state.core.blocking_lock();
    match (core.settings.dock_x, core.settings.dock_y) {
        (Some(x), Some(y)) => Some((x, y)),
        _ => None,
    }
}

/// 胶囊默认位置:主显示器右缘中部(按真实物理尺寸计算)
fn default_dock(app: &AppHandle) -> (i64, i64) {
    let Some(dot) = dot_window(app) else { return (1800, 400) };
    let (w, h) = match window_box(&dot) {
        Some(b) => (b.w, b.h),
        None => {
            let scale = dot.scale_factor().unwrap_or(1.0);
            ((DOT_LOGICAL * scale) as i64, (DOT_LOGICAL * scale) as i64)
        }
    };
    if let Ok(Some(mon)) = app.primary_monitor() {
        let size = mon.size();
        let (mw, mh) = (size.width as i64, size.height as i64);
        (mw - w - EDGE_MARGIN, mh / 2 - h / 2)
    } else {
        (1800 - w - EDGE_MARGIN, 400)
    }
}

/// 启动时定位胶囊
pub fn initial_position(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    let (x, y) = saved_dock(&state).unwrap_or_else(|| default_dock(app));
    apply_dock_position(app, x, y);
}

/// 写入 dock 位置(拖拽结束/吸附后调用;仅胶囊窗口)
pub fn apply_dock_position(app: &AppHandle, x: i64, y: i64) {
    if let Some(dot) = dot_window(app) {
        let _ = dot.set_position(PhysicalPosition::new(x as i32, y as i32));
    }
    // 持久化
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        let state = state.inner().clone();
        let mut core = state.core.blocking_lock();
        core.settings.dock_x = Some(x);
        core.settings.dock_y = Some(y);
        let _ = crate::settings::save_settings(&state.data_dir, &core.settings);
        drop(core);
    }
}

/// 拖拽落点吸附:贴近屏幕边缘(阈值 24px)则咬合,并钳制在圆点所在显示器内
#[tauri::command]
pub fn dock_snap(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    x: i64,
    y: i64,
) -> Result<(i64, i64), String> {
    let _ = state;
    let dot = dot_window(&app).ok_or("胶囊窗口不存在")?;
    let (mx, my, mw, mh) = dot_monitor_bounds(&dot);
    let (dw, dh) = match window_box(&dot) {
        Some(b) => (b.w, b.h),
        None => {
            let scale = dot.scale_factor().unwrap_or(1.0);
            ((DOT_LOGICAL * scale) as i64, (DOT_LOGICAL * scale) as i64)
        }
    };

    let mut nx = x;
    let mut ny = y;
    // 贴近左/右/上/下边缘则咬合
    if (x - mx).abs() <= SNAP_THRESHOLD {
        nx = mx;
    } else if ((mx + mw) - (x + dw)).abs() <= SNAP_THRESHOLD {
        nx = mx + mw - dw;
    }
    if (y - my).abs() <= SNAP_THRESHOLD {
        ny = my;
    } else if ((my + mh) - (y + dh)).abs() <= SNAP_THRESHOLD {
        ny = my + mh - dh;
    }
    // 边界钳制(圆点完整留在屏内)
    nx = nx.clamp(mx, (mx + mw - dw).max(mx));
    ny = ny.clamp(my, (my + mh - dh).max(my));
    apply_dock_position(&app, nx, ny);
    Ok((nx, ny))
}

/// 面板窗口锚定于胶囊停靠位置:
/// - 圆点在屏幕右半侧 → 面板向左展开(面板右缘对齐圆点右缘);
/// - 圆点在左半侧 → 向右展开;
/// - 全程按真实物理尺寸钳制在圆点所在显示器边界内,绝不越出屏幕。
/// mode: "active"(新卡到达)| "popup"(点胶囊)
pub fn show_panel(app: &AppHandle) {
    show_panel_mode(app, "active");
}

pub fn show_panel_mode(app: &AppHandle, mode: &str) {
    let Some(state) = app.try_state::<Arc<AppState>>().map(|s| s.inner().clone()) else {
        return;
    };
    let Some(dot) = dot_window(app) else { return };
    let Some(panel) = panel_window(app) else { return };

    let (dx, dy) = saved_dock(&state).unwrap_or_else(|| default_dock(app));
    let dot_box = window_box(&dot).unwrap_or(WinBox { w: 64, h: 64 });
    let (mx, my, mw, mh) = dot_monitor_bounds(&dot);

    let scale = panel.scale_factor().unwrap_or(1.0);
    let panel_box = window_box(&panel).unwrap_or(WinBox {
        w: (PANEL_LOGICAL.0 * scale) as i64,
        h: (PANEL_LOGICAL.1 * scale) as i64,
    });

    // 水平:按圆点中心在屏幕的左右半侧决定展开方向
    let dot_cx = dx + dot_box.w / 2;
    let open_left = dot_cx > mx + mw / 2;
    let mut px = if open_left {
        dx + dot_box.w - panel_box.w // 面板右缘对齐圆点右缘
    } else {
        dx + dot_box.w // 面板左缘贴圆点右缘
    };
    // 钳制:面板完整留在显示器内
    let px_min = mx + EDGE_MARGIN;
    let px_max = (mx + mw - panel_box.w - EDGE_MARGIN).max(px_min);
    px = px.clamp(px_min, px_max);

    // 垂直:面板中心对齐圆点中心,再钳制
    let mut py = dy + dot_box.h / 2 - panel_box.h / 2;
    let py_min = my + EDGE_MARGIN;
    let py_max = (my + mh - panel_box.h - EDGE_MARGIN).max(py_min);
    py = py.clamp(py_min, py_max);

    let _ = panel.set_position(PhysicalPosition::new(px as i32, py as i32));
    let _ = panel.show();
    if let Some(d) = dot_window(app) {
        let _ = d.hide();
    }
    let _ = app.emit("panel:show", serde_json::json!({ "mode": mode }));
}

/// 关闭面板(失焦自动收起):恢复小圆点
pub fn close_panel(app: &AppHandle) {
    if let Some(panel) = panel_window(app) {
        let _ = panel.hide();
    }
    if let Some(dot) = dot_window(app) {
        let _ = dot.show();
    }
    let _ = app.emit("panel:closed", serde_json::json!({}));
}

/// 常驻窗口关闭保护:点 ✕ / Alt+F4 一律转为隐藏(退出只能走托盘菜单),防止窗口被销毁后无法再开
pub fn protect_windows_from_close(app: &AppHandle) {
    for label in ["capsule-dot", "capsule-panel", "settings"] {
        let Some(w) = app.get_webview_window(label) else {
            continue;
        };
        let app = app.clone();
        let owned_label = label.to_string();
        w.on_window_event(move |e| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = e {
                api.prevent_close();
                if let Some(target) = app.get_webview_window(&owned_label) {
                    let _ = target.hide();
                }
            }
        });
    }
}

/// 隐藏胶囊(仅托盘图标)
pub fn hide_capsule(app: &AppHandle) {
    for label in ["capsule-dot", "capsule-panel"] {
        if let Some(w) = app.get_webview_window(label) {
            let _ = w.hide();
        }
    }
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        let state = state.inner().clone();
        let mut core = state.core.blocking_lock();
        core.capsule_hidden = true;
        core.settings.capsule_hidden = true;
        let _ = crate::settings::save_settings(&state.data_dir, &core.settings);
        drop(core);
    }
}

/// 恢复胶囊
pub fn show_capsule(app: &AppHandle) {
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        let state = state.inner().clone();
        let (x, y) = saved_dock(&state).unwrap_or_else(|| default_dock(app));
        apply_dock_position(app, x, y);
    }
    if let Some(dot) = dot_window(app) {
        let _ = dot.show();
    }
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        let state = state.inner().clone();
        let mut core = state.core.blocking_lock();
        core.capsule_hidden = false;
        core.settings.capsule_hidden = false;
        let _ = crate::settings::save_settings(&state.data_dir, &core.settings);
        drop(core);
    }
}

/// 设置窗口(打开即用)
pub fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.center();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// 胶囊状态上报(前端状态机:idle/active/popup)——仅记录,不控制行为
#[tauri::command]
pub fn capsule_set_state(
    state: tauri::State<'_, Arc<AppState>>,
    capsule_state: String,
) -> Result<(), String> {
    let mut core = state.core.blocking_lock();
    core.capsule_state = capsule_state;
    Ok(())
}

/// 开/关面板(供前端调用);mode: active|popup
#[tauri::command]
pub fn panel_open(app: tauri::AppHandle, mode: Option<String>) -> Result<(), String> {
    show_panel_mode(&app, mode.as_deref().unwrap_or("popup"));
    Ok(())
}

#[tauri::command]
pub fn panel_close(app: tauri::AppHandle) -> Result<(), String> {
    close_panel(&app);
    Ok(())
}

#[tauri::command]
pub fn capsule_hide(app: tauri::AppHandle) -> Result<(), String> {
    hide_capsule(&app);
    Ok(())
}

#[tauri::command]
pub fn capsule_show(app: tauri::AppHandle) -> Result<(), String> {
    show_capsule(&app);
    Ok(())
}

#[tauri::command]
pub fn settings_open(app: tauri::AppHandle) -> Result<(), String> {
    open_settings(&app);
    Ok(())
}
