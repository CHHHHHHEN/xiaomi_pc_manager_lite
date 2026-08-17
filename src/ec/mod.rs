pub mod backend;
pub mod battery;
pub mod config;
pub mod error;
pub mod fnkey;
pub mod limits;
pub mod performance;
pub mod wmi_util;

pub mod winring0;
pub mod wmi;

/// 共享的内存测试后端（仅测试编译时存在）。
#[cfg(test)]
pub mod mock;

/// EC register addresses used across backends
pub mod addr {
    /// Performance mode register
    pub const PERF_MODE: u16 = 0x68;
    /// Battery care enabled/disabled register
    pub const BATTERY_CARE: u16 = 0xA4;
    /// Battery charge limit register
    pub const CHARGE_LIMIT: u16 = 0xA7;
    /// EC command port (I/O 0x66)
    pub const EC_CMD: u16 = 0x66;
    /// EC data port (I/O 0x62)
    pub const EC_DATA: u16 = 0x62;
}
