//! 主窗口（eframe 窗口）的显示控制。
//!
//! 托盘驻留通过**把窗口移到屏幕外**（`SetWindowPos` 到 -32000,-32000，保持
//! `WS_VISIBLE`）实现：winit 仍正常投递 `RedrawRequested` → `update()` 与
//! 托盘命令处理保持运行；任务栏不显示图标靠**扩展样式切换**（隐藏时换成
//! `WS_EX_TOOLWINDOW`，显示时恢复 `WS_EX_APPWINDOW`）。不能用
//! `ShowWindow(SW_HIDE)`：隐藏窗口不接收 `WM_PAINT`，winit 据此不再派发
//! `RedrawRequested`，`update()` 永久停止，托盘/热键/Fn+K 命令积压到窗口
//! 恢复才执行（实测回归，修订 1.19）。

use windows::Win32::Foundation::{GetLastError, HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    GWL_EXSTYLE, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE,
    SW_SHOW, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

use crate::util::WideString;

/// 居中回退时窗口的最小可接受尺寸（像素）。低于 `util::MIN_WINDOW_SIZE`
/// （400×500）——居中回退发生在窗口尺寸可能异常（副屏拔除残留/异常会话）
/// 时，按较小下限 clamp 到屏幕内即可，不必强制满足主窗口最小尺寸。
/// 命名收敛：字面量 320/200 曾各自书写三处（修订 1.47 清理）。
const MIN_CENTER_WIDTH_PX: i32 = 320;
const MIN_CENTER_HEIGHT_PX: i32 = 200;

/// eframe 窗口标题（`eframe::run_native` 的第一个参数）。
///
/// 必须与 `eframe::run_native`（gui/app.rs）的标题一致——`find_main_window`
/// 用 `FindWindowW` 按该标题定位主窗口，两者漂移会导致托盘隐藏/显示/退出
/// 静默失效。统一来自 `util::APP_NAME`（见该常量的注释）。
pub const MAIN_WINDOW_TITLE: &str = crate::util::APP_NAME;

/// 隐藏态窗口的离屏位置（负坐标，Windows 视为"移出可见区但保持 WS_VISIBLE"）。
///
/// 为什么不能 `ShowWindow(SW_HIDE)`：隐藏窗口不再接收 `WM_PAINT`，而 winit
/// 只有收到 `WM_PAINT` 才派发 `RedrawRequested` → eframe `update()` 永久
/// 停止，托盘/热键/Fn+K 发来的 `UiCommand` 全部积压到窗口恢复可见才执行
/// （实测回归，见 docs 修订 1.19）。改为**保持窗口可见但移到屏幕外**：
/// `WS_VISIBLE` 位仍在 → `WM_PAINT` 照常到达 → update 循环不断 → 命令被
/// 实时消费；屏幕外位置使用户看不到窗口、任务栏不占位。
const HIDDEN_POS: (i32, i32) = (-32000, -32000);

/// 隐藏前记录的窗口在屏位置（Show 恢复用）。
///
/// 历史实现把窗口隐藏到 `HIDDEN_POS` 后，`show_main_window` 总是把窗口
/// **居中到主屏**（`GetSystemMetrics(SM_CXSCREEN/SM_CYSCREEN)`），用户把
/// 窗口拖到副屏/角落的偏好每次隐藏-显示都会丢失（L1 回归）。修复：隐藏时
/// 用 `SWP_NOSIZE` 移走、先把当前位置记到此处，显示时优先恢复到该位置，
/// 仅当记录位置不在任何屏的虚拟屏幕范围内（拔掉副屏等）才回退居中。
static LAST_POS: std::sync::Mutex<Option<(i32, i32)>> = std::sync::Mutex::new(None);

/// 窗口扩展样式的读-改-写串行化（修订 1.44，评审第 5 轮）。
///
/// `hide_main_window` 与 `show_main_window` 分别在**两个线程**运行：GUI 线程
/// （`--autostart` 首帧隐藏、关闭按钮路径）与托盘 worker 线程（托盘 toggle）。
/// 两者都对 `GWL_EXSTYLE` 做 `GetWindowLongPtrW → 清/置位 → SetWindowLongPtrW`
/// 的非原子序列——并发交错时后写者基于旧值重建，丢失对方的位更新：隐藏态
/// 残留 `WS_EX_APPWINDOW`（任务栏对离屏"隐藏"窗口显示按钮）或可见态残留
/// `WS_EX_TOOLWINDOW`（有窗口无任务栏按钮），正是本模块文档声称防止的状态。
/// 用互斥锁把读-改-写收成临界区，消除丢失更新。
static EXSTYLE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 按标题查找主窗口句柄（**不做进程归属校验**）。
///
/// `find_main_window` 的底层：FindWindowW 只按标题匹配。进程归属校验
/// （PID 比对）由调用方按场景决定——同进程的 hide/show/quit 必须校验；
/// **唤醒另一实例**（单实例保护的第二实例路径）恰恰需要对方进程的窗口。
fn find_main_window_by_title() -> Option<HWND> {
    let title = WideString::new(MAIN_WINDOW_TITLE);
    let hwnd = unsafe { FindWindowW(None, title.as_pcwstr()) }.ok()?;
    if hwnd.0.is_null() {
        return None;
    }
    Some(hwnd)
}

/// 校验窗口确实属于当前进程（修订 1.46）：FindWindowW 只按标题匹配，其它
/// 进程的"同标题窗口"（无关应用或攻击者伪造的 UI）不得被 hide/show/quit
/// 误操作。返回 `Some` 时窗口属于本进程；`None` 时记录告警。
fn require_own_process(hwnd: HWND) -> Option<HWND> {
    let mut owner_pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner_pid)) };
    if owner_pid == std::process::id() {
        Some(hwnd)
    } else {
        log::warn!(
            "FindWindowW matched title '{}' but window belongs to PID {} (not {}); ignoring",
            MAIN_WINDOW_TITLE,
            owner_pid,
            std::process::id()
        );
        None
    }
}

/// 校验窗口确实属于当前进程后返回句柄（供本模块与 `platform::icon` 共用，
/// 后者设置窗口图标时需要定位主窗口）。
pub(crate) fn find_main_window() -> Option<HWND> {
    find_main_window_by_title().and_then(require_own_process)
}

/// 主窗口当前是否可见（任务栏图标随可见性出现/消失）。
///
/// 隐藏态用"窗口移出屏幕外"实现（见 `HIDDEN_POS`）：`WS_VISIBLE` 位仍在，
/// `IsWindowVisible` 恒返回 true，不能直接用它判定——改为比较窗口位置，
/// 位于隐藏坐标即视为隐藏。**最小化窗口同样报告 `(-32000,-32000)` 位置**
/// （修订 1.46 审计）：最小化到任务栏的窗口是"可见"状态（通知门控应放行
/// 托盘气泡），用 `IsIconic` 区分最小化与离屏隐藏——最小化窗口视为可见，
/// 离屏隐藏（且非最小化）才视为隐藏。
pub fn main_window_visible() -> bool {
    find_main_window()
        .map(|hwnd| unsafe { IsWindowVisible(hwnd).as_bool() && !window_at_hidden_pos(hwnd) })
        .unwrap_or(false)
}

/// 窗口矩形是否位于隐藏原点（-32000,-32000）。
///
/// hide/show 两侧各自内联书写过同一 `left == HIDDEN_POS.0 && top ==
/// HIDDEN_POS.1` 判定（修订 1.49 整理），收敛为单一谓词。
fn rect_at_hidden_pos(rect: &RECT) -> bool {
    rect.left == HIDDEN_POS.0 && rect.top == HIDDEN_POS.1
}

/// 窗口是否位于隐藏原点（-32000,-32000）且未被最小化。
fn window_at_hidden_pos(hwnd: HWND) -> bool {
    let mut rect = unsafe { std::mem::zeroed() };
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return false;
    }
    // 最小化窗口也会把 GetWindowRect 报告为 (-32000,-32000)：只有"非最小化
    // 且位于隐藏坐标"才是本应用的隐藏态（见 main_window_visible 注释）。
    if unsafe { IsIconic(hwnd).as_bool() } {
        return false;
    }
    rect_at_hidden_pos(&rect)
}

/// 判断给定的窗口左上角（x, y）是否位于**虚拟屏幕**（所有监视器的并集，
/// 坐标可为负）范围内。
///
/// 记录在 `LAST_POS` 的位置可能已失效：副屏被拔出、分辨率变更等都会使
/// 保存坐标落到屏幕外。此时若原样恢复窗口会"看不见"（只能靠托盘重新
/// 显示），必须回退居中。用虚拟屏幕（`GetSystemMetrics` 的
/// `SM_XVIRTUALSCREEN`/`SM_YVIRTUALSCREEN`/`SM_CXVIRTUALSCREEN`/
/// `SM_CYVIRTUALSCREEN`）判定，比主屏尺寸更准确（多显示器场景）。
fn saved_pos_on_screen(x: i32, y: i32, w: i32, h: i32) -> bool {
    let vx = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let vy = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let vw = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let vh = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if vw <= 0 || vh <= 0 {
        return false;
    }
    // 位置与窗口尺寸构成的矩形与虚拟屏幕有交集即认为可用（不要求完全在屏
    // 内：窗口可以部分在屏内，用户能手动拖回）。
    x + w > vx && x < vx + vw && y + h > vy && y < vy + vh
}

/// 隐藏主窗口：移到屏幕外（保持 WS_VISIBLE，update 循环继续，见 HIDDEN_POS
/// 的注释），并把扩展样式从"应用窗口"（WS_EX_APPWINDOW，任务栏显示按钮）
/// 换成"工具窗口"（WS_EX_TOOLWINDOW，任务栏不显示按钮）。仅"移出屏幕 +
/// 保留 WS_VISIBLE"时任务栏仍显示按钮（实测，修订 1.19）——必须同时切换
/// 扩展样式才能真正驻留托盘。`main_window_visible` 按位置判定隐藏。
pub fn hide_main_window() {
    if let Some(hwnd) = find_main_window() {
        log::info!("Hide main window 0x{:X} (offscreen)", hwnd.0 as usize);
        // **最小化窗口先还原再隐藏**（修订 1.46 审计）：若窗口处于最小化
        // （IsIconic）状态直接移到 HIDDEN_POS，会得到一个"离屏但仍是
        // 最小化"的窗口——`window_at_hidden_pos` 对最小化窗口返回 false
        // （最小化窗口的 GetWindowRect 同样报告 -32000,-32000，须用
        // IsIconic 区分，见该函数注释），`main_window_visible` 恒返回
        // true，托盘下次 toggle 走 hide 分支永不 show，窗口卡死。先
        // SW_RESTORE 把窗口还原到在屏位置，后续记录位置/移屏外才是
        // 普通的"可见→离屏"转换。
        if unsafe { IsIconic(hwnd).as_bool() } {
            let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
        }
        // 记录当前在屏位置（Show 时恢复，见 LAST_POS 注释）。
        let mut rect: RECT = unsafe { std::mem::zeroed() };
        let on_screen =
            unsafe { GetWindowRect(hwnd, &mut rect).is_ok() } && !rect_at_hidden_pos(&rect);
        if on_screen {
            // 与 EXSTYLE_LOCK 同一毒锁恢复约定：LAST_POS 的记录/读取
            // 在 hide/show 两处访问，持锁 panic 会毒化——恢复比永久
            // 丢失窗口位置更可取（L5 回归，与其他锁统一收敛）。
            let mut guard = crate::util::lock_or_recover(&LAST_POS, "last pos");
            *guard = Some((rect.left, rect.top));
        }
        // 去掉 WS_EX_APPWINDOW、加上 WS_EX_TOOLWINDOW：任务栏按钮消失。
        // 读-改-写收进互斥锁临界区，防止与 show 侧并发交错丢失位更新
        // （见 EXSTYLE_LOCK 注释）。经 lock_or_recover 获取：临界区是 FFI
        // 读-改-写，持锁 panic 会毒化——恢复毒锁比永久损坏更可取（与全
        // 项目锁恢复约定一致）。
        {
            let _guard = crate::util::lock_or_recover(&EXSTYLE_LOCK, "exstyle");
            set_exstyle_bits(
                hwnd,
                WS_EX_APPWINDOW.0 as isize,
                WS_EX_TOOLWINDOW.0 as isize,
                "Hide",
            );
        }
        if unsafe {
            SetWindowPos(
                hwnd,
                None,
                HIDDEN_POS.0,
                HIDDEN_POS.1,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
        .is_err()
        {
            log::warn!(
                "Hide: SetWindowPos (offscreen) failed: {:#x}",
                unsafe { GetLastError() }.0
            );
        }
    } else {
        log::warn!("Hide main window: not found");
    }
}

/// 显示并激活主窗口：恢复"应用窗口"扩展样式（任务栏显示按钮）、恢复到
/// 屏幕上的位置（原位置不可恢复时默认居中）。
pub fn show_main_window() {
    if let Some(hwnd) = find_main_window() {
        log::info!("Show main window 0x{:X}", hwnd.0 as usize);
        show_window(hwnd);
    } else {
        log::warn!("Show main window: not found");
    }
}

/// 唤醒**另一实例**的主窗口（单实例保护的第二实例路径，main.rs）。
///
/// 与 `show_main_window` 的差异：**不做进程归属校验**——此时窗口必然属于
/// 另一实例（本进程没有自己的主窗口），`find_main_window_by_title` 命中的
/// 正是它的窗口。进程归属校验（1.46）用于防止"同名窗口误操作"，
/// 只约束本进程的 hide/show/quit；唤醒其它实例本来就要操作对方窗口。
pub fn wake_existing_window() {
    if let Some(hwnd) = find_main_window_by_title() {
        log::info!("Wake existing instance window 0x{:X}", hwnd.0 as usize);
        show_window(hwnd);
    } else {
        log::warn!("Wake existing instance: main window not found");
    }
}

/// 把主窗口恢复到可见并激活：恢复"应用窗口"扩展样式（任务栏显示按钮）、
/// 恢复到屏幕上的位置（原位置不可恢复时默认居中）。`show_main_window` 与
/// `wake_existing_window` 共用（后者是另一实例的窗口，逻辑相同）。
fn show_window(hwnd: HWND) {
    // 恢复任务栏样式（与 hide_main_window 的交换对称）。读-改-写同样收进
    // EXSTYLE_LOCK 临界区（见该常量注释）；lock_or_recover 恢复毒锁。
    {
        let _guard = crate::util::lock_or_recover(&EXSTYLE_LOCK, "exstyle");
        set_exstyle_bits(
            hwnd,
            WS_EX_TOOLWINDOW.0 as isize,
            WS_EX_APPWINDOW.0 as isize,
            "Show",
        );
    }
    // ShowWindow 返回 0 = 窗口此前已隐藏（SW_RESTORE 恢复前可见性），
    // 非错误；但记录 last-error 便于排查"显示后仍不可见"类问题。
    let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
    let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
    // 从隐藏位置拖回屏幕内：
    // 若当前不在隐藏位置（如用户手动拖动后点击托盘），位置不变。
    if window_at_hidden_pos(hwnd) {
        // 保留用户调整过的窗口尺寸：隐藏时用 SWP_NOSIZE 移走，尺寸
        // 未变，GetWindowRect 的宽高仍是用户最后的窗口大小。只有
        // 尺寸非法（≤0）或过大（超过屏幕）时才回退默认尺寸。
        let mut rect: RECT = unsafe { std::mem::zeroed() };
        let (mut w, mut h) = (
            crate::util::DEFAULT_WINDOW_SIZE.0 as i32,
            crate::util::DEFAULT_WINDOW_SIZE.1 as i32,
        );
        if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() {
            let rw = rect.right - rect.left;
            let rh = rect.bottom - rect.top;
            if rw > 0 && rh > 0 {
                w = rw;
                h = rh;
            }
        }
        // 恢复隐藏前记录的在屏位置（L1 修复）；仅当记录位置落在
        // 虚拟屏幕范围外（副屏被拔掉等）时才回退居中。
        let saved = {
            let guard = crate::util::lock_or_recover(&LAST_POS, "last pos");
            (*guard).filter(|(x, y)| saved_pos_on_screen(*x, *y, w, h))
        };
        let (x, y) = match saved {
            Some((sx, sy)) => (sx, sy),
            None => {
                let sw = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(MIN_CENTER_WIDTH_PX);
                let sh = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(MIN_CENTER_HEIGHT_PX);
                // 窗口比屏幕大时收到屏幕内（宽度至少 MIN_CENTER_WIDTH_PX，
                // 避免极端用户拖动到副屏后主屏显示不全）。GetSystemMetrics
                // 在异常会话中可能返回 0，.max(…) 保证 clamp 下限不 panic
                //（修订 1.46 审计：原 `w.clamp(320, sw)` 在 sw<320 时 panic）。
                let w = w.clamp(MIN_CENTER_WIDTH_PX, sw);
                let h = h.clamp(MIN_CENTER_HEIGHT_PX, sh);
                let x = (sw - w) / 2;
                let y = (sh - h) / 2;
                (x, y)
            }
        };
        let _ = unsafe { SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE) };
    }
    // 托盘点击/第二实例触发，属于用户交互上下文，一般可成功置前。
    if unsafe { SetForegroundWindow(hwnd) }.0 == 0 {
        log::warn!(
            "Show: SetForegroundWindow failed: {:#x}",
            unsafe { GetLastError() }.0
        );
    }
}

/// 扩展样式读-改-写的统一实现（hide/show 交换 EXSWIN 的同一份样板，修订
/// 1.47 收敛）。调用方必须持有 `EXSTYLE_LOCK` 临界区。
///
/// 失败判定（修订 1.46）：`SetWindowLongPtrW` 返回 0 **且** `GetLastError`
/// 非零才是失败——窗口上一值恰好为 0 时返回值 0 属正常。`GetLastError`
/// 只读取一次，避免历史实现"判定处读一次、日志里再读一次"读到被后续
/// 调用改写的陈旧 last-error。
fn set_exstyle_bits(hwnd: HWND, clear: isize, set: isize, what: &str) {
    let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    if ex != 0 {
        let new_ex = (ex & !clear) | set;
        let ret = unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex) };
        if ret == 0 {
            let err = unsafe { GetLastError() }.0;
            if err != 0 {
                log::warn!("{}: SetWindowLongPtrW failed: {:#x}", what, err);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_main_window_title_is_stable() {
        assert_eq!(MAIN_WINDOW_TITLE, "Xiaomi PC Manager Lite");
    }

    /// 隐藏坐标必须在屏幕外（负值，Windows 视为"移出可见区"），
    /// 且保持可判定：窗口位于该坐标时 main_window_visible 应判为隐藏。
    #[test]
    fn test_hidden_pos_is_offscreen_negative() {
        assert!(
            HIDDEN_POS.0 < 0 && HIDDEN_POS.1 < 0,
            "hidden position must be off-screen (negative)"
        );
        // 与 -16000 的差说明坐标足够负、会被系统钳到屏幕外。
        assert!(HIDDEN_POS.0 <= -1000 && HIDDEN_POS.1 <= -1000);
    }

    /// 保存位置在虚拟屏幕范围内时必须被接受（L1：托盘隐藏-显示保留用户
    /// 把窗口拖到副屏/角落的位置，不再每次居中回主屏）。测试不假设
    /// 显示器数量：用当前虚拟屏幕的实际边界构造"必然在屏内"与"必然在屏外"
    /// 的坐标，保证在任何机器上断言都成立。
    #[test]
    fn test_saved_pos_virtual_screen_bounds() {
        let vx = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        let vy = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
        let vw = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        let vh = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
        assert!(vw > 0 && vh > 0, "virtual screen must be non-empty");
        // 虚拟屏幕起点：必在屏内。
        assert!(saved_pos_on_screen(vx, vy, 520, 680));
        // 虚拟屏幕内部一点：必在屏内。
        assert!(saved_pos_on_screen(vx + 40, vy + 40, 520, 680));
        // 明显越出虚拟屏幕（-16000 与隐藏坐标同级）：必在屏外。
        assert!(!saved_pos_on_screen(-16000, -16000, 520, 680));
        // 越出右/下边界（偏移超过虚拟屏宽高）：必在屏外。
        assert!(!saved_pos_on_screen(vx + vw + 500, vy + vh + 500, 520, 680));
    }
}
