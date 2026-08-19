//! 充电上限领域策略：默认值、矛盾兜底值与自洽规则。
//!
//! 这些常量/规则同时被 `app::config`（配置消毒、默认值）与 `app::battery`
//! （硬件写入前兜底）引用。历史实现把常量定义在 `battery.rs`，而
//! `battery.rs` 又依赖 `config::AppConfig`——形成 `config ↔ battery`
//! 的模块级循环依赖。收敛到独立模块后两侧都只依赖本模块，依赖方向单一。

/// 配置中充电上限的默认值（也是 GUI 滑块/后端预设的常见起点）。
pub const DEFAULT_CHARGE_LIMIT: u8 = 80;

/// 充电上限的**满充**值：`limit == FULL_CHARGE_LIMIT` 即"不限制充电"（养护
/// 关闭），`limit < FULL_CHARGE_LIMIT` 即养护开启。该语义散落在 battery.rs
/// 的 `care_enabled_from_limit`/`apply_battery_state`、limits.rs 的自洽规则、
/// config.rs 消毒与 mock/winring0 的读回校验各自书写 `100`——收敛到此处后，
/// 任何阈值调整只改这一处，全部路径同时生效。
pub const FULL_CHARGE_LIMIT: u8 = 100;

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
    if enabled && (limit == 0 || limit >= FULL_CHARGE_LIMIT) {
        FALLBACK_CARE_LIMIT
    } else {
        limit
    }
}

/// 充电上限**读回值**的非法判定：`0` 或 `> FULL_CHARGE_LIMIT` 为垃圾值
/// （修订 1.50 收敛）。
///
/// 该谓词此前在 winring0.rs（`get_charge_limit`）、mock.rs（
/// `validate_read_raw`）与 battery.rs（`apply_battery_state` 的读回闭包）
/// 各手写一份 `raw == 0 || raw > FULL_CHARGE_LIMIT`——真实后端在各自校验，
/// 领域层在纵深防御校验，三处语义必须一致（曾有一次"一侧漏掉 0"被修复）。
/// 收敛到领域层后，任一地方改阈值/改规则都会同步三处。
///
/// 语义：`0` 不可能合法（GUI 滑块下限 40、WMI 预设下限 40、配置消毒把 0
/// 归一为默认），`> 100` 是损坏的寄存器值（如 0xFF=255）；两者都不得冒充
/// 有效状态展示/持久化。
pub fn charge_limit_readback_is_invalid(raw: u8) -> bool {
    raw == 0 || raw > FULL_CHARGE_LIMIT
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

    /// 读回非法判定（修订 1.50 收敛到领域层）：0 与 >100 是垃圾值，其余合法。
    #[test]
    fn test_charge_limit_readback_is_invalid() {
        assert!(charge_limit_readback_is_invalid(0), "0 is garbage");
        assert!(charge_limit_readback_is_invalid(101), "101 > 100");
        assert!(charge_limit_readback_is_invalid(u8::MAX), "255 is garbage");
        assert!(!charge_limit_readback_is_invalid(1));
        assert!(!charge_limit_readback_is_invalid(40));
        assert!(!charge_limit_readback_is_invalid(80));
        assert!(!charge_limit_readback_is_invalid(FULL_CHARGE_LIMIT));
    }
}
