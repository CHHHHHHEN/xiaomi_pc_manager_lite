use eframe::egui;

use crate::app;
use crate::app::command::UiCommand;
use crate::app::config::BackendPreference;
use crate::app::limits::FULL_CHARGE_LIMIT;
use crate::app::performance::PerfMode;
use crate::ec;
use crate::util::err_fmt;

use super::app::XiaomiApp;

/// NFR-REL-03：连续硬件读取失败达到此阈值后，GUI 展示持久提示（见
/// `refresh_from_backend` 的计数逻辑）。
const HW_FAILURE_PAUSE_THRESHOLD: u32 = 3;

/// 用户可见错误文案（修订 1.47 收敛）：同名字面量散落多处、各自手写，
/// 修改措辞时容易漏改其一导致文案漂移（测试锁定这些文案的 contains 断言）。
/// 统一收敛为单一来源——带占位符的文案共用 `util::err_fmt`（修订 1.49
/// 整理 + 1.50 提升到 leaf 层供全项目复用）；无占位符的 `ERR_SET_CARE`
/// 用常量。
const ERR_SET_CHARGE_LIMIT: &str = "设置充电上限失败";
const ERR_SYNC_CARE: &str = "同步电池养护状态失败";
const ERR_BACKEND_SWITCH: &str = "后端切换失败";
const ERR_AUTOSTART: &str = "设置开机自启动失败";
const ERR_SET_CARE: &str = "设置电池养护失败";

impl XiaomiApp {
    /// 每帧从命令通道取出全部待处理命令并分发。
    ///
    /// 本函数只做**路由**：取命令、记录日志、转交 `handle_command`、在消费
    /// 过任何命令后请求重绘。各命令的具体处理收敛在 `handle_command` 的
    /// 分支方法中，避免一个巨型 match 同时承担"路由"与"业务逻辑"。
    pub fn process_commands(&mut self, ctx: &egui::Context) {
        let mut needs_repaint = false;
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            needs_repaint = true;
            // 命令来源在界面外（托盘/热键/Fn+K/电源广播），GUI 不透明的主要
            // 排查入口：记录每一条命令以便对照"用户操作了什么、程序做了何反应"。
            log::info!("UiCommand: {:?}", cmd);
            self.handle_command(cmd, ctx);
        }
        if needs_repaint {
            ctx.request_repaint();
        }
    }

    /// 单条命令的分发（process_commands 的匹配主体，拆出以便各分支独立
    /// 成方法、各自可测）。
    fn handle_command(&mut self, cmd: UiCommand, ctx: &egui::Context) {
        match cmd {
            UiCommand::ToggleBatteryCare => {
                self.set_battery_care_internal(!self.runtime.battery_care_enabled);
            }
            UiCommand::CyclePerfMode => self.handle_cycle_perf_mode(),
            UiCommand::SetPerfMode(mode) => self.handle_set_perf_mode(mode),
            // 电源广播（插拔/唤醒）触发的重设：受 auto_reapply 开关门控
            //（见 reapply_config）。
            UiCommand::ReapplyConfig => self.reapply_config(),
            // Fn 绑定"重新应用设置"：用户主动动作，不受自动重设开关门控
            //（见 ReapplyConfigManual 的注释，修订 1.30 M3 回归）。
            UiCommand::ReapplyConfigManual => self.apply_config_and_sync(),
            UiCommand::FnEventSeen { class, hex } => self.handle_fn_event_seen(class, hex),
            UiCommand::SetAutostart(enabled) => self.set_autostart(enabled),
            UiCommand::SetAutostartResult(enabled, result) => {
                self.handle_autostart_result(enabled, result);
            }
            UiCommand::WmiAvailable(backend) => self.handle_wmi_available(backend),
            UiCommand::BatteryHealthUpdated {
                designed_mwh,
                full_mwh,
            } => self.handle_battery_health_updated(designed_mwh, full_mwh),
            UiCommand::BatteryEtaUpdated {
                remaining_mwh,
                charge_rate_mw,
                discharge_rate_mw,
                charging,
                discharging,
            } => self.handle_battery_eta_updated(
                remaining_mwh,
                charge_rate_mw,
                discharge_rate_mw,
                charging,
                discharging,
            ),
            UiCommand::BackendSwitchResult { user_pref, result } => {
                // 手动后端切换的后台创建结果（try_switch_backend 已异步化，
                // 修订 1.36）：消费时校验偏好未变，过期结果丢弃。
                self.handle_backend_switch_result(user_pref, result);
            }
            UiCommand::Quit => self.handle_quit(ctx),
        }
    }

    /// 托盘/热键循环切换性能模式（UiCommand::CyclePerfMode）。
    ///
    /// 循环序列定义在领域模块（app::performance::CYCLE）。当前模式未知
    /// （硬件读回未定义代码 0x00/0xFF 等）时按领域语义回到循环首项
    /// Smart——与 next_cycle_mode 对"不在序列内"的处理一致。历史实现
    /// 静默把未知当成 Smart 再取下一项（写出 Quiet），既违反循环契约
    /// 又无日志。
    fn handle_cycle_perf_mode(&mut self) {
        match PerfMode::from_ec_value(self.runtime.performance_mode) {
            Some(current) => {
                let next = app::performance::next_cycle_mode(current);
                self.set_perf_mode_internal(next);
            }
            None => {
                log::warn!(
                    "CyclePerfMode: unknown current mode {:#x}; cycling to {}",
                    self.runtime.performance_mode,
                    PerfMode::Smart.name()
                );
                self.set_perf_mode_internal(PerfMode::Smart);
            }
        }
    }

    /// 托盘子菜单直接按值设置性能模式（UiCommand::SetPerfMode）。
    fn handle_set_perf_mode(&mut self, mode: u8) {
        // 未知值（损坏/旧配置）安全忽略。
        match PerfMode::from_ec_value(mode) {
            Some(m) => self.set_perf_mode_internal(m),
            None => log::warn!("SetPerfMode: unknown mode {:#x} ignored", mode),
        }
    }

    /// Fn 捕获模式下的实时事件（UiCommand::FnEventSeen）。
    ///
    /// 记录最近一条，GUI 展示并用于添加新绑定。事件频率由用户按键节奏
    /// 决定，仅保留最新一条不缓存历史。
    ///
    /// **按下/释放过滤**（修订 1.31，L-回归）：固件对一次物理按键发送按下
    /// （`012801`）与释放（`012800`）两条事件，释放总是**后到**。若直接把
    /// 释放事件写入 `last_fn_event`，"最近捕获"显示 `01-28-00`、"使用此键"
    /// 绑定 `012800`——下一次物理按键发出 `012801` 不再命中此前缀，绑定
    /// 失效或变成"在释放时触发"（与 F-FNK-06 的按下/释放语义冲突）。修复：
    /// 释放事件（hex 以 `00` 结尾）不得覆盖同键码的按下事件（`...01`）。
    fn handle_fn_event_seen(&mut self, class: String, hex: String) {
        let keep_previous =
            Self::keep_press_over_release(self.last_fn_event.as_ref(), &class, &hex);
        if !keep_previous {
            // 捕获事件在 watcher 侧已限流（CAPTURE_FORWARD_MIN_MS，L2 回归），
            // 正常按键频率下不刷屏；这里 debug 级避免与上方 UiCommand 全量
            // 日志重复。仅保留最新一条。
            log::debug!("Fn capture event: {} / {}", class, hex);
            self.last_fn_event = Some((class, hex));
        }
    }

    /// 电池健康读数更新（UiCommand::BatteryHealthUpdated）。
    ///
    /// 读数（root\WMI 容量）变化很慢（容量磨损/校准），直接覆盖不合并。
    /// 健康度由容量推导（满充/设计），除数量级外无其它校验需求。
    fn handle_battery_health_updated(&mut self, designed_mwh: u32, full_mwh: u32) {
        let health = crate::platform::battery_health::BatteryHealth {
            designed_mwh,
            full_mwh,
        };
        let pct = health.health_percent_u8();
        log::info!(
            "Battery health: designed={} mWh, full={} mWh (health={:?})",
            designed_mwh,
            full_mwh,
            pct
        );
        self.battery_health = Some(health);
        // 与托盘侧一致的毒锁恢复（worker.rs 用 lock_or_recover）：毒锁下静默
        // 跳过会让托盘 tooltip 与 GUI 状态长期背离，恢复 + 告警统一收敛
        //（util.rs 约定）。
        self.with_tray_status().battery_health_percent = pct;
    }

    /// 电池充放电状态更新（UiCommand::BatteryEtaUpdated）。
    ///
    /// 预计剩余/充满时长（root\WMI BatteryStatus，修订 1.37）：由剩余容量
    /// 与充放电速率估算，与电池健康无关的另一条信息。速率无效（0/未充
    /// 放电/异常）时展示"未知"。放电只需剩余容量与放电速率；充电额外需要
    /// 满充容量（只在此分支读取）。
    fn handle_battery_eta_updated(
        &mut self,
        remaining_mwh: u32,
        charge_rate_mw: u32,
        discharge_rate_mw: u32,
        charging: bool,
        discharging: bool,
    ) {
        let text = if discharging {
            crate::platform::battery_health::eta_discharge_minutes(remaining_mwh, discharge_rate_mw)
                .map(|m| {
                    format!(
                        "预计剩余约 {}",
                        crate::platform::battery_health::format_minutes(m)
                    )
                })
        } else if charging {
            let full_mwh = self.battery_health.map(|h| h.full_mwh).unwrap_or(0);
            // 充电目标按养护上限截断（修订 1.46）：养护开启时硬件在
            // charge_limit%（如 80%）即停充，"预计充满"必须算到上限而非
            // 100%——否则用户看到"3 小时充满"，实际 2 小时就到 80% 停充，
            // 文案与硬件行为不符。
            let target_mwh = {
                let limit = self.runtime.charge_limit.min(FULL_CHARGE_LIMIT) as u32;
                let t = if limit < FULL_CHARGE_LIMIT as u32 {
                    full_mwh as u64 * limit as u64 / 100
                } else {
                    full_mwh as u64
                };
                t as u32
            };
            crate::platform::battery_health::eta_charge_minutes(
                remaining_mwh,
                charge_rate_mw,
                target_mwh,
            )
            .map(|m| {
                format!(
                    "预计充满约 {}",
                    crate::platform::battery_health::format_minutes(m)
                )
            })
        } else {
            None
        };
        self.battery_eta_text = text;
        self.with_tray_status().battery_eta_text = self.battery_eta_text.clone();
    }

    /// 退出命令（UiCommand::Quit）。
    ///
    /// 请求 eframe 正常退出事件循环：置位 quitting 后下一帧的 close_requested
    /// 放行（不再取消/隐藏到托盘），run_native 返回，各组件 Drop 正常执行
    /// （WinRing0 后端 DeinitializeOls 卸载驱动等）。不能用 process::exit
    /// 跳过清理（修订 1.21）。
    ///
    /// F-GUI-21（修订 1.33）：各变更路径本已即时持久化，退出时再兜底保存
    /// 一次——防御未来新增的 config 变更路径遗漏 save_state 导致退出丢配置
    ///（与"增量落盘"设计不冲突）。
    fn handle_quit(&mut self, ctx: &egui::Context) {
        log::info!("Quit: setting quitting flag");
        // 先等待在飞的自启动操作收尾（可能回滚配置），再兜底保存最终状态
        //（drain 内的回滚路径也会各自 save_state，此处统一兜底）。
        self.drain_pending_autostart();
        self.save_state();
        self.quitting = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// 退出前有界等待在飞的自启动操作（修订 1.50 修复）。
    ///
    /// 自启动注册/删除走**串行后台 worker**（可能尚未执行完）：用户取消勾选
    /// "开机自启动"后立刻经托盘"退出"，若 `DeleteTask` 尚未完成进程就结束，
    /// 计划任务残留而配置已落盘为关——下次启动 `autostart::sync` 对"配置关 +
    /// 任务在"采取保守不删除，任务被永久残留、App 照常自启，与配置矛盾
    /// （F-AUTO-03 契约背离）。此处有界阻塞等待 `autostart_in_flight` 归零，
    /// 期间只处理 `SetAutostartResult` 回执（退出中其余命令丢弃）；
    /// 超时/通道关闭则放弃（正常操作毫秒级完成，3s 仅覆盖极端挂起）。
    fn drain_pending_autostart(&mut self) {
        const AUTOSTART_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
        if self.autostart_in_flight == 0 {
            return;
        }
        log::info!(
            "Quit: waiting for {} pending autostart operation(s)",
            self.autostart_in_flight
        );
        let deadline = std::time::Instant::now() + AUTOSTART_DRAIN_TIMEOUT;
        while self.autostart_in_flight > 0 {
            let wait = deadline.saturating_duration_since(std::time::Instant::now());
            if wait.is_zero() {
                log::warn!(
                    "Quit: {} autostart result(s) still pending after {}s; exiting anyway",
                    self.autostart_in_flight,
                    AUTOSTART_DRAIN_TIMEOUT.as_secs()
                );
                return;
            }
            match self.cmd_rx.recv_timeout(wait) {
                Ok(UiCommand::SetAutostartResult(enabled, result)) => {
                    self.handle_autostart_result(enabled, result);
                }
                // 退出中其它后台命令（电池健康/捕获事件等）对最终状态无影响，
                // 直接丢弃，只等自启动回执。
                Ok(_other) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    log::warn!(
                        "Quit: autostart command channel closed with {} result(s) pending",
                        self.autostart_in_flight
                    );
                    return;
                }
            }
        }
    }
    /// 电源切换时重设配置（UiCommand::ReapplyConfig）。
    ///
    /// 与启动 apply 路径一致（见 app::battery::apply_config_to_hardware）：
    /// 统一处理"写限值 → 写养护 → 写性能模式（含狂暴的交流电源降级）"。
    /// 兜底只作用在辅助函数内部，**不**提前改写 config——写入失败时内存中的
    /// config 不会被污染。
    ///
    /// **门控条件**（修订 1.31）：`auto_reapply_on_power_change` 关闭时仍执行
    /// 的情形是"电池供电时自动切换节能"（`auto_switch_to_quiet_on_battery`）
    /// 开启——该功能依赖拔插电源事件才能生效，若被重设开关一起关掉，用户
    /// 明确开启的"电池自动切节能"就静默失效（配置陷阱，F-PWR-07）。重设
    /// 开关只约束"整份配置重写"，自动切节能是独立的、被用户显式请求的行为。
    pub(crate) fn reapply_config(&mut self) {
        let reapply_ok = self.config.auto_reapply_on_power_change;
        let quiet_ok = self.config.auto_switch_to_quiet_on_battery;
        if !reapply_ok && !quiet_ok {
            log::debug!(
                "ReapplyConfig ignored: auto_reapply_on_power_change off and auto_switch_to_quiet_on_battery off"
            );
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
        let outcome = app::battery::apply_config_to_hardware(
            &*self.backend,
            &self.config,
            crate::platform::power::power_status(),
        );
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
            crate::app::battery::sync_config_after_apply(&mut self.config, *applied);
        }
        // 写入失败字段的遍历统一收敛在 ApplyOutcome::field_errors。
        // 注意：**先收集、后展示**——refresh_from_backend 成功时会把
        // error_msg 清空、失败时整体替换，写入失败必须在刷新之后才
        // 合并展示（F-ERR-03），否则被清空/覆盖静默吞掉。
        let mut errs: Vec<String> = Vec::new();
        for (field, e) in outcome.field_errors() {
            log::error!("Reapply {} failed: {}", field, e);
            errs.push(format!("重设{}失败: {}", field, e));
        }
        // 规范化（如 care=true + limit=100 兜底为 80）修改了配置，
        // 需要持久化，否则配置文件中残留的矛盾组合每次都会被重写。
        self.save_state();
        self.refresh_from_backend();
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
                // thread::Builder + catch_unwind（修订 1.44）：与其它后台线程
                // （托盘/Fn/电池健康/wmi-worker）同规格——enable/disable 内部
                // 是 COM/Task Scheduler FFI 边界，panic 会静默终止本线程，
                // 之后的 autostart 请求全部落空且无日志；Builder 防 spawn 失败
                // panic 传播，catch_unwind 记录语义化错误（请求随后因通道关闭
                // 而 send 失败，GUI 侧已有"worker channel closed"告警）。
                let spawn = crate::util::spawn_guarded("autostart-worker", move || {
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
                if let Err(e) = spawn {
                    log::error!("failed to spawn autostart worker thread: {}", e);
                    // worker 未创建 = 操作必然不会执行（请求根本没投递），
                    // 与 send 失败路径共用同一套"操作未生效"处理。
                    self.fail_autostart_operation(enabled, format!("worker 线程创建失败 ({})", e));
                    return;
                }
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
            // **重置 worker 槽位**（修订 1.46 审计）：worker 已死但本实例仍持有
            // 其 Sender，后续每次切换都走进 send 失败路径——开机自启动在本会话
            // 内永久失效（唯一恢复途径是重启应用）。清空槽位后，下一次请求按
            // 惰性创建逻辑重建 worker，操作自动自愈。
            self.autostart_worker = None;
            self.fail_autostart_operation(enabled, format!("worker 已退出 ({})", e));
        } else {
            // 在飞计数 +1（修订 1.44）：见 `autostart_in_flight` 字段注释——
            // 结果回传时只有计数归零（= 本请求是最新）才允许 enable 失败回滚。
            self.autostart_in_flight = self.autostart_in_flight.saturating_add(1);
        }
    }

    /// 自启动操作**未生效**的统一处理（spawn 失败与 send 失败共用，修订 1.46
    /// 审计去重）：请求未投递 = 操作未执行，必须回滚/告警而非静默——
    /// - enable 请求失败：任务不会创建，配置却已勾选 → 回滚为未勾选
    ///   （与 SetAutostartResult 的 enable 失败回滚语义一致）；
    /// - disable 请求失败：配置保持用户选择（关，F-AUTO-03 契约），
    ///   但任务未删除、开机仍会自启——必须展示错误让用户知道操作
    ///   没有生效，否则静默背离（下次启动 sync 也不删除）。
    fn fail_autostart_operation(&mut self, enabled: bool, detail: String) {
        // 在飞计数清零（修订 1.47 审计）：本函数只在"请求未投递"时调用——
        // 要么 worker 从未创建（spawn 失败），要么 worker 已死（send 失败）。
        // 两种情况都不会再有 SetAutostartResult 回执到达，历史 `autostart_in_flight`
        // 计数永久泄漏：后续重建的 worker 结果回传时 `is_latest` 恒 false，
        // enable 失败回滚本会话内永久失效。此处清零恢复"最新意图"判定。
        self.autostart_in_flight = 0;
        if enabled {
            self.config.auto_start_on_boot = false;
            self.save_state();
        }
        self.push_error(err_fmt(ERR_AUTOSTART, detail));
    }

    /// 自启动 worker 的结果回执（UiCommand::SetAutostartResult）。
    ///
    /// 在飞计数 -1：仅当计数归零时本结果对应**最新**请求——此时配置的
    /// auto_start_on_boot 反映的就是本请求的意图，回滚安全。计数未归零说明
    /// 还有更新的请求在 worker 排队，本结果是旧请求的迟到回执，回滚会把
    /// 新意图误覆盖。
    fn handle_autostart_result(&mut self, enabled: bool, result: Result<(), String>) {
        let is_latest = {
            self.autostart_in_flight = self.autostart_in_flight.saturating_sub(1);
            self.autostart_in_flight == 0
        };
        match result {
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
                // **只回滚"仍是最新意图"的失败**（修订 1.44）：串行
                // worker 中较早请求的结果可能晚于更新的请求到达
                // （快速连点 ON→OFF→ON 时，先发的 enable#1 失败结果
                // 可能落后于 OFF#2 与 ON#3 已落盘）。此时配置反映的
                // 是更新的用户意图，回滚会把它覆盖回旧值，重新制造
                // "任务在而配置关"的背离（M3 回归）。
                if enabled && is_latest && self.config.auto_start_on_boot {
                    self.config.auto_start_on_boot = false;
                    self.save_state();
                }
                // **删除失败不回滚**（F-AUTO-03，修订 1.32）：取消勾选
                // 时配置必须仍按用户选择保存（关），仅在 GUI 展示错误。
                // 历史实现把 disable 失败回滚为勾选（true），与需求
                // 文档"删除失败时配置仍按用户选择保存"直接冲突——用户
                // 明确要关闭，因临时权限/占用失败被翻回开启，下次启动
                // 反而继续自启。关闭侧没有"复选框与实际任务状态背离"
                // 问题：任务计划中残留的只是陈旧任务，sync 启动时对
                // "配置关但任务在"采取保守不自动删除（autostart.rs
                // sync），留给用户决定，故无回滚必要。
                self.push_error(err_fmt(ERR_AUTOSTART, e));
            }
        }
    }

    /// WMI 延迟恢复探测结果（UiCommand::WmiAvailable）应用。
    ///
    /// 探测是后台异步的，期间用户可能手动切换了后端，必须校验
    /// "当前仍期望 WMI 且尚未恢复"才应用，否则丢弃过期结果——
    /// 误应用会把用户刚选的 WinRing0 覆盖回 WMI。
    fn handle_wmi_available(&mut self, backend: Box<dyn ec::backend::EcBackend>) {
        let wants_wmi = self.wants_wmi();
        if !wants_wmi {
            log::info!(
                "WMI delayed recovery: user preference no longer WMI; probed backend dropped"
            );
        } else if self.wmi_active_and_healthy() {
            // 当前已是**健康**的 WMI：探测结果丢弃（修订 1.45：
            // 熔断的 WMI 不算"已激活"——needs_rebuild 为 true 时
            // 探测结果要允许应用以自动重建，见 arm_wmi_recovery）。
            log::info!(
                "WMI delayed recovery: WMI already active and healthy; probed backend dropped"
            );
        } else if backend.preference() != BackendPreference::Wmi {
            log::warn!(
                "WMI delayed recovery: probed backend is '{}' not WMI; dropped",
                backend.name()
            );
        } else if self.pending_backend_switch.is_some() {
            // 手动切换进行中（修订 1.39）：探测结果让位于用户的手动
            // 请求——手动切换的结果会经 BackendSwitchResult 应用，
            // 探测结果此刻应用会抢先覆盖用户刚发起的切换意图。
            log::info!("WMI delayed recovery: manual backend switch in progress; probe dropped");
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

    // ── Fn 功能键绑定 ──────────────────────────────────────────────────
    // 配置 `config.fn_key_bindings` 是持久化事实来源；`self.fn_bindings`
    // （`Arc<RwLock<Vec<_>>>`）是与 Fn 监听线程共享的运行时镜像。每次
    // 修改配置后必须经 `commit_fn_bindings` 同时更新共享镜像并落盘，
    // 否则 GUI 里改的绑定在监听线程不生效、或重启后丢失。

    /// 把当前配置中的绑定表同步进共享镜像并持久化（唯一提交点）。
    fn commit_fn_bindings(&mut self) {
        // 与全项目锁恢复约定一致（util::lock_write_or_recover）：毒锁下不写
        // 共享镜像会造成"磁盘已保存新绑定、监听线程仍用旧表"的持久化-运行时
        // 背离——恢复毒锁继续写入，保证两者始终一致（L6 回归）。
        let mut guard = crate::util::lock_write_or_recover(&self.fn_bindings, "fn bindings");
        *guard = self.config.fn_key_bindings.clone();
        drop(guard);
        self.save_state();
    }

    /// 修改某条绑定的动作（index 越界时告警忽略）。
    pub(crate) fn set_fn_binding_action(&mut self, index: usize, action: app::fnkey::FnAction) {
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

    /// 修改某条绑定为 `RunCommand` 时的自定义命令行。
    ///
    /// 仅在动作切换为 `RunCommand` 时展示/保存命令文本；其余动作保留已有
    /// 命令不动（避免用户在动作下拉间切换时误清空配置好的命令）。空/纯空白
    /// 命令仍可保存——监听线程遇 `RunCommand` + 空命令会跳过并告警
    /// （见 `app::fnkey::run_external_command`）。
    pub(crate) fn set_fn_binding_command(&mut self, index: usize, command: &str) {
        let Some(binding) = self.config.fn_key_bindings.get_mut(index) else {
            log::warn!("set_fn_binding_command: index {} out of range", index);
            return;
        };
        if binding.command.as_deref() == Some(command) {
            return;
        }
        log::info!(
            "Fn binding {} command -> {}",
            binding.label(),
            app::fnkey::redact_command(command)
        );
        binding.command = Some(command.to_string());
        self.commit_fn_bindings();
    }

    /// 按已知功能键目录添加绑定（GUI"添加绑定"下拉）。
    /// 相同 (class, prefix) 已存在时只更新动作，不重复添加。
    /// `command`：`RunCommand` 动作的命令行草稿（其余动作忽略，传 `""` 即可）；
    /// 仅在动作确实为 `RunCommand` 时写入（避免把用户之前输入的草稿误存进
    /// 其它动作的绑定）。
    pub(crate) fn add_fn_binding(
        &mut self,
        class: &str,
        prefix: &str,
        action: app::fnkey::FnAction,
        command: &str,
    ) -> bool {
        // 与 config.rs 消毒同一套规则（修订 1.32/M3）：前缀须至少一个完整
        // 字节、类名须为合法 WQL 标识符——单字节前缀与非法类名即使手改
        // 配置也会被丢弃，GUI 侧保持一致避免"能加进去但下次加载被删"。
        // 类名先 trim 再校验/存储：`valid_class` 按 trim 后校验，存储未
        // trim 的类名会"校验通过但永不匹配订阅类"（修订 1.47 审计，与
        // config.rs::sanitize 的规范化同源）。
        let class = class.trim();
        if !app::fnkey::valid_class(class) {
            log::warn!("add_fn_binding: invalid class ignored: {:?}", class);
            return false;
        }
        let prefix = app::fnkey::normalize_hex(prefix);
        if !app::fnkey::valid_prefix(&prefix) {
            log::warn!("add_fn_binding: invalid prefix ignored: {:?}", prefix);
            return false;
        }
        // 只有 RunCommand 动作会携带命令文本；其余动作恒为 None。
        let command = if action == app::fnkey::FnAction::RunCommand {
            Some(command.to_string())
        } else {
            None
        };
        let existing = self
            .config
            .fn_key_bindings
            .iter_mut()
            .find(|b| b.class == class && b.prefix == prefix);
        if let Some(b) = existing {
            log::info!(
                "Fn:: binding {}/{} already exists; setting action {}",
                class,
                app::fnkey::FnKeyBinding::display_prefix(&prefix),
                action.name()
            );
            b.action = action;
            b.command = command;
        } else {
            log::info!(
                "Fn:: add binding {} / {} -> {}",
                class,
                app::fnkey::FnKeyBinding::display_prefix(&prefix),
                action.name()
            );
            self.config.fn_key_bindings.push(app::fnkey::FnKeyBinding {
                class: class.to_string(),
                prefix,
                action,
                command,
            });
        }
        self.commit_fn_bindings();
        true
    }

    /// 删除某条绑定。允许清空列表：清空后 Fn 监听线程按"无绑定"空转
    /// （见 fnkey.rs 的 no-op 逻辑），不强制保留至少一条。
    pub(crate) fn remove_fn_binding(&mut self, index: usize) {
        let Some(binding) = self.config.fn_key_bindings.get(index) else {
            log::warn!("remove_fn_binding: index {} out of range", index);
            return;
        };
        log::info!("Fn:: remove binding {}", binding.label());
        self.config.fn_key_bindings.remove(index);
        self.commit_fn_bindings();
    }

    /// 捕获模式下，**释放事件不得覆盖同键码的按下事件**（修订 1.31 回归修复）。
    ///
    /// 固件对一次物理按键先后发送按下与释放两条事件（如 `012801` / `012800`，
    /// 见 F-FNK-06 的 ReportHex 语义）。捕获窗口把两者都转发给 GUI，而释放
    /// 总是后到——若直接覆盖 `last_fn_event`，"最近捕获"显示释放码、"使用
    /// 此键"绑定释放前缀，下一次物理按键（按下码）不再命中，绑定静默失效。
    ///
    /// 判定规则：新事件是**释放**且已存事件是同一键码的**按下**时，保留旧的
    /// 按下事件。按下/释放判定与 Fn 监听派发共用 `app::fnkey` 的
    /// `is_press_report` / `is_release_report`（同一条"状态字节位于第 3 字节"
    /// 的硬件规则，历史两处各自实现有漂移风险）。同键码判定：事件类相同、
    /// 去掉状态字节后的 hex 相同（`012800` 与 `012801` 经
    /// `key_without_status_byte` 均得 `0128`）——统一收敛到该 helper（状态
    /// 字节区域是 `[4..6]`，历史实现"去掉末字节"仅在报告恰为 3 字节时等价，
    /// 报告更长时会判错，见 fnkey.rs 注释）；
    /// `None`/类不同/非释放时直接更新。
    fn keep_press_over_release(prev: Option<&(String, String)>, class: &str, hex: &str) -> bool {
        // 仅当新事件是释放（状态字节为 `00`）时考虑保留旧按下事件。
        if !crate::app::fnkey::is_release_report(hex) {
            return false;
        }
        let Some((prev_class, prev_hex)) = prev else {
            return false;
        };
        prev_class == class
        && crate::app::fnkey::is_press_report(prev_hex)
        // 同键码：去掉状态字节后完全相同。
        && crate::app::fnkey::key_without_status_byte(hex)
            == crate::app::fnkey::key_without_status_byte(prev_hex)
    }
}

impl XiaomiApp {
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
            app::battery::care_enabled_from_limit(applied),
            applied
        );
        self.runtime.charge_limit = applied;
        // 养护关闭（applied == 100）时保留 config 中用户期望的上限供重新开启
        // 时恢复，开启时回写硬件实际生效值——统一收敛在
        // battery::sync_config_after_apply。
        crate::app::battery::sync_config_after_apply(&mut self.config, applied);
        self.runtime.battery_care_enabled = app::battery::care_enabled_from_limit(applied);
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
        let outcome = app::battery::apply_battery_state(&*self.backend, enabled, desired_limit);
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
                    self.push_error(ERR_SET_CARE.to_string());
                }
                self.save_state();
            }
            // 限值写入失败时不得更新状态：硬件未变更，UI 与配置保持一致，
            // 错误在 GUI 中展示（F-ERR-03）。
            Err(e) => {
                log::error!("Set battery care: charge limit write failed: {}", e);
                let mut errs = Vec::new();
                // 与 set_charge_limit_internal 的失败文案统一（附带错误详情，
                // 修订 1.46 审计：历史在此只写"设置充电上限失败"，排查时
                // 看不到具体原因）。
                errs.push(err_fmt(ERR_SET_CHARGE_LIMIT, e));
                if outcome.care.is_err() {
                    errs.push(ERR_SET_CARE.to_string());
                }
                self.push_error(errs.join("; "));
            }
        }
    }

    pub fn set_charge_limit_internal(&mut self, limit: u8) {
        let limit = limit.min(FULL_CHARGE_LIMIT);
        // 养护位由限值推导：<100% 即养护开启，100% 即关闭。统一的
        // apply_battery_state 会写限值 → 写养护位 → 读回实际生效值
        // （WMI 量化到最近预设，如 85→80，见 AC-BAT-04）。
        let outcome = app::battery::apply_battery_state(
            &*self.backend,
            app::battery::care_enabled_from_limit(limit),
            limit,
        );
        let applied = match outcome.charge_limit {
            Ok(applied) => applied,
            Err(e) => {
                log::error!("Failed to set charge limit: {}", e);
                self.push_error(err_fmt(ERR_SET_CHARGE_LIMIT, e));
                return;
            }
        };
        // 提交前先判定养护位是否发生翻转：commit_battery_write_state 会把
        // runtime.battery_care_enabled 同步为 applied<100，下方的联动日志需要
        // 依据翻转前的状态差异。
        let care_changed =
            app::battery::care_enabled_from_limit(applied) != self.runtime.battery_care_enabled;
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
                self.push_error(err_fmt(ERR_SYNC_CARE, e));
            } else {
                log::info!(
                    "Battery care {} (synced from charge limit)",
                    if app::battery::care_enabled_from_limit(applied) {
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
        // 降级规则统一收敛在 app::battery::effective_perf_for_power
        // （电源状态未知时不静默降级，按用户选择写入并告警）。
        // 电源状态经 platform::power 查询（PowerSource 端口实现）。
        let status = crate::platform::power::power_status();
        let raw = app::battery::effective_perf_for_power(mode.ec_value(), status);
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
                self.push_error(err_fmt("设置性能模式失败", e));
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
        //
        // "请求的偏好已经生效"的判定：Auto 偏好的目标是 WMI（Auto 探测结果
        // 必然还是 WMI，重建毫无意义），其余偏好即目标后端本身。统一成一个
        // 谓词，避免两条近似分支各自维护漂移。
        let already_active = match pref {
            BackendPreference::Auto => self.backend.preference() == BackendPreference::Wmi,
            _ => self.backend.preference() == pref,
        };
        if already_active && !self.backend.needs_rebuild() {
            log::info!(
                "Backend '{}' already matches preference {:?}; no re-init needed",
                self.backend.name(),
                pref
            );
            // 若存在**进行中**的切换（修订 1.46 审计）：该分支在单飞守卫之前
            // 执行，会清空 pending——正在创建的后端结果到达时被判为 stale 而
            // drop（其 Drop 会卸载刚加载的 WinRing0 驱动，属预期的"加载后未
            // 使用即释放"），对用户无副作用；但记录日志便于确认并非逻辑丢失。
            if self.pending_backend_switch.is_some() {
                log::info!(
                    "Backend switch already in-flight (pending {:?}); cancelling it for already-active '{}'",
                    self.pending_backend_switch,
                    self.backend.name()
                );
            }
            self.pending_backend_switch = None;
            self.confirm_preference(pref);
            return true;
        }

        // 单飞（single-flight，修订 1.39 回归修复）：切换创建**进行中**时拒绝
        // 新的切换请求。两个并发的 `create_backend(WinRing0)` 会各自加载同一
        // 份驱动：先到的结果应用、后到的被丢弃，而 `WinRing0Backend::Drop`
        // 无条件执行 `DeinitializeOls` 卸载驱动——正在使用的后端的驱动被拆掉，
        // 端口读写全部失效（只能重启恢复，与 winring0.rs 已知故障类一致）。
        // 创建通常 <1s（WMI 最坏 10s），期间忽略重复请求可接受。WMI 后端的
        // 并发丢弃虽无害，但统一单飞保持行为确定。
        if self.pending_backend_switch.is_some() {
            log::warn!(
                "Backend switch already in progress; ignoring request to {:?}",
                pref
            );
            return false;
        }

        // 其余路径都需要创建（可能阻塞）的后端，放**后台线程**执行（修订
        // 1.36）：`create_backend(Wmi)` 的握手上限 ~10s（含 4 次 ×2s 重试），
        // 放 GUI 线程会让窗口冻结、托盘/Fn/电源命令积压（与 wmi_recover 的
        // 延迟恢复同模式，此前手动切换路径遗漏了同样的处理）。
        //
        // Auto 且当前是 WinRing0 时只**探测 WMI**：可用则切、不可用保留现有
        // WinRing0（绝不重建——重建会先卸载活驱动再依赖它，只能重启恢复，
        // 见 winring0.rs 注释）。
        let target = if pref == BackendPreference::Auto
            && self.backend.preference() == BackendPreference::WinRing0
        {
            BackendPreference::Wmi
        } else {
            pref
        };
        self.pending_backend_switch = Some(pref);
        // 手动切换是明确的用户意图：复位"运行期熔断自动重建"单次上限，
        // 使下一次熔断仍可自动恢复（修订 1.45 审计）。
        self.wmi_auto_rebuild_attempted = false;
        let cmd_tx = self.cmd_tx.clone();
        match std::thread::Builder::new()
            .name("backend-switch".to_string())
            .spawn(move || {
                // catch_unwind（修订 1.45 审计）：create_backend 走 COM/驱动
                // FFI 边界，panic 时若不捕获，BackendSwitchResult 永不回传，
                // pending_backend_switch 永久卡住（单飞守卫拒绝后续一切切换，
                // 后端切换 UI 会话内死锁）。panic 按失败回传，消费路径清空
                // pending 并展示错误（与其它后台线程的 catch_unwind 规格一致）。
                let result: Result<Box<dyn ec::backend::EcBackend>, String> =
                    crate::util::catch_panic(|| {
                        ec::backend::create_backend(target).map_err(|e| e.to_string())
                    })
                    .unwrap_or_else(|payload| Err(err_fmt("后端创建线程异常", payload)));
                if cmd_tx
                    .send(UiCommand::BackendSwitchResult {
                        user_pref: pref,
                        result,
                    })
                    .is_err()
                {
                    log::warn!("backend switch: GUI channel closed; result dropped");
                }
            }) {
            Ok(_) => true,
            Err(e) => {
                log::error!("Failed to spawn backend switch thread: {}", e);
                self.pending_backend_switch = None;
                self.push_error(err_fmt(ERR_BACKEND_SWITCH, e));
                false
            }
        }
    }

    /// 后台切换线程结果的消费（修订 1.36 异步化）：校验偏好未变后应用或
    /// 按 Auto 语义保留当前后端。语义与历史同步实现完全一致，只是
    /// `create_backend` 移到了后台线程，GUI 不再被 WMI 握手冻结。
    fn handle_backend_switch_result(
        &mut self,
        user_pref: BackendPreference,
        result: Result<Box<dyn ec::backend::EcBackend>, String>,
    ) {
        // 过期结果：发起后用户改选/确认了其它后端（pending 已变或清空）→
        // 丢弃。新建但未应用的后端经 Box drop 释放——WinRing0 后端 Drop 会
        // 卸载自己加载的驱动实例，无其它使用者，安全。
        if self.pending_backend_switch != Some(user_pref) {
            log::info!(
                "Backend switch result stale (pending={:?}, delivered={:?}); dropped",
                self.pending_backend_switch,
                Some(user_pref)
            );
            return;
        }
        self.pending_backend_switch = None;
        match result {
            Ok(backend) => {
                // user_pref 保持用户选择（Auto 场景 config 记录 Auto），
                // 实际后端由 create_backend 决定。
                self.apply_backend_switch(backend, user_pref);
            }
            Err(e) => {
                // Auto + 当前 WinRing0：探测 WMI 失败 → 保留现有后端
                //（不得重建，语义与注释一致）。其余失败如实展示。
                if user_pref == BackendPreference::Auto
                    && self.backend.preference() == BackendPreference::WinRing0
                {
                    log::info!(
                        "Auto: WMI unavailable ({}); keeping active WinRing0 backend",
                        e
                    );
                    self.confirm_preference(BackendPreference::Auto);
                } else {
                    log::error!("Failed to switch EC backend: {}", e);
                    self.push_error(err_fmt(ERR_BACKEND_SWITCH, e));
                }
            }
        }
    }

    /// 记录新的后端偏好：同步两个视图（`current_pref` 显示值 + `config.backend`
    /// 持久化值）并落盘。二者是同一值的两个事实来源，任何切换路径都必须
    /// 经此修改——直接各自赋值曾在 confirm_preference / apply_backend_switch
    /// 重复三次，新增路径若只改其一会让"GUI 显示 A、状态栏运行 B"的矛盾
    /// 重新出现（修订 1.47 收敛）。
    fn set_preference(&mut self, pref: BackendPreference) {
        self.current_pref = pref;
        self.config.backend = pref;
        self.save_state();
    }

    /// 确认不重建后端的偏好切换：更新显示偏好与持久化配置并保存。
    /// 仅用于"目标后端无需重建"（同种后端 no-op / Auto 语义下的保留）路径。
    fn confirm_preference(&mut self, pref: BackendPreference) {
        self.set_preference(pref);
    }

    /// 完成后端切换的公共逻辑（create_backend 之外的部分），单独抽出便于测试。
    /// 注意：不得在 refresh_from_backend() 之后清空 error_msg —— 刷新产生的
    /// 读取失败必须保留并在 GUI 中展示（F-ERR-03）；刷新成功时它自会清空。
    fn apply_backend_switch(
        &mut self,
        new_backend: Box<dyn ec::backend::EcBackend>,
        pref: BackendPreference,
    ) {
        log::info!("Switched EC backend to: {}", new_backend.name());
        self.backend = new_backend;
        // 显示偏好与持久化偏好统一经 set_preference 同步（修订 1.47 收敛，
        // 历史在此各自赋两遍）。
        self.set_preference(pref);
        self.refresh_from_backend();
    }

    pub fn refresh_from_backend(&mut self) {
        let mut errors: Vec<String> = Vec::new();
        match self.backend.get_performance_mode() {
            Ok(mode) => {
                self.runtime.performance_mode = mode;
            }
            Err(e) => {
                log::error!("Backend refresh: {}", e);
                errors.push(err_fmt("读取性能模式", e));
            }
        }
        match self.backend.get_battery_state() {
            Ok((_care, limit)) => {
                // 钳制到 [0,100]：损坏的 EC 读值（如 0xFF=255）不得显示为
                // "充电上限: 255%" 或使滑块/养护位推导溢出。上限超过 100
                // 视为垃圾值钳到 100。
                let limit = limit.min(FULL_CHARGE_LIMIT);
                // `0` 同样是垃圾值（真实后端统一拒绝，见 winring0/mock 的
                // 读回契约）：钳制只处理 >100，0 会以"充电上限: 0%"漏进 UI，
                // 且与"养护=限值<100%"推导出 care=true 的矛盾组合。这里补上
                // 最后一道防线（修订 1.50）：0 按读取失败处理。
                if limit == 0 {
                    let e = "充电上限寄存器值 0 非法".to_string();
                    log::error!("Backend refresh: {}", e);
                    errors.push(err_fmt("读取电池状态", e));
                } else {
                    self.runtime.charge_limit = limit;
                    // 领域不变式：养护 == 上限 < 100%（care_enabled_from_limit）。
                    // 读回的 care 位与 limit 冲突时（垃圾值场景下存在），以
                    // limit 为权威重新推导——否则"养护: 开启 · 上限: 100%"的
                    // 矛盾组合会展示给用户（M5 回归修复：历史实现把 care 原样
                    // 存进 runtime，钳制后的 limit=100 与 care=true 并存）。
                    self.runtime.battery_care_enabled =
                        app::battery::care_enabled_from_limit(limit);
                }
            }
            Err(e) => {
                log::error!("Backend refresh: {}", e);
                errors.push(err_fmt("读取电池状态", e));
            }
        }
        // NFR-REL-03：EC 读写连续失败计数。任意读取成功即清零（硬件恢复）；
        // 连续失败达到阈值后暂停自动重试并提示用户（错误已展示，见下方
        // error_msg 分支）。历史实现无限重试且无任何"连续失败"概念——
        // 驱动失效/EC 掉线等持续故障下，用户只看到反复刷新的相同错误。
        if errors.is_empty() {
            self.consecutive_read_failures = 0;
        } else {
            self.consecutive_read_failures = self.consecutive_read_failures.saturating_add(1);
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
            // 连续失败达阈值：把"已暂停自动重试"的持久提示并入错误展示，
            // 让用户区分"偶发一次失败"与"后端已不可用"（NFR-REL-03）。
            if self.consecutive_read_failures >= HW_FAILURE_PAUSE_THRESHOLD {
                log::warn!(
                    "Hardware read failed {} consecutive times",
                    self.consecutive_read_failures
                );
                // 措辞修正（修订 1.33）：历史文案写"已暂停自动重试"，但刷新
                // 只由用户/启动/电源事件触发，没有周期性的自动重试循环——
                // "暂停"是误导。改为如实描述"连续失败 N 次"，并保留同样的
                // 排查指引（检查驱动或切换后端）。
                errors.push(format!(
                    "硬件连续读取失败 {} 次；请检查驱动或切换后端",
                    self.consecutive_read_failures
                ));
            }
            self.error_msg = Some(errors.join("; "));
        }
    }

    pub(crate) fn save_state(&self) {
        if let Err(e) = self.store.save(&self.config) {
            log::error!("save config: {}", e);
        }
    }

    /// 获取托盘共享状态的写锁（毒锁恢复，与托盘侧一致的约定，见 util.rs）。
    ///
    /// 三处写入口（BatteryHealthUpdated / BatteryEtaUpdated / sync_tray_status）
    /// 曾各自重复 `lock_or_recover(&self.tray_status, "tray status")`（修订
    /// 1.46 审计去重）。
    fn with_tray_status(&self) -> std::sync::MutexGuard<'_, crate::tray::TrayStatus> {
        crate::util::lock_or_recover(&self.tray_status, "tray status")
    }

    /// 把当前运行时状态写入托盘共享状态（tooltip/菜单展示）。
    ///
    /// 所有改变运行时状态的路径（刷新、切换养护/上限/性能模式、切换后端）
    /// 都应调用，使托盘悬停提示保持实时；未同步时托盘显示的仍是旧状态。
    pub(crate) fn sync_tray_status(&self) {
        // 毒锁恢复与托盘侧一致（util.rs 约定）：静默跳过会让 tooltip 长期
        // 停留在旧状态且无日志，恢复毒锁更可排查。字段赋值收敛在
        // TrayStatus::sync_runtime（修订 1.49 整理）。
        let mut guard = self.with_tray_status();
        guard.sync_runtime(
            self.runtime.battery_care_enabled,
            self.runtime.charge_limit,
            self.runtime.performance_mode,
            self.config.notify_on_charge_limit,
        );
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
        // Command::arg 接受 AsRef<OsStr>：直接传 PathBuf，不经 to_string_lossy
        //（Windows 路径可能是非 UTF-8 的 UTF-16 序列，lossy 会破坏路径——
        // 修订 1.46 与 privilege.rs 的 from_os_str 同源加固）。
        match std::process::Command::new("explorer.exe")
            .arg("/select,")
            .arg(&path)
            .spawn()
        {
            Ok(_) => log::info!("Opening log file in Explorer: {}", path.display()),
            Err(e) => {
                log::error!("Failed to open log file in Explorer: {}", e);
                self.push_error(err_fmt("打开日志失败", e));
            }
        }
    }
}

#[cfg(test)]
mod tests;
