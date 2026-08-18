//! 进程权限查询与自我提权（WinRing0 后端需要管理员权限）。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWDEFAULT;

use crate::util::WideString;

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
    let verb = WideString::new("runas");
    // 路径直接用 OsStr 构造 UTF-16（不经 to_string_lossy）：
    // Windows 路径可能是非 UTF-8 的 UTF-16 序列，lossy 会替换成 U+FFFD，
    // ShellExecuteW 拿到错误路径 → 提权静默失败、本进程继续非管理员运行
    // （修订 1.46 安全加固，见 util::WideString::from_os_str）。
    let path = WideString::from_os_str(exe.as_os_str());
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    // 命令行**在整个 UTF-16 域内构建**（不经 String/OsStr lossy 往返）：
    // 参数可能含未配对代理项（非合法 UTF-8），to_string_lossy 会替换成
    // U+FFFD——拼进 lpParameters 后提权进程收到被破坏的参数（修订 1.46
    // 审计，与路径侧同源问题，见 build_command_line）。
    let args_buf = WideString::from_units(build_command_line(&args));
    let ret = unsafe {
        ShellExecuteW(
            None,
            verb.as_pcwstr(),
            path.as_pcwstr(),
            args_buf.as_pcwstr(),
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
        log::warn!(
            "Elevation failed (ret={}); continuing without admin. WinRing0 will be unavailable.",
            ret_val
        );
        false
    }
}

/// 将进程参数重建为 Windows 命令行 UTF-16 序列（供 ShellExecuteW 的
/// lpParameters）。**整个流程保持 UTF-16 域**（修订 1.46 审计）：每个参数经
/// `encode_wide` 直取原始 UTF-16 单元（含未配对代理项），不再经
/// `to_string_lossy` 往返——lossy 会把非 UTF-8 的代理项替换成 U+FFFD，
/// 提权进程收到被破坏的参数。
///
/// 不含空格/制表符/引号的参数原样拼接；含这些字符的参数按 MSDN 命令行
/// 解析规则（CommandLineToArgvW）转义后整体用双引号包裹：
/// - 引号前的连续反斜杠必须加倍再加一（`\"` 使引号成为字面字符而非闭合符）；
/// - 参数末尾的连续反斜杠数量必须加倍（否则会转义包裹参数的闭合引号，
///   导致 `C:\Program Files\` 这类路径解析错误）。
///
/// 返回不含结尾 NUL 的 UTF-16 单元（调用方经 `WideString::from_units` 持有）。
fn build_command_line(args: &[std::ffi::OsString]) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    /// 单个参数按 CommandLineToArgvW 规则转义为带双引号包裹的 UTF-16。
    fn quote(arg: &[u16]) -> Vec<u16> {
        const BACKSLASH: u16 = b'\\' as u16;
        const QUOTE: u16 = b'"' as u16;
        let mut out = Vec::with_capacity(arg.len() + 2);
        out.push(QUOTE);
        let mut backslashes = 0usize;
        for &c in arg {
            match c {
                BACKSLASH => backslashes += 1,
                QUOTE => {
                    out.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2 + 1));
                    out.push(c);
                    backslashes = 0;
                }
                _ => {
                    out.extend(std::iter::repeat_n(BACKSLASH, backslashes));
                    out.push(c);
                    backslashes = 0;
                }
            }
        }
        // 参数以反斜杠结尾：加倍转义，避免闭合引号。
        out.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2));
        out.push(QUOTE);
        out
    }

    let mut cmdline = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            cmdline.push(b' ' as u16);
        }
        let wide: Vec<u16> = arg.encode_wide().collect();
        // 空参数也必须保留为 `""`（CommandLineToArgvW 语义）：直接 extend
        // 空片会把它从命令行里静默丢掉，参数个数错位（修订 1.46 审计）。
        if wide.is_empty()
            || wide
                .iter()
                .any(|&c| c == b' ' as u16 || c == b'\t' as u16 || c == b'"' as u16)
        {
            cmdline.extend(quote(&wide));
        } else {
            cmdline.extend(wide);
        }
    }
    cmdline
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把 UTF-16 单元还原为 String（测试辅助：期望值都是合法 ASCII/UTF-16）。
    fn wide_to_string(units: &[u16]) -> String {
        String::from_utf16_lossy(units)
    }

    #[test]
    fn test_build_command_line_plain_args() {
        let args: Vec<std::ffi::OsString> = vec!["--autostart".into()];
        assert_eq!(wide_to_string(&build_command_line(&args)), "--autostart");
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
            wide_to_string(&build_command_line(&args)),
            "--param \"has space\" \"tab\there\" \"say \\\"hi\\\"\""
        );
    }

    #[test]
    fn test_build_command_line_trailing_backslash() {
        // 参数含空格且以反斜杠结尾：末尾反斜杠必须加倍，否则会转义包裹
        // 参数的闭合引号，`CommandLineToArgvW` 解析后参数不完整。
        let args: Vec<std::ffi::OsString> = vec!["C:\\Program Files\\".into()];
        assert_eq!(
            wide_to_string(&build_command_line(&args)),
            "\"C:\\Program Files\\\\\""
        );
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
            assert_eq!(
                wide_to_string(&build_command_line(&args)),
                expected_cmdline,
                "arg: {arg}"
            );
        }
    }

    /// 非 UTF-8 参数（含未配对代理项，Windows 上 `args_os` 可能产生）必须
    /// 在 UTF-16 域内原样保留——`to_string_lossy` 会把代理项替换成 U+FFFD，
    /// 提权进程收到被破坏的参数（修订 1.46 审计）。
    #[test]
    fn test_build_command_line_preserves_unpaired_surrogates() {
        use std::os::windows::ffi::OsStringExt;
        // 含代理项且**无**空格/引号：不包裹引号，原样输出全部单元
        // （含未配对代理项 0xD800，不做 U+FFFD 替换）。
        let args: Vec<std::ffi::OsString> =
            vec![std::ffi::OsString::from_wide(&[0x44, 0xD800, 0x21])];
        let wide = build_command_line(&args);
        assert_eq!(&wide[..], &[0x44, 0xD800, 0x21]);
        // 含代理项 + 空格：按规则加引号包裹，代理项仍原样保留。
        let args2: Vec<std::ffi::OsString> =
            vec![std::ffi::OsString::from_wide(&[0x44, 0xD800, 0x20, 0x21])];
        let wide2 = build_command_line(&args2);
        assert_eq!(
            &wide2[..],
            &[b'"' as u16, 0x44, 0xD800, 0x20, 0x21, b'"' as u16],
            "arg with space + surrogate must be quoted without corruption"
        );
    }

    /// 空参数必须保留为 `""`（修订 1.46 审计）：`CommandLineToArgvW` 对
    /// `""` 还原出一个空字符串，对完全缺失的参数则少一个参数——丢弃空参
    /// 会让参数个数错位。
    #[test]
    fn test_build_command_line_preserves_empty_arg() {
        let args: Vec<std::ffi::OsString> = vec!["--flag".into(), "".into(), "tail".into()];
        let wide = build_command_line(&args);
        assert_eq!(
            wide_to_string(&wide),
            "--flag \"\" tail",
            "empty arg must be preserved as \"\""
        );
    }

    #[test]
    fn test_build_command_line_empty() {
        assert!(build_command_line(&[]).is_empty());
    }

    #[test]
    fn test_is_admin_does_not_panic() {
        // 只验证可调用、不崩溃；结果取决于运行环境。
        let _ = is_admin();
    }
}
