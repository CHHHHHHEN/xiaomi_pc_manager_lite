use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW,
    GetMessageW, SetWindowLongPtrW, TranslateMessage,
    WINDOW_EX_STYLE, WINDOW_STYLE, MSG, GWLP_WNDPROC,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::Foundation::{HWND, HINSTANCE, WPARAM, LPARAM, LRESULT};
use windows::core::PCWSTR;

const HWND_MESSAGE: HWND = HWND(std::ptr::with_exposed_provenance_mut(-3isize as usize));

pub unsafe fn create_message_window() -> Result<HWND, String> {
    let hinstance = HINSTANCE(
        GetModuleHandleW(None)
            .map_err(|e| format!("GetModuleHandleW: {}", e))?
            .0,
    );

    let class_name: Vec<u16> = "STATIC\0".encode_utf16().collect();

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR::from_raw(class_name.as_ptr()),
        PCWSTR::null(),
        WINDOW_STYLE::default(),
        0, 0, 0, 0,
        Some(HWND_MESSAGE),
        None,
        Some(hinstance),
        None,
    )
    .map_err(|e| format!("CreateWindowExW: {}", e))?;

    Ok(hwnd)
}

pub unsafe fn set_wndproc(hwnd: HWND, wndproc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT) {
    SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wndproc as *const () as isize);
}

pub unsafe fn message_loop(hwnd: HWND) {
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).into() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    let _ = DestroyWindow(hwnd);
}
