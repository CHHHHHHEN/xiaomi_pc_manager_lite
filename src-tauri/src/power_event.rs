use std::sync::{mpsc, OnceLock};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, WM_DESTROY, PostQuitMessage,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use crate::gui::app::UiCommand;

static CMD_TX: OnceLock<mpsc::Sender<UiCommand>> = OnceLock::new();

const WM_POWERBROADCAST: u32 = 0x0218;
const PBT_APMPOWERSTATUSCHANGE: u32 = 0x0018;

pub fn start_power_monitor(cmd_tx: mpsc::Sender<UiCommand>) {
    CMD_TX.set(cmd_tx).ok();
    std::thread::spawn(|| unsafe { power_thread() });
}

unsafe fn power_thread() {
    let hwnd = match crate::msg_window::create_message_window() {
        Ok(w) => w,
        Err(e) => {
            log::error!("Power event window: {}", e);
            return;
        }
    };
    crate::msg_window::set_wndproc(hwnd, power_wndproc);
    crate::msg_window::message_loop(hwnd);
}

unsafe extern "system" fn power_wndproc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_POWERBROADCAST => {
            if wparam.0 == PBT_APMPOWERSTATUSCHANGE as usize {
                log::info!("Power status changed");
                if let Some(tx) = CMD_TX.get() {
                    let _ = tx.send(UiCommand::ReapplyConfig);
                }
            }
            LRESULT(1)
        }
        WM_DESTROY => { PostQuitMessage(0); LRESULT(0) }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
