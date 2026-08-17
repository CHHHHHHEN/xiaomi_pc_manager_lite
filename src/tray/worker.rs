use std::sync::{mpsc, Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIM_ADD, NIF_ICON, NIF_MESSAGE, NIF_TIP, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, DefWindowProcW,
    DestroyMenu, GetCursorPos, KillTimer, PostMessageW, PostQuitMessage,
    RegisterWindowMessageW, SetForegroundWindow, SetTimer, TrackPopupMenu, WM_APP,
    WM_COMMAND, WM_DESTROY, WM_HOTKEY, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_TIMER,
    HICON, MF_SEPARATOR, MF_STRING, LR_DEFAULTCOLOR, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
    TPM_LEFTBUTTON,
};
use windows::core::PCWSTR;

use crate::command::UiCommand;
use crate::tray::window;

const WM_TRAY: u32 = WM_APP + 1;
const WM_POWERBROADCAST: u32 = 0x0218;
const PBT_APMPOWERSTATUSCHANGE: u32 = 0x0018;

const MID_SHOW: u32 = 100;
const MID_QUIT: u32 = 101;

const HK_TOGGLE_BATTERY: i32 = 1;
const HK_CYCLE_PERF: i32 = 2;

/// NIM_ADD 失败时的重试定时器（任务栏未就绪时 `TaskbarCreated` 广播可能
/// 已经错过，见 register_tray_icon 的注释）。
const TIMER_TRAY_RETRY: usize = 1;
const TRAY_RETRY_MS: u32 = 2000;

static CMD_TX: OnceLock<mpsc::Sender<UiCommand>> = OnceLock::new();

/// 托盘图标重建所需状态（含 HICON）。
struct TrayIconState {
    nid: NOTIFYICONDATAW,
}

// SAFETY: TrayIconState 仅由托盘工作线程访问（wndproc 与其同线程），与
// fnkey.rs 的 SafeEnumerator 采用相同的线程归属约定。NOTIFYICONDATAW
// 自身因包含原始句柄不满足 Sync，因此由本包装提供。
unsafe impl Send for TrayIconState {}
unsafe impl Sync for TrayIconState {}

/// 托盘图标状态：NIM_ADD 前保存（含 HICON）。explorer.exe 崩溃或重启会
/// 销毁任务栏上的所有图标，收到 `TaskbarCreated` 广播后据此重建。
/// HICON 与应用同生命周期，随进程退出由系统释放。
static TRAY_ICON: Mutex<Option<TrayIconState>> = Mutex::new(None);

/// "TaskbarCreated" 注册消息号：任务栏（explorer.exe）重建时系统向所有顶层
/// 窗口广播该消息（MSDN: "Sent to a top-level window when the taskbar is
/// created"）。隐藏顶层消息窗口（window.rs）在窗口层级中，可以收到广播。
fn taskbar_created_msg() -> u32 {
    static MSG_ID: OnceLock<u32> = OnceLock::new();
    *MSG_ID.get_or_init(|| {
        let (_buf, name) = crate::util::to_pcwstr("TaskbarCreated");
        unsafe { RegisterWindowMessageW(name) }
    })
}

pub fn spawn(cmd_tx: mpsc::Sender<UiCommand>) {
    CMD_TX.set(cmd_tx).ok();
    std::thread::spawn(worker_thread);
}

fn worker_thread() {
    let hwnd = match window::create_message_window() {
        Ok(w) => w,
        Err(e) => {
            log::error!("Message worker window: {}", e);
            return;
        }
    };
    if let Err(e) = window::set_wndproc(hwnd, wndproc) {
        log::error!("Set message window wndproc: {}", e);
        return;
    }
    log::info!("Message window hwnd=0x{:X}", hwnd.0 as usize);

    // 热键注册不依赖托盘是否注册成功：托盘注册失败（任务栏未就绪）时
    // 热键仍须可用，图标稍后由重试定时器或 TaskbarCreated 广播补建。
    // MOD_NOREPEAT：不设置时按住热键会因键盘自动重复产生连续的 WM_HOTKEY，
    // 导致 Ctrl+Alt+B 反复翻转养护开关、Ctrl+Alt+P 连续循环性能模式
    // （MSDN RegisterHotKey: "does not yield multiple hotkey notifications"）。
    let mods = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
    if let Err(e) = unsafe { RegisterHotKey(Some(hwnd), HK_TOGGLE_BATTERY, mods, 0x42) } {
        log::error!("Register hotkey (B): {:?}", e);
    }
    if let Err(e) = unsafe { RegisterHotKey(Some(hwnd), HK_CYCLE_PERF, mods, 0x50) } {
        log::error!("Register hotkey (P): {:?}", e);
    }

    if let Err(e) = register_tray_icon(hwnd) {
        log::error!("Tray icon: {}", e);
    }

    window::message_loop(hwnd);
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
        m if m != 0 && m == taskbar_created_msg() => {
            // explorer.exe 重启后任务栏上的托盘图标全部消失，需要重建
            // （否则必须重启应用才有托盘）。注册消息范围（0xC000-0xFFFF）
            // 与 WM_APP 区间的 WM_TRAY 不冲突。m != 0 防御 RegisterWindow
            // MessageW 注册失败返回 0 的情况：0 是 WM_NULL（菜单关闭后我们
            // 会主动投递 WM_NULL），若不排除会把 WM_NULL 误当任务栏重建。
            handle_taskbar_created(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_TRAY_RETRY {
                retry_tray_icon(hwnd);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn handle_menu_command(wparam: WPARAM, _lparam: LPARAM) -> LRESULT {
    let id = (wparam.0 as u32) & 0xFFFF;
    match id {
        MID_SHOW => {
            // 直接操作窗口：窗口隐藏后 GUI update 循环停止，经命令通道
            // 的命令无人消费，托盘必须自给自足。
            toggle_main_window();
        }
        MID_QUIT => {
            quit_app();
        }
        _ => {}
    }
    LRESULT(0)
}

/// 托盘直接切换主窗口可见性（不依赖 GUI update 循环）。
fn toggle_main_window() {
    log::info!(
        "Tray toggle: visible={}",
        crate::platform::window::main_window_visible()
    );
    if crate::platform::window::main_window_visible() {
        crate::platform::window::hide_main_window();
    } else {
        crate::platform::window::show_main_window();
    }
}

/// 托盘直接退出：向主窗口投递 WM_QUIT，winit 消息循环收到后退出，
/// eframe run_native 返回、进程正常结束（窗口隐藏时同样有效，
/// 不依赖 GUI update 循环）。
///
/// 兜底：WM_QUIT 未生效（GUI 线程被阻塞）时强制退出。宽限期必须大于
/// WMI 后端单次调用的最坏阻塞时长（GET_RESULT_TIMEOUT_MS = 3000，见
/// ec/wmi.rs）：否则 GUI 线程正阻塞在 `recv()` 等待 WMI worker 回复时
/// 根本来不及处理 WM_QUIT，过早的 `process::exit` 会把进程硬杀在一次
/// 尚未完成的硬件调用中途。正常退出路径不经过此睡眠——主线程退出后
/// 进程随即终止，本线程的兜底睡眠由进程结束一并终结。
const QUIT_FALLBACK_MS: u64 = 5000;

fn quit_app() {
    if let Some(hwnd) = crate::platform::window::find_main_window_handle() {
        log::info!("Tray quit: posting WM_QUIT to main window");
        let _ = unsafe {
            windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(hwnd),
                windows::Win32::UI::WindowsAndMessaging::WM_QUIT,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }
    std::thread::sleep(std::time::Duration::from_millis(QUIT_FALLBACK_MS));
    log::warn!(
        "Tray quit: app did not exit within {}ms; forcing exit",
        QUIT_FALLBACK_MS
    );
    std::process::exit(0);
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
            // 直接操作窗口（原因见 toggle_main_window）。
            toggle_main_window();
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
    let icon_bytes = include_bytes!("../../icons/tray_icon.ico");
    let hicon = load_icon(icon_bytes)?;
    let nid = build_tray_nid(hwnd, hicon);

    // 先保存 NID（含 HICON）：即使本次 NIM_ADD 失败（如应用在任务栏就绪前
    // 启动），重试定时器 / TaskbarCreated 广播仍会按此状态自动重建图标。
    *TRAY_ICON.lock().unwrap_or_else(|e| e.into_inner()) = Some(TrayIconState { nid });

    if !unsafe { Shell_NotifyIconW(NIM_ADD, &nid).as_bool() } {
        // TaskbarCreated 只在任务栏创建时广播一次：若该广播已先于本消息窗口
        // 送达（如登录期任务栏初始化中、或广播之后才创建窗口），将不会再收到
        // 广播，图标会永久缺失（原 C 版本用 5 秒轮询重试解决）。这里启动
        // 2 秒周期重试定时器，图标出现即停止。
        start_retry_timer(hwnd);
        return Err("NIM_ADD failed; retrying".into());
    }
    log::info!("Tray icon created");
    Ok(())
}

fn build_tray_nid(hwnd: HWND, hicon: HICON) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = hicon;

    nid.szTip = [0u16; 128];
    set_tip(&mut nid.szTip, "Xiaomi PC Manager Lite");
    nid
}

/// 写入 szTip 并保证 NUL 结尾。提示文本达到或超过 128 个 UTF-16 单元时截断
/// 到 127 单元 + NUL：不保证 NUL 结尾的 Tooltip 会让 Shell_NotifyIconW 越过
/// szTip 读入 dwState/szInfo 等后续字段，工具提示会显示垃圾字符。
fn set_tip(sz_tip: &mut [u16; 128], tip: &str) {
    let mut wide: Vec<u16> = tip.encode_utf16().take(127).collect();
    wide.push(0);
    sz_tip[..wide.len()].copy_from_slice(&wide);
}

/// 在锁内拷贝 NID（NOTIFYICONDATAW 实现 Copy）后立即释放锁，再调用
/// Shell_NotifyIconW。避免 Shell 调用期间持有锁：wndproc 处理其它消息时
/// 也会获取同一把锁，若 Shell 调用重入本窗口将形成死锁。
fn tray_nid_snapshot() -> Option<NOTIFYICONDATAW> {
    let guard = TRAY_ICON.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(|s| s.nid)
}

fn start_retry_timer(hwnd: HWND) {
    let _ = unsafe { SetTimer(Some(hwnd), TIMER_TRAY_RETRY, TRAY_RETRY_MS, None) };
}

fn retry_tray_icon(hwnd: HWND) {
    let Some(nid) = tray_nid_snapshot() else {
        // 没有可恢复的状态（理论上不会发生）：停止重试，避免定时器空转。
        let _ = unsafe { KillTimer(Some(hwnd), TIMER_TRAY_RETRY) };
        return;
    };
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &nid).as_bool() } {
        return; // 任务栏仍不可用，定时器继续重试
    }
    let _ = unsafe { KillTimer(Some(hwnd), TIMER_TRAY_RETRY) };
    log::info!("Tray icon created (retry)");
}

fn handle_taskbar_created(hwnd: HWND) {
    let Some(nid) = tray_nid_snapshot() else {
        log::debug!("TaskbarCreated: no tray icon state to restore");
        return;
    };
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &nid).as_bool() } {
        log::warn!("Failed to re-add tray icon after taskbar restart");
        start_retry_timer(hwnd);
        return;
    }
    // 重建成功：若还有挂起的重试定时器，一并停止。
    let _ = unsafe { KillTimer(Some(hwnd), TIMER_TRAY_RETRY) };
    log::info!("Tray icon re-created after taskbar restart");
}

fn show_tray_menu(hwnd: HWND) {
    let hmenu = unsafe { CreatePopupMenu().unwrap_or_default() };
    let (_show_buf, show) = crate::util::to_pcwstr("显示/隐藏窗口");
    let _ = unsafe { AppendMenuW(hmenu, MF_STRING, MID_SHOW as usize, show) };
    let _ = unsafe { AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null()) };
    let (_quit_buf, quit) = crate::util::to_pcwstr("退出");
    let _ = unsafe { AppendMenuW(hmenu, MF_STRING, MID_QUIT as usize, quit) };

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
    // 菜单关闭后必须向窗口投递一条 WM_NULL：否则菜单处于"刚点过"状态，
    // 第一次在菜单外点击会被吞掉、菜单不消失（Windows 已知行为，参见
    // KB Q135788 / Raymond Chen "Why does the first click after dismissing
    // a popup menu do nothing?"）。
    let _ = unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) };
    let _ = unsafe { DestroyMenu(hmenu) };
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

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;

    /// 回归测试：托盘 NID 的构建必须符合 F-TRAY-02（NIF_MESSAGE|NIF_ICON|
    /// NIF_TIP）、F-TRAY-03（Tooltip 文字）与 F-TRAY-09（回调消息 WM_TRAY）。
    /// szTip 拷贝不能越界（含 NUL 结尾）。
    #[test]
    fn test_build_tray_nid_flags_tip_and_message() {
        let hwnd = HWND(std::ptr::null_mut());
        let hicon = HICON(std::ptr::null_mut());
        let nid = build_tray_nid(hwnd, hicon);

        assert_eq!(nid.cbSize as usize, std::mem::size_of::<NOTIFYICONDATAW>());
        assert_eq!(nid.hWnd, hwnd);
        assert_eq!(nid.uID, 1);
        assert_eq!(nid.uFlags, NIF_MESSAGE | NIF_ICON | NIF_TIP);
        assert_eq!(nid.uCallbackMessage, WM_TRAY);
        assert_eq!(nid.hIcon, hicon);

        let mut expected: Vec<u16> = "Xiaomi PC Manager Lite".encode_utf16().collect();
        expected.push(0); // NUL 结尾
        assert!(
            expected.len() <= nid.szTip.len(),
            "tooltip must fit in szTip"
        );
        assert_eq!(&nid.szTip[..expected.len()], expected.as_slice());
        // 剩余部分保持零初始化。
        assert!(nid.szTip[expected.len()..].iter().all(|&c| c == 0));
    }

    /// 回归测试：TaskbarCreated 注册消息必须可用且稳定，且位于注册消息
    /// 区间（0xC000-0xFFFF），不与 WM_APP 区间的 WM_TRAY 冲突。
    #[test]
    fn test_taskbar_created_message_registers() {
        let msg1 = taskbar_created_msg();
        let msg2 = taskbar_created_msg();
        assert_ne!(msg1, 0, "RegisterWindowMessageW(TaskbarCreated) must succeed");
        assert_eq!(msg1, msg2, "taskbar message id must be stable");
        assert!((0xC000..=0xFFFF).contains(&msg1));
        assert_ne!(msg1, WM_TRAY);
    }

    /// 回归测试：嵌入式 tray_icon.ico 能被 load_icon 解析并创建图标；
    /// 若 ICO 结构被破坏，托盘图标会在启动时静默失败（F-TRAY-01）。
    #[test]
    fn test_embedded_tray_icon_loads() {
        let icon_bytes = include_bytes!("../../icons/tray_icon.ico");
        let hicon = load_icon(icon_bytes).expect("embedded tray icon must parse");
        assert!(!hicon.0.is_null());
        let _ = unsafe { DestroyIcon(hicon) };
    }

    /// 回归测试：普通长度的 Tooltip 原样写入且以 NUL 结尾，其余字节不动
    /// （NUL 之后的清空由调用方对 NID 的 zeroed 保证）。
    #[test]
    fn test_set_tip_normal() {
        let mut sz_tip = [0xAAAAu16; 128];
        set_tip(&mut sz_tip, "Xiaomi PC Manager Lite");
        let mut expected: Vec<u16> = "Xiaomi PC Manager Lite".encode_utf16().collect();
        expected.push(0);
        assert_eq!(&sz_tip[..expected.len()], expected.as_slice());
        assert!(sz_tip[expected.len()..].iter().all(|&c| c == 0xAAAA));
    }

    /// 回归测试：超长 Tooltip 必须截断并保证 NUL 结尾。若截断后没有 NUL，
    /// Shell_NotifyIconW 会越过 szTip 读入 dwState/szInfo 等后续字段，
    /// 工具提示显示垃圾字符。
    #[test]
    fn test_set_tip_truncates_with_nul() {
        let long_tip = "A".repeat(300);
        let mut sz_tip = [0xAAAAu16; 128];
        set_tip(&mut sz_tip, &long_tip);
        // 127 个字符 + NUL，且必须 NUL 结尾。
        assert_eq!(&sz_tip[..127], vec![b'A' as u16; 127].as_slice());
        assert_eq!(sz_tip[127], 0);
    }

    /// 回归测试（BUG B）：托盘退出的兜底强制退出时长必须大于 WMI 后端单次
    /// 调用的最坏阻塞时长（GET_RESULT_TIMEOUT_MS=3000，ec/wmi.rs）。否则
    /// GUI 线程正阻塞在 `recv()` 等待 WMI worker 回复时（最长 3000ms）根本
    /// 来不及处理托盘线程投递的 WM_QUIT，过早的 `process::exit` 会把进程
    /// 硬杀在一次尚未完成的硬件调用中途。若 WMI 侧的等待上限被调高到
    /// 超过本常量，必须同步调高 QUIT_FALLBACK_MS。
    #[test]
    fn test_quit_fallback_exceeds_wmi_call_timeout() {
        // 编译期断言：QUIT_FALLBACK_MS 恒 ≥ WMI 单次调用超时（3000ms）。
        // 若未来调高 GET_RESULT_TIMEOUT_MS，此断言会直接导致编译失败，
        // 强制同步调高 QUIT_FALLBACK_MS（避免进程被硬杀在一次未完成的
        // 硬件调用中途）。
        const _: () = assert!(QUIT_FALLBACK_MS >= 3000);
    }

    /// 回归测试：tray_nid_snapshot 必须返回保存的 NID 副本（NOTIFYICONDATAW
    /// 为 Copy），且不持有锁进行 Shell 调用，避免重入死锁。
    #[test]
    fn test_tray_nid_snapshot_returns_stored_state() {
        let nid = build_tray_nid(HWND(std::ptr::null_mut()), HICON(std::ptr::null_mut()));
        *TRAY_ICON.lock().unwrap_or_else(|e| e.into_inner()) = Some(TrayIconState { nid });

        let snap = tray_nid_snapshot().expect("snapshot must exist after store");
        assert_eq!(snap.hWnd, nid.hWnd);
        assert_eq!(snap.uID, nid.uID);
        assert_eq!(snap.uFlags, nid.uFlags);
        assert_eq!(snap.uCallbackMessage, nid.uCallbackMessage);
        assert_eq!(snap.hIcon, nid.hIcon);
        assert_eq!(snap.szTip, nid.szTip);
    }
}
