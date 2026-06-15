use eframe::egui;
use windows::Win32::System::Power::GetSystemPowerStatus;

use crate::command::UiCommand;
use crate::ec;
use crate::ec::config::BackendPreference;
use crate::ec::performance::PerfMode;

use super::app::XiaomiApp;

const PERF_CYCLE: [PerfMode; 3] = [PerfMode::Smart, PerfMode::Quiet, PerfMode::Extreme];

impl XiaomiApp {
    pub fn process_commands(&mut self, ctx: &egui::Context) {
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
                    self.set_battery_care_internal(!self.battery_care_enabled);
                }
                UiCommand::CyclePerfMode => {
                    let current_raw = self.performance_mode;
                    let current = PerfMode::from_ec_value(current_raw).unwrap_or(PerfMode::Smart);
                    let next_raw = if current == PERF_CYCLE[0] {
                        PERF_CYCLE[1] as u8
                    } else if current == PERF_CYCLE[1] {
                        PERF_CYCLE[2] as u8
                    } else {
                        PERF_CYCLE[0] as u8
                    };
                    let ac_online = ac_power_status();
                    let next_val = if next_raw == PerfMode::Extreme as u8 && !ac_online {
                        PerfMode::Fast as u8
                    } else {
                        next_raw
                    };
                    self.set_perf_mode_internal(next_val);
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

    pub fn set_battery_care_internal(&mut self, enabled: bool) {
        let new_val = enabled;
        match self.backend.set_battery_care(new_val) {
            Ok(_) => log::info!("Battery care set to {}", if new_val { "enabled" } else { "disabled" }),
            Err(e) => log::error!("Failed to set battery care: {}", e),
        }
        let limit = if new_val { self.config.battery_charge_limit } else { 100 };
        match self.backend.set_charge_limit(limit) {
            Ok(_) => log::info!("Charge limit set to {}%", limit),
            Err(e) => log::error!("Failed to set charge limit: {}", e),
        }
        self.config.battery_care_enabled = new_val;
        self.config.battery_charge_limit = limit;
        self.battery_care_enabled = new_val;
        self.charge_limit = limit;
        self.save_state();
    }

    pub fn set_charge_limit_internal(&mut self, limit: u8) {
        let limit = limit.min(100);
        match self.backend.set_charge_limit(limit) {
            Ok(_) => log::info!("Charge limit set to {}%", limit),
            Err(e) => log::error!("Failed to set charge limit: {}", e),
        }
        self.charge_limit = limit;
        self.config.battery_charge_limit = limit;
        self.save_state();
    }

    pub fn set_perf_mode_internal(&mut self, mode: u8) {
        let mode_name = PerfMode::from_ec_value(mode)
            .map(|m| m.name())
            .unwrap_or("未知");
        match self.backend.set_performance_mode(mode) {
            Ok(_) => log::info!("Performance mode set to {} ({:#x})", mode_name, mode),
            Err(e) => log::error!("Failed to set performance mode: {}", e),
        }
        self.performance_mode = mode;
        self.config.performance_mode = mode;
        self.save_state();
    }

    pub fn try_switch_backend(&mut self, pref: BackendPreference) -> bool {
        match ec::backend::create_backend(pref) {
            Ok(new_backend) => {
                log::info!("Switched EC backend to: {}", new_backend.name());
                self.backend = new_backend;
                self.backend_name = self.backend.name().to_string();
                self.current_pref = pref;
                self.config.backend = self.current_pref;
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

    pub fn refresh_from_backend(&mut self) {
        let mut errors: Vec<String> = Vec::new();
        match self.backend.get_performance_mode() {
            Ok(mode) => {
                self.performance_mode = mode;
                self.config.performance_mode = mode;
            }
            Err(e) => errors.push(format!("读取性能模式: {}", e)),
        }
        match self.backend.get_battery_care_enabled() {
            Ok(enabled) => {
                self.battery_care_enabled = enabled;
                self.config.battery_care_enabled = enabled;
            }
            Err(e) => errors.push(format!("读取电池养护: {}", e)),
        }
        match self.backend.get_charge_limit() {
            Ok(limit) => {
                self.charge_limit = limit;
                self.config.battery_charge_limit = limit;
            }
            Err(e) => errors.push(format!("读取充电上限: {}", e)),
        }
        if errors.is_empty() {
            self.save_state();
        }
        if !errors.is_empty() {
            self.error_msg = Some(errors.join("; "));
        } else {
            self.error_msg = None;
        }
    }

    pub(crate) fn save_state(&self) {
        if let Err(e) = self.config.save() {
            log::error!("save config: {}", e);
        }
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
