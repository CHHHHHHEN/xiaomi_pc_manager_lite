use eframe::egui::{self, Color32, Vec2};

use crate::ec;

use super::app::XiaomiApp;

impl XiaomiApp {
    pub fn show_main_view(&mut self, ui: &mut egui::Ui) {
        self.show_status_section(ui);
        ui.separator();
        ui.add_space(8.0);
        self.show_battery_care_section(ui);
        ui.separator();
        ui.add_space(8.0);
        self.show_performance_mode_section(ui);
        ui.separator();
        ui.add_space(8.0);
        self.show_settings_section(ui);
    }

    fn show_status_section(&mut self, ui: &mut egui::Ui) {
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
            ui.label(egui::RichText::new(format!("电池养护: {}", status)).strong());
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
    }

    fn show_battery_care_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("电池养护");
        ui.horizontal(|ui| {
            let mut enabled = self.battery_care_enabled;
            if ui.checkbox(&mut enabled, "启用电池养护").changed() {
                self.set_battery_care_internal(enabled);
            }
        });
        if self.battery_care_enabled {
            if !self.backend.supports_continuous_charge_limit() {
                ui.horizontal(|ui| {
                    ui.label("充电上限:");
                    for &limit in &[40, 50, 60, 70, 80, 90, 100] {
                        let selected = self.charge_limit == limit;
                        if ui.selectable_label(selected, format!("{}%", limit)).clicked() {
                            self.set_charge_limit_internal(limit);
                        }
                    }
                });
            } else {
                let mut limit = self.charge_limit as f32;
                ui.horizontal(|ui| {
                    ui.label("充电上限:");
                    if ui
                        .add(egui::Slider::new(&mut limit, 40.0..=100.0).step_by(1.0).suffix("%"))
                        .changed()
                    {
                        self.set_charge_limit_internal(limit.round() as u8);
                    }
                });
            }
        }
    }

    fn show_performance_mode_section(&mut self, ui: &mut egui::Ui) {
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
                        self.set_perf_mode_internal(val);
                    }

                    if (i + 1) % ncols == 0 {
                        ui.end_row();
                    }
                }
            });
    }

    fn show_settings_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("设置");
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.label("EC 后端偏好:");
            let mut pref = self.current_pref;
            let changed = ui
                .radio_value(&mut pref, crate::ec::config::BackendPreference::Auto, "自动")
                .changed()
                | ui
                    .radio_value(&mut pref, crate::ec::config::BackendPreference::Wmi, "WMI")
                    .changed()
                | ui
                    .radio_value(&mut pref, crate::ec::config::BackendPreference::WinRing0, "WinRing0")
                    .changed();
            if changed && pref != self.current_pref {
                self.try_switch_backend(pref);
            }
        });

        ui.add_space(8.0);

        let mut auto = self.config.auto_apply_on_startup;
        if ui.checkbox(&mut auto, "启动时自动应用设置").changed() {
            self.config.auto_apply_on_startup = auto;
            self.save_state();
        }

        let mut reapply = self.config.auto_reapply_on_power_change;
        if ui.checkbox(&mut reapply, "电源切换时自动重设").changed() {
            self.config.auto_reapply_on_power_change = reapply;
            self.save_state();
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

pub fn titlebar_button(ui: &mut egui::Ui, size: egui::Vec2, kind: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let hovered = response.hovered();
    if hovered {
        ui.painter().rect_filled(rect, 0.0, Color32::from_white_alpha(40));
    }
    let stroke = egui::Stroke::new(2.0, Color32::WHITE);
    let cx = rect.center().x;
    let cy = rect.center().y;
    let pad = 10.0;
    let painter = ui.painter();
    match kind {
        "close" | "关闭" => {
            let r = pad * 0.5;
            painter.line_segment(
                [egui::pos2(cx - r, cy - r), egui::pos2(cx + r, cy + r)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(cx + r, cy - r), egui::pos2(cx - r, cy + r)],
                stroke,
            );
        }
        "minimize" | "最小化" => {
            let half = pad * 0.4;
            painter.line_segment(
                [egui::pos2(cx - half, cy), egui::pos2(cx + half, cy)],
                stroke,
            );
        }
        "maximize" | "最大化" => {
            let half = pad * 0.45;
            let r = egui::Rect::from_center_size(
                egui::pos2(cx, cy),
                egui::vec2(half * 2.0, half * 2.0),
            );
            painter.rect_stroke(r, 2.0, stroke, egui::StrokeKind::Inside);
        }
        "restore" | "还原" => {
            let half = pad * 0.4;
            let r1 = egui::Rect::from_center_size(
                egui::pos2(cx + 2.0, cy - 2.0),
                egui::vec2(half * 2.0, half * 2.0),
            );
            let r2 = egui::Rect::from_center_size(
                egui::pos2(cx - 2.0, cy + 2.0),
                egui::vec2(half * 2.0, half * 2.0),
            );
            painter.rect_stroke(r1, 2.0, stroke, egui::StrokeKind::Inside);
            painter.rect_stroke(r2, 2.0, stroke, egui::StrokeKind::Inside);
        }
        _ => {}
    }
    response
}

pub fn load_cjk_font() -> Option<(String, Vec<u8>)> {
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

pub fn load_icon_data() -> Option<egui::IconData> {
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
