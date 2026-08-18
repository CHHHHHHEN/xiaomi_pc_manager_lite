/// GUI 线程消费的 UI 命令（托盘/热键/Fn+Key 监听/自启动 worker 线程发送）。
///
/// 不派生 `Debug`：`WmiAvailable` 携带 `Box<dyn EcBackend>`（无 Debug），
/// 手动实现等价格式（见下方 impl）。每变体至少一条 Debug 断言测试锁定格式。
pub enum UiCommand {
    ToggleBatteryCare,
    CyclePerfMode,
    /// 直接切换到指定性能模式（托盘子菜单/热键按值设置）。
    SetPerfMode(u8),
    /// 把持久化配置整份重新应用到硬件（**受 `auto_reapply_on_power_change`
    /// 门控**）：由电源广播（插拔/唤醒）这类被动场景发送——开关关闭时
    /// 静默忽略（用户主动关闭了"电源切换自动重设"，此时被动重设不应覆盖
    /// 硬件状态）。
    ReapplyConfig,
    /// 把持久化配置整份重新应用到硬件（**不受门控**）：由用户**主动**动作
    /// 发送（Fn 绑定"重新应用设置"）。与 `ReapplyConfig` 的语义区别见
    /// `gui::commands` 的 `reapply_config`/`apply_config_and_sync`——用户
    /// 明确请求重设时，开关关闭不应让命令静默无效（M3 回归，修订 1.30）。
    ReapplyConfigManual,
    /// Fn 捕获模式（`FnKeyBindings` 设置的"捕获功能键"）下收到的事件：
    /// 参数为 (事件类, 归一化报告 hex)。
    FnEventSeen {
        class: String,
        hex: String,
    },
    /// 用户勾选/取消"开机自启动"：在后台线程执行计划任务注册/删除，
    /// 完成后发送 SetAutostartResult 回传结果（GUI 线程不触碰 COM）。
    SetAutostart(bool),
    /// 开机自启动操作结果：参数为（期望值, 结果）。
    SetAutostartResult(bool, Result<(), String>),
    /// 首次启动时 WMI 服务尚未就绪导致后端回退，延迟恢复探测线程探测到
    /// WMI 可用后把建好的后端交回 GUI（见 XiaomiApp 的 wmi_recover_*）。
    /// GUI 消费时校验用户偏好仍指向 WMI（探测期间手动切换则丢弃）。
    WmiAvailable(Box<dyn crate::app::ec::EcBackend>),
    /// 手动后端切换的异步结果（见 gui::commands::try_switch_backend）：
    /// 创建后端在**后台线程**执行（WMI 握手最长 ~10s，放 GUI 线程会卡帧，
    /// 与 wmi_recover 的延迟恢复同模式）。`user_pref` 是用户发起切换时的
    /// 偏好（GUI 消费时校验当前偏好仍等于它，中途改选则丢弃过期结果）。
    BackendSwitchResult {
        user_pref: crate::app::ec::BackendPreference,
        result: Result<Box<dyn crate::app::ec::EcBackend>, String>,
    },
    /// 电池健康读数更新（`platform::battery_health` 后台线程上报）：
    /// 参数为（设计容量 mWh, 当前满充容量 mWh）。满充容量/设计容量来自
    /// `root\WMI` 的 `BatteryStaticData`/`BatteryFullChargedCapacity`，
    /// 与 EC 后端无关。
    BatteryHealthUpdated {
        designed_mwh: u32,
        full_mwh: u32,
    },
    /// 电池充放电状态更新（同一后台线程，修订 1.37）：用于预计剩余/充满
    /// 时长。参数为（剩余容量 mWh, 充电速率 mW, 放电速率 mW, 是否充电,
    /// 是否放电），来自 `root\WMI` 的 `BatteryStatus`。
    BatteryEtaUpdated {
        remaining_mwh: u32,
        charge_rate_mw: u32,
        discharge_rate_mw: u32,
        charging: bool,
        discharging: bool,
    },
    /// 退出应用：由托盘"退出"菜单发起。GUI 收到后通过
    /// `ViewportCommand::Close` 请求 eframe 正常退出事件循环，
    /// 从而运行各组件 `Drop`（WinRing0 后端 DeinitializeOls 等）。
    /// 不能靠直接向主窗口 `PostMessage(WM_QUIT)`：winit 事件循环
    /// 只处理自己派发的消息，外部 WM_QUIT 不触发 `run_native` 返回，
    /// 进程只能靠托盘 worker 的 15s 兜底 `process::exit` 强杀，
    /// 跳过所有清理（实测，修订 1.21）。
    Quit,
}

impl std::fmt::Debug for UiCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 输出格式与派生 Debug 保持一致（gui::commands::process_commands 的
        // `{:?}` 日志依赖可读性；tests::test_ui_command_debug 锁定各变体格式）。
        match self {
            Self::ToggleBatteryCare => f.write_str("ToggleBatteryCare"),
            Self::CyclePerfMode => f.write_str("CyclePerfMode"),
            Self::SetPerfMode(mode) => write!(f, "SetPerfMode({})", mode),
            Self::ReapplyConfig => f.write_str("ReapplyConfig"),
            Self::ReapplyConfigManual => f.write_str("ReapplyConfigManual"),
            Self::FnEventSeen { class, hex } => {
                write!(
                    f,
                    "FnEventSeen {{ class: \"{}\", hex: \"{}\" }}",
                    class, hex
                )
            }
            Self::SetAutostart(enabled) => write!(f, "SetAutostart({})", enabled),
            Self::SetAutostartResult(enabled, result) => {
                write!(f, "SetAutostartResult({}, {:?})", enabled, result)
            }
            // 后端实例不展示内容（无 Debug），只标记命令类型。
            Self::WmiAvailable(_) => f.write_str("WmiAvailable(_)"),
            Self::BackendSwitchResult { user_pref, result } => {
                write!(
                    f,
                    "BackendSwitchResult {{ user_pref: {:?}, result: {} }}",
                    user_pref,
                    match result {
                        Ok(_) => "Ok(_)".to_string(),
                        Err(e) => format!("Err({})", e),
                    }
                )
            }
            Self::BatteryHealthUpdated {
                designed_mwh,
                full_mwh,
            } => write!(
                f,
                "BatteryHealthUpdated {{ designed_mwh: {}, full_mwh: {} }}",
                designed_mwh, full_mwh
            ),
            Self::BatteryEtaUpdated {
                remaining_mwh,
                charge_rate_mw,
                discharge_rate_mw,
                charging,
                discharging,
            } => write!(
                f,
                "BatteryEtaUpdated {{ remaining_mwh: {}, charge_rate_mw: {}, discharge_rate_mw: {}, charging: {}, discharging: {} }}",
                remaining_mwh, charge_rate_mw, discharge_rate_mw, charging, discharging
            ),
            Self::Quit => f.write_str("Quit"),
        }
    }
}
