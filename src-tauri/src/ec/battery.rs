/// BatteryCare 状态与充电限制逻辑

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
        .unwrap_or(80)
}

#[derive(Debug, Clone, Copy)]
pub struct BatteryStatus {
    pub enabled: bool,
    pub charge_limit: u8,
}
