use eframe::egui;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};

use crate::command::UiCommand;
use crate::ec;
use crate::ec::config::{BackendPreference, ConfigStore};
use crate::ec::fnkey::SharedBindings;
use crate::tray::{SharedTrayStatus, TrayStatus};

use super::view;

/// GUI 运行时硬件状态（与持久化配置解耦的独立事实来源）。
///
/// 历史上这三个字段直接挂在 `XiaomiApp` 上、与 `config` 的同名字段并列，
/// 同一层出现两组同名状态：`battery_care_enabled` 既是"硬件实际状态"又是
/// "配置期望值"，读取/更新极易用错源。收敛到独立结构体后，`runtime.*` 表示
/// **硬件/界面当前认知**，`config.*` 表示**持久化的用户期望**——二者仅在
/// auto_apply 关闭且硬件被外部改动时不同（见 `refresh_from_backend` 的注释）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RuntimeState {
    /// 电池养护当前状态（界面勾选框与状态栏显示）。
    pub battery_care_enabled: bool,
    /// 充电上限当前值（界面滑块/预设档位显示）。
    pub charge_limit: u8,
    /// 性能模式当前值（界面高亮显示；狂暴在电池供电时写入会降级，见
    /// `ec::battery::effective_perf_for_current_power`）。
    pub performance_mode: u8,
}

impl RuntimeState {
    /// 以持久化配置初始化运行时状态（随后会被 `refresh_from_config` 用硬件
    /// 实际状态覆盖）。
    fn from_config(config: &ec::config::AppConfig) -> Self {
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
    pub(crate) config: ec::config::AppConfig,
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
    pub(crate) fn_add_action: ec::fnkey::FnAction,
    /// 托盘 tooltip/菜单共享的运行时状态（GUI 写入，托盘线程周期读取）。
    pub(crate) tray_status: SharedTrayStatus,
    /// egui Context 的线程安全克隆：供托盘/Fn 监听/自启动 worker 线程在
    /// 发送命令后唤醒隐藏的 GUI 事件循环（见 update 的隐藏态处理说明）。
    pub(crate) egui_ctx: egui::Context,
    /// 退出标志：托盘"退出"命令（UiCommand::Quit）置位后，下一帧的
    /// close_requested 不再被取消/隐藏，而是放行让 eframe 真正退出事件
    /// 循环（保证 Drop 清理执行）。用户点击窗口关闭按钮时不置位，仍走
    /// "隐藏到托盘"路径（修订 1.21）。
    pub(crate) quitting: bool,
}

impl XiaomiApp {
    pub fn new(
        store: ConfigStore,
        backend: Box<dyn ec::backend::EcBackend>,
        config: ec::config::AppConfig,
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
            fn_bindings,
            fn_capture: Arc::new(AtomicBool::new(false)),
            last_fn_event: None,
            fn_add_preset_index: 0,
            fn_add_action: ec::fnkey::FnAction::CyclePerfMode,
            tray_status,
            // 占位 Context：真正的实例在 run_app 的 eframe 创建闭包中注入
            //（cc.egui_ctx.clone()），供托盘/Fn worker 唤醒隐藏事件循环。
            egui_ctx: egui::Context::default(),
            quitting: false,
        };

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
}

pub fn run_app(
    store: ConfigStore,
    backend: Box<dyn ec::backend::EcBackend>,
    config: ec::config::AppConfig,
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
            .with_inner_size([520.0, 680.0])
            .with_min_inner_size([400.0, 500.0])
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

            // egui Context 注入：托盘/Fn worker 线程在发送命令后据此唤醒
            // 隐藏的 GUI 事件循环（否则隐藏态下命令积压、窗口恢复才执行）。
            let ctx = cc.egui_ctx.clone();
            let mut app = app;
            app.egui_ctx = ctx.clone();

            // 托盘线程共享的运行时状态（tooltip/菜单实时展示）。
            crate::tray::spawn(cmd_tx.clone(), app.tray_status.clone(), ctx.clone());
            // Fn 监听线程与 GUI 共享绑定表与捕获开关（配置保存即即时生效）。
            crate::ec::fnkey::spawn(
                cmd_tx.clone(),
                app.fn_bindings.clone(),
                app.fn_capture.clone(),
                ctx,
            );

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
        // 任务栏缩小渲染效果差，见 platform::window::set_main_window_icon）。
        if self.icon_tex.is_none() {
            crate::platform::window::set_main_window_icon();
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

    #[test]
    fn test_ui_command_debug() {
        assert_eq!(
            format!("{:?}", UiCommand::ToggleBatteryCare),
            "ToggleBatteryCare"
        );
        assert_eq!(format!("{:?}", UiCommand::CyclePerfMode), "CyclePerfMode");
        assert_eq!(format!("{:?}", UiCommand::ReapplyConfig), "ReapplyConfig");
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
        assert_eq!(format!("{:?}", UiCommand::Quit), "Quit");
    }

    #[test]
    fn test_xiaomi_app_new_with_defaults() {
        let backend = Box::new(crate::ec::backend::NullBackend);
        let config = crate::ec::config::AppConfig::default();
        let app = XiaomiApp::new(
            test_store(),
            backend,
            config,
            crate::ec::config::BackendPreference::Auto,
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
        let config = crate::ec::config::AppConfig {
            battery_care_enabled: true,
            battery_charge_limit: 60,
            performance_mode: 0x02,
            ..Default::default()
        };
        let app = XiaomiApp::new(
            test_store(),
            backend,
            config,
            crate::ec::config::BackendPreference::Wmi,
            Some("初始化失败".into()),
            false,
        );

        assert!(app.runtime.battery_care_enabled);
        assert_eq!(app.runtime.charge_limit, 60);
        assert_eq!(app.runtime.performance_mode, 0x02);
        assert_eq!(app.current_pref, crate::ec::config::BackendPreference::Wmi);
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
        let config = crate::ec::config::AppConfig::default();
        let app = XiaomiApp::new(
            test_store(),
            backend,
            config,
            crate::ec::config::BackendPreference::Auto,
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
