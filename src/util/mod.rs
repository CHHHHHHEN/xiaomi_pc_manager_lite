//! 跨层工具集（leaf 层，无内部依赖）。
//!
//! 本模块是唯一不依赖任何 crate 内部模块的工具层，供 `win`/`app`/`ec`/
//! `platform`/`gui`/`tray` 各层使用。按职责拆分为独立子模块，避免"一切
//! 工具堆在一个文件"：
//! - [`app`]：应用元数据（名称/版本/窗口尺寸/日志路径）——全项目单一事实来源；
//! - [`text`]：UTF-16 缓冲（`WideString`），Windows FFI 的安全承载；
//! - [`thread`]：命名后台线程的 `spawn_guarded`/`catch_panic` 兜底与
//!   panic 消息提取；
//! - [`sync`]：互斥锁毒化恢复与"只告警一次"闩；
//! - [`fs`]：原子文件写（临时文件 + fsync + rename），配置与驱动提取共用。
//!
//! 为了不给既有调用点引入破坏，本模块将各子模块的公开项统一 `pub use`
//! 重导出（如 `crate::util::log_file_path`、`crate::util::WideString` 等
//! 仍可直接使用）；新增代码应优先按职责引用子模块路径。

pub mod app;
pub(crate) mod fs;
pub mod sync;
pub mod text;
pub mod thread;

pub use app::{
    exe_dir, log_file_path, APP_ID, APP_NAME, APP_VERSION, DEFAULT_WINDOW_SIZE, MIN_WINDOW_SIZE,
};
pub(crate) use fs::atomic_write;
pub(crate) use sync::{lock_or_recover, lock_read_or_recover, lock_write_or_recover, log_once};
pub use text::WideString;
pub(crate) use thread::catch_panic;
pub use thread::{panic_message, spawn_guarded};

/// `"<标签>: <错误>"` 用户可见错误文案的统一构造（修订 1.50 收敛）。
///
/// 历史实现散落各处：`gui/commands.rs` 的私有 `err_fmt`，以及
/// autostart/com/message_window/icon/embed/battery_health 等模块里
/// 40+ 处手写的 `format!("Label: {}", e)`。措辞形状（冒号 + 空格）是全项目
/// 用户可见错误的统一约定，一旦某处漏写冒号/空格，文案会与其他错误观感
/// 不一致。收敛到 leaf 层单一来源，各调用点只需提供标签。
pub fn err_fmt(label: &str, e: impl std::fmt::Display) -> String {
    format!("{}: {}", label, e)
}
