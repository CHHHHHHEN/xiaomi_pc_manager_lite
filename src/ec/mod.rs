//! 硬件访问层（Hardware Abstraction Layer）：实现 `app::ec` 定义的后端端口。
//!
//! 端口类型（`EcBackend` trait、`EcError`、`BackendPreference`、
//! `EcBackendFactory`）定义在 `app::ec`；本层只保留具体适配器
//! （`winring0`/`wmi`）、后端创建（`backend::create_backend`）与空后端
//! （`backend::NullBackend`）。通用 WMI/COM 基础设施在 crate 根的 `wmi_util`。
//!
//! 依赖方向：`ec` → `app`（适配器依赖端口），`app` 不依赖 `ec`。

pub mod addr;
pub mod backend;
pub mod fn_watcher;
pub mod winring0;
pub mod wmi;

/// 共享的内存测试后端（仅测试编译时存在）。
#[cfg(test)]
pub mod mock;
