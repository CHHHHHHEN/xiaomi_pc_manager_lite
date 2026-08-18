//! 共享锁毒化恢复与"只告警一次"闩。

/// 获取互斥锁，毒锁（持有者 panic）时记录警告并恢复。
///
/// 生产代码在 6 处（winring0 端口 I/O、WMI 应答锁、托盘 NID、窗口图标缓存）
/// 各自重复实现过同一套恢复样板（`lock().unwrap_or_else(|e| e.into_inner())`），
/// 统一收敛到 `util::sync`。恢复被污染锁是安全且合理的选择：临界区持有者
/// panic 后数据可能不一致，但对这些"纯命令通道/原子状态"场景，恢复比死锁/
/// 退出更可取——原代码正是这样做的，此处仅收敛样板、不改语义。
pub(crate) fn lock_or_recover<'a, T>(
    lock: &'a std::sync::Mutex<T>,
    what: &str,
) -> std::sync::MutexGuard<'a, T> {
    lock.lock().unwrap_or_else(|e| {
        log::warn!("{} mutex was poisoned, recovering", what);
        e.into_inner()
    })
}

/// 获取共享读锁，毒锁（持有者 panic）时记录警告并恢复。
///
/// 与 `lock_or_recover` 同一套毒锁恢复约定，面向 `RwLock`（Fn 绑定表读侧）。
pub(crate) fn lock_read_or_recover<'a, T>(
    lock: &'a std::sync::RwLock<T>,
    what: &str,
) -> std::sync::RwLockReadGuard<'a, T> {
    lock.read().unwrap_or_else(|e| {
        log::warn!("{} rwlock was poisoned, recovering", what);
        e.into_inner()
    })
}

/// 获取独占写锁，毒锁（持有者 panic）时记录恢复。
///
/// 与 `lock_or_recover` 同一套毒锁恢复约定，面向 `RwLock`（Fn 绑定表写）。
pub(crate) fn lock_write_or_recover<'a, T>(
    lock: &'a std::sync::RwLock<T>,
    what: &str,
) -> std::sync::RwLockWriteGuard<'a, T> {
    lock.write().unwrap_or_else(|e| {
        log::warn!("{} rwlock was poisoned, recovering", what);
        e.into_inner()
    })
}

/// 按 `AtomicBool` 闩执行"只告警一次"：首次调用返回 true 并记录日志，之后
/// 同一闩下的重复告警静默返回 false。
///
/// 多个模块各自手写过同一套"`swap(true)` 判首次 + 记日志"样板（power.rs
/// 的未知电源状态、wmi.rs 的熔断根因），统一收敛到 `util::sync`。`level`
/// 由调用方指定：两者告警严重度不同（warn vs error），收敛样板但保留各自的
/// 日志级别与措辞。
pub(crate) fn log_once(
    level: log::Level,
    flag: &std::sync::atomic::AtomicBool,
    message: impl std::fmt::Display,
) -> bool {
    let first = !flag.swap(true, std::sync::atomic::Ordering::Relaxed);
    if first {
        log::log!(level, "{}", message);
    }
    first
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
