use std::sync::{mpsc, OnceLock};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, MOD_CONTROL};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, WM_HOTKEY, WM_DESTROY, PostQuitMessage,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use crate::gui::app::UiCommand;

static CMD_TX: OnceLock<mpsc::Sender<UiCommand>> = OnceLock::new();

const HK_TOGGLE_BATTERY: i32 = 1;
const HK_CYCLE_PERF: i32 = 2;

pub fn setup_hotkeys(cmd_tx: mpsc::Sender<UiCommand>) {
    CMD_TX.set(cmd_tx).ok();
    std::thread::spawn(|| unsafe { hotkey_thread() });
}

unsafe fn hotkey_thread() {
    let hwnd = match crate::msg_window::create_message_window() {
        Ok(w) => w,
        Err(e) => {
            log::error!("Hotkey window: {}", e);
            return;
        }
    };
    crate::msg_window::set_wndproc(hwnd, hotkey_wndproc);

    let mods = MOD_CONTROL | MOD_ALT;
    let _ = RegisterHotKey(Some(hwnd), HK_TOGGLE_BATTERY, mods, 0x42); // B
    let _ = RegisterHotKey(Some(hwnd), HK_CYCLE_PERF, mods, 0x50);     // P

    crate::msg_window::message_loop(hwnd);
}

unsafe extern "system" fn hotkey_wndproc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_HOTKEY => {
            if let Some(tx) = CMD_TX.get() {
                let id = wparam.0 as i32;
                match id {
                    HK_TOGGLE_BATTERY => { let _ = tx.send(UiCommand::ToggleBatteryCare); }
                    HK_CYCLE_PERF => { let _ = tx.send(UiCommand::CyclePerfMode); }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => { PostQuitMessage(0); LRESULT(0) }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
