//! 托盘气泡通知的**展示**（NIF_INFO）。
//!
//! 从 `tray/worker.rs` 按职责切分：通知的触发判定（纯决策）与文案已收敛在
//! `app::notify`，本模块只做"把通知弹到系统托盘"的展示（调用
//! `Shell_NotifyIconW`），只依赖 `NOTIFYICONDATAW` 快照，不需要消息窗口/
//! 托盘状态。

use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_INFO, NIIF_INFO, NIM_MODIFY, NOTIFYICONDATAW,
};

/// 气泡标题与正文的容量上限（NIF_INFO 的 szInfoTitle/szInfo 均为
/// 64 UTF-16 单元；标题可全用 64，正文尾部须保留 NUL 故取 63）。
const SZ_INFO_TITLE_CAP: usize = 64;
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
    // 先整体清零再拷贝正文：NUL 结尾是 Shell_NotifyIconW 读取字符串的
    // 唯一终止信号（其余单元须为 0）。
    let title = crate::util::WideString::new(crate::util::APP_NAME);
    let title_len = title.units().len().min(SZ_INFO_TITLE_CAP - 1);
    nid.szInfoTitle.fill(0);
    nid.szInfoTitle[..title_len].copy_from_slice(&title.units()[..title_len]);
    let info_wide: Vec<u16> = body.encode_utf16().take(SZ_INFO_CAP).collect();
    nid.szInfo.fill(0);
    nid.szInfo[..info_wide.len()].copy_from_slice(&info_wide);
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
