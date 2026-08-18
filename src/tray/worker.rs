use std::cell::RefCell;
use std::sync::{Arc, Mutex, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DefWindowProcW, DestroyMenu, DestroyWindow, GetCursorPos,
    KillTimer, PostMessageW, PostQuitMessage, RegisterWindowMessageW, SetForegroundWindow,
    SetTimer, TrackPopupMenu, HICON, HMENU, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    PBT_APMPOWERSTATUSCHANGE, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, TPM_BOTTOMALIGN,
    TPM_LEFTALIGN, TPM_LEFTBUTTON, WM_APP, WM_COMMAND, WM_DESTROY, WM_HOTKEY, WM_LBUTTONUP,
    WM_NULL, WM_POWERBROADCAST, WM_RBUTTONUP, WM_TIMER,
};

use crate::app::command::UiCommand;
use crate::app::notify;
use crate::app::sink::{CommandSink, CommandSinkExt};
use crate::tray::message_window;
use crate::tray::notify as tray_notify;

use crate::tray::{SharedTrayStatus, TrayStatus};

const WM_TRAY: u32 = WM_APP + 1;

// PBT_APMPOWERSTATUSCHANGE / PBT_APMRESUMEAUTOMATIC / PBT_APMRESUMESUSPEND /
// WM_POWERBROADCAST 均来自 windows crate（修订 1.46 审计）：历史实现手写
// 常量，曾把 APMPOWERSTATUSCHANGE 误写为 0x0018（=24，不属于任何 PBT 事件），
// 导致 `handle_power_broadcast` 的 wParam 比较永不成立——"电源切换时自动
// 重设"静默失效。改用 crate 常量后该值由 SDK 定义保证，且与
// windows-rs 0.62.2 的 Win32_UI_WindowsAndMessaging 绑定一致
// （PBT_APMPOWERSTATUSCHANGE=0x000A、PBT_APMRESUMEAUTOMATIC=0x0012、
// PBT_APMRESUMESUSPEND=0x0007、WM_POWERBROADCAST=0x0218，回归测试锁定）。

const MID_SHOW: u32 = 100;
const MID_QUIT: u32 = 101;
const MID_TOGGLE_BATTERY: u32 = 102;
const MID_CYCLE_PERF: u32 = 103;
/// 性能模式直接选择的子菜单项基址：`MID_PERF_BASE + PerfMode::all()` 下标。
const MID_PERF_BASE: u32 = 110;

const HK_TOGGLE_BATTERY: i32 = 1;

/// 养护开关热键的虚拟键码（'B'，配合 Ctrl+Alt，见 F-HOTKEY-01）。
const VK_B: u32 = 0x42;

/// NIM_ADD 失败时的重试定时器（任务栏未就绪时 `TaskbarCreated` 广播可能
/// 已经错过，见 register_tray_icon 的注释）。
const TIMER_TRAY_RETRY: usize = 1;
const TRAY_RETRY_MS: u32 = 2000;

/// Tooltip 刷新的定时器：托盘常驻期间后台定期读取共享状态刷新 tooltip，
/// 使"性能模式/电池养护"在托盘悬停文字上保持实时（窗口离屏隐藏时 GUI
/// update 循环仍运行，但托盘由消息泵的 WM_TIMER 独立驱动，与 GUI 解耦）。
const TIMER_STATUS: usize = 2;
const STATUS_REFRESH_MS: u32 = 2000;

// 托盘 worker 线程的线程局部状态。
//
// 用**线程局部存储**而非进程级 `static`：托盘消息窗口的 `wndproc`（及
// 热键/电源广播/托盘事件回调）全部在该 worker 线程上执行（`message_loop`
// 的 `DispatchMessageW` 同线程），状态生命周期恰好等于 worker 线程。
// 进程级 `OnceLock` 方案在 `spawn` 被再次调用时 `set().ok()` 静默丢弃
// 新值，全局状态横跨本不需要跨线程的边界。
//
// 历史实现把命令端口、共享状态、通知基线等散落为 5 个小 thread_local，
// 本结构体统一收敛：命令端口是 `CommandSink` trait 对象（发送命令 + 唤醒
// 事件循环，不再直接持有 `egui::Context`）。
struct TrayThreadState {
    /// 命令端口（发送命令 + 唤醒 GUI 事件循环）。
    sink: Arc<dyn CommandSink>,
    /// 与托盘共享的状态（tooltip/菜单展示）。
    status: SharedTrayStatus,
    /// 上一次已知的性能模式（触发"性能模式变化"通知的基线）。
    last_perf_mode: Option<u8>,
    /// 上一次已知的电池养护状态（触发"养护变化"通知的基线）。
    last_battery_care: Option<bool>,
    /// 充电上限"已到达"的武装状态（键控上限，见 `app::notify`）：
    /// `None` = 尚未采到基线（启动后首个 2s tick 前），首采样不误报。
    charge_limit_reached: Option<(u8, bool)>,
}

thread_local! {
    static TRAY_STATE: RefCell<Option<TrayThreadState>> = const { RefCell::new(None) };
}

/// 读取当前线程的命令端口副本（worker 线程及其 wndproc 回调使用）。
fn cmd_sink() -> Option<Arc<dyn CommandSink>> {
    TRAY_STATE.with(|s| s.borrow().as_ref().map(|t| t.sink.clone()))
}

/// 发送命令并唤醒 GUI 事件循环（经 `CommandSink`）。
///
/// 窗口离屏隐藏（驻留托盘）时 update 循环仍以 500ms 间隔运行，命令最迟
/// 一个间隔被处理；投递后 sink 立即 `request_repaint` 把延迟压到最小。
fn send_command(cmd: UiCommand) {
    if let Some(sink) = cmd_sink() {
        sink.dispatch(cmd);
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

pub fn spawn(sink: Arc<dyn CommandSink>, status: SharedTrayStatus) {
    // 与 Fn 监听/电池健康/自启动/WMI worker 共用 util::spawn_guarded 兜底：
    // release 已移除 panic=abort，本线程 panic 会静默终止托盘功能（图标
    // 消失/热键失效）而应用仍存活——捕获并记录语义化错误；Builder 防 spawn
    // 失败 panic 传播到 GUI update 线程。
    if let Err(e) = crate::util::spawn_guarded("tray-worker", move || worker_thread(sink, status)) {
        log::error!("failed to spawn tray worker thread: {}", e);
    }
}

fn worker_thread(sink: Arc<dyn CommandSink>, status: SharedTrayStatus) {
    // 托盘 worker 生命周期起点：消息窗口创建失败等路径会提前 return，
    // 从日志的 start → (error) → 无 exit 即可判断 worker 是否完整存活。
    log::info!("Tray worker thread started");
    // 托盘状态（命令端口 + 共享托盘状态 + 通知基线）存入本线程局部存储：
    // wndproc 及全部回调在本线程执行，通过 TRAY_STATE 读取；worker 线程
    // 结束（message_loop 返回）即随线程局部存储一起销毁。
    TRAY_STATE.with(|s| {
        *s.borrow_mut() = Some(TrayThreadState {
            sink,
            status,
            last_perf_mode: None,
            last_battery_care: None,
            charge_limit_reached: None,
        });
    });

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
    if let Err(e) = unsafe { RegisterHotKey(Some(hwnd), HK_TOGGLE_BATTERY, mods, VK_B) } {
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
    // SetTimer 失败（极罕见）时 tooltip 永不刷新且无任何日志——记录一条
    // 告警便于排查"悬停提示一直不变"（修订 1.47 审计）。
    if unsafe { SetTimer(Some(hwnd), TIMER_STATUS, STATUS_REFRESH_MS, None) } == 0 {
        log::warn!(
            "SetTimer (status refresh) failed: {:#x}",
            unsafe { GetLastError() }.0
        );
    }

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

/// 托盘"性能模式"子菜单的菜单 ID → 模式映射（F-TRAY-12）：子菜单项 ID =
/// `MID_PERF_BASE + PerfMode::all()` 下标。纯函数便于单测锁定 ID 布局
///（改动 ID/顺序会让右键菜单命错模式或点不动）。
fn perf_menu_mode_from_id(menu_id: u32) -> Option<crate::app::performance::PerfMode> {
    let idx = menu_id.checked_sub(MID_PERF_BASE)? as usize;
    crate::app::performance::PerfMode::all().get(idx).copied()
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
        // 性能模式子菜单项：MID_PERF_BASE + PerfMode::all() 下标（映射见
        // perf_menu_mode_from_id）。未匹配的 ID 静默忽略。
        id => {
            if let Some(mode) = perf_menu_mode_from_id(id) {
                log::info!("Tray menu: set perf mode {}", mode.name());
                send_command(UiCommand::SetPerfMode(mode.ec_value()));
            }
        }
    }
    LRESULT(0)
}

/// 托盘直接切换主窗口可见性（不依赖 GUI update 循环）。
fn toggle_main_window() {
    // `main_window_visible` 内部是 FindWindowW + GetWindowThreadProcessId +
    // GetWindowRect 多次系统调用——一次取回可见性，hide/show 二选一
    // （修订 1.47 审计：历史实现调用了两次，查窗口两次）。
    let visible = crate::platform::window::main_window_visible();
    log::info!("Tray toggle: visible={}", visible);
    if visible {
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
/// 阻塞 3000ms——单条命令最坏约 12000ms；若 worker 彻底卡死、3s 上限
/// 不兑现，第一次调用会被 GUI 侧 CALL_REPLY_TIMEOUT（6s）兜住后熔断，
/// 后续调用快速失败，因此单个命令的最坏阻塞 = max(4×3s, 6s) = 12s。
/// 取 15s 覆盖并留余量（下方测试用编译期断言锁定该关系）；多条命令的
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
///   配置（"唤醒后设置不生效"是用户高频反馈）。
///
/// 是否真正写入的**门控**在 `gui::commands::reapply_config`（
/// `auto_reapply_on_power_change` / `auto_switch_to_quiet_on_battery` 任一
/// 开启才执行）——本函数只负责"广播 → 命令"的映射，不在此判定配置开关
/// （修订 1.47 审计：历史注释把门控语义写在本函数内，与实际实现位置不符，
/// 误导维护者）。
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
            // 双击检测（修订 1.31）：Windows 对一次双击发送两条 WM_LBUTTONUP
            //（中间的 WM_LBUTTONDBLCLK 被忽略）。若每次单击都 toggle，双击
            // 会让窗口"打开又立刻关闭"——用户肌肉记忆里的托盘双击手势
            // 直接失效。修复：第二次单击若落在系统双击间隔内，按**强制显示**
            // 处理而非再次 toggle。F-TRAY-04 的单击 toggle 语义保持不变。
            if is_double_click() {
                log::debug!("Tray double-click; force-showing main window");
                crate::platform::window::show_main_window();
            } else {
                // 直接操作窗口（原因见 toggle_main_window）。
                toggle_main_window();
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

// 托盘最近一次单击时间（线程局部，双击判定用）。
thread_local! {
    static LAST_CLICK: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
}

/// 判断本次托盘单击是否为双击序列的第二击。
///
/// 用 GetDoubleClickTime()（系统设置的双击间隔）判定：上一次单击在间隔内
/// 则视为双击，并刷新时间戳使第三次单击重新开始一轮。线程局部存储避免
/// 与托盘 worker 之外的状态耦合。
fn is_double_click() -> bool {
    LAST_CLICK.with(|last| {
        let now = std::time::Instant::now();
        let is_double = last
            .get()
            .map(|prev| now.duration_since(prev).as_millis() <= get_double_click_ms())
            .unwrap_or(false);
        last.set(Some(now));
        is_double
    })
}

/// 系统双击间隔（毫秒）。
///
/// `GetDoubleClickTime()` 无失败返回（MSDN：返回值即当前系统双击间隔，
/// 系统无自定义值时内部使用默认 500ms，不会返回 0）——直接采用系统值，
/// 不存在"取默认 500ms"的兜底分支（历史实现的分支不可达）。
fn get_double_click_ms() -> u128 {
    unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() as u128 }
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
        // 保留 GetLastError：任务栏未就绪（最常见的重试场景）与 NID 字段非法
        // （编程错误）在日志里必须可区分（修订 1.47 审计：历史实现丢弃了
        // last-error，"NIM_ADD failed"无法定位根因）。
        let last_error = unsafe { GetLastError() }.0;
        start_retry_timer(hwnd);
        return Err(format!(
            "NIM_ADD failed (last error: {:#x}); retrying",
            last_error
        ));
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
///
/// 电量/电源状态由调用方传入**单次** `power_snapshot`（`refresh_tray_tooltip`
/// 每 2s 已查询一次并用于充电判定，此处复用而非二次调用 `GetSystemPowerStatus`）。
fn build_tooltip(status: &TrayStatus, snap: &crate::platform::power::PowerSnapshot) -> String {
    let perf = crate::app::performance::PerfMode::name_or_unknown(status.performance_mode);
    let care = if status.battery_care_enabled {
        format!(
            "{} (上限{}%)",
            crate::app::battery::care_label(true),
            status.charge_limit
        )
    } else {
        crate::app::battery::care_label(false).to_string()
    };
    let power = match (snap.status, snap.battery_percent) {
        (crate::platform::power::PowerStatus::OnAc, Some(pct)) => format!("交流 {pct}%"),
        (crate::platform::power::PowerStatus::OnAc, None) => "交流".to_string(),
        (crate::platform::power::PowerStatus::OnBattery, Some(pct)) => format!("电池 {pct}%"),
        (crate::platform::power::PowerStatus::OnBattery, None) => "电池".to_string(),
        (crate::platform::power::PowerStatus::Unknown, _) => "未知".to_string(),
    };
    let mut parts = vec![
        crate::util::APP_NAME.to_string(),
        format!("性能:{perf}"),
        format!("养护:{care}"),
        format!("电源:{power}"),
    ];
    // 电池健康（root\WMI 容量读数，GUI 后台线程上报）：尚未读到时不展示。
    if let Some(p) = status.battery_health_percent {
        parts.push(format!("健康:{p}%"));
    }
    // 预计剩余/充满时长（GUI 估算，修订 1.37）：速率不可用时不展示。
    if let Some(eta) = &status.battery_eta_text {
        parts.push(eta.clone());
    }
    parts.join(" · ")
}

/// 按共享状态刷新托盘 tooltip（NIM_MODIFY）。由 STATUS_REFRESH_MS 定时器
/// 周期驱动：托盘常驻期间后台定期更新，保证悬停提示保持最新（不依赖 GUI
/// update 循环）。图标尚未创建（NIM_ADD 失败重试期）时
/// 直接跳过——定时器继续跑，图标恢复后下一轮即生效。
fn refresh_tray_tooltip() {
    let Some(status) = TRAY_STATE.with(|s| s.borrow().as_ref().map(|t| t.status.clone())) else {
        return;
    };
    let Some(mut nid) = tray_nid_snapshot() else {
        return;
    };
    // 电量/电源快照：每 tick 查询一次，tooltip 文案与充电达上限判定共用
    // （GetSystemPowerStatus 是 Windows 系统调用，双份查询浪费）。
    let snap = crate::platform::power::power_snapshot();
    let (text, perf_mode, battery_care, charge_limit, notify_on_charge_limit) = {
        let guard = crate::util::lock_or_recover(&status, "tray status");
        (
            build_tooltip(&guard, &snap),
            guard.performance_mode,
            guard.battery_care_enabled,
            guard.charge_limit,
            guard.notify_on_charge_limit,
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
    // 通知基线与"充电已达上限"武装状态统一保存在 TrayThreadState。
    let (should_notify_perf, should_notify_care, should_notify_charge) = TRAY_STATE.with(|s| {
        let mut guard = s.borrow_mut();
        let Some(st) = guard.as_mut() else {
            return (false, false, false);
        };
        let should_notify_perf = notify::should_notify_perf_change(st.last_perf_mode, perf_mode);
        st.last_perf_mode = Some(perf_mode);
        let should_notify_care =
            notify::should_notify_care_change(st.last_battery_care, battery_care);
        st.last_battery_care = Some(battery_care);
        // 充电达到养护上限：按电源快照 + 上限阈值判定跨线。上限变化时复位
        // 武装状态（键控上限）：当前上限与上次判定上限一致时才复用——否则
        // 用户改上限（80→90、关闭养护使 limit=100）后，旧上限的"已武装"
        // 会压制新上限的到达通知。
        let prev_at_limit = notify::armed_state_for_limit(st.charge_limit_reached, charge_limit);
        let (should_notify_charge, at_limit) = notify::charge_limit_notification_decision(
            prev_at_limit,
            snap.battery_percent,
            charge_limit,
            snap.status == crate::app::power::PowerStatus::OnAc,
            notify_on_charge_limit,
        );
        st.charge_limit_reached = at_limit.map(|armed| (charge_limit, armed));
        (should_notify_perf, should_notify_care, should_notify_charge)
    });
    if crate::platform::window::main_window_visible() {
        return;
    }
    if should_notify_perf {
        let name = crate::app::performance::PerfMode::name_or_unknown(perf_mode);
        tray_notify::show_perf_notification(nid, name);
    }
    if should_notify_care {
        tray_notify::show_battery_care_notification(nid, battery_care);
    }
    if should_notify_charge {
        tray_notify::show_tray_notification(
            nid,
            &notify::charge_limit_notification_text(charge_limit),
        );
    }
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

/// 构建"性能模式"子菜单（列出全部模式，当前模式打勾）。
///
/// 从 `show_tray_menu` 抽出（历史实现内联 ~30 行）：子菜单项 ID 按
/// `MID_PERF_BASE + PerfMode::all()` 下标布局，创建失败（极罕见）时返回
/// `None`，调用方跳过该块（以 0 为 item ID 的 MF_POPUP 会追加"点不动"的
/// 坏条目，不如不显示）。
fn build_perf_submenu() -> Option<HMENU> {
    let perf_sub = match unsafe { CreatePopupMenu() } {
        Ok(menu) => menu,
        Err(e) => {
            log::error!("Tray: CreatePopupMenu (perf submenu) failed: {}", e);
            return None;
        }
    };
    let current_perf = TRAY_STATE
        .with(|s| s.borrow().as_ref().map(|t| t.status.clone()))
        .map(|s| crate::util::lock_or_recover(&s, "tray status").performance_mode);
    for (idx, mode) in crate::app::performance::PerfMode::all().iter().enumerate() {
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
    Some(perf_sub)
}

/// 追加一条普通字符串菜单项（MF_STRING + 显式 item ID）。
/// 从 `show_tray_menu` 抽出：历史实现逐个内联 `WideString::new` +
/// `AppendMenuW`，6 个常规项重复同一模板。
fn append_string_item(hmenu: HMENU, label: &str, id: usize) {
    let wide = crate::util::WideString::new(label);
    let _ = unsafe { AppendMenuW(hmenu, MF_STRING, id, wide.as_pcwstr()) };
}

fn show_tray_menu(hwnd: HWND) {
    // 主菜单创建失败时显式退出并记录错误：历史实现把失败静默吞掉
    // （AppendMenuW/TrackPopupMenu 全部对着空句柄空转，菜单不出现在任务
    // 栏），用户右击毫无反应且日志无任何线索。创建失败即放弃本次展示。
    let hmenu = match unsafe { CreatePopupMenu() } {
        Ok(menu) => menu,
        Err(e) => {
            log::error!("Tray: CreatePopupMenu failed: {}", e);
            return;
        }
    };

    // 性能模式直接选择子菜单：列出全部模式，当前模式打勾（读取共享状态）。
    // 相比"切换性能模式"循环，用户可一步直达目标模式（F-PERF 核心场景）。
    // CreatePopupMenu 失败（NULL，极罕见）时跳过子菜单块——以 0 为 item ID
    // 的 MF_POPUP 会追加一个"点不动"的坏条目（0 是合法 ID，WM_COMMAND 落入
    // handle_menu_command 被静默忽略），不如不显示。
    if let Some(perf_sub) = build_perf_submenu() {
        let perf_title = crate::util::WideString::new("性能模式");
        let _ =
            unsafe { AppendMenuW(hmenu, MF_POPUP, perf_sub.0 as usize, perf_title.as_pcwstr()) };
    }

    // 菜单项顺序：性能模式 → 常用操作（电池养护 / 性能模式循环）→ 分隔 → 窗口显隐 → 退出。
    append_string_item(hmenu, "切换电池养护", MID_TOGGLE_BATTERY as usize);
    append_string_item(hmenu, "切换性能模式", MID_CYCLE_PERF as usize);
    let _ = unsafe { AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null()) };
    append_string_item(hmenu, "显示/隐藏窗口", MID_SHOW as usize);
    let _ = unsafe { AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null()) };
    append_string_item(hmenu, "退出", MID_QUIT as usize);

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
/// `platform::icon::create_hicon_from_ico` 的注释）。恶意/损坏 ICO 的
/// 越界/溢出区间由该 helper 统一校验。
fn load_icon(bytes: &[u8]) -> Result<windows::Win32::UI::WindowsAndMessaging::HICON, String> {
    crate::platform::icon::create_hicon_from_ico(bytes, crate::platform::icon::tray_icon_size_px())
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
    /// 性能模式子菜单 ID 映射（F-TRAY-12）：MID_PERF_BASE + 下标命中对应模式，
    /// 越界/非性能 ID 返回 None（静默忽略）。
    #[test]
    fn test_perf_menu_mode_from_id() {
        for (idx, mode) in crate::app::performance::PerfMode::all().iter().enumerate() {
            assert_eq!(
                perf_menu_mode_from_id(MID_PERF_BASE + idx as u32),
                Some(*mode),
                "menu id must map to the mode at the same index"
            );
        }
        // 越界 / 主菜单 ID / 下溢 / 上溢。
        assert_eq!(perf_menu_mode_from_id(MID_PERF_BASE - 1), None);
        assert_eq!(perf_menu_mode_from_id(MID_PERF_BASE + 100), None);
        assert_eq!(perf_menu_mode_from_id(MID_QUIT), None);
        assert_eq!(perf_menu_mode_from_id(0), None);
        assert_eq!(perf_menu_mode_from_id(u32::MAX), None);
    }

    #[test]
    fn test_power_broadcast_constants_match_sdk() {
        // windows crate 提供的 PBT/WM_POWERBROADCAST 常量与 MSDN WinUser.h
        // 文档值一致（修订 1.46 起代码改用 crate 常量；本测试锁定其 SDK 值，
        // 若 crate 升级后某常量漂移立即暴露）。
        assert_eq!(PBT_APMPOWERSTATUSCHANGE, 0x000A);
        assert_eq!(PBT_APMRESUMEAUTOMATIC, 0x0012);
        assert_eq!(PBT_APMRESUMESUSPEND, 0x0007);
        assert_eq!(WM_POWERBROADCAST, 0x0218);
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
    /// WM_QUIT 必须先完成当前命令批次的排空：单条命令（如 ReapplyConfig）
    /// 最坏含 4 次顺序 WMI 调用（写限值 + 写养护 + 读回 + 写性能模式），
    /// 每次调用的最坏阻塞为 GET_RESULT_TIMEOUT_MS（ec/wmi.rs，worker 侧）；
    /// worker 彻底卡死（3s 上限不兑现）时，第一次调用由 GUI 侧
    /// CALL_REPLY_TIMEOUT（6s）兜底熔断、后续快速失败——单命令最坏阻塞
    /// = max(4×GET_RESULT_TIMEOUT_MS, CALL_REPLY_TIMEOUT)。过早的
    /// `process::exit` 会把进程硬杀在一次尚未完成的硬件调用中途。任一侧
    /// 上限被调高时本断言立即编译失败，强制同步调高 QUIT_FALLBACK_MS。
    #[test]
    fn test_quit_fallback_exceeds_wmi_call_timeout() {
        // 编译期断言：宽限期必须同时覆盖"4 次 worker 侧超时"与"一次 GUI 侧
        // 应答超时（熔断首调用）"。引用真实常量而非硬编码，防止调高超时后
        // 静默击穿 15s 强杀预算。
        const _: () = assert!(
            QUIT_FALLBACK_MS >= 4 * crate::ec::wmi::GET_RESULT_TIMEOUT_MS as u64
                && QUIT_FALLBACK_MS >= crate::ec::wmi::CALL_REPLY_TIMEOUT.as_millis() as u64
        );
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
            performance_mode: crate::app::performance::PerfMode::Smart.ec_value(),
            battery_health_percent: None,
            battery_eta_text: None,
            notify_on_charge_limit: false,
        };
        let snap = crate::platform::power::PowerSnapshot {
            status: crate::platform::power::PowerStatus::OnAc,
            battery_percent: Some(80),
        };
        let tip = build_tooltip(&status, &snap);
        assert!(tip.contains("智能"), "perf name must appear: {}", tip);
        assert!(tip.contains("开启"), "care state must appear: {}", tip);
        assert!(tip.contains("80%"), "limit must appear: {}", tip);

        let off = TrayStatus {
            battery_care_enabled: false,
            ..status
        };
        assert!(build_tooltip(&off, &snap).contains("关闭"));
    }

    /// 托盘 tooltip 文案必须能写进 szTip（≤127 UTF-16 单元 + NUL）。
    ///
    /// ETA 段（`battery_eta_text`）是 tooltip 的**末段**（追加在 电源: 之后），
    /// 也是最易触顶截断的一段——用最长的实际估算文案（"预计充满约 23 小时
    /// 59 分钟"）做边界锁定：任何未来段（健康/性能名）长度增长都必须保持在
    /// 该最坏组合仍 ≤127，否则测试失败提示去调整顺序或文案（修订 1.46）。
    #[test]
    fn test_build_tooltip_fits_in_sz_tip() {
        let status = TrayStatus {
            battery_care_enabled: true,
            charge_limit: 80,
            performance_mode: crate::app::performance::PerfMode::Extreme.ec_value(),
            battery_health_percent: Some(78),
            battery_eta_text: Some("预计充满约 23 小时 59 分钟".to_string()),
            notify_on_charge_limit: false,
        };
        let snap = crate::platform::power::PowerSnapshot {
            status: crate::platform::power::PowerStatus::OnAc,
            battery_percent: Some(80),
        };
        let tip = build_tooltip(&status, &snap);
        assert!(tip.contains("健康:78%"), "health must appear: {}", tip);
        assert!(
            tip.contains("预计充满约 23 小时 59 分钟"),
            "ETA must appear: {}",
            tip
        );
        assert!(
            tip.encode_utf16().count() <= 127,
            "tooltip must fit szTip: {} ({} units)",
            tip,
            tip.encode_utf16().count()
        );
        let mut sz_tip = [0u16; 128];
        set_tip(&mut sz_tip, &tip);
        assert_eq!(
            sz_tip[tip.encode_utf16().count()],
            0,
            "must be NUL-terminated"
        );
    }

    /// 双击判定（修订 1.31）：间隔内的第二次单击视为双击；间隔外恢复单击。
    /// 线程局部时间戳在测试线程内自洽，不依赖真实点击。
    #[test]
    fn test_double_click_detection() {
        // 第一次单击：非双击。
        assert!(!is_double_click(), "first click must not be a double-click");
        // 紧接着第二次：双击（GetDoubleClickTime 默认 ≥100ms，两次调用间隙
        // 远小于该值）。
        assert!(
            is_double_click(),
            "immediate second click must be a double-click"
        );
        // 模拟"间隔已过"：把线程局部时间戳拨旧，第三次单击恢复为单击。
        let window = get_double_click_ms() as u64 + 100;
        LAST_CLICK.with(|c| {
            c.set(Some(
                std::time::Instant::now() - std::time::Duration::from_millis(window),
            ))
        });
        assert!(
            !is_double_click(),
            "click after the double-click window must be a single click"
        );
    }
}
