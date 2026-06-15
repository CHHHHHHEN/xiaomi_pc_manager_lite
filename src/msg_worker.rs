//! Single-threaded message pump for tray icon, global hotkeys and power events.
//!
//! Windows requires a `HWND` (or thread message queue) to receive
//! `WM_TRAY`/`WM_HOTKEY`/`WM_POWERBROADCAST` messages. This module
//! creates a single message-only window and runs one message loop,
//! dispatching the three message families to their respective handlers.

use std::sync::{mpsc, Arc, Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, MOD_CONTROL};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIM_ADD, NIF_ICON, NIF_MESSAGE, NIF_TIP, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, DefWindowProcW, DestroyIcon,
    DestroyMenu, GetCursorPos, PostQuitMessage, SetForegroundWindow, TrackPopupMenu,
    WM_APP, WM_COMMAND, WM_DESTROY, WM_HOTKEY, WM_LBUTTONUP, WM_RBUTTONUP, MF_SEPARATOR,
    MF_STRING, LR_DEFAULTCOLOR, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_LEFTBUTTON,
};
use windows::core::PCWSTR;

use crate::gui::app::UiCommand;
use crate::msg_window;

const WM_TRAY: u32 = WM_APP + 1;
const WM_POWERBROADCAST: u32 = 0x0218;
const PBT_APMPOWERSTATUSCHANGE: u32 = 0x0018;

const MID_SHOW: u32 = 100;
const MID_QUIT: u32 = 101;

const HK_TOGGLE_BATTERY: i32 = 1;
const HK_CYCLE_PERF: i32 = 2;

pub struct TrayState {
    pub battery_care_enabled: bool,
    pub perf_mode: u8,
}

static CMD_TX: OnceLock<mpsc::Sender<UiCommand>> = OnceLock::new();
pub static TRAY_STATE: OnceLock<Arc<Mutex<TrayState>>> = OnceLock::new();

/// Spawn the single background thread that hosts the tray icon,
/// global hotkeys, and power-event listener.
pub fn spawn(cmd_tx: mpsc::Sender<UiCommand>, state: Arc<Mutex<TrayState>>) {
    CMD_TX.set(cmd_tx).ok();
    TRAY_STATE.set(state).ok();
    std::thread::spawn(worker_thread);
}

fn worker_thread() {
    let hwnd = match msg_window::create_message_window() {
        Ok(w) => w,
        Err(e) => {
            log::error!("Message worker window: {}", e);
            return;
        }
    };
    msg_window::set_wndproc(hwnd, wndproc);

    if let Err(e) = register_tray_icon(hwnd) {
        log::error!("Tray icon: {}", e);
        msg_window::message_loop(hwnd);
        return;
    }

    let mods = MOD_CONTROL | MOD_ALT;
    if let Err(e) = unsafe { RegisterHotKey(Some(hwnd), HK_TOGGLE_BATTERY, mods, 0x42) } {
        log::error!("Register hotkey (B): {:?}", e);
    }
    if let Err(e) = unsafe { RegisterHotKey(Some(hwnd), HK_CYCLE_PERF, mods, 0x50) } {
        log::error!("Register hotkey (P): {:?}", e);
    }

    msg_window::message_loop(hwnd);
    // Tray icon and hotkeys are torn down with the window.
}

unsafe extern "system" fn wndproc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_COMMAND => handle_menu_command(wparam, lparam),
        WM_HOTKEY => handle_hotkey(wparam),
        WM_POWERBROADCAST => handle_power_broadcast(wparam, lparam),
        m if m == WM_TRAY => handle_tray_event(hwnd, lparam),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn handle_menu_command(wparam: WPARAM, _lparam: LPARAM) -> LRESULT {
    let id = (wparam.0 as u32) & 0xFFFF;
    if let Some(tx) = CMD_TX.get() {
        match id {
            MID_SHOW => {                 let _ = tx.send(UiCommand::ToggleWindow); }
            MID_QUIT => {
                let _ = tx.send(UiCommand::Quit);
                unsafe { PostQuitMessage(0); }
            }
            _ => {}
        }
    }
    LRESULT(0)
}

fn handle_hotkey(wparam: WPARAM) -> LRESULT {
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

fn handle_power_broadcast(wparam: WPARAM, _lparam: LPARAM) -> LRESULT {
    if wparam.0 == PBT_APMPOWERSTATUSCHANGE as usize {
        log::info!("Power status changed");
        if let Some(tx) = CMD_TX.get() {
            let _ = tx.send(UiCommand::ReapplyConfig);
        }
    }
    LRESULT(1)
}

fn handle_tray_event(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    match lparam.0 as u32 {
        WM_LBUTTONUP => {
            if let Some(tx) = CMD_TX.get() {
                let _ = tx.send(UiCommand::ToggleWindow);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            show_tray_menu(hwnd);
            LRESULT(0)
        }
        _ => LRESULT(0),
    }
}

fn register_tray_icon(hwnd: HWND) -> Result<(), String> {
    let icon_bytes = include_bytes!("../icons/tray_icon.ico");
    let hicon = load_icon(icon_bytes)?;

    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = hicon;

    let tip: Vec<u16> = "Xiaomi PC Manager Lite\0".encode_utf16().collect();
    let n = tip.len().min(128);
    nid.szTip[..n].copy_from_slice(&tip[..n]);

    if !unsafe { Shell_NotifyIconW(NIM_ADD, &nid).as_bool() } {
        let _ = unsafe { DestroyIcon(hicon) };
        return Err("NIM_ADD failed".into());
    }
    log::info!("Tray icon created");
    Ok(())
}

fn show_tray_menu(hwnd: HWND) {
    let hmenu = unsafe { CreatePopupMenu().unwrap_or_default() };
    let show = wstr("显示/隐藏窗口");
    let _ = unsafe { AppendMenuW(hmenu, MF_STRING, MID_SHOW as usize, PCWSTR(show.as_ptr())) };
    let _ = unsafe { AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null()) };
    let quit = wstr("退出");
    let _ = unsafe { AppendMenuW(hmenu, MF_STRING, MID_QUIT as usize, PCWSTR(quit.as_ptr())) };

    let mut pt = POINT { x: 0, y: 0 };
    let _ = unsafe { GetCursorPos(&mut pt) };
    let _ = unsafe { SetForegroundWindow(hwnd) };
    let _ = unsafe {
        TrackPopupMenu(
            hmenu,
            TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_LEFTBUTTON,
            pt.x,
            pt.y,
            Some(0),
            hwnd,
            None,
        )
    };
    let _ = unsafe { DestroyMenu(hmenu) };
}

fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn load_icon(bytes: &[u8]) -> Result<windows::Win32::UI::WindowsAndMessaging::HICON, String> {
    if bytes.len() < 6 {
        return Err("ICO too short".into());
    }
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    if count == 0 || bytes.len() < 6 + count * 16 {
        return Err("No icon entries".into());
    }
    let e = 6;
    let off = u32::from_le_bytes([bytes[e + 12], bytes[e + 13], bytes[e + 14], bytes[e + 15]]) as usize;
    let sz = u32::from_le_bytes([bytes[e + 8], bytes[e + 9], bytes[e + 10], bytes[e + 11]]) as usize;
    if off + sz > bytes.len() {
        return Err("OOB".into());
    }
    unsafe {
        CreateIconFromResourceEx(&bytes[off..off + sz], true, 0x00030000, 0, 0, LR_DEFAULTCOLOR)
            .map_err(|e| format!("CreateIconFromResourceEx: {}", e))
    }
}
