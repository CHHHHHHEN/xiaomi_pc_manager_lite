use std::sync::{Arc, Mutex, mpsc, OnceLock};
use windows::Win32::Foundation::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::UI::Shell::*;
use windows::core::PCWSTR;
use crate::gui::app::UiCommand;

static CMD_TX: OnceLock<mpsc::Sender<UiCommand>> = OnceLock::new();
pub static TRAY_STATE: OnceLock<Arc<Mutex<TrayState>>> = OnceLock::new();

pub struct TrayState {
    pub battery_care_enabled: bool,
    pub perf_mode: u8,
}

const MID_SHOW: u32 = 100;
const MID_QUIT: u32 = 101;
const WM_TRAY: u32 = WM_APP + 1;

pub fn setup_tray(cmd_tx: mpsc::Sender<UiCommand>, state: Arc<Mutex<TrayState>>) {
    CMD_TX.set(cmd_tx).ok();
    TRAY_STATE.set(state).ok();
    std::thread::spawn(|| unsafe { tray_thread() });
}

unsafe fn tray_thread() {
    let hwnd = match crate::msg_window::create_message_window() {
        Ok(w) => w,
        Err(e) => { log::error!("Tray window: {}", e); return; }
    };
    crate::msg_window::set_wndproc(hwnd, tray_wndproc);

    let icon_bytes = include_bytes!("../icons/tray_icon.ico");
    let hicon = match load_icon(icon_bytes) {
        Ok(h) => h,
        Err(e) => { log::error!("Tray icon: {}", e); return; }
    };

    let mut nid = std::mem::zeroed::<NOTIFYICONDATAW>();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = hicon;

    let tip: Vec<u16> = "Xiaomi PC Manager Lite\0".encode_utf16().collect();
    let n = tip.len().min(128);
    nid.szTip[..n].copy_from_slice(&tip[..n]);

    if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
        log::error!("NIM_ADD failed");
        let _ = DestroyIcon(hicon);
        return;
    }

    log::info!("Tray icon created");
    crate::msg_window::message_loop(hwnd);

    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    let _ = DestroyIcon(hicon);
}

unsafe extern "system" fn tray_wndproc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DESTROY => { PostQuitMessage(0); LRESULT(0) }
        WM_COMMAND => {
            let id = (wparam.0 as u32) & 0xFFFF;
            match id {
                MID_SHOW => { if let Some(tx) = CMD_TX.get() { let _ = tx.send(UiCommand::ToggleWindow); } }
                MID_QUIT => {
                    if let Some(tx) = CMD_TX.get() { let _ = tx.send(UiCommand::Quit); }
                    PostQuitMessage(0);
                }
                _ => {}
            }
            LRESULT(0)
        }
        m if m == WM_TRAY => {
            match lparam.0 as u32 {
                WM_LBUTTONUP => {
                    if let Some(tx) = CMD_TX.get() { let _ = tx.send(UiCommand::ToggleWindow); }
                }
                WM_RBUTTONUP => show_tray_menu(hwnd),
                _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let hmenu = CreatePopupMenu().unwrap_or(HMENU::default());
    let show = wstr("显示/隐藏窗口");
    let _ = AppendMenuW(hmenu, MF_STRING, MID_SHOW as usize, PCWSTR(show.as_ptr()));
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
    let quit = wstr("退出");
    let _ = AppendMenuW(hmenu, MF_STRING, MID_QUIT as usize, PCWSTR(quit.as_ptr()));

    let mut pt = POINT { x: 0, y: 0 };
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(hmenu, TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_LEFTBUTTON, pt.x, pt.y, Some(0), hwnd, None);
    let _ = DestroyMenu(hmenu);
}

unsafe fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn load_icon(bytes: &[u8]) -> Result<HICON, String> {
    if bytes.len() < 6 {
        return Err("ICO too short".into());
    }
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    if count == 0 || bytes.len() < 6 + count * 16 {
        return Err("No icon entries".into());
    }
    let e = 6;
    let off = u32::from_le_bytes([bytes[e+12], bytes[e+13], bytes[e+14], bytes[e+15]]) as usize;
    let sz = u32::from_le_bytes([bytes[e+8], bytes[e+9], bytes[e+10], bytes[e+11]]) as usize;
    if off + sz > bytes.len() { return Err("OOB".into()); }
    CreateIconFromResourceEx(&bytes[off..off+sz], true, 0x00030000, 0, 0, LR_DEFAULTCOLOR)
        .map_err(|e| format!("CreateIconFromResourceEx: {}", e))
}
