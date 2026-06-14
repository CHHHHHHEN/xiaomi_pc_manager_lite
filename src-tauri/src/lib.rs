pub mod commands;
pub mod ec;
pub mod embed;
pub mod tray;
pub mod hotkey;
pub mod power_event;

use std::sync::Mutex;

pub struct AppState {
    pub backend: Mutex<Box<dyn ec::backend::EcBackend>>,
    pub config: Mutex<ec::config::AppConfig>,
}

fn apply_startup_config(backend: &dyn ec::backend::EcBackend, config: &ec::config::AppConfig) {
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

pub fn run() {
    env_logger::init();

    let backend = ec::backend::create_backend(ec::config::BackendPreference::Auto);
    let backend = match backend {
        Ok(b) => b,
        Err(e) => {
            log::error!("Failed to create EC backend: {}", e);
            panic!("EC backend required: {:?}", e);
        }
    };
    log::info!("EC backend: {}", backend.name());

    let mut config = ec::config::AppConfig::load();

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

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_shell::init())
        .manage(AppState {
            backend: Mutex::new(backend),
            config: Mutex::new(config),
        })
        .setup(|app| {
            let handle = app.handle();
            tray::setup_tray(handle).unwrap_or_else(|e| {
                log::error!("Failed to setup tray: {}", e);
            });

            let app_hotkey = app.handle().clone();
            hotkey::setup_hotkeys(app_hotkey);

            let app_power = app.handle().clone();
            power_event::start_power_monitor(app_power);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_backend_name,
            commands::get_status,
            commands::set_battery_care,
            commands::set_charge_limit,
            commands::set_performance_mode,
            commands::get_config,
            commands::save_config,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
