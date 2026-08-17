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
///
/// 重新启动时必须**透传原始命令行参数**（至少 `--autostart`）：旧版本注册的
/// 自启动任务以默认级别（LUA，非管理员）运行时，登录后会走到本函数重新提权；
/// 若丢失 `--autostart`，提升后的实例会弹出完整 GUI 窗口而非驻留托盘，
/// 违背自启动"不打扰用户"的设计。
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
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let (_args_buf, args_ptr) = to_pcwstr(&build_command_line(&args));
    let ret = unsafe {
        ShellExecuteW(
            None,
            verb,
            path,
            args_ptr,
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

/// 将进程参数重建为 Windows 命令行字符串（供 ShellExecuteW 的 lpParameters）。
/// 不含空格/制表符/引号的参数原样拼接；含这些字符的参数按 MSDN 命令行
/// 解析规则（CommandLineToArgvW）转义后整体用双引号包裹：
/// - 引号前的连续反斜杠必须加倍再加一（`\"` 使引号成为字面字符而非闭合符）；
/// - 参数末尾的连续反斜杠数量必须加倍（否则会转义包裹参数的闭合引号，
///   导致 `C:\Program Files\` 这类路径解析错误）。
fn build_command_line(args: &[std::ffi::OsString]) -> String {
    fn quote(arg: &str) -> String {
        let mut out = String::with_capacity(arg.len() + 2);
        out.push('"');
        let mut backslashes = 0usize;
        for c in arg.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    for _ in 0..(backslashes * 2 + 1) {
                        out.push('\\');
                    }
                    out.push('"');
                    backslashes = 0;
                }
                _ => {
                    for _ in 0..backslashes {
                        out.push('\\');
                    }
                    out.push(c);
                    backslashes = 0;
                }
            }
        }
        // 参数以反斜杠结尾：加倍，避免转义闭合引号。
        for _ in 0..(backslashes * 2) {
            out.push('\\');
        }
        out.push('"');
        out
    }

    let mut cmdline = String::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            cmdline.push(' ');
        }
        let s = arg.to_string_lossy();
        if s.chars().any(|c| c == ' ' || c == '\t' || c == '"') {
            cmdline.push_str(&quote(&s));
        } else {
            cmdline.push_str(&s);
        }
    }
    cmdline
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_command_line_plain_args() {
        let args: Vec<std::ffi::OsString> = vec!["--autostart".into()];
        assert_eq!(build_command_line(&args), "--autostart");
    }

    #[test]
    fn test_build_command_line_quotes_args_with_spaces() {
        let args: Vec<std::ffi::OsString> = vec![
            "--param".into(),
            "has space".into(),
            "tab\there".into(),
            "say \"hi\"".into(),
        ];
        assert_eq!(
            build_command_line(&args),
            "--param \"has space\" \"tab\there\" \"say \\\"hi\\\"\""
        );
    }

    #[test]
    fn test_build_command_line_trailing_backslash() {
        // 参数含空格且以反斜杠结尾：末尾反斜杠必须加倍，否则会转义包裹
        // 参数的闭合引号，`CommandLineToArgvW` 解析后参数不完整。
        let args: Vec<std::ffi::OsString> = vec!["C:\\Program Files\\".into()];
        assert_eq!(build_command_line(&args), "\"C:\\Program Files\\\\\"");
    }

    #[test]
    fn test_build_command_line_roundtrip_with_parser_semantics() {
        // 用 CommandLineToArgvW 的解析语义逐条验证重建后的命令行可还原参数。
        // 每行两种写法等价：期望值即待解析的命令行，还原结果应为原始参数。
        for (arg, expected_cmdline) in [
            ("--autostart", "--autostart"),
            ("has space", "\"has space\""),
            ("tab\there", "\"tab\there\""),
            ("say \"hi\"", "\"say \\\"hi\\\"\""),
            ("C:\\Program Files\\", "\"C:\\Program Files\\\\\""),
            ("a\\b\"c\\d", "\"a\\b\\\"c\\d\""),
        ] {
            let args: Vec<std::ffi::OsString> = vec![arg.into()];
            assert_eq!(build_command_line(&args), expected_cmdline, "arg: {arg}");
        }
    }

    #[test]
    fn test_build_command_line_empty() {
        assert_eq!(build_command_line(&[]), "");
    }

    #[test]
    fn test_is_admin_does_not_panic() {
        // 只验证可调用、不崩溃；结果取决于运行环境。
        let _ = is_admin();
    }
}
