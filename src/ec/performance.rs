/// Performance mode 枚举与 EC 值映射

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerfMode {
    Eco = 0x0A,
    Quiet = 0x02,
    Smart = 0x09,
    Fast = 0x03,
    Extreme = 0x04,
}

impl PerfMode {
    pub fn from_ec_value(val: u8) -> Option<Self> {
        match val {
            0x0A => Some(Self::Eco),
            0x02 => Some(Self::Quiet),
            0x09 => Some(Self::Smart),
            0x03 => Some(Self::Fast),
            0x04 => Some(Self::Extreme),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Eco => "节能",
            Self::Quiet => "静音",
            Self::Smart => "智能",
            Self::Fast => "极速",
            Self::Extreme => "狂暴",
        }
    }

    pub fn all() -> &'static [PerfMode] {
        &[Self::Eco, Self::Quiet, Self::Smart, Self::Fast, Self::Extreme]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_perf_mode_ec_values() {
        assert_eq!(PerfMode::Eco as u8, 0x0A);
        assert_eq!(PerfMode::Quiet as u8, 0x02);
        assert_eq!(PerfMode::Smart as u8, 0x09);
        assert_eq!(PerfMode::Fast as u8, 0x03);
        assert_eq!(PerfMode::Extreme as u8, 0x04);
    }

    #[test]
    fn test_from_ec_value_valid() {
        assert_eq!(PerfMode::from_ec_value(0x0A), Some(PerfMode::Eco));
        assert_eq!(PerfMode::from_ec_value(0x02), Some(PerfMode::Quiet));
        assert_eq!(PerfMode::from_ec_value(0x09), Some(PerfMode::Smart));
        assert_eq!(PerfMode::from_ec_value(0x03), Some(PerfMode::Fast));
        assert_eq!(PerfMode::from_ec_value(0x04), Some(PerfMode::Extreme));
    }

    #[test]
    fn test_from_ec_value_invalid() {
        assert_eq!(PerfMode::from_ec_value(0x00), None);
        assert_eq!(PerfMode::from_ec_value(0x01), None);
        assert_eq!(PerfMode::from_ec_value(0x05), None);
        assert_eq!(PerfMode::from_ec_value(0x06), None);
        assert_eq!(PerfMode::from_ec_value(0x07), None);
        assert_eq!(PerfMode::from_ec_value(0x08), None);
        assert_eq!(PerfMode::from_ec_value(0x0B), None);
        assert_eq!(PerfMode::from_ec_value(0xFF), None);
    }

    #[test]
    fn test_name() {
        assert_eq!(PerfMode::Eco.name(), "节能");
        assert_eq!(PerfMode::Quiet.name(), "静音");
        assert_eq!(PerfMode::Smart.name(), "智能");
        assert_eq!(PerfMode::Fast.name(), "极速");
        assert_eq!(PerfMode::Extreme.name(), "狂暴");
    }

    #[test]
    fn test_all() {
        let all = PerfMode::all();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0], PerfMode::Eco);
        assert_eq!(all[1], PerfMode::Quiet);
        assert_eq!(all[2], PerfMode::Smart);
        assert_eq!(all[3], PerfMode::Fast);
        assert_eq!(all[4], PerfMode::Extreme);
    }

    #[test]
    fn test_smart_is_default() {
        assert_eq!(PerfMode::Smart as u8, 0x09);
    }

    #[test]
    fn test_from_ec_value_roundtrip() {
        for mode in PerfMode::all() {
            let val = *mode as u8;
            assert_eq!(PerfMode::from_ec_value(val), Some(*mode));
        }
    }

    #[test]
    fn test_perf_mode_debug() {
        assert_eq!(format!("{:?}", PerfMode::Eco), "Eco");
        assert_eq!(format!("{:?}", PerfMode::Quiet), "Quiet");
        assert_eq!(format!("{:?}", PerfMode::Smart), "Smart");
        assert_eq!(format!("{:?}", PerfMode::Fast), "Fast");
        assert_eq!(format!("{:?}", PerfMode::Extreme), "Extreme");
    }
}
