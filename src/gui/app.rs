use eframe::egui::{self, Color32, Frame, Margin, Vec2};
use eframe::egui::ViewportCommand;
use std::sync::{Arc, Mutex, mpsc};
use windows::Win32::System::Power::GetSystemPowerStatus;

use crate::ec;
use crate::ec::config::BackendPreference;

#[derive(Debug)]
pub enum UiCommand {
    ToggleWindow,
    Quit,
    ToggleBatteryCare,
    CyclePerfMode,
    ReapplyConfig,
}

pub struct XiaomiApp {
    pub cmd_tx: mpsc::Sender<UiCommand>,
    cmd_rx: mpsc::Receiver<UiCommand>,
    backend: Box<dyn ec::backend::EcBackend>,
    config: ec::config::AppConfig,
    current_pref: BackendPreference,
    backend_name: String,
    battery_care_enabled: bool,
    charge_limit: u8,
    performance_mode: u8,
    error_msg: Option<String>,
}

impl XiaomiApp {
    pub fn new(backend: Box<dyn ec::backend::EcBackend>, config: ec::config::AppConfig, pref: BackendPreference, init_error: Option<String>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let backend_name = backend.name().to_string();
        let battery_care_enabled = config.battery_care_enabled;
        let charge_limit = config.battery_charge_limit;
        let performance_mode = config.performance_mode;

        Self {
            cmd_tx,
            cmd_rx,
            backend,
            config,
            current_pref: pref,
            backend_name,
            battery_care_enabled,
            charge_limit,
            performance_mode,
            error_msg: init_error,
        }
    }
}

fn load_cjk_font() -> Option<(String, Vec<u8>)> {
    const CJK_FONTS: &[(&str, &str)] = &[
        ("msyh", r"C:\Windows\Fonts\msyh.ttc"),
        ("msyhbd", r"C:\Windows\Fonts\msyhbd.ttc"),
        ("simhei", r"C:\Windows\Fonts\simhei.ttf"),
        ("simsun", r"C:\Windows\Fonts\simsun.ttc"),
        ("noto-cjk", r"C:\Windows\Fonts\NotoSansCJK-Regular.ttc"),
    ];
    for (name, path) in CJK_FONTS {
        if let Ok(data) = std::fs::read(path) {
            return Some(((*name).to_owned(), data));
        }
    }
    None
}

fn load_icon_data() -> Option<egui::IconData> {
    let png_bytes = include_bytes!("../../icons/icon.png");
    let img = image::load_from_memory(png_bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(egui::IconData {
        rgba: rgba.into_raw(),
        width: w,
        height: h,
    })
}

pub fn run_app(backend: Box<dyn ec::backend::EcBackend>, config: ec::config::AppConfig, init_error: Option<String>) {
    let pref = config.backend.clone();
    let app = XiaomiApp::new(backend, config, pref, init_error);
    let cmd_tx = app.cmd_tx.clone();

    let tray_state = Arc::new(Mutex::new(crate::msg_worker::TrayState {
        battery_care_enabled: app.battery_care_enabled,
        perf_mode: app.performance_mode,
    }));

    crate::msg_worker::spawn(cmd_tx.clone(), tray_state);
    crate::fnkey::spawn(cmd_tx.clone());

    let icon = load_icon_data();
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 680.0])
            .with_min_inner_size([400.0, 500.0])
            .with_decorations(false)
            .with_resizable(true)
            .with_icon(icon.unwrap_or_default()),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "Xiaomi PC Manager Lite",
        native_options,
        Box::new(move |cc| {
            let mut fonts = egui::FontDefinitions::default();
            if let Some((name, data)) = load_cjk_font() {
                fonts.font_data.insert(name.clone(), egui::FontData::from_owned(data).into());
                if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                    family.insert(0, name.clone());
                }
                if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                    family.insert(0, name);
                }
            } else {
                log::warn!("No CJK font found; UI may show boxes for CJK characters");
            }
            cc.egui_ctx.set_fonts(fonts);
            Ok(Box::new(app))
        }),
    );
}

impl XiaomiApp {
    fn process_commands(&mut self, ctx: &egui::Context) {
        let mut needs_repaint = false;
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            needs_repaint = true;
            match cmd {
                UiCommand::ToggleWindow => {
                    let visible = ctx.viewport(|vp| vp.builder.visible.unwrap_or(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(!visible));
                }
                UiCommand::Quit => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                UiCommand::ToggleBatteryCare => {
                    let new_val = !self.battery_care_enabled;
                    match self.backend.set_battery_care(new_val) {
                        Ok(_) => log::info!("Battery care set to {}", if new_val { "enabled" } else { "disabled" }),
                        Err(e) => log::error!("Failed to set battery care: {}", e),
                    }
                    self.config.battery_care_enabled = new_val;
                    self.battery_care_enabled = new_val;
                    self.save_state();
                }
                UiCommand::CyclePerfMode => {
                    // 3-mode cycle: Smart(0x09) → Quiet(0x02) → Extreme → Smart ...
                    // Extreme = 0x04 (Beast) when plugged, 0x03 (Fast) on battery
                    const CYCLE: [u8; 3] = [0x09, 0x02, 0x04]; // Smart, Quiet, Extreme
                    let current = self.performance_mode;
                    let next_raw = if current == CYCLE[0] {
                        CYCLE[1]
                    } else if current == CYCLE[1] {
                        CYCLE[2]
                    } else {
                        CYCLE[0]
                    };
                    let ac_online = ac_power_status();
                    let next_val = if next_raw == 0x04 && !ac_online {
                        0x03 // Fast on battery
                    } else {
                        next_raw
                    };
                    let mode_name = ec::performance::PerfMode::from_ec_value(next_val)
                        .map(|m| m.name())
                        .unwrap_or("未知");
                    match self.backend.set_performance_mode(next_val) {
                        Ok(_) => log::info!("Performance mode set to {} ({:#x})", mode_name, next_val),
                        Err(e) => log::error!("Failed to set performance mode: {}", e),
                    }
                    self.config.performance_mode = next_val;
                    self.performance_mode = next_val;
                    self.save_state();
                }
                UiCommand::ReapplyConfig => {
                    if self.config.auto_reapply_on_power_change {
                        log::info!("Reapplying config on power change");
                        if let Err(e) = self.backend.set_battery_care(self.config.battery_care_enabled) {
                            log::error!("Reapply battery care: {}", e);
                        }
                        if let Err(e) = self.backend.set_charge_limit(self.config.battery_charge_limit) {
                            log::error!("Reapply charge limit: {}", e);
                        }
                        if let Err(e) = self.backend.set_performance_mode(self.config.performance_mode) {
                            log::error!("Reapply perf mode: {}", e);
                        }
                        self.refresh_from_backend();
                    }
                }
            }
        }
        if needs_repaint {
            ctx.request_repaint();
        }
    }

    fn try_switch_backend(&mut self, pref: BackendPreference) -> bool {
        match ec::backend::create_backend(pref.clone()) {
            Ok(new_backend) => {
                log::info!("Switched EC backend to: {}", new_backend.name());
                self.backend = new_backend;
                self.backend_name = self.backend.name().to_string();
                self.current_pref = pref;
                self.config.backend = self.current_pref.clone();
                if let Err(e) = self.config.save() {
                    log::error!("save config: {}", e);
                }
                self.refresh_from_backend();
                self.error_msg = None;
                true
            }
            Err(e) => {
                log::error!("Failed to switch EC backend: {}", e);
                self.error_msg = Some(format!("后端切换失败: {}", e));
                false
            }
        }
    }

    fn refresh_from_backend(&mut self) {
        if let Ok(mode) = self.backend.get_performance_mode() {
            self.performance_mode = mode;
        }
        if let Ok(enabled) = self.backend.get_battery_care_enabled() {
            self.battery_care_enabled = enabled;
        }
        if let Ok(limit) = self.backend.get_charge_limit() {
            self.charge_limit = limit;
        }
    }

    /// Persist current in-memory state to disk and push it to the tray icon.
    fn save_state(&self) {
        if let Err(e) = self.config.save() {
            log::error!("save config: {}", e);
        }
        if let Some(state) = crate::msg_worker::TRAY_STATE.get() {
            if let Ok(mut s) = state.lock() {
                s.battery_care_enabled = self.battery_care_enabled;
                s.perf_mode = self.performance_mode;
            }
        }
    }
}

impl eframe::App for XiaomiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_commands(ctx);
        ctx.request_repaint_after(std::time::Duration::from_secs(5));

        // Title bar
        egui::TopBottomPanel::top("title_bar")
            .frame(Frame {
                fill: Color32::from_rgb(0x25, 0x50, 0xAA),
                inner_margin: Margin::symmetric(8, 4),
                ..Default::default()
            })
            .show(ctx, |ui| {
                let total_rect = ui.available_rect_before_wrap();
                let button_strip_width = 96.0_f32;
                let title_rect = egui::Rect::from_min_max(
                    total_rect.min,
                    egui::pos2(
                        (total_rect.max.x - button_strip_width).max(total_rect.min.x),
                        total_rect.max.y,
                    ),
                );
                let button_strip_rect = egui::Rect::from_min_max(
                    egui::pos2(
                        (total_rect.max.x - button_strip_width).max(total_rect.min.x),
                        total_rect.min.y,
                    ),
                    total_rect.max,
                );

                let title_drag = ui.interact(
                    title_rect,
                    ui.id().with("title_bar_drag"),
                    egui::Sense::click_and_drag(),
                );
                if title_drag.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if title_drag.double_clicked() {
                    let is_maximized =
                        ctx.viewport(|v| v.builder.maximized.unwrap_or(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!is_maximized));
                }

                ui.painter().text(
                    title_rect.left_center() + egui::vec2(4.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    "Xiaomi PC Manager Lite",
                    egui::FontId::proportional(14.0),
                    Color32::WHITE,
                );

                ui.allocate_new_ui(
                    egui::UiBuilder::new()
                        .max_rect(button_strip_rect)
                        .layout(egui::Layout::right_to_left(egui::Align::Center)),
                    |ui| {
                        if ui
                            .button(
                                egui::RichText::new("✕").color(Color32::WHITE).size(12.0),
                            )
                            .clicked()
                        {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                        }
                        let is_maximized =
                            ctx.viewport(|v| v.builder.maximized.unwrap_or(false));
                        let max_icon = if is_maximized { "❐" } else { "□" };
                        if ui
                            .button(
                                egui::RichText::new(max_icon).color(Color32::WHITE).size(12.0),
                            )
                            .clicked()
                        {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!is_maximized));
                        }
                        if ui
                            .button(
                                egui::RichText::new("─").color(Color32::WHITE).size(12.0),
                            )
                            .clicked()
                        {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                        }
                    },
                );
            });

        // Content
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.show_main_view(ui);
            });
        });

        // Resize handle
        egui::TopBottomPanel::bottom("resize_handle")
            .min_height(0.0)
            .show_separator_line(false)
            .frame(Frame {
                fill: Color32::TRANSPARENT,
                inner_margin: Margin::symmetric(0, 0),
                ..Default::default()
            })
            .show(ctx, |ui| {
                let height = 14.0;
                let (_id, rect) = ui.allocate_space(egui::vec2(ui.available_width(), height));
                let handle_size = 14.0;
                let corner = rect.right_bottom();
                let handle_rect = egui::Rect::from_min_size(
                    egui::pos2(corner.x - handle_size, corner.y - handle_size),
                    egui::vec2(handle_size, handle_size),
                );
                let resize_id = ui.next_auto_id();
                let resize_resp = ui.interact(handle_rect, resize_id, egui::Sense::drag());
                if resize_resp.dragged() {
                    let delta = resize_resp.drag_delta();
                    let s = ctx.screen_rect().size();
                    let new = egui::vec2((s.x + delta.x).max(400.0), (s.y + delta.y).max(500.0));
                    ctx.send_viewport_cmd(ViewportCommand::InnerSize(new));
                }
                if resize_resp.hovered() || resize_resp.dragged() {
                    ctx.set_cursor_icon(egui::CursorIcon::ResizeSouthEast);
                }
                let painter = ui.painter();
                let p = corner;
                for i in 0..3 {
                    let off = (i as f32) * 4.0;
                    painter.line_segment(
                        [egui::pos2(p.x - off - 2.0, p.y), egui::pos2(p.x, p.y - off - 2.0)],
                        egui::Stroke::new(2.0, Color32::from_gray(140)),
                    );
                }
            });
    }
}

impl XiaomiApp {
    fn show_main_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("状态");
            if ui
                .button(egui::RichText::new("刷新").size(13.0))
                .on_hover_text("重新读取后端状态")
                .clicked()
            {
                self.refresh_from_backend();
                self.error_msg = None;
            }
        });
        ui.horizontal(|ui| {
            ui.label("后端:");
            ui.colored_label(Color32::from_rgb(0x25, 0x50, 0xAA), &self.backend_name);
        });
        ui.horizontal(|ui| {
            let status = if self.battery_care_enabled { "开启" } else { "关闭" };
            ui.label(
                egui::RichText::new(format!("电池养护: {}", status)).strong(),
            );
            if !self.battery_care_enabled {
                ui.colored_label(Color32::GRAY, "(充电至100%)");
            }
        });
        ui.horizontal(|ui| {
            ui.label(format!("充电上限: {}%", self.charge_limit));
        });
        let perf_name = ec::performance::PerfMode::from_ec_value(self.performance_mode)
            .map(|m| m.name())
            .unwrap_or("未知");
        ui.horizontal(|ui| {
            ui.label("性能模式: ");
            ui.colored_label(Color32::from_rgb(0x25, 0x50, 0xAA), perf_name);
        });
        if let Some(err) = &self.error_msg {
            ui.colored_label(Color32::RED, err);
        }

        ui.separator();
        ui.add_space(8.0);

        // Battery care
        ui.heading("电池养护");
        ui.horizontal(|ui| {
            let mut enabled = self.battery_care_enabled;
            if ui.checkbox(&mut enabled, "启用电池养护").changed() {
                self.battery_care_enabled = enabled;
                self.config.battery_care_enabled = enabled;
                match self.backend.set_battery_care(enabled) {
                    Ok(_) => log::info!("Battery care {}", if enabled { "enabled" } else { "disabled" }),
                    Err(e) => log::error!("Failed to set battery care: {}", e),
                }
                self.save_state();
            }
        });
        if self.battery_care_enabled {
            let mut limit = self.charge_limit as f32;
            ui.horizontal(|ui| {
                ui.label("充电上限:");
                if ui
                    .add(egui::Slider::new(&mut limit, 40.0..=100.0).step_by(1.0).suffix("%"))
                    .changed()
                {
                    let new_limit = limit.round() as u8;
                    self.charge_limit = new_limit;
                    self.config.battery_charge_limit = new_limit;
                    match self.backend.set_charge_limit(new_limit) {
                        Ok(_) => log::info!("Charge limit set to {}%", new_limit),
                        Err(e) => log::error!("Failed to set charge limit: {}", e),
                    }
                    self.save_state();
                }
            });
        }

        ui.separator();
        ui.add_space(8.0);

        // Performance mode
        ui.heading("性能模式");
        let modes = ec::performance::PerfMode::all();
        let ncols = 3;
        egui::Grid::new("perf_grid")
            .min_col_width(100.0)
            .max_col_width(140.0)
            .spacing([8.0, 8.0])
            .show(ui, |ui| {
                for (i, mode) in modes.iter().enumerate() {
                    let val = *mode as u8;
                    let is_selected = val == self.performance_mode;
                    let btn = egui::Button::new(egui::RichText::new(mode.name()).size(14.0))
                        .min_size(Vec2::new(100.0, 36.0))
                        .fill(if is_selected {
                            Color32::from_rgb(0x25, 0x50, 0xAA)
                        } else {
                            Color32::from_gray(220)
                        })
                        .stroke(egui::Stroke::new(
                            1.0,
                            if is_selected {
                                Color32::from_rgb(0x1A, 0x3C, 0x80)
                            } else {
                                Color32::from_gray(180)
                            },
                        ))
                        .corner_radius(6);

                    if ui.add(btn).clicked() {
                        self.performance_mode = val;
                        self.config.performance_mode = val;
                        match self.backend.set_performance_mode(val) {
                            Ok(_) => log::info!("Performance mode set to {} ({:#x})", mode.name(), val),
                            Err(e) => log::error!("Failed to set performance mode: {}", e),
                        }
                        self.save_state();
                    }

                    if (i + 1) % ncols == 0 {
                        ui.end_row();
                    }
                }
            });

        ui.separator();
        ui.add_space(8.0);

        // Settings
        ui.heading("设置");
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.label("EC 后端偏好:");
            let mut pref = self.current_pref.clone();
            let changed = ui
                .radio_value(&mut pref, BackendPreference::Auto, "自动")
                .changed()
                | ui
                    .radio_value(&mut pref, BackendPreference::Wmi, "WMI")
                    .changed()
                | ui
                    .radio_value(&mut pref, BackendPreference::WinRing0, "WinRing0")
                    .changed();
            if changed && pref != self.current_pref {
                self.try_switch_backend(pref);
                self.refresh_from_backend();
            }
        });

        ui.add_space(8.0);

        let mut auto = self.config.auto_apply_on_startup;
        if ui.checkbox(&mut auto, "启动时自动应用设置").changed() {
            self.config.auto_apply_on_startup = auto;
            if let Err(e) = self.config.save() {
                log::error!("save config: {}", e);
            }
        }

        let mut reapply = self.config.auto_reapply_on_power_change;
        if ui.checkbox(&mut reapply, "电源切换时自动重设").changed() {
            self.config.auto_reapply_on_power_change = reapply;
            if let Err(e) = self.config.save() {
                log::error!("save config: {}", e);
            }
        }

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("版本 0.2.0")
                .color(Color32::GRAY)
                .size(11.0),
        );
    }
}

fn ac_power_status() -> bool {
    let mut status = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemPowerStatus(&mut status).is_ok() } {
        status.ACLineStatus == 1
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ui_command_debug() {
        assert_eq!(format!("{:?}", UiCommand::ToggleWindow), "ToggleWindow");
        assert_eq!(format!("{:?}", UiCommand::Quit), "Quit");
        assert_eq!(format!("{:?}", UiCommand::ToggleBatteryCare), "ToggleBatteryCare");
        assert_eq!(format!("{:?}", UiCommand::CyclePerfMode), "CyclePerfMode");
        assert_eq!(format!("{:?}", UiCommand::ReapplyConfig), "ReapplyConfig");
    }

    #[test]
    fn test_xiaomi_app_new_with_defaults() {
        let backend = Box::new(crate::ec::backend::NullBackend);
        let config = crate::ec::config::AppConfig::default();
        let app = XiaomiApp::new(backend, config, crate::ec::config::BackendPreference::Auto, None);

        assert_eq!(app.backend_name, "无后端");
        assert!(!app.battery_care_enabled);
        assert_eq!(app.charge_limit, 80);
        assert_eq!(app.performance_mode, 0x09);
        assert!(app.error_msg.is_none());
    }

    #[test]
    fn test_xiaomi_app_new_with_custom_config() {
        let backend = Box::new(crate::ec::backend::NullBackend);
        let config = crate::ec::config::AppConfig {
            battery_care_enabled: true,
            battery_charge_limit: 60,
            performance_mode: 0x02,
            ..Default::default()
        };
        let app = XiaomiApp::new(
            backend,
            config,
            crate::ec::config::BackendPreference::Wmi,
            Some("初始化失败".into()),
        );

        assert!(app.battery_care_enabled);
        assert_eq!(app.charge_limit, 60);
        assert_eq!(app.performance_mode, 0x02);
        assert_eq!(app.current_pref, crate::ec::config::BackendPreference::Wmi);
        assert_eq!(app.error_msg.as_deref(), Some("初始化失败"));
    }

    #[test]
    fn test_xiaomi_app_new_with_backend_error() {
        let backend = Box::new(crate::ec::backend::NullBackend);
        let config = crate::ec::config::AppConfig::default();
        let app = XiaomiApp::new(
            backend,
            config,
            crate::ec::config::BackendPreference::Auto,
            Some("后端不可用".into()),
        );

        assert_eq!(app.error_msg.as_deref(), Some("后端不可用"));
    }

    #[test]
    fn test_cycle_perf_mode_internal_const() {
        let cycle: [u8; 3] = [0x09, 0x02, 0x04];
        assert_eq!(cycle.len(), 3);
        assert_eq!(cycle[0], 0x09); // Smart
        assert_eq!(cycle[1], 0x02); // Quiet
        assert_eq!(cycle[2], 0x04); // Extreme / Beast
    }

    #[test]
    fn test_xiaomi_app_send() {
        fn assert_send<T: Send>() {}
        assert_send::<XiaomiApp>();
    }
}
