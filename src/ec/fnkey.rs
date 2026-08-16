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

/// Fn+K 所在的 OEM ACPI 事件类（F-FNK-01）。
const FN_K_WMI_CLASS: &str = "HID_EVENT20";

/// 订阅的事件类：不存在的类会被 ExecNotificationQuery 拒绝并跳过，
/// 由订阅重试逻辑低频重试等待 OEM 提供程序就绪。
const WMI_CLASSES: &[&str] = &[FN_K_WMI_CLASS];

/// Fn+K 按下事件的 ReportHex 前缀：`01-28-01`（固定前缀 `01` + 键码
/// `28` + 按下状态 `01`，见 F-FNK-04）。释放事件（`012800`）不命中
/// 此前缀，一次物理按键恰好派发一次切换（F-FNK-06）。
const FN_K_PRESS_PREFIX: &str = "012801";

struct SafeEnumerator(IEnumWbemClassObject);
// SAFETY: SafeEnumerator is only used on the dedicated Fn+K watcher thread.
// COM is initialized in MTA on that thread, and the enumerator is never
// accessed from any other thread.
unsafe impl Send for SafeEnumerator {}

pub fn spawn(cmd_tx: mpsc::Sender<UiCommand>) {
    std::thread::spawn(move || {
        if let Err(e) = run_watcher(&cmd_tx) {
            log::error!("Fn+K watcher: {}", e);
        }
    });
}

/// 订阅 WMI 事件类；不存在的类会被 ExecNotificationQuery 拒绝并跳过。
/// 返回成功订阅的 (类名, 枚举器) 列表。
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
                    log::info!("Fn+K: subscribed to {}", class_name);
                    Some((*class_name, SafeEnumerator(e)))
                }
                Err(_) => {
                    log::warn!("Fn+K: cannot subscribe to {} (not available)", class_name);
                    None
                }
            }
        })
        .collect()
}

/// Fn+K 监听主循环（可重入）：COM 初始化、连接 root\wmi、订阅事件类都在
/// 这里完成。连接阶段的任何失败（如 WMI 服务尚未就绪、OEM 提供程序加载
/// 较晚）以及运行期连接失效（Next 失败后重订阅仍无结果、空订阅持续 30s）
/// 都会返回 Err 由外层 run_watcher 延时重试，监听不会因启动时的瞬时故障
/// 或 WMI 服务重启而永久失效（F-FNK-07 的自恢复设计）。
fn run_watcher_once(cmd_tx: &mpsc::Sender<UiCommand>) -> Result<(), String> {
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

    log::info!("Fn+K watcher connected to root\\wmi");

    let mut enumerators: Vec<(&'static str, SafeEnumerator)> = subscribe(&services);
    // 空订阅连续失败计数：提供程序可能只是加载较晚（值得同连接重试几轮），
    // 但若连接本身已死（winmgmt 重启、休眠唤醒后旧会话失效），在同一个
    // services 上重订阅将永远失败——必须返回 Err 由外层 run_watcher 重建
    // 连接（见下方两处返回点，F-FNK-07 的自恢复要求）。
    let mut empty_streak: u32 = 0;

    loop {
        if enumerators.is_empty() {
            empty_streak += 1;
            // 连续约 30 秒（6 次 × 5s）没有任何事件类可用：连接极可能已失效，
            // 继续同连接重试没有意义，返回 Err 让外层重建 locator/services
            // 连接后重新订阅，而不是让监听永久失效。
            if empty_streak >= 6 {
                return Err("no WMI event classes for 30s; rebuilding connection".to_string());
            }
            // 没有任何事件类订阅成功：WMI 提供程序可能只是尚未就绪（如开机
            // 时 OEM 驱动加载较晚、WMI 服务重启）。低频休眠后重试订阅，既不
            // 烧 CPU 也能在提供程序就绪后自动恢复 Fn+K 事件（F-FNK-07）。
            log::warn!("Fn+K: no WMI event classes available; retrying in 5s");
            std::thread::sleep(std::time::Duration::from_secs(5));
            enumerators = subscribe(&services);
            continue;
        }
        empty_streak = 0;

        let mut resubscribe = false;
        for (class_name, SafeEnumerator(ref enumerator)) in &enumerators {
            let mut objects: [Option<IWbemClassObject>; 1] = [None];
            let mut returned: u32 = 0;

            let hr = unsafe { enumerator.Next(100, &mut objects, &mut returned as *mut u32) };

            if hr.is_err() {
                // Next() 失败时（如 WMI 提供程序连接断开、服务重启、休眠唤醒
                // 后枚举器失效）会立即返回错误码。若不做延迟，该循环将零休眠
                // 地重复调用失败接口，造成 100% CPU 忙循环。
                log::warn!(
                    "Fn+K: IEnumWbemClassObject::Next failed (hr=0x{:08X}); resubscribing in 1s",
                    hr.0 as u32
                );
                // 失败后原地重试 Next 无法恢复：前向枚举器在提供程序断开后
                // 会永久返回错误（如 WBEM_E_INVALID_ENUMERATION）。必须重新
                // 订阅，否则 Fn+K 事件静默失效直到应用重启。
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
            // Next 失败可能只是单个枚举器/提供程序故障，也可能是整个连接
            // 断开（winmgmt 重启、休眠唤醒后旧会话失效）。先用现有连接
            // 重订阅一次；若仍无任何类可用，说明连接本身已死——必须返回
            // Err 让外层 run_watcher 重建连接，否则在失效连接上反复重订阅
            // 会永远失败，Fn+K 监听静默失效直到应用重启。
            enumerators = subscribe(&services);
            if enumerators.is_empty() {
                return Err(
                    "WMI enumerator failed and resubscribe returned nothing; rebuilding connection"
                        .to_string(),
                );
            }
        }
    }
}

fn run_watcher(cmd_tx: &mpsc::Sender<UiCommand>) -> Result<(), String> {
    loop {
        match run_watcher_once(cmd_tx) {
            // run_watcher_once 内部是无限事件循环，正常返回理论上不发生；
            // 收到 Err 说明连接阶段失败，低频延时重试而不是让监听线程退出。
            Err(e) => {
                log::warn!("Fn+K watcher startup failed: {}; retrying in 5s", e);
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
            Ok(()) => return Ok(()),
        }
    }
}

fn process_event(obj: &IWbemClassObject, class_name: &str, cmd_tx: &mpsc::Sender<UiCommand>) {
    let report_hex = get_detail_hex(obj).or_else(|| get_string_prop(obj, "ReportHex"));
    let report_hex = match report_hex {
        Some(h) => h,
        None => {
            log::debug!("Fn+K [{}]: no EventDetail/ReportHex", class_name);
            return;
        }
    };
    log::debug!("Fn+K [{}]: EventDetail={}", class_name, report_hex);

    if handle_report(class_name, &report_hex, cmd_tx) {
        return;
    }
    // 其余功能键（Fn 锁、麦克风静音等，F-FNK-09）与未知事件不产生任何
    // 动作，仅记录日志。
    log::debug!("Fn+K [{}]: unmatched event {}", class_name, report_hex);
}

/// 匹配并派发：事件类为 HID_EVENT20 且归一化后的报告以 `012801` 开头时，
/// 发送 UiCommand::CyclePerfMode 循环切换性能模式。返回是否已派发
/// （F-FNK-04 / F-FNK-05）。
fn handle_report(class_name: &str, report_hex: &str, cmd_tx: &mpsc::Sender<UiCommand>) -> bool {
    if !is_fn_k_press(class_name, report_hex) {
        return false;
    }
    log::info!("Fn+K: matched ({})", report_hex);
    let _ = cmd_tx.send(UiCommand::CyclePerfMode);
    true
}

/// 事件 hex 统一归一化：剔除所有非字母数字字符（如 "01-28-01" 的分隔符）
/// 并转大写。EventDetail 字节路径生成的是大写十六进制，但 ReportHex 字符串
/// 回退路径的字母大小写由固件决定，可能是小写——不归一化会导致小写报告
/// 永远匹配不上（F-FNK-04）。
fn normalize_hex(report_hex: &str) -> String {
    report_hex
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Fn+K 按下判定：类名一致 + 归一化报告以前缀 `012801` 开头。
/// 释放事件（`012800`）不命中此前缀，不会触发切换（F-FNK-06）。
fn is_fn_k_press(class_name: &str, report_hex: &str) -> bool {
    class_name == FN_K_WMI_CLASS && normalize_hex(report_hex).starts_with(FN_K_PRESS_PREFIX)
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
                let hex_str: String = bytes.iter().map(|b| format!("{:02X}", b)).collect();
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
    fn test_fn_k_press_matches() {
        assert!(is_fn_k_press("HID_EVENT20", "012801FFFF"));
    }

    #[test]
    fn test_fn_k_matches_with_separators() {
        // 报告带分隔符（"01-28-01" 形式，见文档 F-FNK-04）时同样能匹配。
        assert!(is_fn_k_press("HID_EVENT20", "01-28-01 00 00"));
    }

    #[test]
    fn test_fn_k_lowercase_report_normalized() {
        // 固件以小写提供 ReportHex（如 "012801ffff..."）时，归一化大写后必须能匹配。
        assert!(is_fn_k_press("HID_EVENT20", "012801ffff"));
    }

    #[test]
    fn test_fn_k_release_not_matched() {
        // 释放事件 012800 不命中按下前缀 012801，一次按键只触发一次切换（F-FNK-06）。
        assert!(!is_fn_k_press("HID_EVENT20", "012800"));
    }

    #[test]
    fn test_wrong_class_rejected() {
        assert!(!is_fn_k_press("HID_EVENT21", "012801"));
    }

    #[test]
    fn test_unmatched_prefix_rejected() {
        assert!(!is_fn_k_press("HID_EVENT20", "0120"));
        assert!(!is_fn_k_press("HID_EVENT20", "010701"));
    }

    #[test]
    fn test_handle_report_dispatches_cycle_perf_mode() {
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(handle_report("HID_EVENT20", "012801", &tx));
        match rx.try_recv() {
            Ok(UiCommand::CyclePerfMode) => {}
            other => panic!("Expected CyclePerfMode, got {:?}", other),
        }
    }

    #[test]
    fn test_handle_report_unmatched_sends_nothing() {
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(!handle_report("HID_EVENT20", "010701", &tx));
        assert!(rx.try_recv().is_err());
    }
}
