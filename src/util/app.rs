//! 应用级元数据与路径常量（唯一事实来源）。

/// 应用显示名称（唯一事实来源）。
///
/// 该字符串曾散落在 window.rs（FindWindowW 按标题找主窗口）、gui/app.rs
/// （eframe::run_native 标题）、gui/view.rs（标题栏文字）、tray/worker.rs
/// （托盘 tooltip）与 autostart.rs（任务作者）各自硬编码——其中 eframe 标题
/// 与 `MAIN_WINDOW_TITLE` 一旦漂移，托盘隐藏/显示/退出等功能会静默失效。
/// 统一收敛到此处后，任一处改名都会同时作用于全部展示/查找路径。
pub const APP_NAME: &str = "Xiaomi PC Manager Lite";

/// 面向机器的产品标识符（无空格），全项目唯一事实来源（修订 1.50 收敛）。
///
/// 历史实现把 `"XiaomiPcManagerLite"` 字面量散落五处：AppUserModelID
/// （launch.rs）、计划任务名 `TASK_NAME`（autostart.rs）、配置目录名
/// （config.rs 两处）、日志目录名（`log_file_path`）、单实例互斥体名
/// （single_instance.rs）。任一处漂移都会静默破坏对应功能（AUMID 漂移 →
/// 托盘气泡被丢弃；互斥体名漂移 → 双实例并存），统一收敛到此处。
pub const APP_ID: &str = "XiaomiPcManagerLite";

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
        .unwrap_or_else(|| std::env::temp_dir().join(APP_ID).join("app.log"))
}

/// 可执行文件所在目录（`current_exe()` 的父目录，绝对路径）。
///
/// 修订 1.50 收敛：此前 `ec::embed`（两处）、`ec::winring0` 各自手写
/// `current_exe() + parent()`，父目录缺失的错误文案还不一致
/// （"no parent directory" vs "可执行文件路径没有父目录"）。统一到此处后，
/// 错误形状与用户可见文案保持一致。
pub fn exe_dir() -> Result<std::path::PathBuf, String> {
    std::env::current_exe()
        .map_err(|e| crate::util::err_fmt("current_exe", e))?
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| "可执行文件路径没有父目录".to_string())
}
