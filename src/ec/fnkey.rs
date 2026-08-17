//! Fn 功能键 WMI 事件监听（可自定义绑定）。
//!
//! 监听 OEM ACPI 事件类（`HID_EVENT20` 等），把固件报告（`EventDetail` /
//! `ReportHex`）与配置中的绑定表（`FnKeyBinding`）做**前缀匹配**，命中后
//! 派发对应的 `UiCommand`。
//!
//! 默认只有 Fn+K（`012801` → 循环切换性能模式），与历史硬编码行为一致；
//! 用户可在 GUI"Fn 功能键"设置中添加/修改/删除绑定，绑定表通过
//! `SharedBindings`（`Arc<RwLock<Vec<FnKeyBinding>>>`）与 GUI 线程共享——
//! 保存即即时生效，无需重启应用或重连监听。
//!
//! 事件类参考（Meow-Box / 本机 2025 RedmiBook Pro 14 实证）：HID_EVENT20
//! 承载 Fn+K 等按键报告；其余类（HID_EVENT21-23、WMIEvent）在不同机型/固件
//! 上承载不同的功能键事件。本实现只订阅绑定表中出现的事件类（绑定为空且
//! 未捕获时监听闲置，不浪费连接）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use windows::core::BSTR;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayUnaccessData,
};
use windows::Win32::System::Wmi::*;

use windows::Win32::System::Variant::{VARENUM, VT_ARRAY, VT_UI1};

use crate::command::UiCommand;

/// Fn+K 所在的 OEM ACPI 事件类（F-FNK-01）。
pub const FN_K_WMI_CLASS: &str = "HID_EVENT20";

/// Fn+K 按下事件的 ReportHex 前缀：`01-28-01`（固定前缀 `01` + 键码
/// `28` + 按下状态 `01`，见 F-FNK-04）。释放事件（`012800`）不命中
/// 此前缀，一次物理按键恰好派发一次切换（F-FNK-06）。
pub const FN_K_PRESS_PREFIX: &str = "012801";

/// Fn 键可绑定的动作（枚举名即配置文本，不得破坏性改名）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FnAction {
    /// 循环切换性能模式（Smart → Quiet → Extreme → Smart）。
    CyclePerfMode,
    /// 切换电池养护启用状态。
    ToggleBatteryCare,
    /// 把持久化配置整份重新应用到硬件（与"电源切换时自动重设"同一路径）。
    ReapplyConfig,
    /// 绑定保留但禁用（事件命中时被消费、不派发命令）。
    None,
}

impl FnAction {
    pub fn name(&self) -> &'static str {
        match self {
            Self::CyclePerfMode => "循环切换性能模式",
            Self::ToggleBatteryCare => "切换电池养护",
            Self::ReapplyConfig => "重新应用设置",
            Self::None => "无动作",
        }
    }

    pub fn all() -> &'static [FnAction] {
        &[
            Self::CyclePerfMode,
            Self::ToggleBatteryCare,
            Self::ReapplyConfig,
            Self::None,
        ]
    }

    /// 动作对应的 UI 命令；`None` 时返回 None（绑定仅消费事件，不派发）。
    /// 按 Rust 惯例命名：`as_*` 表示"便宜的借用读取"（`&self`），而 `to_*`
    /// 保留给消耗型转换——这里返回轻量 `Option<UiCommand>`，用 `as_` 前缀
    /// 顺带消除 clippy 的 `wrong_self_convention` 告警。
    pub fn as_ui_command(&self) -> Option<UiCommand> {
        match self {
            Self::CyclePerfMode => Some(UiCommand::CyclePerfMode),
            Self::ToggleBatteryCare => Some(UiCommand::ToggleBatteryCare),
            Self::ReapplyConfig => Some(UiCommand::ReapplyConfig),
            Self::None => None,
        }
    }
}

/// 一条 Fn 功能键绑定：事件类 + 报告前缀 → 动作。
///
/// 前缀匹配（`normalize_hex` 后 starts_with）：绑定的 `prefix` 是归一化
/// 十六进制（如 `012801`），事件报告归一化后以此为前缀即命中。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FnKeyBinding {
    /// OEM ACPI 事件类（如 `HID_EVENT20`）。
    pub class: String,
    /// 归一化事件前缀（如 `012801`）。
    pub prefix: String,
    /// 命中后派发的动作。
    pub action: FnAction,
}

impl FnKeyBinding {
    /// 默认的 Fn+K 绑定（与历史硬编码行为完全一致）。
    pub fn fn_k() -> Self {
        Self {
            class: FN_K_WMI_CLASS.to_string(),
            prefix: FN_K_PRESS_PREFIX.to_string(),
            action: FnAction::CyclePerfMode,
        }
    }

    /// GUI 展示标签，如 `HID_EVENT20 / 01-28-01`。
    pub fn label(&self) -> String {
        format!("{} / {}", self.class, Self::display_prefix(&self.prefix))
    }

    /// 归一化 hex → 带分隔符可读形式（`012801` → `01-28-01`），便于与
    /// 用户观察到的按键编码对照。
    pub fn display_prefix(prefix: &str) -> String {
        let p = normalize_hex(prefix);
        p.as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap_or("??"))
            .collect::<Vec<_>>()
            .join("-")
    }
}

/// 默认的功能键绑定（Fn+K → 循环切换性能）。
pub fn default_bindings() -> Vec<FnKeyBinding> {
    vec![FnKeyBinding::fn_k()]
}

/// 共享绑定表：GUI 线程写（保存配置时同步更新）、监听线程读。
pub type SharedBindings = std::sync::Arc<RwLock<Vec<FnKeyBinding>>>;

/// 已知的 Fn 键目录（用于 GUI"添加绑定"预设与捕获提示）。
///
/// 键码来自 Meow-Box 项目（Xiaomi Book Pro 14）与 F-FNK 文档（2025
/// RedmiBook Pro 14 实测）。同一事件类内不同键的编码；前缀取"按下"事件
/// 的最短可区分字节（含按下状态字节），避免释放/状态变化事件重复命中。
#[derive(Debug, Clone, Copy)]
pub struct KnownFnKey {
    /// 事件类名（`HID_EVENT20` 等）。
    pub class: &'static str,
    /// 归一化前缀。
    pub prefix: &'static str,
    /// 中文名。
    pub name: &'static str,
}

pub const KNOWN_FN_KEYS: &[KnownFnKey] = &[
    // Fn+K：用按下状态字节完整前缀（012801），避免释放事件（012800）
    // 也命中造成一次按键派发两次动作。
    KnownFnKey {
        class: "HID_EVENT20",
        prefix: "012801",
        name: "Fn+K 性能模式",
    },
    KnownFnKey {
        class: "HID_EVENT20",
        prefix: "0125",
        name: "PC Manager 键",
    },
    KnownFnKey {
        class: "HID_EVENT20",
        prefix: "0123",
        name: "小爱同学 (F7)",
    },
    KnownFnKey {
        class: "HID_EVENT20",
        prefix: "011B",
        name: "设置 (F9)",
    },
    KnownFnKey {
        class: "HID_EVENT20",
        prefix: "0101",
        name: "投影 (F8)",
    },
    KnownFnKey {
        class: "HID_EVENT20",
        prefix: "0121",
        name: "麦克风静音 (F4)",
    },
    KnownFnKey {
        class: "HID_EVENT20",
        prefix: "0107",
        name: "Fn 锁 (Fn+Esc)",
    },
    KnownFnKey {
        class: "HID_EVENT20",
        prefix: "0109",
        name: "大写锁定",
    },
];

struct SafeEnumerator(IEnumWbemClassObject);
// SAFETY: SafeEnumerator is only used on the dedicated Fn watcher thread.
// COM is initialized in MTA on that thread, and the enumerator is never
// accessed from any other thread.
unsafe impl Send for SafeEnumerator {}

/// 启动 Fn 功能键监听线程。
///
/// - `bindings`：共享绑定表（GUI 保存配置时同步更新，即时生效）；
/// - `capture`：捕获开关。开启后，收到的**每条**事件都以
///   `UiCommand::FnEventSeen { class, hex }` 发送给 GUI 展示，方便用户
///   观察真实键码、配置新绑定。
pub fn spawn(
    cmd_tx: mpsc::Sender<UiCommand>,
    bindings: SharedBindings,
    capture: Arc<AtomicBool>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        // 注入 egui Context 到本线程线程局部存储：dispatch 发送命令后用它
        // 唤醒隐藏的 GUI 事件循环（与托盘 send_command 同理）。
        WATCHER_CTX.with(|c| *c.borrow_mut() = Some(ctx));
        // Fn 监听线程生命周期日志：该线程本应无限运行（内部是无限重试
        // 循环），正常情况下只有进程退出才结束。记录 start/end 两端的
        // 时间点，"Fn 静默失效"时能确认线程是否还活着。
        log::info!("Fn watcher thread started");
        if let Err(e) = run_watcher(&cmd_tx, &bindings, &capture) {
            log::error!("Fn watcher: {}", e);
        }
        log::info!("Fn watcher thread exited");
    });
}

// 发送命令并立即唤醒 GUI 事件循环：把命令延迟压到最小（egui 的 mpsc 不
// 唤醒事件循环，500ms 定时帧是兜底；发送后 request_repaint 即时消费，
// 托盘/Fn 交互响应更快）。窗口离屏隐藏时 update 循环仍运行，命令同样
// 会被消费（见 platform::window 的修订 1.19）。
thread_local! {
    static WATCHER_CTX: std::cell::RefCell<Option<egui::Context>> =
        const { std::cell::RefCell::new(None) };
}

fn send_watcher_command(cmd_tx: &mpsc::Sender<UiCommand>, cmd: UiCommand) {
    if let Err(e) = cmd_tx.send(cmd) {
        log::warn!("Fn watcher: command send failed: {}", e);
    }
    WATCHER_CTX.with(|c| {
        if let Some(ctx) = c.borrow().as_ref() {
            ctx.request_repaint();
        }
    });
}

/// 从绑定表提取需要订阅的事件类集合（去重、稳定排序）。
fn binding_classes(bindings: &[FnKeyBinding]) -> Vec<String> {
    let mut v: Vec<String> = bindings.iter().map(|b| b.class.clone()).collect();
    v.sort();
    v.dedup();
    v
}

/// 捕获模式下需要订阅的事件类集合：绑定表中的类 ∪ 全部已知类的并集
/// （KNOWN_FN_KEYS 的 class 去重、稳定排序）。
///
/// 捕获的目的正是"发现未绑定的新键"（F-FNK-12）：若只订阅绑定表中的类，
/// 用户删除全部绑定后捕获将收不到任何事件（实测修正，修订 1.22）。
fn capture_classes(bindings: &[FnKeyBinding]) -> Vec<String> {
    let mut v = binding_classes(bindings);
    for k in KNOWN_FN_KEYS {
        if !v.contains(&k.class.to_string()) {
            v.push(k.class.to_string());
        }
    }
    v.sort();
    v.dedup();
    v
}

/// 订阅给定的事件类；不存在的类会被 ExecNotificationQuery 拒绝并跳过。
/// 返回成功订阅的 (类名, 枚举器) 列表。
fn subscribe(services: &IWbemServices, classes: &[String]) -> Vec<(String, SafeEnumerator)> {
    classes
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
                    log::info!("Fn: subscribed to {}", class_name);
                    Some((class_name.clone(), SafeEnumerator(e)))
                }
                Err(_) => {
                    log::warn!("Fn: cannot subscribe to {} (not available)", class_name);
                    None
                }
            }
        })
        .collect()
}

/// run_watcher_once 的退出原因（供外层 run_watcher 决定重试节奏）。
enum WatcherError {
    /// 本周期内从未订阅到任何事件类（连续 30s 空订阅）：最可能是本机
    /// 根本没有这些 OEM 事件类（如非小米机型），重建连接也无法改变——
    /// 需要退避，避免无限高频重建连接并刷屏日志。
    NoEventClasses,
    /// 连接/订阅阶段失败，或订阅后连接失效（Next 失败后重订阅仍为空）：
    /// 多为瞬态（WMI 服务尚未就绪、OEM 驱动加载较晚、服务重启、休眠
    /// 唤醒），保持快速重试等待恢复。
    Reconnect(String),
}

/// Fn 监听主循环（可重入）：COM 初始化、连接 root\wmi、订阅事件类都在
/// 这里完成。连接阶段的任何失败以及运行期连接失效（Next 失败后重订阅仍
/// 无结果、空订阅持续 30s）都会返回 Err 由外层 run_watcher 延时重试。
fn run_watcher_once(
    cmd_tx: &mpsc::Sender<UiCommand>,
    bindings: &SharedBindings,
    capture: &AtomicBool,
) -> Result<(), WatcherError> {
    // COM 公寓初始化与每次退出的 CoUninitialize 严格配对（见历史注释：
    // 高频重试周期内重复 init/uninit 不泄漏，但配对是正确模式）。
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|e| WatcherError::Reconnect(format!("COM init: {}", e)))?;
    }
    let result = run_watcher_loop(cmd_tx, bindings, capture);
    unsafe {
        CoUninitialize();
    }
    result
}

fn run_watcher_loop(
    cmd_tx: &mpsc::Sender<UiCommand>,
    bindings: &SharedBindings,
    capture: &AtomicBool,
) -> Result<(), WatcherError> {
    let services = crate::ec::wmi_util::connect_root_wmi().map_err(WatcherError::Reconnect)?;

    log::info!("Fn watcher connected to root\\wmi");

    let mut enumerators: Vec<(String, SafeEnumerator)> = Vec::new();
    // 当前已订阅的类集合（用于检测绑定变化后重订阅）。
    let mut subscribed_classes: Vec<String> = Vec::new();
    let mut empty_streak: u32 = 0;

    loop {
        // 绑定表变化（GUI 添加/删除绑定 → 事件类集合可能变化）时重订阅。
        // 捕获模式下额外订阅全部已知类（见 capture_classes 注释）。
        let capturing = capture.load(Ordering::Relaxed);
        let classes = if capturing {
            capture_classes(&lock_or_recover_bindings(bindings))
        } else {
            binding_classes(&lock_or_recover_bindings(bindings))
        };
        if classes != subscribed_classes {
            subscribed_classes = classes.clone();
            enumerators = subscribe(&services, &classes);
            empty_streak = 0;
        }

        if enumerators.is_empty() {
            empty_streak += 1;
            // 没有绑定且未捕获时，没有订阅任何类是正常状态：空转等待
            // 新绑定（GUI 添加绑定后下一轮即订阅），不刷屏日志。
            if subscribed_classes.is_empty() && !capturing {
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
            if empty_streak >= 6 {
                return Err(WatcherError::NoEventClasses);
            }
            log::warn!(
                "Fn: no event classes available; retrying in 5s (capture={})",
                capturing
            );
            std::thread::sleep(std::time::Duration::from_secs(5));
            continue;
        }
        empty_streak = 0;

        let mut resubscribe = false;
        for (class_name, SafeEnumerator(ref enumerator)) in &enumerators {
            let mut objects: [Option<IWbemClassObject>; 1] = [None];
            let mut returned: u32 = 0;

            let hr = unsafe { enumerator.Next(100, &mut objects, &mut returned as *mut u32) };

            if hr.is_err() {
                log::warn!(
                    "Fn: IEnumWbemClassObject::Next failed (hr=0x{:08X}); resubscribing in 1s",
                    hr.0 as u32
                );
                std::thread::sleep(std::time::Duration::from_secs(1));
                resubscribe = true;
                break;
            }

            if returned == 0 {
                continue;
            }

            if let Some(ref obj) = objects[0] {
                process_event(obj, class_name, bindings, capture, cmd_tx);
            }
        }
        if resubscribe {
            enumerators = subscribe(&services, &subscribed_classes);
            if enumerators.is_empty() {
                return Err(WatcherError::Reconnect(
                    "WMI enumerator failed and resubscribe returned nothing; rebuilding connection"
                        .to_string(),
                ));
            }
        }
    }
}

fn run_watcher(
    cmd_tx: &mpsc::Sender<UiCommand>,
    bindings: &SharedBindings,
    capture: &AtomicBool,
) -> Result<(), String> {
    let mut stale_cycles: u32 = 0;
    loop {
        match run_watcher_once(cmd_tx, bindings, capture) {
            Err(WatcherError::NoEventClasses) => {
                stale_cycles += 1;
                let delay = if stale_cycles <= 3 { 5u64 } else { 30u64 };
                log::warn!(
                    "Fn: no event classes for {} consecutive cycle(s); retrying in {}s",
                    stale_cycles,
                    delay
                );
                std::thread::sleep(std::time::Duration::from_secs(delay));
            }
            Err(WatcherError::Reconnect(e)) => {
                stale_cycles = 0;
                log::warn!("Fn watcher startup failed: {}; retrying in 5s", e);
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
            Ok(()) => return Ok(()),
        }
    }
}

/// 读共享绑定表；毒锁（GUI 线程在临界区内 panic）时恢复并告警。
/// 恢复为"解出原始数据"而非空列表：panic 后数据可能不一致，但对
/// "绑定表"这类配置场景，恢复现有数据比丢配置更可取（与 util.rs 的
/// lock_or_recover 语义一致）。
fn lock_or_recover_bindings(
    bindings: &SharedBindings,
) -> std::sync::RwLockReadGuard<'_, Vec<FnKeyBinding>> {
    bindings.read().unwrap_or_else(|e| {
        log::warn!("fn bindings lock poisoned, recovering");
        e.into_inner()
    })
}

fn process_event(
    obj: &IWbemClassObject,
    class_name: &str,
    bindings: &SharedBindings,
    capture: &AtomicBool,
    cmd_tx: &mpsc::Sender<UiCommand>,
) {
    let Some(report_hex) = get_detail_hex(obj).or_else(|| get_string_prop(obj, "ReportHex")) else {
        log::debug!("Fn [{}]: no EventDetail/ReportHex", class_name);
        return;
    };
    let normalized = normalize_hex(&report_hex);
    log::debug!(
        "Fn [{}]: EventDetail={} (normalized {})",
        class_name,
        report_hex,
        normalized
    );

    // 捕获模式：每条事件转发给 GUI（用于发现键码、配置绑定）。
    if capture.load(Ordering::Relaxed) {
        send_watcher_command(
            cmd_tx,
            UiCommand::FnEventSeen {
                class: class_name.to_string(),
                hex: normalized.clone(),
            },
        );
    }

    if dispatch_bindings(class_name, &normalized, bindings, cmd_tx) {
        return;
    }
    // 其余事件（未绑定或 Fn 锁等，见 F-FNK-09）不产生任何动作，仅记录日志。
    log::debug!("Fn [{}]: unmatched event {}", class_name, normalized);
}

/// 与绑定表做前缀匹配并派发动作。命中第一条绑定即消费（与 Meow-Box 的
/// "first matching binding" 语义一致），`None` 动作的绑定同样消费（禁用）。
fn dispatch_bindings(
    class_name: &str,
    normalized: &str,
    bindings: &SharedBindings,
    cmd_tx: &mpsc::Sender<UiCommand>,
) -> bool {
    for binding in lock_or_recover_bindings(bindings).iter() {
        if binding.class != class_name {
            continue;
        }
        let prefix = normalize_hex(&binding.prefix);
        if prefix.is_empty() || !normalized.starts_with(&prefix) {
            continue;
        }
        log::info!("Fn: matched {} -> {}", binding.label(), normalized);
        if let Some(cmd) = binding.action.as_ui_command() {
            send_watcher_command(cmd_tx, cmd);
        } else {
            log::debug!("Fn: binding {} has no action; consumed", binding.label());
        }
        return true;
    }
    false
}

/// 事件 hex 统一归一化：剔除所有非字母数字字符（如 "01-28-01" 的分隔符）
/// 并转大写。EventDetail 字节路径生成的是大写十六进制，但 ReportHex 字符串
/// 回退路径的字母大小写由固件决定，可能是小写——不归一化会导致小写报告
/// 永远匹配不上（F-FNK-04）。
/// 归一化 hex 到大写无分隔形式：剔除分隔符等非十六进制字符并转大写。
/// 只保留 `[0-9A-F]`——事件报告与绑定前缀都是 hex，混入其它字母
/// （如 G-Z）既不可能匹配真实事件，也会让"非 hex 输入"检测失效。
/// 不含任何十六进制字符时返回空串（调用方据此拒绝非法输入）。
pub fn normalize_hex(report_hex: &str) -> String {
    report_hex
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// 将 VT_UI1 SAFEARRAY 的数据缓冲转为大写十六进制字符串。
///
/// `from_raw_parts` 要求指针非空且对齐，即使长度为 0 也如此：空数组时
/// `SafeArrayAccessData` 成功返回的 `data` 可能为空指针，直接构造 0 长度
/// 切片属于 UB。这里在长度为 0 时返回 None，由调用方按"无数据"处理。
fn bytes_to_hex(data: *const u8, len: usize) -> Option<String> {
    if len == 0 || data.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    Some(bytes.iter().map(|b| format!("{:02X}", b)).collect())
}

fn get_detail_hex(obj: &IWbemClassObject) -> Option<String> {
    // 属性值由 OwnedVariant 在 Drop 时自动释放（VARIANT 及它持有的
    // SAFEARRAY/BSTR），无需手动 VariantClear。
    let val = crate::ec::wmi_util::get_property(obj, "EventDetail")?;
    let vt = unsafe { val.Anonymous.Anonymous.vt };

    if vt == VARENUM(VT_ARRAY.0 | VT_UI1.0) {
        let sa = unsafe { val.Anonymous.Anonymous.Anonymous.parray };
        if sa.is_null() {
            return None;
        }
        let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
        if unsafe { SafeArrayAccessData(sa, &mut data) }.is_err() {
            return None;
        }
        let lbound = unsafe { SafeArrayGetLBound(sa, 1) }.unwrap_or(0);
        let ubound = unsafe { SafeArrayGetUBound(sa, 1) }.unwrap_or(-1);
        let len = ubound.saturating_sub(lbound).saturating_add(1) as usize;
        let hex_str = bytes_to_hex(data as *const u8, len);
        unsafe { SafeArrayUnaccessData(sa).ok() };
        hex_str
    } else {
        unsafe { crate::ec::wmi_util::bstr_from_variant(&val) }
    }
}

fn get_string_prop(obj: &IWbemClassObject, name: &str) -> Option<String> {
    let val = crate::ec::wmi_util::get_property(obj, name)?;
    unsafe { crate::ec::wmi_util::bstr_from_variant(&val) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_bindings() -> SharedBindings {
        std::sync::Arc::new(RwLock::new(default_bindings()))
    }

    /// 默认绑定表必须包含 Fn+K → 循环切换性能（与历史行为一致）。
    #[test]
    fn test_default_bindings_has_fn_k() {
        let b = default_bindings();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].class, "HID_EVENT20");
        assert_eq!(b[0].prefix, "012801");
        assert_eq!(b[0].action, FnAction::CyclePerfMode);
    }

    /// 归一化必须能匹配带分隔符/大小写的报告。
    #[test]
    fn test_normalize_hex() {
        assert_eq!(normalize_hex("01-28-01 00 00"), "0128010000");
        assert_eq!(normalize_hex("012801ffff"), "012801FFFF");
        assert_eq!(normalize_hex(""), "");
    }

    /// 绑定前缀匹配：按下命中、释放不命中（F-FNK-06）、类不匹配不命中。
    #[test]
    fn test_dispatch_binding_match_semantics() {
        let bindings = test_bindings();
        let (tx, _rx) = mpsc::channel();

        // Fn+K 按下（012801）命中。
        assert!(dispatch_bindings(
            "HID_EVENT20",
            "012801FFFF",
            &bindings,
            &tx
        ));
        // 释放（012800）不命中按下前缀。
        assert!(!dispatch_bindings("HID_EVENT20", "012800", &bindings, &tx));
        // 类不匹配不命中。
        assert!(!dispatch_bindings("HID_EVENT21", "012801", &bindings, &tx));
        // 其它键（如 Fn+Esc 0107）不命中。
        assert!(!dispatch_bindings("HID_EVENT20", "010701", &bindings, &tx));
    }

    /// 命中绑定必须派发对应的 UiCommand。
    #[test]
    fn test_dispatch_sends_ui_command() {
        let bindings = test_bindings();
        let (tx, rx) = mpsc::channel();
        assert!(dispatch_bindings("HID_EVENT20", "012801", &bindings, &tx));
        match rx.try_recv() {
            Ok(UiCommand::CyclePerfMode) => {}
            other => panic!("Expected CyclePerfMode, got {:?}", other),
        }
    }

    /// 绑定消费但无动作：命中返回 true 但不派发命令。
    #[test]
    fn test_dispatch_none_action_consumes_without_sending() {
        let bindings: SharedBindings = std::sync::Arc::new(RwLock::new(vec![FnKeyBinding {
            class: "HID_EVENT20".into(),
            prefix: "0107".into(),
            action: FnAction::None,
        }]));
        let (tx, rx) = mpsc::channel();
        assert!(dispatch_bindings("HID_EVENT20", "010701", &bindings, &tx));
        assert!(rx.try_recv().is_err());
    }

    /// 自定义绑定：任意类/前缀 → 任意动作。
    #[test]
    fn test_dispatch_custom_binding() {
        let bindings = Arc::new(RwLock::new(vec![FnKeyBinding {
            class: "HID_EVENT20".into(),
            prefix: "0123".into(),
            action: FnAction::ToggleBatteryCare,
        }]));
        let (tx, rx) = mpsc::channel();
        assert!(dispatch_bindings("HID_EVENT20", "012301", &bindings, &tx));
        match rx.try_recv() {
            Ok(UiCommand::ToggleBatteryCare) => {}
            other => panic!("Expected ToggleBatteryCare, got {:?}", other),
        }
    }

    /// 绑定事件类型去重。
    #[test]
    fn test_binding_classes_deduplicated() {
        let bindings = vec![
            FnKeyBinding::fn_k(),
            FnKeyBinding {
                class: "HID_EVENT20".into(),
                prefix: "0107".into(),
                action: FnAction::None,
            },
            FnKeyBinding {
                class: "HID_EVENT21".into(),
                prefix: "FF".into(),
                action: FnAction::ReapplyConfig,
            },
        ];
        assert_eq!(
            binding_classes(&bindings),
            vec!["HID_EVENT20", "HID_EVENT21"]
        );
    }

    /// 捕获模式的订阅类 = 绑定类 ∪ 已知类（去重、排序）：
    /// 删除全部绑定后捕获仍订阅已知类，否则无法"发现新键"（修订 1.22）。
    #[test]
    fn test_capture_classes_include_known_when_bindings_empty() {
        // 空绑定表：捕获模式必须仍覆盖全部已知类。
        let empty: Vec<FnKeyBinding> = Vec::new();
        let cap = capture_classes(&empty);
        assert!(
            cap.contains(&"HID_EVENT20".to_string()),
            "capture with empty bindings must still subscribe to known classes"
        );
        // 已知类去重且排序稳定。
        assert_eq!(cap, {
            let mut c = cap.clone();
            c.sort();
            c.dedup();
            c
        });
    }

    #[test]
    fn test_capture_classes_merge_bindings_and_known() {
        let bindings = vec![FnKeyBinding {
            class: "HID_EVENT21".into(),
            prefix: "FF".into(),
            action: FnAction::ReapplyConfig,
        }];
        let cap = capture_classes(&bindings);
        // 绑定中的类 + 已知类的类（HID_EVENT20）都被覆盖。
        assert!(cap.contains(&"HID_EVENT21".to_string()));
        assert!(cap.contains(&"HID_EVENT20".to_string()));
    }

    /// 非捕获模式（capture 关闭）的订阅类只来自绑定表：不额外订阅已知类，
    /// 避免未绑定的键也触发 WMI 事件流量。
    #[test]
    fn test_non_capture_bindings_only() {
        let bindings = vec![FnKeyBinding::fn_k()];
        assert_eq!(binding_classes(&bindings), vec!["HID_EVENT20"]);
    }

    /// 空 SAFEARRAY（长度为 0、指针可能为空）不得构造 0 长度切片（UB），
    /// 应返回 None 而非崩溃。
    #[test]
    fn test_bytes_to_hex_empty_buffer_is_none() {
        assert_eq!(bytes_to_hex(std::ptr::null(), 0), None);
        assert_eq!(bytes_to_hex(std::ptr::dangling::<u8>(), 0), None);
    }

    #[test]
    fn test_bytes_to_hex_non_empty() {
        let data = [0x01u8, 0x28, 0x01, 0x00, 0xFF];
        let hex = bytes_to_hex(data.as_ptr(), data.len()).expect("non-empty data");
        assert_eq!(hex, "01280100FF");
    }

    #[test]
    fn test_bytes_to_hex_null_nonzero_len_is_none() {
        assert_eq!(bytes_to_hex(std::ptr::null(), 4), None);
    }

    /// 展示形式：归一化前缀 → 带分隔符。
    #[test]
    fn test_display_prefix() {
        assert_eq!(FnKeyBinding::display_prefix("012801"), "01-28-01");
        assert_eq!(FnKeyBinding::display_prefix("0107"), "01-07");
    }

    /// 已知功能键目录：编码来自 Meow-Box（HID_EVENT20 类），不得为空。
    #[test]
    fn test_known_fn_keys_non_empty_and_distinct() {
        assert!(!KNOWN_FN_KEYS.is_empty());
        let mut set = std::collections::HashSet::new();
        for k in KNOWN_FN_KEYS {
            assert!(!k.prefix.is_empty());
            assert!(set.insert((k.class, k.prefix)));
        }
    }
}
