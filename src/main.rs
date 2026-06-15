#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod command;
mod ec;
mod embed;
mod gui;
mod tray;
mod util;

use ec::backend::EcBackend;
use ec::config::AppConfig;

fn main() {
    env_logger::init();

    let mut config = AppConfig::load();

    let (backend, init_error): (Box<dyn EcBackend>, Option<String>) =
        match ec::backend::create_backend(config.backend) {
            Ok(b) => {
                log::info!("EC backend: {} (preference: {:?})", b.name(), config.backend);
                (b, None)
            }
            Err(_) => {
                log::warn!("Configured backend {:?} unavailable; falling back to Auto", config.backend);
                match ec::backend::create_backend(ec::config::BackendPreference::Auto) {
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
        };

    apply_startup_config(&*backend, &config);

    if let Ok(mode) = backend.get_performance_mode() {
        config.performance_mode = mode;
    }
    if let Ok(enabled) = backend.get_battery_care_enabled() {
        config.battery_care_enabled = enabled;
    }
    if let Ok(limit) = backend.get_charge_limit() {
        config.battery_charge_limit = limit;
    }

    if let Err(e) = config.save() {
        log::warn!("save initial config: {}", e);
    }

    gui::run_app(backend, config, init_error);
}

fn apply_startup_config(backend: &dyn EcBackend, config: &AppConfig) {
    if config.auto_apply_on_startup {
        log::info!("Applying config on startup");
        if let Err(e) = backend.set_battery_care(config.battery_care_enabled) {
            log::warn!("apply battery care on startup: {}", e);
        }
        if let Err(e) = backend.set_charge_limit(config.battery_charge_limit) {
            log::warn!("apply charge limit on startup: {}", e);
        }
        if let Err(e) = backend.set_performance_mode(config.performance_mode) {
            log::warn!("apply perf mode on startup: {}", e);
        }
    }
}
