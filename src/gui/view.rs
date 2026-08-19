use eframe::egui::{self, Color32, Frame, Margin, Vec2};

use crate::app;

use super::app::XiaomiApp;

/// 品牌蓝：标题栏底色与状态/选中态的统一强调色。历史实现在 view.rs 与
/// app.rs 各自硬编码 `0x25,0x50,0xAA`，重复为两个事实来源——统一收敛到此处。
pub const BRAND_BLUE: Color32 = Color32::from_rgb(0x25, 0x50, 0xAA);

/// 绿色（健康/交流充电中）、橙色（轻度衰减/警告）、红色（警示）。
/// 电量进度条（交流供电、<20% 红色警示）与电池健康度配色曾各自硬编码
/// 同一组 RGB（修订 1.46 审计）——统一收敛到常量，两处语义不再漂移。
const COLOR_OK: Color32 = Color32::from_rgb(0x1B, 0x5E, 0x20);
const COLOR_WARN: Color32 = Color32::from_rgb(0xB0, 0x5F, 0x00);
const COLOR_BAD: Color32 = Color32::from_rgb(0xC0, 0x39, 0x2B);

/// 电池健康度等级配色（与电量进度条同风格）：≥95% 绿（健康）、80~95% 橙
/// （轻度衰减）、<80% 红（明显衰减，提醒关注）。阈值与 F-BAT-13 一致。
fn battery_health_color(pct: f32) -> Color32 {
    if pct >= 95.0 {
        COLOR_OK
    } else if pct >= 80.0 {
        COLOR_WARN
    } else {
        COLOR_BAD
    }
}

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

    /// 自绘标题栏：品牌蓝底色 + 应用图标/标题 + 最小化/最大化/关闭按钮。
    /// 从 app.rs::update 抽出，使 update 保持为"事件编排 + 内容层装配"的薄层，
    /// 渲染细节统一收敛到 view 层。
    pub(crate) fn show_title_bar(&self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("title_bar")
            .frame(Frame {
                fill: BRAND_BLUE,
                inner_margin: Margin::symmetric(8, 4),
                ..Default::default()
            })
            .show(ctx, |ui| {
                let total_rect = ui.available_rect_before_wrap();
                // 最大化状态（`None` = 平台尚未上报窗口状态，按未最大化处理）：
                // 整帧只查询一次，双击路径与按钮条共用同一结果（同一帧两个
                // 按钮/双击不应读到不同值。历史在双击路径与按钮条各自
                // ctx.input 查询，多一次锁开销且语义需保持一致——修订 1.47
                // 收敛为单次查询）。
                let is_maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                // 按钮区宽度 = 3 个 32px 按钮 + 按钮间默认 item_spacing(8px)×2，
                // 随按钮尺寸推导而非硬编码 96（修订 1.46 审计：96 只够 3 个
                // 按钮本体，右侧布局的间距把 Minimize 挤出标题区）。
                let btn_size = egui::vec2(32.0, total_rect.height());
                let spacing = ui.spacing().item_spacing.x;
                let button_strip_width = btn_size.x * 3.0 + spacing * 2.0;
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
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!is_maximized));
                }

                // 标题栏：左侧应用图标 + 标题文字。
                let icon_size = 18.0;
                if let Some(tex) = &self.icon_tex {
                    let icon_rect = egui::Rect::from_center_size(
                        egui::pos2(
                            title_rect.left() + 2.0 + icon_size / 2.0,
                            title_rect.center().y,
                        ),
                        egui::vec2(icon_size, icon_size),
                    );
                    ui.painter().image(
                        tex.id(),
                        icon_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                    ui.painter().text(
                        egui::pos2(icon_rect.right() + 4.0, title_rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        crate::util::APP_NAME,
                        egui::FontId::proportional(14.0),
                        Color32::WHITE,
                    );
                } else {
                    ui.painter().text(
                        title_rect.left_center() + egui::vec2(4.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        crate::util::APP_NAME,
                        egui::FontId::proportional(14.0),
                        Color32::WHITE,
                    );
                }

                // 最大化状态已在上方查询一次（双击路径 + 按钮条共用，见函数开头）。
                // 按钮条内的最大化/还原按钮按该结果分派。
                ui.allocate_new_ui(
                    egui::UiBuilder::new()
                        .max_rect(button_strip_rect)
                        .layout(egui::Layout::right_to_left(egui::Align::Center)),
                    |ui| {
                        if titlebar_button(ui, btn_size, TitleBarKind::Close)
                            .on_hover_text("隐藏到托盘")
                            .clicked()
                        {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        if titlebar_button(
                            ui,
                            btn_size,
                            if is_maximized {
                                TitleBarKind::Restore
                            } else {
                                TitleBarKind::Maximize
                            },
                        )
                        .on_hover_text(if is_maximized { "还原" } else { "最大化" })
                        .clicked()
                        {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!is_maximized));
                        }
                        if titlebar_button(ui, btn_size, TitleBarKind::Minimize)
                            .on_hover_text("最小化")
                            .clicked()
                        {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        }
                    },
                );
            });
    }

    /// 右下角自绘缩放手柄（无边框窗口需自绘 resize 角标）。
    pub(crate) fn show_resize_handle(&self, ctx: &egui::Context) {
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
                    let new = egui::vec2(
                        (s.x + delta.x).max(crate::util::MIN_WINDOW_SIZE.0),
                        (s.y + delta.y).max(crate::util::MIN_WINDOW_SIZE.1),
                    );
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(new));
                }
                if resize_resp.hovered() || resize_resp.dragged() {
                    ctx.set_cursor_icon(egui::CursorIcon::ResizeSouthEast);
                }
                let painter = ui.painter();
                let p = corner;
                for i in 0..3 {
                    let off = (i as f32) * 4.0;
                    painter.line_segment(
                        [
                            egui::pos2(p.x - off - 2.0, p.y),
                            egui::pos2(p.x, p.y - off - 2.0),
                        ],
                        egui::Stroke::new(2.0, Color32::from_gray(140)),
                    );
                }
            });
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
            }
        });
        ui.horizontal(|ui| {
            ui.label("后端:");
            ui.colored_label(BRAND_BLUE, self.backend.name());
        });
        // 电源状态 + 电池电量：来自 Windows 系统 API（GetSystemPowerStatus），
        // 实时读取、无需后端往返。电量未知（API 失败/255）时显示"未知"。
        let power = crate::platform::power::power_snapshot();
        ui.horizontal(|ui| {
            ui.label("电源:");
            let power_text =
                crate::app::power::power_status_text_gui(power.status, power.battery_percent);
            ui.colored_label(BRAND_BLUE, power_text);
        });
        // 电量进度条：电量已知时按百分比填充（<20% 红色警示，交流供电绿色，
        // 其余品牌蓝），未知时显示灰色占位。
        if let Some(pct) = power.battery_percent {
            let pct_f = pct as f32 / 100.0;
            let fill = match (power.status, pct) {
                (_, p) if p < 20 => COLOR_BAD,
                (crate::platform::power::PowerStatus::OnAc, _) => COLOR_OK,
                _ => BRAND_BLUE,
            };
            ui.add(
                egui::ProgressBar::new(pct_f)
                    .desired_width(ui.available_width())
                    .fill(fill)
                    .text(format!("{}%", pct)),
            );
        } else {
            ui.add(
                egui::ProgressBar::new(0.0)
                    .desired_width(ui.available_width())
                    .fill(Color32::from_gray(180))
                    .text("电量未知"),
            );
        }
        // 电池健康（后台线程经 root\WMI 容量读数上报，见
        // platform::battery_health）：未读到（WMI 未就绪/无电池/类不可用）时
        // 不展示该行，避免"未知"占位噪音。健康度 = 满充容量 / 设计容量；
        // ≥95% 绿、80~95% 橙、<80% 红，与电量进度条同风格（F-BAT-13）。
        if let Some(health) = self.battery_health {
            if let Some(pct) = health.health_percent_u8() {
                let color = battery_health_color(pct as f32);
                ui.horizontal(|ui| {
                    ui.label("电池健康:");
                    ui.colored_label(
                        color,
                        format!(
                            "{:.0}% (设计 {} mWh · 满充 {} mWh)",
                            pct, health.designed_mwh, health.full_mwh
                        ),
                    );
                });
                // 进度条与文案使用**同一**舍入值（health_percent_u8）：避免
                // 文案 99% 而进度条 98.6% 的两套显示不一致。
                ui.add(
                    egui::ProgressBar::new((pct as f32 / 100.0).clamp(0.0, 1.0))
                        .desired_width(ui.available_width())
                        .fill(color)
                        .text(format!("{}%", pct)),
                );
            }
        }
        // 预计剩余/充满时长（root\WMI BatteryStatus 速率估算，修订 1.37）：
        // 速率不可用（满电停充/异常）时不展示。
        if let Some(eta) = &self.battery_eta_text {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(eta).color(Color32::from_gray(80)));
            });
        }
        ui.horizontal(|ui| {
            let status = app::battery::care_label(self.runtime.battery_care_enabled);
            ui.label(egui::RichText::new(format!("电池养护: {}", status)).strong());
            if !self.runtime.battery_care_enabled {
                ui.colored_label(Color32::GRAY, "(充电至100%)");
            }
        });
        ui.horizontal(|ui| {
            ui.label(format!("充电上限: {}%", self.runtime.charge_limit));
        });
        let perf_name = app::performance::PerfMode::name_or_unknown(self.runtime.performance_mode);
        ui.horizontal(|ui| {
            ui.label("性能模式: ");
            ui.colored_label(BRAND_BLUE, perf_name);
        });
        if let Some(err) = &self.error_msg {
            ui.colored_label(Color32::RED, err);
        }
    }

    fn show_battery_care_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("电池养护");
        ui.horizontal(|ui| {
            let mut enabled = self.runtime.battery_care_enabled;
            if ui
                .checkbox(&mut enabled, "启用电池养护")
                .on_hover_text("开启后充电达到上限即停止，延缓电池老化")
                .changed()
            {
                self.set_battery_care_internal(enabled);
            }
        });
        if self.runtime.battery_care_enabled {
            if !self.backend.supports_continuous_charge_limit() {
                ui.horizontal(|ui| {
                    ui.label("充电上限:")
                        .on_hover_text("充满即停的充电阈值，数值越低对电池越友好");
                    for &limit in crate::ec::wmi::protocol::WMI_PRESET_PERCENTS {
                        let selected = self.runtime.charge_limit == limit;
                        if ui
                            .selectable_label(selected, format!("{}%", limit))
                            .on_hover_text(format!("充电达到 {}% 后停止", limit))
                            .clicked()
                        {
                            self.set_charge_limit_internal(limit);
                        }
                    }
                });
            } else {
                // F-PWR-04（修订 1.33）：拖动中沿用 `charge_limit_drag` 工作值
                // 而非每帧从 runtime 重取——电源切换触发的 refresh_from_backend
                // 会改写 runtime.charge_limit，若滑块每帧重新初始化 limit，拖动
                // 途中被后台刷新"拽回"，用户手指下的值会跳变（离电瞬间尤其
                // 明显）。未拖动时（None）退回到 runtime 值。
                let mut limit = self.charge_limit_drag.unwrap_or(self.runtime.charge_limit) as f32;
                ui.horizontal(|ui| {
                    ui.label("充电上限:")
                        .on_hover_text("充满到该百分比即停止，40%~100% 可调");
                    let resp = ui.add(
                        egui::Slider::new(&mut limit, 40.0..=100.0)
                            .step_by(1.0)
                            .suffix("%"),
                    );
                    // 拖动中把工作值持久到 self（供下一帧继续），拖动结束
                    // （或点击/键盘改变）时一次性写入硬件：若每个 changed()
                    // 帧都调用 set_charge_limit_internal，拖动一次滑块会触发
                    // 几十次 EC 写入 + 读回 + 配置文件落盘（WMI 后端单次调用
                    // 可达数十毫秒），造成界面卡顿并长时间占用 EC（NFR-UX-02）。
                    if resp.dragged() {
                        self.charge_limit_drag = Some(limit.round() as u8);
                    }
                    if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                        self.charge_limit_drag = None;
                        self.set_charge_limit_internal(limit.round() as u8);
                    }
                });
            }
        }
    }

    fn show_performance_mode_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("性能模式");
        let modes = app::performance::PerfMode::all();
        let ncols = 3;
        egui::Grid::new("perf_grid")
            .min_col_width(100.0)
            .max_col_width(140.0)
            .spacing([8.0, 8.0])
            .show(ui, |ui| {
                for (i, mode) in modes.iter().enumerate() {
                    let val = mode.ec_value();
                    let is_selected = val == self.runtime.performance_mode;

                    // 先注册交互（点击/hover），背景与文字由下方自定义绘制：
                    // egui 默认按钮样式在浅色填充下文字对比度差（背景≈纯白、
                    // 字体跟随主题浅色→灰色）。选中 = 品牌蓝底白字；
                    // 未选中 = 柔和浅灰底深色字，hover 时背景加深。
                    let mut resp = ui.add(
                        egui::Button::new(egui::RichText::new(mode.name()).size(14.0))
                            .min_size(Vec2::new(100.0, 36.0))
                            .fill(Color32::TRANSPARENT)
                            .stroke(egui::Stroke::NONE)
                            .corner_radius(6),
                    );
                    resp = resp.on_hover_text(mode.description());

                    let fill = if is_selected {
                        BRAND_BLUE
                    } else if resp.hovered() {
                        Color32::from_rgb(0xD6, 0xD9, 0xDE)
                    } else {
                        Color32::from_rgb(0xEF, 0xF1, 0xF4)
                    };
                    let stroke_color = if is_selected {
                        Color32::from_rgb(0x1A, 0x3C, 0x80)
                    } else {
                        Color32::from_rgb(0xC2, 0xC6, 0xCC)
                    };
                    let text_color = if is_selected {
                        Color32::WHITE
                    } else {
                        Color32::from_rgb(0x33, 0x33, 0x33)
                    };

                    let painter = ui.painter();
                    painter.rect_filled(resp.rect, 6.0, fill);
                    painter.rect_stroke(
                        resp.rect,
                        6.0,
                        egui::Stroke::new(1.0, stroke_color),
                        egui::StrokeKind::Inside,
                    );
                    painter.text(
                        resp.rect.center(),
                        egui::Align2::CENTER_CENTER,
                        mode.name(),
                        egui::FontId::proportional(14.0),
                        text_color,
                    );

                    if resp.clicked() {
                        self.set_perf_mode_internal(*mode);
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
                .radio_value(
                    &mut pref,
                    crate::app::config::BackendPreference::Auto,
                    "自动",
                )
                .changed()
                | ui.radio_value(&mut pref, crate::app::config::BackendPreference::Wmi, "WMI")
                    .changed()
                | ui.radio_value(
                    &mut pref,
                    crate::app::config::BackendPreference::WinRing0,
                    "WinRing0",
                )
                .changed();
            if changed && pref != self.current_pref {
                self.try_switch_backend(pref);
            }
        });

        ui.add_space(8.0);

        if toggle_config_bool(
            ui,
            "启动时自动应用设置",
            &mut self.config.auto_apply_on_startup,
            None,
        ) {
            self.save_state();
        }

        if toggle_config_bool(
            ui,
            "电源切换时自动重设",
            &mut self.config.auto_reapply_on_power_change,
            None,
        ) {
            self.save_state();
        }

        // 电池供电自动切节能：打开后拔掉电源即自动切到 Eco（风扇静音、
        // 降低 CPU 功耗），插回电源恢复用户所选模式。仅在拔电且配置开启
        // 时生效（见 battery::apply_config_to_hardware 的降级逻辑）。
        if toggle_config_bool(
            ui,
            "电池供电时自动切换节能",
            &mut self.config.auto_switch_to_quiet_on_battery,
            Some("拔掉电源自动切换到节能模式，插回电源恢复原模式"),
        ) {
            // 立即生效：若当前在电池供电，马上按新配置重设硬件。
            // 注意用 apply_config_and_sync 而非 reapply_config：用户主动切换
            // 必须无条件应用，不受"电源切换时自动重设"开关约束。其内部会
            // save_state，此处不再重复落盘（修订 1.47 清理：历史在此先
            // save_state 一次、apply 后又一次，配置被重复写入磁盘）。
            self.apply_config_and_sync();
        }

        // 充电达到养护上限通知：电池充到配置的充电上限（或充满）时弹托盘
        // 通知，方便用户知晓"已到养护上限"。默认关闭（不主动打扰）。
        if toggle_config_bool(
            ui,
            "充电达到上限时通知",
            &mut self.config.notify_on_charge_limit,
            Some("电池充电达到养护上限（或充满）时弹托盘通知"),
        ) {
            self.save_state();
            self.sync_tray_status();
        }

        // F-AUTO-01: 开机自启动复选框。注册/删除走后台线程
        // （UiCommand::SetAutostart）：**请求时即同步写入配置**（修订 1.25
        // 修复勾选闪烁与中途退出配置/task 背离），后台 worker 完成后再经
        // SetAutostartResult 回传结果（成功仅确认；失败回滚并展示错误）。
        let mut autostart = self.config.auto_start_on_boot;
        if ui.checkbox(&mut autostart, "开机自启动").changed() {
            // 与其它发送方一致记录失败（全项目约定，见 main.rs/commands.rs）：
            // channel 断开意味着 GUI 事件循环即将退出，静默丢弃会掩盖时序。
            if let Err(e) = self
                .cmd_tx
                .send(crate::app::command::UiCommand::SetAutostart(autostart))
            {
                log::warn!("SetAutostart send failed: {}", e);
            }
        }

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);
        self.show_fn_key_section(ui);

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("版本 {}", crate::util::APP_VERSION))
                    .color(Color32::GRAY)
                    .size(11.0),
            );
            if ui
                .button(egui::RichText::new("打开日志").size(11.0))
                .on_hover_text("在文件资源管理器中打开日志文件")
                .clicked()
            {
                self.open_log_file();
            }
        });
    }

    /// Fn 功能键自定义绑定（监听线程经共享绑定表即时生效）。
    ///
    /// 三个区块（捕获模式 / 绑定列表 / 添加绑定）逻辑独立，从单一 ~180 行
    /// 函数拆出（修订 1.47 整理），保持本函数为区块编排薄层。
    fn show_fn_key_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Fn 功能键");
            ui.label(
                egui::RichText::new("(自定义绑定)")
                    .size(11.0)
                    .color(Color32::GRAY),
            );
        });

        if self.config.fn_key_bindings.is_empty() {
            ui.colored_label(
                Color32::GRAY,
                "当前没有绑定。可在下方「添加绑定」中选择预设键码。",
            );
        }
        self.show_fn_capture_row(ui);
        self.show_fn_binding_list(ui);
        self.show_fn_add_binding(ui);
    }

    /// 捕获模式开关 + 最近捕获事件展示（发现新键并"绑定为指定动作"）。
    fn show_fn_capture_row(&mut self, ui: &mut egui::Ui) {
        let capturing = self.fn_capture.load(std::sync::atomic::Ordering::Relaxed);
        ui.horizontal(|ui| {
            let mut on = capturing;
            if ui.checkbox(&mut on, "捕获功能键事件").changed() {
                self.toggle_fn_capture();
            }
            if capturing {
                ui.colored_label(
                    Color32::from_rgb(0x00, 0x80, 0x00),
                    "捕获中：请按目标功能键…",
                );
            }
        });
        if capturing {
            if let Some((class, hex)) = &self.last_fn_event {
                // 克隆出闭包所需数据，避免闭包内 &mut self 与 &self.last_fn_event
                // 的借用冲突（egui 闭包与外层借用无法共存）。
                let class = class.clone();
                let hex = hex.clone();
                ui.horizontal(|ui| {
                    ui.label("最近捕获:");
                    ui.monospace(app::fnkey::binding_label(&class, &hex));
                });
                // 用捕获到的键直接添加绑定（无需从预设挑选）：取 `capture_prefix`
                // 截断后的前缀（保留键码信息又不过度匹配，截断/释放归一化规则
                // 见该函数）。
                let prefix = capture_prefix(&hex);
                // 动作选择必须保存在 self 上（每帧 UI 重建，局部变量会
                // 重置回默认——历史实现用局部变量导致用户选中的动作在
                // 下一帧丢失，"使用此键"恒绑定默认动作，H1 回归）。
                let mut action = self.fn_capture_action;
                ui.horizontal(|ui| {
                    ui.label("绑定为:");
                    fn_action_combo(ui, "fn_capture_action", &mut action);
                    self.fn_capture_action = action;
                    // RunCommand 动作：附带命令行输入框（草稿跨帧保持，
                    // 与添加绑定流程的 fn_add_command 同理；否则捕获绑定
                    // 到的 RunCommand 命令为空、需事后到列表再改一次）。
                    if action == crate::app::fnkey::FnAction::RunCommand {
                        run_command_field(ui, &mut self.fn_capture_command, 200.0);
                    }
                    if ui.button("使用此键").clicked() {
                        let command = self.fn_capture_command.clone();
                        // 仅绑定真正写入（返回 true）时清空草稿：若校验拒绝
                        // （罕见：捕获产物异常），保留用户输入便于重试（修订
                        // 1.33 回归修正）。
                        if self.add_fn_binding(&class, &prefix, action, &command) {
                            self.fn_capture_command.clear();
                        }
                    }
                });
            } else {
                ui.colored_label(Color32::GRAY, "（等待功能键事件…）");
            }
        }
    }

    /// 绑定列表：每条一行（展示 + 动作下拉 + 删除）。
    ///
    /// 删除会前移后续条目下标，当前 for 的索引序列在删除后失效——用
    /// `mutated` 标记跳出循环，下一帧用更新后的列表重新渲染。
    fn show_fn_binding_list(&mut self, ui: &mut egui::Ui) {
        let binding_count = self.config.fn_key_bindings.len();
        let mut mutated = false;
        for i in 0..binding_count {
            if mutated {
                break;
            }
            let binding_snapshot = {
                let b = &self.config.fn_key_bindings[i];
                (
                    b.class.clone(),
                    b.prefix.clone(),
                    b.action,
                    b.command.clone(),
                )
            };
            let (class, prefix, action, command) = &binding_snapshot;
            // 标签统一经 app::fnkey::binding_label（修订 1.50 收敛：历史为取
            // label 临时构造整条 FnKeyBinding，与捕获行的 format 各写一份）。
            let label = format!("{}. {}", i + 1, app::fnkey::binding_label(class, prefix));
            ui.horizontal(|ui| {
                ui.label(label);
                let mut selected_action = *action;
                fn_action_combo(
                    ui,
                    ui.id().with(format!("fn_action_{}", i)),
                    &mut selected_action,
                );
                if selected_action != *action {
                    self.set_fn_binding_action(i, selected_action);
                }
                // RunCommand 动作：展示命令行输入框（其余动作不展示）。
                // 输入框宽度有限，放在下一行水平组避免挤爆当前行；失去焦点
                // 时若内容有变化才落盘（避免每帧都触发 save_state）。
                if selected_action == app::fnkey::FnAction::RunCommand {
                    let mut cmd_text = command.clone().unwrap_or_default();
                    let resp = run_command_field(ui, &mut cmd_text, 240.0);
                    if resp.lost_focus() && cmd_text != command.as_deref().unwrap_or_default() {
                        self.set_fn_binding_command(i, &cmd_text);
                    }
                }
                if ui.small_button("删除").clicked() {
                    self.remove_fn_binding(i);
                    mutated = true;
                }
            });
        }
    }

    /// 添加绑定：预设键码下拉 + 动作 + 添加按钮。
    ///
    /// 选择状态必须保存在 self 上（每帧 UI 重新构建，局部变量每帧重置
    /// 回默认值导致下拉永远显示第一项、选中无法保持——见 fn_add_* 修复）。
    fn show_fn_add_binding(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("添加:");
            let mut selected = self.fn_add_preset_index;
            // 索引由下方枚举 KNOWN_FN_KEYS 的 ComboBox 写入，恒在界内；但
            // 渲染路径不应 panic——越界时告警并回退第 0 项（修订 1.46 审计：
            // 历史用 .expect 让数组误改直接崩溃整个 GUI，单次越界不应杀死
            // 应用，且下拉仍按合法索引渲染、下一帧即自愈）。
            let selected_key = crate::app::fnkey::KNOWN_FN_KEYS.get(selected).or_else(|| {
                log::error!(
                    "fn_add_preset_index {} out of KNOWN_FN_KEYS bounds; falling back to 0",
                    selected
                );
                selected = 0;
                crate::app::fnkey::KNOWN_FN_KEYS.first()
            });
            egui::ComboBox::from_id_salt("fn_add_preset")
                .selected_text(selected_key.map(|k| k.name).unwrap_or_default())
                .show_ui(ui, |ui| {
                    for (idx, k) in crate::app::fnkey::KNOWN_FN_KEYS.iter().enumerate() {
                        ui.selectable_value(&mut selected, idx, k.name);
                    }
                });
            self.fn_add_preset_index = selected;
            let mut add_action = self.fn_add_action;
            fn_action_combo(ui, "fn_add_action", &mut add_action);
            self.fn_add_action = add_action;
            // RunCommand 动作：附带命令行输入框（草稿跨帧保持，见
            // app.rs 的 fn_add_command 注释）；其余动作不展示。
            if add_action == app::fnkey::FnAction::RunCommand {
                run_command_field(ui, &mut self.fn_add_command, 200.0);
            }
            if ui.button("添加绑定").clicked() {
                // 点击路径同样防越界：告警 + 跳过（不 panic，渲染路径同规则）。
                let Some(k) = crate::app::fnkey::KNOWN_FN_KEYS.get(selected) else {
                    log::warn!(
                        "fn_add_preset_index {} out of KNOWN_FN_KEYS; ignoring add click",
                        selected
                    );
                    return;
                };
                // 先克隆命令文本（闭包内 &mut self.add_fn_binding 与
                // &self.fn_add_command 无法共存借用）。
                let command = self.fn_add_command.clone();
                self.add_fn_binding(k.class, k.prefix, add_action, &command);
                // 添加成功后清空命令草稿（避免下次添加残留上次的命令）。
                self.fn_add_command.clear();
            }
        });
    }
}

/// 设置区布尔开关的"勾选 → 写回配置"统一样板（修订 1.47 收敛）。
///
/// 启动自动应用 / 电源重设 / 电池自动切节能 / 达上限通知四个开关曾各自
/// 重复 `let mut x = config.f; if checkbox(...).changed() { config.f = x;
/// save_state(); }` 的样板。本函数只负责勾选检测与写回，返回 `true` 表示
/// 发生了变更——持久化与"即时生效"动作由调用方决定（各开关的落盘策略
/// 不同：`apply_config_and_sync` 内部会 save_state，直接 save 的开关由
/// 调用方显式调用）。
fn toggle_config_bool(
    ui: &mut egui::Ui,
    label: &str,
    field: &mut bool,
    hover: Option<&str>,
) -> bool {
    let mut value = *field;
    let mut response = ui.checkbox(&mut value, label);
    if let Some(text) = hover {
        response = response.on_hover_text(text);
    }
    if response.changed() {
        *field = value;
        true
    } else {
        false
    }
}

/// Fn 动作下拉框（`ComboBox` + `FnAction::all()` 列表）。捕获行、绑定列表
/// 行、添加行曾三处各自复制同一段 `ComboBox ... for a in FnAction::all()`
/// 代码——统一收敛到此处，新增动作类型只改这一处。
///
/// `id_salt` 由调用方区分同名下拉（egui 按 id 区分交互状态）。
fn fn_action_combo(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    action: &mut app::fnkey::FnAction,
) {
    egui::ComboBox::from_id_salt(id_salt)
        .selected_text(action.name())
        .show_ui(ui, |ui| {
            for a in crate::app::fnkey::FnAction::all() {
                ui.selectable_value(action, *a, a.name());
            }
        });
}

/// 把捕获到的功能键事件 hex 截断为**绑定的前缀**。
///
/// 捕获事件可能带后续状态/长度字节，取前 6 个 hex（如 `012801` = 3 字节）
/// 作为前缀，既保留键码信息又不过度匹配。
///
/// 截断必须是**偶数字符**（完整字节）：hex 是半字节编码，奇数长度前缀
/// （如 `01280`）匹配不到任何真实事件，且展示时会缺半个字节（L3 回归）。
/// 极端输入（长度 < 2，如单 hex 字符 `"A"`）会匹配所有以 A 开头的事件，
/// 与空前缀同属危险配置——回退到**偶数长度**（0）而非保留奇数串。
///
/// **释放事件归一化为按下**（修订 1.50 修复）：捕获开启时若用户已按住目标
/// 键，收到的首条事件可能是释放（状态字节 `00`）——直接绑定该前缀只会命中
/// 未来的释放事件，下一次物理按键（按下 `01`）永不命中，绑定看起来"静默
/// 失效"（F-FNK-06 语义冲突）。前缀末字节若是 `00`（释放）则改写为 `01`
/// （按下），让绑定命中下一次物理按键的按下事件。
fn capture_prefix(hex: &str) -> String {
    let mut prefix_len = hex.len().min(6);
    if !prefix_len.is_multiple_of(2) {
        prefix_len -= 1;
    }
    let p = if prefix_len >= 2 {
        &hex[..prefix_len]
    } else {
        &hex[..hex.len() - hex.len() % 2]
    };
    if p.len() >= 6 && p.ends_with("00") {
        format!("{}01", &p[..p.len() - 2])
    } else {
        p.to_string()
    }
}

/// RunCommand 动作的命令行输入框（提示语 + 宽度）。捕获行/绑定列表行/
/// 添加行三处曾各自复制同一段 `TextEdit::singleline(...).hint_text(...)`——
/// 统一收敛到此处。
fn run_command_field(ui: &mut egui::Ui, text: &mut String, width: f32) -> egui::Response {
    ui.add(
        egui::TextEdit::singleline(text)
            .hint_text("例如 start notepad 或 C:\\tools\\app.exe")
            .desired_width(width),
    )
}

/// 标题栏按钮类型：编译期确定绘制分支（历史实现用 `&str` 分发，
/// 拼写错误静默落入 `_ => {}` 分支不绘制）。
#[derive(Clone, Copy)]
pub enum TitleBarKind {
    Close,
    Minimize,
    Maximize,
    Restore,
}

pub fn titlebar_button(ui: &mut egui::Ui, size: egui::Vec2, kind: TitleBarKind) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let hovered = response.hovered();
    if hovered {
        ui.painter()
            .rect_filled(rect, 0.0, Color32::from_white_alpha(40));
    }
    let stroke = egui::Stroke::new(2.0, Color32::WHITE);
    let cx = rect.center().x;
    let cy = rect.center().y;
    let pad = 10.0;
    let painter = ui.painter();
    match kind {
        TitleBarKind::Close => {
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
        TitleBarKind::Minimize => {
            let half = pad * 0.4;
            painter.line_segment(
                [egui::pos2(cx - half, cy), egui::pos2(cx + half, cy)],
                stroke,
            );
        }
        TitleBarKind::Maximize => {
            let half = pad * 0.45;
            let r = egui::Rect::from_center_size(
                egui::pos2(cx, cy),
                egui::vec2(half * 2.0, half * 2.0),
            );
            painter.rect_stroke(r, 2.0, stroke, egui::StrokeKind::Inside);
        }
        TitleBarKind::Restore => {
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
    // 图标来自嵌入资源（include_bytes），内容进程内恒定——解码一次并缓存：
    // 历史实现每次调用都重新解码 PNG，run_app 的 with_icon 与首帧 update 的
    // 标题栏纹理各解一次。OnceLock 保证只解码一次，后续调用直接复用。
    // 解码统一收敛到 `platform::icon::app_icon_rgba`（与多尺寸 ICO 构建共用
    // 同一缓存，修订 1.49 整理）。
    static CACHE: std::sync::OnceLock<Option<egui::IconData>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let img = crate::platform::icon::app_icon_rgba()?;
            let (w, h) = img.dimensions();
            Some(egui::IconData {
                rgba: img.into_raw(),
                width: w,
                height: h,
            })
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 电池健康度等级配色（F-BAT-13）：≥95% 绿 / 80~95% 橙 / <80% 红。
    /// 边界值归属（≥80 归橙、<80 归红）用色值三元组锁定，防止未来调整阈值
    /// 时与需求文档漂移。
    #[test]
    fn test_battery_health_color_thresholds() {
        let green = Color32::from_rgb(0x1B, 0x5E, 0x20);
        let orange = Color32::from_rgb(0xB0, 0x5F, 0x00);
        let red = Color32::from_rgb(0xC0, 0x39, 0x2B);

        // ≥95 绿。
        assert_eq!(battery_health_color(100.0), green);
        assert_eq!(battery_health_color(95.0), green);
        // 80~95 橙（含 80 边界，不含 95）。
        assert_eq!(battery_health_color(94.9), orange);
        assert_eq!(battery_health_color(80.0), orange);
        // <80 红。
        assert_eq!(battery_health_color(79.9), red);
        assert_eq!(battery_health_color(0.0), red);
    }

    /// 捕获前缀（修订 1.50）：按下事件原样截断为 6 hex；释放事件（末状态
    /// 字节 `00`）归一化为按下（`01`）——否则绑定只命中未来的释放事件、
    /// 下一次物理按键永不触发（F-FNK-06）；奇数长度/超长输入回退与截断
    /// 语义不变。
    #[test]
    fn test_capture_prefix_normalizes_release_to_press() {
        // 按下事件：前 6 位保留。
        assert_eq!(capture_prefix("01280100"), "012801");
        // 释放事件（捕获开启时按键已被按住的首条事件）：状态字节 00 → 01。
        assert_eq!(capture_prefix("012800"), "012801");
        // 含后续长度字节的报告同样只取前 6。
        assert_eq!(capture_prefix("0128010002"), "012801");
        // 非释放前缀不受影响（末字节 01/其它）。
        assert_eq!(capture_prefix("012501"), "012501");
        // 奇数长度回退到偶数。
        assert_eq!(capture_prefix("01280"), "0128");
    }
}
