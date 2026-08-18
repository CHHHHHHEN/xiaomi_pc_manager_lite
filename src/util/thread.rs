//! 线程/panic 兜底工具。

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
/// 统一收敛到 `util::thread`：
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
/// `BackendSwitchResult`）。两处此前各自手写同一套 `catch_unwind +
/// unwrap_or_else + panic_message` 样板（修订 1.45/1.46 逐处补齐），收敛到此。
pub(crate) fn catch_panic<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> T,
{
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .map_err(|panic| panic_message(&*panic))
}

#[cfg(test)]
mod tests {
    use super::*;

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
