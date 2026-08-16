//! 进程权限查询与自我提权（WinRing0 后端需要管理员权限）。

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWDEFAULT;
use windows::core::PCWSTR;

use crate::util::to_pcwstr;

/// 当前进程是否以管理员（提升）权限运行。
pub fn is_admin() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            windows::Win32::Security::TokenElevation,
            Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// 以管理员权限重新启动本进程（ShellExecuteW "runas"）。
///
/// 返回 `true` 表示已成功拉起新的提升实例，调用方应立即退出本进程；
/// 返回 `false` 表示未启动（用户拒绝 UAC / 启动失败），调用方继续
/// 以当前权限运行。
///
/// 注意：ShellExecuteW 成功时返回 > 32 的值；用户拒绝 UAC 时返回
/// ERROR_CANCELLED (1223)——该值也大于 32，不能把"拒绝"误判为"已启动"，
/// 否则调用方退出后没有任何实例在运行，应用会无声消失。
pub fn elevate_self() -> bool {
    if is_admin() {
        return false;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            log::warn!("current_exe: {}", e);
            return false;
        }
    };
    let (_verb_buf, verb) = to_pcwstr("runas");
    let (_path_buf, path) = to_pcwstr(&exe.to_string_lossy());
    let ret = unsafe {
        ShellExecuteW(
            None,
            verb,
            path,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWDEFAULT,
        )
    };
    const ERROR_CANCELLED: isize = 1223;
    let ret_val = ret.0 as isize;
    if ret_val > 32 && ret_val != ERROR_CANCELLED {
        log::info!("Elevated instance launched; exiting non-admin process.");
        true
    } else if ret_val == ERROR_CANCELLED {
        log::warn!("Elevation declined by the user; continuing without admin. WinRing0 will be unavailable.");
        false
    } else {
        log::warn!("Elevation failed (ret={}); continuing without admin. WinRing0 will be unavailable.", ret_val);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_admin_does_not_panic() {
        // 只验证可调用、不崩溃；结果取决于运行环境。
        let _ = is_admin();
    }
}
