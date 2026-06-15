//! Fn+Key 功能键 WMI 事件监控 (F-FNKEY)

use std::sync::mpsc;
use std::time::Duration;

use windows::Win32::System::Com::{
    CoInitializeEx, CoSetProxyBlanket, CoCreateInstance, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Wmi::*;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayUnaccessData, SafeArrayGetUBound};
use windows::core::{BSTR, GUID, PCWSTR};

use crate::gui::app::UiCommand;

const CLSID_WMI_LOCATOR: GUID = GUID::from_u128(0xCB8555CC_9128_11D1_AD9B_00C04FD8FDFF);

const RPC_C_AUTHN_WINNT: u32 = 10u32;
const RPC_C_AUTHZ_NONE: u32 = 0u32;

const WMI_CLASSES: &[&str] = &[
    "HID_EVENT20",
    "HID_EVENT21",
    "HID_EVENT22",
    "HID_EVENT23",
    "WMIEvent",
];

#[allow(dead_code)]
pub enum FnAction {
    CyclePerformanceMode,
    ShowFnLockOsd,
    ShowCapsLockOsd,
    ShowKeyboardBacklightOsd,
    MicrophoneMuteOn,
    MicrophoneMuteOff,
    ToggleTouchpad,
    VolumeUp,
    VolumeDown,
    VolumeMute,
    BrightnessUp,
    BrightnessDown,
    MediaPrevious,
    MediaNext,
    MediaPlayPause,
    ToggleAirplaneMode,
    LockWindows,
    Screenshot,
    OpenCalculator,
    OpenSettings,
    OpenProjection,
    OpenApplication,
    SendStandardKey,
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
    use crate::gui::app::UiCommand;

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

/// Wrapper that makes `IEnumWbemClassObject` `Send`.
/// COM interfaces are safe to send when each thread initializes its own apartment.
struct SafeEnumerator(IEnumWbemClassObject);
unsafe impl Send for SafeEnumerator {}

fn dispatch_action(action: &FnAction, cmd_tx: &mpsc::Sender<UiCommand>) {
    match action {
        FnAction::CyclePerformanceMode => {
            let _ = cmd_tx.send(UiCommand::CyclePerfMode);
        }
        FnAction::ShowFnLockOsd => {
            log::info!("FnKey action: Fn 锁 OSD (not yet implemented)");
        }
        FnAction::ShowCapsLockOsd => {
            log::info!("FnKey action: 大写锁定 OSD (not yet implemented)");
        }
        FnAction::ShowKeyboardBacklightOsd => {
            log::info!("FnKey action: 键盘背光 OSD (not yet implemented)");
        }
        FnAction::MicrophoneMuteOn => {
            log::info!("FnKey action: 麦克风静音开 (not yet implemented)");
        }
        FnAction::MicrophoneMuteOff => {
            log::info!("FnKey action: 麦克风静音关 (not yet implemented)");
        }
        FnAction::ToggleTouchpad => {
            log::info!("FnKey action: 切换触摸板 (not yet implemented)");
        }
        FnAction::VolumeUp => {
            log::info!("FnKey action: 音量加 (not yet implemented)");
        }
        FnAction::VolumeDown => {
            log::info!("FnKey action: 音量减 (not yet implemented)");
        }
        FnAction::VolumeMute => {
            log::info!("FnKey action: 音量静音 (not yet implemented)");
        }
        FnAction::BrightnessUp => {
            log::info!("FnKey action: 亮度加 (not yet implemented)");
        }
        FnAction::BrightnessDown => {
            log::info!("FnKey action: 亮度减 (not yet implemented)");
        }
        FnAction::MediaPrevious => {
            log::info!("FnKey action: 上一首 (not yet implemented)");
        }
        FnAction::MediaNext => {
            log::info!("FnKey action: 下一首 (not yet implemented)");
        }
        FnAction::MediaPlayPause => {
            log::info!("FnKey action: 播放/暂停 (not yet implemented)");
        }
        FnAction::ToggleAirplaneMode => {
            log::info!("FnKey action: 飞行模式 (not yet implemented)");
        }
        FnAction::LockWindows => {
            log::info!("FnKey action: 锁定工作站 (not yet implemented)");
        }
        FnAction::Screenshot => {
            log::info!("FnKey action: 截图 (not yet implemented)");
        }
        FnAction::OpenCalculator => {
            log::info!("FnKey action: 打开计算器 (not yet implemented)");
        }
        FnAction::OpenSettings => {
            log::info!("FnKey action: 打开设置 (not yet implemented)");
        }
        FnAction::OpenProjection => {
            log::info!("FnKey action: 打开投影切换 (not yet implemented)");
        }
        FnAction::OpenApplication => {
            log::info!("FnKey action: 启动自定义应用 (not yet implemented)");
        }
        FnAction::SendStandardKey => {
            log::info!("FnKey action: 发送标准键 (not yet implemented)");
        }
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

    // Single polling loop across all enumerators
    loop {
        for (class_name, SafeEnumerator(ref enumerator)) in &enumerators {
            let mut objects: [Option<IWbemClassObject>; 1] = [None];
            let mut returned: u32 = 0;

            let hr = unsafe {
                enumerator.Next(
                    1000, // 1 second timeout
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

        std::thread::sleep(Duration::from_millis(100));
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

    // Extract InstanceName and Active per F-FNKEY-04
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

fn get_detail_hex(obj: &IWbemClassObject) -> Option<String> {
    let wide: Vec<u16> = "EventDetail\0".encode_utf16().collect();
    let mut val = VARIANT::default();
    let mut _type = 0i32;
    let mut _flavor = 0i32;

    if unsafe {
        obj
            .Get(
                PCWSTR(wide.as_ptr()),
                0,
                &mut val,
                Some(&mut _type as *mut i32),
                Some(&mut _flavor as *mut i32),
            )
    }.is_err() {
        return None;
    }

    let vt = unsafe { val.Anonymous.Anonymous.vt.0 };

    // VT_ARRAY | VT_UI1 = 0x2008 = 8200
    if vt == 0x2008 {
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

    // VT_BSTR = 8
    if vt == 8 {
        let bstr = unsafe { &*val.Anonymous.Anonymous.Anonymous.bstrVal };
        let ptr = bstr.as_ptr();
        if !ptr.is_null() {
            let len = bstr.len();
            let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
            let s = String::from_utf16_lossy(slice);
            return Some(s);
        }
    }

    None
}

fn get_bool_prop(obj: &IWbemClassObject, name: &str) -> Option<bool> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut val = VARIANT::default();
    let mut _type = 0i32;
    let mut _flavor = 0i32;

    if unsafe {
        obj.Get(
            PCWSTR(wide.as_ptr()),
            0,
            &mut val,
            Some(&mut _type as *mut i32),
            Some(&mut _flavor as *mut i32),
        )
    }.is_err() {
        return None;
    }

    // VT_BOOL = 11
    if unsafe { val.Anonymous.Anonymous.vt.0 == 11 } {
        // VARIANT_TRUE = -1 (0xFFFF), VARIANT_FALSE = 0
        Some(unsafe { val.Anonymous.Anonymous.Anonymous.boolVal } != windows::Win32::Foundation::VARIANT_BOOL(0))
    } else {
        None
    }
}

fn get_string_prop(obj: &IWbemClassObject, name: &str) -> Option<String> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut val = VARIANT::default();
    let mut _type = 0i32;
    let mut _flavor = 0i32;

    if unsafe {
        obj.Get(
            PCWSTR(wide.as_ptr()),
            0,
            &mut val,
            Some(&mut _type as *mut i32),
            Some(&mut _flavor as *mut i32),
        )
    }.is_err() {
        return None;
    }

    if unsafe { val.Anonymous.Anonymous.vt.0 == 8 } {
        let bstr = unsafe { &*val.Anonymous.Anonymous.Anonymous.bstrVal };
        let ptr = bstr.as_ptr();
        if !ptr.is_null() {
            let len = bstr.len();
            let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
            let s = String::from_utf16_lossy(slice);
            return Some(s);
        }
    }

    None
}
