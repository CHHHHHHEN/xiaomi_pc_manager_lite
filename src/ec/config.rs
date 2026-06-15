use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum BackendPreference {
    #[default]
    Auto,
    WinRing0,
    Wmi,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub battery_care_enabled: bool,
    pub battery_charge_limit: u8,
    pub performance_mode: u8,
    pub auto_apply_on_startup: bool,
    pub auto_reapply_on_power_change: bool,
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
            backend: BackendPreference::Auto,
        }
    }
}

fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("XiaomiPcManagerLite")
}

fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

impl AppConfig {
    pub fn load() -> Self {
        let path = config_path();
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
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
        assert_eq!(cfg.backend, BackendPreference::Auto);
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
            backend: BackendPreference::Wmi,
        };
        let s = toml::to_string_pretty(&cfg).expect("serialize");
        let deserialized: AppConfig = toml::from_str(&s).expect("deserialize");
        assert_eq!(cfg.battery_care_enabled, deserialized.battery_care_enabled);
        assert_eq!(cfg.battery_charge_limit, deserialized.battery_charge_limit);
        assert_eq!(cfg.performance_mode, deserialized.performance_mode);
        assert_eq!(cfg.auto_apply_on_startup, deserialized.auto_apply_on_startup);
        assert_eq!(cfg.auto_reapply_on_power_change, deserialized.auto_reapply_on_power_change);
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
        assert_eq!(cfg.backend, BackendPreference::Auto);
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
        assert_eq!(cfg.backend, cloned.backend);
    }
}
