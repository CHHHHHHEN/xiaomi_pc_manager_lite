#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod command;
mod ec;
mod embed;
mod gui;
mod platform;
mod tray;
mod util;

use ec::backend::EcBackend;
use ec::config::{AppConfig, BackendPreference};

/// In debug builds, set up a panic hook that pauses before exit so
/// the user can read panic messages in the console.
#[cfg(debug_assertions)]
fn init_pause_on_panic() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        prev(info);
        use std::io::Write;
        let _ = std::io::stdout().write_all(b"\n--- PANIC ---\nPress Enter to exit...");
        let _ = std::io::stdin().read_line(&mut String::new());
    }));
}

#[cfg(not(debug_assertions))]
fn init_pause_on_panic() {}

/// 初始化日志：默认写入 `%TEMP%\XiaomiPcManagerLite\app.log`（每次启动覆盖），
/// 可用 `XIAOMI_LOG_FILE` 覆盖路径。GUI 程序无控制台，文件日志便于排查
/// 托盘/后台运行场景的问题。
fn init_logging() {
    let mut builder = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    );
    let log_path = std::env::var_os("XIAOMI_LOG_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("XiaomiPcManagerLite").join("app.log"));
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::File::create(&log_path) {
        Ok(file) => {
            builder.target(env_logger::Target::Pipe(Box::new(file)));
        }
        Err(e) => {
            eprintln!("log file {}: {}", log_path.to_string_lossy(), e);
        }
    }
    let _ = builder.try_init();
}

fn main() {
    init_logging();
    init_pause_on_panic();

    // 启动即提权：**WMI 与 WinRing0 都需要管理员权限**。本机实测
    // （受限令牌对照实验）：非管理员下 `SELECT * FROM MICommonInterface`
    // 直接返回拒绝访问（Access denied），WMI 后端完全不可用；WinRing0
    // 驱动加载同样需要管理员。用户拒绝 UAC 时继续以非管理员运行，
    // create_backend 会失败并回退，GUI 显示错误（见下方回退逻辑）。
    if crate::platform::privilege::elevate_self() {
        return;
    }

    // 单实例保护（F-AUTO-08）：提权完成后的最终实例在此取得互斥体所有权。
    // 已在运行的另一实例（如自启动驻留托盘中）存在时，把已有窗口调到前台
    // 并退出，避免双份托盘/热键/Fn+K 订阅同时写 EC。互斥体句柄必须持有至
    // 进程退出：**不能**放在 match 臂体内（臂体内绑定在臂结束时即被 drop，
    // 互斥体立即释放、单实例保护失效），用 let 绑定到 main 作用域末尾。
    let _instance_guard: Option<crate::platform::single_instance::SingleInstanceGuard> =
        match crate::platform::single_instance::acquire() {
            crate::platform::single_instance::SingleInstance::Acquired(guard) => Some(guard),
            // 已有实例在运行：唤醒已有窗口后退出，不重复启动。
            crate::platform::single_instance::SingleInstance::Existing => {
                crate::platform::window::show_main_window();
                return;
            }
            // API 异常无法确认冲突（如 CreateMutexW 罕见失败）：按文档契约
            // "防御性按无冲突处理，不阻塞启动"继续启动。历史实现把 Unknown
            // 与 Existing 一并处理，导致 API 异常时应用静默退出、绝不启动。
            crate::platform::single_instance::SingleInstance::Unknown => {
                log::warn!("Single instance check unavailable; proceeding");
                None
            }
        };

    let config = AppConfig::load();

    // 后端创建与启动应用在后台线程执行：WMI 后端会在此线程调用
    // CoInitializeEx(MTA) 初始化 COM。GUI 主线程因此不携带任何 COM 初始化
    // 状态——21e0aaf 修复的回归正是主线程先被初始化为 MTA 后，其它组件
    // （当时 Tauri/tao 栈的 OleInitialize，要求 STA）再初始化 COM 时返回
    // RPC_E_CHANGED_MODE 崩溃；保持主线程"未初始化 COM"可让 eframe/winit
    // 及任何后续组件按需安全初始化。
    let thread_config = config.clone();
    // F-AUTO-06: 开机自启动任务一致性校验（后台线程，不阻塞 GUI；
    // 该线程的 COM 状态由 create_backend 或 autostart 自行初始化）。
    {
        let cfg = thread_config.clone();
        std::thread::spawn(move || {
            if let Err(e) = platform::autostart::sync(cfg.auto_start_on_boot) {
                log::warn!("autostart sync: {}", e);
            }
        });
    }
    // 返回修改后的 config：启动同步（量化读回、矛盾兜底）发生在该线程的
    // config 副本上并已落盘；若不把该副本交还给 GUI，GUI 的 save_state()
    // 会把未同步的旧值（如 care=true+limit=100、85% 非预设值）重新写回
    // 磁盘，覆盖启动时验证过的配置，导致磁盘配置反复"复活"矛盾组合。
    let (backend, config, init_error) =
        std::thread::spawn(move || -> (Box<dyn EcBackend>, AppConfig, Option<String>) {
        let mut config = thread_config;
        let (backend, mut init_error): (Box<dyn EcBackend>, Option<String>) =
            match ec::backend::create_backend(config.backend) {
                Ok(b) => {
                    log::info!("EC backend: {} (preference: {:?})", b.name(), config.backend);
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
                            match ec::backend::create_backend(pref) {
                                Ok(b) => {
                                    let name = b.name().to_string();
                                    log::info!("Fallback EC backend: {}", name);
                                    (b, Some(format!("优先后端不可用，已自动切换至 {}", name)))
                                }
                                Err(e) => {
                                    log::error!("Failed to create any EC backend: {}", e);
                                    (Box::new(ec::backend::NullBackend), Some(e.to_string()))
                                }
                            }
                        }
                        None => {
                            log::error!("All EC backends unavailable: {}", primary_err);
                            (Box::new(ec::backend::NullBackend), Some(primary_err.to_string()))
                        }
                    }
                }
            };

        if config.auto_apply_on_startup {
            let outcome = apply_startup_config(&*backend, &config);

            // F-START-04: 自动应用失败的错误除了记录日志，还要在 GUI 中展示。
            if !outcome.errors.is_empty() {
                let apply_err = format!("启动应用设置失败: {}", outcome.errors.join("; "));
                init_error = Some(match init_error.take() {
                    Some(e) => format!("{}; {}", e, apply_err),
                    None => apply_err,
                });
            }

            // Only sync the stored config to the verified hardware state when it
            // was actually applied.  Otherwise the saved user preferences would be
            // silently overwritten by whatever the hardware currently reports.
            sync_startup_config(&*backend, &mut config, &outcome);

            if let Err(e) = config.save() {
                log::warn!("save initial config: {}", e);
            }
        }

        (backend, config, init_error)
    })
    .join()
    .expect("EC backend init thread panicked");

    // F-AUTO-07: --autostart 启动时驻留托盘（首帧最小化）。
    // 用 args_os 而非 args：Windows 允许非 UTF-8 的命令行参数，args()
    // 在遇到非 UTF-8 参数时会 panic，args_os 则只做逐字节比较。
    let autostart = std::env::args_os().any(|a| a == std::ffi::OsStr::new("--autostart"));
    gui::run_app(backend, config, init_error, autostart);
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

/// 启动应用的结果：逐项记录是否成功写入硬件。
struct StartupApplyOutcome {
    charge_limit_ok: bool,
    battery_care_ok: bool,
    perf_mode_ok: bool,
    /// 实际写入 EC 的性能模式 raw code（经交流电源保护降级后的值）。
    perf_mode_written: u8,
    /// 失败项的中文描述（每项一条），用于在 GUI 中向用户展示。
    errors: Vec<String>,
}

impl Default for StartupApplyOutcome {
    fn default() -> Self {
        Self {
            charge_limit_ok: true,
            battery_care_ok: true,
            perf_mode_ok: true,
            perf_mode_written: 0,
            errors: Vec::new(),
        }
    }
}

fn apply_startup_config(backend: &dyn EcBackend, config: &AppConfig) -> StartupApplyOutcome {
    let mut outcome = StartupApplyOutcome::default();
    if !config.auto_apply_on_startup {
        return outcome;
    }
    log::info!("Applying config on startup");
    // Keep battery care and charge limit coherent: when care is disabled
    // the limit must be 100%, otherwise backends that derive the care bit
    // from the limit would report it as enabled.  The limit is written
    // first because some EC firmware auto-syncs the care bit from it.
    let desired_limit = if config.battery_care_enabled && config.battery_charge_limit >= 100 {
        // 旧版本/手改配置可能残留 care=true + limit=100 的矛盾组合
        // （旧版 refresh_from_backend 曾把硬件状态写回 config）。
        // 与 GUI 切换路径（set_battery_care_internal）保持一致，兜底为
        // 80%，否则 100% 写进硬件后养护实际失效、配置被静默改写。
        log::warn!(
            "Incoherent config: battery care on with limit {}%; using 80%",
            config.battery_charge_limit
        );
        80
    } else {
        config.battery_charge_limit
    };
    if config.battery_care_enabled {
        outcome.charge_limit_ok = match backend.set_charge_limit(desired_limit) {
            Ok(_) => true,
            Err(e) => {
                log::warn!("apply charge limit on startup: {}", e);
                outcome.errors.push(format!("充电上限: {}", e));
                false
            }
        };
    } else if let Err(e) = backend.set_charge_limit(100) {
        log::warn!("apply charge limit on startup: {}", e);
        outcome.charge_limit_ok = false;
        outcome.errors.push(format!("充电上限: {}", e));
    }
    outcome.battery_care_ok = match backend.set_battery_care(config.battery_care_enabled) {
        Ok(_) => true,
        Err(e) => {
            log::warn!("apply battery care on startup: {}", e);
            outcome.errors.push(format!("电池养护: {}", e));
            false
        }
    };
    // 狂暴模式需要交流电源：写入时按电源状态选择实际 raw code，但用户的
    // 选择仍保存在 config 中，待接入电源后通过 ReapplyConfig 恢复。
    let raw =
        ec::performance::effective_ec_value(config.performance_mode, ec::performance::ac_power_status());
    outcome.perf_mode_written = raw;
    outcome.perf_mode_ok = match backend.set_performance_mode(raw) {
        Ok(_) => true,
        Err(e) => {
            log::warn!("apply perf mode on startup: {}", e);
            outcome.errors.push(format!("性能模式: {}", e));
            false
        }
    };
    outcome
}

/// 把启动应用成功后验证过的硬件配置回写进持久化配置，使磁盘配置与硬件
/// 实际状态保持一致（量化读回、矛盾兜底等规范化已发生在 apply 路径）。
///
/// 只在**对应项写入成功**时才回写：写入失败时硬件未按期望改变，读回的是
/// 硬件旧状态，用它覆盖配置会把用户的选择静默改掉。
///
/// 关键场景（B）：电池养护开启但充电上限写入失败时，WMI 的 set_battery_care
/// 是契约性 no-op 恒返回 Ok（battery_care_ok=true），而硬件上限仍是 100%，
/// 读回 care=false。若此时回写，config.battery_care_enabled 会被改成 false，
/// 启动应用失败被静默持久化，下次启动还会按 care=false 强制写 100%，
/// 用户设置的充电上限被永久摧毁。因此电池回写必须同时要求 charge_limit_ok。
fn sync_startup_config(
    backend: &dyn EcBackend,
    config: &mut AppConfig,
    outcome: &StartupApplyOutcome,
) {
    // 性能模式：仅当写入成功且写入的 raw code 就是用户选择的模式时才回写。
    // 狂暴模式在电池供电时降级为极速（perf_mode_written != performance_mode），
    // 不能把降级值当成用户选择。
    if outcome.perf_mode_ok && outcome.perf_mode_written == config.performance_mode {
        if let Ok(mode) = backend.get_performance_mode() {
            config.performance_mode = mode;
        }
    }
    // 电池养护 + 充电上限：两者都写入成功才回写（原因见函数注释）。
    if outcome.battery_care_ok && outcome.charge_limit_ok {
        if let Ok(enabled) = backend.get_battery_care_enabled() {
            config.battery_care_enabled = enabled;
            // 养护关闭时硬件上限恒为 100%，保留用户期望的 limit（供重新
            // 开启养护时恢复）。
            if enabled {
                if let Ok(limit) = backend.get_charge_limit() {
                    config.battery_charge_limit = limit;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ec::error::EcError;

    /// 充电上限写入失败、养护写入恒成功的后端（模拟 WMI 场景：set_battery_care
    /// 是 no-op 恒 Ok，而 set_charge_limit 被固件拒绝）。读回值为硬件旧状态
    /// （养护关闭、上限 100%）。
    struct ChargeLimitFailsBackend;

    impl EcBackend for ChargeLimitFailsBackend {
        fn name(&self) -> &'static str {
            "charge-limit-fails"
        }
        fn read_byte(&self, _addr: u16) -> Result<u8, EcError> {
            Err(EcError::BackendUnavailable("mock".into()))
        }
        fn write_byte(&self, _addr: u16, _value: u8) -> Result<(), EcError> {
            Ok(())
        }
        fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
            Ok(false)
        }
        fn get_charge_limit(&self) -> Result<u8, EcError> {
            Ok(100)
        }
        fn set_battery_care(&self, _enabled: bool) -> Result<(), EcError> {
            Ok(())
        }
        fn set_charge_limit(&self, _percent: u8) -> Result<(), EcError> {
            Err(EcError::BackendUnavailable("charge limit rejected".into()))
        }
        fn get_performance_mode(&self) -> Result<u8, EcError> {
            Ok(0x09)
        }
        fn set_performance_mode(&self, _mode: u8) -> Result<(), EcError> {
            Ok(())
        }
    }

    /// 全部写入成功、读回硬件量化值的模拟后端（模拟 WMI 85%→80% 量化）。
    struct QuantSyncBackend;

    impl EcBackend for QuantSyncBackend {
        fn name(&self) -> &'static str {
            "quant-sync"
        }
        fn read_byte(&self, _addr: u16) -> Result<u8, EcError> {
            Err(EcError::BackendUnavailable("mock".into()))
        }
        fn write_byte(&self, _addr: u16, _value: u8) -> Result<(), EcError> {
            Ok(())
        }
        fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
            Ok(true)
        }
        fn get_charge_limit(&self) -> Result<u8, EcError> {
            Ok(80)
        }
        fn set_battery_care(&self, _enabled: bool) -> Result<(), EcError> {
            Ok(())
        }
        fn set_charge_limit(&self, _percent: u8) -> Result<(), EcError> {
            Ok(())
        }
        fn get_performance_mode(&self) -> Result<u8, EcError> {
            Ok(0x09)
        }
        fn set_performance_mode(&self, _mode: u8) -> Result<(), EcError> {
            Ok(())
        }
    }

    /// 回归测试（B）：养护开启但充电上限写入失败时，启动同步不得把用户
    /// 的 care=true 静默改写为硬件读回的 false。历史实现只检查 battery_care_ok
    /// （WMI 的 set_battery_care 恒 Ok），导致应用失败被静默持久化，下次
    /// 启动还会按 care=false 强制写 100%，摧毁用户设置的充电上限。
    #[test]
    fn test_partial_write_failure_keeps_user_config() {
        let backend = ChargeLimitFailsBackend;
        let config = AppConfig {
            auto_apply_on_startup: true,
            battery_care_enabled: true,
            battery_charge_limit: 80,
            ..Default::default()
        };
        let outcome = apply_startup_config(&backend, &config);

        // 写入结果必须如实反映失败。
        assert!(!outcome.charge_limit_ok);
        assert!(outcome.battery_care_ok);
        assert!(!outcome.errors.is_empty());

        let mut cfg = config.clone();
        sync_startup_config(&backend, &mut cfg, &outcome);
        // 用户选择必须保留，不得被硬件旧状态覆盖。
        assert!(cfg.battery_care_enabled, "care must not be overwritten by readback");
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
        let backend = QuantSyncBackend;
        let config = AppConfig {
            auto_apply_on_startup: true,
            battery_care_enabled: true,
            battery_charge_limit: 85,
            ..Default::default()
        };
        let outcome = apply_startup_config(&backend, &config);
        assert!(outcome.charge_limit_ok && outcome.battery_care_ok);

        let mut cfg = config;
        sync_startup_config(&backend, &mut cfg, &outcome);
        assert!(cfg.battery_care_enabled);
        assert_eq!(cfg.battery_charge_limit, 80);
    }
}

