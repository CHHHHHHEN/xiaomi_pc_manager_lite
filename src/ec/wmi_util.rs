use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::System::Com::{
    CoCreateInstance, CoSetProxyBlanket, CLSCTX_INPROC_SERVER, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
    RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Wmi::{IWbemContext, IWbemLocator, IWbemServices};
use windows::core::{BSTR, GUID, PCWSTR};

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
