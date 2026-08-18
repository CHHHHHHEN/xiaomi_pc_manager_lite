//! 应用核心层（Application Core）：领域模型、策略与用例。
//!
//! 本层集中承载与硬件无关的领域逻辑：配置、性能指标、充电上限/电池养护策略、
//! Fn 绑定模型、通知判定、启动编排。**不依赖任何 GUI 框架（egui/eframe）、
//! Windows 平台 API 或硬件访问适配器（`ec`）**——平台/硬件查询经端口抽象
//! （`ec::EcBackend`、`ec::EcBackendFactory`、`power::PowerSource`、
//! `sink::CommandSink`）由上层适配器实现，纯逻辑可脱离平台单元测试。
//!
//! 分层关系（依赖方向单一、无环）：
//! - `app`（本层）：领域模型 + 用例编排 + 端口定义（`ec`/`power`/`sink`）；
//! - `ec`：硬件访问适配器（实现 `app::ec` 的端口）；
//! - `platform`：Windows 服务（实现 `app::power::PowerSource` 等端口）；
//! - `gui` / `tray`：表现层与组合根（依赖 app 的领域模型、ec/platform 的
//!   适配器实现）。

pub mod battery;
pub mod command;
pub mod config;
pub mod ec;
pub mod fnkey;
pub mod limits;
pub mod notify;
pub mod performance;
pub mod power;
pub mod sink;
pub mod startup;
