use tauri::{
    AppHandle, Manager, Runtime,
    menu::{CheckMenuItem, CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder, SubmenuBuilder, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    image::Image,
};
use crate::AppState;
use crate::ec::performance::PerfMode;

pub struct TrayState<R: Runtime> {
    pub battery_care: CheckMenuItem<R>,
}

impl<R: Runtime> TrayState<R> {
    fn new(battery_care: CheckMenuItem<R>) -> Self {
        Self { battery_care }
    }
}

pub fn setup_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let battery_care = CheckMenuItemBuilder::with_id("battery_care", "电池养护")
        .checked(false)
        .build(app)?;

    set_battery_care_checked(app, &battery_care);

    let submenu = SubmenuBuilder::new(app, "性能模式")
        .items(&[
            &MenuItemBuilder::with_id("perf_eco", "Eco").build(app)?,
            &MenuItemBuilder::with_id("perf_quiet", "Quiet").build(app)?,
            &MenuItemBuilder::with_id("perf_smart", "Smart").build(app)?,
            &MenuItemBuilder::with_id("perf_fast", "Fast").build(app)?,
            &MenuItemBuilder::with_id("perf_extreme", "Extreme").build(app)?,
        ])
        .build()?;

    let show_hide = MenuItemBuilder::with_id("show_hide", "显示/隐藏窗口").build(app)?;
    let quit = PredefinedMenuItem::quit(app, Some("退出"))?;

    let menu = MenuBuilder::new(app)
        .item(&battery_care)
        .item(&submenu)
        .separator()
        .item(&show_hide)
        .separator()
        .item(&quit)
        .build()?;

    let icon = Image::from_bytes(include_bytes!("../icons/tray_icon.ico"))?;
    let icon = icon.to_owned();

    TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .tooltip("Xiaomi PC Manager Lite")
        .on_menu_event(handle_tray_event)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button, button_state, .. } = event {
                if button == MouseButton::Left && button_state == MouseButtonState::Up {
                    let app = tray.app_handle();
                    toggle_window_visibility(app);
                }
            }
        })
        .build(app)?;

    app.manage(TrayState::new(battery_care));

    Ok(())
}

fn set_battery_care_checked(app: &AppHandle, item: &CheckMenuItem<tauri::Wry>) {
    if let Some(state) = app.try_state::<AppState>() {
        if let Ok(config) = state.config.lock() {
            let _ = item.set_checked(config.battery_care_enabled);
        }
    }
}

fn toggle_window_visibility(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if window.is_visible().unwrap_or(false) {
            let _ = window.hide();
        } else {
            let _ = window.show();
            let _ = window.set_focus();
        }
    }
}

fn handle_tray_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    let id = event.id();
    match id.as_ref() {
        "battery_care" => {
            if let Some(state) = app.try_state::<AppState>() {
                let new_state = {
                    let mut config = state.config.lock().unwrap();
                    let new_val = !config.battery_care_enabled;
                    config.battery_care_enabled = new_val;
                    if let Ok(backend) = state.backend.lock() {
                        let _ = backend.set_battery_care(new_val);
                    }
                    config.save().ok();
                    new_val
                };
                if let Some(tray) = app.try_state::<TrayState<tauri::Wry>>() {
                    let _ = tray.battery_care.set_checked(new_state);
                }
            }
        }
        "show_hide" => toggle_window_visibility(app),
        id_str if id_str.starts_with("perf_") => {
            let mode_name = id_str.strip_prefix("perf_").unwrap();
            if let Some(mode) = PerfMode::all().iter()
                .find(|m| m.name().to_lowercase() == mode_name)
            {
                let mode_val = *mode as u8;
                if let Some(state) = app.try_state::<AppState>() {
                    if let Ok(backend) = state.backend.lock() {
                        let _ = backend.set_performance_mode(mode_val);
                    }
                    if let Ok(mut config) = state.config.lock() {
                        config.performance_mode = mode_val;
                        config.save().ok();
                    }
                }
            }
        }
        _ => {}
    }
}

pub fn sync_tray_state(app: &AppHandle) {
    if let (Some(state), Some(tray)) = (
        app.try_state::<AppState>(),
        app.try_state::<TrayState<tauri::Wry>>(),
    ) {
        if let Ok(config) = state.config.lock() {
            let _ = tray.battery_care.set_checked(config.battery_care_enabled);
        }
    }
}
