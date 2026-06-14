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
            Self::Eco => "Eco",
            Self::Quiet => "Quiet",
            Self::Smart => "Smart",
            Self::Fast => "Fast",
            Self::Extreme => "Extreme",
        }
    }

    pub fn is_valid(val: u8) -> bool {
        matches!(val, 0x0A | 0x02 | 0x09 | 0x03 | 0x04)
    }

    pub fn all() -> &'static [PerfMode] {
        &[Self::Eco, Self::Quiet, Self::Smart, Self::Fast, Self::Extreme]
    }
}
