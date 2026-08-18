//! 硬件访问端口（Port）：领域层对"硬件后端"的依赖契约。
//!
//! 本模块承载三层与硬件相关的领域类型，`ec` 适配器层实现/依赖它们，
//! 从而使依赖方向收敛为单向：
//!
//! ```text
//! app（领域，本文件：端口 + 错误）  ←──  ec（WinRing0/WMI 适配器实现端口）
//! ```
//!
//! - `EcBackend`：领域层需要的硬件操作抽象（读/写电池养护、充电上限、
//!   性能模式），与具体实现（端口 I/O / WMI 协议）无关；
//! - `EcError`：硬件操作失败的领域错误类型（适配器把平台错误映射为它）；
//! - `BackendPreference`：用户/配置层面的后端选择（Auto / WinRing0 / WMI）；
//! - `EcBackendFactory`：创建后端实例的端口。启动编排（`app::startup`）
//!   依赖该端口完成后端创建与 NullBackend 兜底，不直接接触 `ec` 的实现
//!   细节；组合根（main.rs）注入真实工厂。

use serde::{Deserialize, Serialize};

/// 后端偏好：用户/配置层面的选择（与具体后端实现解耦）。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum BackendPreference {
    /// 自动选择：WMI 优先，失败回退 WinRing0（F-HAL-13）。
    ///
    /// 注意与 `AppConfig::default().backend`（= `Wmi`）的差异：枚举级默认
    /// `Auto` 用于"未指定偏好"的语义默认（如 `MockBackend::default()`、
    /// `EcBackend::preference()` 的 trait 默认）；应用配置的默认是 `Wmi`。
    /// 两者在 `create_backend` 下行为等价（Auto 第一步即尝试 WMI），刻意
    /// 保留差异——不要把 `BackendPreference::default()` 当成应用默认使用
    /// （修订 1.46 审计）。
    #[default]
    Auto,
    WinRing0,
    Wmi,
}

/// 硬件访问失败的错误类型（适配器把平台/驱动错误统一映射为它）。
#[derive(Debug, Clone, thiserror::Error)]
pub enum EcError {
    #[error("WinRing0 DLL 加载失败: {0}")]
    DllLoad(String),
    #[error("WinRing0 初始化失败: {0}")]
    InitFailed(String),
    #[error("WMI 连接失败: {0}")]
    WmiConnect(String),
    #[error("WMI MICommonInterface 未找到")]
    WmiInterfaceNotFound,
    #[error("WMI MiInterface 调用失败 (状态={0:#x})")]
    WmiCallFailed(u16),
    #[error("WMI 调用返回错误 (hr=0x{0:08X})")]
    WmiCallHResult(u32),
    #[error("EC 操作超时 (地址: {0:#x})")]
    Timeout(u16),
    #[error("硬件返回无效数据: {0}")]
    InvalidData(String),
    #[error("后端不可用: {0}")]
    BackendUnavailable(String),
}

/// 硬件操作能力端口：领域层与 GUI 依赖的唯一硬件接口。
pub trait EcBackend: Send + Sync {
    fn name(&self) -> &'static str;

    // ── High-level battery operations ──
    fn get_battery_care_enabled(&self) -> Result<bool, EcError>;
    fn get_charge_limit(&self) -> Result<u8, EcError>;
    fn set_battery_care(&self, enabled: bool) -> Result<(), EcError>;
    fn set_charge_limit(&self, percent: u8) -> Result<(), EcError>;

    /// 一次调用同时获取电池养护状态与充电上限。
    ///
    /// 默认实现分别调用两个 getter；能一次往返同时返回两者的后端
    /// （如 WMI：养护位与上限来自同一条命令的同一响应字段）应覆写，
    /// 否则 GUI 每次刷新会多一次完整硬件往返（B-WMI-1）。
    fn get_battery_state(&self) -> Result<(bool, u8), EcError> {
        let care = self.get_battery_care_enabled()?;
        let limit = self.get_charge_limit()?;
        Ok((care, limit))
    }

    // ── High-level performance mode operations ──
    fn get_performance_mode(&self) -> Result<u8, EcError>;
    fn set_performance_mode(&self, mode: u8) -> Result<(), EcError>;

    /// Whether the backend supports arbitrary (continuous) charge limit values.
    /// WinRing0 supports 0–100, WMI only supports a fixed set.
    fn supports_continuous_charge_limit(&self) -> bool {
        true
    }

    /// The backend preference this backend corresponds to.
    ///
    /// Used to detect "already on this backend" so a switch can be a no-op
    /// (recreating the same backend would tear down the driver that is in
    /// active use, see winring0.rs) and to let the GUI reflect the backend
    /// that is actually running after a fallback.
    fn preference(&self) -> BackendPreference {
        BackendPreference::Auto
    }

    /// Whether this backend is the null placeholder (created when no real
    /// backend could be initialized).
    ///
    /// Callers use this instead of comparing `name()` strings (which is
    /// fragile — a renamed display name silently breaks the check): when the
    /// backend is null, the *user's configured* preference is still the
    /// authoritative one (it will be retried on next startup), while for a
    /// live backend the actual `preference()` is shown.
    fn is_null(&self) -> bool {
        false
    }

    /// Whether this backend is in a **faulted/latched** state requiring
    /// recreation to recover (e.g. WMI 应答超时熔断)。
    ///
    /// 默认返回 false（WinRing0 无此概念）；WMI 后端在应答超时后熔断返回
    /// true。调用方（GUI 后端切换）用它绕过"同种后端 no-op"优化：熔断的
    /// 后端即便偏好未变也必须重建才能恢复（F2）。
    fn needs_rebuild(&self) -> bool {
        false
    }
}

/// 后端创建端口：把"如何构造后端实例"与"如何使用后端"解耦。
///
/// 启动编排（`app::startup::init_backend`）依赖本端口完成真实后端的创建
/// 与 NullBackend 兜底，不再直接调用 `ec` 的工厂或具体后端类型。组合根
/// （main.rs）持有真实实现（`ec::backend::BackendFactory`）注入之。
pub trait EcBackendFactory: Send + Sync {
    /// 按偏好创建一个后端实例；失败返回领域错误。
    fn create(&self, pref: BackendPreference) -> Result<Box<dyn EcBackend>, EcError>;
    /// 创建一个兜底空后端（GUI 仍能启动并展示错误）。
    fn null_backend(&self) -> Box<dyn EcBackend>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_dll_load() {
        let err = EcError::DllLoad("无法加载 DLL".into());
        assert_eq!(err.to_string(), "WinRing0 DLL 加载失败: 无法加载 DLL");
    }

    #[test]
    fn test_display_init_failed() {
        let err = EcError::InitFailed("拒绝访问 (0x5)".into());
        assert_eq!(err.to_string(), "WinRing0 初始化失败: 拒绝访问 (0x5)");
    }

    #[test]
    fn test_display_wmi_connect() {
        let err = EcError::WmiConnect("拒绝访问".into());
        assert_eq!(err.to_string(), "WMI 连接失败: 拒绝访问");
    }

    #[test]
    fn test_display_wmi_interface_not_found() {
        let err = EcError::WmiInterfaceNotFound;
        assert_eq!(err.to_string(), "WMI MICommonInterface 未找到");
    }

    #[test]
    fn test_display_wmi_call_failed() {
        let err = EcError::WmiCallFailed(0x0001);
        assert_eq!(err.to_string(), "WMI MiInterface 调用失败 (状态=0x1)");
    }

    #[test]
    fn test_display_wmi_call_hrresult() {
        let err = EcError::WmiCallHResult(0x8004102F);
        assert_eq!(err.to_string(), "WMI 调用返回错误 (hr=0x8004102F)");
    }

    #[test]
    fn test_display_timeout() {
        let err = EcError::Timeout(0x66);
        assert_eq!(err.to_string(), "EC 操作超时 (地址: 0x66)");
    }

    #[test]
    fn test_display_invalid_data() {
        let err = EcError::InvalidData("充电上限寄存器值 0xff 非法".into());
        assert_eq!(
            err.to_string(),
            "硬件返回无效数据: 充电上限寄存器值 0xff 非法"
        );
    }

    #[test]
    fn test_display_backend_unavailable() {
        let err = EcError::BackendUnavailable("两个后端均不可用".into());
        assert_eq!(err.to_string(), "后端不可用: 两个后端均不可用");
    }

    #[test]
    fn test_error_trait_impl() {
        fn assert_error<E: std::error::Error>() {}
        assert_error::<EcError>();
    }

    #[test]
    fn test_debug_impl() {
        let err = EcError::WmiCallFailed(0x0001);
        let debug = format!("{:?}", err);
        assert!(debug.contains("WmiCallFailed"));
    }

    #[test]
    fn test_source_returns_none() {
        use std::error::Error;
        let err = EcError::InitFailed("error".into());
        assert!(err.source().is_none());
        let err = EcError::DllLoad("foo".into());
        assert!(err.source().is_none());
    }

    #[test]
    fn test_ec_backend_trait_object_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Box<dyn EcBackend>>();
    }

    #[test]
    fn test_ec_backend_factory_trait_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<&dyn EcBackendFactory>();
    }
}
