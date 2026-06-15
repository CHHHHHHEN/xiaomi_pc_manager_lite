//! BatteryCare 状态与充电限制逻辑

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
