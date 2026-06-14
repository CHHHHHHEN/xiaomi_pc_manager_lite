use super::config::BackendPreference;
use super::error::EcError;

pub trait EcBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn is_available(&self) -> bool;

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
        BackendPreference::Auto => try_wmi().or_else(|_| try_winring0()),
    }
}

fn try_wmi() -> Result<Box<dyn EcBackend>, EcError> {
    Ok(Box::new(super::wmi::WmiBackend::new()?))
}

fn try_winring0() -> Result<Box<dyn EcBackend>, EcError> {
    Ok(Box::new(super::winring0::WinRing0Backend::new()?))
}
