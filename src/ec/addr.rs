//! EC 寄存器地址常量（跨后端共享）。

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
