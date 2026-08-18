use windows::core::{BSTR, GUID, PCWSTR};
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
    SAFEARRAY,
};
use windows::Win32::System::Ole::{SafeArrayGetLBound, SafeArrayGetUBound};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_BSTR, VT_I4};
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
/// 代码重复。统一收敛到此处（修订 1.46 审计）：
/// - `autostart.rs` 直接复用，删其私有 `ComScope`；
/// - `battery_health.rs` 在 catch_unwind 包裹的 poll_loop 里用本作用域，
///   panic 展开时 Drop 自动执行 CoUninitialize。
pub struct ComScope;

impl ComScope {
    pub fn init() -> Result<Self, String> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| format!("CoInitializeEx: {}", e))?;
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
            .map_err(|e| format!("CoCreateInstance: {}", e))?
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
            .map_err(|e| format!("ConnectServer root\\wmi: {}", e))?
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
pub unsafe fn safe_array_bounds(sa: *const SAFEARRAY) -> Result<(i32, i32), String> {
    let lbound =
        unsafe { SafeArrayGetLBound(sa, 1) }.map_err(|e| format!("SafeArrayGetLBound: {}", e))?;
    let ubound =
        unsafe { SafeArrayGetUBound(sa, 1) }.map_err(|e| format!("SafeArrayGetUBound: {}", e))?;
    Ok((lbound, ubound))
}

/// 读取 SAFEARRAY 的 1 维元素数量（`ubound - lbound + 1`，含两端）。
///
/// wmi.rs（schema 属性名、响应数组）与 fnkey.rs（EventDetail 数组）各自重复
/// 书写过同一句 `ubound.saturating_sub(lbound).saturating_add(1)`——统一收敛
/// 到此处。边界查询失败（真实 COM 错误）由底层 `safe_array_bounds` 显式
/// 上抛，调用方按各自场景清理/回退。
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

/// VARIANT 的 RAII 包装：Drop 时自动释放（委托给 VARIANT 自身的 Drop）。
///
/// windows-rs 0.62 的 `VARIANT` **实现了 Drop**（扩展模块中自动
/// `VariantClear`，见 windows-rs 源码 `extensions/Win32/System/Variant.rs`）。
/// 历史实现误以为它"不实现 Drop"，于是包一层 `OwnedVariant` 再手动
/// `VariantClear`——同一 VARIANT 被清两次：先由本包装清、再把（此时已为
/// VT_EMPTY 的）VARIANT 交给其自身 Drop 清一次。第二次清空是 no-op，无害，
/// 但重复清理掩盖了真实语义、且错误注释会诱导后续维护者"补一个
/// ManuallyDrop"或误删清理。修复：本包装**不再手动清零**，只依赖 VARIANT
/// 自带的 Drop（唯一清理路径），注释与实现一致。持有者只管借用读取。
pub struct OwnedVariant(VARIANT);

impl OwnedVariant {
    pub fn new(v: VARIANT) -> Self {
        Self(v)
    }
}

impl std::ops::Deref for OwnedVariant {
    type Target = VARIANT;
    fn deref(&self) -> &VARIANT {
        &self.0
    }
}

// 无需显式 impl Drop：VARIANT 自身 Drop 会 VariantClear，此处保持默认。

/// 从 WMI 对象按名读取属性值（`IWbemClassObject::Get`）。
///
/// fnkey.rs 与 wmi.rs 各自重复实现过同样的样板（to_pcwstr → Get → 取
/// VARIANT），统一收敛到此处。返回的 VARIANT 由 `OwnedVariant` 在 Drop 时
/// 自动释放，调用方无需手动 `VariantClear`。
///
/// 属性缺失（`WBEM_E_NOT_FOUND`）时返回 None——这是各类事件对象属性可选
/// 的**正常**情形（如 `EventDetail` 与 `ReportHex` 二选一）。其它 `Get`
/// 失败（`WBEM_E_ACCESS_DENIED`/provider 错误等）说明对象本身或提供程序
/// 异常，历史上被 `.ok()?` 静默吞掉、与"属性不存在"无法区分，真实故障
/// 永远不可见——此处记录 warn 日志（仅此，不改变返回语义，调用方仍按
/// None 处理；异常值本身不携带可恢复信息，告警足以排查）。
pub fn get_property(obj: &IWbemClassObject, name: &str) -> Option<OwnedVariant> {
    let wname = crate::util::WideString::new(name);
    let mut val = VARIANT::default();
    let mut _type = 0i32;
    let mut _flavor = 0i32;
    let hr = unsafe {
        obj.Get(
            wname.as_pcwstr(),
            0,
            &mut val,
            Some(&mut _type as *mut i32),
            Some(&mut _flavor as *mut i32),
        )
    };
    if let Err(e) = hr {
        // WBEM_E_NOT_FOUND (0x80041002)：属性缺失，正常路径（部分事件对象
        // 只带 ReportHex 不带 EventDetail 等），静默返回 None。
        if (e.code().0 as u32) != (windows::Win32::System::Wmi::WBEM_E_NOT_FOUND.0 as u32) {
            log::warn!("WMI Get({}) failed: {}", name, e);
        }
        return None;
    }
    Some(OwnedVariant::new(val))
}

pub unsafe fn bstr_from_variant(val: &VARIANT) -> Option<String> {
    let vt = val.Anonymous.Anonymous.vt.0;
    if vt != VT_BSTR.0 {
        return None;
    }
    // Take the address of the union member instead of forming a reference to
    // its possibly-null value; BSTR's Deref handles the null case safely.
    let bstr = &*std::ptr::addr_of!(val.Anonymous.Anonymous.Anonymous.bstrVal);
    if bstr.is_empty() {
        return None;
    }
    Some(String::from_utf16_lossy(bstr))
}

/// 从 WMI 对象读取字符串属性（属性缺失/类型不符时返回 None）。
///
/// fnkey.rs（ReportHex）与 wmi.rs（InstanceName 等）各自重复实现过同样的
/// `get_property + bstr_from_variant` 两步样板，统一收敛到此处。
pub fn get_string_prop(obj: &IWbemClassObject, name: &str) -> Option<String> {
    let val = get_property(obj, name)?;
    unsafe { bstr_from_variant(&val) }
}

/// 从 WMI 对象读取布尔属性（属性缺失/类型不符时返回 None）。
pub fn get_bool_prop(obj: &IWbemClassObject, name: &str) -> Option<bool> {
    let val = get_property(obj, name)?;
    unsafe { bool_from_variant(&val) }
}

/// 从 WMI 对象读取 u32 属性（属性缺失/类型不符/类型不对时返回 None）。
/// 见 `uint_from_variant` 的类型说明（真机 root\WMI 电池容量返回 VT_I4）。
pub fn uint_prop(obj: &IWbemClassObject, name: &str) -> Option<u32> {
    let val = get_property(obj, name)?;
    unsafe { uint_from_variant(&val) }
}

pub unsafe fn bool_from_variant(val: &VARIANT) -> Option<bool> {
    let vt = val.Anonymous.Anonymous.vt.0;
    if vt != VT_BOOL.0 {
        return None;
    }
    Some(val.Anonymous.Anonymous.Anonymous.boolVal != VARIANT_BOOL(0))
}

/// 读取整数属性（电池容量等 uint32 字段）为 u32。
///
/// WMI 提供程序对 uint32 属性的变体类型并不统一：
/// - `root\WMI` 的 ACPI 电池类（`BatteryStaticData.DesignedCapacity` /
///   `BatteryFullChargedCapacity.FullChargedCapacity`）在
///   **2025 RedmiBook Pro 14 实测返回 `VT_I4`（3）**——以有符号整型承载
///   非负数值（本机 DesignedCapacity=76990、FullChargedCapacity=77255）；
/// - 其它提供程序可能返回 `VT_UI4`（19）或 `VT_UINT`（22）。
///
/// 三种类型统一在此收敛：`VT_I4` 取 `lVal`（负数按"不可读"处理，容量类
/// 字段不可能为负）；`VT_UI4`/`VT_UINT` 取 `ulVal`。其它类型（字符串/
/// 布尔/浮点）返回 None，由调用方按"属性不可读"处理。
pub unsafe fn uint_from_variant(val: &VARIANT) -> Option<u32> {
    let vt = val.Anonymous.Anonymous.vt.0;
    if vt == VT_I4.0 {
        let l = val.Anonymous.Anonymous.Anonymous.lVal;
        return (l >= 0).then_some(l as u32);
    }
    if vt != windows::Win32::System::Variant::VT_UI4.0
        && vt != windows::Win32::System::Variant::VT_UINT.0
    {
        return None;
    }
    Some(val.Anonymous.Anonymous.Anonymous.ulVal)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 属性变体类型三态（真机实证：root\WMI 电池容量返回 VT_I4）：
    /// VT_I4（正数）→ u32；VT_UI4 / VT_UINT → u32；VT_I4 负数 → None；
    /// 其它类型 → None。VARIANT 由 windows crate 的 From 构造。
    #[test]
    fn test_uint_from_variant() {
        unsafe {
            // VT_I4 正数（本机 BatteryStaticData.DesignedCapacity 实测类型）。
            assert_eq!(uint_from_variant(&VARIANT::from(76990i32)), Some(76990));
            // VT_UI4 / VT_UINT（u32 构造）。
            assert_eq!(uint_from_variant(&VARIANT::from(77255u32)), Some(77255));
            assert_eq!(uint_from_variant(&VARIANT::from(123u32)), Some(123));
            // VT_I4 负数：容量不可能为负，按"不可读"处理。
            assert_eq!(uint_from_variant(&VARIANT::from(-1i32)), None);
            // VT_EMPTY / 未初始化：类型不符。
            assert_eq!(uint_from_variant(&VARIANT::default()), None);
        }
    }

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
