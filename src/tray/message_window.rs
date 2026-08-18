use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, SetWindowLongPtrW,
    TranslateMessage, GWLP_WNDPROC, MSG, WINDOW_EX_STYLE, WINDOW_STYLE,
};

/// 创建用于接收 Windows 消息的隐藏窗口。
///
/// 注意：这里**不能**使用消息专用窗口（`HWND_MESSAGE` 作父窗口）。消息专用
/// 窗口不参与桌面窗口层级、不会被广播枚举（MSDN: "does not receive broadcast
/// messages"），因此永远收不到 `WM_POWERBROADCAST`，导致“电源切换时自动重设”
/// 功能失效。改为创建隐藏的顶层窗口（父窗口为空、从不显示）：既能接收直接
/// 消息（托盘通知、`WM_HOTKEY`、菜单命令），也能接收系统广播（F-PWR-01）。
pub fn create_message_window() -> Result<HWND, String> {
    let hinstance = HINSTANCE(
        unsafe { GetModuleHandleW(None) }
            .map_err(|e| format!("GetModuleHandleW: {}", e))?
            .0,
    );

    // STATIC 类的窗口过程会被替换为自定义 wndproc（见 set_wndproc），
    // 窗口本身仅作为消息接收者，不渲染任何内容。
    let class_name = crate::util::WideString::new("STATIC");

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name.as_pcwstr(),
            PCWSTR::null(),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None, // 顶层窗口（桌面为父窗口）；从不 ShowWindow，保持隐藏
            None,
            Some(hinstance),
            None,
        )
    }
    .map_err(|e| format!("CreateWindowExW: {}", e))?;

    Ok(hwnd)
}

/// 替换消息窗口的窗口过程。失败（返回 0 且带错误码）时必须上报：否则窗口
/// 仍使用 STATIC 类默认过程，托盘点击 / 热键 / 电源广播全部静默失效。
pub fn set_wndproc(
    hwnd: HWND,
    wndproc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
) -> Result<(), String> {
    let ret = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wndproc as *const () as isize) };
    if ret == 0 {
        // STATIC 类的原窗口过程永不为空，ret == 0 即失败。
        let err = unsafe { GetLastError() };
        return Err(format!("SetWindowLongPtrW: {:#x}", err.0));
    }
    Ok(())
}

pub fn message_loop(hwnd: HWND) {
    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 == 0 {
            break; // WM_QUIT received
        }
        if ret.0 == -1 {
            log::error!("GetMessageW failed (last error: {:#x})", unsafe {
                GetLastError().0
            });
            // GetMessageW 错误路径此前直接 break 泄漏 HWND（只有 WM_QUIT 路径
            // 执行到函数末尾的 DestroyWindow）——两条退出路径都必须显式销毁，
            // 否则泄漏窗口/用户对象（修订 1.47 审计）。
            let _ = unsafe { DestroyWindow(hwnd) };
            return;
        }
        let _ = unsafe { TranslateMessage(&msg) };
        unsafe {
            DispatchMessageW(&msg);
        }
    }
    let _ = unsafe { DestroyWindow(hwnd) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, IsWindowVisible, GWLP_HWNDPARENT,
    };

    /// 回归测试：消息窗口必须是隐藏的**顶层**窗口，而不是消息专用窗口
    /// （父窗口 = HWND_MESSAGE）。消息专用窗口不接收系统广播（如
    /// WM_POWERBROADCAST，MSDN 明确说明 "does not receive broadcast messages"），
    /// 会导致“电源切换时自动重设”功能失效。
    #[test]
    fn test_message_window_is_hidden_top_level() {
        let hwnd = create_message_window().expect("create window");
        unsafe {
            // 顶层窗口的父窗口句柄为 0；消息专用窗口的父窗口为 HWND_MESSAGE (-3)。
            let parent = GetWindowLongPtrW(hwnd, GWLP_HWNDPARENT);
            assert_eq!(parent, 0, "window must be top-level, not message-only");
            // 窗口必须保持隐藏，不干扰用户界面。
            assert!(!IsWindowVisible(hwnd).as_bool(), "window must stay hidden");
            let _ = DestroyWindow(hwnd);
        }
    }
}
