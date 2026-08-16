//! WMI EC backend — MICommonInterface.MiInterface protocol

use super::backend::EcBackend;
use super::battery;
use super::error::EcError;
use super::addr as ec_addr;
use std::sync::OnceLock;

use windows::Win32::System::Com::{
    CoInitializeEx, CoSetProxyBlanket, CoCreateInstance, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Ole::SafeArrayCreateVector;
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetElement,
    SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayUnaccessData,
};
use windows::Win32::System::Variant::{VARIANT, VARENUM, VT_ARRAY, VT_UI1, VariantClear};
use windows::Win32::System::Wmi::*;
use windows::core::{BSTR, GUID, PCWSTR};
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;

/// CLSID_WbemAdministrativeLocator ({CB8555CC-9128-11D1-AD9B-00C04FD8FDFF}).
/// The classic CLSID_WbemLocator ({DC12A687-...}) is missing on newer
/// Windows Insider builds; this administrative locator is registered on
/// all WMI-capable systems and supports IWbemLocator.
const CLSID_WMI_LOCATOR: GUID = GUID::from_u128(0xCB8555CC_9128_11D1_AD9B_00C04FD8FDFF);

const RPC_C_AUTHN_WINNT: u32 = 10u32;
const RPC_C_AUTHZ_NONE: u32 = 0u32;

/// MiInterface command constants (little-endian bytes)
const CMD_READ: u16 = 0xFA00;
const CMD_WRITE: u16 = 0xFB00;
const FUN2_BATTERY: u16 = 0x1000;
const FUN2_PERF: u16 = 0x0800;

static COM_INIT: OnceLock<Result<(), EcError>> = OnceLock::new();

fn ensure_com() -> Result<(), EcError> {
    COM_INIT.get_or_init(|| {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let ok = hr.is_ok() || hr.0 == RPC_E_CHANGED_MODE.0;
        if hr.is_ok() {
            log::info!("COM initialized (MTA)");
        } else if hr.0 == RPC_E_CHANGED_MODE.0 {
            log::warn!("COM already initialized with different mode; proceeding");
        }
        if ok {
            Ok(())
        } else {
            let err = EcError::WmiConnect(format!("COM init: {}", hr));
            log::error!("COM init failed: {}", err);
            Err(err)
        }
    })
    .clone()
}

fn to_le16(buf: &mut [u8; 32], offset: usize, val: u16) {
    buf[offset] = (val & 0xFF) as u8;
    buf[offset + 1] = ((val >> 8) & 0xFF) as u8;
}

/// 将百分比换算为 WMI 充电上限 raw code：精确匹配优先，否则取最近的预设值。
fn wmi_rawcode_for_percent(percent: u8) -> u8 {
    battery::percent_to_wmi_rawcode(percent)
        .or_else(|| battery::percent_to_wmi_rawcode(battery::nearest_wmi_percent(percent)))
        .unwrap_or(0)
}

pub struct WmiBackend {
    services: IWbemServices,
}

// SAFETY: WmiBackend wraps an IWbemServices COM pointer that was created
// under MTA. All calls go through that same apartment via &self, and
// IWbemServices is thread-safe under MTA (the proxy/stub layer handles
// concurrency via COM's internal mechanisms).
unsafe impl Send for WmiBackend {}
unsafe impl Sync for WmiBackend {}

impl WmiBackend {
    pub fn new() -> Result<Self, EcError> {
        unsafe {
            ensure_com()?;

            let locator: IWbemLocator = CoCreateInstance(&CLSID_WMI_LOCATOR, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| EcError::WmiConnect(format!("CoCreateInstance: {}", e)))?;

            let services = locator
                .ConnectServer(
                    &BSTR::from("root\\wmi"),
                    &BSTR::new(),
                    &BSTR::new(),
                    &BSTR::new(),
                    0,
                    &BSTR::new(),
                    None::<&IWbemContext>,
                )
                .map_err(|e| EcError::WmiConnect(format!("ConnectServer: {}", e)))?;

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
            .map_err(|_| EcError::WmiConnect("CoSetProxyBlanket failed".into()))?;

            Ok(Self { services })
        }
    }

    /// Discover the first user-defined property name from a WMI class/instance
    /// object.  Xiaomi EC firmware revisions use different parameter names
    /// across models (e.g. InData/OutData, InParam/OutParam).  Reading the
    /// actual name from the schema avoids hardcoded assumptions.
    ///
    /// GetNames() with no qualifier returns ALL properties including system
    /// properties (__GENUS, __CLASS, ...), which always come first, and the
    /// output parameter class additionally carries the method return value
    /// (ReturnValue / ReturnCode).  Both kinds must be skipped so the first
    /// user parameter (InData / OutData) is picked.
    unsafe fn param_name_from_schema(obj: &IWbemClassObject, fallback: &str) -> String {
        let is_return_value_prop = |name: &str| {
            name.eq_ignore_ascii_case("ReturnValue") || name.eq_ignore_ascii_case("ReturnCode")
        };
        let sa = match obj.GetNames(
            None::<&PCWSTR>,
            WBEM_CONDITION_FLAG_TYPE(0),
            std::ptr::null(),
        ) {
            Ok(sa) => sa,
            Err(_) => return fallback.to_string(),
        };
        if sa.is_null() {
            return fallback.to_string();
        }
        let lbound = SafeArrayGetLBound(sa, 1).unwrap_or(0);
        let ubound = SafeArrayGetUBound(sa, 1).unwrap_or(-1);
        for i in lbound..=ubound {
            let indices = [i];
            let mut bstr_ptr: *const u16 = std::ptr::null();
            if SafeArrayGetElement(
                sa,
                indices.as_ptr(),
                &mut bstr_ptr as *mut *const u16 as *mut core::ffi::c_void,
            )
            .is_err()
                || bstr_ptr.is_null()
            {
                continue;
            }
            // SafeArrayGetElement 对 BSTR 元素返回**调用方拥有**的深拷贝
            // （实测：返回指针与数组内元素指针不同，且 SafeArrayDestroy 后
            // 该 BSTR 依然有效），因此 BSTR 包装器必须在其析构时释放拷贝；
            // 数组自身元素的释放由 SafeArrayDestroy 负责，互不干扰。
            let bstr = BSTR::from_raw(bstr_ptr);
            let name = String::from_utf16_lossy(&bstr[..]);
            // 系统属性（__*）与返回值属性（ReturnValue/ReturnCode）都不是
            // 数据参数，必须整体跳过；若全是这两类则说明该 schema 没有用户
            // 参数，回退到约定名（InData/OutData）。绝不能把 ReturnValue
            // 当参数名返回——Get("ReturnValue") 拿到的是方法返回值，
            // 不是 32 字节数组，必然失败。
            if name.starts_with("__") || is_return_value_prop(&name) {
                continue;
            }
            let _ = SafeArrayDestroy(sa);
            return name;
        }
        let _ = SafeArrayDestroy(sa);
        fallback.to_string()
    }

    /// Send a 32-byte buffer via MiInterface and receive the 32-byte response.
    ///
    /// Command buffer layout (per F-HAL-05):
    ///   fun1(2B) + fun2(2B) + fun3(2B) + fun4(4B) + zero-padding = 32 bytes
    ///
    /// Response buffer layout (per F-HAL-08):
    ///   Status(2B) + Function(2B) + Data0(2B) + Data1(4B) + Data2(4B) + Data3(4B)
    fn mi_interface_call(&self, buffer: &[u8; 32]) -> Result<[u8; 32], EcError> {
        unsafe {
            let mut class: Option<IWbemClassObject> = None;
            self.services
                .GetObject(
                    &BSTR::from("MICommonInterface"),
                    WBEM_FLAG_RETURN_WBEM_COMPLETE,
                    None::<&IWbemContext>,
                    Some(&mut class as *mut Option<IWbemClassObject>),
                    None,
                )
                .map_err(|e| EcError::WmiConnect(format!("GetObject: {}", e)))?;
            let class = class.ok_or(EcError::WmiInterfaceNotFound)?;

            let mut in_sig: Option<IWbemClassObject> = None;
            let mut out_sig: Option<IWbemClassObject> = None;
            let (_mn_buf, method_name) = crate::util::to_pcwstr("MiInterface");
            class
                .GetMethod(method_name, 0, &mut in_sig, &mut out_sig)
                .map_err(|e| EcError::WmiConnect(format!("GetMethod: {}", e)))?;

            let in_sig = in_sig.ok_or(EcError::WmiInterfaceNotFound)?;
            let in_param_name = Self::param_name_from_schema(&in_sig, "InData");
            log::info!("WMI: MiInterface input parameter -> '{}'", in_param_name);

            let in_params = in_sig
                .SpawnInstance(0)
                .map_err(|e| EcError::WmiConnect(format!("SpawnInstance: {}", e)))?;

            let sa = SafeArrayCreateVector(VT_UI1, 0, 32);
            if sa.is_null() {
                return Err(EcError::WmiConnect("SafeArrayCreateVector failed".into()));
            }

            let mut data_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
            if SafeArrayAccessData(sa, &mut data_ptr).is_err() {
                SafeArrayDestroy(sa).ok();
                return Err(EcError::WmiConnect("SafeArrayAccessData failed".into()));
            }
            std::ptr::copy_nonoverlapping(buffer.as_ptr(), data_ptr as *mut u8, 32);
            if SafeArrayUnaccessData(sa).is_err() {
                SafeArrayDestroy(sa).ok();
                return Err(EcError::WmiConnect("SafeArrayUnaccessData failed".into()));
            }

            let v = VARIANT {
                Anonymous: windows::Win32::System::Variant::VARIANT_0 {
                    Anonymous: core::mem::ManuallyDrop::new(windows::Win32::System::Variant::VARIANT_0_0 {
                        vt: VARENUM(VT_ARRAY.0 | VT_UI1.0),
                        wReserved1: 0,
                        wReserved2: 0,
                        wReserved3: 0,
                        Anonymous: windows::Win32::System::Variant::VARIANT_0_0_0 { parray: sa },
                    }),
                },
            };

            let (_in_buf, in_pcwstr) = crate::util::to_pcwstr(&in_param_name);
            if let Err(e) = in_params.Put(in_pcwstr, 0, &v as *const VARIANT, 0) {
                SafeArrayDestroy(sa).ok();
                return Err(EcError::WmiConnect(format!("Put '{}': {}", in_param_name, e)));
            }
            // 关键：Put 之后**不能** SafeArrayDestroy(sa)。实测验证（含
            // HeapValidate 逐步检测）IWbemClassObject::Put 对 SAFEARRAY 保留
            // 引用而非深拷贝：Put 后立即释放数组，ExecMethod 内部仍会访问该
            // 数组，造成 OLE 堆损坏（进程以 STATUS_HEAP_CORRUPTION 退出）。
            // 数组必须存活到异步调用完成（GetResultObject 成功）且 in_params
            // 释放之后；中途出错时宁可泄漏数组也不得提前释放（异步操作可能
            // 仍在引用它）。

            let mut call_result: Option<IWbemCallResult> = None;
            self.services
                .ExecMethod(
                    &BSTR::from("MICommonInterface"),
                    &BSTR::from("MiInterface"),
                    WBEM_FLAG_RETURN_IMMEDIATELY,
                    None::<&IWbemContext>,
                    &in_params,
                    None,
                    Some(&mut call_result as *mut Option<IWbemCallResult>),
                )
                .map_err(|e| EcError::WmiCallHResult(e.code().0 as u32))?;

            let call_result =
                call_result.ok_or(EcError::WmiCallFailed(0))?;

            log::info!("WMI: GetResultObject waiting...");
            let out_params = match call_result.GetResultObject(10000) {
                Ok(p) => p,
                Err(e) => {
                    log::error!(
                        "WMI: GetResultObject failed: hr=0x{:08X}",
                        e.code().0 as u32
                    );
                    return Err(EcError::WmiCallHResult(e.code().0 as u32));
                }
            };

            let out_param_name = Self::param_name_from_schema(&out_params, "OutData");
            log::info!("WMI: MiInterface output parameter -> '{}'", out_param_name);

            let (_out_buf, out_pcwstr) = crate::util::to_pcwstr(&out_param_name);
            let mut out_val = VARIANT::default();
            let mut out_type = 0i32;
            let mut out_flavor = 0i32;
            if let Err(e) = out_params.Get(
                out_pcwstr,
                0,
                &mut out_val,
                Some(&mut out_type as *mut i32),
                Some(&mut out_flavor as *mut i32),
            ) {
                return Err(EcError::WmiConnect(format!("Get '{}': {}", out_param_name, e)));
            }

            let expected_vt = VARENUM(VT_ARRAY.0 | VT_UI1.0);
            if out_val.Anonymous.Anonymous.vt != expected_vt {
                VariantClear(&mut out_val).ok();
                return Err(EcError::WmiCallFailed(0));
            }
            let out_sa = out_val.Anonymous.Anonymous.Anonymous.parray;
            if out_sa.is_null() {
                VariantClear(&mut out_val).ok();
                return Err(EcError::WmiCallFailed(0));
            }

            // The response is expected to be a 32-byte array; refuse to read a
            // shorter one instead of over-reading the buffer.
            let lbound = SafeArrayGetLBound(out_sa, 1).unwrap_or(0);
            let ubound = SafeArrayGetUBound(out_sa, 1).unwrap_or(-1);
            let len = ubound.saturating_sub(lbound).saturating_add(1) as usize;
            if len < 32 {
                log::error!("WMI: output array too short ({} bytes)", len);
                VariantClear(&mut out_val).ok();
                return Err(EcError::WmiCallFailed(0));
            }

            let mut out_data: *mut core::ffi::c_void = std::ptr::null_mut();
            if SafeArrayAccessData(out_sa, &mut out_data).is_err() {
                VariantClear(&mut out_val).ok();
                return Err(EcError::WmiConnect("SafeArrayAccessData out failed".into()));
            }

            let mut result = [0u8; 32];
            std::ptr::copy_nonoverlapping(out_data as *const u8, result.as_mut_ptr(), 32);

            SafeArrayUnaccessData(out_sa).ok();
            // Release the output VARIANT (and the SafeArray it owns).
            VariantClear(&mut out_val).ok();

            // 异步调用已完成：先释放仍引用输入数组的 in_params，再销毁数组
            // （见上方 Put 处的说明，顺序不可颠倒）。
            drop(in_params);
            SafeArrayDestroy(sa).ok();

            // F-HAL-08: 响应前 2 字节为 Status（小端）。非 0 表示 EC 拒绝了
            // 该命令：读操作的数据字段无意义，写操作实际并未生效。
            // 不校验的话，写失败会被误判为成功，读失败会返回垃圾数据。
            let status = u16::from_le_bytes([result[0], result[1]]);
            if status != 0 {
                log::error!("WMI: MiInterface returned status {:#x}", status);
                return Err(EcError::WmiCallFailed(status));
            }

            Ok(result)
        }
    }

    /// Build a read command buffer.
    /// Layout: fun1=0xFA00, fun2=selector, fun3=sub-op, fun4=0
    /// Per F-HAL-06: 充电读 fun3=0x0002, 性能读 fun3=0x0000
    fn read_battery(&self) -> Result<[u8; 32], EcError> {
        let mut buf = [0u8; 32];
        to_le16(&mut buf, 0, CMD_READ);       // fun1
        to_le16(&mut buf, 2, FUN2_BATTERY);   // fun2
        to_le16(&mut buf, 4, 0x0002);          // fun3 = 子操作(充电读)
        // fun4 保持 0x00000000
        self.mi_interface_call(&buf)
    }

    /// Build a write command buffer for battery.
    /// Layout: fun1=0xFB00, fun2=0x1000, fun3=0x0002, fun4=raw_code
    /// Per F-HAL-07: 充电写 fun3=0x0002, fun4=充电上限 raw code
    fn write_battery(&self, raw_code: u8) -> Result<(), EcError> {
        let mut buf = [0u8; 32];
        to_le16(&mut buf, 0, CMD_WRITE);      // fun1
        to_le16(&mut buf, 2, FUN2_BATTERY);   // fun2
        to_le16(&mut buf, 4, 0x0002);          // fun3 = 参数(充电写=0x0002)
        // fun4 = 充电上限 raw code (4 bytes, LE)
        let v = raw_code as u32;
        buf[6] = (v & 0xFF) as u8;
        buf[7] = ((v >> 8) & 0xFF) as u8;
        buf[8] = ((v >> 16) & 0xFF) as u8;
        buf[9] = ((v >> 24) & 0xFF) as u8;
        self.mi_interface_call(&buf)?;
        Ok(())
    }

    fn read_perf(&self) -> Result<[u8; 32], EcError> {
        let mut buf = [0u8; 32];
        to_le16(&mut buf, 0, CMD_READ);       // fun1
        to_le16(&mut buf, 2, FUN2_PERF);      // fun2
        to_le16(&mut buf, 4, 0x0000);          // fun3 = 子操作(性能读=0x0000)
        // fun4 保持 0x00000000
        self.mi_interface_call(&buf)
    }

    /// Build a write command buffer for performance mode.
    /// Layout: fun1=0xFB00, fun2=0x0800, fun3=mode, fun4=0
    /// Per F-HAL-07: 性能写 fun3=模式 raw code, fun4=0
    fn write_perf(&self, mode: u8) -> Result<(), EcError> {
        let mut buf = [0u8; 32];
        to_le16(&mut buf, 0, CMD_WRITE);      // fun1
        to_le16(&mut buf, 2, FUN2_PERF);      // fun2
        to_le16(&mut buf, 4, mode as u16);     // fun3 = 参数(模式 raw code)
        // fun4 保持 0x00000000
        self.mi_interface_call(&buf)?;
        Ok(())
    }
}

impl EcBackend for WmiBackend {
    fn name(&self) -> &'static str {
        "WMI (MICommonInterface)"
    }

    fn read_byte(&self, addr: u16) -> Result<u8, EcError> {
        match addr {
            ec_addr::PERF_MODE => {
                let buf = self.read_perf()?;
                Ok(buf[4]) // Data0
            }
            ec_addr::CHARGE_LIMIT => {
                let buf = self.read_battery()?;
                // Data1 = 充电上限 raw code（如 0=100%、1=80%）。read_byte 对
                // 该地址的约定语义是百分比（与 write_byte(CHARGE_LIMIT) 接收
                // 百分比、以及 WinRing0 后端该地址读写均为百分比保持一致），
                // 必须换算后再返回，否则读写不对称。
                Ok(battery::wmi_rawcode_to_percent(buf[6]).unwrap_or(100))
            }
            ec_addr::BATTERY_CARE => {
                let buf = self.read_battery()?;
                // WMI 没有独立的电池养护位；充电上限 < 100% 表示已启用
                let raw = buf[6]; // Data1 = 充电上限 raw code
                let percent = battery::wmi_rawcode_to_percent(raw).unwrap_or(100);
                Ok(if percent < 100 { 0x01 } else { 0x00 })
            }
            _ => Err(EcError::ReadFailed(addr)),
        }
    }

    fn write_byte(&self, addr: u16, value: u8) -> Result<(), EcError> {
        match addr {
            ec_addr::PERF_MODE => self.write_perf(value),
            // WMI 没有独立的电池养护位（read_byte(BATTERY_CARE) 由充电上限
            // <100% 推导，返回 0x01/0x00）。写入侧必须与 set_battery_care 保持
            // 同一语义：0x00 = 关闭养护（上限提到 100%），非 0 = 启用养护——
            // 上限由调用方通过 set_charge_limit 单独设置。绝不能把养护位原值
            // 直接当作充电上限 raw code 写入，否则 write_byte(BATTERY_CARE,
            // 0x01) 会把充电上限静默改成 80%，覆盖用户设置。
            ec_addr::BATTERY_CARE => {
                if value == 0 {
                    self.set_charge_limit(100)
                } else {
                    Ok(())
                }
            }
            // read_byte(CHARGE_LIMIT) 返回的是百分比，写入侧保持一致：
            // 先把百分比换算成 WMI raw code 再写入，避免读写语义不一致
            // （否则按 raw code 直接写入，读回时按百分比解析会完全错位）。
            ec_addr::CHARGE_LIMIT => self.write_battery(wmi_rawcode_for_percent(value)),
            _ => Err(EcError::WriteFailed(addr)),
        }
    }

    fn supports_continuous_charge_limit(&self) -> bool {
        false
    }

    fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
        let buf = self.read_battery()?;
        let raw = buf[6]; // Data1 = 充电上限 raw code
        let percent = battery::wmi_rawcode_to_percent(raw).unwrap_or(100);
        log::info!("WMI: battery care enabled -> {}, limit -> {}%", percent < 100, percent);
        Ok(percent < 100)
    }

    fn get_charge_limit(&self) -> Result<u8, EcError> {
        let buf = self.read_battery()?;
        let raw = buf[6]; // Data1 = 充电上限 raw code
        let percent = battery::wmi_rawcode_to_percent(raw).unwrap_or(100);
        log::info!("WMI: charge limit -> {}%", percent);
        Ok(percent)
    }

    fn set_battery_care(&self, enabled: bool) -> Result<(), EcError> {
        log::info!("WMI: set battery care -> {}", if enabled { "enabled" } else { "disabled" });
        if !enabled {
            // WMI has no independent battery-care bit: care is the charge
            // limit being below 100%.  Disabling care must therefore raise the
            // limit to 100%; when enabling, the caller sets the desired limit.
            self.set_charge_limit(100)?;
        }
        Ok(())
    }

    fn set_charge_limit(&self, percent: u8) -> Result<(), EcError> {
        let percent = percent.min(100);
        let raw = wmi_rawcode_for_percent(percent);
        log::info!("WMI: set charge limit -> {}% (raw {:#x})", percent, raw);
        self.write_battery(raw)
    }

    fn get_performance_mode(&self) -> Result<u8, EcError> {
        let buf = self.read_perf()?;
        log::info!("WMI: read perf mode -> {:#x}", buf[4]);
        Ok(buf[4])
    }

    fn set_performance_mode(&self, mode: u8) -> Result<(), EcError> {
        log::info!("WMI: set perf mode -> {:#x}", mode);
        self.write_perf(mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wmi_rawcode_for_percent_exact() {
        assert_eq!(wmi_rawcode_for_percent(100), 0);
        assert_eq!(wmi_rawcode_for_percent(80), 1);
        assert_eq!(wmi_rawcode_for_percent(90), 4);
        assert_eq!(wmi_rawcode_for_percent(70), 5);
        assert_eq!(wmi_rawcode_for_percent(60), 6);
        assert_eq!(wmi_rawcode_for_percent(50), 7);
        assert_eq!(wmi_rawcode_for_percent(40), 8);
    }

    #[test]
    fn test_wmi_rawcode_for_percent_nearest() {
        assert_eq!(wmi_rawcode_for_percent(85), 1); // 80%（与最近预设一致）
        assert_eq!(wmi_rawcode_for_percent(55), 6); // 60%
        assert_eq!(wmi_rawcode_for_percent(95), 0); // 100%
        assert_eq!(wmi_rawcode_for_percent(45), 7); // 50%
    }
}
