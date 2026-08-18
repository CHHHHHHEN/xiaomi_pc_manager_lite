//! 共享的内存测试后端（仅测试编译）。
//!
//! `ec::battery`、`startup`、`gui::commands` 的测试各自重复实现了多个
//! 仅操作成败/量化行为不同的 mock `EcBackend`（历史累计 9+ 份、约 450 行），
//! 存在漂移风险。统一收敛到此处：单一可配置后端经字段区分行为，测试断言
//! 通过 `Arc` 原子暴露的内部状态完成。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

use crate::app::config::BackendPreference;
use crate::app::ec::{EcBackend, EcError};

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
    /// 养护位**写入请求**的落地记录（`set_battery_care` 写入，`care_write_is_noop`
    /// 时不写入）。注意：**读侧**（get_battery_care_enabled / get_battery_state）
    /// 一律按 `charge_limit < 100` 推导，与真实后端（winring0/wmi）契约一致——
    /// 本字段仅供测试断言"写养护位请求到达后端"（修订 1.47 审计）。
    pub battery_care: Arc<AtomicBool>,
    pub perf_mode: Arc<AtomicU8>,
    /// `get_battery_state` 调用计数（回归测试：刷新必须单次往返）。
    pub battery_state_calls: Arc<AtomicU32>,
    pub name: &'static str,
    pub preference: BackendPreference,
    pub quantize: bool,
    /// `Arc<AtomicBool>`：测试在 clone 后仍能翻转读失败开关（NFR-REL-03
    /// 验证"失败达阈值暂停、成功恢复后清零"需要共享可变的成败行为）。
    pub read_fails: Arc<AtomicBool>,
    pub set_charge_limit_fails: bool,
    pub set_battery_care_fails: bool,
    pub set_perf_fails: bool,
    /// 模拟 WMI 的养护位契约 no-op：`set_battery_care` 返回 Ok 但**不落地**
    /// （读回恒为 false）。启动同步必须不被这种固件误导（L5 回归：不能把
    /// care=false 写进配置，否则下次启动按 care=false 强制 100%）。
    pub care_write_is_noop: bool,
    /// 模拟 WMI 后端熔断（`needs_rebuild() == true`）：死 worker/超时熔断后
    /// 只能靠重建恢复（修订 1.45 的 WMI 熔断自动恢复测试用）。
    pub needs_rebuild: bool,
}

impl Default for MockBackend {
    fn default() -> Self {
        Self {
            // 初始化为合法值 100%（养护关闭）：真实后端（winring0 / wmi）的
            // get_charge_limit 把 0/>100 判为垃圾值返回 Err——mock 默认 0 会让
            // 直接读默认实例的测试观察到 (false, 0)，与真机契约背离（修订 1.46
            // 审计）。100% 是每个机器都合法存在的状态（未开启养护）。
            charge_limit: Arc::new(AtomicU8::new(100)),
            battery_care: Arc::new(AtomicBool::new(false)),
            perf_mode: Arc::new(AtomicU8::new(0x09)),
            battery_state_calls: Arc::new(AtomicU32::new(0)),
            name: "mock",
            preference: BackendPreference::Auto,
            quantize: false,
            read_fails: Arc::new(AtomicBool::new(false)),
            set_charge_limit_fails: false,
            set_battery_care_fails: false,
            set_perf_fails: false,
            care_write_is_noop: false,
            needs_rebuild: false,
        }
    }
}

impl MockBackend {
    /// 全部读取/写入都失败的后端（验证错误路径与"已是该后端"的判断）。
    pub fn all_fail(name: &'static str, preference: BackendPreference) -> Self {
        Self {
            name,
            preference,
            read_fails: Arc::new(AtomicBool::new(true)),
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
            read_fails: Arc::new(AtomicBool::new(true)),
            set_battery_care_fails: true,
            set_perf_fails: true,
            ..Default::default()
        }
    }

    /// 模拟 WMI 量化：`set_charge_limit` 就近取预设值。
    pub fn quantizing() -> Self {
        Self {
            quantize: true,
            ..Default::default()
        }
    }

    /// 模拟"充电上限写入被固件拒绝"的后端：仅 `set_charge_limit` 失败。
    pub fn charge_limit_fails() -> Self {
        Self {
            set_charge_limit_fails: true,
            ..Default::default()
        }
    }

    fn fail(&self) -> EcError {
        EcError::BackendUnavailable(format!("{} 拒绝操作", self.name))
    }

    /// 读开关的统一前置检查（三个 getter 曾各自重复 `read_fails` 判定，
    /// 修订 1.47 清理）。
    fn ensure_readable(&self) -> Result<(), EcError> {
        if self.read_fails.load(Ordering::Relaxed) {
            return Err(self.fail());
        }
        Ok(())
    }

    /// 读回原始值的合法性校验（与真实后端同一读回契约，见
    /// `get_charge_limit` 的注释）：`0` 与 `>FULL_CHARGE_LIMIT` 是垃圾值。
    /// 三个 getter 曾各自书写同一份 `if raw == 0 || raw > 100` 判定。
    fn validate_read_raw(raw: u8) -> Result<u8, EcError> {
        if raw == 0 || raw > crate::app::limits::FULL_CHARGE_LIMIT {
            return Err(EcError::InvalidData(format!(
                "充电上限 mock 值 {} 非法",
                raw
            )));
        }
        Ok(raw)
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
        self.ensure_readable()?;
        // 与真实后端同款推导契约（winring0.rs / wmi.rs 的 get_battery_care_enabled
        // 均为 care = 充电上限 < 100%，见 care_enabled_from_limit）：mock 直接
        // 返回存储位会让"写限值不改养护位 → 读回 care=false"的测试在 CI 通过、
        // 真机上行为不同（修订 1.47 审计，与 get_charge_limit 的垃圾值契约
        // 对齐同源）。`battery_care` 原子仍保留：测试用它断言"写养护位请求
        // 到达后端"（set_battery_care 的落地），读侧一律按限值推导。
        let raw = Self::validate_read_raw(self.charge_limit.load(Ordering::Relaxed))?;
        Ok(crate::app::battery::care_enabled_from_limit(raw))
    }

    fn get_charge_limit(&self) -> Result<u8, EcError> {
        self.ensure_readable()?;
        // 与真实后端（winring0 / wmi 的 get_charge_limit）同一读回契约：0 与
        // >100 是垃圾值，必须返回 Err 而非 Ok——mock 返回 Ok(0) 会让"读回
        // 垃圾值"类测试在 CI 通过、真机上行为不同（修订 1.46 审计，与写入
        // 路径的 validate_charge_limit_write 对称）。校验收敛在
        // `validate_read_raw`（修订 1.47 清理）。
        Self::validate_read_raw(self.charge_limit.load(Ordering::Relaxed))
    }

    fn set_battery_care(&self, enabled: bool) -> Result<(), EcError> {
        if self.set_battery_care_fails {
            return Err(self.fail());
        }
        if !self.care_write_is_noop {
            self.battery_care.store(enabled, Ordering::Relaxed);
        }
        Ok(())
    }

    fn set_charge_limit(&self, percent: u8) -> Result<(), EcError> {
        if self.set_charge_limit_fails {
            return Err(self.fail());
        }
        // 与真实后端（winring0 / wmi 的 set_charge_limit）走同一份写入前校验：
        // 0 非法（读回契约把 0 判为垃圾值）、>100 钳到 100。mock 放行 0 会让
        // 针对"写入 0 必须失败"的测试在 CI 通过、真机上行为不同（测试漂移）。
        let pct = crate::app::battery::validate_charge_limit_write(percent)?;
        let pct = if self.quantize {
            crate::app::battery::nearest_wmi_percent(pct)
        } else {
            pct
        };
        self.charge_limit.store(pct, Ordering::Relaxed);
        Ok(())
    }

    fn get_battery_state(&self) -> Result<(bool, u8), EcError> {
        self.battery_state_calls.fetch_add(1, Ordering::Relaxed);
        self.ensure_readable()?;
        let limit = Self::validate_read_raw(self.charge_limit.load(Ordering::Relaxed))?;
        // 养护位同样按限值推导（修订 1.47 审计，对齐 winring0/wmi 的
        // get_battery_state：care = limit < 100），见 get_battery_care_enabled。
        Ok((crate::app::battery::care_enabled_from_limit(limit), limit))
    }

    fn get_performance_mode(&self) -> Result<u8, EcError> {
        if self.read_fails.load(Ordering::Relaxed) {
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

    fn needs_rebuild(&self) -> bool {
        self.needs_rebuild
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归测试（mock/真实后端契约对齐）：写入 0% 必须像真实后端一样被
    /// `validate_charge_limit_write` 拒绝。历史 mock 用 `percent.min(100)`
    /// 放行 0，导致"写入 0 必须失败"类测试在 CI 通过、真机上行为不同。
    #[test]
    fn test_mock_set_charge_limit_rejects_zero() {
        let backend = MockBackend::default();
        assert!(
            backend.set_charge_limit(0).is_err(),
            "0% must be rejected before reaching the store"
        );
        // 写入被拒绝，内部状态保持默认合法值 100（未被 0 污染）。
        assert_eq!(backend.charge_limit.load(Ordering::Relaxed), 100);
        backend.set_charge_limit(80).unwrap();
        assert_eq!(backend.charge_limit.load(Ordering::Relaxed), 80);
        assert_eq!(backend.get_charge_limit().unwrap(), 80);
    }

    /// 回归测试（修订 1.46 审计）：mock 的**读回**也要像真实后端一样拒绝
    /// 垃圾值（0/>100 返回 Err）——只约束写入会让"读回垃圾值"类测试在 CI
    /// 通过、真机上行为不同。显式把内部状态改成垃圾值模拟"读回损坏的 EC
    /// 寄存器"。
    #[test]
    fn test_mock_read_rejects_invalid_charge_limit() {
        let backend = MockBackend::default();
        // 默认 100%（合法），读回正常。
        assert_eq!(backend.get_charge_limit().unwrap(), 100);
        // 显式注入 0（模拟寄存器损坏）：读回必须 Err。
        backend.charge_limit.store(0, Ordering::Relaxed);
        assert!(backend.get_charge_limit().is_err());
        assert!(backend.get_battery_state().is_err());
        // 修复后恢复可读。
        backend.set_charge_limit(60).unwrap();
        assert_eq!(backend.get_charge_limit().unwrap(), 60);
        // 养护位由限值推导（修订 1.47 审计）：60 < 100 → care=true，与
        // 真实后端（winring0/wmi）的 get_battery_state 语义一致。
        assert_eq!(backend.get_battery_state().unwrap(), (true, 60));
    }
}
