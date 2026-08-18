//! Xiaomi PC Manager Lite 应用库。
//!
//! 二进制入口（`src/main.rs`）只调用 [`launch::run`]；本 crate 承载全部应用
//! 逻辑，使测试、文档与潜在工具能以库的方式复用。
//!
//! 分层（依赖方向单一、无环）：
//! - `util`：跨层工具（应用元数据 / UTF-16 / 线程与 panic / 锁恢复）；
//! - `win`：Windows 互操作基础设施（COM/WMI 生命周期、VARIANT），只依赖 `util`；
//! - `app`：领域层（端口 + 纯逻辑），不依赖任何 GUI / 平台 / 硬件适配器；
//! - `ec`：硬件访问适配器（WinRing0 / WMI，含驱动嵌入），依赖 `app` / `win`；
//! - `platform`：Windows 平台集成（窗口 / 电源 / 自启动 / 提权 / 电池健康），
//!   依赖 `app` / `win`；
//! - `gui` / `tray`：表现层与组合根，依赖 `app` / `ec` / `platform` / `win`；
//! - `launch`：组合根启动编排（日志、提权、后端初始化、GUI 启动）。

pub mod app;
pub mod ec;
pub mod gui;
pub mod launch;
pub mod platform;
pub mod tray;
pub mod util;
pub mod win;

#[cfg(test)]
pub mod testutil;

pub use launch::run;
