mod message_window;
pub mod worker;

pub use worker::spawn;

/// 托盘工具提示/菜单展示所需的运行时状态快照（GUI 线程写入、托盘线程读取）。
///
/// 线程模型：GUI 线程在每次状态变更（后端刷新、命令执行）后经
/// `TrayStatus` 的共享实例更新；托盘 worker 线程按固定周期读取并刷新
/// tooltip，右键菜单打开时读取以展示当前性能模式。共享经 `Mutex`：
/// 双方都是短临界区、无嵌套锁，不存在死锁风险。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrayStatus {
    /// 电池养护当前状态。
    pub battery_care_enabled: bool,
    /// 充电上限当前值（%）。
    pub charge_limit: u8,
    /// 性能模式当前值（EC raw code）。
    pub performance_mode: u8,
}

impl Default for TrayStatus {
    fn default() -> Self {
        Self {
            battery_care_enabled: false,
            charge_limit: 80,
            performance_mode: crate::ec::performance::PerfMode::Smart.ec_value(),
        }
    }
}

/// 托盘共享状态的类型别名。
pub type SharedTrayStatus = std::sync::Arc<std::sync::Mutex<TrayStatus>>;
