#[derive(Debug)]
pub enum UiCommand {
    ToggleBatteryCare,
    CyclePerfMode,
    /// 直接切换到指定性能模式（托盘子菜单/热键按值设置）。
    SetPerfMode(u8),
    ReapplyConfig,
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
    /// 退出应用：由托盘"退出"菜单发起。GUI 收到后通过
    /// `ViewportCommand::Close` 请求 eframe 正常退出事件循环，
    /// 从而运行各组件 `Drop`（WinRing0 后端 DeinitializeOls 等）。
    /// 不能靠直接向主窗口 `PostMessage(WM_QUIT)`：winio 事件循环
    /// 只处理自己派发的消息，外部 WM_QUIT 不触发 `run_native` 返回，
    /// 进程只能靠托盘 worker 的 15s 兜底 `process::exit` 强杀，
    /// 跳过所有清理（实测，修订 1.21）。
    Quit,
}
