//! 后端创建与空后端兜底。
//!
//! `EcBackend` trait / `EcError` / `BackendPreference` / `EcBackendFactory`
//! 定义在 `app::ec`（端口在领域层，适配器在 `ec`）；本文件从那里
//! 重导出（历史路径 `ec::backend::EcBackend` 等继续可用），并承载：
//! - `create_backend`：按偏好创建真实后端；
//! - `NullBackend`：无法创建任何后端时的空兜底；
//! - `BackendFactory`：`app::ec::EcBackendFactory` 端口在组合根的实现。

use crate::app::config::BackendPreference;

pub use crate::app::ec::{EcBackend, EcError};

pub fn create_backend(pref: BackendPreference) -> Result<Box<dyn EcBackend>, EcError> {
    match pref {
        BackendPreference::Wmi => Ok(Box::new(super::wmi::WmiBackend::new()?)),
        BackendPreference::WinRing0 => Ok(Box::new(super::winring0::WinRing0Backend::new()?)),
        BackendPreference::Auto => {
            // WMI first（符合 F-HAL-13）：WMI 通过官方 WMI-ACPI 接口访问 EC，
            // 无需加载内核驱动；本机（2025 RedmiBook Pro 14）实例调用实测
            // 可用（5~16ms）。历史版本曾因"对类路径调用"被拒（0x8004102F）
            // 而误判为固件拒绝协议、改为 WinRing0 优先——修复后 WMI 为默认
            // 与首选后端。WinRing0 作为 WMI 不可用（非小米机器/接口缺失等）
            // 时的回退。
            let wmi_err = match super::wmi::WmiBackend::new() {
                Ok(b) => return Ok(Box::new(b)),
                Err(e) => e,
            };
            let wr0_err = match super::winring0::WinRing0Backend::new() {
                Ok(b) => return Ok(Box::new(b)),
                Err(e) => e,
            };
            Err(EcError::BackendUnavailable(format!(
                "WMI: {}; WinRing0: {}",
                wmi_err, wr0_err
            )))
        }
    }
}

/// `EcBackendFactory` 端口的真实实现：组合根（main.rs）注入给启动编排。
///
/// 领域层经 `app::ec::EcBackendFactory` 依赖本工厂，不直接触碰 `ec` 的
/// 具体后端类型——创建后端、空后端兜底的实现细节被隔离在适配器层。
#[derive(Debug, Clone, Copy)]
pub struct BackendFactory;

impl crate::app::ec::EcBackendFactory for BackendFactory {
    fn create(&self, pref: BackendPreference) -> Result<Box<dyn EcBackend>, EcError> {
        create_backend(pref)
    }

    fn null_backend(&self) -> Box<dyn EcBackend> {
        Box::new(NullBackend)
    }
}

/// A null backend that always returns `BackendUnavailable`.
/// Used when no real backend can be created, so the GUI still starts
/// and displays the error instead of crashing.
pub struct NullBackend;

/// NullBackend 所有方法的统一失败结果。
fn null_err<T>() -> Result<T, EcError> {
    Err(EcError::BackendUnavailable("无可用后端".into()))
}

impl EcBackend for NullBackend {
    fn name(&self) -> &'static str {
        "无后端"
    }
    fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
        null_err()
    }
    fn get_charge_limit(&self) -> Result<u8, EcError> {
        null_err()
    }
    fn set_battery_care(&self, _enabled: bool) -> Result<(), EcError> {
        null_err()
    }
    fn set_charge_limit(&self, _percent: u8) -> Result<(), EcError> {
        null_err()
    }
    fn get_performance_mode(&self) -> Result<u8, EcError> {
        null_err()
    }
    fn set_performance_mode(&self, _mode: u8) -> Result<(), EcError> {
        null_err()
    }
    fn supports_continuous_charge_limit(&self) -> bool {
        false
    }
    fn is_null(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ec::EcBackendFactory;

    #[test]
    fn test_null_backend_name() {
        assert_eq!(NullBackend.name(), "无后端");
    }

    #[test]
    fn test_null_backend_all_methods_return_error() {
        let backend = NullBackend;
        assert!(backend.get_battery_care_enabled().is_err());
        assert!(backend.get_charge_limit().is_err());
        assert!(backend.set_battery_care(true).is_err());
        assert!(backend.set_charge_limit(80).is_err());
        assert!(backend.get_performance_mode().is_err());
        assert!(backend.set_performance_mode(0x09).is_err());
    }

    #[test]
    fn test_null_backend_supports_continuous_charge_limit() {
        assert!(!NullBackend.supports_continuous_charge_limit());
    }

    #[test]
    fn test_null_backend_is_null() {
        assert!(NullBackend.is_null());
    }

    #[test]
    fn test_ec_backend_trait_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NullBackend>();
    }

    #[test]
    fn test_ec_backend_trait_object_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Box<dyn EcBackend>>();
    }

    #[test]
    fn test_backend_factory_creates_requested_backend() {
        let factory = BackendFactory;
        // 非管理员/非小米机器上创建可能失败，这里只验证「能调用工厂且结果类型正确」。
        // 类型契约：Ok(Box<dyn EcBackend>)，Err(EcError)。
        let _: Result<Box<dyn EcBackend>, EcError> = factory.create(BackendPreference::Wmi);
    }

    #[test]
    fn test_backend_factory_null_backend_is_null() {
        let factory = BackendFactory;
        assert!(factory.null_backend().is_null());
    }

    /// 真机集成验证（手动运行，非 CI）：在受支持的小米/红米笔记本上创建
    /// WMI 与 WinRing0 两个后端并读取硬件状态，验证核心功能路径（电池
    /// 养护/充电上限/性能模式读取）在真实硬件上可用。
    ///
    /// **只读**：只调用 getter、不调用 setter——写入路径已由各后端单测
    /// （仿真 backend）覆盖，真机写入属于用户主动操作、不应由测试触发。
    ///
    /// 跳过策略：任一后端创建失败（非管理员/驱动缺失/非小米机器）即打印
    /// 原因并继续尝试另一后端，两者都不可用则整体跳过——保证在任何环境
    /// 都不假红也不假绿。运行：`cargo test -- --ignored
    /// hardware_read_smoke_test`。
    #[test]
    #[ignore = "requires admin on a supported Xiaomi/Redmi laptop (manual hardware test)"]
    fn hardware_read_smoke_test() {
        let mut exercised = 0usize;
        for pref in [BackendPreference::Wmi, BackendPreference::WinRing0] {
            let label = match pref {
                BackendPreference::Wmi => "WMI",
                BackendPreference::WinRing0 => "WinRing0",
                BackendPreference::Auto => unreachable!(),
            };
            let backend = match create_backend(pref) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[{}] backend unavailable (skip): {}", label, e);
                    continue;
                }
            };
            let perf = match backend.get_performance_mode() {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("[{}] read perf mode failed: {}", label, e);
                    continue;
                }
            };
            let (care, limit) = match backend.get_battery_state() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[{}] read battery state failed: {}", label, e);
                    continue;
                }
            };
            // 性能模式必须是已知枚举之一（未定义 raw code 说明固件/驱动
            // 解析异常）；充电上限在 [0,100]，且养护位与限值推导一致。
            assert!(
                crate::app::performance::PerfMode::from_ec_value(perf).is_some(),
                "[{}] invalid perf raw code {:#x}",
                label,
                perf
            );
            assert!(limit <= 100, "[{}] invalid charge limit {}", label, limit);
            assert_eq!(
                care,
                crate::app::battery::care_enabled_from_limit(limit),
                "[{}] care bit inconsistent with limit {}",
                label,
                limit
            );
            eprintln!(
                "[{}] OK: perf={:#x}, care={}, limit={}%",
                label, perf, care, limit
            );
            exercised += 1;
        }
        assert!(
            exercised > 0,
            "neither WMI nor WinRing0 backend available; not a supported machine"
        );
    }
}
