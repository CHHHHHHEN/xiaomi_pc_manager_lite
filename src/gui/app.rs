use eframe::egui::{self, Color32, Frame, Margin};
use eframe::egui::ViewportCommand;
use std::sync::mpsc;

use crate::command::UiCommand;
use crate::ec;
use crate::ec::config::BackendPreference;

use super::view;

pub struct XiaomiApp {
    pub cmd_tx: mpsc::Sender<UiCommand>,
    pub(crate) cmd_rx: mpsc::Receiver<UiCommand>,
    pub(crate) backend: Box<dyn ec::backend::EcBackend>,
    pub(crate) config: ec::config::AppConfig,
    pub(crate) current_pref: BackendPreference,
    pub(crate) backend_name: String,
    pub(crate) battery_care_enabled: bool,
    pub(crate) charge_limit: u8,
    pub(crate) performance_mode: u8,
    pub(crate) error_msg: Option<String>,
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

pub fn run_app(backend: Box<dyn ec::backend::EcBackend>, config: ec::config::AppConfig, init_error: Option<String>) {
    let pref = config.backend;
    let app = XiaomiApp::new(backend, config, pref, init_error);
    let cmd_tx = app.cmd_tx.clone();

    crate::tray::spawn(cmd_tx.clone());
    crate::ec::fnkey::spawn(cmd_tx.clone());

    let icon = view::load_icon_data();
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
            if let Some((name, data)) = view::load_cjk_font() {
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

impl eframe::App for XiaomiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_commands(ctx);
        ctx.request_repaint_after(std::time::Duration::from_secs(5));

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

                let btn_size = egui::vec2(32.0, total_rect.height());
                ui.allocate_new_ui(
                    egui::UiBuilder::new()
                        .max_rect(button_strip_rect)
                        .layout(egui::Layout::right_to_left(egui::Align::Center)),
                    |ui| {
                        if view::titlebar_button(ui, btn_size, "close")
                            .on_hover_text("退出")
                            .clicked()
                        {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        let is_maximized =
                            ctx.viewport(|v| v.builder.maximized.unwrap_or(false));
                        if view::titlebar_button(ui, btn_size, if is_maximized { "restore" } else { "maximize" })
                            .on_hover_text(if is_maximized { "还原" } else { "最大化" })
                            .clicked()
                        {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!is_maximized));
                        }
                        if view::titlebar_button(ui, btn_size, "minimize")
                            .on_hover_text("最小化")
                            .clicked()
                        {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        }
                    },
                );
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.show_main_view(ui);
            });
        });

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
        assert_eq!(cycle[0], 0x09);
        assert_eq!(cycle[1], 0x02);
        assert_eq!(cycle[2], 0x04);
    }

    #[test]
    fn test_xiaomi_app_send() {
        fn assert_send<T: Send>() {}
        assert_send::<XiaomiApp>();
    }
}
