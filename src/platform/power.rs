//! 电源状态查询（Windows 系统 API）。

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

fn classify_acline(ac_line_status: u8) -> PowerStatus {
    match ac_line_status {
        // MSDN SYSTEM_POWER_STATUS.ACLineStatus：0=电池，1=交流，255=未知。
        0 => PowerStatus::OnBattery,
        1 => PowerStatus::OnAc,
        _ => PowerStatus::Unknown,
    }
}

/// 未知电源状态告警的**去重**：`power_status`/`power_snapshot` 被 GUI 每帧
/// 与托盘每 2s 轮询，若某台机器 `ACLineStatus` 恒为 255（无电池/驱动异常），
/// 未去重的实现会在每个调用点刷一条 warn——每秒几十条重复日志，把真实告警
/// 淹没并加速日志轮转（M2 回归，修订 1.30）。只在**状态值变化**时记录：
/// 首次出现未知值告警一次，之后同样的未知值静默。
static LAST_WARNED_UNKNOWN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 记录未知电源状态告警（首次出现时）。返回是否实际记录了本次告警。
fn warn_unknown_once() -> bool {
    let first = !LAST_WARNED_UNKNOWN.swap(true, std::sync::atomic::Ordering::Relaxed);
    if first {
        log::warn!("ACLineStatus unknown; power state unknown (won't repeat)");
    }
    first
}

/// 一次查询 `GetSystemPowerStatus`，失败时返回 None 并记录错误。
///
/// `power_status` / `power_snapshot` 共用此函数，避免各自重复错误日志。
fn system_power_status() -> Option<windows::Win32::System::Power::SYSTEM_POWER_STATUS> {
    let mut status = unsafe { std::mem::zeroed() };
    if unsafe { windows::Win32::System::Power::GetSystemPowerStatus(&mut status) }.is_err() {
        log::error!("GetSystemPowerStatus failed; power state unknown");
        return None;
    }
    Some(status)
}

/// 查询当前电源状态。
///
/// 历史实现位于 `ec::performance`（纯逻辑枚举/EC 值映射），但电源查询是纯
/// Windows 系统能力（`GetSystemPowerStatus`），与领域模型无关——收敛到
/// platform 层后，`ec::performance` 保持为无平台依赖的纯领域模块。
pub fn power_status() -> PowerStatus {
    let Some(status) = system_power_status() else {
        return PowerStatus::Unknown;
    };
    let power = classify_acline(status.ACLineStatus);
    if power == PowerStatus::Unknown {
        warn_unknown_once();
    }
    power
}

/// 一次调用同时返回电源状态与电池电量百分比（状态栏/托盘共用，避免两次
/// 完整查询）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerSnapshot {
    pub status: PowerStatus,
    /// 电池电量百分比；`None` 表示 API 失败、未知（255）或未装电池。
    pub battery_percent: Option<u8>,
}

/// 电源状态与电池电量的一次性快照。
pub fn power_snapshot() -> PowerSnapshot {
    let Some(s) = system_power_status() else {
        return PowerSnapshot {
            status: PowerStatus::Unknown,
            battery_percent: None,
        };
    };
    let status = classify_acline(s.ACLineStatus);
    if status == PowerStatus::Unknown {
        warn_unknown_once();
    }
    // MSDN BatteryLifePercent：0-100 有效，255=未知/未装。255 返回 None，
    // 由调用方显示"未知"而非荒谬的 255%。
    let battery_percent = (s.BatteryLifePercent != 255).then_some(s.BatteryLifePercent);
    PowerSnapshot {
        status,
        battery_percent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MSDN 语义：0=电池、1=交流、255=未知（不能把未知静默当电池）。
    #[test]
    fn test_classify_acline_semantics() {
        assert_eq!(classify_acline(0), PowerStatus::OnBattery);
        assert_eq!(classify_acline(1), PowerStatus::OnAc);
        assert_eq!(classify_acline(255), PowerStatus::Unknown);
        // 其余值均按未知处理，绝不静默归入电池。
        assert_eq!(classify_acline(2), PowerStatus::Unknown);
    }

    /// 仅验证可调用、不崩溃；结果取决于运行环境。
    #[test]
    fn test_power_status_does_not_panic() {
        let _ = power_status();
    }

    /// 未知电源状态告警去重（M2 回归，修订 1.30）：首次告警返回 true，
    /// 之后同状态的重复告警返回 false——GUI 每帧 + 托盘每 2s 轮询下
    /// 不会每秒刷几十条重复日志。
    #[test]
    fn test_warn_unknown_once_deduplicates() {
        // 该静态量被生产路径共享：把已知的当前值记下，测试结束后恢复，
        // 避免污染其它测试或真实轮询的状态。
        let prev = LAST_WARNED_UNKNOWN.load(std::sync::atomic::Ordering::Relaxed);
        LAST_WARNED_UNKNOWN.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(warn_unknown_once(), "first unknown must be reported");
        assert!(!warn_unknown_once(), "repeated unknown must be silenced");
        LAST_WARNED_UNKNOWN.store(prev, std::sync::atomic::Ordering::Relaxed);
    }
}
