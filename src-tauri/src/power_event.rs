use std::sync::OnceLock;
use tauri::{AppHandle, Manager};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};

const WM_POWERBROADCAST: u32 = 0x0218u32;
const PBT_APMPOWERSTATUSCHANGE: u32 = 0x0018u32;

static POWER_APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

pub fn start_power_monitor(app: AppHandle) {
    POWER_APP_HANDLE.set(app).ok();
    std::thread::spawn(power_message_loop);
}

fn power_message_loop() {
    unsafe {
        let hwnd = match crate::msg_window::create_message_window() {
            Ok(w) => w,
            Err(e) => {
                log::error!("Create power event window failed: {}", e);
                return;
            }
        };

        crate::msg_window::set_wndproc(hwnd, power_wndproc);
        crate::msg_window::message_loop(hwnd);
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
            let battery_care = config.battery_care_enabled;
            let charge_limit = config.battery_charge_limit;
            let perf_mode = config.performance_mode;
            drop(config);

            if let Ok(backend) = state.backend.lock() {
                if let Err(e) = backend.set_battery_care(battery_care) {
                    log::warn!("re-apply battery care: {}", e);
                }
                if let Err(e) = backend.set_charge_limit(charge_limit) {
                    log::warn!("re-apply charge limit: {}", e);
                }
                if let Err(e) = backend.set_performance_mode(perf_mode) {
                    log::warn!("re-apply perf mode: {}", e);
                }
            }

            if let Some(tray) = app.try_state::<crate::tray::TrayState<tauri::Wry>>() {
                let _ = tray.battery_care.set_checked(battery_care);
            }
        }
    }
}
