//! 电源状态查询（Windows 系统 API）——`PowerSource` 端口的平台实现。
//!
//! 领域类型 `PowerStatus`/`PowerSnapshot` 与端口 `PowerSource` 定义在
//! `app::power`（纯领域模块）。本模块提供 `WindowsPowerSource`（`GetSystemPowerStatus`
//! 实现）以及面向兼容的 `power_status()` / `power_snapshot()` 便捷函数。

use crate::app::power::PowerSource;
pub use crate::app::power::{PowerSnapshot, PowerStatus};

/// MSDN `SYSTEM_POWER_STATUS` 的"未知"哨兵值：`ACLineStatus == 255`（未知）
/// 与 `BatteryLifePercent == 255`（未知/未装电池）共用同一字节。
const UNKNOWN_SENTINEL: u8 = 255;

fn classify_acline(ac_line_status: u8) -> PowerStatus {
    match ac_line_status {
        // MSDN SYSTEM_POWER_STATUS.ACLineStatus：0=电池，1=交流，255=未知。
        0 => PowerStatus::OnBattery,
        1 => PowerStatus::OnAc,
        _ => PowerStatus::Unknown,
    }
}

/// 未知电源状态告警的**去重**：`WindowsPowerSource::snapshot` 被 GUI 每帧
/// 与托盘每 2s 轮询，若某台机器 `ACLineStatus` 恒为 255（无电池/驱动异常），
/// 未去重的实现会在每个调用点刷一条 warn。只在**状态值变化**时记录。
static LAST_WARNED_UNKNOWN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 未知电源状态告警（仅首次出现时记录一次，之后静默）。
fn warn_unknown_once() -> bool {
    crate::util::log_once(
        log::Level::Warn,
        &LAST_WARNED_UNKNOWN,
        "Power state unknown; won't repeat this session",
    )
}

/// `GetSystemPowerStatus` 查询的 `PowerSource` 实现。
///
/// 用法：`let power = WindowsPowerSource; power.snapshot().status`。
/// 领域层/用例层只依赖 `&dyn PowerSource`，不触碰 Windows API。
pub struct WindowsPowerSource;

impl PowerSource for WindowsPowerSource {
    fn snapshot(&self) -> PowerSnapshot {
        power_snapshot()
    }
}

/// 一次查询 `GetSystemPowerStatus`，失败时返回 None 并记录错误（经
/// `warn_unknown_once` 去重）。
fn system_power_status() -> Option<windows::Win32::System::Power::SYSTEM_POWER_STATUS> {
    let mut status = unsafe { std::mem::zeroed() };
    if unsafe { windows::Win32::System::Power::GetSystemPowerStatus(&mut status) }.is_err() {
        warn_unknown_once();
        return None;
    }
    Some(status)
}

/// 查询当前电源状态。
///
/// 历史实现位于 `ec::performance`（纯逻辑枚举/EC 值映射），但电源查询是纯
/// Windows 系统能力（`GetSystemPowerStatus`），与领域模型无关——收敛到
/// platform 层后，`app::performance` 保持为无平台依赖的纯领域模块。
pub fn power_status() -> PowerStatus {
    power_snapshot().status
}

/// 电源状态与电池电量的一次性快照（GUI 状态栏/托盘共用，避免两次查询）。
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
    // 由调用方显示"未知"而非荒谬的 255%；其余越界值（如损坏驱动上报 150）
    // 同样返回 None——100 是合法上限，不允许显示"电量 150%"（修订 1.50）。
    let battery_percent = (s.BatteryLifePercent != UNKNOWN_SENTINEL && s.BatteryLifePercent <= 100)
        .then_some(s.BatteryLifePercent);
    PowerSnapshot {
        status,
        battery_percent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 串行化依赖共享静态 `LAST_WARNED_UNKNOWN` 的两个测试。
    static POWER_TEST_SERIALIZE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// MSDN 语义：0=电池、1=交流、255=未知（不能把未知静默当电池）。
    #[test]
    fn test_classify_acline_semantics() {
        assert_eq!(classify_acline(0), PowerStatus::OnBattery);
        assert_eq!(classify_acline(1), PowerStatus::OnAc);
        assert_eq!(classify_acline(UNKNOWN_SENTINEL), PowerStatus::Unknown);
        // 其余值均按未知处理，绝不静默归入电池。
        assert_eq!(classify_acline(2), PowerStatus::Unknown);
    }

    /// 仅验证可调用、不崩溃；结果取决于运行环境。
    #[test]
    fn test_power_status_does_not_panic() {
        let _guard = POWER_TEST_SERIALIZE.lock().unwrap();
        let _ = power_status();
        let _ = WindowsPowerSource.snapshot();
    }

    /// 未知电源状态告警去重。
    #[test]
    fn test_warn_unknown_once_deduplicates() {
        let _guard = POWER_TEST_SERIALIZE.lock().unwrap();
        let prev = LAST_WARNED_UNKNOWN.load(std::sync::atomic::Ordering::Relaxed);
        LAST_WARNED_UNKNOWN.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(warn_unknown_once(), "first unknown must be reported");
        assert!(!warn_unknown_once(), "repeated unknown must be silenced");
        LAST_WARNED_UNKNOWN.store(prev, std::sync::atomic::Ordering::Relaxed);
    }
}
