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
            // WinRing0 first: more reliable EC access. Fall back to WMI when
            // the driver can't be loaded (no admin, HVCI, etc.).
            let wr0_err = match super::winring0::WinRing0Backend::new() {
                Ok(b) => return Ok(Box::new(b)),
                Err(e) => e,
            };
            let wmi_err = match super::wmi::WmiBackend::new() {
                Ok(b) => return Ok(Box::new(b)),
                Err(e) => e,
            };
            Err(EcError::BackendUnavailable(format!(
                "WinRing0: {}; WMI: {}",
                wr0_err, wmi_err
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
