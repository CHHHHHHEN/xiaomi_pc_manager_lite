//! 主窗口（eframe 窗口）的显示控制。
//!
//! 托盘驻留使用 `ShowWindow(SW_HIDE)` **隐藏**而非最小化：winit 不知道
//! 窗口被隐藏，仍正常投递重绘事件，`update()` 与托盘命令处理保持运行；
//! 而 `ViewportCommand::Minimized` 会在任务栏保留图标，不符合
//! "最小化到托盘后任务栏不再显示程序图标"的需求。
//!
//! 注意：不能用 `ViewportCommand::Visible(false)`——eframe/winit 在窗口
//! 隐藏后不再投递 RedrawRequested（已实测），`update()` 永久停止，
//! 托盘命令无法处理，应用变僵尸进程。

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, IsWindowVisible, SetForegroundWindow, ShowWindow, SW_HIDE, SW_RESTORE,
    SW_SHOW,
};

use crate::util::to_pcwstr;

/// eframe 窗口标题（`eframe::run_native` 的第一个参数）。
pub const MAIN_WINDOW_TITLE: &str = "Xiaomi PC Manager Lite";

fn find_main_window() -> Option<HWND> {
    let (_buf, title) = to_pcwstr(MAIN_WINDOW_TITLE);
    let hwnd = unsafe { FindWindowW(None, title) }.ok()?;
    if hwnd.0.is_null() {
        None
    } else {
        Some(hwnd)
    }
}

/// 供托盘层直接操作主窗口（窗口隐藏后 GUI update 循环停止，
/// 托盘必须自给自足地隐藏/显示/退出窗口）。
pub(crate) fn find_main_window_handle() -> Option<HWND> {
    find_main_window()
}

/// 主窗口当前是否可见（任务栏图标随可见性出现/消失）。
pub fn main_window_visible() -> bool {
    find_main_window()
        .map(|hwnd| unsafe { IsWindowVisible(hwnd).as_bool() })
        .unwrap_or(false)
}

/// 隐藏主窗口（任务栏图标随之消失，仅驻留托盘）。
pub fn hide_main_window() {
    if let Some(hwnd) = find_main_window() {
        log::info!("Hide main window 0x{:X}", hwnd.0 as usize);
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    } else {
        log::warn!("Hide main window: not found");
    }
}

/// 显示并激活主窗口。
pub fn show_main_window() {
    if let Some(hwnd) = find_main_window() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = ShowWindow(hwnd, SW_SHOW);
            // 托盘点击触发，属于用户交互上下文，一般可成功置前。
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_main_window_title_is_stable() {
        assert_eq!(MAIN_WINDOW_TITLE, "Xiaomi PC Manager Lite");
    }
}
