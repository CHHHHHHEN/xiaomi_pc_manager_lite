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

impl AppConfig {
    pub fn load() -> Self {
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
            self.battery_charge_limit = 80;
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let s = toml::to_string_pretty(self).map_err(|e| e.to_string())?;

        // Atomic write: write to temporary file, then rename (NFR-REL-04)
        let path = config_path();
        let tmp_path = path.with_extension("toml.tmp");
        std::fs::write(&tmp_path, &s).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp_path, &path).map_err(|e| e.to_string())?;

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
