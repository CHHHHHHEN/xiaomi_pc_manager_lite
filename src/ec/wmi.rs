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
use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayUnaccessData, SafeArrayDestroy};
use windows::Win32::System::Variant::{VARIANT, VARENUM, VT_ARRAY, VT_UI1};
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

            let in_params = in_sig
                .ok_or(EcError::WmiInterfaceNotFound)?
                .SpawnInstance(0)
                .map_err(|e| EcError::WmiConnect(format!("SpawnInstance: {}", e)))?;

            let sa = SafeArrayCreateVector(VT_UI1, 0, 32);
            if sa.is_null() {
                return Err(EcError::WmiConnect("SafeArrayCreateVector failed".into()));
            }

            let mut data_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
            SafeArrayAccessData(sa, &mut data_ptr)
                .ok()
                .ok_or(EcError::WmiConnect("SafeArrayAccessData failed".into()))?;
            std::ptr::copy_nonoverlapping(buffer.as_ptr(), data_ptr as *mut u8, 32);
            SafeArrayUnaccessData(sa)
                .ok()
                .ok_or(EcError::WmiConnect("SafeArrayUnaccessData failed".into()))?;

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

            let (_in_buf, in_name) = crate::util::to_pcwstr(IN_PARAM);
            in_params
                .Put(in_name, 0, &v as *const VARIANT, 0)
                .map_err(|e| EcError::WmiConnect(format!("Put {}: {}", IN_PARAM, e)))?;

            SafeArrayDestroy(sa)
                .ok()
                .ok_or(EcError::WmiConnect("SafeArrayDestroy failed".into()))?;

            // Use RETURN_IMMEDIATELY because the WMI provider's synchronous
            // path fails with WBEM_E_INVALID_METHOD_PARAMETERS on this build.
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
                .map_err(|e| EcError::WmiCallFailed(e.code().0 as u16))?;

            let call_result =
                call_result.ok_or(EcError::WmiCallFailed(0))?;

            // Wait up to 10 seconds for the provider to respond
            log::info!("WMI: GetResultObject waiting...");
            let out_params = match call_result.GetResultObject(10000) {
                Ok(p) => p,
                Err(e) => {
                    log::error!(
                        "WMI: GetResultObject failed: hr=0x{:08X}",
                        e.code().0 as u32
                    );
                    return Err(EcError::WmiCallFailed(e.code().0 as u16));
                }
            };

            let (_out_buf, out_name) = crate::util::to_pcwstr(OUT_PARAM);
            let mut out_val = VARIANT::default();
            let mut out_type = 0i32;
            let mut out_flavor = 0i32;
            out_params
                .Get(out_name, 0, &mut out_val, Some(&mut out_type as *mut i32), Some(&mut out_flavor as *mut i32))
                .map_err(|e| EcError::WmiConnect(format!("Get {}: {}", OUT_PARAM, e)))?;

            let expected_vt = VARENUM(VT_ARRAY.0 | VT_UI1.0);
            if out_val.Anonymous.Anonymous.vt != expected_vt {
                return Err(EcError::WmiCallFailed(0));
            }
            let out_sa = out_val.Anonymous.Anonymous.Anonymous.parray;
            if out_sa.is_null() {
                return Err(EcError::WmiCallFailed(0));
            }

            let mut out_data: *mut core::ffi::c_void = std::ptr::null_mut();
            SafeArrayAccessData(out_sa, &mut out_data)
                .ok()
                .ok_or(EcError::WmiConnect("SafeArrayAccessData out failed".into()))?;

            let mut result = [0u8; 32];
            std::ptr::copy_nonoverlapping(out_data as *const u8, result.as_mut_ptr(), 32);

            SafeArrayUnaccessData(out_sa)
                .ok()
                .ok_or(EcError::WmiConnect("SafeArrayUnaccessData out failed".into()))?;

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
                Ok(buf[6]) // Data1
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
            ec_addr::BATTERY_CARE | ec_addr::CHARGE_LIMIT => self.write_battery(value),
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
            self.set_charge_limit(100)?;
        } else {
            self.set_charge_limit(80)?;
        }
        Ok(())
    }

    fn set_charge_limit(&self, percent: u8) -> Result<(), EcError> {
        let percent = percent.min(100);
        let raw = battery::percent_to_wmi_rawcode(percent)
            .or_else(|| battery::percent_to_wmi_rawcode(battery::nearest_wmi_percent(percent)))
            .unwrap_or(0);
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

/// Property names on the MICommonInterface.MiInterface method signature.
const IN_PARAM: &str = "InData";
const OUT_PARAM: &str = "OutData";
