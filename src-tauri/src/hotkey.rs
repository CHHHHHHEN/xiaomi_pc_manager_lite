use std::sync::OnceLock;
use tauri::{AppHandle, Emitter, Manager};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageW, SetWindowLongPtrW, TranslateMessage,
    WINDOW_EX_STYLE, WINDOW_STYLE, MSG, GWLP_WNDPROC, WM_HOTKEY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::Foundation::{HWND, HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::core::PCWSTR;

const HOTKEY_TOGGLE_BATTERY: i32 = 1;
const HOTKEY_CYCLE_PERF: i32 = 2;
const HWND_MESSAGE: HWND = HWND(std::ptr::with_exposed_provenance_mut(-3isize as usize));

static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

pub fn setup_hotkeys(app: AppHandle) {
    APP_HANDLE.set(app).ok();
    std::thread::spawn(hotkey_message_loop);
}

fn hotkey_message_loop() {
    unsafe {
        let hinstance = HINSTANCE(GetModuleHandleW(None).unwrap().0);

        let class_name: Vec<u16> = "STATIC\0".encode_utf16().collect();

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE::default(),
            0, 0, 0, 0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance),
            None,
        );

        let hwnd = match hwnd {
            Ok(w) => w,
            Err(e) => {
                log::error!("CreateWindowExW for hotkey window failed: {}", e);
                return;
            }
        };

        SetWindowLongPtrW(hwnd, GWLP_WNDPROC, hotkey_wndproc as *const () as isize);

        let mods = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
        let _ = RegisterHotKey(Some(hwnd), HOTKEY_TOGGLE_BATTERY, mods, 0x42); // B
        let _ = RegisterHotKey(Some(hwnd), HOTKEY_CYCLE_PERF, mods, 0x50);     // P

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = DestroyWindow(hwnd);
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
