use eframe::egui::{self, Color32, Frame, Margin, Vec2};

use crate::ec;

use super::app::XiaomiApp;

/// 品牌蓝：标题栏底色与状态/选中态的统一强调色。历史实现在 view.rs 与
/// app.rs 各自硬编码 `0x25,0x50,0xAA`，重复为两个事实来源——统一收敛到此处。
pub const BRAND_BLUE: Color32 = Color32::from_rgb(0x25, 0x50, 0xAA);

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
                    let is_maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
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

                let btn_size = egui::vec2(32.0, total_rect.height());
                ui.allocate_new_ui(
                    egui::UiBuilder::new()
                        .max_rect(button_strip_rect)
                        .layout(egui::Layout::right_to_left(egui::Align::Center)),
                    |ui| {
                        if titlebar_button(ui, btn_size, "close")
                            .on_hover_text("隐藏到托盘")
                            .clicked()
                        {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        let is_maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                        if titlebar_button(
                            ui,
                            btn_size,
                            if is_maximized { "restore" } else { "maximize" },
                        )
                        .on_hover_text(if is_maximized { "还原" } else { "最大化" })
                        .clicked()
                        {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!is_maximized));
                        }
                        if titlebar_button(ui, btn_size, "minimize")
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
                    let new = egui::vec2((s.x + delta.x).max(400.0), (s.y + delta.y).max(500.0));
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
            let power_text = match (power.status, power.battery_percent) {
                (crate::platform::power::PowerStatus::OnAc, Some(pct)) => {
                    format!("交流电 · 电量 {}%", pct)
                }
                (crate::platform::power::PowerStatus::OnAc, None) => "交流电".to_string(),
                (crate::platform::power::PowerStatus::OnBattery, Some(pct)) => {
                    format!("电池 · 电量 {}%", pct)
                }
                (crate::platform::power::PowerStatus::OnBattery, None) => {
                    "电池 · 电量未知".to_string()
                }
                (crate::platform::power::PowerStatus::Unknown, _) => "未知".to_string(),
            };
            ui.colored_label(BRAND_BLUE, power_text);
        });
        // 电量进度条：电量已知时按百分比填充（<20% 红色警示，交流供电绿色，
        // 其余品牌蓝），未知时显示灰色占位。
        if let Some(pct) = power.battery_percent {
            let pct_f = pct as f32 / 100.0;
            let fill = match (power.status, pct) {
                (_, p) if p < 20 => Color32::from_rgb(0xC0, 0x39, 0x2B),
                (crate::platform::power::PowerStatus::OnAc, _) => {
                    Color32::from_rgb(0x1B, 0x5E, 0x20)
                }
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
        ui.horizontal(|ui| {
            let status = if self.runtime.battery_care_enabled {
                "开启"
            } else {
                "关闭"
            };
            ui.label(egui::RichText::new(format!("电池养护: {}", status)).strong());
            if !self.runtime.battery_care_enabled {
                ui.colored_label(Color32::GRAY, "(充电至100%)");
            }
        });
        ui.horizontal(|ui| {
            ui.label(format!("充电上限: {}%", self.runtime.charge_limit));
        });
        let perf_name = ec::performance::PerfMode::name_or_unknown(self.runtime.performance_mode);
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
                    for &limit in crate::ec::battery::WMI_PRESET_PERCENTS {
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
                let mut limit = self.runtime.charge_limit as f32;
                ui.horizontal(|ui| {
                    ui.label("充电上限:")
                        .on_hover_text("充满到该百分比即停止，40%~100% 可调");
                    let resp = ui.add(
                        egui::Slider::new(&mut limit, 40.0..=100.0)
                            .step_by(1.0)
                            .suffix("%"),
                    );
                    // 只在拖动结束（或点击/键盘改变）时一次性写入硬件：若每个
                    // changed() 帧都调用 set_charge_limit_internal，拖动一次滑块
                    // 会触发几十次 EC 写入 + 读回 + 配置文件落盘（WMI 后端单次
                    // 调用可达数十毫秒），造成界面卡顿并长时间占用 EC（NFR-UX-02）。
                    if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
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
                    crate::ec::config::BackendPreference::Auto,
                    "自动",
                )
                .changed()
                | ui.radio_value(&mut pref, crate::ec::config::BackendPreference::Wmi, "WMI")
                    .changed()
                | ui.radio_value(
                    &mut pref,
                    crate::ec::config::BackendPreference::WinRing0,
                    "WinRing0",
                )
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

        // 电池供电自动切节能：打开后拔掉电源即自动切到 Eco（风扇静音、
        // 降低 CPU 功耗），插回电源恢复用户所选模式。仅在拔电且配置开启
        // 时生效（见 battery::apply_config_to_hardware 的降级逻辑）。
        let mut quiet = self.config.auto_switch_to_quiet_on_battery;
        if ui
            .checkbox(&mut quiet, "电池供电时自动切换节能")
            .on_hover_text("拔掉电源自动切换到节能模式，插回电源恢复原模式")
            .changed()
        {
            self.config.auto_switch_to_quiet_on_battery = quiet;
            self.save_state();
            // 立即生效：若当前在电池供电，马上按新配置重设硬件。
            // 注意用 apply_and_sync 而非 reapply_config：用户主动切换
            // 必须无条件应用，不受"电源切换时自动重设"开关约束。
            self.apply_config_and_sync();
        }

        // F-AUTO-01: 开机自启动复选框。注册/删除走后台线程
        // （UiCommand::SetAutostart）：**请求时即同步写入配置**（修订 1.25
        // 修复勾选闪烁与中途退出配置/task 背离），后台 worker 完成后再经
        // SetAutostartResult 回传结果（成功仅确认；失败回滚并展示错误）。
        let mut autostart = self.config.auto_start_on_boot;
        if ui.checkbox(&mut autostart, "开机自启动").changed() {
            let _ = self
                .cmd_tx
                .send(crate::command::UiCommand::SetAutostart(autostart));
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
        // 捕获模式开关 + 最近捕获事件展示。
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
                    ui.monospace(format!(
                        "{} / {}",
                        class,
                        ec::fnkey::FnKeyBinding::display_prefix(&hex)
                    ));
                });
                // 用捕获到的键直接添加绑定（无需从预设挑选）：捕获事件可能
                // 带后续状态/长度字节，取前 6 个 hex（如 012801 = 3 字节）
                // 作为前缀，既保留键码信息又不过度匹配。
                // 注意截断必须是**偶数字符**（完整字节）：hex 是半字节编码，
                // 奇数长度前缀（如 "01280"）匹配不到任何真实事件，且展示
                // 时会缺半个字节（L3 回归）。按字节截断：先取前 6 字符，
                // 若为奇数则回退到偶数位（等价于去掉末位半个字节）。
                let mut prefix_len = hex.len().min(6);
                if prefix_len % 2 != 0 {
                    prefix_len -= 1;
                }
                // 防御：归一化后至少 2 字符（1 字节）才可作为前缀，否则空
                // 前缀会"匹配一切"，属于危险配置（见 config.rs 绑定消毒）。
                // 极端单字符输入（长度 1）时回退到**偶数长度**（0）而非
                // 保留整个奇数串——单 hex 字符（如 "A"）会匹配所有以 A
                // 开头的事件，与空前缀同属危险配置。
                let prefix = if prefix_len >= 2 {
                    &hex[..prefix_len]
                } else {
                    &hex[..hex.len() - hex.len() % 2]
                };
                // 动作选择必须保存在 self 上（每帧 UI 重建，局部变量会
                // 重置回默认——历史实现用局部变量导致用户选中的动作在
                // 下一帧丢失，"使用此键"恒绑定默认动作，H1 回归）。
                let mut action = self.fn_capture_action;
                ui.horizontal(|ui| {
                    ui.label("绑定为:");
                    egui::ComboBox::from_id_salt("fn_capture_action")
                        .selected_text(action.name())
                        .show_ui(ui, |ui| {
                            for a in crate::ec::fnkey::FnAction::all() {
                                ui.selectable_value(&mut action, *a, a.name());
                            }
                        });
                    self.fn_capture_action = action;
                    // RunCommand 动作：附带命令行输入框（草稿跨帧保持，
                    // 与添加绑定流程的 fn_add_command 同理；否则捕获绑定
                    // 到的 RunCommand 命令为空、需事后到列表再改一次）。
                    if action == crate::ec::fnkey::FnAction::RunCommand {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.fn_capture_command)
                                .hint_text("例如 start notepad 或 C:\\tools\\app.exe")
                                .desired_width(200.0),
                        );
                    }
                    if ui.button("使用此键").clicked() {
                        let command = self.fn_capture_command.clone();
                        // 仅绑定真正写入（返回 true）时清空草稿：若校验拒绝
                        // （罕见：捕获产物异常），保留用户输入便于重试（修订
                        // 1.33 回归修正）。
                        if self.add_fn_binding(&class, prefix, action, &command) {
                            self.fn_capture_command.clear();
                        }
                    }
                });
            } else {
                ui.colored_label(Color32::GRAY, "（等待功能键事件…）");
            }
        }

        // 绑定列表：每条一行（展示 + 动作下拉 + 删除）。
        // 删除会前移后续条目下标，当前 for 的索引序列在删除后失效——
        // 用 mutated 标记跳出循环，下一帧用更新后的列表重新渲染。
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
            let label = format!(
                "{}. {}",
                i + 1,
                ec::fnkey::FnKeyBinding {
                    class: class.clone(),
                    prefix: prefix.clone(),
                    action: *action,
                    command: None,
                }
                .label()
            );
            ui.horizontal(|ui| {
                ui.label(label);
                let mut selected_action = *action;
                egui::ComboBox::from_id_salt(ui.id().with(format!("fn_action_{}", i)))
                    .selected_text(selected_action.name())
                    .show_ui(ui, |ui| {
                        for a in ec::fnkey::FnAction::all() {
                            ui.selectable_value(&mut selected_action, *a, a.name());
                        }
                    });
                if selected_action != *action {
                    self.set_fn_binding_action(i, selected_action);
                }
                // RunCommand 动作：展示命令行输入框（其余动作不展示）。
                // 输入框宽度有限，放在下一行水平组避免挤爆当前行；失去焦点
                // 时若内容有变化才落盘（避免每帧都触发 save_state）。
                if selected_action == ec::fnkey::FnAction::RunCommand {
                    let mut cmd_text = command.clone().unwrap_or_default();
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut cmd_text)
                            .hint_text("例如 start notepad 或 C:\\tools\\app.exe")
                            .desired_width(240.0),
                    );
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

        // 添加绑定：预设键码下拉 + 动作 + 添加按钮。
        // 选择状态必须保存在 self 上（每帧 UI 重新构建，局部变量每帧重置
        // 回默认值导致下拉永远显示第一项、选中无法保持——见 fn_add_* 修复）。
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("添加:");
            let mut selected = self.fn_add_preset_index;
            egui::ComboBox::from_id_salt("fn_add_preset")
                .selected_text(crate::ec::fnkey::KNOWN_FN_KEYS[selected].name)
                .show_ui(ui, |ui| {
                    for (idx, k) in crate::ec::fnkey::KNOWN_FN_KEYS.iter().enumerate() {
                        ui.selectable_value(&mut selected, idx, k.name);
                    }
                });
            self.fn_add_preset_index = selected;
            let mut add_action = self.fn_add_action;
            egui::ComboBox::from_id_salt("fn_add_action")
                .selected_text(add_action.name())
                .show_ui(ui, |ui| {
                    for a in crate::ec::fnkey::FnAction::all() {
                        ui.selectable_value(&mut add_action, *a, a.name());
                    }
                });
            self.fn_add_action = add_action;
            // RunCommand 动作：附带命令行输入框（草稿跨帧保持，见
            // app.rs 的 fn_add_command 注释）；其余动作不展示。
            if add_action == ec::fnkey::FnAction::RunCommand {
                ui.add(
                    egui::TextEdit::singleline(&mut self.fn_add_command)
                        .hint_text("例如 start notepad 或 C:\\tools\\app.exe")
                        .desired_width(200.0),
                );
            }
            if ui.button("添加绑定").clicked() {
                let k = &crate::ec::fnkey::KNOWN_FN_KEYS[selected];
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

pub fn titlebar_button(ui: &mut egui::Ui, size: egui::Vec2, kind: &str) -> egui::Response {
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
        "close" => {
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
        "minimize" => {
            let half = pad * 0.4;
            painter.line_segment(
                [egui::pos2(cx - half, cy), egui::pos2(cx + half, cy)],
                stroke,
            );
        }
        "maximize" => {
            let half = pad * 0.45;
            let r = egui::Rect::from_center_size(
                egui::pos2(cx, cy),
                egui::vec2(half * 2.0, half * 2.0),
            );
            painter.rect_stroke(r, 2.0, stroke, egui::StrokeKind::Inside);
        }
        "restore" => {
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
