//! 充电上限领域策略：默认值、矛盾兜底值与自洽规则。
//!
//! 这些常量/规则同时被 `ec::config`（配置消毒、默认值）与 `ec::battery`
//! （硬件写入前兜底）引用。历史实现把常量定义在 `battery.rs`，而
//! `battery.rs` 又依赖 `config::AppConfig`——形成 `config ↔ battery`
//! 的模块级循环依赖。收敛到独立模块后两侧都只依赖本模块，依赖方向单一。

/// 配置中充电上限的默认值（也是 GUI 滑块/后端预设的常见起点）。
pub const DEFAULT_CHARGE_LIMIT: u8 = 80;

/// 电池养护开启时上限被清为 100%（矛盾组合）时的统一兜底值。
pub const FALLBACK_CARE_LIMIT: u8 = DEFAULT_CHARGE_LIMIT;

/// 电池养护开启时充电上限的自洽规则：养护开启但上限无效（`0`，未设置/垃圾值）
/// 或 ≥100%（矛盾组合）时兜底为 `FALLBACK_CARE_LIMIT`，其余情况原样返回。
///
/// 该规则曾在多个模块各自实现过，存在漂移风险——统一收敛到此处后，任何一处
/// 修改规则都会同时作用于全部路径。`enabled == false` 时返回原值。
///
/// `0` 必须纳入兜底：两个后端各自的读回都把 `0` 判为非法（winring0 的
/// get_charge_limit、WMI 的 raw code 映射），写入 `0` 会落到"WinRing0 写
/// 0x00、WMI 就近映射成 40%"的静默写寄存器兜底，与读回契约不一致。
pub fn coherent_charge_limit(enabled: bool, limit: u8) -> u8 {
    if enabled && (limit == 0 || limit >= 100) {
        FALLBACK_CARE_LIMIT
    } else {
        limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 养护开启 + 上限 ≥100 或 =0（矛盾/无效组合）：兜底 80。
    #[test]
    fn test_coherent_charge_limit_care_on_incoherent() {
        assert_eq!(coherent_charge_limit(true, 100), 80);
        assert_eq!(coherent_charge_limit(true, 200), 80);
        assert_eq!(coherent_charge_limit(true, 0), 80);
    }

    /// 养护开启 + 上限 <100：原样返回。
    #[test]
    fn test_coherent_charge_limit_care_on_valid() {
        assert_eq!(coherent_charge_limit(true, 60), 60);
        assert_eq!(coherent_charge_limit(true, 80), 80);
        assert_eq!(coherent_charge_limit(true, 99), 99);
    }

    /// 养护关闭：任何上限都原样返回（100% 上限是合法组合）。
    #[test]
    fn test_coherent_charge_limit_care_off() {
        assert_eq!(coherent_charge_limit(false, 100), 100);
        assert_eq!(coherent_charge_limit(false, 80), 80);
        assert_eq!(coherent_charge_limit(false, 0), 0);
    }

    /// 幂等：多次应用规则结果稳定。
    #[test]
    fn test_coherent_charge_limit_idempotent() {
        for (enabled, limit) in [
            (true, 100u8),
            (true, 80),
            (false, 100),
            (true, 200),
            (true, 0),
        ] {
            let once = coherent_charge_limit(enabled, limit);
            let twice = coherent_charge_limit(enabled, once);
            assert_eq!(once, twice, "coherent_charge_limit must be idempotent");
        }
    }

    /// 兜底值必须与默认值一致（规则收敛后仍是 80）。
    #[test]
    fn test_fallback_equals_default() {
        assert_eq!(FALLBACK_CARE_LIMIT, DEFAULT_CHARGE_LIMIT);
        assert_eq!(DEFAULT_CHARGE_LIMIT, 80);
    }
}
