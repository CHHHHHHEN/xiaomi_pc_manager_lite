use std::cell::RefCell;
use std::sync::{mpsc, Mutex, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DefWindowProcW, DestroyMenu, DestroyWindow, GetCursorPos,
    KillTimer, PostMessageW, PostQuitMessage, RegisterWindowMessageW, SetForegroundWindow,
    SetTimer, TrackPopupMenu, HICON, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_LEFTBUTTON, WM_APP, WM_COMMAND, WM_DESTROY, WM_HOTKEY,
    WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_TIMER,
};

use crate::command::UiCommand;
use crate::tray::message_window;
use crate::tray::{SharedTrayStatus, TrayStatus};

const WM_TRAY: u32 = WM_APP + 1;
const WM_POWERBROADCAST: u32 = 0x0218;

/// PBT_APMPOWERSTATUSCHANGE = 0x000A：交流/电池供电状态变化。
/// 值由 Microsoft SDK（WinUser.h）定义。历史实现误写为 0x0018（=24，
/// 不属于任何 PBT 事件），导致 `handle_power_broadcast` 的 wParam 比较
/// 永不成立——"电源切换时自动重设"静默失效，且连对应用日志都不会出现。
/// 常量值由回归测试锁定（test_power_broadcast_constants_match_sdk）。
const PBT_APMPOWERSTATUSCHANGE: u32 = 0x000A;

/// PBT_APMRESUMEAUTOMATIC = 0x0012：系统从低功耗状态自动恢复（每次恢复
/// 都会发送）。PBT_APMRESUMESUSPEND = 0x0007：用户输入触发的恢复。
/// 两者不触发重设，但记录便于排查"休眠唤醒后状态异常"。
const PBT_APMRESUMEAUTOMATIC: u32 = 0x0012;
const PBT_APMRESUMESUSPEND: u32 = 0x0007;

const MID_SHOW: u32 = 100;
const MID_QUIT: u32 = 101;
const MID_TOGGLE_BATTERY: u32 = 102;
const MID_CYCLE_PERF: u32 = 103;
/// 性能模式直接选择的子菜单项基址：`MID_PERF_BASE + PerfMode::all()` 下标。
const MID_PERF_BASE: u32 = 110;

const HK_TOGGLE_BATTERY: i32 = 1;

/// NIM_ADD 失败时的重试定时器（任务栏未就绪时 `TaskbarCreated` 广播可能
/// 已经错过，见 register_tray_icon 的注释）。
const TIMER_TRAY_RETRY: usize = 1;
const TRAY_RETRY_MS: u32 = 2000;

/// Tooltip 刷新的定时器：托盘常驻期间后台定期读取共享状态刷新 tooltip，
/// 使"性能模式/电池养护"在托盘悬停文字上保持实时（窗口离屏隐藏时 GUI
/// update 循环仍运行，但托盘由消息泵的 WM_TIMER 独立驱动，与 GUI 解耦）。
const TIMER_STATUS: usize = 2;
const STATUS_REFRESH_MS: u32 = 2000;

// 托盘 worker 线程的命令通道发送端。
//
// 用**线程局部存储**而非进程级 `static`：托盘消息窗口的 `wndproc`（及
// 热键/电源广播/托盘事件回调）全部在该 worker 线程上执行（`message_loop`
// 的 `DispatchMessageW` 同线程），通道生命周期恰好等于 worker 线程。
// 进程级 `OnceLock` 方案在 `spawn` 被再次调用时 `set().ok()` 静默丢弃
// 新发送端，全局状态横跨本不需要跨线程的边界。
thread_local! {
    static CMD_TX: RefCell<Option<mpsc::Sender<UiCommand>>> =
        const { RefCell::new(None) };
}

// 托盘线程的共享状态引用（worker 线程及其 wndproc 回调使用）。
thread_local! {
    static TRAY_STATUS: RefCell<Option<SharedTrayStatus>> =
        const { RefCell::new(None) };
}

// 托盘线程记录的上一次已知性能模式：用于检测"性能模式变化"并在窗口隐藏时
// 弹托盘通知（见 show_perf_notification）。
thread_local! {
    static LAST_PERF_MODE: RefCell<Option<u8>> = const { RefCell::new(None) };
}

// 托盘线程记录的上一次已知电池养护状态：用于检测"养护状态变化"并在窗口
// 隐藏时弹托盘通知（见 show_battery_care_notification）。与 LAST_PERF_MODE
// 分离：两者变化相互独立（Fn+K 循环性能模式不会触发养护通知，反之亦然）。
thread_local! {
    static LAST_BATTERY_CARE: RefCell<Option<bool>> = const { RefCell::new(None) };
}

// 托盘线程的 egui Context 唤醒句柄（GUI 事件循环隐藏态唤醒用）。
thread_local! {
    static EGUI_CTX: RefCell<Option<egui::Context>> = const { RefCell::new(None) };
}

/// 读取当前线程的命令发送端副本（worker 线程及其 wndproc 回调使用）。
fn cmd_tx() -> Option<mpsc::Sender<UiCommand>> {
    CMD_TX.with(|tx| tx.borrow().clone())
}

/// 发送命令并唤醒 GUI 事件循环。
///
/// 窗口离屏隐藏（驻留托盘）时 update 循环仍以 500ms 间隔运行（修订 1.19），
/// 命令最迟一个间隔被处理；发送后立即 `ctx.request_repaint()` 把延迟压到
/// 最小（托盘点击/热键/Fn+K 即时响应）。
fn send_command(cmd: UiCommand) {
    if let Some(tx) = cmd_tx() {
        if let Err(e) = tx.send(cmd) {
            log::warn!("Tray: command send failed: {}", e);
        }
        // 无论发送成功与否都尝试唤醒：即使通道已断，请求重绘也无副作用。
        EGUI_CTX.with(|c| {
            if let Some(ctx) = c.borrow().as_ref() {
                ctx.request_repaint();
            }
        });
    }
}

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
        let name = crate::util::WideString::new("TaskbarCreated");
        unsafe { RegisterWindowMessageW(name.as_pcwstr()) }
    })
}

pub fn spawn(cmd_tx: mpsc::Sender<UiCommand>, status: SharedTrayStatus, ctx: egui::Context) {
    std::thread::spawn(move || worker_thread(cmd_tx, status, ctx));
}

fn worker_thread(cmd_tx: mpsc::Sender<UiCommand>, status: SharedTrayStatus, ctx: egui::Context) {
    // 托盘 worker 生命周期起点：消息窗口创建失败等路径会提前 return，
    // 从日志的 start → (error) → 无 exit 即可判断 worker 是否完整存活。
    log::info!("Tray worker thread started");
    // 发送端存入本线程的线程局部存储：wndproc 及全部回调在本线程执行，
    // 通过 cmd_tx() 读取；worker 线程结束（message_loop 返回）即随线程
    // 局部存储一起销毁，发送端关闭，GUI 侧 recv 立即得到断开信号。
    CMD_TX.with(|tx| *tx.borrow_mut() = Some(cmd_tx));
    // GUI 事件循环唤醒句柄：发送命令后 request_repaint 即时唤醒（窗口离屏
    // 存活时 update 循环仍运行，命令随时可被消费，见修订 1.19）。
    EGUI_CTX.with(|c| *c.borrow_mut() = Some(ctx));
    TRAY_STATUS.with(|s| *s.borrow_mut() = Some(status));

    let hwnd = match message_window::create_message_window() {
        Ok(w) => w,
        Err(e) => {
            log::error!("Message worker window: {}", e);
            return;
        }
    };
    if let Err(e) = message_window::set_wndproc(hwnd, wndproc) {
        log::error!("Set message window wndproc: {}", e);
        // create_message_window 已成功创建窗口，set_wndproc 失败时若直接
        // return 会泄漏该 HWND（及其用户对象）——显式销毁后返回。
        let _ = unsafe { DestroyWindow(hwnd) };
        return;
    }
    log::info!("Message window hwnd=0x{:X}", hwnd.0 as usize);

    // 热键注册不依赖托盘是否注册成功：托盘注册失败（任务栏未就绪）时
    // 热键仍须可用，图标稍后由重试定时器或 TaskbarCreated 广播补建。
    // MOD_NOREPEAT：不设置时按住热键会因键盘自动重复产生连续的 WM_HOTKEY，
    // 导致 Ctrl+Alt+B 反复翻转养护开关（MSDN RegisterHotKey: "does not yield
    // multiple hotkey notifications"）。
    let mods = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
    if let Err(e) = unsafe { RegisterHotKey(Some(hwnd), HK_TOGGLE_BATTERY, mods, 0x42) } {
        log::error!("Register hotkey (B): {:?}", e);
    } else {
        log::info!("Hotkey registered: Ctrl+Alt+B (toggle battery care)");
    }
    // 性能模式循环不再注册全局热键（Ctrl+Alt+P 已移除）：循环由 Fn+K
    // 功能键绑定提供（见 F-HOTKEY-02 / F-FNK，修订 1.19），Fn+K 是笔记本
    // 上最直接的单键入口，且不占用全局热键槽位、不与其他软件冲突。

    if let Err(e) = register_tray_icon(hwnd) {
        log::error!("Tray icon: {}", e);
    }

    // 启动 tooltip 实时刷新定时器（NIM_ADD 失败时仍启动：状态刷新只改
    // tooltip 文案，与图标是否出现独立；图标恢复后即开始实时更新）。
    let _ = unsafe { SetTimer(Some(hwnd), TIMER_STATUS, STATUS_REFRESH_MS, None) };

    message_window::message_loop(hwnd);
    // message_loop 收到 WM_QUIT 后返回：托盘 worker 生命周期结束。托盘驻留
    // 期间该线程本应无限存活，此处记录退出，避免"托盘图标消失/热键失效"
    // 时日志里只有 start 没有 exit 无从定位。
    log::info!("Tray worker message loop exited; worker thread ending");
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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
            } else if wparam.0 == TIMER_STATUS {
                refresh_tray_tooltip();
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
            // 直接操作窗口：托盘隐藏/显示不依赖 GUI update 循环（窗口离屏
            // 隐藏时循环仍运行，但托盘自给自足可即时生效，修订 1.19）。
            toggle_main_window();
        }
        MID_QUIT => {
            quit_app();
        }
        MID_TOGGLE_BATTERY => {
            log::info!("Tray menu: toggle battery care");
            send_command(UiCommand::ToggleBatteryCare);
        }
        MID_CYCLE_PERF => {
            log::info!("Tray menu: cycle perf mode");
            send_command(UiCommand::CyclePerfMode);
        }
        // 性能模式子菜单项：MID_PERF_BASE + PerfMode::all() 下标。
        id if (MID_PERF_BASE..MID_PERF_BASE + 16).contains(&id) => {
            let idx = (id - MID_PERF_BASE) as usize;
            if let Some(mode) = crate::ec::performance::PerfMode::all().get(idx) {
                log::info!("Tray menu: set perf mode {}", mode.name());
                send_command(UiCommand::SetPerfMode(mode.ec_value()));
            }
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

/// 兜底：WM_QUIT 未生效（GUI 线程被阻塞）时强制退出。宽限期必须大于
/// GUI 线程**处理完一整条命令**的最坏阻塞时长，而不只是 WMI 单次调用的
/// 时长（GET_RESULT_TIMEOUT_MS = 3000，见 ec/wmi.rs）：GUI 线程只有在
/// 进入消息循环后才会处理 WM_QUIT，而 process_commands 每帧会一次性
/// 排空整个命令队列，每条命令（如 ToggleBatteryCare）含多次顺序 WMI
/// 往返（写限值 + 写养护 + 读回，ReapplyConfig 可达 4 次），每次最坏
/// 阻塞 3000ms——单条命令最坏约 9000ms。若宽限期只覆盖单次调用，GUI
/// 正处理一条慢速 WMI 命令时过早 `process::exit` 会把进程硬杀在一次
/// 尚未完成的硬件调用中途，EC 状态可能撕裂。取 5×3000 覆盖单条命令
/// 的最坏情况并留余量（下方测试用编译期断言锁定该关系）；多条命令的
/// 批量排空发生在 WMI 每条调用都超时的极端故障下，此时超过宽限期强制
/// 退出仍是可接受的兜底。正常退出路径不经过此睡眠——主线程退出后
/// 进程随即终止，本线程的兜底睡眠由进程结束一并终结。
const QUIT_FALLBACK_MS: u64 = 15000;

fn quit_app() {
    log::info!("Tray quit: requesting app shutdown");
    // 经命令通道请求 GUI 线程正常退出（ViewportCommand::Close → eframe
    // run_native 返回 → 组件 Drop 清理）。不能直接 `PostMessage(WM_QUIT)`
    // 给主窗口：winit 事件循环不消费外部 WM_QUIT，run_native 不返回，
    // 只能靠下方兜底 process::exit 强杀、跳过所有清理（实测，修订 1.21）。
    send_command(UiCommand::Quit);
    std::thread::sleep(std::time::Duration::from_millis(QUIT_FALLBACK_MS));
    log::warn!(
        "Tray quit: app did not exit within {}ms; forcing exit",
        QUIT_FALLBACK_MS
    );
    std::process::exit(0);
}

fn handle_hotkey(wparam: WPARAM) -> LRESULT {
    let id = wparam.0 as i32;
    log::debug!("Hotkey received: id={}", id);
    if id == HK_TOGGLE_BATTERY {
        send_command(UiCommand::ToggleBatteryCare);
    }
    LRESULT(0)
}

fn handle_power_broadcast(wparam: WPARAM, _lparam: LPARAM) -> LRESULT {
    match power_broadcast_to_command(wparam.0 as u32) {
        Some(UiCommand::ReapplyConfig) => {
            log::info!("Power broadcast 0x{:08X}; sending ReapplyConfig", wparam.0);
            send_command(UiCommand::ReapplyConfig);
        }
        _ => {
            log::debug!("Power broadcast ignored: 0x{:08X}", wparam.0);
        }
    }
    LRESULT(1)
}

/// 电源广播 → 应执行的命令的纯决策逻辑（可单测）。
///
/// - `PBT_APMPOWERSTATUSCHANGE`（AC/电池切换）：重新应用配置；
/// - 休眠唤醒（`PBT_APMRESUMEAUTOMATIC`/`PBT_APMRESUMESUSPEND`）：休眠期间
///   EC/固件可能重置部分寄存器（风扇策略/充电上限），唤醒后同样重新应用
///   配置（"唤醒后设置不生效"是用户高频反馈）；内部再按
///   `auto_reapply_on_power_change` 决定是否真正写入；
/// - 其余电源事件（挂起、电源设置变更等）：不处理。
fn power_broadcast_to_command(wparam: u32) -> Option<UiCommand> {
    match wparam {
        PBT_APMPOWERSTATUSCHANGE | PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => {
            Some(UiCommand::ReapplyConfig)
        }
        _ => None,
    }
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
    *crate::util::lock_or_recover(&TRAY_ICON, "tray icon") = Some(TrayIconState { nid });

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
    set_tip(&mut nid.szTip, crate::util::APP_NAME);
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

/// 生成托盘 tooltip 文案（含实时状态）。
fn build_tooltip(status: &TrayStatus) -> String {
    let perf = crate::ec::performance::PerfMode::name_or_unknown(status.performance_mode);
    let care = if status.battery_care_enabled {
        format!("开启 (上限{}%)", status.charge_limit)
    } else {
        "关闭".to_string()
    };
    // 电量来自系统 API（GetSystemPowerStatus），实时读取。
    let snap = crate::platform::power::power_snapshot();
    let power = match (snap.status, snap.battery_percent) {
        (crate::platform::power::PowerStatus::OnAc, Some(pct)) => format!("交流 {pct}%"),
        (crate::platform::power::PowerStatus::OnAc, None) => "交流".to_string(),
        (crate::platform::power::PowerStatus::OnBattery, Some(pct)) => format!("电池 {pct}%"),
        (crate::platform::power::PowerStatus::OnBattery, None) => "电池".to_string(),
        (crate::platform::power::PowerStatus::Unknown, _) => "未知".to_string(),
    };
    format!(
        "{} · 性能:{} · 养护:{} · 电源:{}",
        crate::util::APP_NAME,
        perf,
        care,
        power
    )
}

/// 按共享状态刷新托盘 tooltip（NIM_MODIFY）。由 STATUS_REFRESH_MS 定时器
/// 周期驱动：托盘常驻期间后台定期更新，保证悬停提示保持最新（不依赖 GUI
/// update 循环）。图标尚未创建（NIM_ADD 失败重试期）时
/// 直接跳过——定时器继续跑，图标恢复后下一轮即生效。
fn refresh_tray_tooltip() {
    let Some(status) = TRAY_STATUS.with(|s| s.borrow().clone()) else {
        return;
    };
    let Some(mut nid) = tray_nid_snapshot() else {
        return;
    };
    let (text, perf_mode, battery_care) = {
        let guard = crate::util::lock_or_recover(&status, "tray status");
        (
            build_tooltip(&guard),
            guard.performance_mode,
            guard.battery_care_enabled,
        )
    };
    // 只更新 tooltip（NIF_TIP）：NIM_MODIFY 按 nid 的 uFlags 只改声明字段，
    // 不改图标/消息，避免重建图标的闪烁与消息通道重置。
    nid.uFlags = NIF_TIP;
    set_tip(&mut nid.szTip, &text);
    if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() } {
        // 修改失败通常意味着图标尚未注册（重试期），忽略——定时器下轮重试。
        log::debug!("Tray: NIM_MODIFY tooltip failed (icon not ready?)");
    }

    // 性能模式/电池养护变化时弹通知。只在主窗口隐藏时弹：窗口可见时用户
    // 能直接看到 GUI 变化，再弹通知反而是打扰（NFR-UX 一致性）。
    let should_notify_perf =
        should_notify_perf_change(LAST_PERF_MODE.with(|m| *m.borrow()), perf_mode);
    LAST_PERF_MODE.with(|m| *m.borrow_mut() = Some(perf_mode));
    let should_notify_care =
        should_notify_care_change(LAST_BATTERY_CARE.with(|m| *m.borrow()), battery_care);
    LAST_BATTERY_CARE.with(|m| *m.borrow_mut() = Some(battery_care));
    if crate::platform::window::main_window_visible() {
        return;
    }
    if should_notify_perf {
        let name = crate::ec::performance::PerfMode::name_or_unknown(perf_mode);
        show_perf_notification(nid, name);
    }
    if should_notify_care {
        show_battery_care_notification(nid, battery_care);
    }
}

/// 纯决策：性能模式变化且非首次采样时是否需要弹通知。
///
/// 首次采样（last 为 None）不弹：启动时托盘首次拿到状态只是基线，并非用户
/// 操作导致的切换，弹通知会打扰。之后每次变化都视为真实切换（Fn+K/热键/
/// 电池自动切节能/托盘菜单），返回 true 由调用方在窗口隐藏时弹气泡。
fn should_notify_perf_change(last: Option<u8>, current: u8) -> bool {
    matches!(last, Some(prev) if prev != current)
}

/// 纯决策：电池养护状态变化且非首次采样时是否需要弹通知。
///
/// 与 `should_notify_perf_change` 语义一致（首次采样不弹、之后每次变化都
/// 视为真实切换）。单独成函数便于单元测试与独立演进。
fn should_notify_care_change(last: Option<bool>, current: bool) -> bool {
    matches!(last, Some(prev) if prev != current)
}

/// 弹托盘气泡通知（NIF_INFO）：通用通知（性能模式/电池养护共用）。
///
/// 只在"窗口隐藏 + 状态变化"时调用。气泡是系统托盘通知，无需额外窗口即可
/// 展示，用户在当前窗口继续工作的同时获得状态切换反馈。
fn show_tray_notification(nid: NOTIFYICONDATAW, body: &str) {
    log::info!("Tray notification: {}", body);
    let mut nid = nid;
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    // 气泡标题与正文（szInfoTitle/szInfo 均为 64 UTF-16 单元上限）。
    let title = crate::util::WideString::new(crate::util::APP_NAME);
    let title_len = title.units().len().min(64);
    nid.szInfoTitle[..title_len].copy_from_slice(&title.units()[..title_len]);
    let info_wide: Vec<u16> = body.encode_utf16().take(63).collect();
    nid.szInfo[..info_wide.len()].copy_from_slice(&info_wide);
    // NIM_MODIFY 携带 NIF_INFO 触发气泡。
    if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() } {
        log::debug!("Tray: NIM_MODIFY notification failed");
    }
}

/// 弹托盘气泡通知：性能模式已切换。
fn show_perf_notification(nid: NOTIFYICONDATAW, perf_name: &str) {
    show_tray_notification(nid, &format!("性能模式: {}", perf_name));
}

/// 弹托盘气泡通知：电池养护已启用/停用。
fn show_battery_care_notification(nid: NOTIFYICONDATAW, enabled: bool) {
    show_tray_notification(
        nid,
        if enabled {
            "电池养护: 已启用"
        } else {
            "电池养护: 已停用"
        },
    );
}

/// 在锁内拷贝 NID（NOTIFYICONDATAW 实现 Copy）后立即释放锁，再调用
/// Shell_NotifyIconW。避免 Shell 调用期间持有锁：wndproc 处理其它消息时
/// 也会获取同一把锁，若 Shell 调用重入本窗口将形成死锁。
fn tray_nid_snapshot() -> Option<NOTIFYICONDATAW> {
    let guard = crate::util::lock_or_recover(&TRAY_ICON, "tray icon");
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

    // 性能模式直接选择子菜单：列出全部模式，当前模式打勾（读取共享状态）。
    // 相比"切换性能模式"循环，用户可一步直达目标模式（F-PERF 核心场景）。
    let perf_sub = unsafe { CreatePopupMenu().unwrap_or_default() };
    let current_perf = TRAY_STATUS
        .with(|s| s.borrow().clone())
        .map(|s| crate::util::lock_or_recover(&s, "tray status").performance_mode);
    for (idx, mode) in crate::ec::performance::PerfMode::all().iter().enumerate() {
        let name = crate::util::WideString::new(mode.name());
        let flags = if Some(mode.ec_value()) == current_perf {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        let _ = unsafe {
            AppendMenuW(
                perf_sub,
                flags,
                (MID_PERF_BASE + idx as u32) as usize,
                name.as_pcwstr(),
            )
        };
    }
    let perf_title = crate::util::WideString::new("性能模式");
    let _ = unsafe { AppendMenuW(hmenu, MF_POPUP, perf_sub.0 as usize, perf_title.as_pcwstr()) };

    // 菜单项顺序：性能模式 → 常用操作（电池养护 / 性能模式循环）→ 分隔 → 窗口显隐 → 退出。
    let toggle = crate::util::WideString::new("切换电池养护");
    let _ = unsafe {
        AppendMenuW(
            hmenu,
            MF_STRING,
            MID_TOGGLE_BATTERY as usize,
            toggle.as_pcwstr(),
        )
    };
    let cycle = crate::util::WideString::new("切换性能模式");
    let _ = unsafe { AppendMenuW(hmenu, MF_STRING, MID_CYCLE_PERF as usize, cycle.as_pcwstr()) };
    let _ = unsafe { AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null()) };
    let show = crate::util::WideString::new("显示/隐藏窗口");
    let _ = unsafe { AppendMenuW(hmenu, MF_STRING, MID_SHOW as usize, show.as_pcwstr()) };
    let _ = unsafe { AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null()) };
    let quit = crate::util::WideString::new("退出");
    let _ = unsafe { AppendMenuW(hmenu, MF_STRING, MID_QUIT as usize, quit.as_pcwstr()) };

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

/// 从托盘 ICO 字节构建 HICON：解析多尺寸 ICO，取不小于 DPI 缩放后目标
/// 尺寸（16 逻辑 px × DPI/96）的最小单帧交给 `CreateIconFromResourceEx`。
///
/// **为什么不能把整份 ICO 直接传**：实测整份多帧 ICO 会返回
/// `0x80070006`（INVALID_HANDLE），单帧 PNG 块才能创建（见
/// `platform::window::create_hicon_from_ico` 的注释）。恶意/损坏 ICO 的
/// 越界/溢出区间由该 helper 统一校验。
fn load_icon(bytes: &[u8]) -> Result<windows::Win32::UI::WindowsAndMessaging::HICON, String> {
    crate::platform::window::create_hicon_from_ico(
        bytes,
        crate::platform::window::tray_icon_size_px(),
    )
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

        let mut expected: Vec<u16> = crate::util::APP_NAME.encode_utf16().collect();
        expected.push(0); // NUL 结尾
        assert!(
            expected.len() <= nid.szTip.len(),
            "tooltip must fit in szTip"
        );
        assert_eq!(&nid.szTip[..expected.len()], expected.as_slice());
        // 剩余部分保持零初始化。
        assert!(nid.szTip[expected.len()..].iter().all(|&c| c == 0));
    }

    /// 回归测试：恶意/损坏的 ICO 声明越界或溢出的图像偏移时，load_icon 必须
    /// 返回 Err 而不是构造越界切片 panic（32 位平台 off+sz 可能回绕，绕过
    /// 旧的 off+sz > len 检查后 `&bytes[off..off+sz]` 越界崩溃）。
    #[test]
    fn test_load_icon_rejects_malformed_offsets() {
        // 合法头部（1 个条目），但图像偏移/长度声明超出缓冲。
        fn ico_with_entry(off: u32, sz: u32) -> Vec<u8> {
            let mut b = vec![0u8; 6 + 16];
            b[4] = 1;
            b[5] = 0;
            b[6 + 8..6 + 12].copy_from_slice(&sz.to_le_bytes());
            b[6 + 12..6 + 16].copy_from_slice(&off.to_le_bytes());
            b
        }
        // off+sz 在 u32 内回绕为小值，若用未检查加法会绕过 OOB 校验。
        let wrapped = ico_with_entry(u32::MAX, 2);
        assert_eq!(wrapped.len(), 22);
        // off+sz 计算（u64 不溢出）：0x100000001 > 22 → 必须拒绝。
        assert!(load_icon(&ico_with_entry(u32::MAX, 2)).is_err());
        // 偏移正常但超出缓冲。
        assert!(load_icon(&ico_with_entry(1000, 10)).is_err());
        // 偏移合法但长度越界。
        assert!(load_icon(&ico_with_entry(6, 10_000)).is_err());
    }

    /// 回归测试：TraybarCreated 注册消息必须可用且稳定，且位于注册消息
    /// 区间（0xC000-0xFFFF），不与 WM_APP 区间的 WM_TRAY 冲突。
    #[test]
    fn test_taskbar_created_message_registers() {
        let msg1 = taskbar_created_msg();
        let msg2 = taskbar_created_msg();
        assert_ne!(
            msg1, 0,
            "RegisterWindowMessageW(TaskbarCreated) must succeed"
        );
        assert_eq!(msg1, msg2, "taskbar message id must be stable");
        assert!((0xC000..=0xFFFF).contains(&msg1));
        assert_ne!(msg1, WM_TRAY);
    }

    /// 回归测试（电源广播）：PBT 事件常量必须与 Windows SDK（WinUser.h）
    /// 一致。历史实现把 PBT_APMPOWERSTATUSCHANGE 误写为 0x0018（不属于
    /// 任何 PBT 事件），WM_POWERBROADCAST 的 wParam 比较永不成立——"电源
    /// 切换时自动重设"静默失效且日志中从未出现过 "Power status changed"。
    #[test]
    fn test_power_broadcast_constants_match_sdk() {
        // 本机 MSDN 文档值（WinUser.h）：0x000A / 0x0012 / 0x0007。
        assert_eq!(PBT_APMPOWERSTATUSCHANGE, 0x000A);
        assert_eq!(PBT_APMRESUMEAUTOMATIC, 0x0012);
        assert_eq!(PBT_APMRESUMESUSPEND, 0x0007);
    }

    /// 电源广播 → 命令的纯决策：电源切换、两种唤醒事件都应触发 Reapply，
    /// 其余事件（挂起、电源设置变更等）不处理。
    #[test]
    fn test_power_broadcast_to_command() {
        use std::matches;
        assert!(matches!(
            power_broadcast_to_command(PBT_APMPOWERSTATUSCHANGE),
            Some(UiCommand::ReapplyConfig)
        ));
        assert!(matches!(
            power_broadcast_to_command(PBT_APMRESUMEAUTOMATIC),
            Some(UiCommand::ReapplyConfig)
        ));
        assert!(matches!(
            power_broadcast_to_command(PBT_APMRESUMESUSPEND),
            Some(UiCommand::ReapplyConfig)
        ));
        // 不相关事件（挂起、电源设置变更等）不触发。
        assert!(power_broadcast_to_command(0x0004).is_none()); // PBT_APMSUSPEND
        assert!(power_broadcast_to_command(0x0018).is_none()); // 历史误写的错误值
        assert!(power_broadcast_to_command(0xFFFF).is_none());
    }

    /// 性能模式变化通知的纯决策：
    /// - 首次采样（None → 值）不弹（启动基线，非用户操作）；
    /// - 后续值变化弹；值相同不弹。
    #[test]
    fn test_should_notify_perf_change() {
        assert!(
            !should_notify_perf_change(None, 0x09),
            "first sample must not notify"
        );
        assert!(
            !should_notify_perf_change(Some(0x09), 0x09),
            "no change must not notify"
        );
        assert!(
            should_notify_perf_change(Some(0x09), 0x02),
            "perf change must notify"
        );
        assert!(
            should_notify_perf_change(Some(0x02), 0x04),
            "another perf change must notify"
        );
    }

    /// 回归测试：电池养护状态变化通知的决策逻辑（与性能模式同语义）。
    #[test]
    fn test_should_notify_care_change() {
        assert!(
            !should_notify_care_change(None, false),
            "first sample must not notify"
        );
        assert!(
            !should_notify_care_change(Some(false), false),
            "no change must not notify"
        );
        assert!(
            should_notify_care_change(Some(false), true),
            "care enabled must notify"
        );
        assert!(
            should_notify_care_change(Some(true), false),
            "care disabled must notify"
        );
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
        set_tip(&mut sz_tip, crate::util::APP_NAME);
        let mut expected: Vec<u16> = crate::util::APP_NAME.encode_utf16().collect();
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

    /// 回归测试：托盘退出的兜底强制退出时长必须大于 GUI 线程处理
    /// **一整条命令**的最坏阻塞时长——而非仅 WMI 单次调用。GUI 线程处理
    /// WM_QUIT 必须先完成当前命令批次的排空：单条命令（如 ToggleBatteryCare）
    /// 最坏含 3 次顺序 WMI 调用（写限值 + 写养护 + 读回），每次调用的最坏
    /// 阻塞为 GET_RESULT_TIMEOUT_MS（ec/wmi.rs）；ReapplyConfig 可达
    /// 4 次。过早的 `process::exit` 会把进程硬杀在一次尚未完成的硬件调用
    /// 中途。若 WMI 侧的等待上限被调高到超过本常量/3，必须同步调高
    /// QUIT_FALLBACK_MS。
    #[test]
    fn test_quit_fallback_exceeds_wmi_call_timeout() {
        // 编译期断言：QUIT_FALLBACK_MS 恒 ≥ 3 × WMI 单次调用超时。引用真实
        // 常量而非硬编码 3000，保证未来调高 GET_RESULT_TIMEOUT_MS 时此断言
        // 立即编译失败，强制同步调高 QUIT_FALLBACK_MS（避免进程被硬杀在
        // 一次未完成的硬件调用中途）。
        const _: () = assert!(QUIT_FALLBACK_MS >= 3 * crate::ec::wmi::GET_RESULT_TIMEOUT_MS as u64);
    }

    /// 回归测试：tray_nid_snapshot 必须返回保存的 NID 副本（NOTIFYICONDATAW
    /// 为 Copy），且不持有锁进行 Shell 调用，避免重入死锁。
    #[test]
    fn test_tray_nid_snapshot_returns_stored_state() {
        let nid = build_tray_nid(HWND(std::ptr::null_mut()), HICON(std::ptr::null_mut()));
        *crate::util::lock_or_recover(&TRAY_ICON, "tray icon") = Some(TrayIconState { nid });

        let snap = tray_nid_snapshot().expect("snapshot must exist after store");
        assert_eq!(snap.hWnd, nid.hWnd);
        assert_eq!(snap.uID, nid.uID);
        assert_eq!(snap.uFlags, nid.uFlags);
        assert_eq!(snap.uCallbackMessage, nid.uCallbackMessage);
        assert_eq!(snap.hIcon, nid.hIcon);
        assert_eq!(snap.szTip, nid.szTip);
    }

    /// 托盘 tooltip 文案应包含当前性能模式与电池养护状态。
    #[test]
    fn test_build_tooltip_includes_status() {
        let status = TrayStatus {
            battery_care_enabled: true,
            charge_limit: 80,
            performance_mode: crate::ec::performance::PerfMode::Smart.ec_value(),
        };
        let tip = build_tooltip(&status);
        assert!(tip.contains("智能"), "perf name must appear: {}", tip);
        assert!(tip.contains("开启"), "care state must appear: {}", tip);
        assert!(tip.contains("80%"), "limit must appear: {}", tip);

        let off = TrayStatus {
            battery_care_enabled: false,
            ..status
        };
        assert!(build_tooltip(&off).contains("关闭"));
    }

    /// 托盘 tooltip 文案必须能写进 szTip（≤127 UTF-16 单元 + NUL）。
    #[test]
    fn test_build_tooltip_fits_in_sz_tip() {
        let status = TrayStatus {
            battery_care_enabled: true,
            charge_limit: 80,
            performance_mode: crate::ec::performance::PerfMode::Extreme.ec_value(),
        };
        let tip = build_tooltip(&status);
        assert!(tip.encode_utf16().count() <= 127);
        let mut sz_tip = [0u16; 128];
        set_tip(&mut sz_tip, &tip);
        assert_eq!(
            sz_tip[tip.encode_utf16().count()],
            0,
            "must be NUL-terminated"
        );
    }
}
