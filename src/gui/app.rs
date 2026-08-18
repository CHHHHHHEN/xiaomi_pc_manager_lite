use eframe::egui;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};

use crate::app;
use crate::app::command::UiCommand;
use crate::app::config::{BackendPreference, ConfigStore};
use crate::app::fnkey::SharedBindings;
use crate::ec;
use crate::tray::{SharedTrayStatus, TrayStatus};

use super::view;

/// WMI 延迟恢复探测参数（见 `XiaomiApp::wmi_recover_*`）。
///
/// 首次启动（尤其随登录自启动）时 WinMgmt 服务/MICommonInterface 提供程序
/// 可能尚未就绪，WMI 后端在启动握手预算（HANDSHAKE_TIMEOUT≈10s）内失败并
/// 回退 WinRing0——用户看到的"WMI 总是不可用，手动切换却可用"（F-BUG）。
/// 回退后按指数退避继续探测 WMI（20s→40s→80s→160s，最多 4 次），可用即
/// 自动切换回 WMI，无需用户手动操作。探测在后台线程执行，不阻塞 GUI。
const WMI_RECOVER_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(20);
const WMI_RECOVER_MAX_ATTEMPTS: u32 = 4;

/// GUI 运行时硬件状态（与持久化配置解耦的独立事实来源）。
///
/// 历史上这三个字段直接挂在 `XiaomiApp` 上、与 `config` 的同名字段并列，
/// 同一层出现两组同名状态：`battery_care_enabled` 既是"硬件实际状态"又是
/// "配置期望值"，读取/更新极易用错源。收敛到独立结构体后，`runtime.*` 表示
/// **硬件/界面当前认知**，`config.*` 表示**持久化的用户期望**——二者仅在
/// auto_apply 关闭且硬件被外部改动时不同（见 `refresh_from_backend` 的注释）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct RuntimeState {
    /// 电池养护当前状态（界面勾选框与状态栏显示）。
    pub battery_care_enabled: bool,
    /// 充电上限当前值（界面滑块/预设档位显示）。
    pub charge_limit: u8,
    /// 性能模式当前值（界面高亮显示；狂暴在电池供电时写入会降级，见
    /// `app::battery::effective_perf_for_power`）。
    pub performance_mode: u8,
}

impl RuntimeState {
    /// 以持久化配置初始化运行时状态（随后会被 `refresh_from_backend` 用硬件
    /// 实际状态覆盖）。
    fn from_config(config: &app::config::AppConfig) -> Self {
        Self {
            battery_care_enabled: config.battery_care_enabled,
            charge_limit: config.battery_charge_limit,
            performance_mode: config.performance_mode,
        }
    }
}

pub struct XiaomiApp {
    /// 配置读写入口（路径在 main 启动时解析一次，见 ConfigStore）。
    pub(crate) store: ConfigStore,
    pub cmd_tx: mpsc::Sender<UiCommand>,
    pub(crate) cmd_rx: mpsc::Receiver<UiCommand>,
    pub(crate) backend: Box<dyn ec::backend::EcBackend>,
    pub(crate) config: app::config::AppConfig,
    pub(crate) current_pref: BackendPreference,
    /// 运行时状态（硬件实际/界面当前显示），与 `config`（持久化用户期望）
    /// 分离，见 `RuntimeState` 的注释。
    pub(crate) runtime: RuntimeState,
    pub(crate) error_msg: Option<String>,
    /// `--autostart` 启动（F-AUTO-07）：首帧隐藏驻留托盘，不打扰用户。
    pub(crate) start_minimized: bool,
    /// start_minimized 等待窗口可见的已尝试帧数：窗口创建晚于首帧时用
    /// 立即重绘快速等待；超过上限后按 500ms 低频重试（见 update()），
    /// 避免"窗口始终未可见"时陷入 60fps 忙循环（NFR-PERF-01）。
    autostart_wait_frames: u32,
    /// start_minimized 开始等待的时间点：低频重试窗口有时间上限（30s），
    /// 超过后放弃隐藏（此时窗口仍不可见，属于异常场景）。
    autostart_start: std::time::Instant,
    /// 标题栏应用图标纹理（首帧由 icon.png 创建）。
    pub(crate) icon_tex: Option<egui::TextureHandle>,
    /// 开机自启动操作的**串行** worker 发送端（首次 set_autostart 惰性创建，
    /// 见 commands.rs 的说明）：所有请求按到达顺序执行、结果按相同顺序回传。
    /// 挂在本实例上而非进程级 static——发送端随实例 drop 而关闭，worker 线程
    /// 自然退出，不残留跨实例的全局通道。
    pub(crate) autostart_worker: Option<std::sync::mpsc::Sender<bool>>,
    /// 尚未收到结果的 autostart 请求数（修订 1.44）：串行 worker 按请求顺序
    /// 回传结果，但 GUI 侧"当前配置"会在每次请求时即时落盘（M3 修复），
    /// 无法区分"这个失败属于最新请求"还是"旧请求的迟到失败"。只有本计数
    /// 归零时收到的结果才对应**最新**请求——此时 enable 失败才允许回滚
    /// （否则会把更新的 ON 意图误回滚为 OFF，制造"任务在而配置关"背离）。
    pub(crate) autostart_in_flight: u32,
    /// 与 Fn 监听线程共享的绑定表（GUI 保存配置时同步更新，即时生效）。
    pub(crate) fn_bindings: SharedBindings,
    /// Fn 捕获模式开关（与监听线程共享）：开启后收到的每条功能键事件都以
    /// `UiCommand::FnEventSeen` 回传 GUI 展示，便于用户观察真实键码。
    pub(crate) fn_capture: Arc<AtomicBool>,
    /// 捕获模式下最近一次收到的功能键事件 (事件类, 归一化 hex)。
    pub(crate) last_fn_event: Option<(String, String)>,
    /// "Fn 功能键 → 添加绑定"中预设键码下拉的当前选中下标（跨帧保持：
    /// egui UI 每帧重建，局部变量会被重置回默认，见 view.rs 注释）。
    pub(crate) fn_add_preset_index: usize,
    /// "Fn 功能键 → 添加绑定"中动作下拉的当前选中动作（同上，跨帧保持）。
    pub(crate) fn_add_action: app::fnkey::FnAction,
    /// "Fn 功能键 → 添加绑定"中 RunCommand 动作的命令草稿（跨帧保持，
    /// 添加成功后清空）。
    pub(crate) fn_add_command: String,
    /// "Fn 捕获 → 绑定为"中动作下拉的当前选中动作（跨帧保持。egui UI
    /// 每帧重建，局部变量会被重置回默认——若用局部变量，用户选中的动作
    /// 在下一帧即丢失，点击"使用此键"时恒绑定默认动作，见 view.rs）。
    pub(crate) fn_capture_action: app::fnkey::FnAction,
    /// "Fn 捕获 → 绑定为"中 RunCommand 动作的命令草稿（跨帧保持，
    /// 捕获场景无法从预设键码带出命令，需在此直接输入，见 view.rs）。
    pub(crate) fn_capture_command: String,
    /// 托盘 tooltip/菜单共享的运行时状态（GUI 写入，托盘线程周期读取）。
    pub(crate) tray_status: SharedTrayStatus,
    /// 退出标志：托盘"退出"命令（UiCommand::Quit）置位后，下一帧的
    /// close_requested 不再被取消/隐藏，而是放行让 eframe 真正退出事件
    /// 循环（保证 Drop 清理执行）。用户点击窗口关闭按钮时不置位，仍走
    /// "隐藏到托盘"路径（修订 1.21）。
    pub(crate) quitting: bool,
    /// WMI 延迟恢复：下一次探测 WMI 后端可用性的时间点。
    /// `None` = 无需探测（当前已是 WMI / 用户偏好非 WMI / 已达预算上限）。
    pub(crate) wmi_recover_at: Option<std::time::Instant>,
    /// 已发起的 WMI 延迟恢复探测次数（指数退避与上限用，见 WMI_RECOVER_*）。
    pub(crate) wmi_recover_attempts: u32,
    /// 是否已为**运行期熔断**的 WMI 后端自动发起过一次重建（修订 1.45 审计）：
    /// 熔断（needs_rebuild）的 WMI 此前只能靠用户手动点击后端单选重建。现在
    /// 检测到熔断后自动武装一次探测循环；单次上限避免"永久损坏的 WMI 每次
    /// 重建后再次熔断 → 无限探测循环"。用户手动切换后端时复位（下个熔断可
    /// 再次自动恢复）。
    pub(crate) wmi_auto_rebuild_attempted: bool,
    /// 连续硬件**读取**失败计数（NFR-REL-03）：`refresh_from_backend` 的
    /// 读取失败连续累计，达到阈值后 GUI 展示持久提示（"连续读取失败 N 次"）；
    /// 任意一次成功读取清零计数并移除提示。写入失败不参与计数（写入路径
    /// 各自即时展示错误），字段名与行为一致（修订 1.47 清理）。
    pub(crate) consecutive_read_failures: u32,
    /// 充电上限滑块的**拖动中工作值**（F-PWR-04 回归，修订 1.33）：拖动期间
    /// 持久到 self，避免电源切换触发的 `refresh_from_backend` 改写 runtime
    /// 后滑块被"拽回"（egui 每帧从 runtime 重新初始化 limit）。None =
    /// 未在拖动，直接使用 runtime.charge_limit。
    pub(crate) charge_limit_drag: Option<u8>,
    /// 电池健康读数（`platform::battery_health` 后台线程上报）。`None` =
    /// 尚未读到（WMI 未就绪/本机无电池/类不可用），界面不展示该行。
    pub(crate) battery_health: Option<crate::platform::battery_health::BatteryHealth>,
    /// 预计剩余/充满时长文案（`platform::battery_health` 上报的充放电速率
    /// 估算，修订 1.37）：如 "预计剩余约 2 小时 30 分钟" /
    /// "预计充满约 1 小时 20 分钟"（`format_minutes` 中文化，修订 1.39）。
    /// `None` = 速率不可用（满电停充/速率 0/无电池），不展示该行。
    pub(crate) battery_eta_text: Option<String>,
    /// 手动后端切换的进行中标记（修订 1.36 异步化）：`try_switch_backend`
    /// 发起后台创建时记录目标偏好，`BackendSwitchResult` 到达时若与当前
    /// pending 不一致则丢弃（用户已改选其它后端/已确认，旧结果过期）。
    /// `None` = 无进行中的切换。
    pub(crate) pending_backend_switch: Option<crate::app::config::BackendPreference>,
}

impl XiaomiApp {
    pub fn new(
        store: ConfigStore,
        backend: Box<dyn ec::backend::EcBackend>,
        config: app::config::AppConfig,
        pref: BackendPreference,
        init_error: Option<String>,
        start_minimized: bool,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        // 先借 config 初始化运行时快照，再整体移入结构体（config 非 Copy）。
        let runtime = RuntimeState::from_config(&config);
        // Fn 绑定共享状态：以持久化配置初始化（config 稍后整体移入结构体，
        // 绑定表在此先行 clone）。GUI 修改时同步写入并保存配置（见
        // commands.rs 的 fn 绑定处理器）。
        let fn_bindings =
            std::sync::Arc::new(std::sync::RwLock::new(config.fn_key_bindings.clone()));
        // 托盘共享状态：以持久化配置初始化（随后被 refresh_from_backend
        // 用硬件实际状态覆盖），与托盘线程共享。
        let tray_status: SharedTrayStatus = Arc::new(std::sync::Mutex::new(TrayStatus {
            battery_care_enabled: config.battery_care_enabled,
            charge_limit: config.battery_charge_limit,
            performance_mode: config.performance_mode,
            battery_health_percent: None,
            battery_eta_text: None,
            notify_on_charge_limit: config.notify_on_charge_limit,
        }));

        let mut app = Self {
            store,
            cmd_tx,
            cmd_rx,
            backend,
            config,
            current_pref: pref,
            runtime,
            error_msg: None,
            start_minimized,
            autostart_wait_frames: 0,
            autostart_start: std::time::Instant::now(),
            icon_tex: None,
            autostart_worker: None,
            autostart_in_flight: 0,
            fn_bindings,
            fn_capture: Arc::new(AtomicBool::new(false)),
            last_fn_event: None,
            fn_add_preset_index: 0,
            fn_add_action: app::fnkey::FnAction::CyclePerfMode,
            fn_add_command: String::new(),
            fn_capture_action: app::fnkey::FnAction::CyclePerfMode,
            fn_capture_command: String::new(),
            charge_limit_drag: None,
            tray_status,
            quitting: false,
            wmi_recover_at: None,
            wmi_recover_attempts: 0,
            wmi_auto_rebuild_attempted: false,
            consecutive_read_failures: 0,
            battery_health: None,
            battery_eta_text: None,
            pending_backend_switch: None,
        };

        // WMI 延迟恢复探测初始化（首次启动 WMI 失败回退时启用，见
        // arm_wmi_recovery 注释）。
        app.arm_wmi_recovery();

        // AC-START-03: GUI 启动后应显示硬件当前实际状态，而非仅显示
        // 持久化的配置（auto_apply 关闭时两者可能不一致）。
        // WMI 后端为线程亲和 worker 代理，任意线程调用均安全。
        app.refresh_from_backend();
        // GUI 初始状态的快照（info 级别）：启动后界面认为硬件处于什么状态。
        // 后续任何"界面显示与预期不符"的问题，都能与这一行基线对比——历史
        // 上这条信息只在 refresh_from_backend 的 debug 日志里，默认日志级别
        // 看不到，问题发生时无从确认 GUI 的初始认知。
        log::info!(
            "GUI initial state: backend={}, perf={:#x}, care={}, limit={}%",
            app.backend.name(),
            app.runtime.performance_mode,
            app.runtime.battery_care_enabled,
            app.runtime.charge_limit
        );
        if let Some(init) = init_error {
            // 复用 push_error 的统一合并（F-ERR-03）：历史实现在此手工拼接
            // "init; reads"，与 commands.rs 的 push_error 各维护一份合并逻辑，
            // 存在漂移。合并顺序与展示可读性无关（两条信息都会完整展示）。
            app.push_error(init);
        }

        app
    }

    /// 用户当前**期望** WMI 生效（偏好 Auto/Wmi）。
    ///
    /// WMI 延迟恢复的启用/停止判定（arm_wmi_recovery、maybe_probe_wmi_recovery、
    /// process_commands 的 WmiAvailable 分支）曾各自书写同一份
    /// `matches!(backend, Auto | Wmi)`，统一收敛到此处。
    pub(crate) fn wants_wmi(&self) -> bool {
        matches!(
            self.config.backend,
            BackendPreference::Auto | BackendPreference::Wmi
        )
    }

    /// 当前实际后端是否为**健康**的 WMI（未熔断）。
    ///
    /// 与 `wants_wmi` 配套的"是否还需探测"判定：已健康的 WMI 无需探测。
    pub(crate) fn wmi_active_and_healthy(&self) -> bool {
        self.backend.preference() == BackendPreference::Wmi && !self.backend.needs_rebuild()
    }

    /// 初始化/复位 WMI 延迟恢复探测计划（F-BUG：启动时 WMI 服务未就绪；
    /// 修订 1.45：运行期 WMI 熔断也可自动重建）。
    ///
    /// 启用条件（满足其一）：
    /// - 用户偏好仍是 WMI（Auto/Wmi）且当前实际后端**不是** WMI（启动回退场景）；
    /// - 当前后端是 WMI 但已熔断（needs_rebuild，运行期死 worker/超时熔断场景）。
    ///
    /// 其余情况（用户偏好非 WMI、或 WMI 健康）停止探测。首个探测点在
    /// `WMI_RECOVER_INITIAL_DELAY` 之后。
    ///
    /// 幂等：已武装（`wmi_recover_at` 非 None）时不重置计时——每帧调用的
    /// 探测检查与运行期熔断检测都依赖"不反复推迟首探"的语义。
    fn arm_wmi_recovery(&mut self) {
        let wants_wmi = self.wants_wmi();
        let wmi_healthy = self.wmi_active_and_healthy();
        if !wants_wmi || wmi_healthy {
            self.wmi_recover_at = None;
            return;
        }
        if self.wmi_recover_at.is_some() {
            return;
        }
        // 新一次武装 = 新一轮探测计划：复位已消耗的探测预算（修订 1.47
        // 审计）。历史实现只在本函数幂等前提下递增 `wmi_recover_attempts`，
        // 首轮 4 次全部失败后该计数停留在 4——之后运行期再熔断重新武装时
        // `next_probe_after(4)` 直接返回 None，新一轮只有 1 次探测机会
        // （且这次失败即永久放弃），预算从未恢复。这里在新循环起点清零。
        self.wmi_recover_attempts = 0;
        self.wmi_recover_at = Some(std::time::Instant::now() + WMI_RECOVER_INITIAL_DELAY);
        log::info!(
            "WMI delayed recovery armed (backend '{}'); first probe in {}s",
            self.backend.name(),
            WMI_RECOVER_INITIAL_DELAY.as_secs()
        );
    }

    /// 到期则发起一次 WMI 延迟恢复探测（指数退避，见 WMI_RECOVER_* 常量）。
    ///
    /// 探测在**后台线程**执行（`create_backend(Wmi)` 会创建 wmi-worker 并
    /// 同步等待握手，放 GUI 线程会卡帧）；结果经 `UiCommand::WmiAvailable`
    /// 回传，由 process_commands 校验偏好后应用切换。
    fn maybe_probe_wmi_recovery(&mut self) {
        // 运行期熔断自动恢复（修订 1.45 审计）：当前后端是**已熔断**的 WMI
        // 时自动武装一次探测循环（此前只能手动点击后端单选重建）。熔断的
        // 判定权威是 backend.needs_rebuild()（wmi.rs 熔断置位后恒 true）。
        let wants_wmi = self.wants_wmi();
        if wants_wmi
            && self.backend.preference() == BackendPreference::Wmi
            && self.backend.needs_rebuild()
            && !self.wmi_auto_rebuild_attempted
        {
            self.wmi_auto_rebuild_attempted = true;
            log::warn!("WMI backend wedged; arming automatic rebuild probe");
            self.arm_wmi_recovery();
        }
        let Some(due) = self.wmi_recover_at else {
            return;
        };
        // 手动切换进行中（修订 1.47 审计）不发起探测：探测的
        // BackendSwitchResult/WmiAvailable 结果会因 pending 不匹配而被判 stale
        // drop（commands.rs 的 WmiAvailable 分支），白白消耗一次完整 WMI
        // 连接握手（最多 ~10s）。手动切换完成/失败后 pending 清空，下一帧
        // 探测自然恢复。
        //
        // 判定必须在递增探测预算**之前**：历史版本先 `wmi_recover_attempts += 1`
        // 并排布退避、最后才检查 pending——被跳过的探测照样消耗预算，且第 4 次
        // （最后一次）探测被跳过后 `next_probe_after` 返回 None 直接把
        // `wmi_recover_at` 置 None，WMI 延迟恢复被静默取消（本轮 4 次预算里
        // 一次真正的探测都没跑过就 give-up）。手动切换窗口（~10s WMI 握手）
        // 恰好覆盖一次探测期，必须不占预算。
        if self.pending_backend_switch.is_some() {
            log::debug!("WMI delayed recovery: manual switch in flight; skipping probe");
            return;
        }
        // 用户偏好改为非 WMI，或当前后端是健康的 WMI（含探测应用成功）：
        // 停止探测。
        let wmi_healthy = self.wmi_active_and_healthy();
        if !wants_wmi || wmi_healthy {
            self.wmi_recover_at = None;
            return;
        }
        if std::time::Instant::now() < due {
            return;
        }
        self.wmi_recover_attempts += 1;
        // 指数退避：首次探测后 20s→40s→80s→160s。最后一次尝试不再排布下一次
        // 探测（否则会留下一个永不触发的死槽，give-up 要等到那个死槽到期才
        // 记录）。
        match next_probe_after(self.wmi_recover_attempts) {
            Some(backoff) => {
                self.wmi_recover_at = Some(std::time::Instant::now() + backoff);
                log::info!(
                    "WMI delayed recovery: probe #{}/{} started (next in {:?})",
                    self.wmi_recover_attempts,
                    WMI_RECOVER_MAX_ATTEMPTS,
                    backoff
                );
            }
            None => {
                self.wmi_recover_at = None;
                log::warn!(
                    "WMI delayed recovery: probe #{}/{} started (last attempt; no more retries)",
                    self.wmi_recover_attempts,
                    WMI_RECOVER_MAX_ATTEMPTS
                );
            }
        }
        let cmd_tx = self.cmd_tx.clone();
        // 与各后台线程共用 util::spawn_guarded 兜底（修订 1.45 + 1.47 收敛）：
        // Builder 防 spawn 失败 panic 传播到 GUI update 线程杀死应用；
        // catch_unwind——create_backend 走 COM/驱动 FFI 边界，panic 时若
        // 不捕获会静默终止本线程且无日志。探测自愈不依赖本结果（下次探测
        // 已排布），失败只记录语义化日志。
        if let Err(e) = crate::util::spawn_guarded("wmi-recovery-probe", move || {
            match ec::backend::create_backend(BackendPreference::Wmi) {
                Ok(backend) => {
                    if cmd_tx.send(UiCommand::WmiAvailable(backend)).is_err() {
                        log::warn!(
                            "WMI delayed recovery: GUI channel closed; probed backend dropped"
                        );
                    }
                }
                Err(e) => {
                    log::warn!("WMI delayed recovery: probe failed: {}", e);
                }
            }
        }) {
            log::warn!("WMI delayed recovery: failed to spawn probe thread: {}", e);
        }
    }
}

/// 第 N 次探测后的下一次退避延时（纯函数，便于单元测试）。
///
/// 指数退避：第 1 次后 40s、第 2 次后 80s、第 3 次后 160s。达到
/// `WMI_RECOVER_MAX_ATTEMPTS`（最后一次探测）返回 None——不再排布下一次
/// 探测（历史上会留下一个永不触发的死槽，give-up 因此推迟一个退避周期）。
fn next_probe_after(attempts: u32) -> Option<std::time::Duration> {
    if attempts >= WMI_RECOVER_MAX_ATTEMPTS {
        return None;
    }
    Some(WMI_RECOVER_INITIAL_DELAY * (1u32 << attempts))
}

/// 探测预算是否已耗尽（`next_probe_after` 的布尔化，供测试断言"新一轮
/// 武装必须复位预算"）。
#[cfg(test)]
fn probes_exhausted(attempts: u32) -> bool {
    next_probe_after(attempts).is_none()
}

/// `app::sink::CommandSink` 的 GUI 实现：把命令投递到 eframe 命令通道，
/// 并请求 egui 立即重绘（唤醒隐藏的事件循环）。
///
/// 后台线程（托盘 / Fn 监听 / 电池健康 / WMI 恢复探测）只依赖 `CommandSink`
/// 端口，不再直接持有 `egui::Context`（领域/驱动层与 GUI 框架解耦）。
pub struct GuiCommandSink {
    tx: mpsc::Sender<UiCommand>,
    ctx: egui::Context,
}

impl crate::app::sink::CommandSink for GuiCommandSink {
    fn send(&self, command: UiCommand) -> Result<(), ()> {
        self.tx.send(command).map_err(|_| ())
    }
    fn wake(&self) {
        self.ctx.request_repaint();
    }
}

pub fn run_app(
    store: ConfigStore,
    backend: Box<dyn ec::backend::EcBackend>,
    config: app::config::AppConfig,
    pref: BackendPreference,
    init_error: Option<String>,
    start_minimized: bool,
) {
    let app = XiaomiApp::new(store, backend, config, pref, init_error, start_minimized);
    let cmd_tx = app.cmd_tx.clone();

    log::info!("GUI starting (--autostart: {})", start_minimized);
    let gui_start = std::time::Instant::now();

    let icon = view::load_icon_data();
    // 图标加载失败（嵌入资源损坏/解码失败）时窗口图标静默回退为默认：
    // 该失败不影响功能，但用户会看到"没有应用图标"，必须记录以便区分
    // 资源问题与逻辑问题。
    if icon.is_none() {
        log::warn!("Failed to load embedded app icon; falling back to default");
    }
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(crate::util::DEFAULT_WINDOW_SIZE)
            .with_min_inner_size(crate::util::MIN_WINDOW_SIZE)
            .with_decorations(false)
            .with_resizable(true)
            .with_icon(icon.unwrap_or_default()),
        ..Default::default()
    };

    if let Err(e) = eframe::run_native(
        // 标题必须与 platform::window::MAIN_WINDOW_TITLE 完全一致：
        // FindWindowW 按该标题定位主窗口，漂移会让托盘操作静默失效。
        crate::util::APP_NAME,
        native_options,
        Box::new(move |cc| {
            let mut fonts = egui::FontDefinitions::default();
            if let Some((name, data)) = view::load_cjk_font() {
                fonts
                    .font_data
                    .insert(name.clone(), egui::FontData::from_owned(data).into());
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

            // egui Context 经 CommandSink 传入托盘/Fn worker 线程：发送命令后
            // 唤醒隐藏的 GUI 事件循环（否则隐藏态下命令积压、窗口恢复才执行）。
            let ctx = cc.egui_ctx.clone();
            let app = app;
            let sink: Arc<dyn crate::app::sink::CommandSink> = Arc::new(GuiCommandSink {
                tx: cmd_tx.clone(),
                ctx: ctx.clone(),
            });

            // 托盘线程共享的运行时状态（tooltip/菜单实时展示）。
            crate::tray::spawn(sink.clone(), app.tray_status.clone());
            // Fn 监听线程与 GUI 共享绑定表与捕获开关（配置保存即即时生效）。
            crate::ec::fn_watcher::spawn(
                sink.clone(),
                app.fn_bindings.clone(),
                app.fn_capture.clone(),
            );
            // 电池健康监测线程（root\WMI 容量读数，与 EC 后端无关）。与托盘/
            // Fn 统一经 CommandSink 通信（发送后唤醒，隐藏态下即时上屏）。
            crate::platform::battery_health::spawn(sink);

            Ok(Box::new(app))
        }),
    ) {
        log::error!("GUI exited with error: {}", e);
    }
    // GUI 存活时长：从"GUI starting"到事件循环退出。托盘驻留（隐藏到托盘）
    // 的应用此数值即本次运行总时长，用于确认进程是否真的走完了退出流程。
    log::info!(
        "GUI event loop exited (ran for {} s)",
        gui_start.elapsed().as_secs()
    );
}

impl eframe::App for XiaomiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // F-AUTO-07: --autostart 启动时隐藏驻留托盘（任务栏不显示图标）。
        // 首帧窗口可能尚未创建完成（FindWindow 找不到），保持标记并重试。
        // 前 10 帧立即重绘（窗口即将出现，尽快隐藏、避免任务栏闪烁）；
        // 超过后窗口仍未可见按 500ms 低频重试（NFR-PERF-01）。
        // 注意：**不能**在首 10 帧后就把 start_minimized 置 false——若窗口
        // 因首帧字体编译 / GPU 初始化较慢而在首 10 帧之后才被创建，提前清除
        // 标记会让 --autostart 实例的窗口原样显示在桌面上，违背"驻留托盘
        // 不打扰用户"。低频重试需保持到窗口出现并隐藏，或超时（30s）放弃。
        if self.start_minimized {
            if crate::platform::window::main_window_visible() {
                self.start_minimized = false;
                crate::platform::window::hide_main_window();
            } else if self.autostart_wait_frames < 10 {
                self.autostart_wait_frames += 1;
                ctx.request_repaint();
            } else if self.autostart_start.elapsed() >= std::time::Duration::from_secs(30) {
                self.start_minimized = false;
                log::warn!("--autostart: main window never became visible within 30s");
            }
            // 其余情况（窗口尚不可见且未超时）：保持 start_minimized，
            // 由 update 末尾的 request_repaint_after(500ms) 提供低频重试帧；
            // 窗口一旦出现，下一帧即被上面的可见分支隐藏。
        }

        // 标题栏应用图标：首帧由 icon.png 创建纹理（窗口图标与任务栏图标
        // 已由 with_icon 设置；标题栏图标补齐自绘标题栏的显示）。
        // 任务栏/窗口图标用多尺寸 ICO 覆盖设置（with_icon 对大 PNG 的
        // 任务栏缩小渲染效果差，见 platform::icon::set_main_window_icon）。
        if self.icon_tex.is_none() {
            crate::platform::icon::set_main_window_icon();
            if let Some(icon) = view::load_icon_data() {
                let color_image = egui::ColorImage::from_rgba_unmultiplied(
                    [icon.width as usize, icon.height as usize],
                    &icon.rgba,
                );
                self.icon_tex =
                    Some(ctx.load_texture("app_icon", color_image, egui::TextureOptions::LINEAR));
            }
        }

        // AC-GUI-05 / F-TRAY-02: 关闭窗口（标题栏关闭按钮 / Alt+F4）时隐藏到
        // 托盘（任务栏图标消失）而非退出进程；仅当用户通过托盘菜单"退出"
        // （quitting）才真正关闭。
        //
        // 隐藏实现（platform::window）：**移到屏幕外**而非 `ShowWindow(SW_HIDE)`。
        // 隐藏窗口不接收 WM_PAINT → winit 不派发 RedrawRequested → eframe
        // update() 停止，托盘/热键/Fn+K 命令全部积压到窗口恢复才执行（实测
        // 回归，修订 1.19）。保持 WS_VISIBLE 但移到屏幕外后，update 循环不断、
        // 命令被实时消费，任务栏同样不占位。
        // 用户点击窗口关闭按钮 → 隐藏到托盘；托盘"退出"命令置位 quitting
        // 后 close_requested 放行，让 eframe 正常退出（修订 1.21：外部
        // WM_QUIT 不触发 run_native 返回，只能靠强杀，跳过 Drop 清理）。
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.quitting {
                log::info!("Quit: close requested; letting eframe exit");
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                crate::platform::window::hide_main_window();
            }
        }

        self.process_commands(ctx);
        // WMI 延迟恢复：首次启动时 WMI 服务未就绪而回退后，按指数退避
        // 探测并自动切回 WMI（见 maybe_probe_wmi_recovery 注释）。
        self.maybe_probe_wmi_recovery();
        // 窗口移到屏幕外（隐藏到托盘）后 update 循环仍以 500ms 间隔运行
        // （见 platform::window 的离屏隐藏设计，修订 1.19）；若间隔过大，
        // 托盘点击 / 全局快捷键 / Fn+K 命令最长要等一个间隔才会被处理
        // （egui 的 mpsc 不会唤醒事件循环）。500ms 上限保证命令延迟 ≤
        // NFR-UX-02 要求（≤500ms），空闲时每 500ms 一帧的 CPU 开销可忽略。
        // 托盘/Fn worker 发送命令时还会额外 request_repaint() 立即唤醒。
        ctx.request_repaint_after(std::time::Duration::from_millis(500));

        self.show_title_bar(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.show_main_view(ui);
            });
        });

        self.show_resize_handle(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个用例独立的临时配置目录：避免任何保存落到真实配置路径。
    fn test_store() -> ConfigStore {
        crate::testutil::temp_store("app-test")
    }

    /// WMI 延迟恢复退避节奏（修订 1.43 死槽修复回归）：第 1~3 次探测后
    /// 分别退避 40s/80s/160s；达到 MAX（最后一次探测）返回 None、不再排布
    /// 下一次——历史实现会留下一个永不触发的死槽，give-up 因此推迟一个
    /// 退避周期。
    #[test]
    fn test_next_probe_after_backoff() {
        let s = |n| std::time::Duration::from_secs(n);
        assert_eq!(next_probe_after(1), Some(s(40)));
        assert_eq!(next_probe_after(2), Some(s(80)));
        assert_eq!(next_probe_after(3), Some(s(160)));
        // 最后一次探测（attempts == MAX）后不再排布下一次。
        assert_eq!(next_probe_after(WMI_RECOVER_MAX_ATTEMPTS), None);
    }

    /// 构造指定后端/偏好的 app（复用测试目录）。
    fn probe_test_app(
        backend: crate::ec::mock::MockBackend,
        config: crate::app::config::AppConfig,
    ) -> XiaomiApp {
        let pref = config.backend;
        XiaomiApp::new(test_store(), Box::new(backend), config, pref, None, false)
    }

    /// 回归测试（修订 1.47）：新一轮武装必须复位已消耗的探测预算。历史实现
    /// `wmi_recover_attempts` 只增不减——首轮 4 次探测全失败后计数停在 4，
    /// 运行期再次熔断重新武装时 `next_probe_after(4)` 直接返回 None，新一轮
    /// 只有 1 次探测机会（且失败即永久放弃）。复位后新一轮完整拥有 4 次预算。
    #[test]
    fn test_arm_wmi_recovery_resets_attempt_budget() {
        let mut app = probe_test_app(
            crate::ec::mock::MockBackend {
                name: "mock-wmi",
                preference: BackendPreference::Wmi,
                needs_rebuild: true,
                ..Default::default()
            },
            crate::app::config::AppConfig::default(),
        );
        // 首轮探测预算耗尽：attempts 停留在预算上限。
        app.wmi_recover_attempts = WMI_RECOVER_MAX_ATTEMPTS;
        assert!(probes_exhausted(app.wmi_recover_attempts));

        // 重新武装（先清空，再 arm——幂等分支只在新武装时复位）。
        app.wmi_recover_at = None;
        app.arm_wmi_recovery();
        assert_eq!(
            app.wmi_recover_attempts, 0,
            "fresh arm must reset the exhausted probe budget"
        );
        assert!(app.wmi_recover_at.is_some());
    }

    /// 回归测试（修订 1.45 审计）：已熔断的 WMI 后端（needs_rebuild）必须
    /// 自动武装恢复探测——此前只能手动点击后端单选重建，熔断后 GUI/托盘
    /// 停留陈旧状态且无自动恢复。
    #[test]
    fn test_wedged_wmi_auto_arms_recovery_probe() {
        let mut app = probe_test_app(
            crate::ec::mock::MockBackend {
                name: "mock-wmi",
                preference: BackendPreference::Wmi,
                needs_rebuild: true,
                ..Default::default()
            },
            crate::app::config::AppConfig::default(),
        );
        // 构造期的 arm_wmi_recovery 已识别"熔断 WMI 不可用"并武装。
        assert!(
            app.wmi_recover_at.is_some(),
            "wedged WMI must have the recovery probe armed"
        );
        // 每帧探测检查：单次自动重建上限置位，探测保持武装。
        app.maybe_probe_wmi_recovery();
        assert!(app.wmi_auto_rebuild_attempted);
        assert!(app.wmi_recover_at.is_some(), "probe must remain armed");
    }

    /// 健康的 WMI 后端不得触发自动重建探测。
    #[test]
    fn test_healthy_wmi_does_not_arm_auto_rebuild() {
        let mut app = probe_test_app(
            crate::ec::mock::MockBackend {
                name: "mock-wmi",
                preference: BackendPreference::Wmi,
                needs_rebuild: false,
                ..Default::default()
            },
            crate::app::config::AppConfig::default(),
        );
        app.maybe_probe_wmi_recovery();
        assert!(app.wmi_recover_at.is_none(), "healthy WMI must not probe");
        assert!(!app.wmi_auto_rebuild_attempted);
    }

    /// 用户偏好非 WMI 时不自动武装 WMI 探测（即使后端"已熔断"——熔断标记
    /// 仅属 WMI 后端，非 WMI 偏好下的重建语义与恢复探测无关）。
    #[test]
    fn test_non_wmi_preference_no_auto_rebuild() {
        let mut app = probe_test_app(
            crate::ec::mock::MockBackend {
                name: "mock-wr0",
                preference: BackendPreference::WinRing0,
                needs_rebuild: true,
                ..Default::default()
            },
            crate::app::config::AppConfig {
                backend: BackendPreference::WinRing0,
                ..crate::app::config::AppConfig::default()
            },
        );
        app.maybe_probe_wmi_recovery();
        assert!(
            app.wmi_recover_at.is_none(),
            "non-WMI preference must not arm a WMI probe"
        );
        assert!(!app.wmi_auto_rebuild_attempted);
    }

    #[test]
    fn test_ui_command_debug() {
        assert_eq!(
            format!("{:?}", UiCommand::ToggleBatteryCare),
            "ToggleBatteryCare"
        );
        assert_eq!(format!("{:?}", UiCommand::CyclePerfMode), "CyclePerfMode");
        assert_eq!(format!("{:?}", UiCommand::ReapplyConfig), "ReapplyConfig");
        assert_eq!(
            format!("{:?}", UiCommand::ReapplyConfigManual),
            "ReapplyConfigManual"
        );
        // 其余变体：每个变体至少一条 Debug 断言，防止枚举演进时漏测。
        assert_eq!(
            format!("{:?}", UiCommand::SetPerfMode(0x04)),
            "SetPerfMode(4)"
        );
        assert_eq!(
            format!(
                "{:?}",
                UiCommand::FnEventSeen {
                    class: "HID_EVENT20".into(),
                    hex: "012801".into()
                }
            ),
            "FnEventSeen { class: \"HID_EVENT20\", hex: \"012801\" }"
        );
        assert_eq!(
            format!("{:?}", UiCommand::SetAutostart(true)),
            "SetAutostart(true)"
        );
        assert_eq!(
            format!("{:?}", UiCommand::SetAutostartResult(true, Ok(()))),
            "SetAutostartResult(true, Ok(()))"
        );
        assert_eq!(
            format!(
                "{:?}",
                UiCommand::WmiAvailable(Box::new(crate::ec::backend::NullBackend))
            ),
            "WmiAvailable(_)"
        );
        assert_eq!(
            format!(
                "{:?}",
                UiCommand::BackendSwitchResult {
                    user_pref: crate::app::config::BackendPreference::Auto,
                    result: Ok(Box::new(crate::ec::backend::NullBackend)),
                }
            ),
            "BackendSwitchResult { user_pref: Auto, result: Ok(_) }"
        );
        assert_eq!(
            format!(
                "{:?}",
                UiCommand::BackendSwitchResult {
                    user_pref: crate::app::config::BackendPreference::Wmi,
                    result: Err("boom".into()),
                }
            ),
            "BackendSwitchResult { user_pref: Wmi, result: Err(boom) }"
        );
        assert_eq!(
            format!(
                "{:?}",
                UiCommand::BatteryHealthUpdated {
                    designed_mwh: 76990,
                    full_mwh: 77255
                }
            ),
            "BatteryHealthUpdated { designed_mwh: 76990, full_mwh: 77255 }"
        );
        assert_eq!(
            format!(
                "{:?}",
                UiCommand::BatteryEtaUpdated {
                    remaining_mwh: 61057,
                    charge_rate_mw: 0,
                    discharge_rate_mw: 12000,
                    charging: false,
                    discharging: true,
                }
            ),
            "BatteryEtaUpdated { remaining_mwh: 61057, charge_rate_mw: 0, discharge_rate_mw: 12000, charging: false, discharging: true }"
        );
        assert_eq!(format!("{:?}", UiCommand::Quit), "Quit");
    }

    #[test]
    fn test_xiaomi_app_new_with_defaults() {
        let backend = Box::new(crate::ec::backend::NullBackend);
        let config = crate::app::config::AppConfig::default();
        let app = XiaomiApp::new(
            test_store(),
            backend,
            config,
            crate::app::config::BackendPreference::Auto,
            None,
            false,
        );

        assert_eq!(app.backend.name(), "无后端");
        assert!(!app.runtime.battery_care_enabled);
        assert_eq!(app.runtime.charge_limit, 80);
        assert_eq!(app.runtime.performance_mode, 0x09);
        // NullBackend 所有读取均失败，启动刷新后应呈现错误信息。
        assert!(app.error_msg.is_some());
    }

    #[test]
    fn test_xiaomi_app_new_with_custom_config() {
        let backend = Box::new(crate::ec::backend::NullBackend);
        let config = crate::app::config::AppConfig {
            battery_care_enabled: true,
            battery_charge_limit: 60,
            performance_mode: 0x02,
            ..Default::default()
        };
        let app = XiaomiApp::new(
            test_store(),
            backend,
            config,
            crate::app::config::BackendPreference::Wmi,
            Some("初始化失败".into()),
            false,
        );

        assert!(app.runtime.battery_care_enabled);
        assert_eq!(app.runtime.charge_limit, 60);
        assert_eq!(app.runtime.performance_mode, 0x02);
        assert_eq!(app.current_pref, crate::app::config::BackendPreference::Wmi);
        // 初始化错误与启动刷新产生的读取错误合并展示。
        assert!(app
            .error_msg
            .as_deref()
            .unwrap_or_default()
            .contains("初始化失败"));
    }

    #[test]
    fn test_xiaomi_app_new_with_backend_error() {
        let backend = Box::new(crate::ec::backend::NullBackend);
        let config = crate::app::config::AppConfig::default();
        let app = XiaomiApp::new(
            test_store(),
            backend,
            config,
            crate::app::config::BackendPreference::Auto,
            Some("后端不可用".into()),
            false,
        );

        assert_eq!(
            app.error_msg.as_deref().map(|s| s.contains("后端不可用")),
            Some(true)
        );
    }

    #[test]
    fn test_xiaomi_app_send() {
        fn assert_send<T: Send>() {}
        assert_send::<XiaomiApp>();
    }
}
