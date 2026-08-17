use eframe::egui;

use crate::command::UiCommand;
use crate::ec;
use crate::ec::config::BackendPreference;
use crate::ec::performance::{ac_power_status, effective_ec_value, PerfMode};

use super::app::XiaomiApp;

const PERF_CYCLE: [PerfMode; 3] = [PerfMode::Smart, PerfMode::Quiet, PerfMode::Extreme];

impl XiaomiApp {
    pub fn process_commands(&mut self, ctx: &egui::Context) {
        let mut needs_repaint = false;
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            needs_repaint = true;
            match cmd {
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
                    self.set_perf_mode_internal(next_raw);
                }
                UiCommand::ReapplyConfig => {
                    if self.config.auto_reapply_on_power_change {
                        log::info!("Reapplying config on power change");
                        // Keep battery care and limit coherent (see
                        // apply_startup_config in main.rs).
                        let mut errs: Vec<String> = Vec::new();
                        if self.config.battery_care_enabled {
                            // 与启动 apply/GUI 切换路径一致：养护开启时上限
                            // 必须 < 100%，非自洽的旧配置兜底为 80%。
                            // 注意：兜底只能作用在局部变量上，**不得**提前改写
                            // config——若写入失败，else 路径不会保存配置，内存
                            // 中的 config 若已被改成 80 就会与未更新的 care 状态
                            // 构成矛盾，随后任何一次 save_state 都会把"用户期望
                            // 100% 但写入失败"的状态静默持久化为 80%（与
                            // set_battery_care_internal 的兜底规则一致）。
                            let mut limit = self.config.battery_charge_limit;
                            if limit >= 100 {
                                limit = 80;
                            }
                            if let Err(e) = self.backend.set_charge_limit(limit) {
                                log::error!("Reapply charge limit: {}", e);
                                errs.push(format!("重设充电上限失败: {}", e));
                            } else {
                                // 与 set_charge_limit_internal / 启动同步
                                // （sync_startup_config）的读回约定一致：WMI 后端
                                // 会把非预设值量化到最近的预设（如 85→80），成功写入
                                // 后必须读回硬件实际生效值再持久化，否则 config 与
                                // 硬件长期背离——每次电源切换都重复量化写入，UI
                                // 滑块显示硬件值（80）而配置仍是 85。
                                // 仅当应用值 <100%（养护开启）时同步；读回 100%
                                // 意味着写入被硬件拒绝（养护未生效），保留用户
                                // 期望值，避免 care=true + limit=100 的矛盾配置。
                                let applied =
                                    self.backend.get_charge_limit().unwrap_or(limit).min(100);
                                if applied < 100 {
                                    self.config.battery_charge_limit = applied;
                                }
                            }
                        } else if let Err(e) = self.backend.set_charge_limit(100) {
                            log::error!("Reapply charge limit: {}", e);
                            errs.push(format!("重设充电上限失败: {}", e));
                        }
                        if let Err(e) = self.backend.set_battery_care(self.config.battery_care_enabled) {
                            log::error!("Reapply battery care: {}", e);
                            errs.push(format!("重设电池养护失败: {}", e));
                        }
                        // 狂暴模式需要交流电源：与启动应用路径保持一致的保护。
                        let raw = effective_ec_value(self.config.performance_mode, ac_power_status());
                        if let Err(e) = self.backend.set_performance_mode(raw) {
                            log::error!("Reapply perf mode: {}", e);
                            errs.push(format!("重设性能模式失败: {}", e));
                        }
                        // 规范化（如 care=true + limit=100 兜底为 80）修改了配置，
                        // 需要持久化，否则配置文件中残留的矛盾组合每次都会被重写。
                        self.save_state();
                        self.refresh_from_backend();
                        // refresh_from_backend 成功时会清空 error_msg，写入失败
                        // 必须在其后合并展示（F-ERR-03），否则错误被静默吞掉。
                        if !errs.is_empty() {
                            self.error_msg = Some(match self.error_msg.take() {
                                Some(existing) => format!("{}; {}", existing, errs.join("; ")),
                                None => errs.join("; "),
                            });
                        }
                    }
                }
                UiCommand::SetAutostart(enabled) => {
                    // 计划任务注册/删除走后台线程：ITaskService 需要在本线程
                    // 初始化 COM，GUI 线程的 COM 状态由 eframe/winit 管理，
                    // 不得污染（见 21e0aaf 的公寓冲突教训）。
                    let tx = self.cmd_tx.clone();
                    std::thread::spawn(move || {
                        let result = if enabled {
                            crate::platform::autostart::enable()
                        } else {
                            crate::platform::autostart::disable()
                        };
                        let _ = tx.send(UiCommand::SetAutostartResult(enabled, result));
                    });
                }
                UiCommand::SetAutostartResult(enabled, result) => {
                    match result {
                        Ok(()) => {
                            log::info!("Autostart set to {}", enabled);
                            self.config.auto_start_on_boot = enabled;
                            self.save_state();
                        }
                        Err(e) => {
                            log::error!("Autostart operation failed: {}", e);
                            self.push_error(format!("设置开机自启动失败: {}", e));
                        }
                    }
                }
            }
        }
        if needs_repaint {
            ctx.request_repaint();
        }
    }

    pub fn set_battery_care_internal(&mut self, enabled: bool) {
        // When disabling, only the hardware limit is raised to 100%; the
        // persisted desired limit must be kept so it is not lost when battery
        // care is re-enabled later.
        // 注意：兜底 80% 只能作用在局部变量上，不能提前改写 config——若随后
        // 硬件写入失败（limit_ok=false），else 分支不会保存配置，内存中的
        // config 若已被改成 80 就与未更新的 care=false 构成矛盾状态，后续
        // 任何一次 save_state 都会把"用户期望 100%、改写失败"静默持久化为
        // 80%。成功路径会在下方用读回值统一落盘，这里无需预写。
        let desired_limit = self.config.battery_charge_limit;
        let limit = if enabled {
            if desired_limit >= 100 {
                80
            } else {
                desired_limit
            }
        } else {
            100
        };
        // Write charge limit first; some EC firmware auto-syncs the battery
        // care bit from the charge limit register.  Touching the limit first
        // lets us clear the bit afterwards without the EC re-asserting it.
        let limit_ok = match self.backend.set_charge_limit(limit) {
            Ok(_) => {
                log::info!("Charge limit set to {}%", limit);
                true
            }
            Err(e) => {
                log::error!("Failed to set charge limit: {}", e);
                false
            }
        };
        let care_ok = match self.backend.set_battery_care(enabled) {
            Ok(_) => {
                log::info!("Battery care set to {}", if enabled { "enabled" } else { "disabled" });
                true
            }
            Err(e) => {
                log::error!("Failed to set battery care: {}", e);
                false
            }
        };
        if limit_ok {
            // 限值是两种后端判定养护状态的权威依据（WMI 养护位由限值<100%
            // 推导，WinRing0 读回亦按限值）：set_charge_limit 成功即硬件养护
            // 状态已按限值生效。即使 set_battery_care 失败，状态与持久化配置
            // 也必须按限值保持自洽——否则下次启动会按旧配置（care=false）
            // 强制写 100%，静默覆盖用户刚设置的限值。
            let applied = self.backend.get_charge_limit().unwrap_or(limit).min(100);
            self.charge_limit = applied;
            // When disabling, keep the stored limit as the desired value for
            // when care is re-enabled; when enabling, sync it to the applied
            // value so the persisted config matches the hardware.
            if enabled {
                self.config.battery_charge_limit = applied;
            }
            self.config.battery_care_enabled = enabled;
            self.battery_care_enabled = enabled;
            if !care_ok {
                // 与 set_charge_limit_internal 的联动失败处理一致：限值才是
                // 权威依据，写入失败仅告警（F-ERR-03），状态保持自洽。
                self.push_error("设置电池养护失败".to_string());
            }
            self.save_state();
        } else {
            // 限值写入失败时不得更新状态：硬件未变更，UI 与配置保持一致，
            // 错误在 GUI 中展示（F-ERR-03）。
            let mut errs = Vec::new();
            errs.push("设置充电上限失败".to_string());
            if !care_ok {
                errs.push("设置电池养护失败".to_string());
            }
            self.push_error(errs.join("; "));
        }
    }

    pub fn set_charge_limit_internal(&mut self, limit: u8) {
        let limit = limit.min(100);
        if let Err(e) = self.backend.set_charge_limit(limit) {
            log::error!("Failed to set charge limit: {}", e);
            self.push_error(format!("设置充电上限失败: {}", e));
            return;
        }
        log::info!("Charge limit set to {}%", limit);
        // Read back the value the hardware actually applied: the WMI backend
        // only accepts preset values (nearest one is chosen), so the applied
        // value may differ from the requested one.  Using the read-back value
        // keeps the UI and the persisted config coherent with the hardware
        // (AC-BAT-04: 设置 85% 应显示并保存硬件实际生效的 80%).
        let applied = self.backend.get_charge_limit().unwrap_or(limit).min(100);
        self.charge_limit = applied;
        // 100% = 养护关闭（与 set_battery_care_internal 关闭路径/sync_startup_config
        // 的约定一致）：此时必须**保留** config 中用户期望的上限，供重新开启养护
        // 时恢复。若把 applied=100 写回 config，用户的期望值（如 60%）会被覆盖为
        // 100%，重新开启养护时只能走"≥100 兜底 80%"分支，用户原设置永久丢失。
        // 只有当应用值 <100%（养护开启）时才把硬件实际生效值同步进配置。
        if applied < 100 {
            self.config.battery_charge_limit = applied;
        }
        // On both backends the charge limit is the authoritative battery-care
        // control: WMI has no separate care bit, and WinRing0 derives care
        // from the limit.  Keep the UI flag consistent with the limit.
        let care = applied < 100;
        if care != self.battery_care_enabled {
            match self.backend.set_battery_care(care) {
                Ok(_) => {
                    log::info!(
                        "Battery care {} (synced from charge limit)",
                        if care { "enabled" } else { "disabled" }
                    );
                    self.battery_care_enabled = care;
                }
                Err(e) => {
                    // The limit was already applied and is the authoritative
                    // control on both backends, so keep the state coherent
                    // with the limit even though the care bit write failed.
                    // Otherwise the persisted config would be inconsistent
                    // (care off with a sub-100% limit) and the next startup
                    // would force the limit back to 100%, silently destroying
                    // the user's choice.
                    log::warn!(
                        "Failed to sync battery care bit: {}; deriving care from charge limit",
                        e
                    );
                    self.battery_care_enabled = care;
                    self.push_error(format!("同步电池养护状态失败: {}", e));
                }
            }
        }
        // The limit must always be mirrored to the persisted care flag, even
        // when the runtime flag already matched the limit: the runtime state
        // (from refresh_from_backend) can diverge from the persisted config
        // (e.g. auto_apply off with the hardware state changed externally).
        // Without this, config would keep care=false while the limit is
        // sub-100%, and the next startup would write 100% and silently
        // destroy the user's limit.
        self.config.battery_care_enabled = care;
        self.save_state();
    }

    pub fn set_perf_mode_internal(&mut self, mode: u8) {
        // 写入硬件时按电源状态选择实际 raw code（狂暴模式需要交流电源），
        // 但用户的选择原样保存，以便重新插电/重启后能恢复狂暴模式。
        let raw = effective_ec_value(mode, ac_power_status());
        if raw != mode {
            log::warn!("Extreme mode requires AC power; using Fast mode instead");
        }
        let mode_name = PerfMode::from_ec_value(raw)
            .map(|m| m.name())
            .unwrap_or("未知");
        match self.backend.set_performance_mode(raw) {
            Ok(_) => {
                log::info!("Performance mode set to {} ({:#x})", mode_name, raw);
                self.performance_mode = mode;
                self.config.performance_mode = mode;
                self.save_state();
            }
            Err(e) => {
                log::error!("Failed to set performance mode: {}", e);
                self.push_error(format!("设置性能模式失败: {}", e));
            }
        }
    }

    pub fn try_switch_backend(&mut self, pref: BackendPreference) -> bool {
        match ec::backend::create_backend(pref) {
            Ok(new_backend) => self.apply_backend_switch(new_backend, pref),
            Err(e) => {
                log::error!("Failed to switch EC backend: {}", e);
                self.push_error(format!("后端切换失败: {}", e));
                false
            }
        }
    }

    /// 完成后端切换的公共逻辑（create_backend 之外的部分），单独抽出便于测试。
    /// 注意：不得在 refresh_from_backend() 之后清空 error_msg —— 刷新产生的
    /// 读取失败必须保留并在 GUI 中展示（F-ERR-03）；刷新成功时它自会清空。
    fn apply_backend_switch(&mut self, new_backend: Box<dyn ec::backend::EcBackend>, pref: BackendPreference) -> bool {
        log::info!("Switched EC backend to: {}", new_backend.name());
        self.backend = new_backend;
        self.backend_name = self.backend.name().to_string();
        self.current_pref = pref;
        self.config.backend = self.current_pref;
        if let Err(e) = self.config.save() {
            log::error!("save config: {}", e);
        }
        self.refresh_from_backend();
        true
    }

    pub fn refresh_from_backend(&mut self) {
        let mut errors: Vec<String> = Vec::new();
        match self.backend.get_performance_mode() {
            Ok(mode) => {
                self.performance_mode = mode;
            }
            Err(e) => errors.push(format!("读取性能模式: {}", e)),
        }
        match self.backend.get_battery_state() {
            Ok((care, limit)) => {
                // 钳制到 [0,100]：损坏的 EC 读值（如 0xFF=255）不得显示为
                // "充电上限: 255%" 或使滑块/养护位推导溢出。上限超过 100
                // 视为垃圾值钳到 100。
                self.battery_care_enabled = care;
                self.charge_limit = limit.min(100);
            }
            Err(e) => errors.push(format!("读取电池状态: {}", e)),
        }
        // Only the runtime state is refreshed here; the persisted config keeps
        // the user's desired settings so they are not silently overwritten by
        // whatever the hardware currently reports.
        if errors.is_empty() {
            self.error_msg = None;
        } else {
            self.error_msg = Some(errors.join("; "));
        }
    }

    pub(crate) fn save_state(&self) {
        if let Err(e) = self.config.save() {
            log::error!("save config: {}", e);
        }
    }

    /// 合并错误信息而非覆盖：F-ERR-03 要求所有硬件操作失败都应在 GUI 中
    /// 展示，覆盖式写入会让较早的错误（如刷新时的读取失败）被静默丢弃。
    /// 与 refresh_from_backend 合并 init_error 的模式保持一致。
    pub(crate) fn push_error(&mut self, msg: String) {
        self.error_msg = Some(match self.error_msg.take() {
            Some(existing) => format!("{}; {}", existing, msg),
            None => msg,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ec::backend::EcBackend;
    use crate::ec::config::AppConfig;
    use crate::ec::error::EcError;

    /// In-memory backend that records writes so the GUI logic can be tested
    /// without touching real hardware.  State is shared via Arc so tests can
    /// inspect what the backend reports/recorded.
    #[derive(Clone, Default)]
    struct MockBackend {
        charge_limit: std::sync::Arc<std::sync::atomic::AtomicU8>,
        battery_care: std::sync::Arc<std::sync::atomic::AtomicBool>,
        perf_mode: std::sync::Arc<std::sync::atomic::AtomicU8>,
        /// get_battery_state 调用计数（回归测试：刷新必须单次往返）。
        battery_state_calls: std::sync::Arc<std::sync::atomic::AtomicU32>,
    }

    impl EcBackend for MockBackend {
        fn name(&self) -> &'static str {
            "mock"
        }
        fn read_byte(&self, _addr: u16) -> Result<u8, EcError> {
            Err(EcError::BackendUnavailable("mock".into()))
        }
        fn write_byte(&self, _addr: u16, _value: u8) -> Result<(), EcError> {
            Ok(())
        }
        fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
            Ok(self.battery_care.load(std::sync::atomic::Ordering::Relaxed))
        }
        fn get_charge_limit(&self) -> Result<u8, EcError> {
            Ok(self.charge_limit.load(std::sync::atomic::Ordering::Relaxed))
        }
        fn get_battery_state(&self) -> Result<(bool, u8), EcError> {
            self.battery_state_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok((
                self.battery_care.load(std::sync::atomic::Ordering::Relaxed),
                self.charge_limit.load(std::sync::atomic::Ordering::Relaxed),
            ))
        }
        fn set_battery_care(&self, enabled: bool) -> Result<(), EcError> {
            self.battery_care.store(enabled, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        fn set_charge_limit(&self, percent: u8) -> Result<(), EcError> {
            self.charge_limit.store(percent, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        fn get_performance_mode(&self) -> Result<u8, EcError> {
            Ok(self.perf_mode.load(std::sync::atomic::Ordering::Relaxed))
        }
        fn set_performance_mode(&self, mode: u8) -> Result<(), EcError> {
            self.perf_mode.store(mode, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    /// Point the config writer at a temp directory so save_state() never
    /// touches the user's real config during tests.
    fn redirect_config_dir() {
        let dir = std::env::temp_dir().join(format!("xmpl-test-{}", std::process::id()));
        std::env::set_var("XIAOMI_PC_MANAGER_CONFIG_DIR", dir);
    }

    /// Backend that fails every operation, used to verify that failed writes
    /// never corrupt the UI state or the persisted config.
    struct FailingBackend;

    impl EcBackend for FailingBackend {
        fn name(&self) -> &'static str {
            "failing"
        }
        fn read_byte(&self, _addr: u16) -> Result<u8, EcError> {
            Err(EcError::BackendUnavailable("failing".into()))
        }
        fn write_byte(&self, _addr: u16, _value: u8) -> Result<(), EcError> {
            Err(EcError::BackendUnavailable("failing".into()))
        }
        fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
            Err(EcError::BackendUnavailable("failing".into()))
        }
        fn get_charge_limit(&self) -> Result<u8, EcError> {
            Err(EcError::BackendUnavailable("failing".into()))
        }
        fn set_battery_care(&self, _enabled: bool) -> Result<(), EcError> {
            Err(EcError::BackendUnavailable("failing".into()))
        }
        fn set_charge_limit(&self, _percent: u8) -> Result<(), EcError> {
            Err(EcError::BackendUnavailable("failing".into()))
        }
        fn get_performance_mode(&self) -> Result<u8, EcError> {
            Err(EcError::BackendUnavailable("failing".into()))
        }
        fn set_performance_mode(&self, _mode: u8) -> Result<(), EcError> {
            Err(EcError::BackendUnavailable("failing".into()))
        }
    }

    fn failing_app() -> XiaomiApp {
        redirect_config_dir();
        XiaomiApp::new(
            Box::new(FailingBackend),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        )
    }

    fn test_app() -> XiaomiApp {
        redirect_config_dir();
        XiaomiApp::new(
            Box::new(MockBackend::default()),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        )
    }

    #[test]
    fn test_perf_mode_selection_is_preserved_on_battery() {
        let mut app = test_app();
        // 电池供电下选择狂暴：硬件写入极速，但用户选择与配置保持狂暴。
        app.set_perf_mode_internal(PerfMode::Extreme as u8);
        assert_eq!(app.performance_mode, PerfMode::Extreme as u8);
        assert_eq!(app.config.performance_mode, PerfMode::Extreme as u8);
    }

    #[test]
    fn test_perf_mode_normal_selection_not_changed() {
        let mut app = test_app();
        app.set_perf_mode_internal(PerfMode::Quiet as u8);
        assert_eq!(app.performance_mode, PerfMode::Quiet as u8);
        assert_eq!(app.config.performance_mode, PerfMode::Quiet as u8);
    }

    /// 回归测试：ToggleWindow 改为基于真实窗口可见性的隐藏/显示
    /// 回归测试：SetAutostart 命令在无窗口环境下安全处理。
    #[test]
    fn test_battery_care_toggle_preserves_desired_limit() {
        let mut app = test_app();
        app.config.battery_charge_limit = 60;

        app.set_battery_care_internal(true);
        assert!(app.battery_care_enabled);
        assert_eq!(app.charge_limit, 60);
        assert_eq!(app.config.battery_charge_limit, 60);

        // Disabling raises the hardware limit to 100% but must keep the
        // desired limit so it is not lost.
        app.set_battery_care_internal(false);
        assert!(!app.battery_care_enabled);
        assert_eq!(app.charge_limit, 100);
        assert_eq!(app.config.battery_charge_limit, 60);

        // Re-enabling must restore the desired limit, not the 100% hardware
        // limit left behind by the disable.
        app.set_battery_care_internal(true);
        assert!(app.battery_care_enabled);
        assert_eq!(app.charge_limit, 60);
        assert_eq!(app.config.battery_charge_limit, 60);
    }

    #[test]
    fn test_battery_care_enable_falls_back_to_80_when_limit_is_100() {
        let mut app = test_app();
        app.config.battery_charge_limit = 100;

        app.set_battery_care_internal(true);
        assert!(app.battery_care_enabled);
        assert_eq!(app.charge_limit, 80);
        assert_eq!(app.config.battery_charge_limit, 80);
    }

    #[test]
    fn test_charge_limit_syncs_battery_care_flag() {
        let mut app = test_app();
        app.battery_care_enabled = true;

        // 100% limit means battery care is off.
        app.set_charge_limit_internal(100);
        assert!(!app.battery_care_enabled);
        assert_eq!(app.charge_limit, 100);
        // 关闭养护时保留用户期望值（默认 80），供重新开启养护时恢复。
        assert_eq!(app.config.battery_charge_limit, 80);

        // A limit below 100% turns battery care back on.
        app.set_charge_limit_internal(90);
        assert!(app.battery_care_enabled);
        assert_eq!(app.charge_limit, 90);
        assert_eq!(app.config.battery_charge_limit, 90);
    }

    /// 回归测试：把上限拖到 100%（养护关闭）不得把 config 中用户期望的上限
    /// 覆盖为 100%。历史实现无条件写回 config.battery_charge_limit=applied，
    /// 用户从 60% 拖到 100% 后期望值被永久改写为 100，重新开启养护时只能走
    /// "≥100 兜底 80%"分支，60% 的原设置丢失——与 set_battery_care_internal
    /// 关闭路径（保留期望上限）和 sync_startup_config 的约定不一致。
    #[test]
    fn test_charge_limit_to_100_preserves_desired_limit() {
        let mut app = test_app();
        // 用户养护开启、期望上限 60%。
        app.config.battery_charge_limit = 60;
        app.charge_limit = 60;
        app.battery_care_enabled = true;

        // 拖到 100%：硬件提升到 100（养护关闭），但 config 期望值保留 60。
        app.set_charge_limit_internal(100);
        assert!(!app.battery_care_enabled);
        assert_eq!(app.charge_limit, 100);
        assert_eq!(app.config.battery_charge_limit, 60, "desired limit must be preserved");

        // 重新开启养护：恢复 60% 而不是兜底 80%。
        app.set_battery_care_internal(true);
        assert!(app.battery_care_enabled);
        assert_eq!(app.charge_limit, 60);
        assert_eq!(app.config.battery_charge_limit, 60);
    }

    /// 回归测试：运行时养护状态与持久化配置不一致（auto_apply 关闭且硬件
    /// 状态被外部改动时，refresh_from_backend 只更新运行时）时，拖动上限后
    /// 持久化配置必须与限值保持自洽。否则下次启动 apply_startup_config 会
    /// 按 care=false 强制写 100%，静默摧毁用户设置的充电上限。
    #[test]
    fn test_charge_limit_sync_persists_care_when_runtime_diverged() {
        redirect_config_dir();
        let mock = MockBackend::default();
        let mut app = XiaomiApp::new(
            Box::new(mock.clone()),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        // 模拟：硬件养护已开启（limit=60），持久化配置仍是旧值
        // care=false, limit=100（如 auto_apply 关闭时外部改动硬件）。
        mock.charge_limit.store(60, std::sync::atomic::Ordering::Relaxed);
        mock.battery_care.store(true, std::sync::atomic::Ordering::Relaxed);
        app.refresh_from_backend();
        assert!(app.battery_care_enabled);
        assert!(!app.config.battery_care_enabled);

        // 用户把上限拖到 80：运行时 care 未变化（无写入分支），但持久化
        // 配置必须同步为 care=true, limit=80，保持自洽。
        app.set_charge_limit_internal(80);
        assert!(app.config.battery_care_enabled);
        assert_eq!(app.config.battery_charge_limit, 80);

        // 模拟下次启动 apply_startup_config：care=true → 写回 80%，
        // 用户设置不会被 100% 覆盖。
        assert_eq!(mock.charge_limit.load(std::sync::atomic::Ordering::Relaxed), 80);
    }

    #[test]
    fn test_refresh_from_backend_keeps_config_untouched() {
        redirect_config_dir();
        let mock = MockBackend::default();
        let mut app = XiaomiApp::new(
            Box::new(mock.clone()),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        app.config.battery_charge_limit = 60;
        app.config.battery_care_enabled = false;

        // Mock backend reports a different hardware state than the config.
        mock.battery_care.store(true, std::sync::atomic::Ordering::Relaxed);
        mock.charge_limit.store(80, std::sync::atomic::Ordering::Relaxed);
        mock.perf_mode.store(PerfMode::Quiet as u8, std::sync::atomic::Ordering::Relaxed);

        app.refresh_from_backend();
        assert!(app.battery_care_enabled);
        assert_eq!(app.charge_limit, 80);
        assert_eq!(app.performance_mode, PerfMode::Quiet as u8);
        assert!(app.error_msg.is_none());

        // The persisted desired settings must not be overwritten.
        assert_eq!(app.config.battery_charge_limit, 60);
        assert!(!app.config.battery_care_enabled);
    }

    /// 回归测试：损坏的 EC 读值（充电上限 >100，如寄存器返回 0xFF）不得
    /// 显示为荒谬百分比或使 UI 状态溢出——刷新时必须钳制到 100%。
    #[test]
    fn test_refresh_clamps_charge_limit_above_100() {
        redirect_config_dir();
        let mock = MockBackend::default();
        let mut app = XiaomiApp::new(
            Box::new(mock.clone()),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        mock.charge_limit.store(150, std::sync::atomic::Ordering::Relaxed);
        mock.battery_care.store(false, std::sync::atomic::Ordering::Relaxed);

        app.refresh_from_backend();

        assert_eq!(app.charge_limit, 100);
        assert!(!app.battery_care_enabled);
    }

    /// 回归测试（B-WMI-1）：刷新必须通过 get_battery_state 单次往返获取电池
    /// 数据。旧实现分别调用 get_battery_care_enabled + get_charge_limit，
    /// 在 WMI 后端下每次刷新做两次请求相同数据的完整 WMI 往返。
    #[test]
    fn test_refresh_uses_single_battery_roundtrip() {
        redirect_config_dir();
        let mock = MockBackend::default();
        let calls = mock.battery_state_calls.clone();
        let mut app = XiaomiApp::new(
            Box::new(mock.clone()),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        // 构造时已刷新过一次，清零后验证显式刷新只发一次电池往返。
        calls.store(0, std::sync::atomic::Ordering::Relaxed);
        mock.battery_care.store(true, std::sync::atomic::Ordering::Relaxed);
        mock.charge_limit.store(80, std::sync::atomic::Ordering::Relaxed);

        app.refresh_from_backend();

        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(app.battery_care_enabled);
        assert_eq!(app.charge_limit, 80);
        assert!(app.error_msg.is_none());
    }

    /// 回归测试：切换后端后，新后端读取失败产生的错误必须保留在 GUI 中展示
    /// （F-ERR-03），不得被切换逻辑清空（曾因 refresh 后无条件 error_msg=None
    /// 导致切换到一个读取全部失败的后端时错误信息被立即抹掉）。
    #[test]
    fn test_switch_backend_preserves_read_errors() {
        redirect_config_dir();
        let mut app = XiaomiApp::new(
            Box::new(MockBackend::default()),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        assert!(app.error_msg.is_none());

        // 切换到读取全部失败的后端：refresh_from_backend 会设置错误信息，
        // 切换逻辑不得将其清空。
        let ok = app.apply_backend_switch(Box::new(FailingBackend), BackendPreference::Wmi);
        assert!(ok);
        assert_eq!(app.backend_name, "failing");
        let err = app.error_msg.as_deref().unwrap_or_default();
        assert!(err.contains("读取性能模式"), "unexpected: {}", err);
        assert!(err.contains("读取电池状态"), "unexpected: {}", err);
    }

    /// 回归测试：电源切换重设失败时，错误必须合并进 GUI 展示（F-ERR-03），
    /// 且不得被 refresh_from_backend 成功时的 error_msg 清空逻辑吞掉。
    #[test]
    fn test_reapply_config_reports_write_errors() {
        redirect_config_dir();
        let mut app = failing_app();
        app.config.auto_reapply_on_power_change = true;

        let ctx = egui::Context::default();
        app.cmd_tx.send(UiCommand::ReapplyConfig).unwrap();
        app.process_commands(&ctx);

        let err = app.error_msg.as_deref().unwrap_or_default();
        assert!(err.contains("重设充电上限失败"), "unexpected: {}", err);
        assert!(err.contains("重设电池养护失败"), "unexpected: {}", err);
        assert!(err.contains("重设性能模式失败"), "unexpected: {}", err);
        assert!(err.contains("读取性能模式"), "read errors must be preserved: {}", err);
    }

    /// 回归测试：开启养护时 set_charge_limit 成功、但 set_battery_care 失败
    /// （如 EC 拒绝写入养护位）时，硬件限值已生效，UI/配置必须按限值保持
    /// 自洽（限值是两种后端判定养护状态的权威依据）。否则下次启动会按旧的
    /// care=false 强制写 100%，静默覆盖用户设置的限值。
    #[test]
    fn test_battery_care_enable_partial_failure_keeps_config_coherent() {
        redirect_config_dir();
        let mut app = XiaomiApp::new(
            Box::new(PartialCareBackend),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        // 模拟用户养护关闭、期望上限 60%。
        app.config.battery_charge_limit = 60;
        app.config.battery_care_enabled = false;
        app.battery_care_enabled = false;
        app.charge_limit = 100;

        // 开启养护：limit 写入成功（60%），care 位写入失败。
        app.set_battery_care_internal(true);

        // 限值已生效：状态与持久化配置必须按限值自洽（养护开启、上限 60），
        // 并且错误要在 GUI 展示。
        assert!(app.battery_care_enabled);
        assert!(app.config.battery_care_enabled);
        assert_eq!(app.charge_limit, 60);
        assert_eq!(app.config.battery_charge_limit, 60);
        assert!(app.error_msg.as_deref().unwrap_or_default().contains("设置电池养护失败"));

        // 模拟下次启动 apply_startup_config：care=true → set_charge_limit(60)，
        // 用户设置的 60% 不再被覆盖为 100%。
        let cfg = app.config.clone();
        assert!(cfg.battery_care_enabled);
        assert_eq!(cfg.battery_charge_limit, 60);
    }

    /// 回归测试：电源切换重设时，若旧版本/手改配置残留 care=true +
    /// limit=100 的矛盾组合，必须按 GUI 切换路径的规则兜底为 80% 写入
    /// 硬件——否则 WMI 会把 100% 写进硬件使养护失效，WinRing0 则会出现
    /// 养护位开启但上限 100% 的矛盾状态，且配置被静默改写。
    #[test]
    fn test_reapply_config_normalizes_incoherent_limit() {
        redirect_config_dir();
        let mock = MockBackend::default();
        let mut app = XiaomiApp::new(
            Box::new(mock.clone()),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        app.config.battery_care_enabled = true;
        app.config.battery_charge_limit = 100;

        let ctx = egui::Context::default();
        app.cmd_tx.send(UiCommand::ReapplyConfig).unwrap();
        app.process_commands(&ctx);

        // 配置与硬件都按 80% 处理，养护保持开启。
        assert_eq!(app.config.battery_charge_limit, 80);
        assert_eq!(mock.charge_limit.load(std::sync::atomic::Ordering::Relaxed), 80);
        assert!(mock.battery_care.load(std::sync::atomic::Ordering::Relaxed));
        assert!(app.error_msg.is_none());
    }

    #[test]
    fn test_reapply_config_write_failure_keeps_original_limit() {
        redirect_config_dir();
        let mut app = failing_app();
        app.config.auto_reapply_on_power_change = true;
        // 用户配置 care=true + limit=100（矛盾组合），但写入全部失败。
        app.config.battery_care_enabled = true;
        app.config.battery_charge_limit = 100;

        let ctx = egui::Context::default();
        app.cmd_tx.send(UiCommand::ReapplyConfig).unwrap();
        app.process_commands(&ctx);

        // 写入失败时不得把 config 静默改写为 80%（与 set_battery_care_internal
        // 的兜底规则一致），否则用户选择被破坏。
        assert_eq!(
            app.config.battery_charge_limit, 100,
            "config must not be normalized when the write failed"
        );
    }

    /// 回归测试：电源重设成功写入时，若后端量化（WMI 85%→80%），持久化配置
    /// 必须跟随硬件实际生效值，与 set_charge_limit_internal / 启动同步的读回
    /// 约定保持一致。历史实现把请求值（85%）直接持久化，config 与硬件长期
    /// 背离（每次电源切换重复量化写入，UI 滑块显示硬件值 80 而配置仍是 85）。
    #[test]
    fn test_reapply_config_syncs_quantized_limit() {
        redirect_config_dir();
        let backend = QuantizingBackend::new();
        let hw_limit = backend.charge_limit.clone();
        let mut app = XiaomiApp::new(
            Box::new(backend),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        app.config.auto_reapply_on_power_change = true;
        app.config.battery_care_enabled = true;
        app.config.battery_charge_limit = 85;

        let ctx = egui::Context::default();
        app.cmd_tx.send(UiCommand::ReapplyConfig).unwrap();
        app.process_commands(&ctx);

        assert_eq!(hw_limit.load(std::sync::atomic::Ordering::Relaxed), 80);
        assert_eq!(
            app.config.battery_charge_limit, 80,
            "config must follow the hardware-applied value after quantization"
        );
        assert!(app.error_msg.is_none());
    }

    #[test]
    fn test_failed_charge_limit_write_keeps_state_and_reports_error() {
        let mut app = failing_app();
        app.set_charge_limit_internal(60);

        // 写入失败：UI 状态与持久化配置必须保持原样，错误需在 GUI 展示。
        assert_eq!(app.charge_limit, 80);
        assert_eq!(app.config.battery_charge_limit, 80);
        assert!(app
            .error_msg
            .as_deref()
            .unwrap_or_default()
            .contains("设置充电上限失败"));
    }

    #[test]
    fn test_failed_battery_care_write_keeps_state_and_reports_error() {
        let mut app = failing_app();
        app.set_battery_care_internal(true);

        assert!(!app.battery_care_enabled);
        assert!(!app.config.battery_care_enabled);
        assert!(app
            .error_msg
            .as_deref()
            .unwrap_or_default()
            .contains("设置电池养护失败"));
    }

    /// 回归测试：开启养护时，配置上限 ≥100 触发的 80% 兜底只能作用在成功
    /// 路径；写入失败时，config 与 UI 必须保持原样，不允许内存中被提前改写
    /// 成 80（否则后续任何 save_state 都会把"用户期望 100% 但写入失败"的
    /// 状态静默持久化，破坏用户设置）。
    #[test]
    fn test_battery_care_fallback_write_failure_keeps_original_limit() {
        redirect_config_dir();
        let mut app = XiaomiApp::new(
            Box::new(FailingBackend),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        // 用户期望 100%（触发兜底分支），写入全部失败。
        app.config.battery_charge_limit = 100;
        app.charge_limit = 100;

        app.set_battery_care_internal(true);

        // config 不得被提前改写为 80：写入失败时保持用户原值。
        assert_eq!(app.config.battery_charge_limit, 100);
        assert_eq!(app.charge_limit, 100);
        assert!(!app.battery_care_enabled);
        assert!(!app.config.battery_care_enabled);
        assert!(app
            .error_msg
            .as_deref()
            .unwrap_or_default()
            .contains("设置充电上限失败"));
    }

    #[test]
    fn test_failed_perf_mode_write_keeps_state_and_reports_error() {
        let mut app = failing_app();
        app.set_perf_mode_internal(PerfMode::Quiet as u8);

        assert_eq!(app.performance_mode, PerfMode::Smart as u8);
        assert_eq!(app.config.performance_mode, PerfMode::Smart as u8);
        assert!(app
            .error_msg
            .as_deref()
            .unwrap_or_default()
            .contains("设置性能模式失败"));
    }

    /// Backend where set_charge_limit succeeds but set_battery_care always
    /// fails — models an EC where the limit register is writable but the care
    /// bit write is rejected (busy/rejected).
    struct PartialCareBackend;

    impl EcBackend for PartialCareBackend {
        fn name(&self) -> &'static str {
            "partial-care"
        }
        fn read_byte(&self, _addr: u16) -> Result<u8, EcError> {
            Err(EcError::BackendUnavailable("partial-care".into()))
        }
        fn write_byte(&self, _addr: u16, _value: u8) -> Result<(), EcError> {
            Ok(())
        }
        fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
            Err(EcError::BackendUnavailable("partial-care".into()))
        }
        fn get_charge_limit(&self) -> Result<u8, EcError> {
            Err(EcError::BackendUnavailable("partial-care".into()))
        }
        fn set_battery_care(&self, _enabled: bool) -> Result<(), EcError> {
            Err(EcError::BackendUnavailable("partial-care".into()))
        }
        fn set_charge_limit(&self, _percent: u8) -> Result<(), EcError> {
            Ok(())
        }
        fn get_performance_mode(&self) -> Result<u8, EcError> {
            Err(EcError::BackendUnavailable("partial-care".into()))
        }
        fn set_performance_mode(&self, _mode: u8) -> Result<(), EcError> {
            Err(EcError::BackendUnavailable("partial-care".into()))
        }
    }

    /// 回归测试：设置充电上限成功、但联动养护位写入失败时，配置必须保持
    /// 自洽（care 由限值推导），不允许出现 care=false + limit=60 的矛盾组合
    /// ——否则下次启动 auto_apply 会按 care=false 强制写 100%，
    /// 用户选择的 60% 充电上限被静默摧毁。
    #[test]
    fn test_charge_limit_care_sync_failure_keeps_config_coherent() {
        redirect_config_dir();
        let mut app = XiaomiApp::new(
            Box::new(PartialCareBackend),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        // 模拟用户当前养护关闭、上限 100%（运行时与配置一致）。
        app.battery_care_enabled = false;
        app.config.battery_care_enabled = false;
        app.charge_limit = 100;
        app.config.battery_charge_limit = 100;

        // 用户把上限拖到 60%：limit 写入成功，但联动开启的 care 位写入失败。
        app.set_charge_limit_internal(60);

        // 限值是两个后端判定养护状态的权威依据：即使 care 位写失败，
        // 状态与持久化配置也必须按限值保持一致，且错误要在 GUI 展示。
        assert_eq!(app.charge_limit, 60);
        assert!(app.battery_care_enabled);
        assert_eq!(app.config.battery_charge_limit, 60);
        assert!(app.config.battery_care_enabled);
        assert!(app
            .error_msg
            .as_deref()
            .unwrap_or_default()
            .contains("同步电池养护状态失败"));

        // 模拟下次启动的 apply_startup_config 路径：
        // care=true → set_charge_limit(60)，用户选择的 60% 不再被覆盖为 100%。
        let cfg = app.config.clone();
        let mut recorded = Vec::new();
        if cfg.battery_care_enabled {
            recorded.push(("set_charge_limit".to_string(), cfg.battery_charge_limit));
        } else {
            recorded.push(("set_charge_limit".to_string(), 100));
        }
        assert_eq!(recorded, vec![("set_charge_limit".to_string(), 60)]);
    }

    /// 模拟 WMI 后端的量化行为：set_charge_limit 只接受预设值，其余就近取整。
    struct QuantizingBackend {
        charge_limit: std::sync::Arc<std::sync::atomic::AtomicU8>,
    }

    impl QuantizingBackend {
        fn new() -> Self {
            Self {
                charge_limit: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(100)),
            }
        }
    }

    impl EcBackend for QuantizingBackend {
        fn name(&self) -> &'static str {
            "quantizing"
        }
        fn read_byte(&self, _addr: u16) -> Result<u8, EcError> {
            Err(EcError::BackendUnavailable("quantizing".into()))
        }
        fn write_byte(&self, _addr: u16, _value: u8) -> Result<(), EcError> {
            Ok(())
        }
        fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
            Ok(self.charge_limit.load(std::sync::atomic::Ordering::Relaxed) < 100)
        }
        fn get_charge_limit(&self) -> Result<u8, EcError> {
            Ok(self.charge_limit.load(std::sync::atomic::Ordering::Relaxed))
        }
        fn set_battery_care(&self, enabled: bool) -> Result<(), EcError> {
            if !enabled {
                self.charge_limit.store(100, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(())
        }
        fn set_charge_limit(&self, percent: u8) -> Result<(), EcError> {
            let quantized = crate::ec::battery::nearest_wmi_percent(percent.min(100));
            self.charge_limit.store(quantized, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        fn get_performance_mode(&self) -> Result<u8, EcError> {
            Ok(PerfMode::Smart as u8)
        }
        fn set_performance_mode(&self, _mode: u8) -> Result<(), EcError> {
            Ok(())
        }
    }

    /// 回归测试：WMI 后端下，养护开启时若 config 中的上限不是预设值（例如
    /// 之前用 WinRing0 保存的 85%），硬件实际写入就近预设 80%。UI 与持久化
    /// 配置必须显示硬件实际生效的 80%，而不是请求的 85%（AC-BAT-04）。
    #[test]
    fn test_wmi_quantization_readback_keeps_ui_in_sync() {
        redirect_config_dir();
        let backend = QuantizingBackend::new();
        let hw_limit = backend.charge_limit.clone();
        let mut app = XiaomiApp::new(
            Box::new(backend),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        // 模拟之前用 WinRing0 保存的非预设上限。
        app.config.battery_charge_limit = 85;
        app.battery_care_enabled = false;

        app.set_battery_care_internal(true);

        // UI 与持久化配置与硬件实际生效值一致（80%），而非请求值（85%）。
        assert_eq!(app.charge_limit, 80);
        assert_eq!(app.config.battery_charge_limit, 80);
        assert!(app.battery_care_enabled);
        assert!(app.config.battery_care_enabled);
        assert_eq!(hw_limit.load(std::sync::atomic::Ordering::Relaxed), 80);
    }

    /// 回归测试：WMI 后端下直接拖动上限到非预设值，同样需要读回硬件实际
    /// 生效值，防止 UI/配置与硬件状态不一致。
    #[test]
    fn test_wmi_quantization_readback_on_charge_limit_set() {
        redirect_config_dir();
        let backend = QuantizingBackend::new();
        let hw_limit = backend.charge_limit.clone();
        let mut app = XiaomiApp::new(
            Box::new(backend),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        app.config.battery_charge_limit = 100;
        app.battery_care_enabled = false;

        app.set_charge_limit_internal(85);

        assert_eq!(app.charge_limit, 80);
        assert_eq!(app.config.battery_charge_limit, 80);
        assert!(app.battery_care_enabled);
        assert!(app.config.battery_care_enabled);
        assert_eq!(hw_limit.load(std::sync::atomic::Ordering::Relaxed), 80);
    }
}
