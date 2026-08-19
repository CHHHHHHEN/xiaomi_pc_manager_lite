//! WinRing0 驱动的运行时提取与文件名校验（`ec` 适配器层内部）。
//!
//! 本模块承载驱动 DLL/SYS 的嵌入资源提取、原子写、EXE 目录副本与嵌入副本的
//! 内容一致性校验，并提供架构文件名对 `arch_file_names` 作为唯一事实来源。
//! 历史实现放在 crate 根的 `embed.rs`，与 `ec::winring0` 双向依赖（embed 取
//! `arch_file_names`、winring0 取 `atomic_write`/`extract`）——收敛到 `ec`
//! 内部后依赖方向单一：`ec::winring0` → `ec::embed`。

use rust_embed::RustEmbed;
use std::path::{Path, PathBuf};

#[derive(RustEmbed)]
#[folder = "bin"]
struct WinRing0Binaries;

/// 当前架构下的 WinRing0 驱动文件名对 (DLL, SYS)。
///
/// `embed` 的提取路径与 `winring0` 的加载路径都依赖同一组文件名，曾在两处
/// 各自硬编码、漂移风险高——统一收敛到此处（驱动文件名的唯一事实来源），
/// `winring0` 经 `super::embed::arch_file_names` 引用，不再互相依赖。
pub fn arch_file_names() -> (&'static str, &'static str) {
    if cfg!(target_pointer_width = "64") {
        ("WinRing0x64.dll", "WinRing0x64.sys")
    } else {
        ("WinRing0.dll", "WinRing0.sys")
    }
}

/// 嵌入式 WinRing0 DLL 的原始字节（按当前架构文件名，单一事实来源）。
///
/// 加载路径用它做**内容校验**（见 `winring0::WinRing0Backend::new` 与
/// `extract_winring0`）：仅当 EXE 目录中的 DLL 与此字节一致时才允许直接加载，
/// 否则按嵌入副本重新提取——杜绝"EXE 目录被低权限用户塞入同名恶意 DLL"后
/// 提权进程直接加载（修订 1.46 安全加固）。
pub fn embedded_dll_bytes() -> Result<std::borrow::Cow<'static, [u8]>, String> {
    let (dll_name, _) = arch_file_names();
    WinRing0Binaries::get(dll_name)
        .map(|f| f.data)
        .ok_or_else(|| format!("{} not found in embedded binaries", dll_name))
}

/// 原子写文件：先写同目录唯一临时文件，再 rename 覆盖目标。
///
/// **为什么**（修订 1.46 安全加固）：本进程以管理员运行，`std::fs::write`
/// 直接写目标路径会**跟随目标处的符号链接/重解析点**（CreateFile 语义），
/// 低权限用户可在可写的 EXE 目录（便携部署常见）预置 `WinRing0x64.dll/.sys`
/// 符号链接指向任意文件，提权进程随即截断/覆写该文件（TOCTOU 提权写入）。
/// 先写唯一临时文件再 `std::fs::rename`（Windows 上映射为
/// `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`）则**替换整个目录项**而非跟随
/// 重解析点——目标若为链接，被整体替换为正常文件，不会写穿到链接目标。
///
/// 实现统一收敛到 `util::fs::atomic_write`（修订 1.49 整理：与
/// `app::config` 的配置保存共用同一份实现，含 fsync 落盘）。
pub(crate) fn atomic_write(path: &Path, data: &[u8]) -> Result<(), String> {
    crate::util::atomic_write(path, data)
}

/// 校验 EXE 目录中的 DLL 是否与嵌入副本**内容一致**（字节级）。
///
/// 一致 = 自上次提取以来未被外部改写（可信副本，直接加载）；不一致/缺失 =
/// 可能被低权限用户在可写 EXE 目录替换为恶意 DLL——必须重新提取嵌入副本。
pub fn exe_dir_dll_is_embedded() -> bool {
    let (dll_name, _) = arch_file_names();
    // 父目录解析失败（当前 exe 不可查）按"不一致"处理：宁可重新提取嵌入
    // 副本，也不加载来源可疑的同名 DLL。
    let Ok(exe_dir) = crate::util::exe_dir() else {
        return false;
    };
    let Ok(disk) = std::fs::read(exe_dir.join(dll_name)) else {
        return false;
    };
    embedded_dll_bytes()
        .map(|embedded| disk.as_slice() == embedded.as_ref())
        .unwrap_or(false)
}

pub fn extract_winring0() -> Result<PathBuf, String> {
    // 文件名对（DLL, SYS）统一由本模块的 arch_file_names 提供，避免两处
    // 硬编码漂移（winring0 的加载路径经 `super::embed::arch_file_names` 引用）。
    let (dll_name, sys_name) = arch_file_names();

    // 文件名对由 arch_file_names() 保证与当前架构一致（32 位：WinRing0.dll/.sys，
    // 64 位：WinRing0x64.dll/.sys）。**不得**回退到其它架构的文件名：64 位进程
    // 加载 32 位 DLL 必然失败（ERROR_BAD_EXE_FORMAT），且该无条件回退会掩盖
    // "嵌入资源缺失"这一打包问题——错误从清晰的 "xxx not found in embedded
    // binaries" 变成难以排查的 LoadLibrary 失败。精确取用当前架构文件名，缺失即报错。
    let embedded_dll = WinRing0Binaries::get(dll_name)
        .ok_or_else(|| format!("{} not found in embedded binaries", dll_name))?;

    let embedded_sys = WinRing0Binaries::get(sys_name)
        .ok_or_else(|| format!("{} not found in embedded binaries", sys_name))?;

    let target_dir = crate::util::exe_dir()?;

    // Clean up old extraction locations from previous versions.
    // 注意：**不能**整体删除 `%TEMP%\XiaomiPcManagerLite`——该目录正是
    // `main::init_logging` 的日志目录（app.log 由本进程持有打开句柄，删除
    // 必然失败且永远静默失败），而且会误删同目录下其它应用/实例的文件。
    // 只删除按文件名精确匹配的遗留副本，绝不整目录删除。
    //
    // 旧路径在 `%WINDIR%\Temp`。WINDIR 是 Windows 系统变量恒被设置；万一
    // 缺失（环境异常）则跳过该清理并记录 debug——不猜测系统根目录
    // （历史实现硬编码 `C:\Windows` 兜底），该清理本身是尽力而为。
    match std::env::var_os("WINDIR") {
        Some(dir) => {
            let old_sys_dir = std::path::PathBuf::from(dir).join("Temp");
            let _ = std::fs::remove_file(old_sys_dir.join(dll_name));
            let _ = std::fs::remove_file(old_sys_dir.join(sys_name));
        }
        None => {
            log::debug!("WINDIR not set; skipping legacy %WINDIR%\\Temp cleanup");
        }
    }

    // 原子写（临时文件 + rename）：不跟随目标重解析点，见 atomic_write 注释。
    // remove_file 前置清理改为原子替换承载：直接 rename 覆盖既有文件（含陈旧
    // 句柄残留时的重试——历史"remove+write"存在竞态窗口，替换为单次原子操作）。
    //
    // DLL 与 SYS **统一**用带重试的原子写（修订 1.47 审计）：历史实现只给
    // DLL 3 次尝试，SYS 单次直写——上一实例仍持有 `.sys` 句柄时（驱动服务
    // 卸载/文件占用竞态），SYS 提取单次失败即整个后端初始化失败，与 DLL
    // 的容错策略不一致。两者都可能被残留句柄短暂占用，重试节奏统一。
    let dll_path = target_dir.join(dll_name);
    atomic_write_with_retry(&dll_path, &embedded_dll.data)?;
    let sys_path = target_dir.join(sys_name);
    atomic_write_with_retry(&sys_path, &embedded_sys.data)?;

    log::info!("Extracted {} + {} to {:?}", dll_name, sys_name, target_dir);
    Ok(dll_path)
}

/// 原子写 + 短暂重试（覆盖旧版本残留句柄的竞态，DLL/SYS 共用）。
fn atomic_write_with_retry(path: &std::path::Path, data: &[u8]) -> Result<(), String> {
    let mut last_err = None;
    for attempt in 0..3 {
        match atomic_write(path, data) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                if attempt < 2 {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
            }
        }
    }
    Err(last_err.expect("loop always sets last_err on failure"))
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
        let (dll, sys) = arch_file_names();
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
        let (dll_name, sys_name) = arch_file_names();
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

    /// 嵌入 DLL 字节可读且与嵌入资源一致（exe_dir_dll_is_embedded 的输入源）。
    #[test]
    fn test_embedded_dll_bytes_present() {
        let bytes = embedded_dll_bytes().expect("embedded DLL bytes must exist");
        assert!(!bytes.is_empty(), "embedded DLL must not be empty");
    }
}
