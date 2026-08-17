use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::ec::performance::PerfMode;

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum BackendPreference {
    #[default]
    Auto,
    WinRing0,
    Wmi,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub battery_care_enabled: bool,
    pub battery_charge_limit: u8,
    pub performance_mode: u8,
    pub auto_apply_on_startup: bool,
    pub auto_reapply_on_power_change: bool,
    pub auto_start_on_boot: bool,
    pub backend: BackendPreference,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            battery_care_enabled: false,
            battery_charge_limit: 80,
            performance_mode: 0x09,
            auto_apply_on_startup: true,
            auto_reapply_on_power_change: true,
            auto_start_on_boot: false,
            // 默认使用 WMI 后端（本机 2025 RedmiBook Pro 14 实测可用；
            // Auto 模式同样 WMI 优先）。
            backend: BackendPreference::Wmi,
        }
    }
}

fn config_dir() -> PathBuf {
    // Overridable for tests so saving state never touches the real config.
    std::env::var_os("XIAOMI_PC_MANAGER_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("XiaomiPcManagerLite")
        })
}

fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// 清理 `save()` 崩溃后残留的临时文件（形如 `config.toml.<pid>.<seq>.tmp`）。
///
/// 原子保存先写唯一临时文件再 rename；若进程在两步之间崩溃（断电/强杀），
/// 临时文件会永久残留在配置目录。每次加载时清理由本进程在**启动阶段**执行：
/// 此时尚无并发保存者，唯一命名的临时文件不可能属于一个正在进行中的写入，
/// 删除是安全的。测试用目录同样受益（崩溃残留不会跨启动累积）。
fn cleanup_stale_tmp_files() {
    let dir = config_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("config.toml.") && name.ends_with(".tmp") {
            log::debug!("Config: removing stale temp file {}", name);
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 测试专用：串行化所有读写全局 `XIAOMI_PC_MANAGER_CONFIG_DIR` 的用例。
/// cargo test 并行运行多个用例时，各用例对同一环境变量的 `set_var` 会互相
/// 覆盖——读取配置路径的用例（如 test_load_regenerates_missing_config_file、
/// test_concurrent_saves_are_atomic）可能在 `set_var` 之后、读取磁盘之前
/// 被其它用例改写环境变量，读到错误的目录而失败（flaky）。读取者须在
/// 整个用例生命周期持有此锁；仅设置环境变量的写入者（gui::commands 的
/// redirect_config_dir）在 set_var 时短暂持有即可。
#[cfg(test)]
pub(crate) static CONFIG_DIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl AppConfig {
    pub fn load() -> Self {
        // 清理上次崩溃留下的临时文件（见 cleanup_stale_tmp_files）。
        cleanup_stale_tmp_files();
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(s) => match toml::from_str::<AppConfig>(&s) {
                Ok(mut cfg) => {
                    let before = cfg.clone();
                    cfg.sanitize();
                    // 消毒修改了配置（损坏/手改值被修正）时立即落盘：否则
                    // auto_apply_on_startup=false 时 main.rs 不会保存，磁盘上
                    // 的损坏值会每次启动重复告警、永不修复（仅在内存中被纠正）。
                    if cfg != before {
                        if let Err(e) = cfg.save() {
                            log::warn!("Persist sanitized config: {}", e);
                        }
                    }
                    cfg
                }
                Err(e) => {
                    log::warn!("Config parse error at {:?}: {}; using defaults", path, e);
                    AppConfig::default()
                }
            },
            // AC-CFG-02：配置文件缺失（首次运行/被删除）时返回默认值，并
            // 立即落盘重建——否则用户删除 config.toml 后重启应用文件不会
            // 重新生成，直到下一次 GUI 修改设置才被创建。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = AppConfig::default();
                if let Err(e) = cfg.save() {
                    log::warn!("Persist default config: {}", e);
                }
                cfg
            }
            Err(e) => {
                log::warn!("Config load error at {:?}: {}; using defaults", path, e);
                AppConfig::default()
            }
        }
    }

    /// 规范化从磁盘读入的配置，防止手改/损坏的配置把垃圾值写入 EC 硬件：
    /// - `performance_mode` 必须是已知模式，否则回退默认（智能 0x09）。
    ///   历史版本曾把配置里的任意字节（如 0xFF）直接 `set_performance_mode`
    ///   写到 EC 寄存器 0x68，向硬件写入未定义代码。
    /// - `battery_charge_limit` 越界时夹紧到 [0, 100]；0 视为未设置回退 80
    ///   （把 0 写入充电上限寄存器几乎总是错误）。
    /// - 养护开启但上限 ≥100（矛盾组合）时，统一兜底为 80%——与 GUI 切换
    ///   路径（set_battery_care_internal）和启动应用路径（apply_startup_config）
    ///   的规则一致，避免"开着养护却 100%"的配置在启动时被静默改写。
    fn sanitize(&mut self) {
        if PerfMode::from_ec_value(self.performance_mode).is_none() {
            log::warn!(
                "Config: invalid performance_mode {:#x}; resetting to {}",
                self.performance_mode,
                PerfMode::Smart.name()
            );
            self.performance_mode = PerfMode::Smart as u8;
        }
        if self.battery_charge_limit == 0 {
            log::warn!("Config: charge limit 0 is invalid; using default 80%");
            self.battery_charge_limit = 80;
        } else if self.battery_charge_limit > 100 {
            log::warn!(
                "Config: charge limit {}% out of range; clamping to 100%",
                self.battery_charge_limit
            );
            self.battery_charge_limit = 100;
        }
        if self.battery_care_enabled && self.battery_charge_limit >= 100 {
            log::warn!(
                "Config: battery care on with limit {}% (incoherent); using 80%",
                self.battery_charge_limit
            );
        }
        self.battery_charge_limit = crate::ec::battery::coherent_charge_limit(
            self.battery_care_enabled,
            self.battery_charge_limit,
        );
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let s = toml::to_string_pretty(self).map_err(|e| e.to_string())?;

        // Atomic write: write to a temporary file, then rename (NFR-REL-04)。
        // 临时文件名必须**唯一**（pid + 进程内自增序号）：固定名（如
        // config.toml.tmp）在并发保存时存在撕裂重命名风险——写入者 A 完成
        // write 后、rename 前，写入者 B 对同一 tmp 文件重新 truncate+write，
        // A 的 rename 会把 B 尚未写完的 tmp 改名成 config，最终配置文件内容
        // 被撕裂（下轮启动解析失败回退默认）。唯一名 + 原子 rename 保证目标
        // 文件永远是某一次完整写入的产物。清理：write/rename 失败时删除
        // 本次残留的临时文件。
        let path = config_path();
        static SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SAVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp_path = path.with_extension(format!("toml.{}.{}.tmp", std::process::id(), seq));
        if let Err(e) = std::fs::write(&tmp_path, &s) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e.to_string());
        }
        if let Err(e) = std::fs::rename(&tmp_path, &path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e.to_string());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_values() {
        let cfg = AppConfig::default();
        assert!(!cfg.battery_care_enabled);
        assert_eq!(cfg.battery_charge_limit, 80);
        assert_eq!(cfg.performance_mode, 0x09);
        assert!(cfg.auto_apply_on_startup);
        assert!(cfg.auto_reapply_on_power_change);
        assert!(!cfg.auto_start_on_boot);
        assert_eq!(cfg.backend, BackendPreference::Wmi);
    }

    #[test]
    fn test_backend_preference_default() {
        assert_eq!(BackendPreference::default(), BackendPreference::Auto);
    }

    #[test]
    fn test_serialization_roundtrip_all_fields() {
        let cfg = AppConfig {
            battery_care_enabled: true,
            battery_charge_limit: 60,
            performance_mode: 0x02,
            auto_apply_on_startup: false,
            auto_reapply_on_power_change: false,
            auto_start_on_boot: true,
            backend: BackendPreference::Wmi,
        };
        let s = toml::to_string_pretty(&cfg).expect("serialize");
        let deserialized: AppConfig = toml::from_str(&s).expect("deserialize");
        assert_eq!(cfg.battery_care_enabled, deserialized.battery_care_enabled);
        assert_eq!(cfg.battery_charge_limit, deserialized.battery_charge_limit);
        assert_eq!(cfg.performance_mode, deserialized.performance_mode);
        assert_eq!(cfg.auto_apply_on_startup, deserialized.auto_apply_on_startup);
        assert_eq!(cfg.auto_reapply_on_power_change, deserialized.auto_reapply_on_power_change);
        assert_eq!(cfg.auto_start_on_boot, deserialized.auto_start_on_boot);
        assert_eq!(cfg.backend, deserialized.backend);
    }

    #[test]
    fn test_serialization_backend_preference_auto() {
        let cfg = AppConfig {
            backend: BackendPreference::Auto,
            ..Default::default()
        };
        let s = toml::to_string_pretty(&cfg).expect("serialize");
        let deserialized: AppConfig = toml::from_str(&s).expect("deserialize");
        assert_eq!(deserialized.backend, BackendPreference::Auto);
    }

    #[test]
    fn test_serialization_backend_preference_winring0() {
        let cfg = AppConfig {
            backend: BackendPreference::WinRing0,
            ..Default::default()
        };
        let s = toml::to_string_pretty(&cfg).expect("serialize");
        let deserialized: AppConfig = toml::from_str(&s).expect("deserialize");
        assert_eq!(deserialized.backend, BackendPreference::WinRing0);
    }

    #[test]
    fn test_deserialize_invalid_toml_returns_error() {
        let result: Result<AppConfig, _> = toml::from_str("invalid toml content {{{}");
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_partial_content_uses_defaults() {
        let s = r#"battery_care_enabled = true"#;
        let cfg: AppConfig = toml::from_str(s).expect("deserialize partial");
        assert!(cfg.battery_care_enabled);
        assert_eq!(cfg.battery_charge_limit, 80);
        assert_eq!(cfg.performance_mode, 0x09);
        assert!(cfg.auto_apply_on_startup);
        assert!(cfg.auto_reapply_on_power_change);
        assert!(!cfg.auto_start_on_boot);
        assert_eq!(cfg.backend, BackendPreference::Wmi);
    }

    #[test]
    fn test_serialization_contains_all_fields() {
        let cfg = AppConfig::default();
        let s = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(s.contains("battery_care_enabled"));
        assert!(s.contains("battery_charge_limit"));
        assert!(s.contains("performance_mode"));
        assert!(s.contains("auto_apply_on_startup"));
        assert!(s.contains("auto_reapply_on_power_change"));
        assert!(s.contains("auto_start_on_boot"));
        assert!(s.contains("backend"));
    }

    #[test]
    fn test_debug_impl() {
        let cfg = AppConfig::default();
        let debug = format!("{:?}", cfg);
        assert!(debug.contains("battery_care_enabled"));
        assert!(debug.contains("performance_mode"));
    }

    #[test]
    fn test_clone_impl() {
        let cfg = AppConfig::default();
        let cloned = cfg.clone();
        assert_eq!(cfg.battery_care_enabled, cloned.battery_care_enabled);
        assert_eq!(cfg.battery_charge_limit, cloned.battery_charge_limit);
        assert_eq!(cfg.performance_mode, cloned.performance_mode);
        assert_eq!(cfg.auto_apply_on_startup, cloned.auto_apply_on_startup);
        assert_eq!(cfg.auto_reapply_on_power_change, cloned.auto_reapply_on_power_change);
        assert_eq!(cfg.auto_start_on_boot, cloned.auto_start_on_boot);
        assert_eq!(cfg.backend, cloned.backend);
    }

    /// 回归测试：手改/损坏的配置文件不得把未知性能模式（如 0xFF）写入 EC
    /// 硬件——启动应用/电源重设路径会原样 set_performance_mode。加载时必须
    /// 回退到已知模式（智能 0x09）。
    #[test]
    fn test_sanitize_resets_invalid_performance_mode() {
        let mut cfg = AppConfig {
            performance_mode: 0xFF,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.performance_mode, 0x09);

        let mut cfg = AppConfig {
            performance_mode: 0x00,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.performance_mode, 0x09);

        // 合法模式不受影响。
        let mut cfg = AppConfig {
            performance_mode: 0x02,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.performance_mode, 0x02);
    }

    /// 越界充电上限必须在加载时夹紧，否则会被原样写进 EC 寄存器。
    #[test]
    fn test_sanitize_clamps_charge_limit() {
        let mut cfg = AppConfig {
            battery_charge_limit: 200,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.battery_charge_limit, 100);

        // 0 视为未设置，回退默认 80%。
        let mut cfg = AppConfig {
            battery_charge_limit: 0,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.battery_charge_limit, 80);
    }

    /// 养护开启但上限 100%（矛盾组合）必须在加载时兜底为 80%，与 GUI/启动
    /// 应用路径的规则一致，防止矛盾配置在磁盘上反复"复活"。
    #[test]
    fn test_sanitize_normalizes_incoherent_care_and_limit() {
        let mut cfg = AppConfig {
            battery_care_enabled: true,
            battery_charge_limit: 100,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.battery_charge_limit, 80);
        assert!(cfg.battery_care_enabled);

        // 养护关闭 + 100% 上限是合法组合，不得改动。
        let mut cfg = AppConfig {
            battery_care_enabled: false,
            battery_charge_limit: 100,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.battery_charge_limit, 100);
    }

    /// 回归测试（AC-CFG-02）：配置文件缺失时 `load()` 必须立即重建默认配置
    /// 并落盘——历史实现只在文件存在时返回默认值，删除 config.toml 后重启
    /// 应用文件不会重新生成，直到下一次修改设置才被创建。
    #[test]
    fn test_load_regenerates_missing_config_file() {
        let _config_lock = CONFIG_DIR_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("xmpl-cfg-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("XIAOMI_PC_MANAGER_CONFIG_DIR", &dir);

        // 文件不存在：load 返回默认值并重新生成文件。
        let cfg = AppConfig::load();
        assert_eq!(cfg.battery_charge_limit, 80);
        assert!(path.exists(), "missing config file must be regenerated");

        // 重建的文件可被重新加载。
        let reloaded = AppConfig::load();
        assert_eq!(reloaded, cfg);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 并发保存的原子性（NFR-REL-04）：多线程同时 save() 时，目标
    /// config.toml 必须始终是某一次完整写入的产物——可被成功解析，
    /// 且最终不留任何临时文件残留。
    ///
    /// 回归测试（历史实现）：固定名 tmp（config.toml.tmp）在并发保存时，
    /// 写入者 A 完成 write 后、rename 前，写入者 B 对同一 tmp 重新
    /// truncate+write，A 的 rename 会把 B 尚未写完的 tmp 改名成 config，
    /// 导致配置文件内容撕裂、下轮启动解析失败回退默认值。修复后每个
    /// 保存使用唯一 tmp 名（pid+自增序号），该竞态从根上消失。
    #[test]
    fn test_concurrent_saves_are_atomic() {
        let _config_lock = CONFIG_DIR_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("xmpl-save-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("XIAOMI_PC_MANAGER_CONFIG_DIR", &dir);

        const THREADS: usize = 8;
        std::thread::scope(|s| {
            for i in 0..THREADS {
                s.spawn(move || {
                    let cfg = AppConfig {
                        battery_care_enabled: i % 2 == 0,
                        battery_charge_limit: (40 + i as u8 * 8) % 101,
                        ..Default::default()
                    };
                    // 多轮写：放大并发窗口。
                    for _ in 0..20 {
                        cfg.save().expect("save must succeed");
                    }
                });
            }
        });

        // 目标文件必须可解析（未撕裂）。
        let path = config_path();
        let content = std::fs::read_to_string(&path).expect("config must exist");
        let parsed: AppConfig = toml::from_str(&content).expect("config must parse");
        assert!(
            parsed.battery_charge_limit <= 100,
            "parsed limit must be valid"
        );

        // 不得残留任何临时文件。
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("read config dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "config.toml")
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temp files may remain after concurrent saves: {:?}",
            leftovers
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 崩溃残留清理：模拟"原子保存写临时文件后、rename 前进程崩溃"的残留
    /// （`config.toml.<pid>.<seq>.tmp`），`load()` 必须将其清除——否则每次
    /// 崩溃都留下一个文件，永久累积在配置目录。
    #[test]
    fn test_load_cleans_stale_tmp_files() {
        let _config_lock = CONFIG_DIR_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("xmpl-tmp-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("XIAOMI_PC_MANAGER_CONFIG_DIR", &dir);
        std::fs::create_dir_all(&dir).expect("create config dir");

        // 模拟两次崩溃残留 + 一个正常运行遗留的临时文件。
        std::fs::write(dir.join("config.toml.1234.0.tmp"), "partial").expect("write stale 1");
        std::fs::write(dir.join("config.toml.1234.1.tmp"), "partial").expect("write stale 2");

        // 不在清理范围内的其它文件不得被误删。
        std::fs::write(dir.join("config.toml"), "should survive").expect("write config");

        AppConfig::load();

        assert!(
            !dir.join("config.toml.1234.0.tmp").exists(),
            "stale temp file must be removed"
        );
        assert!(
            !dir.join("config.toml.1234.1.tmp").exists(),
            "stale temp file must be removed"
        );
        assert!(
            dir.join("config.toml").exists(),
            "config file itself must never be touched"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 消毒对合法配置必须是幂等且无副作用的。
    #[test]
    fn test_sanitize_idempotent_on_valid_config() {
        let cfg = AppConfig {
            battery_care_enabled: true,
            battery_charge_limit: 80,
            performance_mode: 0x09,
            ..Default::default()
        };
        let mut once = cfg.clone();
        once.sanitize();
        let mut twice = once.clone();
        twice.sanitize();
        assert_eq!(once.battery_charge_limit, twice.battery_charge_limit);
        assert_eq!(once.performance_mode, twice.performance_mode);
        assert_eq!(cfg.battery_charge_limit, once.battery_charge_limit);
        assert_eq!(cfg.performance_mode, once.performance_mode);
    }
}
