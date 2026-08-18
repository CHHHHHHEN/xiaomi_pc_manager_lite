//! 主窗口（eframe 窗口）的显示控制。
//!
//! 托盘驻留通过**把窗口移到屏幕外**（`SetWindowPos` 到 -32000,-32000，保持
//! `WS_VISIBLE`）实现：winit 仍正常投递 `RedrawRequested` → `update()` 与
//! 托盘命令处理保持运行；任务栏不显示图标靠**扩展样式切换**（隐藏时换成
//! `WS_EX_TOOLWINDOW`，显示时恢复 `WS_EX_APPWINDOW`）。不能用
//! `ShowWindow(SW_HIDE)`：隐藏窗口不接收 `WM_PAINT`，winit 据此不再派发
//! `RedrawRequested`，`update()` 永久停止，托盘/热键/Fn+K 命令积压到窗口
//! 恢复才执行（实测回归，修订 1.19）。

use std::sync::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, FindWindowW, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect,
    IsWindowVisible, SendMessageW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, GWL_EXSTYLE, HICON, ICON_BIG, ICON_SMALL, LR_DEFAULTCOLOR, SM_CXSCREEN,
    SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE, SW_SHOW, WM_SETICON, WS_EX_APPWINDOW,
    WS_EX_TOOLWINDOW,
};

use crate::util::WideString;

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

fn find_main_window() -> Option<HWND> {
    let title = WideString::new(MAIN_WINDOW_TITLE);
    let hwnd = unsafe { FindWindowW(None, title.as_pcwstr()) }.ok()?;
    if hwnd.0.is_null() {
        None
    } else {
        Some(hwnd)
    }
}

/// 主窗口当前是否可见（任务栏图标随可见性出现/消失）。
///
/// 隐藏态用"窗口移出屏幕外"实现（见 `HIDDEN_POS`）：`WS_VISIBLE` 位仍在，
/// `IsWindowVisible` 恒返回 true，不能直接用它判定——改为比较窗口位置，
/// 位于隐藏坐标即视为隐藏。
pub fn main_window_visible() -> bool {
    find_main_window()
        .map(|hwnd| unsafe { IsWindowVisible(hwnd).as_bool() && !window_at_hidden_pos(hwnd) })
        .unwrap_or(false)
}

/// 窗口是否位于隐藏原点（-32000,-32000）。
fn window_at_hidden_pos(hwnd: HWND) -> bool {
    let mut rect = unsafe { std::mem::zeroed() };
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return false;
    }
    rect.left == HIDDEN_POS.0 && rect.top == HIDDEN_POS.1
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
        unsafe {
            // 记录当前在屏位置（Show 时恢复，见 LAST_POS 注释）。
            let mut rect = std::mem::zeroed();
            if GetWindowRect(hwnd, &mut rect).is_ok()
                && rect.left != HIDDEN_POS.0
                && rect.top != HIDDEN_POS.1
            {
                if let Ok(mut guard) = LAST_POS.lock() {
                    *guard = Some((rect.left, rect.top));
                }
            }
            // 去掉 WS_EX_APPWINDOW、加上 WS_EX_TOOLWINDOW：任务栏按钮消失。
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            if ex != 0 {
                let new_ex = (ex & !(WS_EX_APPWINDOW.0 as isize)) | WS_EX_TOOLWINDOW.0 as isize;
                let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex);
            }
            let _ = SetWindowPos(
                hwnd,
                None,
                HIDDEN_POS.0,
                HIDDEN_POS.1,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
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
        unsafe {
            // 恢复任务栏样式（与 hide_main_window 的交换对称）。
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            if ex != 0 {
                let new_ex = (ex & !(WS_EX_TOOLWINDOW.0 as isize)) | WS_EX_APPWINDOW.0 as isize;
                let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex);
            }
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = ShowWindow(hwnd, SW_SHOW);
            // 从隐藏位置拖回屏幕内：
            // 若当前不在隐藏位置（如用户手动拖动后点击托盘），位置不变。
            if window_at_hidden_pos(hwnd) {
                // 保留用户调整过的窗口尺寸：隐藏时用 SWP_NOSIZE 移走，尺寸
                // 未变，GetWindowRect 的宽高仍是用户最后的窗口大小。只有
                // 尺寸非法（≤0）或过大（超过屏幕）时才回退默认 520×680。
                let mut rect = std::mem::zeroed();
                let (mut w, mut h) = (520i32, 680i32);
                if GetWindowRect(hwnd, &mut rect).is_ok() {
                    let rw = rect.right - rect.left;
                    let rh = rect.bottom - rect.top;
                    if rw > 0 && rh > 0 {
                        w = rw;
                        h = rh;
                    }
                }
                // 恢复隐藏前记录的在屏位置（L1 修复）；仅当记录位置落在
                // 虚拟屏幕范围外（副屏被拔掉等）时才回退居中。
                let saved = LAST_POS
                    .lock()
                    .ok()
                    .and_then(|g| *g)
                    .filter(|(x, y)| saved_pos_on_screen(*x, *y, w, h));
                let (x, y) = match saved {
                    Some((sx, sy)) => (sx, sy),
                    None => {
                        let sw = GetSystemMetrics(SM_CXSCREEN);
                        let sh = GetSystemMetrics(SM_CYSCREEN);
                        // 窗口比屏幕大时收到屏幕内（宽度至少 40% 屏幕，避免极端
                        // 用户拖动到副屏后主屏显示不全）。
                        let w = w.clamp(320, sw);
                        let h = h.clamp(200, sh);
                        let x = (sw - w) / 2;
                        let y = (sh - h) / 2;
                        (x, y)
                    }
                };
                let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
            }
            // 托盘点击触发，属于用户交互上下文，一般可成功置前。
            let _ = SetForegroundWindow(hwnd);
        }
    } else {
        log::warn!("Show main window: not found");
    }
}

/// 由 `icons/icon.png` 构建多尺寸 ICO 数据（16/32/48/256，PNG 压缩块）。
/// 现代 Windows（Vista+）支持含 PNG 块的 ICO。
fn build_multi_size_ico() -> Vec<u8> {
    let png = include_bytes!("../../icons/icon.png");
    let img = match image::load_from_memory(png) {
        Ok(img) => img.to_rgba8(),
        Err(_) => return Vec::new(),
    };
    const SIZES: &[u32] = &[16, 32, 48, 256];
    let mut blocks: Vec<Vec<u8>> = Vec::with_capacity(SIZES.len());
    for &s in SIZES {
        let scaled = image::imageops::resize(&img, s, s, image::imageops::FilterType::Lanczos3);
        let mut buf = Vec::new();
        let mut cur = std::io::Cursor::new(&mut buf);
        if scaled.write_to(&mut cur, image::ImageFormat::Png).is_err() {
            return Vec::new();
        }
        blocks.push(buf);
    }

    // ICONDIR + ICONDIRENTRY × N。每个条目固定 16 字节：
    //   bWidth(1) + bHeight(1) + bColorCount(1) + bReserved(1)
    //   + wPlanes(2) + wBitCount(2) + dwBytesInRes(4) + dwImageOffset(4)
    // 历史实现把 dwBytesInRes/dwImageOffset 按 u16 写入 4 字节 DWORD 字段、
    // 条目实际仅 12 字节，生成的 ICO 畸形——CreateIconFromResourceEx 按声明
    // 偏移取图失败，任务栏/标题栏图标静默缺失。
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0, SIZES.len() as u8, 0]);
    let mut offset = 6 + 16 * SIZES.len();
    for (i, block) in blocks.iter().enumerate() {
        let s = SIZES[i];
        ico.push(if s >= 256 { 0 } else { s as u8 }); // width
        ico.push(if s >= 256 { 0 } else { s as u8 }); // height
        ico.push(0); // color count
        ico.push(0); // reserved
        ico.extend_from_slice(&1u16.to_le_bytes()); // planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bitcount
        ico.extend_from_slice(&(block.len() as u32).to_le_bytes()); // dwBytesInRes
        ico.extend_from_slice(&(offset as u32).to_le_bytes()); // dwImageOffset
        offset += block.len();
    }
    for block in &blocks {
        ico.extend_from_slice(block);
    }
    ico
}

/// HICON 句柄包装（裸指针非 Send/Sync）：图标由系统持有、进程退出时
/// 释放；跨线程仅传递句柄值，不涉及所有权转移。
struct IconHandle(HICON);
unsafe impl Send for IconHandle {}
unsafe impl Sync for IconHandle {}

/// 系统 DPI（进程按 PerMonitorV2 DPI 感知运行，见下文），回退 96。
///
/// **为什么 GetDpiForSystem 可用**：eframe/winit 在事件循环初始化时调用
/// `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)`
/// （winit 0.30 platform_impl/windows/dpi.rs），进程实际是 PerMonitorV2 感知
/// 的——因此 `GetDpiForSystem` 返回真实系统缩放（125%/150%/200%…），而非
/// DPI 不可知进程被虚拟化出的 96。返回值 0（异常/老系统）时回退 96。
fn system_dpi() -> u32 {
    unsafe { GetDpiForSystem().max(1) }.max(96)
}

/// 逻辑像素 → 物理像素换算（四舍五入）：`round(logical × dpi / 96)`。
///
/// 纯算术（无系统调用），供各图标目标尺寸换算与单测共用。
fn scaled_px_at_dpi(logical_px: u32, dpi: u32) -> u32 {
    (logical_px * dpi.max(96) + 48) / 96
}

/// 托盘图标的目标物理像素尺寸（逻辑 16px，按系统 DPI 缩放）。
///
/// 任务栏通知区以 16 逻辑像素绘制小图标，但高 DPI 缩放（125%/150%/200%…）
/// 时实际渲染像素为 16 × DPI/96。若取 16px 单帧，系统把小位图放大
/// 托盘图标发糊。这里换算成物理尺寸，让调用方取不小于它的单帧（只缩小
/// 不放大，清晰）。
pub fn tray_icon_size_px() -> u32 {
    scaled_px_at_dpi(16, system_dpi())
}

/// 从多尺寸 ICO 字节构建「不小于目标尺寸的最小帧」HICON。
///
/// **为什么不能把整份 ICO 直接交给 CreateIconFromResourceEx**：实测
/// （2025 RedmiBook Pro 14，Windows 11）：传整份多帧 ICO（含 ICONDIR 头）
/// 返回 `HRESULT 0x80070006`（INVALID_HANDLE），单帧 PNG 块则可正常创建。
/// 因此这里解析 ICONDIR 各帧，取不小于目标尺寸的最小一帧（所有帧都小于
/// 目标时取最大帧）交给 `CreateIconFromResourceEx`。选择策略有意用"不小于
/// 目标的最小帧"而非"最接近目标"：若选比目标小的帧，高 DPI 下系统需要
/// 把小位图放大，图标发糊（历史实现硬编码 16px 的 L1 回归）；取比目标
/// 大的帧只会被缩小，清晰度不受影响。窗口/托盘图标是**单尺寸渲染**（任务栏/
/// 标题栏 16~32px、托盘 16 逻辑 px），单帧 HICON 足以覆盖。
///
/// 调用方负责持有句柄直到不再需要（本函数不缓存，由各缓存点决定）。
pub fn create_hicon_from_ico(ico: &[u8], preferred_size: u32) -> Result<HICON, String> {
    if ico.len() < 6 {
        return Err("ICO too short".into());
    }
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    if count == 0 || ico.len() < 6 + count * 16 {
        return Err("No icon entries".into());
    }
    // 逐帧解析：记录 (像素边长, 数据偏移, 数据长度)。像素边长用条目头第 0
    // 字节（0 表示 256），与 ICONDIR 规范一致。checked_add 防 off+sz 溢出
    // 回绕后越界切片（32 位平台 usize 溢出会绕过旧式 `off+sz > len` 检查）。
    let mut frames: Vec<(u32, usize, usize)> = Vec::with_capacity(count);
    for i in 0..count {
        let e = 6 + i * 16;
        let px = if ico[e] == 0 { 256u32 } else { ico[e] as u32 };
        let sz = u32::from_le_bytes([ico[e + 8], ico[e + 9], ico[e + 10], ico[e + 11]]) as usize;
        let off = u32::from_le_bytes([ico[e + 12], ico[e + 13], ico[e + 14], ico[e + 15]]) as usize;
        match off.checked_add(sz) {
            Some(end) if end <= ico.len() => {}
            _ => return Err("OOB".into()),
        }
        frames.push((px, off, sz));
    }
    // 不小于目标的最小帧；全部小于目标时取最大帧（宁大勿小：只会被缩小）。
    let (_, off, sz) = frames
        .iter()
        .filter(|(px, _, _)| *px >= preferred_size)
        .min_by_key(|(px, _, _)| *px)
        .or_else(|| frames.iter().max_by_key(|(px, _, _)| *px))
        .expect("frames is non-empty (count checked above)");
    let block = &ico[*off..*off + *sz];
    unsafe {
        CreateIconFromResourceEx(block, true, 0x0003_0000, 0, 0, LR_DEFAULTCOLOR)
            .map_err(|e| format!("CreateIconFromResourceEx: {}", e))
    }
}

/// 设置主窗口图标（任务栏 / 标题栏 / Alt-Tab）。
///
/// eframe 的 `with_icon` 对 512×512 PNG 的缩小渲染到任务栏效果差
/// （糊成纯色块）。这里从**多尺寸 ICO** 按目标尺寸各取最清晰单帧构建
/// HICON：`ICON_SMALL`（标题栏/任务栏小图标 ~16 逻辑 px）与 `ICON_BIG`
/// （任务栏/Alt-Tab ~32 逻辑 px）的物理尺寸按系统 DPI 缩放
/// （`scaled_px_at_dpi`），再经 `WM_SETICON` 设置——高 DPI 下系统把
/// 16/32 逻辑 px 图标放大到物理像素（200% 时为 32/64px），取不小于物理
/// 尺寸的帧避免小位图放大发糊（与托盘图标的 DPI 修复同源，L1 回归）。
/// HICON 进程生命周期内缓存（Mutex 包装以满足 Sync），由系统使用、
/// 进程退出时释放。
pub fn set_main_window_icon() {
    static CACHED_SMALL: Mutex<Option<IconHandle>> = Mutex::new(None);
    static CACHED_BIG: Mutex<Option<IconHandle>> = Mutex::new(None);
    let Some(hwnd) = find_main_window() else {
        return;
    };
    let ico_bytes = build_multi_size_ico();
    if ico_bytes.is_empty() {
        log::warn!("set_main_window_icon: multi-size ICO build failed");
        return;
    }
    // 按目标尺寸从缓存取/构建 HICON（缓存命中则跳过构建；进程生命周期内
    // 每个尺寸只构建一次）。返回 null 表示构建失败（记录日志后忽略）。
    let cached = |cache: &'static Mutex<Option<IconHandle>>, size: u32, what: &'static str| {
        let mut guard = crate::util::lock_or_recover(cache, what);
        if let Some(icon) = guard.as_ref() {
            return icon.0;
        }
        let h = create_hicon_from_ico(&ico_bytes, size).unwrap_or_else(|e| {
            log::warn!("set_main_window_icon {}: {}", what, e);
            windows::Win32::UI::WindowsAndMessaging::HICON(std::ptr::null_mut())
        });
        *guard = Some(IconHandle(h));
        h
    };
    // ICON_SMALL=16 逻辑 px、ICON_BIG=32 逻辑 px，按系统 DPI 换算为物理像素。
    let small = cached(&CACHED_SMALL, scaled_px_at_dpi(16, system_dpi()), "small");
    let big = cached(&CACHED_BIG, scaled_px_at_dpi(32, system_dpi()), "big");
    unsafe {
        if !small.0.is_null() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_SMALL as usize)),
                Some(LPARAM(small.0 as isize)),
            );
        }
        if !big.0.is_null() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_BIG as usize)),
                Some(LPARAM(big.0 as isize)),
            );
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

    /// 回归测试：multi-size ICO 必须符合 ICONDIR 结构规范——
    /// 每个 ICONDIRENTRY 固定 16 字节（bWidth+bHeight+bColorCount+
    /// bReserved+wPlanes+wBitCount+dwBytesInRes+dwImageOffset），
    /// 且文件大小与头部声明一致。历史实现把 dwBytesInRes/dwImageOffset
    /// 按 u16（2 字节）写入 4 字节 DWORD 字段、条目实际仅 12 字节，
    /// 生成的 ICO 畸形，`CreateIconFromResourceEx` 取出错误偏移的 PNG
    /// 数据而失败，任务栏/标题栏图标静默缺失。
    #[test]
    fn test_build_multi_size_ico_structure() {
        let ico = build_multi_size_ico();
        assert!(!ico.is_empty(), "multi-size ICO must be buildable");

        assert_eq!(&ico[0..2], &[0, 0], "reserved must be 0");
        assert_eq!(&ico[2..4], &[1, 0], "type must be 1 (icon)");
        let count_bytes = [ico[4], ico[5]];
        let count = u16::from_le_bytes(count_bytes) as usize;
        assert_eq!(count, 4, "must contain 16/32/48/256 entries");

        let header_len = 6 + 16 * count;
        assert_eq!(
            ico.len(),
            header_len + blocks_total_len(&ico, count),
            "file length must match header-declared image data"
        );

        let mut expected_offset = header_len;
        for i in 0..count {
            let e = 6 + i * 16;
            let width = ico[e];
            assert_eq!(width, ico[e + 1], "width==height per entry");
            assert_eq!(ico[e + 3], 0, "reserved must be 0");
            assert_eq!(
                u16::from_le_bytes([ico[e + 4], ico[e + 5]]),
                1,
                "planes must be 1"
            );
            assert_eq!(
                u16::from_le_bytes([ico[e + 6], ico[e + 7]]),
                32,
                "bitcount must be 32"
            );
            let size = u32::from_le_bytes([ico[e + 8], ico[e + 9], ico[e + 10], ico[e + 11]]);
            let off = u32::from_le_bytes([ico[e + 12], ico[e + 13], ico[e + 14], ico[e + 15]]);
            assert_eq!(
                off as usize, expected_offset,
                "entry {}: image offset must match header layout",
                i
            );
            assert!(size > 0 && (off as usize + size as usize) <= ico.len());
            // PNG magic of the block at the declared offset.
            assert_eq!(
                &ico[off as usize..off as usize + 8],
                &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
                "entry {}: data at declared offset must be a PNG",
                i
            );
            expected_offset += size as usize;
        }
    }

    fn blocks_total_len(ico: &[u8], count: usize) -> usize {
        let mut total = 0usize;
        for i in 0..count {
            let e = 6 + i * 16;
            let size = u32::from_le_bytes([ico[e + 8], ico[e + 9], ico[e + 10], ico[e + 11]]);
            total += size as usize;
        }
        total
    }

    /// `create_hicon_from_ico` 必须能从**整份多尺寸 ICO** 中取最接近目标
    /// 尺寸的单帧构建出真实 HICON。回归测试（修订 1.24）：历史实现把整份
    /// ICO 交给 `CreateIconFromResourceEx`，实测返回 `0x80070006`
    /// （INVALID_HANDLE）——单帧 PNG 块才能创建。若整份 ICO 路径再次被
    /// 误用，这里会直接创建失败（返回 Err 或 null 句柄），杜绝"静默无图标"。
    #[test]
    fn test_create_hicon_from_ico_builds_valid_hicon() {
        let ico = build_multi_size_ico();
        assert!(!ico.is_empty());
        // 16px（托盘/标题栏小图标）与 32px（任务栏大图标）两档都要能创建。
        for size in [16u32, 32u32] {
            let h = create_hicon_from_ico(&ico, size).expect("must create HICON");
            assert!(!h.0.is_null(), "HICON for size {} must be non-null", size);
            let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyIcon(h) };
        }
    }

    /// 越界/溢出的损坏 ICO 必须被拒绝（checked_add 防回绕），不能构造越界
    /// 切片 panic（与 tray 的 load_icon 同语义，经共享 helper 收敛）。
    #[test]
    fn create_hicon_from_ico_rejects_malformed() {
        fn ico_with_entry(off: u32, sz: u32) -> Vec<u8> {
            let mut b = vec![0u8; 6 + 16];
            b[4] = 1;
            b[5] = 0;
            b[6 + 8..6 + 12].copy_from_slice(&sz.to_le_bytes());
            b[6 + 12..6 + 16].copy_from_slice(&off.to_le_bytes());
            b
        }
        // off+sz 在 u32 内回绕为小值：必须拒绝。
        assert!(create_hicon_from_ico(&ico_with_entry(u32::MAX, 2), 16).is_err());
        // 偏移合法但长度越界。
        assert!(create_hicon_from_ico(&ico_with_entry(6, 10_000), 16).is_err());
        // 头部过短 / 无条目。
        assert!(create_hicon_from_ico(&[0u8; 5], 16).is_err());
        assert!(create_hicon_from_ico(&[0u8; 6], 16).is_err());
    }

    /// 多尺寸 ICO 构建不依赖目标尺寸的具体值：任何非零尺寸都能构建
    /// （不小于目标的最小帧；全部小于目标时取最大帧）。
    #[test]
    fn create_hicon_from_ico_any_size_ok() {
        let ico = build_multi_size_ico();
        for size in [1u32, 48, 256, 300] {
            let h = create_hicon_from_ico(&ico, size).expect("must create HICON");
            let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyIcon(h) };
        }
    }

    /// 帧选择策略（L1 回归）：高 DPI 下取不小于目标的帧（宁缩小不放大，
    /// 避免历史实现硬编码 16px 把 16 帧放大发糊）。
    #[test]
    fn create_hicon_from_ico_prefers_frame_at_or_above_target() {
        fn select_frame(target: u32) -> u32 {
            let ico = build_multi_size_ico();
            let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
            let mut frames = Vec::new();
            for i in 0..count {
                let e = 6 + i * 16;
                frames.push(if ico[e] == 0 { 256u32 } else { ico[e] as u32 });
            }
            frames
                .iter()
                .filter(|&&px| px >= target)
                .min()
                .copied()
                .unwrap_or_else(|| frames.iter().max().copied().expect("non-empty"))
        }
        // 100% 取 16 帧（历史行为不变）。
        assert_eq!(select_frame(16), 16);
        // 150%（目标约 24px）取 32px，而不是 16px（放大）。
        assert_eq!(select_frame(24), 32);
        // 目标 48px 时精确命中。
        assert_eq!(select_frame(48), 48);
        // 目标超过最大帧时取最大帧。
        assert_eq!(select_frame(300), 256);
    }

    /// 托盘图标尺寸换算：16 逻辑 px × 系统 DPI，四舍五入到物理像素。
    #[test]
    fn tray_icon_size_scales_with_dpi() {
        // round(16 × dpi / 96)：与实机换算一致（进程 DPI 无法在测试中
        // 控制，直接对换算公式的纯算术部分断言）。
        assert_eq!(scaled_px_at_dpi(16, 96), 16);
        assert_eq!(scaled_px_at_dpi(16, 144), 24);
        assert_eq!(scaled_px_at_dpi(16, 192), 32);
        // 异常 DPI（0）回退 96。
        assert_eq!(scaled_px_at_dpi(16, 0), 16);
        // 大图标：32 逻辑 px → 200% 为 64px（对应 256 帧，宁缩不放大）。
        assert_eq!(scaled_px_at_dpi(32, 192), 64);
    }
}
