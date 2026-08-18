//! BatteryCare 状态与充电限制逻辑

use crate::app::config::AppConfig;
use crate::app::ec::{EcBackend, EcError};
use crate::app::limits::{coherent_charge_limit, FULL_CHARGE_LIMIT};

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

/// GUI 以固定档位按钮展示的 WMI 预设充电上限（升序）。
///
/// 与 `WMI_CHARGE_LIMITS` 的 percent 集合完全一致（一致性由测试锁定），
/// 但显式给出 GUI 展示顺序。历史实现把同一份档位列表硬编码在 view.rs，
/// 与映射表重复为两个事实来源——统一收敛到此处。
pub const WMI_PRESET_PERCENTS: &[u8] = &[40, 50, 60, 70, 80, 90, 100];

pub fn wmi_rawcode_to_percent(rawcode: u8) -> Option<u8> {
    WMI_CHARGE_LIMITS
        .iter()
        .find(|(r, _)| *r == rawcode)
        .map(|(_, p)| *p)
}

pub fn percent_to_wmi_rawcode(percent: u8) -> Option<u8> {
    WMI_CHARGE_LIMITS
        .iter()
        .find(|(_, p)| *p == percent)
        .map(|(r, _)| *r)
}

/// 找到最接近的 WMI 预设值
pub fn nearest_wmi_percent(percent: u8) -> u8 {
    WMI_CHARGE_LIMITS
        .iter()
        .map(|(_, p)| *p)
        .min_by_key(|p| (*p as i16 - percent as i16).abs())
        .expect("WMI_CHARGE_LIMITS is a non-empty compile-time constant")
}

/// 充电上限**写入硬件前**的统一校验（winring0 与 wmi 两个后端共用）。
///
/// - `0` 非法：读回路径都把 `0` 判为垃圾值（winring0 的 get_charge_limit、
///   WMI 的 raw code 映射均拒绝 0），写入 `0` 会被静默落成"WinRing0 写 0x00
///   寄存器、WMI 就近映射成 40%"的兜底，与读回契约不一致。写入前显式报错，
///   而不是靠写后读回失败兜底暴露问题。
/// - `>100` 钳到 100：与配置消毒、GUI 滑块上限一致。
pub fn validate_charge_limit_write(percent: u8) -> Result<u8, EcError> {
    if percent == 0 {
        return Err(EcError::InvalidData("充电上限 0% 非法".into()));
    }
    Ok(percent.min(FULL_CHARGE_LIMIT))
}

/// 一次写入"充电上限 + 养护位"的结果。
pub struct BatteryApplyOutcome {
    /// 限值写入结果：`Ok(applied)` 为写入成功后读回的硬件实际生效值
    /// （已钳制 ≤100）；`Err` 表示限值写入失败（失败时不尝试读回）。
    pub charge_limit: Result<u8, EcError>,
    /// 养护位写入结果。
    pub care: Result<(), EcError>,
}

/// 充电上限 ⇔ 电池养护的领域不变式：`limit < 100` 即养护开启。
///
/// 该判定曾散落在 wmi.rs、winring0.rs、battery.rs 与 gui/commands.rs 共
/// 7 处各自书写 `< 100`（阈值若变需同步改全部落点）——统一收敛到此处，
/// 任何修改只改这一处，全部路径同时生效。
pub fn care_enabled_from_limit(limit: u8) -> bool {
    limit < FULL_CHARGE_LIMIT
}

/// 电池养护状态的面向用户文案（唯一事实来源）。
///
/// GUI 状态区（"电池养护: 开启"）、托盘 tooltip（"养护:开启 (上限80%)"）、
/// 托盘通知（"电池养护: 开启"）曾各自书写 "开启"/"关闭"/"已启用"/"已停用"
/// 多套措辞，同一状态文案不一致——统一收敛到此处后任何展示处直接引用。
pub fn care_label(enabled: bool) -> &'static str {
    if enabled {
        "开启"
    } else {
        "关闭"
    }
}

/// 电池写入成功后的配置同步规则（限值是两种后端判定养护状态的权威依据）：
/// - `battery_care_enabled` 恒等于 `applied < 100`（养护 = 充电上限非 100%，
///   见 `care_enabled_from_limit`）；
/// - 养护开启（`applied < 100`）时把硬件实际生效值（WMI 量化后）写回
///   持久化的期望上限，使配置与硬件一致；
/// - 养护关闭（`applied == 100`）时**保留**当前配置中的期望上限，供重新
///   开启养护时恢复，不被硬件读回的 100% 覆盖。
///
/// 该规则曾在 startup.rs（sync_startup_config）、gui/commands.rs
/// （set_battery_care_internal / set_charge_limit_internal / ReapplyConfig）
/// 四处各自实现过，存在漂移风险——统一收敛到此处后，任何一处修改都会同时
/// 作用于全部路径。
pub fn sync_config_after_apply(config: &mut AppConfig, applied: u8) {
    let care = care_enabled_from_limit(applied);
    config.battery_care_enabled = care;
    if care {
        config.battery_charge_limit = applied;
    }
}

/// 统一"写充电上限 → 写养护位 → 读回"的序列与兜底规则。
///
/// 该序列曾四处各自实现（main.rs 启动应用、gui/commands.rs 的
/// set_battery_care_internal / set_charge_limit_internal / ReapplyConfig），
/// 存在漂移风险——统一收敛到此处后，任何一处修改规则都会同时作用于全部路径。
///
/// 约定：
/// - 先写限值：部分 EC 固件会从限值寄存器自动同步养护位；
/// - 养护开启时限值先经 `coherent_charge_limit` 兜底（≥100 → 80），
///   关闭时上限为 100%；
/// - 限值写入成功后读回硬件实际生效值（WMI 会把非预设值量化到最近预设，
///   如 85→80），由调用方决定是否回写持久化配置。
pub fn apply_battery_state(
    backend: &dyn EcBackend,
    care: bool,
    desired_limit: u8,
) -> BatteryApplyOutcome {
    let limit = if care {
        coherent_charge_limit(true, desired_limit)
    } else {
        FULL_CHARGE_LIMIT
    };
    let charge_limit = match backend.set_charge_limit(limit) {
        Ok(()) => {
            log::info!("Charge limit set to {}%", limit);
            // 写成功后读回硬件实际生效值（WMI 会把非预设值量化到最近预设，
            // 见 apply_battery_state 文档）。读回失败时保留写入值并记录警告
            // ——不能静默当成读回成功：调用方会把读回值写回持久化配置，
            // 静默吞掉会使 config 与硬件实际值长期背离且无法排查。
            //
            // 读回契约（修订 1.46 审计）：后端 get_charge_limit 对非法值
            // （0 / >100）返回 Err（见 winring0.rs / wmi.rs 的读回校验）。
            // 合法范围由后端保证；万一某后端越界返回 Ok(0)/Ok(>100)，
            // 在此显式拒绝（Err），由调用方走"保留写入值"的兜底，绝不冒充
            // 成功（纵深防御，GarbageReadback 回归测试锁定）。
            let readback = || -> Result<u8, EcError> {
                let actual = backend.get_charge_limit()?;
                // 读回契约（修订 1.46/1.47 审计）：后端 get_charge_limit 对非法值
                // （0 / >100）返回 Err（见 winring0.rs / wmi.rs 的读回校验）。
                // 0 同样是垃圾值（GUI 滑块下限 40、WMI 预设下限 40、配置消毒
                // 把 0 归一为默认）——只判 >100 会让损坏的 0 被当作"合法 0%"
                // 持久化进配置（care=true + limit=0）。此处显式拒绝（Err），
                // 由调用方走"保留写入值"的兜底，绝不冒充成功（纵深防御，
                // GarbageReadback 回归测试锁定）。
                if actual == 0 || actual > FULL_CHARGE_LIMIT {
                    log::warn!(
                        "Charge limit readback out of range: {}%; treating as failure",
                        actual
                    );
                    return Err(EcError::InvalidData(format!(
                        "充电上限读回值 {}% 非法",
                        actual
                    )));
                }
                Ok(actual)
            };
            match readback() {
                Ok(actual) => {
                    // 量化读回结果：请求值与硬件实际生效值不一致（如 85→80）
                    // 是"UI 滑块/配置显示值与硬件不符"类问题的最直接线索，
                    // 必须记录请求值与读回值两者的关系。
                    if actual != limit {
                        log::info!(
                            "Charge limit readback: requested {}%, hardware applied {}% (quantized)",
                            limit,
                            actual
                        );
                    } else {
                        log::debug!("Charge limit readback: {}%", actual);
                    }
                    Ok(actual)
                }
                Err(e) => {
                    log::warn!(
                        "Charge limit written to {}%, but readback failed: {}; assuming {}%",
                        limit,
                        e,
                        limit
                    );
                    // 读回失败兜底：写入值（coherent 后 ≤100，无需再钳）。
                    Ok(limit)
                }
            }
        }
        Err(e) => Err(e),
    };
    // 养护位写入：`care_result` 避免与形参 `care`（bool）同名遮蔽
    // （修订 1.46 审计：`let care = ...set_battery_care(care)` 在函数内
    // 重新绑定同名变量，后续阅读容易混淆"请求值"与"写入结果"）。
    let care_result = match backend.set_battery_care(care) {
        Ok(()) => {
            log::info!(
                "Battery care set to {}",
                if care { "enabled" } else { "disabled" }
            );
            Ok(())
        }
        Err(e) => Err(e),
    };
    BatteryApplyOutcome {
        charge_limit,
        care: care_result,
    }
}

/// 一次"把整份配置应用到硬件"的结果：电池部分（充电上限 + 养护位，见
/// `BatteryApplyOutcome`）、性能模式写入结果，以及性能模式实际写入的 raw
/// code（狂暴在电池供电时降级为极速）。
pub struct ApplyOutcome {
    /// 充电上限与养护位的写入结果。
    pub battery: BatteryApplyOutcome,
    /// 性能模式写入结果。
    pub perf: Result<(), EcError>,
    /// 实际写入 EC 的性能模式 raw code（经交流电源保护降级后的值）。
    pub perf_written: u8,
}

impl ApplyOutcome {
    /// 所有写入失败字段的 (展示名, 错误) 列表。
    ///
    /// 该映射曾在 startup.rs（apply_errors）与 gui/commands.rs
    /// （reapply_config）各自维护一份 `if let Err` 遍历，存在字段漂移风险——
    /// 统一收敛到此处后，新增/改名任何写入字段只改这一处，全部展示路径
    /// 同时生效。
    pub fn field_errors(&self) -> Vec<(&'static str, &EcError)> {
        let mut errors = Vec::new();
        if let Err(e) = &self.battery.charge_limit {
            errors.push(("充电上限", e));
        }
        if let Err(e) = &self.battery.care {
            errors.push(("电池养护", e));
        }
        if let Err(e) = &self.perf {
            errors.push(("性能模式", e));
        }
        errors
    }
}

/// 按电源状态把用户选择的性能模式映射为实际写入值（唯一事实来源）。
///
/// - 交流供电：可写全部模式（狂暴保持）；
/// - 电池供电：狂暴降级为极速（`effective_ec_value` 的 false 分支）；
/// - 电源状态未知：不静默降级（平台层约定），按用户选择原样写入。
///
/// GUI 切换路径（`effective_perf_for_power`）与启动/电源重设路径
/// （`effective_applied_mode`）共用此映射，避免两处漂移。
fn raw_perf_for_status(mode: u8, status: crate::app::power::PowerStatus) -> u8 {
    match status {
        crate::app::power::PowerStatus::OnAc => {
            crate::app::performance::effective_ec_value(mode, true)
        }
        crate::app::power::PowerStatus::OnBattery => {
            crate::app::performance::effective_ec_value(mode, false)
        }
        // 未知电源状态：不静默降级（平台层约定），按用户选择写入。
        crate::app::power::PowerStatus::Unknown => mode,
    }
}

/// 根据给定电源状态计算实际应写入 EC 的性能模式 raw code。
///
/// 历史实现直接在函数内调用 `power_status()` 查询电源（`ec::battery` 反向
/// 依赖平台层），且把"电源状态未知"一律当作电池供电——交流下狂暴模式被静默
/// 降级为极速。改为三态判定：仅在**确认**电池供电时降级，未知时按用户选择
/// 的模式写入并告警（不做静默降级）。电源状态由调用方经 `PowerSource` 端口
/// 取得后传入，本函数保持纯逻辑。
///
/// GUI 切换路径（`gui::commands::set_perf_mode_internal`）与启动/电源重设路径
/// （`apply_config_to_hardware`）共用一个映射来源（`raw_perf_for_status`）。
pub fn effective_perf_for_power(mode: u8, status: crate::app::power::PowerStatus) -> u8 {
    if status == crate::app::power::PowerStatus::Unknown {
        log::warn!("电源状态未知；按用户选择写入性能模式 {:#x}", mode);
    }
    raw_perf_for_status(mode, status)
}

/// 电池自动切节能 + 电源降级合并后的**实际写入**性能模式。
///
/// 纯逻辑（便于单测）：先按"电池供电自动切节能"覆盖用户模式，再走既有
/// 电源降级映射（`raw_perf_for_status`）。返回 (实际写入, 是否与用户选择不同)。
fn effective_applied_mode(
    user_mode: u8,
    auto_switch_to_quiet_on_battery: bool,
    status: crate::app::power::PowerStatus,
) -> (u8, bool) {
    let mode =
        if auto_switch_to_quiet_on_battery && status == crate::app::power::PowerStatus::OnBattery {
            crate::app::performance::PerfMode::Eco.ec_value()
        } else {
            user_mode
        };
    let raw = raw_perf_for_status(mode, status);
    (raw, raw != user_mode)
}

/// 统一"把持久化配置整份应用到硬件"的序列：写充电上限 + 养护位
/// （`apply_battery_state`），再按电源状态写性能模式（狂暴在电池供电时
/// 降级为极速）。
///
/// 该序列曾在 startup.rs（apply_startup_config）与 gui/commands.rs
/// （ReapplyConfig 电源重设）各自实现过，存在漂移风险——统一收敛到此处后，
/// 任何一处修改写序/降级规则都会同时作用于全部路径。调用方仍自行决定
/// 写入成功后的配置回写与错误展示。`status` 由调用方经 `PowerSource` 端口
/// 取得后传入（领域层不直接查询 Windows 电源 API）。
pub fn apply_config_to_hardware(
    backend: &dyn EcBackend,
    config: &AppConfig,
    status: crate::app::power::PowerStatus,
) -> ApplyOutcome {
    let bat = apply_battery_state(
        backend,
        config.battery_care_enabled,
        config.battery_charge_limit,
    );
    let (raw, adjusted) = effective_applied_mode(
        config.performance_mode,
        config.auto_switch_to_quiet_on_battery,
        status,
    );
    if adjusted {
        log::info!(
            "Perf mode {:#x} applied as {:#x} (power-related adjustment)",
            config.performance_mode,
            raw
        );
    }
    let perf = backend.set_performance_mode(raw);
    ApplyOutcome {
        battery: bat,
        perf,
        perf_written: raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::performance::PerfMode;
    use crate::app::power::PowerStatus;
    use crate::ec::mock::MockBackend;
    use std::sync::atomic::Ordering;

    /// 养护开启 + 上限 100%（矛盾组合）：必须兜底写 80% 并读回。
    #[test]
    fn test_apply_battery_state_care_on_incoherent_limit_uses_80() {
        let backend = MockBackend::default();
        let outcome = apply_battery_state(&backend, true, 100);
        assert!(matches!(outcome.charge_limit, Ok(80)));
        assert!(outcome.care.is_ok());
        assert_eq!(backend.charge_limit.load(Ordering::Relaxed), 80);
        assert!(backend.battery_care.load(Ordering::Relaxed));
    }

    /// 养护关闭：上限写 100%，但保留 desired_limit（读回值即 100）。
    #[test]
    fn test_apply_battery_state_disable_writes_100() {
        let backend = MockBackend::default();
        let outcome = apply_battery_state(&backend, false, 60);
        assert!(matches!(outcome.charge_limit, Ok(100)));
        assert_eq!(backend.charge_limit.load(Ordering::Relaxed), 100);
        assert!(!backend.battery_care.load(Ordering::Relaxed));
    }

    /// WMI 量化：请求 85%，读回硬件实际生效的 80%。
    #[test]
    fn test_apply_battery_state_reads_back_quantized_value() {
        let backend = MockBackend::quantizing();
        let outcome = apply_battery_state(&backend, true, 85);
        assert!(matches!(outcome.charge_limit, Ok(80)));
        assert_eq!(backend.charge_limit.load(Ordering::Relaxed), 80);
    }

    /// 限值写入失败：charge_limit 为 Err，不尝试读回，但养护位仍会写入。
    #[test]
    fn test_apply_battery_state_limit_failure_returns_err() {
        let backend = MockBackend::charge_limit_fails();
        let outcome = apply_battery_state(&backend, true, 80);
        assert!(outcome.charge_limit.is_err());
        assert!(outcome.care.is_ok());
    }

    /// 回归测试（回读垃圾值不得冒充 100%）：写入成功后读回失败时，必须以
    /// **写入值**兜底（Ok(written)）而非其他值。后端读回路径（winring0 /
    /// wmi）对非法寄存器值返回 Err，调用方据此保留用户设置——历史实现把
    /// 垃圾值钳到 100 返回 Ok(100)，GUI 会按 care=false 持久化，下次启动
    /// 强制写 100%，用户设置的养护被静默摧毁。
    #[test]
    fn test_apply_battery_state_readback_failure_keeps_written_value() {
        let backend = MockBackend {
            read_fails: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            set_charge_limit_fails: false,
            set_battery_care_fails: false,
            set_perf_fails: false,
            ..Default::default()
        };
        let outcome = apply_battery_state(&backend, true, 60);
        assert!(
            matches!(outcome.charge_limit, Ok(60)),
            "must keep the written value"
        );
        assert!(outcome.care.is_ok());
    }

    /// 回归测试（修订 1.46 审计）：读回值 >100 不得被钳成 Ok(100)——真实后端
    /// 与 mock 都把垃圾读回判为 Err，这里是**纵深防御**：若未来某后端越界返回
    /// Ok(>100)，历史 `actual.min(100)` 会把它静默伪装成"成功写了 100%"（GUI
    /// 按 care=false 持久化、下次启动强制写 100% 摧毁用户养护设置）。正确行为
    /// 与"读回失败"同路径：保留**写入值**（coherent 后，如 60）而非垃圾值。
    /// 用内联后端模拟"写入成功但读回 0xFF=255"的损坏寄存器。
    #[test]
    fn test_apply_battery_state_rejects_readback_above_100() {
        struct GarbageReadback;
        impl crate::app::ec::EcBackend for GarbageReadback {
            fn name(&self) -> &'static str {
                "garbage-readback"
            }
            fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
                Ok(false)
            }
            fn get_charge_limit(&self) -> Result<u8, EcError> {
                Ok(255)
            }
            fn set_battery_care(&self, _enabled: bool) -> Result<(), EcError> {
                Ok(())
            }
            fn set_charge_limit(&self, _percent: u8) -> Result<(), EcError> {
                Ok(())
            }
            fn get_performance_mode(&self) -> Result<u8, EcError> {
                Ok(PerfMode::Smart.ec_value())
            }
            fn set_performance_mode(&self, _mode: u8) -> Result<(), EcError> {
                Ok(())
            }
        }
        let outcome = apply_battery_state(&GarbageReadback, true, 60);
        // 垃圾读回与"读回失败"同路径：保留写入值（60），绝不返回垃圾 100。
        assert!(
            matches!(outcome.charge_limit, Ok(60)),
            "garbage readback (>100) must keep the written value, got {:?}",
            outcome.charge_limit
        );
    }

    /// 回归测试（修订 1.47 审计）：读回值 **0** 同样必须判为垃圾并保留写入值
    /// ——历史实现只拒绝 >100，损坏的 0 会被当作"合法 0%"走 Ok(0)，调用方
    /// 按 care=true + limit=0 持久化，后续任何保存路径都会把这个荒谬组合
    /// 写进磁盘。与 `>100` 同路径：Err → 保留写入值（coherent 后，如 60）。
    /// 用内联后端模拟"写入成功但读回 0x00"的损坏寄存器。
    #[test]
    fn test_apply_battery_state_rejects_readback_zero() {
        struct ZeroReadback;
        impl crate::app::ec::EcBackend for ZeroReadback {
            fn name(&self) -> &'static str {
                "zero-readback"
            }
            fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
                Ok(false)
            }
            fn get_charge_limit(&self) -> Result<u8, EcError> {
                Ok(0)
            }
            fn set_battery_care(&self, _enabled: bool) -> Result<(), EcError> {
                Ok(())
            }
            fn set_charge_limit(&self, _percent: u8) -> Result<(), EcError> {
                Ok(())
            }
            fn get_performance_mode(&self) -> Result<u8, EcError> {
                Ok(PerfMode::Smart.ec_value())
            }
            fn set_performance_mode(&self, _mode: u8) -> Result<(), EcError> {
                Ok(())
            }
        }
        let outcome = apply_battery_state(&ZeroReadback, true, 60);
        assert!(
            matches!(outcome.charge_limit, Ok(60)),
            "zero readback must keep the written value, got {:?}",
            outcome.charge_limit
        );
    }

    /// 写入前校验：0 必须拒绝（读回契约把 0 判为非法，写入 0 会被静默
    /// 落成 WinRing0 0x00 / WMI 40% 的兜底），>100 钳到 100，合法值原样通过。
    #[test]
    fn test_validate_charge_limit_write() {
        assert!(validate_charge_limit_write(0).is_err());
        assert_eq!(validate_charge_limit_write(1).unwrap(), 1);
        assert_eq!(validate_charge_limit_write(40).unwrap(), 40);
        assert_eq!(validate_charge_limit_write(100).unwrap(), 100);
        assert_eq!(validate_charge_limit_write(150).unwrap(), 100);
    }

    /// 回归测试：`apply_battery_state` 的养护开启路径必须把上限 0 兜底为 80
    /// （历史实现原样写 0，WinRing0 写 0x00、WMI 就近映射成 40%），不能把
    /// 无效输入直接写进寄存器再由读回兜底暴露。
    #[test]
    fn test_apply_battery_state_care_on_zero_limit_uses_80() {
        let backend = MockBackend::default();
        let outcome = apply_battery_state(&backend, true, 0);
        assert!(matches!(outcome.charge_limit, Ok(80)));
        assert!(outcome.care.is_ok());
        assert_eq!(backend.charge_limit.load(Ordering::Relaxed), 80);
        assert!(backend.battery_care.load(Ordering::Relaxed));
    }

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
        let codes: std::collections::HashSet<u8> =
            WMI_CHARGE_LIMITS.iter().map(|(r, _)| *r).collect();
        assert_eq!(codes.len(), 7);
        let percents: std::collections::HashSet<u8> =
            WMI_CHARGE_LIMITS.iter().map(|(_, p)| *p).collect();
        assert_eq!(percents.len(), 7);
    }

    #[test]
    fn test_wmi_rawcode_to_percent_bidirectional() {
        for (rawcode, percent) in WMI_CHARGE_LIMITS {
            assert_eq!(percent_to_wmi_rawcode(*percent), Some(*rawcode));
            assert_eq!(wmi_rawcode_to_percent(*rawcode), Some(*percent));
        }
    }

    /// 回归测试：GUI 展示的 WMI 预设档位（WMI_PRESET_PERCENTS）必须与
    /// 映射表（WMI_CHARGE_LIMITS）的 percent 集合完全一致。历史实现把
    /// 同一份档位列表硬编码在 view.rs，与映射表重复为两个事实来源，
    /// 一处修改另一处漂移。
    #[test]
    fn test_wmi_preset_percents_match_mapping_table() {
        let table: std::collections::HashSet<u8> =
            WMI_CHARGE_LIMITS.iter().map(|(_, p)| *p).collect();
        let presets: std::collections::HashSet<u8> = WMI_PRESET_PERCENTS.iter().copied().collect();
        assert_eq!(
            presets, table,
            "view presets must equal mapping table percents"
        );
    }

    /// apply_config_to_hardware 必须一次应用电池养护 + 充电上限 + 性能模式，
    /// 结果与直接调用 apply_battery_state / set_performance_mode 一致。
    #[test]
    fn test_apply_config_to_hardware_applies_battery_and_perf() {
        let backend = MockBackend::default();
        let config = AppConfig {
            battery_care_enabled: true,
            battery_charge_limit: 80,
            performance_mode: 0x09,
            ..Default::default()
        };
        let outcome = apply_config_to_hardware(&backend, &config, PowerStatus::OnAc);
        assert!(outcome.battery.charge_limit.is_ok());
        assert!(outcome.battery.care.is_ok());
        assert!(outcome.perf.is_ok());
        assert_eq!(
            backend.charge_limit.load(Ordering::Relaxed),
            80,
            "charge limit must be written"
        );
        assert!(backend.battery_care.load(Ordering::Relaxed));
        assert_eq!(
            backend.perf_mode.load(Ordering::Relaxed),
            outcome.perf_written
        );
    }

    /// 性能模式写入失败时，perf 字段必须如实反映错误（供启动/重设展示）。
    #[test]
    fn test_apply_config_to_hardware_perf_failure() {
        let backend = MockBackend {
            set_perf_fails: true,
            ..Default::default()
        };
        let outcome = apply_config_to_hardware(&backend, &AppConfig::default(), PowerStatus::OnAc);
        assert!(outcome.perf.is_err());
        assert!(
            outcome.battery.charge_limit.is_ok(),
            "battery still applied"
        );
    }

    /// WMI 量化：整份应用时读回的硬件实际生效值必须跟随预设（85→80）。
    #[test]
    fn test_apply_config_to_hardware_quantizes_limit() {
        let backend = MockBackend::quantizing();
        let config = AppConfig {
            battery_care_enabled: true,
            battery_charge_limit: 85,
            ..Default::default()
        };
        let outcome = apply_config_to_hardware(&backend, &config, PowerStatus::OnAc);
        assert_eq!(outcome.battery.charge_limit.unwrap(), 80);
    }

    /// 电池供电自动切节能（纯逻辑）：开启 + 电池 → Eco；开启 + 交流 →
    /// 保持用户模式；关闭 + 电池 → 保持用户模式。
    #[test]
    fn test_effective_applied_mode_auto_quiet_on_battery() {
        use crate::app::power::PowerStatus;
        let smart = PerfMode::Smart.ec_value();
        // 开启 + 电池：切节能（与用户选择不同）。
        let (raw, adjusted) = effective_applied_mode(smart, true, PowerStatus::OnBattery);
        assert_eq!(raw, PerfMode::Eco.ec_value());
        assert!(adjusted);
        // 开启 + 交流：保持原模式。
        let (raw, adjusted) = effective_applied_mode(smart, true, PowerStatus::OnAc);
        assert_eq!(raw, smart);
        assert!(!adjusted);
        // 关闭 + 电池：保持用户模式。
        let (raw, adjusted) = effective_applied_mode(smart, false, PowerStatus::OnBattery);
        assert_eq!(raw, smart);
        assert!(!adjusted);
    }

    /// 电池自动切节能与狂暴降级的交互：开启时狂暴在电池下直接切 Eco，
    /// 关闭时狂暴在电池下降级为极速（既有规则不变）。
    #[test]
    fn test_effective_applied_mode_auto_quiet_interacts_with_extreme() {
        use crate::app::power::PowerStatus;
        let extreme = PerfMode::Extreme.ec_value();
        let (raw, adjusted) = effective_applied_mode(extreme, true, PowerStatus::OnBattery);
        assert_eq!(raw, PerfMode::Eco.ec_value());
        assert!(adjusted);
        let (raw, adjusted) = effective_applied_mode(extreme, false, PowerStatus::OnBattery);
        assert_eq!(raw, PerfMode::Fast.ec_value());
        assert!(adjusted);
    }

    /// 未知电源状态不静默降级（平台层约定）：按用户选择原样写入。
    #[test]
    fn test_effective_applied_mode_unknown_keeps_user_mode() {
        use crate::app::power::PowerStatus;
        let extreme = PerfMode::Extreme.ec_value();
        let (raw, adjusted) = effective_applied_mode(extreme, false, PowerStatus::Unknown);
        assert_eq!(raw, extreme);
        assert!(!adjusted);
    }

    /// 配置同步规则（持久化权威收敛点）：
    /// - 养护开启（applied < 100）→ care=true 且配置上限 = 硬件实际生效值；
    /// - 养护关闭（applied == 100）→ care=false 且**保留**用户期望上限，
    ///   不被 100% 覆盖（重新开启养护时恢复）。
    #[test]
    fn test_sync_config_after_apply_preserves_desired_limit_when_off() {
        let mut cfg = AppConfig {
            battery_care_enabled: true,
            battery_charge_limit: 80,
            ..Default::default()
        };
        // 养护关闭：硬件写入 100%，配置上限必须保留 80（供重新开启时恢复）。
        sync_config_after_apply(&mut cfg, 100);
        assert!(!cfg.battery_care_enabled);
        assert_eq!(cfg.battery_charge_limit, 80, "desired limit preserved");
        // 养护开启：配置与硬件一致（WMI 量化后实际生效值写回）。
        sync_config_after_apply(&mut cfg, 90);
        assert!(cfg.battery_care_enabled);
        assert_eq!(cfg.battery_charge_limit, 90);
    }

    /// 养护开启时上限写回硬件的实际生效值（含量化）：非预设值经 WMI 量化
    /// 后，配置应记录硬件真实值而非用户输入。
    #[test]
    fn test_sync_config_after_apply_records_quantized_limit() {
        let mut cfg = AppConfig {
            battery_charge_limit: 80,
            ..Default::default()
        };
        // 硬件实际生效 70（WMI 量化最近预设）。
        sync_config_after_apply(&mut cfg, 70);
        assert!(cfg.battery_care_enabled);
        assert_eq!(cfg.battery_charge_limit, 70);
    }
}
