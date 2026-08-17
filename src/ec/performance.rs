/// Performance mode 枚举与 EC 值映射
///
/// 纯领域模块：不依赖平台 API（电源状态查询在 `platform::power`）。

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    /// 模式对应的 EC raw code。
    ///
    /// 业务代码应统一经此访问数值（`from_ec_value` 的反向），避免散落的
    /// `as u8` 隐式依赖枚举判别值布局；测试中的 `as u8` 是有意锁定协议值，
    /// 保留不变。
    pub fn ec_value(&self) -> u8 {
        *self as u8
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

    /// 由 raw code 解析出模式名；未知值（硬件读回的未定义代码）显示"未知"。
    pub fn name_or_unknown(raw: u8) -> &'static str {
        Self::from_ec_value(raw).map(|m| m.name()).unwrap_or("未知")
    }

    /// 模式的一句话描述（GUI 悬停提示用）。
    pub fn description(&self) -> &'static str {
        match self {
            Self::Eco => "最低功耗，最大限度延长电池续航",
            Self::Quiet => "风扇静音优先，适合轻度办公",
            Self::Smart => "根据负载自动调节性能与散热（推荐）",
            Self::Fast => "高性能优先，风扇响应更积极",
            Self::Extreme => "最大性能释放，需要交流电源",
        }
    }

    pub fn all() -> &'static [PerfMode] {
        &[
            Self::Eco,
            Self::Quiet,
            Self::Smart,
            Self::Fast,
            Self::Extreme,
        ]
    }
}

/// 根据电源状态返回实际应写入 EC 的 raw code：
/// 狂暴模式（Extreme）仅在接入交流电源时生效，电池供电时降级为极速模式（Fast）；
/// 其余模式原样返回。
pub fn effective_ec_value(mode: u8, on_ac: bool) -> u8 {
    if mode == PerfMode::Extreme.ec_value() && !on_ac {
        PerfMode::Fast.ec_value()
    } else {
        mode
    }
}

/// Fn+K / 热键循环切换的性能模式序列。
///
/// 该序列曾在 gui/commands.rs 硬编码，与领域模块（性能模式语义）分离——
/// 收敛到此处后，模式增减只需改这一处，GUI 与（未来）其它输入通道同时生效。
pub const CYCLE: &[PerfMode] = &[PerfMode::Smart, PerfMode::Quiet, PerfMode::Extreme];

/// Fn+K / 热键循环切换的下一档性能模式。
///
/// 命中循环内模式时取下一项（Smart → Quiet → Extreme → Smart）；
/// 当前模式不在循环序列中（如 Eco / 未知）时回到循环首项 Smart。
pub fn next_cycle_mode(current: PerfMode) -> PerfMode {
    match CYCLE.iter().position(|m| *m == current) {
        Some(i) => CYCLE[(i + 1) % CYCLE.len()],
        None => CYCLE[0],
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
            let val = mode.ec_value();
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

    /// 每个模式的描述非空，且全部模式的描述互不相同（GUI 悬停提示不应
    /// 出现重复/空文案）。
    #[test]
    fn test_perf_mode_descriptions_nonempty_distinct() {
        let mut descs: Vec<&str> = PerfMode::all().iter().map(|m| m.description()).collect();
        assert!(
            descs.iter().all(|d| !d.is_empty()),
            "every mode must have a non-empty description"
        );
        descs.sort();
        descs.dedup();
        assert_eq!(
            descs.len(),
            PerfMode::all().len(),
            "mode descriptions must be distinct"
        );
    }

    #[test]
    fn test_effective_ec_value_guard() {
        assert_eq!(
            effective_ec_value(PerfMode::Extreme as u8, false),
            PerfMode::Fast as u8
        );
        assert_eq!(
            effective_ec_value(PerfMode::Extreme as u8, true),
            PerfMode::Extreme as u8
        );
        assert_eq!(
            effective_ec_value(PerfMode::Smart as u8, false),
            PerfMode::Smart as u8
        );
        assert_eq!(
            effective_ec_value(PerfMode::Quiet as u8, true),
            PerfMode::Quiet as u8
        );
        assert_eq!(
            effective_ec_value(PerfMode::Fast as u8, false),
            PerfMode::Fast as u8
        );
    }

    /// Fn+K / 热键循环序列：Smart → Quiet → Extreme → Smart。
    #[test]
    fn test_cycle_progresses_in_order() {
        assert_eq!(next_cycle_mode(PerfMode::Smart), PerfMode::Quiet);
        assert_eq!(next_cycle_mode(PerfMode::Quiet), PerfMode::Extreme);
        assert_eq!(next_cycle_mode(PerfMode::Extreme), PerfMode::Smart);
    }

    /// 当前模式不在循环序列中（Eco / Fast / 未知值）时，回到循环首项 Smart。
    #[test]
    fn test_cycle_unknown_mode_restarts_from_first() {
        assert_eq!(next_cycle_mode(PerfMode::Eco), PerfMode::Smart);
        assert_eq!(next_cycle_mode(PerfMode::Fast), PerfMode::Smart);
    }

    /// 循环序列必须是非空、元素唯一的合法模式（防误删/误改）。
    #[test]
    fn test_cycle_is_nonempty_distinct_valid() {
        assert!(!CYCLE.is_empty());
        let distinct: std::collections::HashSet<PerfMode> = CYCLE.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            CYCLE.len(),
            "CYCLE must not contain duplicates"
        );
        for m in CYCLE {
            assert_eq!(PerfMode::from_ec_value(m.ec_value()), Some(*m));
        }
    }
}
