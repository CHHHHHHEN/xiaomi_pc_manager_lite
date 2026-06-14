use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BackendPreference {
    Auto,
    WinRing0,
    Wmi,
}

impl Default for BackendPreference {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub battery_care_enabled: bool,
    pub battery_charge_limit: u8,
    pub performance_mode: u8,
    pub auto_apply_on_startup: bool,
    pub auto_reapply_on_power_change: bool,
    pub backend: BackendPreference,
    pub window_visible: bool,
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
            window_visible: true,
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
        std::fs::write(config_path(), s).map_err(|e| e.to_string())?;
        Ok(())
    }
}
