use rust_embed::RustEmbed;
use std::path::PathBuf;

#[derive(RustEmbed)]
#[folder = "bin"]
struct WinRing0Binaries;

pub fn extract_winring0() -> Result<PathBuf, String> {
    // 文件名对（DLL, SYS）统一由 winring0.rs 提供，避免两处硬编码漂移。
    let (dll_name, sys_name) = crate::ec::winring0::arch_file_names();

    // 文件名对由 arch_file_names() 保证与当前架构一致（32 位：WinRing0.dll/.sys，
    // 64 位：WinRing0x64.dll/.sys）。**不得**回退到其它架构的文件名：64 位进程
    // 加载 32 位 DLL 必然失败（ERROR_BAD_EXE_FORMAT），且该无条件回退会掩盖
    // "嵌入资源缺失"这一打包问题——错误从清晰的 "xxx not found in embedded
    // binaries" 变成难以排查的 LoadLibrary 失败。精确取用当前架构文件名，缺失即报错。
    let embedded_dll = WinRing0Binaries::get(dll_name)
        .ok_or_else(|| format!("{} not found in embedded binaries", dll_name))?;

    let embedded_sys = WinRing0Binaries::get(sys_name)
        .ok_or_else(|| format!("{} not found in embedded binaries", sys_name))?;

    let target_dir = std::env::current_exe()
        .map_err(|e| format!("current_exe: {}", e))?
        .parent()
        .ok_or("no parent directory")?
        .to_path_buf();

    // Clean up old extraction locations from previous versions.
    // 注意：**不能**整体删除 `%TEMP%\XiaomiPcManagerLite`——该目录正是
    // `main::init_logging` 的日志目录（app.log 由本进程持有打开句柄，删除
    // 必然失败且永远静默失败），而且会误删同目录下其它应用/实例的文件。
    // 只删除按文件名精确匹配的遗留副本，绝不整目录删除。
    let old_sys_dir =
        std::path::PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into()))
            .join("Temp");
    let _ = std::fs::remove_file(old_sys_dir.join(dll_name));
    let _ = std::fs::remove_file(old_sys_dir.join(sys_name));

    // Remove stale files at the target location, retry once if handles linger
    for retry in 0..2 {
        let _ = std::fs::remove_file(target_dir.join(dll_name));
        let _ = std::fs::remove_file(target_dir.join(sys_name));
        if retry == 0 {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

    let dll_path = target_dir.join(dll_name);
    std::fs::write(&dll_path, &embedded_dll.data)
        .map_err(|e| format!("write {}: {}", dll_name, e))?;

    let sys_path = target_dir.join(sys_name);
    std::fs::write(&sys_path, &embedded_sys.data)
        .map_err(|e| format!("write {}: {}", sys_name, e))?;

    log::info!("Extracted {} + {} to {:?}", dll_name, sys_name, target_dir);
    Ok(dll_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归测试（无条件兜底清理）：提取必须精确命中当前架构的文件名对，
    /// **不得**回退到其它架构的文件名（历史实现 `.or_else(|| get("WinRing0.dll"))`
    /// 在 64 位进程下会退到 32 位 DLL，加载必然失败 ERROR_BAD_EXE_FORMAT，
    /// 且把"嵌入资源缺失"的打包错误掩盖成难以排查的 LoadLibrary 失败）。
    #[test]
    fn test_arch_file_names_match_pointer_width() {
        let (dll, sys) = crate::ec::winring0::arch_file_names();
        if cfg!(target_pointer_width = "64") {
            assert_eq!(dll, "WinRing0x64.dll");
            assert_eq!(sys, "WinRing0x64.sys");
        } else {
            assert_eq!(dll, "WinRing0.dll");
            assert_eq!(sys, "WinRing0.sys");
        }
    }

    /// 当前架构的嵌入资源必须存在——缺失时 extract_winring0 应如实报错，
    /// 而不是靠跨架构回退继续运行。
    #[test]
    fn test_current_arch_binaries_are_embedded() {
        let (dll_name, sys_name) = crate::ec::winring0::arch_file_names();
        assert!(
            WinRing0Binaries::get(dll_name).is_some(),
            "missing embedded {}",
            dll_name
        );
        assert!(
            WinRing0Binaries::get(sys_name).is_some(),
            "missing embedded {}",
            sys_name
        );
    }
}
