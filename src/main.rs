#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ec;
mod gui;
mod msg_window;
mod msg_worker;
mod embed;
mod fnkey;

use ec::backend::EcBackend;
use ec::config::AppConfig;

fn main() {
    env_logger::init();

    let (backend, init_error): (Box<dyn EcBackend>, Option<String>) =
        match ec::backend::create_backend(ec::config::BackendPreference::Auto) {
            Ok(b) => {
                log::info!("EC backend: {}", b.name());
                (b, None)
            }
            Err(e) => {
                log::error!("Failed to create EC backend: {}", e);
                (Box::new(ec::backend::NullBackend), Some(e.to_string()))
            }
        };

    let mut config = AppConfig::load();

    if init_error.is_none() {
        apply_startup_config(&*backend, &config);
    }

    config.performance_mode = backend
        .get_performance_mode()
        .unwrap_or(config.performance_mode);
    config.battery_care_enabled = backend
        .get_battery_care_enabled()
        .unwrap_or(config.battery_care_enabled);
    config.battery_charge_limit = backend
        .get_charge_limit()
        .unwrap_or(config.battery_charge_limit);
    if config.auto_apply_on_startup && init_error.is_none() {
        if let Err(e) = config.save() {
            log::warn!("save initial config: {}", e);
        }
    }

    gui::app::run_app(backend, config, init_error);
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
