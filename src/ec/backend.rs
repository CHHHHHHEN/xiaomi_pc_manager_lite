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
}

pub fn create_backend(pref: BackendPreference) -> Result<Box<dyn EcBackend>, EcError> {
    match pref {
        BackendPreference::Wmi => try_wmi(),
        BackendPreference::WinRing0 => try_winring0(),
        BackendPreference::Auto => {
            // WinRing0 first: more reliable EC access. Fall back to WMI when
            // the driver can't be loaded (no admin, HVCI, etc.).
            let wr0_err = match try_winring0() {
                Ok(b) => return Ok(b),
                Err(e) => e,
            };
            let wmi_err = match try_wmi() {
                Ok(b) => return Ok(b),
                Err(e) => e,
            };
            Err(EcError::BackendUnavailable(format!(
                "WinRing0: {}; WMI: {}",
                wr0_err, wmi_err
            )))
        }
    }
}

fn try_wmi() -> Result<Box<dyn EcBackend>, EcError> {
    Ok(Box::new(super::wmi::WmiBackend::new()?))
}

fn try_winring0() -> Result<Box<dyn EcBackend>, EcError> {
    Ok(Box::new(super::winring0::WinRing0Backend::new()?))
}

/// A null backend that always returns `BackendUnavailable`.
/// Used when no real backend can be created, so the GUI still starts
/// and displays the error instead of crashing.
pub struct NullBackend;

impl EcBackend for NullBackend {
    fn name(&self) -> &'static str { "无后端" }

    fn read_byte(&self, addr: u16) -> Result<u8, EcError> {
        Err(EcError::BackendUnavailable(format!("read_byte({:#x}) failed: 无可用后端", addr)))
    }
    fn write_byte(&self, addr: u16, _value: u8) -> Result<(), EcError> {
        Err(EcError::BackendUnavailable(format!("write_byte({:#x}) failed: 无可用后端", addr)))
    }
    fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
        Err(EcError::BackendUnavailable("无可用后端".into()))
    }
    fn get_charge_limit(&self) -> Result<u8, EcError> {
        Err(EcError::BackendUnavailable("无可用后端".into()))
    }
    fn set_battery_care(&self, _enabled: bool) -> Result<(), EcError> {
        Err(EcError::BackendUnavailable("无可用后端".into()))
    }
    fn set_charge_limit(&self, _percent: u8) -> Result<(), EcError> {
        Err(EcError::BackendUnavailable("无可用后端".into()))
    }
    fn get_performance_mode(&self) -> Result<u8, EcError> {
        Err(EcError::BackendUnavailable("无可用后端".into()))
    }
    fn set_performance_mode(&self, _mode: u8) -> Result<(), EcError> {
        Err(EcError::BackendUnavailable("无可用后端".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_null_backend_name() {
        let backend = NullBackend;
        assert_eq!(backend.name(), "无后端");
    }

    #[test]
    fn test_null_backend_read_byte_returns_error() {
        let backend = NullBackend;
        let result = backend.read_byte(0x68);
        assert!(result.is_err());
        match result {
            Err(EcError::BackendUnavailable(msg)) => {
                assert!(msg.contains("read_byte"));
            }
            _ => panic!("Expected BackendUnavailable"),
        }
    }

    #[test]
    fn test_null_backend_write_byte_returns_error() {
        let backend = NullBackend;
        let result = backend.write_byte(0x68, 0x09);
        assert!(result.is_err());
        match result {
            Err(EcError::BackendUnavailable(msg)) => {
                assert!(msg.contains("write_byte"));
            }
            _ => panic!("Expected BackendUnavailable"),
        }
    }

    #[test]
    fn test_null_backend_get_battery_care_enabled_returns_error() {
        let backend = NullBackend;
        let result = backend.get_battery_care_enabled();
        assert!(result.is_err());
        match result {
            Err(EcError::BackendUnavailable(_)) => {}
            _ => panic!("Expected BackendUnavailable"),
        }
    }

    #[test]
    fn test_null_backend_get_charge_limit_returns_error() {
        let backend = NullBackend;
        let result = backend.get_charge_limit();
        assert!(result.is_err());
        match result {
            Err(EcError::BackendUnavailable(_)) => {}
            _ => panic!("Expected BackendUnavailable"),
        }
    }

    #[test]
    fn test_null_backend_set_battery_care_returns_error() {
        let backend = NullBackend;
        let result = backend.set_battery_care(true);
        assert!(result.is_err());
    }

    #[test]
    fn test_null_backend_set_charge_limit_returns_error() {
        let backend = NullBackend;
        let result = backend.set_charge_limit(80);
        assert!(result.is_err());
    }

    #[test]
    fn test_null_backend_get_performance_mode_returns_error() {
        let backend = NullBackend;
        let result = backend.get_performance_mode();
        assert!(result.is_err());
    }

    #[test]
    fn test_null_backend_set_performance_mode_returns_error() {
        let backend = NullBackend;
        let result = backend.set_performance_mode(0x09);
        assert!(result.is_err());
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
