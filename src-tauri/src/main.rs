//! KVBA 桌面应用入口:无主窗口,托盘 + 悬浮胶囊后台驻留
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    kvba_lib::run();
}
