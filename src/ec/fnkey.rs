use std::sync::mpsc;

use windows::Win32::System::Com::{
    CoInitializeEx, CoSetProxyBlanket, CoCreateInstance, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Wmi::*;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayUnaccessData,
};
use windows::core::{BSTR, GUID, PCWSTR};

use windows::Win32::System::Variant::{VARENUM, VT_ARRAY, VT_UI1, VariantClear};

use crate::command::UiCommand;

const CLSID_WMI_LOCATOR: GUID = GUID::from_u128(0xCB8555CC_9128_11D1_AD9B_00C04FD8FDFF);

const RPC_C_AUTHN_WINNT: u32 = 10u32;
const RPC_C_AUTHZ_NONE: u32 = 0u32;

/// OEM ACPI 事件类（F-FNKEY-01）：全部订阅；不存在的类会被
/// ExecNotificationQuery 拒绝并跳过，不会影响其余订阅。
const WMI_CLASSES: &[&str] = &[
    "HID_EVENT20",
    "HID_EVENT21",
    "HID_EVENT22",
    "HID_EVENT23",
    "WMIEvent",
];

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

/// 订阅全部 WMI 事件类；不存在的类会被 ExecNotificationQuery 拒绝并跳过，
/// 不影响其余订阅。返回成功订阅的 (类名, 枚举器) 列表。
fn subscribe(services: &IWbemServices) -> Vec<(&'static str, SafeEnumerator)> {
    WMI_CLASSES
        .iter()
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
        .collect()
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

    let mut enumerators: Vec<(&'static str, SafeEnumerator)> = subscribe(&services);

    loop {
        if enumerators.is_empty() {
            // 没有任何事件类订阅成功：不存在的类不会出现，但 WMI 提供程序
            // 可能只是尚未就绪（如开机时 OEM 驱动加载较晚、WMI 服务重启）。
            // 若直接进入下方循环，for 迭代器为空、循环体不做任何工作，会形成
            // 100% CPU 空转；改为低频休眠后重试订阅，既不烧 CPU 也能在提供
            // 程序就绪后自动恢复 Fn 键事件（F-FNKEY-01）。
            log::warn!("FnKey: no WMI event classes available; retrying in 5s");
            std::thread::sleep(std::time::Duration::from_secs(5));
            enumerators = subscribe(&services);
            continue;
        }

        let mut resubscribe = false;
        for (class_name, SafeEnumerator(ref enumerator)) in &enumerators {
            let mut objects: [Option<IWbemClassObject>; 1] = [None];
            let mut returned: u32 = 0;

            let hr = unsafe {
                enumerator.Next(
                    // 每个类最多阻塞 100ms：5 个类全部轮询一遍的最坏延迟约
                    // 500ms，满足 NFR-UX-02（≤500ms）。若沿用 1000ms，
                    // 事件恰在轮询间隙到达时最长要等 ~5s 才被处理。
                    100,
                    &mut objects,
                    &mut returned as *mut u32,
                )
            };

            if hr.is_err() {
                // Next() 失败时（如 WMI 提供程序连接断开、服务重启、
                // 休眠唤醒后枚举器失效）会立即返回错误码。若不做延迟，
                // 该循环将零休眠地重复调用失败接口，造成 100% CPU 忙循环。
                log::warn!(
                    "FnKey: IEnumWbemClassObject::Next failed (hr=0x{:08X}); resubscribing in 1s",
                    hr.0 as u32
                );
                // 失败后原地重试 Next 无法恢复：前向枚举器在提供程序断开后
                // 会永久返回错误（如 WBEM_E_INVALID_ENUMERATION）。必须像
                // 无可用类时那样重新订阅，否则 Fn 键事件静默失效直到应用重启。
                std::thread::sleep(std::time::Duration::from_secs(1));
                resubscribe = true;
                break;
            }

            if returned == 0 {
                continue;
            }

            if let Some(ref obj) = objects[0] {
                process_event(obj, class_name, cmd_tx);
            }
        }
        if resubscribe {
            enumerators = subscribe(&services);
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

    if let Some(key) = match_builtin_key(class_name, &report_hex) {
        log::info!("FnKey: matched {} ({})", key.name, report_hex);
        if let Some(ref action) = key.action {
            dispatch_action(action, cmd_tx);
        }
        return;
    }

    log::debug!("FnKey [{}]: unmatched event {} (InstanceName={})", class_name, report_hex, instance_name);
}

/// 在预定义功能键中查找前缀匹配项（类名一致 + ReportHex 前缀一致）。
/// 匹配前统一归一化：剔除所有非字母数字字符（如 "01-28-01" 的分隔符）并转
/// 大写。EventDetail 字节路径生成的是大写十六进制，但 ReportHex 字符串回退
/// 路径的字母大小写由固件决定，可能是小写——不归一化会导致小写报告永远
/// 匹配不上（F-FNKEY-07）。
fn match_builtin_key(class_name: &str, report_hex: &str) -> Option<&'static FnKeyDef> {
    let clean: String = report_hex
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase();
    BUILTIN_KEYS
        .iter()
        .find(|k| k.wmi_class == class_name && clean.starts_with(k.hex_prefix))
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
    let mut val = get_variant(obj, "EventDetail")?;
    let vt = unsafe { val.Anonymous.Anonymous.vt };

    let result = if vt == VARENUM(VT_ARRAY.0 | VT_UI1.0) {
        let sa = unsafe { val.Anonymous.Anonymous.Anonymous.parray };
        if !sa.is_null() {
            let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
            if unsafe { SafeArrayAccessData(sa, &mut data) }.is_ok() {
                let lbound = unsafe { SafeArrayGetLBound(sa, 1) }.unwrap_or(0);
                let ubound = unsafe { SafeArrayGetUBound(sa, 1) }.unwrap_or(-1);
                let len = ubound.saturating_sub(lbound).saturating_add(1) as usize;
                let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, len) };
                let hex_str: String = bytes.iter()
                    .map(|b| format!("{:02X}", b))
                    .collect();
                unsafe { SafeArrayUnaccessData(sa).ok() };
                Some(hex_str)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        unsafe { crate::ec::wmi_util::bstr_from_variant(&val) }
    };

    // Release the VARIANT (and the SafeArray / BSTR it owns) before returning.
    unsafe { VariantClear(&mut val).ok() };
    result
}

fn get_bool_prop(obj: &IWbemClassObject, name: &str) -> Option<bool> {
    let mut val = get_variant(obj, name)?;
    let result = unsafe { crate::ec::wmi_util::bool_from_variant(&val) };
    unsafe { VariantClear(&mut val).ok() };
    result
}

fn get_string_prop(obj: &IWbemClassObject, name: &str) -> Option<String> {
    let mut val = get_variant(obj, name)?;
    let result = unsafe { crate::ec::wmi_util::bstr_from_variant(&val) };
    unsafe { VariantClear(&mut val).ok() };
    result
}

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
    fn test_match_builtin_key_uppercase_report() {
        let key = match_builtin_key("HID_EVENT20", "012801FFFF").expect("should match");
        assert!(matches!(key.action, Some(FnAction::CyclePerformanceMode)));
    }

    #[test]
    fn test_match_builtin_key_strips_separators() {
        // 报告带分隔符（"01-28-01" 形式，见文档 F-FNKEY-05）时同样能匹配。
        let key = match_builtin_key("HID_EVENT20", "01-28-01 00 00").expect("should match");
        assert!(matches!(key.action, Some(FnAction::CyclePerformanceMode)));
    }

    #[test]
    fn test_match_builtin_key_lowercase_report_normalized() {
        // 固件以小写提供 ReportHex（如 "012801ffff..."）时，归一化大写后必须能匹配。
        let key = match_builtin_key("HID_EVENT20", "012801ffff").expect("should match after uppercase normalization");
        assert!(matches!(key.action, Some(FnAction::CyclePerformanceMode)));
    }

    #[test]
    fn test_match_builtin_key_mixed_case_report_normalized() {
        // 固件以小写字母提供 ReportHex（设置键 "01-1B" 的 B 为小写 b）时，
        // 归一化大写后必须能匹配。
        let key = match_builtin_key("HID_EVENT20", "011b01").expect("should match");
        assert_eq!(key.name, "设置");
    }

    #[test]
    fn test_match_builtin_key_rejects_wrong_class() {
        assert!(match_builtin_key("HID_EVENT21", "012801FFFF").is_none());
    }

    #[test]
    fn test_match_builtin_key_rejects_unmatched_prefix() {
        assert!(match_builtin_key("HID_EVENT20", "0120").is_none());
    }

    #[test]
    fn test_match_builtin_key_short_prefix_wins() {
        // Fn 锁前缀 "0107" 应匹配 "010701"（按下）与 "010700"（释放）。
        let key = match_builtin_key("HID_EVENT20", "010701").expect("should match");
        assert_eq!(key.name, "Fn 锁");
        assert!(match_builtin_key("HID_EVENT20", "010700").is_some());
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
