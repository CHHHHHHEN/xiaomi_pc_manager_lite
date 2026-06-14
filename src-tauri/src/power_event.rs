use std::sync::OnceLock;
use tauri::{AppHandle, Manager};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageW, SetWindowLongPtrW, TranslateMessage,
    WINDOW_EX_STYLE, WINDOW_STYLE, MSG, GWLP_WNDPROC,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;

const PBT_APMPOWERSTATUSCHANGE: u32 = 0x0018u32;
use windows::Win32::Foundation::{HWND, HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::core::PCWSTR;

const WM_POWERBROADCAST: u32 = 0x0218u32;
const HWND_MESSAGE: HWND = HWND(std::ptr::with_exposed_provenance_mut(-3isize as usize));

static POWER_APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

pub fn start_power_monitor(app: AppHandle) {
    POWER_APP_HANDLE.set(app).ok();
    std::thread::spawn(power_message_loop);
}

fn power_message_loop() {
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
                log::error!("CreateWindowExW for power event window failed: {}", e);
                return;
            }
        };

        SetWindowLongPtrW(hwnd, GWLP_WNDPROC, power_wndproc as *const () as isize);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = DestroyWindow(hwnd);
    }
}

unsafe extern "system" fn power_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_POWERBROADCAST => {
            if wparam.0 == PBT_APMPOWERSTATUSCHANGE as usize {
                if let Some(app) = POWER_APP_HANDLE.get() {
                    reapply_config_on_power_change(app);
                }
            }
            LRESULT(1)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn reapply_config_on_power_change(app: &AppHandle) {
    log::info!("Power status changed, reapplying config if enabled");

    if let Some(state) = app.try_state::<crate::AppState>() {
        if let Ok(config) = state.config.lock() {
            if !config.auto_reapply_on_power_change {
                return;
            }
            if let Ok(backend) = state.backend.lock() {
                if let Err(e) = backend.set_battery_care(config.battery_care_enabled) {
                    log::warn!("re-apply battery care: {}", e);
                }
                if let Err(e) = backend.set_charge_limit(config.battery_charge_limit) {
                    log::warn!("re-apply charge limit: {}", e);
                }
                if let Err(e) = backend.set_performance_mode(config.performance_mode) {
                    log::warn!("re-apply perf mode: {}", e);
                }
            }

            if let Some(tray) = app.try_state::<crate::tray::TrayState<tauri::Wry>>() {
                let _ = tray.battery_care.set_checked(config.battery_care_enabled);
            }
        }
    }
}
