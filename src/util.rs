use windows::core::PCWSTR;

/// 应用显示名称（唯一事实来源）。
///
/// 该字符串曾散落在 window.rs（FindWindowW 按标题找主窗口）、gui/app.rs
/// （eframe::run_native 标题）、gui/view.rs（标题栏文字）、tray/worker.rs
/// （托盘 tooltip）与 autostart.rs（任务作者）各自硬编码——其中 eframe 标题
/// 与 `MAIN_WINDOW_TITLE` 一旦漂移，托盘隐藏/显示/退出等功能会静默失效。
/// 统一收敛到此处后，任一处改名都会同时作用于全部展示/查找路径。
pub const APP_NAME: &str = "Xiaomi PC Manager Lite";

/// 面向用户展示的版本号。
///
/// Cargo 的 `CARGO_PKG_VERSION` 是 semver 三段号，无法表达四段的
/// `1.0.0.5`；Windows FileVersion/ProductVersion 与 GUI 展示、日志首行
/// 均以此为唯一事实来源，`Cargo.toml` 的 `version` 保持 `1.0.0`。
pub const APP_VERSION: &str = "1.0.0.5";

/// 日志文件路径（唯一事实来源）。
///
/// 历史实现把"默认 `%TEMP%\XiaomiPcManagerLite\app.log` / `XIAOMI_LOG_FILE`
/// 覆盖"的逻辑散落在 main.rs（init_logging）与 GUI（打开日志按钮）各自实现，
/// 存在漂移风险。统一收敛到此处后，启动初始化与 GUI"打开日志"展示的是同一
/// 个路径，不会出现"日志写到了 A 处、GUI 打开 B 处"的错位。
pub fn log_file_path() -> std::path::PathBuf {
    std::env::var_os("XIAOMI_LOG_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .join("XiaomiPcManagerLite")
                .join("app.log")
        })
}

/// 持有一个带结尾 NUL 的 UTF-16 缓冲，并据此提供 `PCWSTR` 指针。
///
/// 历史实现 `to_pcwstr(s) -> (Vec<u16>, PCWSTR)` 把缓冲与指针拆开返回，
/// 调用方必须自行保证 `let (_buf, ptr) = ...` 的缓冲在 FFI 调用期间存活
/// ——缓冲一旦先于指针 drop，指针即为悬垂，在 `unsafe` FFI 中使用就是
/// use-after-free。`WideString` 将缓冲与指针绑定在同一所有权下，
/// `as_pcwstr()` 借用自身返回指针，编译器保证指针存活期内缓冲必然存在，
/// 从类型层面消除了该悬垂风险。
pub struct WideString(Vec<u16>);

impl WideString {
    pub fn new(s: &str) -> Self {
        Self(s.encode_utf16().chain(std::iter::once(0)).collect())
    }

    pub fn as_pcwstr(&self) -> PCWSTR {
        PCWSTR(self.0.as_ptr())
    }

    /// UTF-16 缓冲（含结尾 NUL）。用于需要**拷贝**到固定大小数组的场景
    /// （如 NOTIFYICONDATAW 的 szInfo/szInfoTitle），调用方负责截断到目标
    /// 数组容量。
    pub fn units(&self) -> &[u16] {
        &self.0
    }
}

/// 获取互斥锁，毒锁（持有者 panic）时记录警告并恢复。
///
/// 生产代码在 6 处（winring0 端口 I/O、WMI 应答锁、托盘 NID、窗口图标缓存）
/// 各自重复实现过同一恢复样板（`lock().unwrap_or_else(|e| e.into_inner())`），
/// 统一收敛到此处。恢复被污染锁是安全且合理的选择：临界区持有者 panic 后
/// 数据可能不一致，但对这些"纯命令通道/原子状态"场景，恢复比死锁/退出更可取
/// ——原代码正是这样做的，此处仅收敛样板、不改语义。
pub(crate) fn lock_or_recover<'a, T>(
    lock: &'a std::sync::Mutex<T>,
    what: &str,
) -> std::sync::MutexGuard<'a, T> {
    lock.lock().unwrap_or_else(|e| {
        log::warn!("{} mutex was poisoned, recovering", what);
        e.into_inner()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 正常获取锁必须返回正确的值（与各处原实现语义一致）。
    #[test]
    fn test_lock_or_recover_normal() {
        let lock = std::sync::Mutex::new(42u32);
        {
            let guard = lock_or_recover(&lock, "test");
            assert_eq!(*guard, 42);
        }
    }

    /// 毒锁（持有者 panic）必须被恢复而不是死锁/传播 poison。
    ///
    /// 回归测试（历史实现）：毒锁恢复路径曾被嵌套在一个同名测试函数体内，
    /// `#[test]` 对嵌套 item 无效，该路径从未真正执行（编译器告警
    /// "cannot test inner items"）——此处必须独立成测试，否则恢复逻辑
    /// 回归时测试仍会假绿。
    #[test]
    fn test_lock_or_recover_poisoned() {
        let lock = std::sync::Mutex::new(42u32);
        // 模拟持锁 panic 造成的污染：把 guard 移进 panic 闭包，随展开被
        // drop（panic 时持有 guard 才会设置 Mutex 的 poisoned 标志）。
        {
            let guard = lock.lock().unwrap();
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _held = guard;
                panic!("simulated panic inside lock");
            }))
            .ok();
        }
        let guard = lock_or_recover(&lock, "test");
        assert_eq!(*guard, 42, "poisoned lock must be recovered");
    }
}
