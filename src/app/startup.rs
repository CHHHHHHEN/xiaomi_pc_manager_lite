//! 启动编排：后端创建/回退、启动自动应用配置与硬件状态回写同步。
//!
//! 从 main.rs 抽出，使 main 保持为薄入口，这些纯逻辑（无 GUI 依赖）可以
//! 独立单元测试。包含：
//! - `init_backend`：创建/回退后端、应用启动配置、计算实际生效的偏好；
//! - `apply_startup_config` / `sync_startup_config`：启动应用与硬件读回同步。

use crate::app::battery::care_enabled_from_limit;
use crate::app::config::{AppConfig, BackendPreference, ConfigStore};
use crate::app::ec::{EcBackend, EcBackendFactory};
use crate::app::power::{PowerSource, PowerStatus};

/// `init_backend` 的完整结果：后端、启动同步后的配置、初始化错误、实际生效偏好。
pub struct StartupResult {
    pub backend: Box<dyn EcBackend>,
    pub config: AppConfig,
    pub init_error: Option<String>,
    pub effective_pref: BackendPreference,
}

/// 优先后端不可用时，应回退到的"另一个"后端。
///
/// - `Wmi` ↔ `WinRing0`：互为回退；
/// - `Auto` 返回 `None`：`create_backend(Auto)` 内部已依次尝试过 WMI 与
///   WinRing0，两者都失败后不存在"另一个"后端可重试——重试 Auto 只会把
///   同一批双后端原样再来一轮（非小米机器上白白多耗一次完整 WMI 连接握手
///   与 WinRing0 停装驱动的清理流程），直接进入错误路径。
fn fallback_preference(pref: BackendPreference) -> Option<BackendPreference> {
    match pref {
        BackendPreference::Wmi => Some(BackendPreference::WinRing0),
        BackendPreference::WinRing0 => Some(BackendPreference::Wmi),
        BackendPreference::Auto => None,
    }
}

/// 启动阶段的后端初始化与配置应用：创建后端（必要时回退）、应用启动配置、
/// 把硬件实际状态回写进持久化配置，并计算 GUI 应显示的实际生效偏好。
///
/// 返回修改后的 config：启动同步（量化读回、矛盾兜底）发生在该副本上并已
/// 落盘；若不把该副本交还给 GUI，GUI 的 save_state() 会把未同步的旧值
/// （如 care=true+limit=100、85% 非预设值）重新写回磁盘，覆盖启动时验证过
/// 的配置，导致磁盘配置反复"复活"矛盾组合。
///
/// 后端创建与 NullBackend 兜底经 `EcBackendFactory` 端口注入（组合根在
/// main.rs 提供 `ec::backend::BackendFactory`），本函数不接触 `ec` 实现细节。
pub fn init_backend(
    store: ConfigStore,
    mut config: AppConfig,
    power: &dyn PowerSource,
    factory: &dyn EcBackendFactory,
) -> StartupResult {
    let (backend, mut init_error): (Box<dyn EcBackend>, Option<String>) =
        match factory.create(config.backend) {
            Ok(b) => {
                log::info!(
                    "EC backend: {} (preference: {:?})",
                    b.name(),
                    config.backend
                );
                (b, None)
            }
            Err(primary_err) => {
                // 回退时**不要**直接尝试 Auto：Auto 是 WMI 优先，会先把刚
                // 失败的优先后端原样再试一遍（WMI 会再次拉起 worker 线程并
                // 等待握手，非小米机器上白白多耗一次完整连接；WinRing0 会
                // 重复停装驱动的清理流程）。直接回退到另一个后端即可。
                match fallback_preference(config.backend) {
                    Some(pref) => {
                        log::warn!(
                            "Configured backend {:?} unavailable; falling back to {:?}",
                            config.backend,
                            pref
                        );
                        match factory.create(pref) {
                            Ok(b) => {
                                let name = b.name().to_string();
                                log::info!("Fallback EC backend: {}", name);
                                (b, Some(format!("优先后端不可用，已自动切换至 {}", name)))
                            }
                            Err(e) => {
                                log::error!("Failed to create any EC backend: {}", e);
                                (factory.null_backend(), Some(e.to_string()))
                            }
                        }
                    }
                    None => {
                        log::error!("All EC backends unavailable: {}", primary_err);
                        (factory.null_backend(), Some(primary_err.to_string()))
                    }
                }
            }
        };

    if config.auto_apply_on_startup {
        let status = power.snapshot().status;
        let outcome = apply_startup_config(&*backend, &config, status);

        // F-START-04: 自动应用失败的错误除了记录日志，还要在 GUI 中展示。
        let apply_err = apply_errors(&outcome);
        if !apply_err.is_empty() {
            let apply_err = format!("启动应用设置失败: {}", apply_err);
            init_error = Some(match init_error.take() {
                Some(e) => format!("{}; {}", e, apply_err),
                None => apply_err,
            });
        }

        // Only sync the stored config to the verified hardware state when it
        // was actually applied.  Otherwise the saved user preferences would be
        // silently overwritten by whatever the hardware currently reports.
        sync_startup_config(&*backend, &mut config, &outcome);

        if let Err(e) = store.save(&config) {
            log::warn!("save initial config: {}", e);
        }
    } else {
        // auto_apply 关闭是配置明确的选择：硬件保持现状、只读不改。
        // 不记录会让用户困惑"为什么启动后设置没生效"。
        log::info!("Startup auto-apply disabled; hardware left untouched");
    }

    // 实际生效的后端偏好：后端创建失败、用 NullBackend 兜底时，config.backend
    // 仍是用户偏好（合理——下次启动还会重试），GUI 的"EC 后端偏好"单选应显示
    // 用户偏好；回退成功时则显示**实际运行**的后端，避免 GUI 显示"偏好选中了
    // 一个不可用后端、而状态栏显示另一个"的矛盾（F-ERR-03 的一致性）。
    let effective_pref = if backend.is_null() {
        config.backend
    } else {
        backend.preference()
    };

    log::info!(
        "Backend init complete: backend={}, effective_pref={:?}, init_error={}",
        backend.name(),
        effective_pref,
        init_error.as_deref().unwrap_or("无")
    );

    StartupResult {
        backend,
        config,
        init_error,
        effective_pref,
    }
}

/// 把一次"整份配置应用到硬件"的结果转成用户可读的失败描述（字段级文案以
/// "; " 连接），同时为每项失败写一条启动阶段日志。
///
/// 与 `gui::commands::reapply_config` 的字段级错误文案不同，这里的条目是无
/// 上下文前缀的字段级描述（"启动应用设置失败: " 前缀由调用方统一拼接）。
/// 失败字段的遍历统一收敛在 `ApplyOutcome::field_errors`。仅构建字符串并
/// 直接返回（历史先收集 Vec 再 join，修订 1.47 清理）。
fn apply_errors(outcome: &crate::app::battery::ApplyOutcome) -> String {
    let mut errors = Vec::new();
    for (field, e) in outcome.field_errors() {
        log::warn!("Startup apply {} failed: {}", field, e);
        errors.push(format!("{}: {}", field, e));
    }
    errors.join("; ")
}

fn apply_startup_config(
    backend: &dyn EcBackend,
    config: &AppConfig,
    status: PowerStatus,
) -> crate::app::battery::ApplyOutcome {
    log::info!(
        "Applying config on startup: care={}, limit={}%, perf={:#x}",
        config.battery_care_enabled,
        config.battery_charge_limit,
        config.performance_mode
    );
    // Keep battery care and charge limit coherent: when care is disabled
    // the limit must be 100%, otherwise backends that derive the care bit
    // from the limit would report it as enabled.  The unified
    // battery::apply_config_to_hardware writes the limit first (some EC
    // firmware auto-syncs the care bit from it) then the care bit, then the
    // perf mode (with AC-power degradation), and returns each result.
    let desired_limit = crate::app::limits::coherent_charge_limit(
        config.battery_care_enabled,
        config.battery_charge_limit,
    );
    if desired_limit != config.battery_charge_limit {
        log::warn!(
            "Incoherent config: battery care on with limit {}%; using {}%",
            config.battery_charge_limit,
            crate::app::limits::FALLBACK_CARE_LIMIT
        );
    }
    crate::app::battery::apply_config_to_hardware(backend, config, status)
}

/// 把启动应用成功后验证过的硬件配置回写进持久化配置，使磁盘配置与硬件
/// 实际状态保持一致（量化读回、矛盾兜底等规范化已发生在 apply 路径）。
///
/// 只在**对应项写入成功**时才回写：写入失败时硬件未按期望改变，读回的是
/// 硬件旧状态，用它覆盖配置会把用户的选择静默改掉。
///
/// 关键场景（B）：电池养护开启但充电上限写入失败时，WMI 的 set_battery_care
/// 是契约性 no-op 恒返回 Ok（battery.care.is_ok() 为 true），而硬件上限仍是
/// 100%，读回 care=false。若此时回写，config.battery_care_enabled 会被改成
/// false，启动应用失败被静默持久化，下次启动还会按 care=false 强制写 100%，
/// 用户设置的充电上限被永久摧毁。因此电池回写必须同时要求
/// `battery.charge_limit.is_ok()`。
fn sync_startup_config(
    backend: &dyn EcBackend,
    config: &mut AppConfig,
    outcome: &crate::app::battery::ApplyOutcome,
) {
    // 性能模式：仅当写入成功且写入的 raw code 就是用户选择的模式时才回写。
    // 狂暴模式在电池供电时降级为极速（perf_written != performance_mode），
    // 不能把降级值当成用户选择。
    if outcome.perf.is_ok() && outcome.perf_written == config.performance_mode {
        if let Ok(mode) = backend.get_performance_mode() {
            config.performance_mode = mode;
        }
    } else {
        log::debug!(
            "Startup sync: perf not written back (write_ok={}, written={:#x}, user={:#x})",
            outcome.perf.is_ok(),
            outcome.perf_written,
            config.performance_mode
        );
    }
    // 电池养护 + 充电上限：两者都写入成功才回写（原因见函数注释）。
    // 读回的养护位与硬件实际生效值统一经 battery::sync_config_after_apply
    // 收敛（养护关闭时保留用户期望上限，开启时回写实际生效值）。
    //
    // **养护位权威来源是限值而非读回位**（修订 1.32/L5）：限值是两种后端
    // 判定养护状态的唯一权威（care_enabled_from_limit），而 `get_battery_care_enabled`
    // 的读回位在部分固件上是"写限值时同步"的从属位，甚至 WMI 写养护是
    // 契约上的 no-op（set_battery_care 返回 Ok 但不落地）——若把读回的
    // false 直接回写，会持久化 care=false + limit=80 的矛盾组合，下次启动
    // 按 care=false 强制写 100%，用户设置的充电上限被永久摧毁（与
    // sync_config_after_apply 注释里的历史回归一致）。读回仅用于日志对照。
    if outcome.battery.care.is_ok() && outcome.battery.charge_limit.is_ok() {
        if let Ok(applied) = outcome.battery.charge_limit.as_ref() {
            let readback = backend.get_battery_care_enabled();
            match readback {
                Ok(enabled) if enabled != care_enabled_from_limit(*applied) => log::warn!(
                    "Startup sync: care read-back {} disagrees with limit-derived {} (limit {}%); trusting limit",
                    enabled,
                    care_enabled_from_limit(*applied),
                    applied
                ),
                Ok(_) => {}
                Err(e) => log::debug!("Startup sync: care read-back failed: {}", e),
            }
            crate::app::battery::sync_config_after_apply(config, *applied);
        }
    } else {
        log::debug!(
            "Startup sync: battery not written back (care_ok={}, limit_ok={})",
            outcome.battery.care.is_ok(),
            outcome.battery.charge_limit.is_ok()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ec::mock::MockBackend;

    /// 回归测试（B）：养护开启但充电上限写入失败时，启动同步不得把用户
    /// 的 care=true 静默改写为硬件读回的 false。历史实现只检查 battery_care_ok
    /// （WMI 的 set_battery_care 恒 Ok），导致应用失败被静默持久化，下次
    /// 启动还会按 care=false 强制写 100%，摧毁用户设置的充电上限。
    #[test]
    fn test_partial_write_failure_keeps_user_config() {
        let backend = MockBackend::charge_limit_fails();
        let config = AppConfig {
            auto_apply_on_startup: true,
            battery_care_enabled: true,
            battery_charge_limit: 80,
            ..Default::default()
        };
        let outcome = apply_startup_config(&backend, &config, PowerStatus::OnBattery);

        // 写入结果必须如实反映失败。
        assert!(outcome.battery.charge_limit.is_err());
        assert!(outcome.battery.care.is_ok());
        assert!(!apply_errors(&outcome).is_empty());

        let mut cfg = config.clone();
        sync_startup_config(&backend, &mut cfg, &outcome);
        // 用户选择必须保留，不得被硬件旧状态覆盖。
        assert!(
            cfg.battery_care_enabled,
            "care must not be overwritten by readback"
        );
        assert_eq!(cfg.battery_charge_limit, 80);
    }

    /// 回归测试（修订 1.32/L5）：WMI 的养护位写入是契约 no-op（返回 Ok 但
    /// 不落地），读回的 care=false 与已写入的限值 80% 矛盾。历史实现用
    /// `get_battery_care_enabled` 的读回值覆盖限值推导的养护位，持久化了
    /// care=false + limit=80 的矛盾组合——下次启动按 care=false 强制写 100%，
    /// 用户设置的充电上限被永久摧毁。修复后**限值推导是权威**，读回仅记录
    /// 日志对照。
    #[test]
    fn test_care_noop_readback_does_not_clobber_limit_derived_care() {
        let backend = MockBackend {
            care_write_is_noop: true,
            ..MockBackend::default()
        };
        let config = AppConfig {
            auto_apply_on_startup: true,
            battery_care_enabled: true,
            battery_charge_limit: 80,
            ..Default::default()
        };
        let outcome = apply_startup_config(&backend, &config, PowerStatus::OnBattery);
        assert!(outcome.battery.care.is_ok());
        assert!(outcome.battery.charge_limit.is_ok());
        assert_eq!(outcome.battery.charge_limit.as_ref().unwrap(), &80);

        let mut cfg = config.clone();
        sync_startup_config(&backend, &mut cfg, &outcome);
        // 限值推导的养护位（applied 80 < 100 → care）必须保留，尽管读回 false。
        assert!(
            cfg.battery_care_enabled,
            "limit-derived care must survive a lying care read-back"
        );
        assert_eq!(cfg.battery_charge_limit, 80);
    }

    /// 回归测试：Auto 偏好失败后必须回退到"无可重试"，而不是把刚失败的
    /// WMI+WinRing0 双后端原样再试一遍（浪费一次完整连接握手 + 驱动清理）。
    #[test]
    fn test_fallback_preference_auto_is_none() {
        assert_eq!(fallback_preference(BackendPreference::Auto), None);
        assert_eq!(
            fallback_preference(BackendPreference::Wmi),
            Some(BackendPreference::WinRing0)
        );
        assert_eq!(
            fallback_preference(BackendPreference::WinRing0),
            Some(BackendPreference::Wmi)
        );
    }

    /// 全量成功时，启动同步把硬件实际生效的量化值（WMI 85→80）回写进配置。
    #[test]
    fn test_full_apply_syncs_quantized_hardware() {
        let backend = MockBackend {
            quantize: true,
            charge_limit: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(80)),
            battery_care: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            ..Default::default()
        };
        let config = AppConfig {
            auto_apply_on_startup: true,
            battery_care_enabled: true,
            battery_charge_limit: 85,
            ..Default::default()
        };
        let outcome = apply_startup_config(&backend, &config, PowerStatus::OnBattery);
        assert!(outcome.battery.charge_limit.is_ok() && outcome.battery.care.is_ok());

        let mut cfg = config;
        sync_startup_config(&backend, &mut cfg, &outcome);
        assert!(cfg.battery_care_enabled);
        assert_eq!(cfg.battery_charge_limit, 80);
    }
}
