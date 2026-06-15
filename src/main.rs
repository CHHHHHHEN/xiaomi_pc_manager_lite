#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ec;
mod gui;
mod tray;
mod hotkey;
mod power_event;
mod msg_window;
mod embed;

use ec::backend::EcBackend;
use ec::config::AppConfig;

fn main() {
    env_logger::init();

    let backend = std::thread::spawn(|| {
        ec::backend::create_backend(ec::config::BackendPreference::Auto)
    })
    .join()
    .expect("backend init thread panicked");

    let backend = match backend {
        Ok(b) => b,
        Err(e) => {
            log::error!("Failed to create EC backend: {}", e);
            eprintln!("Fatal: EC backend initialization failed: {}", e);
            std::process::exit(1);
        }
    };
    log::info!("EC backend: {}", backend.name());

    let mut config = AppConfig::load();

    apply_startup_config(&*backend, &config);

    config.performance_mode = backend
        .get_performance_mode()
        .unwrap_or(config.performance_mode);
    config.battery_care_enabled = backend
        .get_battery_care_enabled()
        .unwrap_or(config.battery_care_enabled);
    config.battery_charge_limit = backend
        .get_charge_limit()
        .unwrap_or(config.battery_charge_limit);
    if config.auto_apply_on_startup {
        config.save().ok();
    }

    gui::app::run_app(backend, config);
}

fn apply_startup_config(backend: &dyn EcBackend, config: &AppConfig) {
    if config.auto_apply_on_startup {
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
