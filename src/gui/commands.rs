use eframe::egui;

use crate::command::UiCommand;
use crate::ec;
use crate::ec::config::BackendPreference;
use crate::ec::performance::PerfMode;

use super::app::XiaomiApp;

impl XiaomiApp {
    pub fn process_commands(&mut self, ctx: &egui::Context) {
        let mut needs_repaint = false;
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            needs_repaint = true;
            // 命令来源在界面外（托盘/热键/Fn+K/电源广播），GUI 不透明的主要
            // 排查入口：记录每一条命令以便对照"用户操作了什么、程序做了何反应"。
            log::info!("UiCommand: {:?}", cmd);
            match cmd {
                UiCommand::ToggleBatteryCare => {
                    self.set_battery_care_internal(!self.runtime.battery_care_enabled);
                }
                UiCommand::CyclePerfMode => {
                    let current = PerfMode::from_ec_value(self.runtime.performance_mode)
                        .unwrap_or(PerfMode::Smart);
                    // 循环序列定义在领域模块（ec::performance::CYCLE）。
                    let next = ec::performance::next_cycle_mode(current);
                    self.set_perf_mode_internal(next);
                }
                UiCommand::SetPerfMode(mode) => {
                    // 托盘子菜单直接按值设置：未知值（损坏/旧配置）安全忽略。
                    match PerfMode::from_ec_value(mode) {
                        Some(m) => self.set_perf_mode_internal(m),
                        None => log::warn!("SetPerfMode: unknown mode {:#x} ignored", mode),
                    }
                }
                UiCommand::ReapplyConfig => self.reapply_config(),
                UiCommand::SetAutostart(enabled) => self.set_autostart(enabled),
                UiCommand::FnEventSeen { class, hex } => {
                    // 捕获模式（Fn 功能键设置中开启）下收到的实时事件：
                    // 记录最近一条，GUI 展示并用于添加新绑定。事件频率由
                    // 用户按键节奏决定，仅保留最新一条不缓存历史。
                    log::info!("Fn capture event: {} / {}", class, hex);
                    self.last_fn_event = Some((class, hex));
                }
                UiCommand::SetAutostartResult(enabled, result) => match result {
                    Ok(()) => {
                        log::info!("Autostart set to {}", enabled);
                        // 配置已在 set_autostart 请求时即时持久化（见 M3 修复
                        // 注释）；此处仅确认：若中途另有请求覆盖（快速连点），
                        // 以最后一次请求为准，不重复写回。
                    }
                    Err(e) => {
                        log::error!("Autostart operation failed: {}", e);
                        // F-AUTO-10：注册失败时恢复复选框为未勾选状态——本次
                        // 期望值与实际结果不符（enable 失败），必须回滚请求时
                        // 写入的期望值，否则复选框与任务实际状态长期背离。
                        // **只回滚"仍是最新意图"的失败**：串行 worker 中较早
                        // 请求的结果可能晚于更新的请求到达（快速连点 ON→OFF
                        // 时，先发的 enable#1 失败结果在 disable#2 已落盘之后
                        // 到达）。此时配置反映的是更新的用户意图，回滚会把它
                        // 覆盖回旧值，重新制造"任务在而配置关"的背离（M3 回归）。
                        if enabled && self.config.auto_start_on_boot {
                            self.config.auto_start_on_boot = false;
                            self.save_state();
                        } else if !enabled && !self.config.auto_start_on_boot {
                            // disable 失败（任务仍存在）：配置若仍为关，回滚为
                            // 勾选（true）更贴近实际；若已被更新的 enable 覆盖
                            // 为 true 则保持（与最新意图一致）。
                            self.config.auto_start_on_boot = true;
                            self.save_state();
                        }
                        self.push_error(format!("设置开机自启动失败: {}", e));
                    }
                },
                UiCommand::WmiAvailable(backend) => {
                    // 延迟恢复探测结果（见 app.rs maybe_probe_wmi_recovery）：
                    // 探测是后台异步的，期间用户可能手动切换了后端，必须校验
                    // "当前仍期望 WMI 且尚未恢复"才应用，否则丢弃过期结果——
                    // 误应用会把用户刚选的 WinRing0 覆盖回 WMI。
                    let wants_wmi = matches!(
                        self.config.backend,
                        BackendPreference::Auto | BackendPreference::Wmi
                    );
                    if !wants_wmi {
                        log::info!(
                            "WMI delayed recovery: user preference no longer WMI; probed backend dropped"
                        );
                    } else if self.backend.preference() == BackendPreference::Wmi {
                        log::info!(
                            "WMI delayed recovery: WMI already active; probed backend dropped"
                        );
                    } else if backend.preference() != BackendPreference::Wmi {
                        log::warn!(
                            "WMI delayed recovery: probed backend is '{}' not WMI; dropped",
                            backend.name()
                        );
                    } else {
                        self.wmi_recover_at = None;
                        log::info!(
                            "WMI delayed recovery: WMI available; switching from '{}'",
                            self.backend.name()
                        );
                        // pref 传用户偏好（Auto/Wmi）：apply_backend_switch
                        // 会据此更新 config.backend 与 current_pref。
                        self.apply_backend_switch(backend, self.config.backend);
                    }
                }
                UiCommand::Quit => {
                    // 请求 eframe 正常退出事件循环：置位 quitting 后下一帧
                    // 的 close_requested 放行（不再取消/隐藏到托盘），
                    // run_native 返回，各组件 Drop 正常执行（WinRing0 后端
                    // DeinitializeOls 卸载驱动等）。不能用 process::exit
                    // 跳过清理（修订 1.21）。
                    log::info!("Quit: setting quitting flag");
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        if needs_repaint {
            ctx.request_repaint();
        }
    }

    /// 电源切换时重设配置（UiCommand::ReapplyConfig）。
    ///
    /// 与启动 apply 路径一致（见 ec::battery::apply_config_to_hardware）：
    /// 统一处理"写限值 → 写养护 → 写性能模式（含狂暴的交流电源降级）"。
    /// 兜底只作用在辅助函数内部，**不**提前改写 config——写入失败时内存中的
    /// config 不会被污染。
    pub(crate) fn reapply_config(&mut self) {
        if !self.config.auto_reapply_on_power_change {
            log::debug!("ReapplyConfig ignored: auto_reapply_on_power_change is off");
            return;
        }
        self.apply_config_and_sync();
    }

    /// 把配置整份应用到硬件并同步 GUI/持久化（**不检查**是否允许重设）。
    ///
    /// 与 `reapply_config` 的唯一区别：不提前检查 `auto_reapply_on_power_change`。
    /// 该开关只约束"电源切换/唤醒自动重设"这类**被动**场景；用户主动切换
    /// （如勾选"电池供电时自动切节能"）必须无条件应用——否则开关开着却
    /// 因为重设开关关闭而静默不生效，用户会以为功能坏了。
    pub(crate) fn apply_config_and_sync(&mut self) {
        log::info!("Reapplying config on hardware");
        let outcome = ec::battery::apply_config_to_hardware(&*self.backend, &self.config);
        if let Ok(applied) = &outcome.battery.charge_limit {
            // 与 set_charge_limit_internal / 启动同步（sync_startup_config）
            // 的读回约定一致：WMI 后端会把非预设值量化到最近的预设（如
            // 85→80），成功写入后必须读回硬件实际生效值再持久化，否则
            // config 与硬件长期背离——每次电源切换都重复量化写入，UI
            // 滑块显示硬件值（80）而配置仍是 85。
            // 仅当应用值 <100%（养护开启）时同步；读回 100% 意味着写入
            // 被硬件拒绝（养护未生效），保留用户期望值，避免 care=true
            // + limit=100 的矛盾配置。统一收敛在
            // battery::sync_config_after_apply（养护关闭时保留期望上限）。
            crate::ec::battery::sync_config_after_apply(&mut self.config, *applied);
        }
        // 写入失败字段的遍历统一收敛在 ApplyOutcome::field_errors。
        let mut errs: Vec<String> = Vec::new();
        for (field, e) in outcome.field_errors() {
            log::error!("Reapply {} failed: {}", field, e);
            errs.push(format!("重设{}失败: {}", field, e));
        }
        // 规范化（如 care=true + limit=100 兜底为 80）修改了配置，
        // 需要持久化，否则配置文件中残留的矛盾组合每次都会被重写。
        self.save_state();
        self.refresh_from_backend();
        // refresh_from_backend 成功时会清空 error_msg，写入失败
        // 必须在其后合并展示（F-ERR-03），否则错误被静默吞掉。
        for err in errs {
            self.push_error(err);
        }
    }

    /// 开机自启动操作的**串行** worker（UiCommand::SetAutostart）。
    ///
    /// 注册/删除走单一串行 worker 线程：ITaskService 需要在该线程初始化 COM，
    /// GUI 线程的 COM 状态由 eframe/winit 管理，不得污染（见 21e0aaf 的公寓
    /// 冲突教训）；同时杜绝并发操作的乱序覆盖（UiCommand::SetAutostart 的
    /// 注册/删除是异步的，快速连续切换时若并发执行，完成顺序不确定，结果以
    /// "最后完成的那个"覆盖 config，可能与用户最后的操作相反）。
    ///
    /// worker 惰性创建并挂在本实例（`self.autostart_worker`）上，而非进程级
    /// static：历史实现用 OnceLock 缓存发送端，把结果通道绑定到"首次调用
    /// 的实例"——若该实例被销毁（如测试重建），结果会投进死通道而静默丢失。
    /// 随实例存活保证结果永远回到本实例；实例 drop 时 Sender 关闭，worker
    /// 线程随即退出。
    /// **请求即时持久化**（M3 修复）：把用户勾选/取消的**期望值**同步写入
    /// 配置，而不是等 worker 结果回来才落盘。历史实现只在
    /// `SetAutostartResult` 到达时写配置——若用户在任务注册完成后、结果
    /// 回传前退出应用（Quit/进程被杀），配置保持旧值（如关）而计划任务
    /// 已是新状态（已注册），下次启动 `autostart::sync` 见"配置关 + 任务
    /// 在"按设计不删除，任务永久残留、App 照常自启动，与配置矛盾。
    /// 即时持久化后：启停中途退出时配置 = 用户最终意图，`sync` 总能据此
    /// 把任务收敛到一致状态（配置开 + 任务缺 → 重建；配置关 + 任务在 → 保守
    /// 不动）。副作用：复选框即时反映新值（修复 1.25 的可见闪烁——历史
    /// 实现勾选后 ~1s 才由结果回写）。
    ///
    /// 单独抽出供测试直接验证"请求即落盘"（不触启真实 worker/计划任务）。
    fn persist_autostart_request(&mut self, enabled: bool) {
        self.config.auto_start_on_boot = enabled;
        self.save_state();
    }

    fn set_autostart(&mut self, enabled: bool) {
        self.persist_autostart_request(enabled);
        let cmd_tx = self.cmd_tx.clone();
        let worker_tx = match &self.autostart_worker {
            Some(tx) => tx.clone(),
            None => {
                let (tx, rx) = std::sync::mpsc::channel::<bool>();
                std::thread::spawn(move || {
                    // 自启动 worker 生命周期日志：该线程按请求串行执行
                    // 计划任务注册/删除，正常情况下在应用退出时随通道关闭
                    // 结束。记录 start/exit 便于确认 worker 是否存活。
                    log::info!("Autostart worker thread started");
                    for enabled in rx {
                        let result = if enabled {
                            crate::platform::autostart::enable()
                        } else {
                            crate::platform::autostart::disable()
                        };
                        let _ = cmd_tx.send(UiCommand::SetAutostartResult(enabled, result));
                    }
                    log::info!("Autostart worker thread exited");
                });
                self.autostart_worker = Some(tx.clone());
                tx
            }
        };
        // 请求投递失败：worker 线程已退出（enable/disable 内部 panic 等罕见情形）
        // 时发送必然失败——用户点击"开机自启动"开关后毫无反应且无日志，属于
        // 典型静默失效，必须记录而非 `let _ =` 吞掉。
        if let Err(e) = worker_tx.send(enabled) {
            log::warn!(
                "Autostart worker channel closed; request to {} dropped: {}",
                if enabled { "enable" } else { "disable" },
                e
            );
        }
    }

    // ── Fn 功能键绑定 ──────────────────────────────────────────────────
    // 配置 `config.fn_key_bindings` 是持久化事实来源；`self.fn_bindings`
    // （`Arc<RwLock<Vec<_>>>`）是与 Fn 监听线程共享的运行时镜像。每次
    // 修改配置后必须经 `commit_fn_bindings` 同时更新共享镜像并落盘，
    // 否则 GUI 里改的绑定在监听线程不生效、或重启后丢失。

    /// 把当前配置中的绑定表同步进共享镜像并持久化（唯一提交点）。
    fn commit_fn_bindings(&mut self) {
        if let Ok(mut guard) = self.fn_bindings.write() {
            *guard = self.config.fn_key_bindings.clone();
        } else {
            log::warn!("Fn bindings lock poisoned; snapshot may be stale");
        }
        self.save_state();
    }

    /// 修改某条绑定的动作（index 越界时告警忽略）。
    pub(crate) fn set_fn_binding_action(&mut self, index: usize, action: ec::fnkey::FnAction) {
        let Some(binding) = self.config.fn_key_bindings.get_mut(index) else {
            log::warn!("set_fn_binding_action: index {} out of range", index);
            return;
        };
        if binding.action == action {
            return;
        }
        log::info!("Fn binding {} action -> {}", binding.label(), action.name());
        binding.action = action;
        self.commit_fn_bindings();
    }

    /// 按已知功能键目录添加绑定（GUI"添加绑定"下拉）。
    /// 相同 (class, prefix) 已存在时只更新动作，不重复添加。
    pub(crate) fn add_fn_binding(
        &mut self,
        class: &str,
        prefix: &str,
        action: ec::fnkey::FnAction,
    ) {
        if class.trim().is_empty() || prefix.trim().is_empty() {
            log::warn!("add_fn_binding: empty class/prefix ignored");
            return;
        }
        let prefix = ec::fnkey::normalize_hex(prefix);
        if prefix.is_empty() {
            log::warn!("add_fn_binding: non-hex prefix ignored: {:?}", prefix);
            return;
        }
        let existing = self
            .config
            .fn_key_bindings
            .iter_mut()
            .find(|b| b.class == class && b.prefix == prefix);
        if let Some(b) = existing {
            log::info!(
                "Fn:: binding {}/{} already exists; setting action {}",
                class,
                ec::fnkey::FnKeyBinding::display_prefix(&prefix),
                action.name()
            );
            b.action = action;
        } else {
            log::info!(
                "Fn:: add binding {} / {} -> {}",
                class,
                ec::fnkey::FnKeyBinding::display_prefix(&prefix),
                action.name()
            );
            self.config.fn_key_bindings.push(ec::fnkey::FnKeyBinding {
                class: class.to_string(),
                prefix,
                action,
            });
        }
        self.commit_fn_bindings();
    }

    /// 删除某条绑定（列表恒保留至少一条？不强制：允许清空后监听线程
    /// 空转，见 fnkey 的"无绑定空转"逻辑）。
    pub(crate) fn remove_fn_binding(&mut self, index: usize) {
        let Some(binding) = self.config.fn_key_bindings.get(index) else {
            log::warn!("remove_fn_binding: index {} out of range", index);
            return;
        };
        log::info!("Fn:: remove binding {}", binding.label());
        self.config.fn_key_bindings.remove(index);
        self.commit_fn_bindings();
    }

    /// 切换 Fn 捕获模式（与监听线程共享的开关）：开启后收到的功能键
    /// 事件实时回传 GUI（UiCommand::FnEventSeen）便于配置新绑定。
    pub(crate) fn toggle_fn_capture(&mut self) {
        let next = !self.fn_capture.load(std::sync::atomic::Ordering::Relaxed);
        self.fn_capture
            .store(next, std::sync::atomic::Ordering::Relaxed);
        log::info!(
            "Fn capture mode {}",
            if next { "enabled" } else { "disabled" }
        );
        if !next {
            // 关闭捕获时清空最近事件：界面上"上一次捕获"不应残留误导。
            self.last_fn_event = None;
        }
    }

    /// 电池写入成功后的统一"提交"：运行时值 + 持久化配置 + 养护位。
    /// 限值是两种后端判定养护状态的权威依据（WMI 养护位由限值 <100% 推导，
    /// WinRing0 读回亦按限值），因此 `applied < 100` 即养护开启。这一组赋值
    /// 在 `set_battery_care_internal` 与 `set_charge_limit_internal` 各自重复
    /// 实现过，存在漂移风险——统一收敛到此处后，任何一处修改规则都会同时
    /// 作用于全部写入路径。
    fn commit_battery_write_state(&mut self, applied: u8) {
        log::info!(
            "Battery state committed: care={}, limit={}%",
            ec::battery::care_enabled_from_limit(applied),
            applied
        );
        self.runtime.charge_limit = applied;
        // 养护关闭（applied == 100）时保留 config 中用户期望的上限供重新开启
        // 时恢复，开启时回写硬件实际生效值——统一收敛在
        // battery::sync_config_after_apply。
        crate::ec::battery::sync_config_after_apply(&mut self.config, applied);
        self.runtime.battery_care_enabled = ec::battery::care_enabled_from_limit(applied);
        self.sync_tray_status();
    }

    pub fn set_battery_care_internal(&mut self, enabled: bool) {
        // When disabling, only the hardware limit is raised to 100%; the
        // persisted desired limit must be kept so it is not lost when battery
        // care is re-enabled later.
        // 注意：兜底 80% 只能作用在 apply_battery_state 内部，不能提前改写
        // config——若随后硬件写入失败（charge_limit 为 Err），else 分支不会
        // 保存配置，内存中的 config 若已被改成 80 就与未更新的 care=false
        // 构成矛盾状态，后续任何一次 save_state 都会把"用户期望 100%、改写
        // 失败"静默持久化为 80%。成功路径会在下方用读回值统一落盘。
        let desired_limit = self.config.battery_charge_limit;
        let outcome = ec::battery::apply_battery_state(&*self.backend, enabled, desired_limit);
        match outcome.charge_limit {
            // 限值是两种后端判定养护状态的权威依据（WMI 的养护位由限值<100%
            // 推导，WinRing0 读回亦按限值）：set_charge_limit 成功即硬件养护
            // 状态已按限值生效。即使 set_battery_care 失败，状态与持久化配置
            // 也必须按限值保持自洽——否则下次启动会按旧配置（care=false）
            // 强制写 100%，静默覆盖用户刚设置的限值。
            Ok(applied) => {
                self.commit_battery_write_state(applied);
                if let Err(e) = &outcome.care {
                    log::error!("Battery care bit write failed after limit applied: {}", e);
                    // 与 set_charge_limit_internal 的联动失败处理一致：限值才是
                    // 权威依据，写入失败仅告警（F-ERR-03），状态保持自洽。
                    self.push_error("设置电池养护失败".to_string());
                }
                self.save_state();
            }
            // 限值写入失败时不得更新状态：硬件未变更，UI 与配置保持一致，
            // 错误在 GUI 中展示（F-ERR-03）。
            Err(e) => {
                log::error!("Set battery care: charge limit write failed: {}", e);
                let mut errs = Vec::new();
                errs.push("设置充电上限失败".to_string());
                if outcome.care.is_err() {
                    errs.push("设置电池养护失败".to_string());
                }
                self.push_error(errs.join("; "));
            }
        }
    }

    pub fn set_charge_limit_internal(&mut self, limit: u8) {
        let limit = limit.min(100);
        // 养护位由限值推导：<100% 即养护开启，100% 即关闭。统一的
        // apply_battery_state 会写限值 → 写养护位 → 读回实际生效值
        // （WMI 量化到最近预设，如 85→80，见 AC-BAT-04）。
        let outcome = ec::battery::apply_battery_state(
            &*self.backend,
            ec::battery::care_enabled_from_limit(limit),
            limit,
        );
        let applied = match outcome.charge_limit {
            Ok(applied) => applied,
            Err(e) => {
                log::error!("Failed to set charge limit: {}", e);
                self.push_error(format!("设置充电上限失败: {}", e));
                return;
            }
        };
        // 提交前先判定养护位是否发生翻转：commit_battery_write_state 会把
        // runtime.battery_care_enabled 同步为 applied<100，下方的联动日志需要
        // 依据翻转前的状态差异。
        let care_changed =
            ec::battery::care_enabled_from_limit(applied) != self.runtime.battery_care_enabled;
        self.commit_battery_write_state(applied);
        if care_changed {
            // On both backends the charge limit is the authoritative battery-care
            // control: WMI has no separate care bit, and WinRing0 derives care
            // from the limit.  Keep the UI flag consistent with the limit.
            if let Err(e) = &outcome.care {
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
                self.push_error(format!("同步电池养护状态失败: {}", e));
            } else {
                log::info!(
                    "Battery care {} (synced from charge limit)",
                    if ec::battery::care_enabled_from_limit(applied) {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
            }
        }
        self.save_state();
    }

    pub fn set_perf_mode_internal(&mut self, mode: PerfMode) {
        // 写入硬件时按电源状态选择实际 raw code（狂暴模式需要交流电源）。
        // 降级规则统一收敛在 ec::battery::effective_perf_for_current_power
        // （电源状态未知时不静默降级，按用户选择写入并告警）。
        let raw = ec::battery::effective_perf_for_current_power(mode.ec_value());
        if raw != mode.ec_value() {
            log::warn!("Extreme mode requires AC power; using Fast mode instead");
        }
        let mode_name = mode.name();
        match self.backend.set_performance_mode(raw) {
            Ok(_) => {
                log::info!("Performance mode set to {} ({:#x})", mode_name, raw);
                // config 记录**用户选择**（插电/重启后恢复狂暴）；runtime 是
                // "硬件/界面当前认知"，必须记录**实际写入**的 raw code——
                // 电池供电下选狂暴硬件跑极速时，若 runtime 也存狂暴，托盘
                // 勾选/GUI 高亮/状态栏会谎报"狂暴"，与实际硬件不符，直到
                // 下一次读取才翻到极速（实测：点狂暴后状态栏显示狂暴、硬件
                // 是极速，点刷新状态栏跳变极速）。统一与 read 路径一致。
                self.runtime.performance_mode = raw;
                self.config.performance_mode = mode.ec_value();
                self.sync_tray_status();
                self.save_state();
            }
            Err(e) => {
                log::error!("Failed to set performance mode: {}", e);
                self.push_error(format!("设置性能模式失败: {}", e));
            }
        }
    }

    pub fn try_switch_backend(&mut self, pref: BackendPreference) -> bool {
        // 目标偏好与当前实际后端一致时直接确认，不重建：create_backend(WinRing0)
        // 会先 cleanup_service 停/删驱动服务，而当前活动后端正依赖它——重建失败
        // 会让工作正常的后端立即失效（见 winring0.rs）。
        // **例外**：当前后端需要重建时（WMI 超时熔断，needs_rebuild=true），
        // 即便偏好未变也必须重建——熔断后唯一恢复途径是全新 worker
        // （create_backend），否则 WMI-only 机器上后端永久卡死在熔断态（F2）。
        if pref != BackendPreference::Auto
            && self.backend.preference() == pref
            && !self.backend.needs_rebuild()
        {
            log::info!(
                "Backend '{}' already active; no re-init needed",
                self.backend.name()
            );
            return self.confirm_preference(pref);
        }
        // Auto：优先 WMI，失败才回退 WinRing0。这里**不能**直接 create_backend(Auto)
        // 重建——当 WMI 不可用且当前已是 WinRing0 时，create_backend(Auto) 会创建
        // 一个新的 WinRing0 后端，随后 apply_backend_switch 丢弃旧实例、触发
        // DeinitializeOls 卸载驱动，而新后端依赖的正是这个驱动——卸载后其端口读写
        // 全部失效，工作正常的后端被自己搞坏（只能重启恢复）。Auto 的语义是
        // "探测并选最优"，因此：
        //   - 当前已是 WMI：Auto 的探测结果必然还是 WMI，直接确认，不重建；
        //   - 当前是 WinRing0：先探测 WMI，可用则切过去；不可用则**保留**现有
        //     WinRing0 后端（不再重新创建），避免卸载活驱动；
        //   - 其余（无后端等）：走下方 create_backend(Auto) 的完整探测。
        if pref == BackendPreference::Auto {
            match self.backend.preference() {
                BackendPreference::Wmi => {
                    // 当前已是 WMI：正常时确认不重建；熔断时（needs_rebuild）
                    // 必须重建全新 worker 才能恢复（F2），走下方 create_backend。
                    if !self.backend.needs_rebuild() {
                        log::info!(
                            "Backend already matches Auto preference (WMI); no re-init needed"
                        );
                        return self.confirm_preference(pref);
                    }
                    log::warn!("Auto: active WMI backend is wedged; rebuilding");
                }
                BackendPreference::WinRing0 => {
                    match ec::backend::create_backend(BackendPreference::Wmi) {
                        Ok(wmi) => {
                            log::info!("Auto: WMI available; switching from WinRing0 to WMI");
                            return self.apply_backend_switch(wmi, pref);
                        }
                        Err(e) => {
                            log::info!(
                                "Auto: WMI unavailable ({}); keeping active WinRing0 backend",
                                e
                            );
                            return self.confirm_preference(pref);
                        }
                    }
                }
                _ => {}
            }
        }
        match ec::backend::create_backend(pref) {
            Ok(new_backend) => self.apply_backend_switch(new_backend, pref),
            Err(e) => {
                log::error!("Failed to switch EC backend: {}", e);
                self.push_error(format!("后端切换失败: {}", e));
                false
            }
        }
    }

    /// 确认不重建后端的偏好切换：更新显示偏好与持久化配置并保存。
    /// 仅用于"目标后端无需重建"（同种后端 no-op / Auto 语义下的保留）路径。
    fn confirm_preference(&mut self, pref: BackendPreference) -> bool {
        self.current_pref = pref;
        self.config.backend = pref;
        self.save_state();
        true
    }

    /// 完成后端切换的公共逻辑（create_backend 之外的部分），单独抽出便于测试。
    /// 注意：不得在 refresh_from_backend() 之后清空 error_msg —— 刷新产生的
    /// 读取失败必须保留并在 GUI 中展示（F-ERR-03）；刷新成功时它自会清空。
    fn apply_backend_switch(
        &mut self,
        new_backend: Box<dyn ec::backend::EcBackend>,
        pref: BackendPreference,
    ) -> bool {
        log::info!("Switched EC backend to: {}", new_backend.name());
        self.backend = new_backend;
        self.current_pref = pref;
        self.config.backend = self.current_pref;
        self.save_state();
        self.refresh_from_backend();
        true
    }

    pub fn refresh_from_backend(&mut self) {
        let mut errors: Vec<String> = Vec::new();
        match self.backend.get_performance_mode() {
            Ok(mode) => {
                self.runtime.performance_mode = mode;
            }
            Err(e) => {
                log::error!("Backend refresh: {}", e);
                errors.push(format!("读取性能模式: {}", e));
            }
        }
        match self.backend.get_battery_state() {
            Ok((_care, limit)) => {
                // 钳制到 [0,100]：损坏的 EC 读值（如 0xFF=255）不得显示为
                // "充电上限: 255%" 或使滑块/养护位推导溢出。上限超过 100
                // 视为垃圾值钳到 100。
                let limit = limit.min(100);
                self.runtime.charge_limit = limit;
                // 领域不变式：养护 == 上限 < 100%（care_enabled_from_limit）。
                // 读回的 care 位与 limit 冲突时（垃圾值场景下存在），以
                // limit 为权威重新推导——否则"养护: 开启 · 上限: 100%"的
                // 矛盾组合会展示给用户（M5 回归修复：历史实现把 care 原样
                // 存进 runtime，钳制后的 limit=100 与 care=true 并存）。
                self.runtime.battery_care_enabled = ec::battery::care_enabled_from_limit(limit);
            }
            Err(e) => {
                log::error!("Backend refresh: {}", e);
                errors.push(format!("读取电池状态: {}", e));
            }
        }
        // Only the runtime state is refreshed here; the persisted config keeps
        // the user's desired settings so they are not silently overwritten by
        // whatever the hardware currently reports.
        self.sync_tray_status();
        if errors.is_empty() {
            log::debug!(
                "Backend refreshed: perf={:#x}, care={}, limit={}%",
                self.runtime.performance_mode,
                self.runtime.battery_care_enabled,
                self.runtime.charge_limit
            );
            self.error_msg = None;
        } else {
            self.error_msg = Some(errors.join("; "));
        }
    }

    pub(crate) fn save_state(&self) {
        if let Err(e) = self.store.save(&self.config) {
            log::error!("save config: {}", e);
        }
    }

    /// 把当前运行时状态写入托盘共享状态（tooltip/菜单展示）。
    ///
    /// 所有改变运行时状态的路径（刷新、切换养护/上限/性能模式、切换后端）
    /// 都应调用，使托盘悬停提示保持实时；未同步时托盘显示的仍是旧状态。
    fn sync_tray_status(&self) {
        if let Ok(mut guard) = self.tray_status.lock() {
            guard.battery_care_enabled = self.runtime.battery_care_enabled;
            guard.charge_limit = self.runtime.charge_limit;
            guard.performance_mode = self.runtime.performance_mode;
        } else {
            log::warn!("tray status lock poisoned; tooltip may be stale");
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

    /// 打开日志文件（设置区域"打开日志"按钮）。
    ///
    /// 用 `explorer.exe /select,<path>` 在资源管理器中定位并选中日志文件——
    /// 直接调用默认文本编辑器对 `.txt` 关联不可靠（可能落在记事本以外的
    /// 程序），资源管理器定位稳定且用户能同时看到轮转副本（app.log.1）。
    /// GUI 主线程不阻塞：explorer 是独立进程。
    ///
    /// `/select,` 必须与路径分开作为独立参数传入：若拼接成一个含内嵌引号的
    /// 参数（`/select,"C:\...\app.log"`），`std::process::Command` 会整体加
    /// 引号并把内嵌引号转义成 `\"`，explorer.exe 无法解析该参数，退化为打开
    /// 默认位置（桌面），日志并未被定位。
    pub(crate) fn open_log_file(&mut self) {
        let path = crate::util::log_file_path();
        let path_str = path.to_string_lossy();
        match std::process::Command::new("explorer.exe")
            .arg("/select,")
            .arg(path_str.as_ref())
            .spawn()
        {
            Ok(_) => log::info!("Opening log file in Explorer: {}", path.display()),
            Err(e) => {
                log::error!("Failed to open log file in Explorer: {}", e);
                self.push_error(format!("打开日志失败: {}", e));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ec::config::AppConfig;
    use crate::ec::mock::MockBackend;

    /// 每个用例独立的临时配置目录：save_state() 永不触碰用户的真实配置。
    fn test_store() -> crate::ec::config::ConfigStore {
        crate::testutil::temp_store("test")
    }

    /// 回归测试：当前后端已是 WMI（Auto 探测的必然结果）时，切换到 Auto 必须
    /// 是 no-op——历史实现会 create_backend(Auto) 重建一个 WMI 代理（每次请求
    /// 都多一次完整连接握手），此处校验不触发任何重建。
    #[test]
    fn test_try_switch_backend_auto_when_already_wmi_is_noop() {
        let store = test_store();
        let mut app = XiaomiApp::new(
            store,
            Box::new(MockBackend::all_fail("pref-wmi", BackendPreference::Wmi)),
            AppConfig::default(),
            BackendPreference::Wmi,
            None,
            false,
        );
        // 构造时的启动刷新会产生读取错误，先清空以便聚焦本用例。
        app.error_msg = None;
        let backend_before = app.backend.name();

        let ok = app.try_switch_backend(BackendPreference::Auto);
        assert!(ok, "Auto switch on an already-WMI backend must succeed");
        assert_eq!(
            app.backend.name(),
            backend_before,
            "backend must not be recreated"
        );
        assert_eq!(app.current_pref, BackendPreference::Auto);
        assert_eq!(app.config.backend, BackendPreference::Auto);
        assert!(
            app.error_msg.is_none(),
            "no-op switch must not produce errors"
        );
    }

    /// 回归测试：当前后端是 WinRing0 时切换到 Auto。WMI 不可用时必须**保留**
    /// 现有 WinRing0 后端而不是重建——重建会创建新实例后再 drop 旧实例，
    /// 触发 DeinitializeOls 卸载驱动，使新 WinRing0 后端的端口读写全部失效
    /// （只能重启恢复）。WMI 可用时按 Auto 语义切到 WMI。两条路径都不得
    /// 留下损坏的后端或产生错误。
    #[test]
    fn test_try_switch_backend_auto_from_winring0_keeps_or_switches() {
        let store = test_store();
        let mut app = XiaomiApp::new(
            store,
            Box::new(MockBackend::all_fail(
                "pref-winring0",
                BackendPreference::WinRing0,
            )),
            AppConfig::default(),
            BackendPreference::WinRing0,
            None,
            false,
        );
        app.error_msg = None;
        let backend_before = app.backend.name();

        let ok = app.try_switch_backend(BackendPreference::Auto);
        assert!(ok, "Auto switch must never leave a broken backend");
        if app.backend.name() == backend_before {
            // WMI 不可用：现有 WinRing0 后端必须被原样保留（未被重建）。
            assert_eq!(app.backend.preference(), BackendPreference::WinRing0);
        } else {
            // WMI 可用：Auto 优先 WMI，应切换到 WMI 后端。
            assert_eq!(app.backend.preference(), BackendPreference::Wmi);
        }
        assert_eq!(app.current_pref, BackendPreference::Auto);
        assert_eq!(app.config.backend, BackendPreference::Auto);
        assert!(app.error_msg.is_none(), "switch must not leave errors");
    }

    /// 回归测试：请求切换到"当前已经激活的同种后端"必须是 no-op。
    /// 历史实现会重建后端：WinRing0 的重建路径先 cleanup_service 停/删当前
    /// 驱动服务，若后续 InitializeOls 失败，正在工作的后端立即失效。
    /// no-op 分支不创建新后端（不触碰真实硬件），因此这里的 WMI 偏好切换
    /// 必须返回 true 且后端实例保持不变。
    #[test]
    fn test_try_switch_backend_same_kind_is_noop() {
        let store = test_store();
        let mut app = XiaomiApp::new(
            store,
            Box::new(MockBackend::all_fail("pref-wmi", BackendPreference::Wmi)),
            AppConfig::default(),
            BackendPreference::Wmi,
            None,
            false,
        );
        // 构造时的启动刷新会产生读取错误，先清空以便聚焦本用例。
        app.error_msg = None;
        let backend_before = app.backend.name();

        let ok = app.try_switch_backend(BackendPreference::Wmi);
        assert!(ok, "same-kind switch must be a no-op that succeeds");
        assert_eq!(
            app.backend.name(),
            backend_before,
            "backend must not be recreated"
        );
        assert_eq!(app.current_pref, BackendPreference::Wmi);
        assert_eq!(app.config.backend, BackendPreference::Wmi);
        assert!(
            app.error_msg.is_none(),
            "no-op switch must not produce errors"
        );
    }

    fn failing_app() -> XiaomiApp {
        XiaomiApp::new(
            test_store(),
            Box::new(MockBackend::all_fail("failing", BackendPreference::Auto)),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        )
    }

    fn test_app() -> XiaomiApp {
        XiaomiApp::new(
            test_store(),
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
        // 电池供电下选择狂暴：硬件按电源状态写入降级值（极速），但
        // config 保留用户选择的狂暴（插电/重启后恢复）；runtime 是"硬件
        // 当前认知"，必须与实际写入一致（修订 1.25 回归测试：历史实现把
        // runtime 存成用户选择，GUI/托盘谎报狂暴而硬件实为极速）。
        app.set_perf_mode_internal(PerfMode::Extreme);
        let applied = ec::battery::effective_perf_for_current_power(PerfMode::Extreme.ec_value());
        assert_eq!(app.runtime.performance_mode, applied);
        assert_eq!(app.config.performance_mode, PerfMode::Extreme as u8);
    }

    /// 托盘子菜单直接指定模式：SetPerfMode 命令按值切换；未知值安全忽略。
    #[test]
    fn test_set_perf_mode_command_direct_and_invalid() {
        let mut app = test_app();
        let ctx = egui::Context::default();

        app.cmd_tx
            .send(UiCommand::SetPerfMode(PerfMode::Quiet as u8))
            .unwrap();
        app.process_commands(&ctx);
        assert_eq!(app.runtime.performance_mode, PerfMode::Quiet as u8);
        assert_eq!(app.config.performance_mode, PerfMode::Quiet as u8);

        // 未知模式（如损坏的 0xFF）：忽略，不改变当前状态。
        app.cmd_tx.send(UiCommand::SetPerfMode(0xFF)).unwrap();
        app.process_commands(&ctx);
        assert_eq!(app.runtime.performance_mode, PerfMode::Quiet as u8);
        assert!(app.error_msg.is_none(), "invalid mode must not error");
    }

    #[test]
    fn test_perf_mode_normal_selection_not_changed() {
        let mut app = test_app();
        app.set_perf_mode_internal(PerfMode::Quiet);
        assert_eq!(app.runtime.performance_mode, PerfMode::Quiet as u8);
        assert_eq!(app.config.performance_mode, PerfMode::Quiet as u8);
    }

    /// 回归测试（M3）：开机自启动请求必须**即时持久化**期望值，而不是等
    /// worker 结果回传才写配置——否则任务注册完成、应用在结果到达前退出时，
    /// 配置保持旧值而计划任务已是新状态，下次启动 sync 不删任务，任务永久
    /// 残留（与配置矛盾）。请求即落盘使中途退出时配置 = 用户最终意图。
    #[test]
    fn test_set_autostart_persists_requested_state_immediately() {
        let mut app = test_app();
        // 与 UiCommand::SetAutostart 等价的处理路径（persist_autostart_request，
        // 不触发真实 worker）：请求后 config 必须立即反映新值并落盘。
        app.persist_autostart_request(true);
        assert!(
            app.config.auto_start_on_boot,
            "config must reflect the request immediately"
        );
        // 结果成功：不再重复写回，但状态保持。
        let ctx = egui::Context::default();
        app.cmd_tx
            .send(UiCommand::SetAutostartResult(true, Ok(())))
            .unwrap();
        app.process_commands(&ctx);
        assert!(app.config.auto_start_on_boot);
        assert!(app.error_msg.is_none(), "success must not error");

        // 关闭并请求回滚：enable 失败时复选框必须回滚为未勾选（F-AUTO-10）。
        app.persist_autostart_request(false);
        assert!(!app.config.auto_start_on_boot);
        app.cmd_tx
            .send(UiCommand::SetAutostartResult(false, Ok(())))
            .unwrap();
        app.process_commands(&ctx);
        assert!(!app.config.auto_start_on_boot);
        app.persist_autostart_request(true);
        app.cmd_tx
            .send(UiCommand::SetAutostartResult(true, Err("测试失败".into())))
            .unwrap();
        app.process_commands(&ctx);
        assert!(
            !app.config.auto_start_on_boot,
            "enable failure must revert checkbox to unchecked"
        );
        let err = app.error_msg.as_deref().unwrap_or_default();
        assert!(err.contains("设置开机自启动失败"), "unexpected: {}", err);
    }

    /// 回归测试（F2/1.1）：**过期的**失败结果不得覆盖更新的用户意图。
    /// 串行 worker 中先发的 enable#1 失败结果可能晚于 disable#2 已落盘之后
    /// 到达——此时配置反映的是更新的意图（关），回滚会把它覆盖回旧值，
    /// 重新制造"任务在而配置关"的背离。
    #[test]
    fn test_autostart_stale_failure_does_not_revert_latest_intent() {
        let mut app = test_app();
        let ctx = egui::Context::default();

        // 快速连点 ON→OFF：先请求 enable（落盘 true），随即请求 disable
        // （落盘 false）。enable 失败结果**迟到**。
        app.persist_autostart_request(true);
        app.persist_autostart_request(false);
        assert!(!app.config.auto_start_on_boot, "latest intent is off");

        // enable#1 失败结果迟到：不得回滚（config 已是最新意图 false）。
        app.cmd_tx
            .send(UiCommand::SetAutostartResult(
                true,
                Err("迟到的失败".into()),
            ))
            .unwrap();
        app.process_commands(&ctx);
        assert!(
            !app.config.auto_start_on_boot,
            "stale failure must not revert the newer OFF intent"
        );

        // 反向：ON 已是最新意图时，迟到的 disable 失败不得把配置回滚成关。
        app.persist_autostart_request(true);
        app.cmd_tx
            .send(UiCommand::SetAutostartResult(
                false,
                Err("迟到的失败".into()),
            ))
            .unwrap();
        app.process_commands(&ctx);
        assert!(
            app.config.auto_start_on_boot,
            "stale disable failure must not revert the newer ON intent"
        );
    }

    /// 回归测试：ToggleWindow 改为基于真实窗口可见性的隐藏/显示
    /// 回归测试：SetAutostart 命令在无窗口环境下安全处理。
    #[test]
    fn test_battery_care_toggle_preserves_desired_limit() {
        let mut app = test_app();
        app.config.battery_charge_limit = 60;

        app.set_battery_care_internal(true);
        assert!(app.runtime.battery_care_enabled);
        assert_eq!(app.runtime.charge_limit, 60);
        assert_eq!(app.config.battery_charge_limit, 60);

        // Disabling raises the hardware limit to 100% but must keep the
        // desired limit so it is not lost.
        app.set_battery_care_internal(false);
        assert!(!app.runtime.battery_care_enabled);
        assert_eq!(app.runtime.charge_limit, 100);
        assert_eq!(app.config.battery_charge_limit, 60);

        // Re-enabling must restore the desired limit, not the 100% hardware
        // limit left behind by the disable.
        app.set_battery_care_internal(true);
        assert!(app.runtime.battery_care_enabled);
        assert_eq!(app.runtime.charge_limit, 60);
        assert_eq!(app.config.battery_charge_limit, 60);
    }

    #[test]
    fn test_battery_care_enable_falls_back_to_80_when_limit_is_100() {
        let mut app = test_app();
        app.config.battery_charge_limit = 100;

        app.set_battery_care_internal(true);
        assert!(app.runtime.battery_care_enabled);
        assert_eq!(app.runtime.charge_limit, 80);
        assert_eq!(app.config.battery_charge_limit, 80);
    }

    #[test]
    fn test_charge_limit_syncs_battery_care_flag() {
        let mut app = test_app();
        app.runtime.battery_care_enabled = true;

        // 100% limit means battery care is off.
        app.set_charge_limit_internal(100);
        assert!(!app.runtime.battery_care_enabled);
        assert_eq!(app.runtime.charge_limit, 100);
        // 关闭养护时保留用户期望值（默认 80），供重新开启养护时恢复。
        assert_eq!(app.config.battery_charge_limit, 80);

        // A limit below 100% turns battery care back on.
        app.set_charge_limit_internal(90);
        assert!(app.runtime.battery_care_enabled);
        assert_eq!(app.runtime.charge_limit, 90);
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
        app.runtime.charge_limit = 60;
        app.runtime.battery_care_enabled = true;

        // 拖到 100%：硬件提升到 100（养护关闭），但 config 期望值保留 60。
        app.set_charge_limit_internal(100);
        assert!(!app.runtime.battery_care_enabled);
        assert_eq!(app.runtime.charge_limit, 100);
        assert_eq!(
            app.config.battery_charge_limit, 60,
            "desired limit must be preserved"
        );

        // 重新开启养护：恢复 60% 而不是兜底 80%。
        app.set_battery_care_internal(true);
        assert!(app.runtime.battery_care_enabled);
        assert_eq!(app.runtime.charge_limit, 60);
        assert_eq!(app.config.battery_charge_limit, 60);
    }

    /// 回归测试：运行时养护状态与持久化配置不一致（auto_apply 关闭且硬件
    /// 状态被外部改动时，refresh_from_backend 只更新运行时）时，拖动上限后
    /// 持久化配置必须与限值保持自洽。否则下次启动 apply_startup_config 会
    /// 按 care=false 强制写 100%，静默摧毁用户设置的充电上限。
    #[test]
    fn test_charge_limit_sync_persists_care_when_runtime_diverged() {
        let store = test_store();
        let mock = MockBackend::default();
        let mut app = XiaomiApp::new(
            store,
            Box::new(mock.clone()),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        // 模拟：硬件养护已开启（limit=60），持久化配置仍是旧值
        // care=false, limit=100（如 auto_apply 关闭时外部改动硬件）。
        mock.charge_limit
            .store(60, std::sync::atomic::Ordering::Relaxed);
        mock.battery_care
            .store(true, std::sync::atomic::Ordering::Relaxed);
        app.refresh_from_backend();
        assert!(app.runtime.battery_care_enabled);
        assert!(!app.config.battery_care_enabled);

        // 用户把上限拖到 80：运行时 care 未变化（无写入分支），但持久化
        // 配置必须同步为 care=true, limit=80，保持自洽。
        app.set_charge_limit_internal(80);
        assert!(app.config.battery_care_enabled);
        assert_eq!(app.config.battery_charge_limit, 80);

        // 模拟下次启动 apply_startup_config：care=true → 写回 80%，
        // 用户设置不会被 100% 覆盖。
        assert_eq!(
            mock.charge_limit.load(std::sync::atomic::Ordering::Relaxed),
            80
        );
    }

    #[test]
    fn test_refresh_from_backend_keeps_config_untouched() {
        let store = test_store();
        let mock = MockBackend::default();
        let mut app = XiaomiApp::new(
            store,
            Box::new(mock.clone()),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        app.config.battery_charge_limit = 60;
        app.config.battery_care_enabled = false;

        // Mock backend reports a different hardware state than the config.
        mock.battery_care
            .store(true, std::sync::atomic::Ordering::Relaxed);
        mock.charge_limit
            .store(80, std::sync::atomic::Ordering::Relaxed);
        mock.perf_mode
            .store(PerfMode::Quiet as u8, std::sync::atomic::Ordering::Relaxed);

        app.refresh_from_backend();
        assert!(app.runtime.battery_care_enabled);
        assert_eq!(app.runtime.charge_limit, 80);
        assert_eq!(app.runtime.performance_mode, PerfMode::Quiet as u8);
        assert!(app.error_msg.is_none());

        // The persisted desired settings must not be overwritten.
        assert_eq!(app.config.battery_charge_limit, 60);
        assert!(!app.config.battery_care_enabled);
    }

    /// 回归测试：损坏的 EC 读值（充电上限 >100，如寄存器返回 0xFF）不得
    /// 显示为荒谬百分比或使 UI 状态溢出——刷新时必须钳制到 100%。
    #[test]
    fn test_refresh_clamps_charge_limit_above_100() {
        let store = test_store();
        let mock = MockBackend::default();
        let mut app = XiaomiApp::new(
            store,
            Box::new(mock.clone()),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        mock.charge_limit
            .store(150, std::sync::atomic::Ordering::Relaxed);
        mock.battery_care
            .store(false, std::sync::atomic::Ordering::Relaxed);

        app.refresh_from_backend();

        assert_eq!(app.runtime.charge_limit, 100);
        assert!(!app.runtime.battery_care_enabled);
    }

    /// 回归测试（M5）：读回 care=true + limit>100（垃圾值场景）时，钳制后
    /// 必须以**上限**为权威重新推导养护位，杜绝"电池养护: 开启 · 充电上限:
    /// 100%"的矛盾组合展示。历史实现把 care 原样写入 runtime，钳制仅作用
    /// 于 limit，两个字段对同一硬件状态给出相反含义。
    #[test]
    fn test_refresh_rebases_care_from_clamped_limit() {
        let store = test_store();
        let mock = MockBackend::default();
        let mut app = XiaomiApp::new(
            store,
            Box::new(mock.clone()),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        // 垃圾值场景：care 位=true 但上限 150%（读回 0xFF 之类）。
        mock.charge_limit
            .store(150, std::sync::atomic::Ordering::Relaxed);
        mock.battery_care
            .store(true, std::sync::atomic::Ordering::Relaxed);

        app.refresh_from_backend();

        assert_eq!(app.runtime.charge_limit, 100);
        assert!(
            !app.runtime.battery_care_enabled,
            "care must be rebased from clamped limit (100 => care off)"
        );
    }

    /// 回归测试（B-WMI-1）：刷新必须通过 get_battery_state 单次往返获取电池
    /// 数据。旧实现分别调用 get_battery_care_enabled + get_charge_limit，
    /// 在 WMI 后端下每次刷新做两次请求相同数据的完整 WMI 往返。
    #[test]
    fn test_refresh_uses_single_battery_roundtrip() {
        let store = test_store();
        let mock = MockBackend::default();
        let calls = mock.battery_state_calls.clone();
        let mut app = XiaomiApp::new(
            store,
            Box::new(mock.clone()),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        // 构造时已刷新过一次，清零后验证显式刷新只发一次电池往返。
        calls.store(0, std::sync::atomic::Ordering::Relaxed);
        mock.battery_care
            .store(true, std::sync::atomic::Ordering::Relaxed);
        mock.charge_limit
            .store(80, std::sync::atomic::Ordering::Relaxed);

        app.refresh_from_backend();

        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(app.runtime.battery_care_enabled);
        assert_eq!(app.runtime.charge_limit, 80);
        assert!(app.error_msg.is_none());
    }

    /// 回归测试：切换后端后，新后端读取失败产生的错误必须保留在 GUI 中展示
    /// （F-ERR-03），不得被切换逻辑清空（曾因 refresh 后无条件 error_msg=None
    /// 导致切换到一个读取全部失败的后端时错误信息被立即抹掉）。
    #[test]
    fn test_switch_backend_preserves_read_errors() {
        let store = test_store();
        let mut app = XiaomiApp::new(
            store,
            Box::new(MockBackend::default()),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        assert!(app.error_msg.is_none());

        // 切换到读取全部失败的后端：refresh_from_backend 会设置错误信息，
        // 切换逻辑不得将其清空。
        let ok = app.apply_backend_switch(
            Box::new(MockBackend::all_fail("failing", BackendPreference::Auto)),
            BackendPreference::Wmi,
        );
        assert!(ok);
        assert_eq!(app.backend.name(), "failing");
        let err = app.error_msg.as_deref().unwrap_or_default();
        assert!(err.contains("读取性能模式"), "unexpected: {}", err);
        assert!(err.contains("读取电池状态"), "unexpected: {}", err);
    }

    /// 回归测试：电源切换重设失败时，错误必须合并进 GUI 展示（F-ERR-03），
    /// 且不得被 refresh_from_backend 成功时的 error_msg 清空逻辑吞掉。
    #[test]
    fn test_reapply_config_reports_write_errors() {
        let mut app = failing_app();
        app.config.auto_reapply_on_power_change = true;

        let ctx = egui::Context::default();
        app.cmd_tx.send(UiCommand::ReapplyConfig).unwrap();
        app.process_commands(&ctx);

        let err = app.error_msg.as_deref().unwrap_or_default();
        assert!(err.contains("重设充电上限失败"), "unexpected: {}", err);
        assert!(err.contains("重设电池养护失败"), "unexpected: {}", err);
        assert!(err.contains("重设性能模式失败"), "unexpected: {}", err);
        assert!(
            err.contains("读取性能模式"),
            "read errors must be preserved: {}",
            err
        );
    }

    /// 回归测试：`apply_config_and_sync`（用户主动重设，如勾选自动切节能）
    /// 必须**不受** `auto_reapply_on_power_change` 开关约束——主动操作无条件
    /// 应用。历史实现把两者绑在一起，开关关闭时用户勾选"电池供电自动切节能"
    /// 静默不生效。
    #[test]
    fn test_apply_config_and_sync_ignores_reapply_switch() {
        let store = test_store();
        let mock = MockBackend::default();
        let hw_perf = mock.perf_mode.clone();
        let mut app = XiaomiApp::new(
            store,
            Box::new(mock.clone()),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        // 重设开关关闭：电源切换/唤醒路径会忽略，但主动应用必须仍生效。
        app.config.auto_reapply_on_power_change = false;
        app.config.performance_mode = 0x04; // 狂暴
        hw_perf.store(0x09, std::sync::atomic::Ordering::Relaxed);

        app.apply_config_and_sync();

        assert!(
            hw_perf.load(std::sync::atomic::Ordering::Relaxed) != 0x09,
            "hardware perf mode must be rewritten despite reapply switch off"
        );
    }

    /// 回归测试：开启养护时 set_charge_limit 成功、但 set_battery_care 失败
    /// （如 EC 拒绝写入养护位）时，硬件限值已生效，UI/配置必须按限值保持
    /// 自洽（限值是两种后端判定养护状态的权威依据）。否则下次启动会按旧的
    /// care=false 强制写 100%，静默覆盖用户设置的限值。
    #[test]
    fn test_battery_care_enable_partial_failure_keeps_config_coherent() {
        let store = test_store();
        let mut app = XiaomiApp::new(
            store,
            Box::new(MockBackend::partial_care("partial-care")),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        // 模拟用户养护关闭、期望上限 60%。
        app.config.battery_charge_limit = 60;
        app.config.battery_care_enabled = false;
        app.runtime.battery_care_enabled = false;
        app.runtime.charge_limit = 100;

        // 开启养护：limit 写入成功（60%），care 位写入失败。
        app.set_battery_care_internal(true);

        // 限值已生效：状态与持久化配置必须按限值自洽（养护开启、上限 60），
        // 并且错误要在 GUI 展示。
        assert!(app.runtime.battery_care_enabled);
        assert!(app.config.battery_care_enabled);
        assert_eq!(app.runtime.charge_limit, 60);
        assert_eq!(app.config.battery_charge_limit, 60);
        assert!(app
            .error_msg
            .as_deref()
            .unwrap_or_default()
            .contains("设置电池养护失败"));

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
        let store = test_store();
        let mock = MockBackend::default();
        let mut app = XiaomiApp::new(
            store,
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
        assert_eq!(
            mock.charge_limit.load(std::sync::atomic::Ordering::Relaxed),
            80
        );
        assert!(mock.battery_care.load(std::sync::atomic::Ordering::Relaxed));
        assert!(app.error_msg.is_none());
    }

    #[test]
    fn test_reapply_config_write_failure_keeps_original_limit() {
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
        let store = test_store();
        let backend = MockBackend::quantizing();
        let hw_limit = backend.charge_limit.clone();
        let mut app = XiaomiApp::new(
            store,
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
        assert_eq!(app.runtime.charge_limit, 80);
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

        assert!(!app.runtime.battery_care_enabled);
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
        let store = test_store();
        let mut app = XiaomiApp::new(
            store,
            Box::new(MockBackend::all_fail("failing", BackendPreference::Auto)),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        // 用户期望 100%（触发兜底分支），写入全部失败。
        app.config.battery_charge_limit = 100;
        app.runtime.charge_limit = 100;

        app.set_battery_care_internal(true);

        // config 不得被提前改写为 80：写入失败时保持用户原值。
        assert_eq!(app.config.battery_charge_limit, 100);
        assert_eq!(app.runtime.charge_limit, 100);
        assert!(!app.runtime.battery_care_enabled);
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
        app.set_perf_mode_internal(PerfMode::Quiet);

        assert_eq!(app.runtime.performance_mode, PerfMode::Smart as u8);
        assert_eq!(app.config.performance_mode, PerfMode::Smart as u8);
        assert!(app
            .error_msg
            .as_deref()
            .unwrap_or_default()
            .contains("设置性能模式失败"));
    }

    /// 回归测试：设置充电上限成功、但联动养护位写入失败时，配置必须保持
    /// 自洽（care 由限值推导），不允许出现 care=false + limit=60 的矛盾组合
    /// ——否则下次启动 auto_apply 会按 care=false 强制写 100%，
    /// 用户选择的 60% 充电上限被静默摧毁。
    #[test]
    fn test_charge_limit_care_sync_failure_keeps_config_coherent() {
        let store = test_store();
        let mut app = XiaomiApp::new(
            store,
            Box::new(MockBackend::partial_care("partial-care")),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        // 模拟用户当前养护关闭、上限 100%（运行时与配置一致）。
        app.runtime.battery_care_enabled = false;
        app.config.battery_care_enabled = false;
        app.runtime.charge_limit = 100;
        app.config.battery_charge_limit = 100;

        // 用户把上限拖到 60%：limit 写入成功，但联动开启的 care 位写入失败。
        app.set_charge_limit_internal(60);

        // 限值是两个后端判定养护状态的权威依据：即使 care 位写失败，
        // 状态与持久化配置也必须按限值保持一致，且错误要在 GUI 展示。
        assert_eq!(app.runtime.charge_limit, 60);
        assert!(app.runtime.battery_care_enabled);
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

    /// 回归测试：WMI 后端下，养护开启时若 config 中的上限不是预设值（例如
    /// 之前用 WinRing0 保存的 85%），硬件实际写入就近预设 80%。UI 与持久化
    /// 配置必须显示硬件实际生效的 80%，而不是请求的 85%（AC-BAT-04）。
    #[test]
    fn test_wmi_quantization_readback_keeps_ui_in_sync() {
        let store = test_store();
        let backend = MockBackend::quantizing();
        let hw_limit = backend.charge_limit.clone();
        let mut app = XiaomiApp::new(
            store,
            Box::new(backend),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        // 模拟之前用 WinRing0 保存的非预设上限。
        app.config.battery_charge_limit = 85;
        app.runtime.battery_care_enabled = false;

        app.set_battery_care_internal(true);

        // UI 与持久化配置与硬件实际生效值一致（80%），而非请求值（85%）。
        assert_eq!(app.runtime.charge_limit, 80);
        assert_eq!(app.config.battery_charge_limit, 80);
        assert!(app.runtime.battery_care_enabled);
        assert!(app.config.battery_care_enabled);
        assert_eq!(hw_limit.load(std::sync::atomic::Ordering::Relaxed), 80);
    }

    /// 回归测试：WMI 后端下直接拖动上限到非预设值，同样需要读回硬件实际
    /// 生效值，防止 UI/配置与硬件状态不一致。
    #[test]
    fn test_wmi_quantization_readback_on_charge_limit_set() {
        let store = test_store();
        let backend = MockBackend::quantizing();
        let hw_limit = backend.charge_limit.clone();
        let mut app = XiaomiApp::new(
            store,
            Box::new(backend),
            AppConfig::default(),
            crate::ec::config::BackendPreference::Auto,
            None,
            false,
        );
        app.config.battery_charge_limit = 100;
        app.runtime.battery_care_enabled = false;

        app.set_charge_limit_internal(85);

        assert_eq!(app.runtime.charge_limit, 80);
        assert_eq!(app.config.battery_charge_limit, 80);
        assert!(app.runtime.battery_care_enabled);
        assert!(app.config.battery_care_enabled);
        assert_eq!(hw_limit.load(std::sync::atomic::Ordering::Relaxed), 80);
    }

    /// Fn 绑定：修改动作必须同步进共享绑定表（监听线程即时生效）。
    #[test]
    fn test_set_fn_binding_action_updates_shared_state() {
        let mut app = test_app();
        app.set_fn_binding_action(0, crate::ec::fnkey::FnAction::ToggleBatteryCare);
        assert_eq!(
            app.config.fn_key_bindings[0].action,
            crate::ec::fnkey::FnAction::ToggleBatteryCare
        );
        let snapshot = app.fn_bindings.read().unwrap().clone();
        assert_eq!(
            snapshot[0].action,
            crate::ec::fnkey::FnAction::ToggleBatteryCare
        );
        // 越界 index 必须被安全忽略（不 panic、不改状态）。
        app.set_fn_binding_action(99, crate::ec::fnkey::FnAction::ReapplyConfig);
        assert_eq!(app.config.fn_key_bindings.len(), 1);
    }

    /// Fn 绑定：add 相同 (class,prefix) 不重复，只更新动作。
    #[test]
    fn test_add_fn_binding_dedup_and_normalize() {
        let mut app = test_app();
        // 带分隔符/小写输入归一化后与默认 Fn+K 相同 → 只更新动作。
        app.add_fn_binding("HID_EVENT20", "01-28-01", crate::ec::fnkey::FnAction::None);
        assert_eq!(app.config.fn_key_bindings.len(), 1);
        assert_eq!(
            app.config.fn_key_bindings[0].action,
            crate::ec::fnkey::FnAction::None
        );

        // 新键码追加。
        app.add_fn_binding(
            "HID_EVENT20",
            "0107",
            crate::ec::fnkey::FnAction::ReapplyConfig,
        );
        assert_eq!(app.config.fn_key_bindings.len(), 2);
        assert_eq!(app.config.fn_key_bindings[1].prefix, "0107");
    }

    /// Fn 绑定：删除与共享状态同步。
    #[test]
    fn test_remove_fn_binding() {
        let mut app = test_app();
        app.remove_fn_binding(0);
        assert!(app.config.fn_key_bindings.is_empty());
        assert!(app.fn_bindings.read().unwrap().is_empty());
        // 越界安全。
        app.remove_fn_binding(0);
    }

    /// Fn 捕获开关：开启后切换标记并记录最近事件。
    #[test]
    fn test_toggle_fn_capture() {
        let mut app = test_app();
        assert!(!app.fn_capture.load(std::sync::atomic::Ordering::Relaxed));
        app.toggle_fn_capture();
        assert!(app.fn_capture.load(std::sync::atomic::Ordering::Relaxed));

        // FnEventSeen 命令更新最近捕获。
        let ctx = egui::Context::default();
        app.cmd_tx
            .send(UiCommand::FnEventSeen {
                class: "HID_EVENT20".into(),
                hex: "012801".into(),
            })
            .unwrap();
        app.process_commands(&ctx);
        assert_eq!(
            app.last_fn_event,
            Some(("HID_EVENT20".to_string(), "012801".to_string()))
        );

        app.toggle_fn_capture();
        assert!(!app.fn_capture.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(app.last_fn_event, None, "capture off clears last event");
    }

    /// 延迟恢复探测结果（UiCommand::WmiAvailable）应用：用户偏好仍是
    /// WMI/Auto（希望 WMI 生效）且当前是回退后端时，探测到的 WMI 后端被
    /// 切换为活动后端。
    #[test]
    fn test_wmi_available_applies_when_preference_wants_wmi() {
        let store = test_store();
        // 构造时后端为 WinRing0（模拟首次启动 WMI 失败回退），偏好保持
        // AppConfig 默认 Wmi（AUTO 语义下当前实例实际是 WinRing0）。
        let mut app = XiaomiApp::new(
            store,
            Box::new(MockBackend::all_fail(
                "pref-winring0",
                BackendPreference::WinRing0,
            )),
            AppConfig::default(),
            BackendPreference::WinRing0,
            None,
            false,
        );
        app.error_msg = None;
        assert_eq!(app.config.backend, BackendPreference::Wmi);

        let ctx = egui::Context::default();
        app.cmd_tx
            .send(UiCommand::WmiAvailable(Box::new(MockBackend::all_fail(
                "pref-wmi",
                BackendPreference::Wmi,
            ))))
            .unwrap();
        app.process_commands(&ctx);

        assert_eq!(app.backend.preference(), BackendPreference::Wmi);
        // 偏好（config.backend）保持不变，仅活动后端切换为 WMI。
        assert_eq!(app.config.backend, BackendPreference::Wmi);
        assert_eq!(app.current_pref, BackendPreference::Wmi);
        assert!(
            app.wmi_recover_at.is_none(),
            "recovery must stop after successful switch"
        );
    }

    /// Auto 偏好下的延迟恢复：config.backend=Auto（实际后端 WinRing0）时，
    /// 探测成功后活动后端切为 WMI，偏好仍保持 Auto（current_pref=Auto）。
    #[test]
    fn test_wmi_available_applies_keeping_auto_preference() {
        let store = test_store();
        let mut app = XiaomiApp::new(
            store,
            Box::new(MockBackend::all_fail(
                "pref-winring0",
                BackendPreference::WinRing0,
            )),
            AppConfig {
                backend: BackendPreference::Auto,
                ..Default::default()
            },
            BackendPreference::WinRing0,
            None,
            false,
        );
        app.error_msg = None;
        assert_eq!(app.config.backend, BackendPreference::Auto);

        let ctx = egui::Context::default();
        app.cmd_tx
            .send(UiCommand::WmiAvailable(Box::new(MockBackend::all_fail(
                "pref-wmi",
                BackendPreference::Wmi,
            ))))
            .unwrap();
        app.process_commands(&ctx);

        assert_eq!(app.backend.preference(), BackendPreference::Wmi);
        assert_eq!(app.config.backend, BackendPreference::Auto);
        assert_eq!(app.current_pref, BackendPreference::Auto);
    }

    /// 探测结果过期：探测期间用户手动把偏好切到 WinRing0，迟到的 WMI
    /// 探测结果必须被丢弃，不得覆盖用户的最新选择。
    #[test]
    fn test_wmi_available_discarded_when_user_picked_winring0() {
        let store = test_store();
        let mut app = XiaomiApp::new(
            store,
            Box::new(MockBackend::all_fail(
                "pref-winring0",
                BackendPreference::WinRing0,
            )),
            AppConfig {
                backend: BackendPreference::WinRing0,
                ..Default::default()
            },
            BackendPreference::WinRing0,
            None,
            false,
        );
        app.error_msg = None;
        let backend_before = app.backend.name();

        let ctx = egui::Context::default();
        app.cmd_tx
            .send(UiCommand::WmiAvailable(Box::new(MockBackend::all_fail(
                "pref-wmi",
                BackendPreference::Wmi,
            ))))
            .unwrap();
        app.process_commands(&ctx);

        assert_eq!(
            app.backend.name(),
            backend_before,
            "probed backend must be dropped when user switched preference"
        );
        assert_eq!(app.config.backend, BackendPreference::WinRing0);
    }

    /// 回归测试：当前后端已经是 WMI 时，迟到的探测结果必须被丢弃（避免
    /// 重复切换把正在使用的后端重建一遍）。
    #[test]
    fn test_wmi_available_discarded_when_already_wmi() {
        let store = test_store();
        let mut app = XiaomiApp::new(
            store,
            Box::new(MockBackend::all_fail("pref-wmi", BackendPreference::Wmi)),
            AppConfig::default(),
            BackendPreference::Wmi,
            None,
            false,
        );
        app.error_msg = None;
        let backend_before = app.backend.name();

        let ctx = egui::Context::default();
        app.cmd_tx
            .send(UiCommand::WmiAvailable(Box::new(MockBackend::all_fail(
                "pref-wmi",
                BackendPreference::Wmi,
            ))))
            .unwrap();
        app.process_commands(&ctx);

        assert_eq!(
            app.backend.name(),
            backend_before,
            "must not recreate an already-active WMI backend"
        );
    }
}
