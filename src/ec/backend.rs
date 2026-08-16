use super::config::BackendPreference;
use super::error::EcError;

pub trait EcBackend: Send + Sync {
    fn name(&self) -> &'static str;

    /// Low-level EC byte access (port I/O for WinRing0)
    fn read_byte(&self, addr: u16) -> Result<u8, EcError>;
    fn write_byte(&self, addr: u16, value: u8) -> Result<(), EcError>;

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

macro_rules! null_err {
    () => {
        Err(EcError::BackendUnavailable("无可用后端".into()))
    };
}

impl EcBackend for NullBackend {
    fn name(&self) -> &'static str { "无后端" }
    fn read_byte(&self, _addr: u16) -> Result<u8, EcError> { null_err!() }
    fn write_byte(&self, _addr: u16, _value: u8) -> Result<(), EcError> { null_err!() }
    fn get_battery_care_enabled(&self) -> Result<bool, EcError> { null_err!() }
    fn get_charge_limit(&self) -> Result<u8, EcError> { null_err!() }
    fn set_battery_care(&self, _enabled: bool) -> Result<(), EcError> { null_err!() }
    fn set_charge_limit(&self, _percent: u8) -> Result<(), EcError> { null_err!() }
    fn get_performance_mode(&self) -> Result<u8, EcError> { null_err!() }
    fn set_performance_mode(&self, _mode: u8) -> Result<(), EcError> { null_err!() }
    fn supports_continuous_charge_limit(&self) -> bool { false }
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
        assert!(backend.read_byte(0x68).is_err());
        assert!(backend.write_byte(0x68, 0x09).is_err());
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
    fn test_ec_backend_trait_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NullBackend>();
    }

    #[test]
    fn test_ec_backend_trait_object_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Box<dyn EcBackend>>();
    }
}
