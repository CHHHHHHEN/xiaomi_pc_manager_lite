//! 窗口/托盘/标题栏图标：多尺寸 ICO 构建、单帧 HICON 创建与窗口图标设置。
//!
//! 从 `platform/window.rs`（窗口显示控制）中按职责切分（修订 1.48 整理）：
//! 窗口生命周期管理（find/hide/show/wake）与图标资源处理（DPI 换算、ICO
//! 解析、HICON 创建）是两件事，合并在一处既拉长文件、又让"图标像素换算"
//! 的纯逻辑与"窗口句柄操作"混杂。本模块自包含全部 image/DPI/HICON 代码，
//! 供 `platform/window.rs` 之外的 `gui`（主窗口图标）与 `tray`（托盘图标）
//! 直接引用。

use std::sync::Mutex;

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
#[cfg(test)]
use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, SendMessageW, HICON, ICON_BIG, ICON_SMALL, LR_DEFAULTCOLOR,
    WM_SETICON,
};

/// 由 `icons/icon.png` 构建多尺寸 ICO 数据（16/32/48/256，PNG 压缩块）。
/// 现代 Windows（Vista+）支持含 PNG 块的 ICO。
fn build_multi_size_ico() -> Vec<u8> {
    let Some(img) = app_icon_rgba() else {
        return Vec::new();
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

/// 应用图标 `icons/icon.png` 解码为 RGBA 位图（进程内恒定，缓存一次）。
///
/// GUI（`egui::IconData`，gui/view.rs）与平台（多尺寸 ICO 构建，本模块）
/// 各自 `include_bytes!` 并解码同一张嵌入 PNG——收敛到此处共享解码与缓存
/// （修订 1.49 整理）。解码失败（资源损坏）返回 None，由调用方决定降级。
pub(crate) fn app_icon_rgba() -> Option<image::RgbaImage> {
    static CACHE: std::sync::OnceLock<Option<image::RgbaImage>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let png = include_bytes!("../../icons/icon.png");
            image::load_from_memory(png).ok().map(|img| img.to_rgba8())
        })
        .clone()
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
    // `.max(96)` 已覆盖异常返回值（0）的下限，无需再 `.max(1)`（修订 1.47
    // 清理：两个 max 叠写无意义）。
    unsafe { GetDpiForSystem() }.max(96)
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
        // windows-rs 的绑定签名是 `(&[u8], bool, u32, i32, i32, IMAGE_FLAGS)`：
        // 资源字节由切片自带长度，**没有** dwResSize 参数——`0x0003_0000` 是
        // dwVersion（Vista+ PNG 压缩帧版本号），不是大小，传原样即正确
        // （修订 1.47 误报澄清，勿按 C 语言签名臆改）。
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
    let Some(hwnd) = crate::platform::window::find_main_window() else {
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
            HICON(std::ptr::null_mut())
        });
        // **仅缓存成功句柄**（修订 1.46）：把构建失败（null）也缓存会把瞬态
        // 失败永久化——本次 ico 字节/资源异常下次启动才能恢复。失败时返回
        // null、不写入缓存，调用方随后 WM_SETICON 时按 null 跳过；下一轮
        // set_main_window_icon 会重新尝试构建。
        if !h.0.is_null() {
            *guard = Some(IconHandle(h));
        }
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
            let _ = unsafe { DestroyIcon(h) };
        }
    }

    /// 越界/溢出的损坏 ICO 必须被拒绝（checked_add 防回绕），不能构造越界
    /// 切片 panic（与托盘图标的 `load_icon` 同语义，经共享 helper 收敛）。
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
            let _ = unsafe { DestroyIcon(h) };
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
