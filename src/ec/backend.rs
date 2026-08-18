use super::config::BackendPreference;
use super::error::EcError;

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
                crate::ec::performance::PerfMode::from_ec_value(perf).is_some(),
                "[{}] invalid perf raw code {:#x}",
                label,
                perf
            );
            assert!(limit <= 100, "[{}] invalid charge limit {}", label, limit);
            assert_eq!(
                care,
                crate::ec::battery::care_enabled_from_limit(limit),
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
