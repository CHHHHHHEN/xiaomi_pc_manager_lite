//! Windows COM/WMI 基础设施：公寓生命周期、连接、查询与 SAFEARRAY 工具。
//!
//! 本模块是 **最低层的 Windows 互操作**（除 `util` 外无内部依赖），供硬件
//! 适配器（`ec`）与平台集成（`platform`）共用。历史实现位于 crate 根的
//! `wmi_util.rs`——crate 根被当作一个没有归属的"第四层"存放共享基础设施，
//! 且 `ec` 与 `platform` 两层的共享依赖无法在依赖图中表达。收敛到 `win::com`
//! 后，`ec` 与 `platform` 都依赖 `win`（单向、无环）。

use crate::util::err_fmt;
use windows::core::{BSTR, GUID, PCWSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
    SAFEARRAY,
};
use windows::Win32::System::Ole::{SafeArrayGetLBound, SafeArrayGetUBound};
use windows::Win32::System::Wmi::{
    IEnumWbemClassObject, IWbemClassObject, IWbemContext, IWbemLocator, IWbemServices,
    WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY,
};

/// 当前线程 COM 公寓（MTA）初始化的 RAII 作用域：`init()` 后本线程进入 MTA，
/// `Drop` 时自动 `CoUninitialize` 配对回收。
///
/// 历史实现（autostart.rs、battery_health.rs）各自手写 `CoInitializeEx` +
/// `CoUninitialize` 配对——battery_health 的 `poll_loop` 在 `poll_connected`
/// panic 时跳过 `CoUninitialize`，每轮 panic 泄漏一次公寓引用计数，且两处
/// 代码重复。统一收敛到 `win::com`（修订 1.46 审计）：
/// - `autostart.rs` 直接复用，删其私有 `ComScope`；
/// - `battery_health.rs` 在 catch_unwind 包裹的 poll_loop 里用本作用域，
///   panic 展开时 Drop 自动执行 CoUninitialize。
pub struct ComScope;

impl ComScope {
    pub fn init() -> Result<Self, String> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| err_fmt("CoInitializeEx", e))?;
        }
        Ok(Self)
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

/// CLSID_WbemAdministrativeLocator ({CB8555CC-9128-11D1-AD9B-00C04FD8FDFF}).
/// The classic CLSID_WbemLocator ({DC12A687-...}) is missing on newer
/// Windows Insider builds; this administrative locator is registered on
/// all WMI-capable systems and supports IWbemLocator.
pub const CLSID_WMI_LOCATOR: GUID = GUID::from_u128(0xCB8555CC_9128_11D1_AD9B_00C04FD8FDFF);

const RPC_C_AUTHN_WINNT: u32 = 10u32;
const RPC_C_AUTHZ_NONE: u32 = 0u32;

/// 连接 `root\wmi` 并设置代理绑定（CoSetProxyBlanket），返回 `IWbemServices`。
///
/// wmi.rs（EC 后端）与 fnkey.rs（Fn+K 事件监听）各自重复实现过同样的一套
/// 连接样板（CoCreateInstance → ConnectServer → CoSetProxyBlanket），统一收敛
/// 到此处。调用方必须已在本线程初始化 COM（MTA）。
pub fn connect_root_wmi() -> Result<IWbemServices, String> {
    let locator: IWbemLocator = unsafe {
        CoCreateInstance(&CLSID_WMI_LOCATOR, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| err_fmt("CoCreateInstance", e))?
    };

    let services = unsafe {
        locator
            .ConnectServer(
                &BSTR::from("root\\wmi"),
                &BSTR::new(),
                &BSTR::new(),
                &BSTR::new(),
                0,
                &BSTR::new(),
                None::<&IWbemContext>,
            )
            .map_err(|e| err_fmt("ConnectServer root\\wmi", e))?
    };

    unsafe {
        CoSetProxyBlanket(
            &services,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            PCWSTR(std::ptr::null()),
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )
        .map_err(|_| "CoSetProxyBlanket failed".to_string())?
    };

    Ok(services)
}

/// 读取 SAFEARRAY 的 1 维上下界（含两端）。
///
/// 历史实现 `SafeArrayGetLBound/UBound` 查询失败时以 `unwrap_or(0/-1)`
/// 静默伪造 `(0, -1)` 边界——调用方把它当成"空数组"继续走后续逻辑，
/// 真实 COM 错误被吞掉。查询失败与"数组为空"是不同场景，必须显式暴露，
/// 由调用方决定如何记录/回退。
///
/// # Safety
///
/// `sa` 必须指向一个有效分配的 `SAFEARRAY`（或为 `null`，此时返回 `Err`），
/// 且其生命周期必须覆盖本调用；调用期间不得被其它线程并发销毁。
pub unsafe fn safe_array_bounds(sa: *const SAFEARRAY) -> Result<(i32, i32), String> {
    let lbound =
        unsafe { SafeArrayGetLBound(sa, 1) }.map_err(|e| err_fmt("SafeArrayGetLBound", e))?;
    let ubound =
        unsafe { SafeArrayGetUBound(sa, 1) }.map_err(|e| err_fmt("SafeArrayGetUBound", e))?;
    Ok((lbound, ubound))
}

/// 读取 SAFEARRAY 的 1 维元素数量（`ubound - lbound + 1`，含两端）。
///
/// wmi.rs（schema 属性名、响应数组）与 fnkey.rs（EventDetail 数组）各自重复
/// 书写过同一句 `ubound.saturating_sub(lbound).saturating_add(1)`——统一收敛
/// 到此处。边界查询失败（真实 COM 错误）由底层 `safe_array_bounds` 显式
/// 上抛，调用方按各自场景清理/回退。
///
/// # Safety
///
/// 与 [`safe_array_bounds`] 相同：`sa` 必须指向有效的 `SAFEAR`（或为 `null`），
/// 且生命周期覆盖本调用。
pub unsafe fn safe_array_len(sa: *const SAFEARRAY) -> Result<usize, String> {
    let (lbound, ubound) = unsafe { safe_array_bounds(sa) }?;
    Ok(ubound.saturating_sub(lbound).saturating_add(1) as usize)
}

/// 执行 WQL 查询（`SELECT * FROM ...`）并返回枚举器。
///
/// 统一使用 `WBEM_FLAG_RETURN_IMMEDIATELY | WBEM_FLAG_FORWARD_ONLY`（只读
/// 快照查询的常规组合）。wmi.rs（枚举 `MICommonInterface` 实例）与
/// battery_health.rs（容量/充放状态类查询）各自重复书写同一套 `ExecQuery`
/// 参数——收敛到此处，错误映射仍由调用方按各自的类型完成。
pub fn exec_query(
    services: &IWbemServices,
    wql: &str,
) -> Result<IEnumWbemClassObject, windows::core::Error> {
    unsafe {
        services.ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from(wql),
            WBEM_FLAG_RETURN_IMMEDIATELY | WBEM_FLAG_FORWARD_ONLY,
            None::<&IWbemContext>,
        )
    }
}

/// 全表查询的 WQL 语句（`SELECT * FROM <class>`）构造点（修订 1.50 收敛）。
///
/// fn_watcher.rs（`ExecNotificationQuery` 事件订阅）与 battery_health.rs
/// （`ExecQuery` 快照查询）此前各自手写同一句 `format!("SELECT * FROM {}",
/// class)`——WQL 形状一旦要统一演化（如追加过滤子句）需同步多处。类名由
/// 调用方校验（`app::fnkey::valid_class` 保证合法 WQL 标识符，无注入面）。
pub fn select_all_wql(class: &str) -> String {
    format!("SELECT * FROM {}", class)
}

/// 从枚举器取**下一个**实例（单槽 `Next`，一次一条）。
///
/// - `Ok(Some(obj))` = 取到一条实例；
/// - `Ok(None)` = 本轮未取到（枚举耗尽，或提供程序异常返回 `returned > 0`
///   但槽位为空的病态情形——按"本轮无数据"处理并告警，不无限重试）；
/// - `Err` = `Next` 调用本身失败（连接失效等瞬态错误）。
///
/// wmi.rs（`resolve_target` 枚举实例）、battery_health.rs（容量/状态类首个
/// 实例）与 fn_watcher.rs（事件轮询）各自重复书写过同一套单槽 `Next`
/// 样板（槽数组、`returned` 计数、超时），统一收敛到此处。`timeout_ms` 为
/// 单次阻塞等待毫秒数（WQL 快照查询用 500；事件轮询用 100）。
///
/// # Safety
///
/// `enumerator` 必须指向一个在本线程（或按其 COM 线程亲和约定）有效的
/// `IEnumWbemClassObject`，且其生命周期覆盖本调用；WMI 连接存活期间不得
/// 被其它线程并发使用。
pub unsafe fn next_instance(
    enumerator: &IEnumWbemClassObject,
    timeout_ms: i32,
) -> Result<Option<IWbemClassObject>, windows::core::Error> {
    let mut objects: [Option<IWbemClassObject>; 1] = [None];
    let mut returned: u32 = 0;
    let hr = unsafe { enumerator.Next(timeout_ms, &mut objects, &mut returned as *mut u32) };
    // Next 在 windows-rs 返回 HRESULT（非 Result）：失败时转成 Error 上抛。
    if hr.is_err() {
        return Err(windows::core::Error::from_hresult(hr));
    }
    if returned == 0 {
        return Ok(None);
    }
    // 防御病态提供程序（修订 1.46）：`returned > 0` 但槽位为空时不能
    // 无限循环——按"本轮无数据"处理（返回 None，由调用方决定继续/停止），
    // 告警让异常可见。历史实现（battery_health）会在此反复阻塞 500ms。
    let Some(obj) = objects[0].take() else {
        log::warn!(
            "WMI Next returned {} but empty slot; treating as no data",
            returned
        );
        return Ok(None);
    };
    Ok(Some(obj))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SAFEARRAY 边界查询（统一收敛点）：真实数组返回含两端的 (lbound, ubound)
    /// （1 维下界 0，32 元素 → 0..=31）；非法指针显式报错——历史实现
    /// `unwrap_or((0,-1))` 会把失败静默当成"空数组"，此处必须可测地失败。
    #[test]
    fn test_safe_array_bounds() {
        use windows::Win32::System::Ole::SafeArrayCreateVector;
        use windows::Win32::System::Variant::VT_UI1;
        unsafe {
            let sa = SafeArrayCreateVector(VT_UI1, 0, 32);
            assert!(!sa.is_null(), "SafeArrayCreateVector must succeed");
            let (lbound, ubound) = safe_array_bounds(sa).expect("bounds query must succeed");
            assert_eq!((lbound, ubound), (0, 31));
            let _ = windows::Win32::System::Ole::SafeArrayDestroy(sa);

            // 非法指针：显式失败而非伪造边界。
            assert!(safe_array_bounds(std::ptr::null()).is_err());
        }
    }
}
