mod message_window;
mod notify;
pub mod worker;

pub use worker::spawn;

/// 托盘工具提示/菜单展示所需的运行时状态快照（GUI 线程写入、托盘线程读取）。
///
/// 线程模型：GUI 线程在每次状态变更（后端刷新、命令执行）后经
/// `TrayStatus` 的共享实例更新；托盘 worker 线程按固定周期读取并刷新
/// tooltip，右键菜单打开时读取以展示当前性能模式。共享经 `Mutex`：
/// 双方都是短临界区、无嵌套锁，不存在死锁风险。
#[derive(Debug, Clone, PartialEq)]
pub struct TrayStatus {
    /// 电池养护当前状态。
    pub battery_care_enabled: bool,
    /// 充电上限当前值（%）。
    pub charge_limit: u8,
    /// 性能模式当前值（EC raw code）。
    pub performance_mode: u8,
    /// 电池健康度（满充/设计容量 × 100，整数百分比）；`None` = 尚未读到或本机无数据
    /// （tooltip 不展示该段）。
    pub battery_health_percent: Option<u8>,
    /// 预计剩余/充满时长文案（GUI 后台线程估算，修订 1.37）；`None` = 速率
    /// 不可用，tooltip 不展示该段。
    pub battery_eta_text: Option<String>,
    /// "充电达到养护上限时通知"开关（GUI 同步配置，托盘读取）。
    pub notify_on_charge_limit: bool,
}

impl TrayStatus {
    /// 以持久化配置初始化（GUI 启动时），随后被硬件实际状态覆盖。
    ///
    /// 三字段 + 通知开关的赋值曾分别在 gui/app.rs（构造）与 gui/commands.rs
    /// （sync_tray_status）各自手写一份（修订 1.49 整理）——收敛到构造器后，
    /// 字段演进只需改此处。
    pub fn from_config(config: &crate::app::config::AppConfig) -> Self {
        Self {
            battery_care_enabled: config.battery_care_enabled,
            charge_limit: config.battery_charge_limit,
            performance_mode: config.performance_mode,
            battery_health_percent: None,
            battery_eta_text: None,
            notify_on_charge_limit: config.notify_on_charge_limit,
        }
    }

    /// 更新运行时三字段 + 通知开关（GUI 每次状态变更后调用，使托盘 tooltip
    /// 保持实时）。健康度与 ETA 由独立事件更新，不属于本方法范围。
    pub fn sync_runtime(
        &mut self,
        battery_care_enabled: bool,
        charge_limit: u8,
        performance_mode: u8,
        notify_on_charge_limit: bool,
    ) {
        self.battery_care_enabled = battery_care_enabled;
        self.charge_limit = charge_limit;
        self.performance_mode = performance_mode;
        self.notify_on_charge_limit = notify_on_charge_limit;
    }
}

/// 托盘共享状态的类型别名。
pub type SharedTrayStatus = std::sync::Arc<std::sync::Mutex<TrayStatus>>;
