//! WMI EC backend — MICommonInterface.MiInterface protocol

use super::backend::EcBackend;
use super::battery;
use super::error::EcError;
use windows::Win32::System::Com::{
    CoInitializeEx, CoSetProxyBlanket, CoCreateInstance, CLSCTX_ALL,
    COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Ole::SafeArrayCreateVector;
use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayUnaccessData, SafeArrayDestroy};
use windows::Win32::System::Variant::{VARIANT, VARENUM, VT_ARRAY, VT_UI1};
use windows::Win32::System::Wmi::*;
use windows::core::{BSTR, GUID, PCWSTR};

/// CLSID_WbemLocator — uuid DC12A687-737F-11CF-884D-00AA004B2E24
#[allow(non_upper_case_globals)]
const CLSID_WbemLocator: GUID = GUID {
    data1: 0xDC12A687,
    data2: 0x737F,
    data3: 0x11CF,
    data4: [0x88, 0x4D, 0x00, 0xAA, 0x00, 0x4B, 0x2E, 0x24],
};

const RPC_C_AUTHN_WINNT: u32 = 10u32;
const RPC_C_AUTHZ_NONE: u32 = 0u32;

/// MiInterface command constants (little-endian bytes)
const CMD_READ: u16 = 0xFA00;
const CMD_WRITE: u16 = 0xFB00;
const FUN2_BATTERY: u16 = 0x1000;
const FUN2_PERF: u16 = 0x0800;

unsafe fn ensure_mta() -> Result<(), EcError> {
    CoInitializeEx(None, COINIT_MULTITHREADED)
        .ok()
        .map_err(|e| EcError::WmiConnect(format!("COM init: {}", e)))
}

fn to_le16(buf: &mut [u8; 32], offset: usize, val: u16) {
    buf[offset] = (val & 0xFF) as u8;
    buf[offset + 1] = ((val >> 8) & 0xFF) as u8;
}

fn from_le16(buf: &[u8; 32], offset: usize) -> u16 {
    u16::from_le_bytes([buf[offset], buf[offset + 1]])
}

pub struct WmiBackend {
    services: IWbemServices,
}

unsafe impl Send for WmiBackend {}
unsafe impl Sync for WmiBackend {}

impl WmiBackend {
    pub fn new() -> Result<Self, EcError> {
        unsafe {
            ensure_mta()?;

            let locator: IWbemLocator = CoCreateInstance(&CLSID_WbemLocator, None, CLSCTX_ALL)
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
            .ok()
            .ok_or(EcError::WmiConnect("CoSetProxyBlanket failed".into()))?;

            Ok(Self { services })
        }
    }

    /// Send a 32-byte buffer via MiInterface and receive the 32-byte response.
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
            let method_name = to_pcwstr("MiInterface");
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
                        Anonymous: windows::Win32::System::Variant::VARIANT_0_0_0 { parray: sa as *mut windows::Win32::System::Com::SAFEARRAY },
                    }),
                },
            };

            let prop_name = to_pcwstr("Buffer");
            in_params
                .Put(prop_name, 0, &v as *const VARIANT, 0)
                .map_err(|e| EcError::WmiConnect(format!("Put Buffer: {}", e)))?;

            SafeArrayDestroy(sa)
                .ok()
                .ok_or(EcError::WmiConnect("SafeArrayDestroy failed".into()))?;

            let mut out_params: Option<IWbemClassObject> = None;
            self.services
                .ExecMethod(
                    &BSTR::from("MICommonInterface"),
                    &BSTR::from("MiInterface"),
                    WBEM_FLAG_RETURN_WBEM_COMPLETE,
                    None::<&IWbemContext>,
                    &in_params,
                    Some(&mut out_params as *mut Option<IWbemClassObject>),
                    None,
                )
                .map_err(|_| EcError::WmiCallFailed(0))?;

            let out_params = out_params.ok_or(EcError::WmiCallFailed(0))?;

            let mut out_val = VARIANT::default();
            let mut out_type = 0i32;
            let mut out_flavor = 0i32;
            out_params
                .Get(prop_name, 0, &mut out_val, Some(&mut out_type as *mut i32), Some(&mut out_flavor as *mut i32))
                .map_err(|e| EcError::WmiConnect(format!("Get Buffer: {}", e)))?;

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

    fn read_battery(&self) -> Result<[u8; 32], EcError> {
        let mut buf = [0u8; 32];
        to_le16(&mut buf, 0, CMD_READ);
        to_le16(&mut buf, 2, FUN2_BATTERY);
        self.mi_interface_call(&buf)
    }

    fn write_battery(&self, raw_code: u8) -> Result<(), EcError> {
        let mut buf = [0u8; 32];
        to_le16(&mut buf, 0, CMD_WRITE);
        to_le16(&mut buf, 2, FUN2_BATTERY);
        buf[4] = raw_code;
        buf[6] = raw_code;
        self.mi_interface_call(&buf)?;
        Ok(())
    }

    fn read_perf(&self) -> Result<[u8; 32], EcError> {
        let mut buf = [0u8; 32];
        to_le16(&mut buf, 0, CMD_READ);
        to_le16(&mut buf, 2, FUN2_PERF);
        self.mi_interface_call(&buf)
    }

    fn write_perf(&self, mode: u8) -> Result<(), EcError> {
        let mut buf = [0u8; 32];
        to_le16(&mut buf, 0, CMD_WRITE);
        to_le16(&mut buf, 2, FUN2_PERF);
        buf[4] = mode;
        buf[6] = mode;
        self.mi_interface_call(&buf)?;
        Ok(())
    }
}

impl EcBackend for WmiBackend {
    fn name(&self) -> &'static str {
        "WMI (MICommonInterface)"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn read_byte(&self, addr: u16) -> Result<u8, EcError> {
        unsafe { ensure_mta()?; }
        let buf = match addr {
            0x68 => self.read_perf()?,
            0xA4 | 0xA7 => self.read_battery()?,
            _ => return Err(EcError::ReadFailed(addr)),
        };
        Ok(buf[4])
    }

    fn write_byte(&self, addr: u16, value: u8) -> Result<(), EcError> {
        unsafe { ensure_mta()?; }
        match addr {
            0x68 => self.write_perf(value),
            0xA4 | 0xA7 => self.write_battery(value),
            _ => Err(EcError::WriteFailed(addr)),
        }
    }

    fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
        unsafe { ensure_mta()?; }
        let buf = self.read_battery()?;
        let limit = from_le16(&buf, 4);
        let raw = limit as u8;
        let percent = battery::wmi_rawcode_to_percent(raw).unwrap_or(100);
        Ok(percent < 100)
    }

    fn get_charge_limit(&self) -> Result<u8, EcError> {
        unsafe { ensure_mta()?; }
        let buf = self.read_battery()?;
        let raw = buf[4];
        Ok(battery::wmi_rawcode_to_percent(raw).unwrap_or(100))
    }

    fn set_battery_care(&self, enabled: bool) -> Result<(), EcError> {
        unsafe { ensure_mta()?; }
        let current = self.get_charge_limit()?;
        if enabled {
            if current == 100 {
                self.set_charge_limit(80)?;
            }
        } else {
            self.set_charge_limit(100)?;
        }
        Ok(())
    }

    fn set_charge_limit(&self, percent: u8) -> Result<(), EcError> {
        unsafe { ensure_mta()?; }
        let percent = percent.min(100);
        let raw = battery::percent_to_wmi_rawcode(percent)
            .or_else(|| Some(battery::nearest_wmi_percent(percent)))
            .unwrap_or(0);
        self.write_battery(raw)
    }

    fn get_performance_mode(&self) -> Result<u8, EcError> {
        unsafe { ensure_mta()?; }
        let buf = self.read_perf()?;
        Ok(buf[4])
    }

    fn set_performance_mode(&self, mode: u8) -> Result<(), EcError> {
        unsafe { ensure_mta()?; }
        self.write_perf(mode)
    }
}

fn to_pcwstr(s: &str) -> PCWSTR {
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let leaked: &'static [u16] = Box::leak(wide.into_boxed_slice());
    PCWSTR(leaked.as_ptr())
}
