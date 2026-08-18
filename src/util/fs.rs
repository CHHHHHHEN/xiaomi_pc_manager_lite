//! 文件系统原子写（跨层共用）。
//!
//! 历史实现（修订 1.46 审计）中 `app::config`（配置保存）与 `ec::embed`
//! （驱动提取）各自手写了一份"唯一临时文件 + rename"的原子写——命名格式、
//! 失败清理与 fsync 行为各不相同，`ec::embed` 的版本还缺 fsync。收敛到
//! 此处后语义统一（修订 1.49 整理）：写临时文件 → `sync_all` 落盘 → rename
//! 覆盖目标 → 任一失败清理本次临时文件。

use std::io::Write;
use std::path::Path;

/// 原子写文件：先写同目录唯一临时文件，再 rename 覆盖目标。
///
/// **为什么**（修订 1.46 安全加固）：本进程以管理员运行，`std::fs::write`
/// 直接写目标路径会**跟随目标处的符号链接/重解析点**（CreateFile 语义），
/// 低权限用户可在可写的目标目录（便携部署常见）预置同名符号链接指向任意
/// 文件，提权进程随即截断/覆写该文件（TOCTOU 提权写入）。先写唯一临时文件
/// 再 `std::fs::rename`（Windows 上映射为 `MoveFileExW
/// (MOVEFILE_REPLACE_EXISTING)`）则**替换整个目录项**而非跟随重解析点——
/// 目标若为链接，被整体替换为正常文件，不会写穿到链接目标。
///
/// **落盘前 fsync**（修订 1.36）：`fs::write` 只关闭句柄，不保证数据块先于
/// 目录项 rename 落盘——断电/强杀时可能出现"目录项已更新而数据块未写"的
/// 0 长度/撕裂文件。写后 `sync_all` 确保数据与元数据刷盘后再 rename。注：
/// Windows NTFS 的 rename 原子性由文件系统日志保证，目录项本身的持久化
/// 不需要额外目录句柄 fsync（与 POSIX 语义不同），此处已覆盖本平台可实现性。
///
/// 临时文件名含 PID + 自增序号：同目录并发写入互不冲突（rename 原子性），
/// 形如 `config.toml.1234.0.tmp`——配置目录的启动清理按 `config.toml.`
/// 前缀 + `.tmp` 后缀匹配（见 `app::config::ConfigStore::cleanup_stale_tmp_files`）。
/// 写入/rename 失败时删除本次残留的临时文件。
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<(), String> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = path
        .parent()
        .ok_or_else(|| format!("no parent for {:?}", path))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("no file name for {:?}", path))?;
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_path = dir.join(format!(
        "{}.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        seq
    ));

    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(data)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("write {}: {}", tmp_path.to_string_lossy(), e));
    }
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("rename {}: {}", path.to_string_lossy(), e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 原子写（临时文件 + rename）必须覆盖既有文件内容（修订 1.46 安全
    /// 加固的写入语义：rename 替换目录项而非跟随重解析点）。
    #[test]
    fn test_atomic_write_replaces_existing() {
        let dir = std::env::temp_dir().join(format!("xmpl-atomic-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("target.bin");
        // 首次写入。
        atomic_write(&path, b"first").expect("write must succeed");
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        // 覆盖既有文件。
        atomic_write(&path, b"second-larger").expect("overwrite must succeed");
        assert_eq!(std::fs::read(&path).unwrap(), b"second-larger");
        // 没有残留临时文件（写入成功路径必 rename）。
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with("target.bin.") && name.ends_with(".tmp")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temp files may remain: {:?}",
            leftovers
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 写入失败（目标目录不存在）时返回错误、不 panic。
    #[test]
    fn test_atomic_write_missing_parent_dir_errors() {
        let dir = std::env::temp_dir().join(format!("xmpl-atomic-write-{}", std::process::id()));
        let path = dir.join("missing").join("target.bin");
        // 目录不存在：必须报错而非静默成功。
        assert!(atomic_write(&path, b"data").is_err());
    }
}
