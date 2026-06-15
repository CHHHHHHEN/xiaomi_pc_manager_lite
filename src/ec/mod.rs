pub mod backend;
pub mod battery;
pub mod config;
pub mod error;
pub mod performance;

pub mod winring0;
pub mod wmi;

/// EC register addresses used across backends
pub mod addr {
    /// Performance mode register
    pub const PERF_MODE: u16 = 0x68;
    /// Battery care enabled/disabled register
    pub const BATTERY_CARE: u16 = 0xA4;
    /// Battery charge limit register
    pub const CHARGE_LIMIT: u16 = 0xA7;
}

#[cfg(test)]
mod tests {
    use super::addr;

    #[test]
    fn test_perf_mode_addr() {
        assert_eq!(addr::PERF_MODE, 0x68);
    }

    #[test]
    fn test_battery_care_addr() {
        assert_eq!(addr::BATTERY_CARE, 0xA4);
    }

    #[test]
    fn test_charge_limit_addr() {
        assert_eq!(addr::CHARGE_LIMIT, 0xA7);
    }
}
