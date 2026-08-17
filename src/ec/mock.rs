//! 共享的内存测试后端（仅测试编译）。
//!
//! `ec::battery`、`startup`、`gui::commands` 的测试各自重复实现了多个
//! 仅操作成败/量化行为不同的 mock `EcBackend`（历史累计 9+ 份、约 450 行），
//! 存在漂移风险。统一收敛到此处：单一可配置后端经字段区分行为，测试断言
//! 通过 `Arc` 原子暴露的内部状态完成。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

use crate::ec::backend::EcBackend;
use crate::ec::config::BackendPreference;
use crate::ec::error::EcError;

/// 可配置的内存 `EcBackend`。
///
/// - `read_fails`：所有 get 返回错误（模拟后端不可读）；
/// - `set_*_fails`：对应写入操作失败；
/// - `quantize`：模拟 WMI 把非预设充电上限就近取整（如 85→80）。
///
/// 状态经 `Arc` 原子共享，测试可把实例 clone 进应用后仍能断言内部值。
#[derive(Clone)]
pub struct MockBackend {
    pub charge_limit: Arc<AtomicU8>,
    pub battery_care: Arc<AtomicBool>,
    pub perf_mode: Arc<AtomicU8>,
    /// `get_battery_state` 调用计数（回归测试：刷新必须单次往返）。
    pub battery_state_calls: Arc<AtomicU32>,
    pub name: &'static str,
    pub preference: BackendPreference,
    pub quantize: bool,
    pub read_fails: bool,
    pub set_charge_limit_fails: bool,
    pub set_battery_care_fails: bool,
    pub set_perf_fails: bool,
}

impl Default for MockBackend {
    fn default() -> Self {
        Self {
            charge_limit: Arc::new(AtomicU8::new(0)),
            battery_care: Arc::new(AtomicBool::new(false)),
            perf_mode: Arc::new(AtomicU8::new(0x09)),
            battery_state_calls: Arc::new(AtomicU32::new(0)),
            name: "mock",
            preference: BackendPreference::Auto,
            quantize: false,
            read_fails: false,
            set_charge_limit_fails: false,
            set_battery_care_fails: false,
            set_perf_fails: false,
        }
    }
}

impl MockBackend {
    /// 全部读取/写入都失败的后端（验证错误路径与"已是该后端"的判断）。
    pub fn all_fail(name: &'static str, preference: BackendPreference) -> Self {
        Self {
            name,
            preference,
            read_fails: true,
            set_charge_limit_fails: true,
            set_battery_care_fails: true,
            set_perf_fails: true,
            ..Default::default()
        }
    }

    /// 模拟"限值寄存器可写、养护位被拒绝"的 EC：读全失败，仅
    /// `set_charge_limit` 成功。
    pub fn partial_care(name: &'static str) -> Self {
        Self {
            name,
            read_fails: true,
            set_battery_care_fails: true,
            set_perf_fails: true,
            ..Default::default()
        }
    }

    /// 模拟 WMI 量化：`set_charge_limit` 就近取预设值。
    pub fn quantizing() -> Self {
        Self {
            charge_limit: Arc::new(AtomicU8::new(100)),
            perf_mode: Arc::new(AtomicU8::new(0x09)),
            quantize: true,
            ..Default::default()
        }
    }

    /// 模拟"充电上限写入被固件拒绝"的后端：仅 `set_charge_limit` 失败。
    pub fn charge_limit_fails() -> Self {
        Self {
            charge_limit: Arc::new(AtomicU8::new(100)),
            set_charge_limit_fails: true,
            ..Default::default()
        }
    }

    fn fail(&self) -> EcError {
        EcError::BackendUnavailable(format!("{} 拒绝操作", self.name))
    }
}

impl EcBackend for MockBackend {
    fn name(&self) -> &'static str {
        self.name
    }

    fn preference(&self) -> BackendPreference {
        self.preference
    }

    fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
        if self.read_fails {
            return Err(self.fail());
        }
        Ok(self.battery_care.load(Ordering::Relaxed))
    }

    fn get_charge_limit(&self) -> Result<u8, EcError> {
        if self.read_fails {
            return Err(self.fail());
        }
        Ok(self.charge_limit.load(Ordering::Relaxed))
    }

    fn set_battery_care(&self, enabled: bool) -> Result<(), EcError> {
        if self.set_battery_care_fails {
            return Err(self.fail());
        }
        self.battery_care.store(enabled, Ordering::Relaxed);
        Ok(())
    }

    fn set_charge_limit(&self, percent: u8) -> Result<(), EcError> {
        if self.set_charge_limit_fails {
            return Err(self.fail());
        }
        let pct = if self.quantize {
            crate::ec::battery::nearest_wmi_percent(percent.min(100))
        } else {
            percent.min(100)
        };
        self.charge_limit.store(pct, Ordering::Relaxed);
        Ok(())
    }

    fn get_battery_state(&self) -> Result<(bool, u8), EcError> {
        self.battery_state_calls.fetch_add(1, Ordering::Relaxed);
        if self.read_fails {
            return Err(self.fail());
        }
        Ok((
            self.battery_care.load(Ordering::Relaxed),
            self.charge_limit.load(Ordering::Relaxed),
        ))
    }

    fn get_performance_mode(&self) -> Result<u8, EcError> {
        if self.read_fails {
            return Err(self.fail());
        }
        Ok(self.perf_mode.load(Ordering::Relaxed))
    }

    fn set_performance_mode(&self, mode: u8) -> Result<(), EcError> {
        if self.set_perf_fails {
            return Err(self.fail());
        }
        self.perf_mode.store(mode, Ordering::Relaxed);
        Ok(())
    }

    fn supports_continuous_charge_limit(&self) -> bool {
        !self.quantize
    }
}
