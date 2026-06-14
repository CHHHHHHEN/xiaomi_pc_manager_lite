use std::sync::OnceLock;
use tauri::{AppHandle, Emitter, Manager};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, MOD_ALT, MOD_CONTROL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, WM_HOTKEY,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};

const HOTKEY_TOGGLE_BATTERY: i32 = 1;
const HOTKEY_CYCLE_PERF: i32 = 2;

static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

pub fn setup_hotkeys(app: AppHandle) {
    APP_HANDLE.set(app).ok();
    std::thread::spawn(hotkey_message_loop);
}

fn hotkey_message_loop() {
    unsafe {
        let hwnd = match crate::msg_window::create_message_window() {
            Ok(w) => w,
            Err(e) => {
                log::error!("Create hotkey window failed: {}", e);
                return;
            }
        };

        crate::msg_window::set_wndproc(hwnd, hotkey_wndproc);

        let mods = MOD_CONTROL | MOD_ALT;
        let _ = RegisterHotKey(Some(hwnd), HOTKEY_TOGGLE_BATTERY, mods, 0x42); // B
        let _ = RegisterHotKey(Some(hwnd), HOTKEY_CYCLE_PERF, mods, 0x50);     // P

        crate::msg_window::message_loop(hwnd);
    }
}

unsafe extern "system" fn hotkey_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_HOTKEY => {
            if let Some(app) = APP_HANDLE.get() {
                let id = wparam.0 as i32;
                match id {
                    HOTKEY_TOGGLE_BATTERY => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.emit("hotkey-toggle-battery-care", ());
                        }
                    }
                    HOTKEY_CYCLE_PERF => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.emit("hotkey-cycle-perf-mode", ());
                        }
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
