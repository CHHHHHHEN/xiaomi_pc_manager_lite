use windows::core::{BSTR, GUID, PCWSTR};
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::System::Com::{
    CoCreateInstance, CoSetProxyBlanket, CLSCTX_INPROC_SERVER, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
    RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Wmi::{IWbemClassObject, IWbemContext, IWbemLocator, IWbemServices};

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
    if vt != 8 {
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

pub unsafe fn bool_from_variant(val: &VARIANT) -> Option<bool> {
    let vt = val.Anonymous.Anonymous.vt.0;
    if vt != 11 {
        return None;
    }
    Some(val.Anonymous.Anonymous.Anonymous.boolVal != VARIANT_BOOL(0))
}
