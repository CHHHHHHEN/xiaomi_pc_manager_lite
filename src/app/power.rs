//! 电源状态的领域模型与端口。
//!
//! `PowerStatus` 三态枚举与 `PowerSnapshot` 是纯领域类型；`PowerSource` 是
//! 查询电源状态的**端口**（trait）。Windows 实现位于 `platform::power`，
//! 领域层只依赖本模块，不触碰 Windows API。
//!
//! 历史实现把 `PowerStatus` 定义在 `platform::power`，导致 `ec::battery`
//! （领域策略）反向依赖平台层。收敛到此处后：`platform::power` 实现
//! `PowerSource` 端口，领域/调用方只持有 `PowerStatus` 值与快照类型。

/// 当前电源状态（三态）。
///
/// 历史实现把 `GetSystemPowerStatus` 的失败**静默当作电池供电**（返回
/// false），且 MSDN 定义的 `ACLineStatus == 255`（未知）也被判为电池——
/// 交流供电下狂暴模式会被静默降级为极速，用户选择被无声改写。改为三态：
/// 只有在**确认**是电池供电时才执行降级；未知（API 失败或 255）单独标记，
/// 由调用方决定处理（不静默降级）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerStatus {
    /// 接入交流电源。
    OnAc,
    /// 电池供电。
    OnBattery,
    /// 无法确认（`GetSystemPowerStatus` 失败或返回未定义值）。
    Unknown,
}

/// 一次电源状态与电池电量查询的快照（GUI 状态栏/托盘共用，避免多次查询）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerSnapshot {
    pub status: PowerStatus,
    /// 电池电量百分比；`None` 表示 API 失败、未知（255）或未装电池。
    pub battery_percent: Option<u8>,
}

/// 电源状态查询端口：调用方只依赖本 trait，具体实现由平台层注入。
///
/// `power_status()` / `power_snapshot()` 的调用方（GUI 刷新、性能模式写入、
/// 启动/电源重设路径）通过本端口拿到电源状态，领域层不直接触碰 Windows API，
/// 使纯逻辑可脱离平台测试。
pub trait PowerSource: Send + Sync {
    fn snapshot(&self) -> PowerSnapshot;
}

/// 电源状态展示文案（GUI 状态栏"电源:"标签的措辞）。
///
/// 与 [`power_status_text_tray`] 共享同一 `(status, percent)` 判定形状；两份
/// 措辞（GUI 较详细、托盘紧凑）收敛到本模块，`PowerStatus` 变体演进时只需
/// 在此一处更新（修订 1.49 整理：两处调用点原本各自手写 5 分支 match）。
pub fn power_status_text_gui(status: PowerStatus, percent: Option<u8>) -> String {
    match (status, percent) {
        (PowerStatus::OnAc, Some(pct)) => format!("交流电 · 电量 {}%", pct),
        (PowerStatus::OnAc, None) => "交流电".to_string(),
        (PowerStatus::OnBattery, Some(pct)) => format!("电池 · 电量 {}%", pct),
        (PowerStatus::OnBattery, None) => "电池 · 电量未知".to_string(),
        (PowerStatus::Unknown, _) => "未知".to_string(),
    }
}

/// 电源状态展示文案（托盘 tooltip"电源:"段的紧凑措辞），见
/// [`power_status_text_gui`] 的收敛说明。
pub fn power_status_text_tray(status: PowerStatus, percent: Option<u8>) -> String {
    match (status, percent) {
        (PowerStatus::OnAc, Some(pct)) => format!("交流 {pct}%"),
        (PowerStatus::OnAc, None) => "交流".to_string(),
        (PowerStatus::OnBattery, Some(pct)) => format!("电池 {pct}%"),
        (PowerStatus::OnBattery, None) => "电池".to_string(),
        (PowerStatus::Unknown, _) => "未知".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GUI 与托盘两种措辞的电源状态文案：5 种 (status, percent) 组合各自
    /// 有确定输出（变体演进时此处强制更新全部组合）。
    #[test]
    fn test_power_status_texts() {
        use PowerStatus::*;
        let cases_gui = [
            ((OnAc, Some(80)), "交流电 · 电量 80%"),
            ((OnAc, None), "交流电"),
            ((OnBattery, Some(42)), "电池 · 电量 42%"),
            ((OnBattery, None), "电池 · 电量未知"),
            ((Unknown, Some(50)), "未知"),
        ];
        for ((status, pct), expected) in cases_gui {
            assert_eq!(power_status_text_gui(status, pct), expected);
        }
        let cases_tray = [
            ((OnAc, Some(80)), "交流 80%"),
            ((OnAc, None), "交流"),
            ((OnBattery, Some(42)), "电池 42%"),
            ((OnBattery, None), "电池"),
            ((Unknown, Some(50)), "未知"),
        ];
        for ((status, pct), expected) in cases_tray {
            assert_eq!(power_status_text_tray(status, pct), expected);
        }
    }
}
