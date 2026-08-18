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
/// `1.0.0.6`；Windows FileVersion/ProductVersion 与 GUI 展示、日志首行
/// 均以此为唯一事实来源，`Cargo.toml` 的 `version` 保持 `1.0.0`。
pub const APP_VERSION: &str = "1.0.0.6";

/// 主窗口默认尺寸与最小尺寸（逻辑像素）。
///
/// eframe 创建（gui/app.rs）、窗口位置恢复兜底（platform/window.rs）、GUI
/// 尺寸钳制（gui/view.rs）曾各自书写同一组字面量且已出现漂移
/// （window.rs 的 320×200 与 app.rs 的 400×500 不一致）——统一收敛到此处。
pub const DEFAULT_WINDOW_SIZE: (f32, f32) = (520.0, 680.0);
pub const MIN_WINDOW_SIZE: (f32, f32) = (400.0, 500.0);

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

    /// 从 `OsStr`（路径/命令行参数）直接构造 UTF-16 缓冲，**不经 `String`
    /// 中间层**。
    ///
    /// Windows 路径/argv 是 UTF-16，可能含未配对代理项（非合法 UTF-8）；
    /// `to_string_lossy` 会把它们替换成 U+FFFD，再编码回 UTF-16 就得到一条
    /// **不同的路径**——用于 `ShellExecuteW` 重启动（路径错 → 提权失败静默
    /// 继续）、DLL 加载（路径错 → 找不到文件）时是静默错误。直接用
    /// `encode_wide` 保留原始 UTF-16 单元（修订 1.46 安全加固）。
    #[cfg(windows)]
    pub fn from_os_str(s: &std::ffi::OsStr) -> Self {
        use std::os::windows::ffi::OsStrExt;
        Self(s.encode_wide().chain(std::iter::once(0)).collect())
    }

    /// UTF-16 缓冲（不含结尾 NUL）。用于构造 `BSTR`（`BSTR::from_wide` 会
    /// 自行加 NUL）等需要"内容不含终止符"的转换。
    #[cfg(windows)]
    pub fn units_no_nul(&self) -> &[u16] {
        // 缓冲恒以单个 0 结尾（new/from_os_str 都追加），安全截掉。
        debug_assert_eq!(self.0.last(), Some(&0));
        &self.0[..self.0.len() - 1]
    }

    /// 从**不含结尾 NUL 的 UTF-16 单元**构造（追加 NUL）。
    ///
    /// 用于提权命令行的宽域构建（`privilege::build_command_line` 直接在
    /// UTF-16 域拼接、再交回 `WideString` 持有），避免 `to_string_lossy`
    /// 往返（修订 1.46 审计，见 `from_os_str` 注释）。
    #[cfg(windows)]
    pub fn from_units(units: Vec<u16>) -> Self {
        let mut v = units;
        v.push(0);
        Self(v)
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

/// 从 `std::panic::catch_unwind` 返回的 panic payload 中提取人类可读消息。
///
/// 各后台线程（main 后端 init / 托盘 / Fn / WMI / 电池健康 / 自启动）此前
/// 各自重复实现过同一套 `downcast_ref::<&str>() → downcast_ref::<String>() →
/// "unknown panic"` 样板，统一收敛到此处。`panic!`/`assert!` 的载荷约定为
/// `&str` 或 `String`；其余类型（`panic!(value)` 走 `Display`）没有统一消息
/// 来源，一律记 "unknown panic"。
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().map(|s| s.to_string()))
        .unwrap_or_else(|| "unknown panic".into())
}

/// 启动一个命名后台线程，兜底捕获 panic 并记录语义化日志。
///
/// 托盘、Fn 监听、WMI worker、电池健康、自启动 worker、WMI 恢复探测等
/// 后台线程此前各自手写同一份 `thread::Builder + catch_unwind +
/// panic_message` 样板（修订 1.33/1.40/1.44 逐处补齐），存在规格漂移风险——
/// 统一收敛到此处（修订 1.47 清理）：
/// - **thread::Builder 而非 thread::spawn**（L1 规则）：spawn 失败（OS 线程
///   资源耗尽）时 Builder 返回 `Err` 由调用方记录告警；裸 spawn 会把 panic
///   传播到调用线程，在 GUI update 线程上直接杀死应用。
/// - **catch_unwind**（修订 1.32，release 已移除 panic=abort）：后台线程内
///   panic 只会静默终止该线程——功能失效而应用仍存活、无任何日志。捕获后
///   用 `panic_message` 记录语义化错误，调用方可据日志识别线程是否还活着。
///
/// 需要线程返回值（如 main 后端 init 的 `JoinHandle::join`）或把 panic 转成
/// 回传结果（如 backend-switch 的 catch→Err 通道）的场景使用 `catch_panic`。
pub fn spawn_guarded<F>(name: &str, f: F) -> std::io::Result<std::thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    let name_owned = name.to_string();
    std::thread::Builder::new()
        .name(name_owned.clone())
        .spawn(move || {
            if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
                let payload = panic_message(&*panic);
                log::error!("{}: thread panicked: {}", name_owned, payload);
            }
        })
}

/// 在闭包内捕获 panic，转成 `Err(消息字符串)`（经 `panic_message` 提取）。
///
/// 与 `spawn_guarded` 的"捕获后仅记日志、闭包无返回值"相对：此处的闭包
/// 需要**返回结果**，调用方把 panic 作为失败结果继续传递（main.rs 后端
/// 初始化线程的 join 值、commands.rs backend-switch 线程经通道回传的
/// `BackendSwitchResult`）。两处此前各自手写同一份 `catch_unwind +
/// unwrap_or_else + panic_message` 样板（修订 1.45/1.46 逐处补齐），收敛到此。
pub(crate) fn catch_panic<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> T,
{
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .map_err(|panic| panic_message(&*panic))
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

/// 获取独占写锁，毒锁（持有者在 panic）时记录恢复。
///
/// 与 `lock_or_recover` 同一套毒作用约定，面向 `RwLock`（Fn 绑定表写）。
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
/// 多个模块各自手写过同一份"`swap(true)` 判首次 + 记日志"样板（power.rs
/// 的未知电源状态、wmi.rs 的熔断根因），统一收敛到此处。`level` 由调用方
/// 指定：两者告警严重度不同（warn vs error），收敛样板但保留各自的日志级别
/// 与措辞。
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

    /// panic payload 消息提取（统一收敛点）：`&str` 与 `String` 两种
    /// `panic!` 载荷都被还原，其余类型统一按 "unknown panic"。
    #[test]
    fn test_panic_message_variants() {
        let str_payload =
            std::panic::catch_unwind(|| panic!("&str payload")).expect_err("must panic");
        assert_eq!(panic_message(&*str_payload), "&str payload");

        let string_payload =
            std::panic::catch_unwind(|| panic!("{}", "String payload")).expect_err("must panic");
        assert_eq!(panic_message(&*string_payload), "String payload");

        // 非字符串载荷（panic_any(42)）：无统一消息，按 unknown 处理。
        let int_payload =
            std::panic::catch_unwind(|| std::panic::panic_any(42i32)).expect_err("must panic");
        assert_eq!(panic_message(&*int_payload), "unknown panic");
    }
}
