//! BatteryCare 状态与充电限制逻辑

use super::backend::EcBackend;
use super::error::EcError;

/// WMI rawCode ⇔ 充电限制百分比映射
/// WMI 仅支持预设值，WinRing0 支持 0-100 连续值
pub const WMI_CHARGE_LIMITS: &[(u8, u8)] = &[
    (0, 100),
    (1, 80),
    (4, 90),
    (5, 70),
    (6, 60),
    (7, 50),
    (8, 40),
];

pub fn wmi_rawcode_to_percent(rawcode: u8) -> Option<u8> {
    WMI_CHARGE_LIMITS.iter().find(|(r, _)| *r == rawcode).map(|(_, p)| *p)
}

pub fn percent_to_wmi_rawcode(percent: u8) -> Option<u8> {
    WMI_CHARGE_LIMITS.iter().find(|(_, p)| *p == percent).map(|(r, _)| *r)
}

/// 找到最接近的 WMI 预设值
pub fn nearest_wmi_percent(percent: u8) -> u8 {
    WMI_CHARGE_LIMITS
        .iter()
        .map(|(_, p)| *p)
        .min_by_key(|p| (*p as i16 - percent as i16).abs())
        .expect("WMI_CHARGE_LIMITS is a non-empty compile-time constant")
}

/// 电池养护开启时充电上限的自洽规则：养护开启但上限 ≥100%（矛盾组合）时
/// 兜底为 80%，其余情况原样返回。
///
/// 该规则在配置消毒（config.rs）、启动应用（main.rs）、电源重设与 GUI 切换
/// （gui/commands.rs）四处各自实现过，存在漂移风险——统一收敛到此处后，
/// 任何一处修改规则都会同时作用于全部路径。`enabled == false` 时返回原值。
pub fn coherent_charge_limit(enabled: bool, limit: u8) -> u8 {
    if enabled && limit >= 100 {
        80
    } else {
        limit
    }
}

/// 一次写入"充电上限 + 养护位"的结果。
pub struct BatteryApplyOutcome {
    /// 限值写入结果：`Ok(applied)` 为写入成功后读回的硬件实际生效值
    /// （已钳制 ≤100）；`Err` 表示限值写入失败（失败时不尝试读回）。
    pub charge_limit: Result<u8, EcError>,
    /// 养护位写入结果。
    pub care: Result<(), EcError>,
}

/// 统一"写充电上限 → 写养护位 → 读回"的序列与兜底规则。
///
/// 该序列曾四处各自实现（main.rs 启动应用、gui/commands.rs 的
/// set_battery_care_internal / set_charge_limit_internal / ReapplyConfig），
/// 存在漂移风险——统一收敛到此处后，任何一处修改规则都会同时作用于全部路径。
///
/// 约定：
/// - 先写限值：部分 EC 固件会从限值寄存器自动同步养护位；
/// - 养护开启时限值先经 `coherent_charge_limit` 兜底（≥100 → 80），
///   关闭时上限为 100%；
/// - 限值写入成功后读回硬件实际生效值（WMI 会把非预设值量化到最近预设，
///   如 85→80），由调用方决定是否回写持久化配置。
pub fn apply_battery_state(
    backend: &dyn EcBackend,
    care: bool,
    desired_limit: u8,
) -> BatteryApplyOutcome {
    let limit = if care {
        coherent_charge_limit(true, desired_limit)
    } else {
        100
    };
    let charge_limit = match backend.set_charge_limit(limit) {
        Ok(()) => {
            log::info!("Charge limit set to {}%", limit);
            Ok(backend.get_charge_limit().unwrap_or(limit).min(100))
        }
        Err(e) => Err(e),
    };
    let care = match backend.set_battery_care(care) {
        Ok(()) => {
            log::info!(
                "Battery care set to {}",
                if care { "enabled" } else { "disabled" }
            );
            Ok(())
        }
        Err(e) => Err(e),
    };
    BatteryApplyOutcome { charge_limit, care }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// 记录写入的模拟后端：可选在写限值时量化到最近 WMI 预设（模拟 WMI），
    /// 可选拒绝限值写入（模拟固件拒绝）。
    #[derive(Default)]
    struct MemoryBatteryBackend {
        charge_limit: std::sync::atomic::AtomicU8,
        care: std::sync::atomic::AtomicBool,
        quantize: bool,
        set_limit_fails: bool,
    }

    impl MemoryBatteryBackend {
        fn quantizing() -> Self {
            Self {
                charge_limit: std::sync::atomic::AtomicU8::new(100),
                care: std::sync::atomic::AtomicBool::new(false),
                quantize: true,
                set_limit_fails: false,
            }
        }
    }

    impl EcBackend for MemoryBatteryBackend {
        fn name(&self) -> &'static str {
            "memory"
        }
        fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
            Ok(self.care.load(Ordering::Relaxed))
        }
        fn get_charge_limit(&self) -> Result<u8, EcError> {
            Ok(self.charge_limit.load(Ordering::Relaxed))
        }
        fn set_battery_care(&self, enabled: bool) -> Result<(), EcError> {
            self.care.store(enabled, Ordering::Relaxed);
            Ok(())
        }
        fn set_charge_limit(&self, percent: u8) -> Result<(), EcError> {
            if self.set_limit_fails {
                return Err(EcError::BackendUnavailable("denied".into()));
            }
            let pct = if self.quantize {
                nearest_wmi_percent(percent.min(100))
            } else {
                percent.min(100)
            };
            self.charge_limit.store(pct, Ordering::Relaxed);
            Ok(())
        }
        fn get_performance_mode(&self) -> Result<u8, EcError> {
            Ok(0x09)
        }
        fn set_performance_mode(&self, _mode: u8) -> Result<(), EcError> {
            Ok(())
        }
    }

    /// 养护开启 + 上限 100%（矛盾组合）：必须兜底写 80% 并读回。
    #[test]
    fn test_apply_battery_state_care_on_incoherent_limit_uses_80() {
        let backend = MemoryBatteryBackend::default();
        let outcome = apply_battery_state(&backend, true, 100);
        assert!(matches!(outcome.charge_limit, Ok(80)));
        assert!(outcome.care.is_ok());
        assert_eq!(backend.charge_limit.load(Ordering::Relaxed), 80);
        assert!(backend.care.load(Ordering::Relaxed));
    }

    /// 养护关闭：上限写 100%，但保留 desired_limit（读回值即 100）。
    #[test]
    fn test_apply_battery_state_disable_writes_100() {
        let backend = MemoryBatteryBackend::default();
        let outcome = apply_battery_state(&backend, false, 60);
        assert!(matches!(outcome.charge_limit, Ok(100)));
        assert_eq!(backend.charge_limit.load(Ordering::Relaxed), 100);
        assert!(!backend.care.load(Ordering::Relaxed));
    }

    /// WMI 量化：请求 85%，读回硬件实际生效的 80%。
    #[test]
    fn test_apply_battery_state_reads_back_quantized_value() {
        let backend = MemoryBatteryBackend::quantizing();
        let outcome = apply_battery_state(&backend, true, 85);
        assert!(matches!(outcome.charge_limit, Ok(80)));
        assert_eq!(backend.charge_limit.load(Ordering::Relaxed), 80);
    }

    /// 限值写入失败：charge_limit 为 Err，不尝试读回，但养护位仍会写入。
    #[test]
    fn test_apply_battery_state_limit_failure_returns_err() {
        let mut backend = MemoryBatteryBackend::default();
        backend.set_limit_fails = true;
        let outcome = apply_battery_state(&backend, true, 80);
        assert!(outcome.charge_limit.is_err());
        assert!(outcome.care.is_ok());
    }

    #[test]
    fn test_wmi_rawcode_to_percent_valid() {
        assert_eq!(wmi_rawcode_to_percent(0), Some(100));
        assert_eq!(wmi_rawcode_to_percent(1), Some(80));
        assert_eq!(wmi_rawcode_to_percent(4), Some(90));
        assert_eq!(wmi_rawcode_to_percent(5), Some(70));
        assert_eq!(wmi_rawcode_to_percent(6), Some(60));
        assert_eq!(wmi_rawcode_to_percent(7), Some(50));
        assert_eq!(wmi_rawcode_to_percent(8), Some(40));
    }

    #[test]
    fn test_wmi_rawcode_to_percent_invalid() {
        assert_eq!(wmi_rawcode_to_percent(2), None);
        assert_eq!(wmi_rawcode_to_percent(3), None);
        assert_eq!(wmi_rawcode_to_percent(9), None);
        assert_eq!(wmi_rawcode_to_percent(10), None);
        assert_eq!(wmi_rawcode_to_percent(0xFF), None);
    }

    #[test]
    fn test_percent_to_wmi_rawcode_valid() {
        assert_eq!(percent_to_wmi_rawcode(100), Some(0));
        assert_eq!(percent_to_wmi_rawcode(80), Some(1));
        assert_eq!(percent_to_wmi_rawcode(90), Some(4));
        assert_eq!(percent_to_wmi_rawcode(70), Some(5));
        assert_eq!(percent_to_wmi_rawcode(60), Some(6));
        assert_eq!(percent_to_wmi_rawcode(50), Some(7));
        assert_eq!(percent_to_wmi_rawcode(40), Some(8));
    }

    #[test]
    fn test_percent_to_wmi_rawcode_invalid() {
        assert_eq!(percent_to_wmi_rawcode(0), None);
        assert_eq!(percent_to_wmi_rawcode(10), None);
        assert_eq!(percent_to_wmi_rawcode(30), None);
        assert_eq!(percent_to_wmi_rawcode(55), None);
        assert_eq!(percent_to_wmi_rawcode(85), None);
        assert_eq!(percent_to_wmi_rawcode(95), None);
        assert_eq!(percent_to_wmi_rawcode(100), Some(0));
    }

    #[test]
    fn test_nearest_wmi_percent_exact() {
        assert_eq!(nearest_wmi_percent(40), 40);
        assert_eq!(nearest_wmi_percent(50), 50);
        assert_eq!(nearest_wmi_percent(60), 60);
        assert_eq!(nearest_wmi_percent(70), 70);
        assert_eq!(nearest_wmi_percent(80), 80);
        assert_eq!(nearest_wmi_percent(90), 90);
        assert_eq!(nearest_wmi_percent(100), 100);
    }

    #[test]
    fn test_nearest_wmi_percent_rounding() {
        assert_eq!(nearest_wmi_percent(85), 80);
        assert_eq!(nearest_wmi_percent(84), 80);
        assert_eq!(nearest_wmi_percent(86), 90);
        assert_eq!(nearest_wmi_percent(45), 50);
        assert_eq!(nearest_wmi_percent(55), 60);
        assert_eq!(nearest_wmi_percent(65), 70);
        assert_eq!(nearest_wmi_percent(75), 80);
        assert_eq!(nearest_wmi_percent(95), 100);
    }

    #[test]
    fn test_nearest_wmi_percent_boundary() {
        assert_eq!(nearest_wmi_percent(0), 40);
        assert_eq!(nearest_wmi_percent(200), 100);
    }

    #[test]
    fn test_wmi_charge_limits_table_completeness() {
        assert_eq!(WMI_CHARGE_LIMITS.len(), 7);
        let codes: std::collections::HashSet<u8> = WMI_CHARGE_LIMITS.iter().map(|(r, _)| *r).collect();
        assert_eq!(codes.len(), 7);
        let percents: std::collections::HashSet<u8> = WMI_CHARGE_LIMITS.iter().map(|(_, p)| *p).collect();
        assert_eq!(percents.len(), 7);
    }

    #[test]
    fn test_wmi_rawcode_to_percent_bidirectional() {
        for (rawcode, percent) in WMI_CHARGE_LIMITS {
            assert_eq!(percent_to_wmi_rawcode(*percent), Some(*rawcode));
            assert_eq!(wmi_rawcode_to_percent(*rawcode), Some(*percent));
        }
    }

    /// 养护开启 + 上限 ≥100（矛盾组合）：兜底 80。
    #[test]
    fn test_coherent_charge_limit_care_on_incoherent() {
        assert_eq!(coherent_charge_limit(true, 100), 80);
        assert_eq!(coherent_charge_limit(true, 200), 80);
    }

    /// 养护开启 + 上限 <100：原样返回。
    #[test]
    fn test_coherent_charge_limit_care_on_valid() {
        assert_eq!(coherent_charge_limit(true, 60), 60);
        assert_eq!(coherent_charge_limit(true, 80), 80);
        assert_eq!(coherent_charge_limit(true, 99), 99);
    }

    /// 养护关闭：任何上限都原样返回（100% 上限是合法组合）。
    #[test]
    fn test_coherent_charge_limit_care_off() {
        assert_eq!(coherent_charge_limit(false, 100), 100);
        assert_eq!(coherent_charge_limit(false, 80), 80);
        assert_eq!(coherent_charge_limit(false, 0), 0);
    }

    /// 幂等：多次应用规则结果稳定。
    #[test]
    fn test_coherent_charge_limit_idempotent() {
        for (enabled, limit) in [(true, 100u8), (true, 80), (false, 100), (true, 200)] {
            let once = coherent_charge_limit(enabled, limit);
            let twice = coherent_charge_limit(enabled, once);
            assert_eq!(once, twice, "coherent_charge_limit must be idempotent");
        }
    }
}
