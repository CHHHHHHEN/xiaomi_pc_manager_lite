#[derive(Debug)]
pub enum UiCommand {
    ToggleBatteryCare,
    CyclePerfMode,
    ReapplyConfig,
    /// 用户勾选/取消"开机自启动"：在后台线程执行计划任务注册/删除，
    /// 完成后发送 SetAutostartResult 回传结果（GUI 线程不触碰 COM）。
    SetAutostart(bool),
    /// 开机自启动操作结果：参数为（期望值, 结果）。
    SetAutostartResult(bool, Result<(), String>),
}
