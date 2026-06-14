use serde::Serialize;
use crate::ec;
use crate::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse {
    pub backend: String,
    pub available: bool,
    pub battery_care_enabled: bool,
    pub charge_limit: u8,
    pub performance_mode: u8,
}

#[tauri::command]
pub fn get_backend_name(state: tauri::State<AppState>) -> Result<String, String> {
    let backend = state.backend.lock().map_err(|e| e.to_string())?;
    Ok(backend.name().to_string())
}

#[tauri::command]
pub fn get_status(state: tauri::State<AppState>) -> Result<StatusResponse, String> {
    let backend = state.backend.lock().map_err(|e| e.to_string())?;
    Ok(StatusResponse {
        backend: backend.name().to_string(),
        available: backend.is_available(),
        battery_care_enabled: backend.get_battery_care_enabled().map_err(|e| e.to_string())?,
        charge_limit: backend.get_charge_limit().map_err(|e| e.to_string())?,
        performance_mode: backend.get_performance_mode().map_err(|e| e.to_string())?,
    })
}

#[tauri::command]
pub fn set_battery_care(state: tauri::State<AppState>, enabled: bool) -> Result<(), String> {
    let backend = state.backend.lock().map_err(|e| e.to_string())?;
    backend.set_battery_care(enabled).map_err(|e| e.to_string())?;
    let mut config = state.config.lock().map_err(|e| e.to_string())?;
    config.battery_care_enabled = enabled;
    config.save().ok();
    Ok(())
}

#[tauri::command]
pub fn set_charge_limit(state: tauri::State<AppState>, percent: u8) -> Result<(), String> {
    let backend = state.backend.lock().map_err(|e| e.to_string())?;
    let percent = percent.min(100);
    backend.set_charge_limit(percent).map_err(|e| e.to_string())?;
    let mut config = state.config.lock().map_err(|e| e.to_string())?;
    config.battery_charge_limit = percent;
    config.save().ok();
    Ok(())
}

#[tauri::command]
pub fn set_performance_mode(state: tauri::State<AppState>, mode: u8) -> Result<(), String> {
    let backend = state.backend.lock().map_err(|e| e.to_string())?;
    backend.set_performance_mode(mode).map_err(|e| e.to_string())?;
    let mut config = state.config.lock().map_err(|e| e.to_string())?;
    config.performance_mode = mode;
    config.save().ok();
    Ok(())
}

#[tauri::command]
pub fn get_config(state: tauri::State<AppState>) -> Result<ec::config::AppConfig, String> {
    let config = state.config.lock().map_err(|e| e.to_string())?;
    Ok(config.clone())
}

#[tauri::command]
pub fn save_config(state: tauri::State<AppState>, config: ec::config::AppConfig) -> Result<(), String> {
    let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
    *cfg = config;
    cfg.save().map_err(|e| e.to_string())?;
    Ok(())
}
