use eframe::egui::{self, Color32, Frame, Margin, Vec2};
use std::sync::{Arc, Mutex, mpsc};

use crate::ec;

#[derive(Debug)]
pub enum UiCommand {
    ToggleWindow,
    ShowWindow,
    Quit,
    ToggleBatteryCare,
    CyclePerfMode,
    SetPerfMode(u8),
    RefreshStatus,
    ReapplyConfig,
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Power,
    Settings,
}

pub struct XiaomiApp {
    pub cmd_tx: mpsc::Sender<UiCommand>,
    cmd_rx: mpsc::Receiver<UiCommand>,
    backend: Box<dyn ec::backend::EcBackend>,
    config: ec::config::AppConfig,
    // Display cache
    backend_name: String,
    battery_care_enabled: bool,
    charge_limit: u8,
    performance_mode: u8,
    error_msg: Option<String>,
    active_tab: Tab,
}

impl XiaomiApp {
    pub fn new(backend: Box<dyn ec::backend::EcBackend>, config: ec::config::AppConfig) -> Self {
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
            backend_name,
            battery_care_enabled,
            charge_limit,
            performance_mode,
            error_msg: None,
            active_tab: Tab::Power,
        }
    }
}

pub fn run_app(backend: Box<dyn ec::backend::EcBackend>, config: ec::config::AppConfig) {
    let app = XiaomiApp::new(backend, config);
    let cmd_tx = app.cmd_tx.clone();

    let tray_state = Arc::new(Mutex::new(crate::tray::TrayState {
        battery_care_enabled: app.battery_care_enabled,
        perf_mode: app.performance_mode,
    }));

    crate::hotkey::setup_hotkeys(cmd_tx.clone());
    crate::power_event::start_power_monitor(cmd_tx.clone());
    crate::tray::setup_tray(cmd_tx, tray_state);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 680.0])
            .with_min_inner_size([400.0, 500.0])
            .with_decorations(false),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "Xiaomi PC Manager Lite",
        native_options,
        Box::new(move |_cc| Ok(Box::new(app))),
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
                UiCommand::ShowWindow => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                }
                UiCommand::Quit => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                UiCommand::ToggleBatteryCare => {
                    let new_val = !self.battery_care_enabled;
                    let _ = self.backend.set_battery_care(new_val);
                    self.config.battery_care_enabled = new_val;
                    self.battery_care_enabled = new_val;
                    self.config.save().ok();
                    if let Some(state) = crate::tray::TRAY_STATE.get() {
                        if let Ok(mut s) = state.lock() {
                            s.battery_care_enabled = new_val;
                        }
                    }
                }
                UiCommand::CyclePerfMode => {
                    let modes = ec::performance::PerfMode::all();
                    let current_idx = modes
                        .iter()
                        .position(|m| *m as u8 == self.performance_mode)
                        .unwrap_or(0);
                    let next_idx = (current_idx + 1) % modes.len();
                    let next_val = modes[next_idx] as u8;
                    let _ = self.backend.set_performance_mode(next_val);
                    self.config.performance_mode = next_val;
                    self.performance_mode = next_val;
                    self.config.save().ok();
                    if let Some(state) = crate::tray::TRAY_STATE.get() {
                        if let Ok(mut s) = state.lock() {
                            s.perf_mode = next_val;
                        }
                    }
                }
                UiCommand::SetPerfMode(mode) => {
                    let _ = self.backend.set_performance_mode(mode);
                    self.config.performance_mode = mode;
                    self.performance_mode = mode;
                    self.config.save().ok();
                    if let Some(state) = crate::tray::TRAY_STATE.get() {
                        if let Ok(mut s) = state.lock() {
                            s.perf_mode = mode;
                        }
                    }
                }
                UiCommand::RefreshStatus => {}
                UiCommand::ReapplyConfig => {
                    if self.config.auto_reapply_on_power_change {
                        let _ = self.backend.set_battery_care(self.config.battery_care_enabled);
                        let _ = self.backend.set_charge_limit(self.config.battery_charge_limit);
                        let _ = self.backend.set_performance_mode(self.config.performance_mode);
                    }
                }
            }
        }
        if needs_repaint {
            ctx.request_repaint();
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
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(" Xiaomi PC Manager Lite")
                            .color(Color32::WHITE)
                            .size(14.0),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(egui::RichText::new("─").color(Color32::WHITE).size(12.0))
                            .clicked()
                        {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        }
                        if ui
                            .button(egui::RichText::new("✕").color(Color32::WHITE).size(12.0))
                            .clicked()
                        {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });
            });

        // Tabs
        egui::TopBottomPanel::top("tabs")
            .frame(Frame {
                fill: Color32::from_gray(245),
                inner_margin: Margin::symmetric(4, 2),
                ..Default::default()
            })
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(self.active_tab == Tab::Power, "  主界面  ")
                        .clicked()
                    {
                        self.active_tab = Tab::Power;
                    }
                    if ui
                        .selectable_label(self.active_tab == Tab::Settings, "  设置  ")
                        .clicked()
                    {
                        self.active_tab = Tab::Settings;
                    }
                });
            });

        // Content
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                match self.active_tab {
                    Tab::Power => self.show_power_tab(ui),
                    Tab::Settings => self.show_settings_tab(ui),
                }
            });
        });
    }
}

impl XiaomiApp {
    fn show_power_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("状态");
        ui.horizontal(|ui| {
            ui.label("后端:");
            ui.colored_label(Color32::from_rgb(0x25, 0x50, 0xAA), &self.backend_name);
        });
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("电池养护: {}", if self.battery_care_enabled { "开启" } else { "关闭" }))
                    .strong(),
            );
            if !self.battery_care_enabled {
                ui.colored_label(Color32::GRAY, "(充电至100%)");
            }
        });
        ui.horizontal(|ui| {
            ui.label(format!("充电上限: {}%", self.charge_limit));
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
                let _ = self.backend.set_battery_care(enabled);
                self.config.save().ok();
                if let Some(state) = crate::tray::TRAY_STATE.get() {
                    if let Ok(mut s) = state.lock() {
                        s.battery_care_enabled = enabled;
                    }
                }
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
                    let _ = self.backend.set_charge_limit(new_limit);
                    self.config.save().ok();
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
                        let _ = self.backend.set_performance_mode(val);
                        self.config.save().ok();
                        if let Some(state) = crate::tray::TRAY_STATE.get() {
                            if let Ok(mut s) = state.lock() {
                                s.perf_mode = val;
                            }
                        }
                    }

                    if (i + 1) % ncols == 0 {
                        ui.end_row();
                    }
                }
            });
    }

    fn show_settings_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("设置");
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.label("EC 后端偏好:");
            let mut pref = self.config.backend.clone();
            let changed = ui
                .radio_value(&mut pref, ec::config::BackendPreference::Auto, "自动")
                .changed()
                | ui
                    .radio_value(&mut pref, ec::config::BackendPreference::Wmi, "WMI")
                    .changed()
                | ui
                    .radio_value(&mut pref, ec::config::BackendPreference::WinRing0, "WinRing0")
                    .changed();
            if changed {
                self.config.backend = pref;
                self.config.save().ok();
            }
        });

        ui.add_space(8.0);

        let mut auto = self.config.auto_apply_on_startup;
        if ui.checkbox(&mut auto, "启动时自动应用设置").changed() {
            self.config.auto_apply_on_startup = auto;
            self.config.save().ok();
        }

        let mut reapply = self.config.auto_reapply_on_power_change;
        if ui.checkbox(&mut reapply, "电源切换时自动重设").changed() {
            self.config.auto_reapply_on_power_change = reapply;
            self.config.save().ok();
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
