use std::sync::mpsc;

use windows::Win32::System::Com::{
    CoInitializeEx, CoSetProxyBlanket, CoCreateInstance, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Wmi::*;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayUnaccessData, SafeArrayGetUBound};
use windows::core::{BSTR, GUID, PCWSTR};

use windows::Win32::System::Variant::{VARENUM, VT_ARRAY, VT_UI1};

use crate::command::UiCommand;

const CLSID_WMI_LOCATOR: GUID = GUID::from_u128(0xCB8555CC_9128_11D1_AD9B_00C04FD8FDFF);

const RPC_C_AUTHN_WINNT: u32 = 10u32;
const RPC_C_AUTHZ_NONE: u32 = 0u32;

const WMI_CLASSES: &[&str] = &["HID_EVENT20"];

#[derive(Debug)]
pub enum FnAction {
    CyclePerformanceMode,
    ShowFnLockOsd,
    ShowCapsLockOsd,
    ShowKeyboardBacklightOsd,
    MicrophoneMuteOn,
    MicrophoneMuteOff,
    OpenSettings,
    OpenProjection,
}

pub struct FnKeyDef {
    pub name: &'static str,
    pub wmi_class: &'static str,
    pub hex_prefix: &'static str,
    pub action: Option<FnAction>,
}

const BUILTIN_KEYS: &[FnKeyDef] = &[
    FnKeyDef { name: "Fn+K 性能模式切换", wmi_class: "HID_EVENT20", hex_prefix: "012801", action: Some(FnAction::CyclePerformanceMode) },
    FnKeyDef { name: "Fn 锁",             wmi_class: "HID_EVENT20", hex_prefix: "0107",   action: Some(FnAction::ShowFnLockOsd) },
    FnKeyDef { name: "大写锁定",          wmi_class: "HID_EVENT20", hex_prefix: "0109",   action: Some(FnAction::ShowCapsLockOsd) },
    FnKeyDef { name: "麦克风静音开",      wmi_class: "HID_EVENT20", hex_prefix: "012101", action: Some(FnAction::MicrophoneMuteOn) },
    FnKeyDef { name: "麦克风静音关",      wmi_class: "HID_EVENT20", hex_prefix: "012100", action: Some(FnAction::MicrophoneMuteOff) },
    FnKeyDef { name: "键盘背光循环",      wmi_class: "HID_EVENT20", hex_prefix: "0105",   action: Some(FnAction::ShowKeyboardBacklightOsd) },
    FnKeyDef { name: "投影切换",          wmi_class: "HID_EVENT20", hex_prefix: "0101",   action: Some(FnAction::OpenProjection) },
    FnKeyDef { name: "设置",              wmi_class: "HID_EVENT20", hex_prefix: "011B",   action: Some(FnAction::OpenSettings) },
    FnKeyDef { name: "小爱同学",          wmi_class: "HID_EVENT20", hex_prefix: "012301", action: None },
    FnKeyDef { name: "PC Manager",        wmi_class: "HID_EVENT20", hex_prefix: "012501", action: None },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_keys_count() {
        assert_eq!(BUILTIN_KEYS.len(), 10);
    }

    #[test]
    fn test_fn_k_definition() {
        let key = BUILTIN_KEYS.iter().find(|k| k.hex_prefix == "012801").unwrap();
        assert_eq!(key.name, "Fn+K 性能模式切换");
        assert_eq!(key.wmi_class, "HID_EVENT20");
        assert_eq!(key.hex_prefix, "012801");
        assert!(matches!(key.action, Some(FnAction::CyclePerformanceMode)));
    }

    #[test]
    fn test_fn_lock_definition() {
        let key = BUILTIN_KEYS.iter().find(|k| k.hex_prefix == "0107").unwrap();
        assert_eq!(key.name, "Fn 锁");
        assert_eq!(key.wmi_class, "HID_EVENT20");
        assert_eq!(key.hex_prefix, "0107");
        assert!(matches!(key.action, Some(FnAction::ShowFnLockOsd)));
    }

    #[test]
    fn test_caps_lock_definition() {
        let key = BUILTIN_KEYS.iter().find(|k| k.hex_prefix == "0109").unwrap();
        assert_eq!(key.name, "大写锁定");
        assert_eq!(key.wmi_class, "HID_EVENT20");
        assert!(matches!(key.action, Some(FnAction::ShowCapsLockOsd)));
    }

    #[test]
    fn test_mic_mute_on_definition() {
        let key = BUILTIN_KEYS.iter().find(|k| k.hex_prefix == "012101").unwrap();
        assert_eq!(key.name, "麦克风静音开");
        assert!(matches!(key.action, Some(FnAction::MicrophoneMuteOn)));
    }

    #[test]
    fn test_mic_mute_off_definition() {
        let key = BUILTIN_KEYS.iter().find(|k| k.hex_prefix == "012100").unwrap();
        assert_eq!(key.name, "麦克风静音关");
        assert!(matches!(key.action, Some(FnAction::MicrophoneMuteOff)));
    }

    #[test]
    fn test_keyboard_backlight_definition() {
        let key = BUILTIN_KEYS.iter().find(|k| k.hex_prefix == "0105").unwrap();
        assert_eq!(key.name, "键盘背光循环");
        assert!(matches!(key.action, Some(FnAction::ShowKeyboardBacklightOsd)));
    }

    #[test]
    fn test_projection_definition() {
        let key = BUILTIN_KEYS.iter().find(|k| k.hex_prefix == "0101").unwrap();
        assert_eq!(key.name, "投影切换");
        assert!(matches!(key.action, Some(FnAction::OpenProjection)));
    }

    #[test]
    fn test_settings_definition() {
        let key = BUILTIN_KEYS.iter().find(|k| k.hex_prefix == "011B").unwrap();
        assert_eq!(key.name, "设置");
        assert!(matches!(key.action, Some(FnAction::OpenSettings)));
    }

    #[test]
    fn test_xiaoai_definition() {
        let key = BUILTIN_KEYS.iter().find(|k| k.hex_prefix == "012301").unwrap();
        assert_eq!(key.name, "小爱同学");
        assert!(key.action.is_none());
    }

    #[test]
    fn test_pc_manager_definition() {
        let key = BUILTIN_KEYS.iter().find(|k| k.hex_prefix == "012501").unwrap();
        assert_eq!(key.name, "PC Manager");
        assert!(key.action.is_none());
    }

    #[test]
    fn test_all_keys_have_wmi_class() {
        for key in BUILTIN_KEYS {
            assert!(!key.wmi_class.is_empty(), "key {} has empty wmi_class", key.name);
            assert!(!key.hex_prefix.is_empty(), "key {} has empty hex_prefix", key.name);
        }
    }

    #[test]
    fn test_all_keys_in_hid_event20() {
        for key in BUILTIN_KEYS {
            assert_eq!(key.wmi_class, "HID_EVENT20", "key {} has unexpected class", key.name);
        }
    }

    #[test]
    fn test_dispatch_action_cycle_perf_mode() {
        let (tx, rx) = std::sync::mpsc::channel();
        dispatch_action(&FnAction::CyclePerformanceMode, &tx);
        match rx.try_recv() {
            Ok(UiCommand::CyclePerfMode) => {}
            other => panic!("Expected CyclePerfMode, got {:?}", other),
        }
    }

    #[test]
    fn test_dispatch_action_non_implemented_does_not_send() {
        let (tx, rx) = std::sync::mpsc::channel();
        dispatch_action(&FnAction::ShowFnLockOsd, &tx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_dispatch_action_microphone_mute() {
        let (tx, rx) = std::sync::mpsc::channel();
        dispatch_action(&FnAction::MicrophoneMuteOn, &tx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_hex_prefix_matching_012801_matches_fn_k() {
        let clean = "012801FFFF".to_string();
        for key in BUILTIN_KEYS {
            if key.hex_prefix == "012801" {
                assert!(clean.starts_with(key.hex_prefix));
            }
        }
    }

    #[test]
    fn test_hex_prefix_matching_0107_matches_fn_lock_press() {
        let clean = "010701".to_string();
        let matched = BUILTIN_KEYS.iter().any(|k| k.hex_prefix == "0107" && clean.starts_with(k.hex_prefix));
        assert!(matched);
    }

    #[test]
    fn test_hex_prefix_matching_010700_matches_fn_lock_release() {
        let clean = "010700".to_string();
        let matched = BUILTIN_KEYS.iter().any(|k| k.hex_prefix == "0107" && clean.starts_with(k.hex_prefix));
        assert!(matched);
    }

    #[test]
    fn test_hex_prefix_no_false_positive() {
        let clean = "0120".to_string();
        let matched = BUILTIN_KEYS.iter().any(|k| clean.starts_with(k.hex_prefix));
        assert!(!matched);
    }

    #[test]
    fn test_builtin_keys_unique_names() {
        let mut names: Vec<&str> = BUILTIN_KEYS.iter().map(|k| k.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), BUILTIN_KEYS.len());
    }

    #[test]
    fn test_builtin_keys_unique_hex_prefixes() {
        let mut prefixes: Vec<&str> = BUILTIN_KEYS.iter().map(|k| k.hex_prefix).collect();
        prefixes.sort();
        prefixes.dedup();
        assert_eq!(prefixes.len(), BUILTIN_KEYS.len());
    }
}

struct SafeEnumerator(IEnumWbemClassObject);
// SAFETY: SafeEnumerator is only used on the dedicated fnkey watcher thread.
// COM is initialized in MTA on that thread, and the enumerator is never
// accessed from any other thread.
unsafe impl Send for SafeEnumerator {}

fn dispatch_action(action: &FnAction, cmd_tx: &mpsc::Sender<UiCommand>) {
    match action {
        FnAction::CyclePerformanceMode => {
            let _ = cmd_tx.send(UiCommand::CyclePerfMode);
        }
        _ => log::info!("FnKey action: {:?} (not yet implemented)", action),
    }
}

pub fn spawn(cmd_tx: mpsc::Sender<UiCommand>) {
    std::thread::spawn(move || {
        if let Err(e) = run_watcher(&cmd_tx) {
            log::error!("FnKey watcher: {}", e);
        }
    });
}

fn run_watcher(cmd_tx: &mpsc::Sender<UiCommand>) -> Result<(), String> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|e| format!("COM init: {}", e))?
    };

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

    log::info!("FnKey watcher connected to root\\wmi");

    let enumerators: Vec<(&str, SafeEnumerator)> = WMI_CLASSES.iter()
        .filter_map(|class_name| {
            let query = format!("SELECT * FROM {}", class_name);
            match unsafe {
                services.ExecNotificationQuery(
                    &BSTR::from("WQL"),
                    &BSTR::from(&query),
                    WBEM_FLAG_RETURN_IMMEDIATELY | WBEM_FLAG_FORWARD_ONLY,
                    None::<&IWbemContext>,
                )
            } {
                Ok(e) => {
                    log::info!("FnKey: subscribed to {}", class_name);
                    Some((*class_name, SafeEnumerator(e)))
                }
                Err(_) => {
                    log::warn!("FnKey: cannot subscribe to {} (not available)", class_name);
                    None
                }
            }
        })
        .collect();

    if enumerators.is_empty() {
        log::warn!("FnKey: no WMI event classes available");
    }

    loop {
        for (class_name, SafeEnumerator(ref enumerator)) in &enumerators {
            let mut objects: [Option<IWbemClassObject>; 1] = [None];
            let mut returned: u32 = 0;

            let hr = unsafe {
                enumerator.Next(
                    1000,
                    &mut objects,
                    &mut returned as *mut u32,
                )
            };

            if hr.is_err() || returned == 0 {
                continue;
            }

            if let Some(ref obj) = objects[0] {
                process_event(obj, class_name, cmd_tx);
            }
        }
    }
}

fn process_event(
    obj: &IWbemClassObject,
    class_name: &str,
    cmd_tx: &mpsc::Sender<UiCommand>,
) {
    let report_hex = get_detail_hex(obj)
        .or_else(|| get_string_prop(obj, "ReportHex"));

    let report_hex = match report_hex {
        Some(h) => h,
        None => {
            log::debug!("FnKey [{}]: no EventDetail/ReportHex", class_name);
            return;
        }
    };

    let instance_name = get_string_prop(obj, "InstanceName").unwrap_or_default();
    let active = get_bool_prop(obj, "Active");

    log::debug!(
        "FnKey [{}]: EventDetail={}, InstanceName={}, Active={:?}",
        class_name, report_hex, instance_name, active,
    );

    let clean_report: String = report_hex.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();

    for key in BUILTIN_KEYS {
        if key.wmi_class == class_name && clean_report.starts_with(key.hex_prefix) {
            log::info!("FnKey: matched {} ({})", key.name, clean_report);
            if let Some(ref action) = key.action {
                dispatch_action(action, cmd_tx);
            }
            return;
        }
    }

    log::debug!("FnKey [{}]: unmatched event {} (InstanceName={})", class_name, report_hex, instance_name);
}

/// Shared helper: get a VARIANT property from a WMI object by name.
fn get_variant(obj: &IWbemClassObject, name: &str) -> Option<VARIANT> {
    let (_wide, prop_name) = crate::util::to_pcwstr(name);
    let mut val = VARIANT::default();
    let mut _type = 0i32;
    let mut _flavor = 0i32;
    unsafe {
        obj
            .Get(prop_name, 0, &mut val, Some(&mut _type as *mut i32), Some(&mut _flavor as *mut i32))
            .ok()?;
    }
    Some(val)
}

fn get_detail_hex(obj: &IWbemClassObject) -> Option<String> {
    let val = get_variant(obj, "EventDetail")?;
    let vt = unsafe { val.Anonymous.Anonymous.vt };

    if vt == VARENUM(VT_ARRAY.0 | VT_UI1.0) {
        let sa = unsafe { val.Anonymous.Anonymous.Anonymous.parray };
        if !sa.is_null() {
            let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
            if unsafe { SafeArrayAccessData(sa, &mut data) }.is_ok() {
                let ubound = unsafe { SafeArrayGetUBound(sa, 1) }.unwrap_or(-1);
                let len = (ubound + 1) as usize;
                let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, len) };
                let hex_str: String = bytes.iter()
                    .map(|b| format!("{:02X}", b))
                    .collect();
                unsafe { SafeArrayUnaccessData(sa).ok() };
                return Some(hex_str);
            }
        }
    }

    unsafe { crate::ec::wmi_util::bstr_from_variant(&val) }
}

fn get_bool_prop(obj: &IWbemClassObject, name: &str) -> Option<bool> {
    let val = get_variant(obj, name)?;
    unsafe { crate::ec::wmi_util::bool_from_variant(&val) }
}

fn get_string_prop(obj: &IWbemClassObject, name: &str) -> Option<String> {
    let val = get_variant(obj, name)?;
    unsafe { crate::ec::wmi_util::bstr_from_variant(&val) }
}
