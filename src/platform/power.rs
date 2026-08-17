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
        log::warn!(
            "ACLineStatus = {}; power state unknown",
            status.ACLineStatus
        );
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
        log::warn!("ACLineStatus = {}; power state unknown", s.ACLineStatus);
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
}
