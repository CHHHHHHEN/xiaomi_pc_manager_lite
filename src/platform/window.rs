//! 主窗口（eframe 窗口）的显示控制。
//!
//! 托盘驻留使用 `ShowWindow(SW_HIDE)` **隐藏**而非最小化：winit 不知道
//! 窗口被隐藏，仍正常投递重绘事件，`update()` 与托盘命令处理保持运行；
//! 而 `ViewportCommand::Minimized` 会在任务栏保留图标，不符合
//! "最小化到托盘后任务栏不再显示程序图标"的需求。
//!
//! 注意：不能用 `ViewportCommand::Visible(false)`——eframe/winit 在窗口
//! 隐藏后不再投递 RedrawRequested（已实测），`update()` 永久停止，
//! 托盘命令无法处理，应用变僵尸进程。

use std::sync::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, FindWindowW, IsWindowVisible, SendMessageW, SetForegroundWindow,
    ShowWindow, SW_HIDE, SW_RESTORE, SW_SHOW, WM_SETICON, ICON_BIG, ICON_SMALL, LR_DEFAULTCOLOR,
};

use crate::util::to_pcwstr;

/// eframe 窗口标题（`eframe::run_native` 的第一个参数）。
pub const MAIN_WINDOW_TITLE: &str = "Xiaomi PC Manager Lite";

fn find_main_window() -> Option<HWND> {
    let (_buf, title) = to_pcwstr(MAIN_WINDOW_TITLE);
    let hwnd = unsafe { FindWindowW(None, title) }.ok()?;
    if hwnd.0.is_null() {
        None
    } else {
        Some(hwnd)
    }
}

/// 供托盘层直接操作主窗口（窗口隐藏后 GUI update 循环停止，
/// 托盘必须自给自足地隐藏/显示/退出窗口）。
pub(crate) fn find_main_window_handle() -> Option<HWND> {
    find_main_window()
}

/// 主窗口当前是否可见（任务栏图标随可见性出现/消失）。
pub fn main_window_visible() -> bool {
    find_main_window()
        .map(|hwnd| unsafe { IsWindowVisible(hwnd).as_bool() })
        .unwrap_or(false)
}

/// 隐藏主窗口（任务栏图标随之消失，仅驻留托盘）。
pub fn hide_main_window() {
    if let Some(hwnd) = find_main_window() {
        log::info!("Hide main window 0x{:X}", hwnd.0 as usize);
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    } else {
        log::warn!("Hide main window: not found");
    }
}

/// 显示并激活主窗口。
pub fn show_main_window() {
    if let Some(hwnd) = find_main_window() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = ShowWindow(hwnd, SW_SHOW);
            // 托盘点击触发，属于用户交互上下文，一般可成功置前。
            let _ = SetForegroundWindow(hwnd);
        }
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
        let scaled =
            image::imageops::resize(&img, s, s, image::imageops::FilterType::Lanczos3);
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
        ico.push(0);                                   // color count
        ico.push(0);                                   // reserved
        ico.extend_from_slice(&1u16.to_le_bytes());    // planes
        ico.extend_from_slice(&32u16.to_le_bytes());   // bitcount
        ico.extend_from_slice(&(block.len() as u32).to_le_bytes()); // dwBytesInRes
        ico.extend_from_slice(&(offset as u32).to_le_bytes());      // dwImageOffset
        offset += block.len();
    }
    for block in &blocks {
        ico.extend_from_slice(block);
    }
    ico
}

/// HICON 句柄包装（裸指针非 Send/Sync）：图标由系统持有、进程退出时
/// 释放；跨线程仅传递句柄值，不涉及所有权转移。
struct IconHandle(windows::Win32::UI::WindowsAndMessaging::HICON);
unsafe impl Send for IconHandle {}
unsafe impl Sync for IconHandle {}

/// 设置主窗口图标（任务栏 / 标题栏 / Alt-Tab）。
///
/// eframe 的 `with_icon` 对 512×512 PNG 的缩小渲染到任务栏效果差
/// （糊成纯色块）。这里用**多尺寸 ICO**（16/32/48/256）创建 HICON 并
/// 通过 `WM_SETICON` 设置，Windows 按目标尺寸原生选用最清晰的帧。
/// HICON 进程生命周期内缓存（Mutex 包装以满足 Sync），由系统使用、
/// 进程退出时释放。
pub fn set_main_window_icon() {
    static ICON: Mutex<Option<IconHandle>> = Mutex::new(None);
    let Some(hwnd) = find_main_window() else {
        return;
    };
    let hicon = {
        let mut guard = ICON.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(icon) = guard.as_ref() {
            icon.0
        } else {
            let ico_bytes = build_multi_size_ico();
            let icon = if ico_bytes.is_empty() {
                windows::Win32::UI::WindowsAndMessaging::HICON(std::ptr::null_mut())
            } else {
                unsafe {
                    CreateIconFromResourceEx(
                        &ico_bytes,
                        true,
                        0x0003_0000, // Windows 3.0+ format
                        0,
                        0,
                        LR_DEFAULTCOLOR,
                    )
                    .unwrap_or_default()
                }
            };
            *guard = Some(IconHandle(icon));
            icon
        }
    };
    if hicon.0.is_null() {
        return;
    }
    unsafe {
        let _ = SendMessageW(
            hwnd,
            WM_SETICON,
            Some(WPARAM(ICON_BIG as usize)),
            Some(LPARAM(hicon.0 as isize)),
        );
        let _ = SendMessageW(
            hwnd,
            WM_SETICON,
            Some(WPARAM(ICON_SMALL as usize)),
            Some(LPARAM(hicon.0 as isize)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_main_window_title_is_stable() {
        assert_eq!(MAIN_WINDOW_TITLE, "Xiaomi PC Manager Lite");
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
        assert_eq!(ico.len(), header_len + blocks_total_len(&ico, count),
            "file length must match header-declared image data");

        let mut expected_offset = header_len;
        for i in 0..count {
            let e = 6 + i * 16;
            let width = ico[e];
            assert_eq!(width, ico[e + 1], "width==height per entry");
            assert_eq!(ico[e + 3], 0, "reserved must be 0");
            assert_eq!(u16::from_le_bytes([ico[e + 4], ico[e + 5]]), 1, "planes must be 1");
            assert_eq!(u16::from_le_bytes([ico[e + 6], ico[e + 7]]), 32, "bitcount must be 32");
            let size = u32::from_le_bytes([ico[e + 8], ico[e + 9], ico[e + 10], ico[e + 11]]);
            let off = u32::from_le_bytes([ico[e + 12], ico[e + 13], ico[e + 14], ico[e + 15]]);
            assert_eq!(off as usize, expected_offset,
                "entry {}: image offset must match header layout", i);
            assert!(size > 0 && (off as usize + size as usize) <= ico.len());
            // PNG magic of the block at the declared offset.
            assert_eq!(&ico[off as usize..off as usize + 8],
                &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
                "entry {}: data at declared offset must be a PNG", i);
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
}
