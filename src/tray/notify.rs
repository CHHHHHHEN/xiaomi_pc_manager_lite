//! 托盘气泡通知的**展示**（NIF_INFO）。
//!
//! 从 `tray/worker.rs` 按职责切分：通知的触发判定（纯决策）与文案已收敛在
//! `app::notify`，本模块只做"把通知弹到系统托盘"的展示（调用
//! `Shell_NotifyIconW`），只依赖 `NOTIFYICONDATAW` 快照，不需要消息窗口/
//! 托盘状态。

use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_INFO, NIIF_INFO, NIM_MODIFY, NOTIFYICONDATAW,
};

/// 气泡标题与正文的**内容**单元上限（`NIF_INFO` 的 szInfoTitle 容量 64、
/// szInfo 容量 256；NUL 结尾占用 1 单元，故内容上限比容量小 1）。调用
/// `util::write_utf16_capped` 时物理容量仍由数组自身兜底。
const SZ_INFO_TITLE_CAP: usize = 63;
const SZ_INFO_CAP: usize = 63;

/// 弹托盘气泡通知（NIF_INFO）：通用通知（性能模式/电池养护共用）。
///
/// 只在"窗口隐藏 + 状态变化"时调用。气泡是系统托盘通知，无需额外窗口即可
/// 展示，用户在当前窗口继续工作的同时获得状态切换反馈。
pub fn show_tray_notification(nid: NOTIFYICONDATAW, body: &str) {
    log::info!("Tray notification: {}", body);
    let mut nid = nid;
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    // 标题/正文统一经 util::write_utf16_capped 保证 NUL 结尾（NUL 是
    // Shell_NotifyIconW 读取字符串的唯一终止信号；NUL 之后保持调用方原值，
    // 读取在 NUL 处停止）。历史实现各自手写 encode_utf16.take + 整体清零
    // 的样板（修订 1.50 收敛）。
    crate::util::write_utf16_capped(
        &mut nid.szInfoTitle,
        SZ_INFO_TITLE_CAP,
        crate::util::APP_NAME,
    );
    crate::util::write_utf16_capped(&mut nid.szInfo, SZ_INFO_CAP, body);
    // NIM_MODIFY 携带 NIF_INFO 触发气泡。
    if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() } {
        log::debug!("Tray: NIM_MODIFY notification failed");
    }
}

/// 弹托盘气泡通知：性能模式已切换。
pub fn show_perf_notification(nid: NOTIFYICONDATAW, perf_name: &str) {
    show_tray_notification(nid, &format!("性能模式: {}", perf_name));
}

/// 弹托盘气泡通知：电池养护已启用/停用。
pub fn show_battery_care_notification(nid: NOTIFYICONDATAW, enabled: bool) {
    show_tray_notification(
        nid,
        &format!("电池养护: {}", crate::app::battery::care_label(enabled)),
    );
}
