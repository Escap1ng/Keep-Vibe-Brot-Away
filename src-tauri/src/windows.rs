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
    let mon = dot
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| dot.primary_monitor().ok().flatten());
    if let Some(m) = mon {
        let p = m.position();
        let s = m.size();
        (p.x as i64, p.y as i64, s.width as i64, s.height as i64)
    } else {
        (0, 0, 1920, 1080)
    }
}

/// 圆点真实物理尺寸;取不到 outer_size 时按 DPI 换算逻辑尺寸兜底
fn dot_size(dot: &tauri::WebviewWindow) -> (i64, i64) {
    match window_box(dot) {
        Some(b) => (b.w, b.h),
        None => {
            let scale = dot.scale_factor().unwrap_or(1.0);
            ((DOT_LOGICAL * scale) as i64, (DOT_LOGICAL * scale) as i64)
        }
    }
}

/// 显示器可见工作区(物理像素)
#[derive(Clone, Copy)]
struct WorkArea {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

impl WorkArea {
    /// 圆点矩形是否完整落在本工作区内
    fn contains(&self, x: i64, y: i64, w: i64, h: i64) -> bool {
        x >= self.x && y >= self.y && x + w <= self.x + self.w && y + h <= self.y + self.h
    }

    /// 把圆点整体钳进本工作区(保证四边不出界、完整可点)
    fn clamp(&self, x: i64, y: i64, w: i64, h: i64) -> (i64, i64) {
        (
            x.clamp(self.x, (self.x + self.w - w).max(self.x)),
            y.clamp(self.y, (self.y + self.h - h).max(self.y)),
        )
    }

    /// 圆点到本工作区的最短间隙平方(用于挑"最近的一块屏")
    fn gap_sq(&self, x: i64, y: i64, w: i64, h: i64) -> i64 {
        let dx = (self.x - (x + w)).max(x - (self.x + self.w)).max(0);
        let dy = (self.y - (y + h)).max(y - (self.y + self.h)).max(0);
        dx * dx + dy * dy
    }
}

fn work_areas(app: &AppHandle) -> Vec<WorkArea> {
    let Ok(monitors) = app.available_monitors() else {
        return vec![];
    };
    monitors
        .iter()
        .map(|m| {
            let p = m.position();
            let s = m.size();
            WorkArea {
                x: p.x as i64,
                y: p.y as i64,
                w: s.width as i64,
                h: s.height as i64,
            }
        })
        .collect()
}

/// 把停靠坐标钳回可见工作区。
/// 保存的坐标可能来自已拔掉的第二块屏或改过的 DPI;一旦落在所有显示器之外,
/// 胶囊看不见就再也拖不回来 —— 所以启动/唤起路径必须自愈(纠正值由调用方回写盘)。
fn clamp_dock(app: &AppHandle, x: i64, y: i64) -> (i64, i64) {
    let areas = work_areas(app);
    // 拿不到显示器信息时原样交回,好过瞎猜一个位置
    if areas.is_empty() {
        return (x, y);
    }
    let Some(dot) = dot_window(app) else {
        return (x, y);
    };
    let (w, h) = dot_size(&dot);
    let (cx, cy) = pick_dock(&areas, x, y, w, h);
    if (cx, cy) != (x, y) {
        log::info!(
            "胶囊停靠位 ({x},{y}) 已不在任何显示器工作区内,纠正为 ({cx},{cy});当前工作区={:?}",
            areas
                .iter()
                .map(|a| (a.x, a.y, a.w, a.h))
                .collect::<Vec<_>>()
        );
    }
    (cx, cy)
}

/// 纯几何:已完整可见则原样返回;否则整体钳进离它最近的那块工作区
fn pick_dock(areas: &[WorkArea], x: i64, y: i64, w: i64, h: i64) -> (i64, i64) {
    let Some(target) = areas
        .iter()
        .find(|a| a.contains(x, y, w, h))
        .or_else(|| areas.iter().min_by_key(|a| a.gap_sq(x, y, w, h)))
    else {
        return (x, y);
    };
    target.clamp(x, y, w, h)
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
    let Some(dot) = dot_window(app) else {
        return (1800, 400);
    };
    let (w, h) = dot_size(&dot);
    if let Ok(Some(mon)) = app.primary_monitor() {
        let size = mon.size();
        let (mw, mh) = (size.width as i64, size.height as i64);
        (mw - w - EDGE_MARGIN, mh / 2 - h / 2)
    } else {
        (1800 - w - EDGE_MARGIN, 400)
    }
}

/// 启动时定位胶囊(越界的持久化坐标会被 clamp_dock 纠正,并由 apply_dock_position 回写)
pub fn initial_position(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    let (x, y) = saved_dock(&state).unwrap_or_else(|| default_dock(app));
    let (x, y) = clamp_dock(app, x, y);
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
pub fn dock_snap(app: tauri::AppHandle, x: i64, y: i64) -> Result<(i64, i64), String> {
    let dot = dot_window(&app).ok_or("胶囊窗口不存在")?;
    let (mx, my, mw, mh) = dot_monitor_bounds(&dot);
    let (dw, dh) = dot_size(&dot);

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
///
/// - 圆点在屏幕右半侧 → 面板向左展开(面板右缘对齐圆点右缘);
/// - 圆点在左半侧 → 向右展开;
/// - 全程按真实物理尺寸钳制在圆点所在显示器边界内,绝不越出屏幕。
///
/// mode: "active"(新卡到达)| "popup"(点胶囊)
pub fn show_panel(app: &AppHandle) {
    show_panel_mode(app, "active");
}

pub fn show_panel_mode(app: &AppHandle, mode: &str) {
    let Some(state) = app.try_state::<Arc<AppState>>().map(|s| s.inner().clone()) else {
        return;
    };
    let Some(dot) = dot_window(app) else { return };
    let Some(panel) = panel_window(app) else {
        return;
    };

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
    let _ = dot.hide();
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

/// 记录胶囊显隐意图(运行期标志 + 设置项);启动时始终复位为显示
fn set_capsule_hidden(app: &AppHandle, hidden: bool) {
    let Some(state) = app.try_state::<Arc<AppState>>().map(|s| s.inner().clone()) else {
        return;
    };
    let mut core = state.core.blocking_lock();
    core.capsule_hidden = hidden;
    core.settings.capsule_hidden = hidden;
    let _ = crate::settings::save_settings(&state.data_dir, &core.settings);
}

/// 隐藏胶囊(仅托盘图标)
pub fn hide_capsule(app: &AppHandle) {
    for label in ["capsule-dot", "capsule-panel"] {
        if let Some(w) = app.get_webview_window(label) {
            let _ = w.hide();
        }
    }
    set_capsule_hidden(app, true);
}

/// 恢复胶囊(回到上次停靠位置;越界坐标同样会被纠正)
pub fn show_capsule(app: &AppHandle) {
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        let state = state.inner().clone();
        let (x, y) = saved_dock(&state).unwrap_or_else(|| default_dock(app));
        let (x, y) = clamp_dock(app, x, y);
        apply_dock_position(app, x, y);
    }
    if let Some(dot) = dot_window(app) {
        let _ = dot.show();
    }
    set_capsule_hidden(app, false);
}

/// 设置窗口(打开即用)
pub fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.center();
        let _ = w.show();
        let _ = w.set_focus();
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn area(x: i64, y: i64, w: i64, h: i64) -> WorkArea {
        WorkArea { x, y, w, h }
    }

    /// 现场复现:拔屏后遗留的 dockX=2358 落在 1707 宽的单屏之外
    #[test]
    fn off_screen_dock_is_pulled_back_inside_the_monitor() {
        let areas = [area(0, 0, 1707, 1067)];
        let (x, y) = pick_dock(&areas, 2358, 860, 72, 72);
        assert_eq!(x, 1707 - 72, "应贴到右缘内侧,保证圆点完整可见");
        assert_eq!(y, 860, "纵向本就合规,不得被改动");
    }

    #[test]
    fn visible_dock_is_left_untouched() {
        let areas = [area(0, 0, 1920, 1080)];
        assert_eq!(pick_dock(&areas, 1848, 512, 48, 48), (1848, 512));
    }

    #[test]
    fn negative_and_over_bottom_coords_are_clamped() {
        let areas = [area(0, 0, 1920, 1080)];
        assert_eq!(pick_dock(&areas, -400, -20, 48, 48), (0, 0));
        assert_eq!(pick_dock(&areas, 900, 5000, 48, 48), (900, 1080 - 48));
    }

    /// 多屏时回到"最近的那块",而不是随机挑一块(副屏在负 X 方向很常见)
    #[test]
    fn nearest_monitor_wins_on_multi_monitor_rigs() {
        let areas = [area(0, 0, 1920, 1080), area(-1920, 0, 1920, 1200)];
        let (x, y) = pick_dock(&areas, -3000, 700, 48, 48);
        assert_eq!((x, y), (-1920, 700), "应吸附到副屏左缘");
        let (x2, y2) = pick_dock(&areas, 4000, 700, 48, 48);
        assert_eq!((x2, y2), (1920 - 48, 700), "主屏侧出界则吸附主屏右缘");
    }

    /// 拔屏 + 高 DPI:圆点物理尺寸可能大于新工作区,仍须给出可见(左上对齐)结果而非 panic
    #[test]
    fn dot_larger_than_work_area_does_not_panic_or_disappear() {
        let areas = [area(0, 0, 640, 480)];
        let (x, y) = pick_dock(&areas, 5000, 5000, 800, 800);
        assert_eq!((x, y), (0, 0));
    }

    #[test]
    fn empty_monitor_list_leaves_coordinate_alone() {
        assert_eq!(pick_dock(&[], 2358, 860, 72, 72), (2358, 860));
    }
}
