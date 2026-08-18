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

/// 进程级"最后执行的自定义命令"防抖表（见 `debounce_duplicate`）。
use std::collections::HashMap;

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
    /// 运行用户自定义命令（脚本 / 打开程序，见 `FnKeyBinding.command`）。
    /// 命令在**独立进程**中执行（`std::process::Command`），不阻塞 WMI
    /// 事件监听循环（修订 1.26 规划的功能，本轮实现）。
    RunCommand,
    /// 绑定保留但禁用（无动作时被消费、不派发命令）。
    None,
}

impl FnAction {
    pub fn name(&self) -> &'static str {
        match self {
            Self::CyclePerfMode => "循环切换性能模式",
            Self::ToggleBatteryCare => "切换电池养护",
            Self::ReapplyConfig => "重新应用设置",
            Self::RunCommand => "运行自定义命令",
            Self::None => "无动作",
        }
    }

    pub fn all() -> &'static [FnAction] {
        &[
            Self::CyclePerfMode,
            Self::ToggleBatteryCare,
            Self::ReapplyConfig,
            Self::RunCommand,
            Self::None,
        ]
    }

    /// 动作对应的 UI 命令；`RunCommand`/`None` 时返回 None（绑定仅消费
    /// 事件，不派发——前者直接执行命令，后者无动作）。
    /// 按 Rust 惯例命名：`as_*` 表示"便宜的借用读取"（`&self`），而 `to_*`
    /// 保留给消耗型转换——这里返回轻量 `Option<UiCommand>`，用 `as_` 前缀
    /// 顺带消除 clippy 的 `wrong_self_convention` 告警。
    pub fn as_ui_command(&self) -> Option<UiCommand> {
        match self {
            Self::CyclePerfMode => Some(UiCommand::CyclePerfMode),
            Self::ToggleBatteryCare => Some(UiCommand::ToggleBatteryCare),
            // 用户主动绑定"重新应用设置"是手动动作，不应受"电源切换时自动
            // 重设"开关门控——开关关闭时被动重设（电源广播）静默忽略是对的，
            // 但用户按下绑定的功能键却毫无反应是不可接受（修订 1.30 M3 回归）。
            Self::ReapplyConfig => Some(UiCommand::ReapplyConfigManual),
            Self::RunCommand | Self::None => None,
        }
    }
}

/// 一条 Fn 功能键绑定：事件类 + 报告前缀 → 动作（可能带自定义命令）。
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
    /// `RunCommand` 动作的命令行（其余动作忽略）。`None`/空串 = 未配置；
    /// 配置向后兼容：旧配置文件无此字段时反序列化为 `None`（`#[serde(default)]`）。
    #[serde(default)]
    pub command: Option<String>,
}

impl FnKeyBinding {
    /// 默认的 Fn+K 绑定（与历史硬编码行为完全一致）。
    pub fn fn_k() -> Self {
        Self {
            class: FN_K_WMI_CLASS.to_string(),
            prefix: FN_K_PRESS_PREFIX.to_string(),
            action: FnAction::CyclePerfMode,
            command: None,
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
        // catch_unwind（修订 1.33）：release 已移除 panic=abort（1.32），
        // 本线程若 panic 只会静默终止该线程——Fn 监听失效而应用仍存活，
        // 用户毫无感知。兜底：panic 被捕获并记录语义化错误（含消息），
        // 与 M4（后端 init 线程 catch_unwind）设计一致；run_watcher 本身
        // 已有错误重试，这里只兜住 panic 级故障。
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_watcher(&cmd_tx, &bindings, &capture)
        }));
        match result {
            Ok(Err(e)) => log::error!("Fn watcher: {}", e),
            Ok(Ok(())) => {}
            Err(panic) => {
                let payload = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown panic".into());
                log::error!("Fn watcher panicked: {}", payload);
            }
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
            // 没有绑定且未捕获时，没有订阅任何类是正常状态：空转等待
            // 新绑定（GUI 添加绑定后下一轮即订阅），不刷屏日志。
            if subscribed_classes.is_empty() && !capturing {
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
            empty_streak += 1;
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
                // 退避节奏（修订 1.32/L4，1.33 修正）：前 3 次 5s、随后 30s、持续
                // 无类后降为 60s 长探测——硬件上没有这些 OEM 事件类
                // （VM / 非小米机型 / 绑定了不存在的类）时，类永远不会
                // 出现，30s 一次的 warn 刷屏毫无价值；长探测保留"驱动
                // 后来安装/类后来出现"的恢复通道，同时把日志频率压到
                // 可忽略。60s（而非更长的 600s）保证 GUI 增删绑定/开关
                // 捕获后最多一分钟内监听线程就能读到新状态，不会出现
                // 用户操作后长时间无响应的假死感（回归修正）。空转
                // （无绑定）场景由 run_watcher_loop 内部的 1s continue
                // 承担，不会走到这里（M1 回归）。
                let delay = if stale_cycles <= 3 {
                    5
                } else if stale_cycles <= 20 {
                    30
                } else {
                    60
                };
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
        // 转发限流（修订 1.32/L2，1.33 修正）：按住键触发固件自动重复时
        // 同一键身份（class + 去末状态字节的 hex）的事件是连续流，逐条
        // 转发会塞满无界 mpsc、每事件唤醒 GUI 重绘。窗口内同一身份只
        // 转发最新一条；**不同按键不受影响**（各自独立去重，快速连按
        // 多个功能键不会漏键）。
        if capture_event_gate(class_name, &normalized) {
            send_watcher_command(
                cmd_tx,
                UiCommand::FnEventSeen {
                    class: class_name.to_string(),
                    hex: normalized.clone(),
                },
            );
        }
    }

    if dispatch_bindings(class_name, &normalized, bindings, cmd_tx) {
        return;
    }
    // 其余事件（未绑定或 Fn 锁等，见 F-FNK-09）不产生任何动作，仅记录日志。
    log::debug!("Fn [{}]: unmatched event {}", class_name, normalized);
}

/// 捕获事件转发限流闸门（修订 1.32/L2，1.33 改按键维度）。
///
/// 返回是否允许本次转发。**按"键身份"去重而非全局窗口**（回归修正）：
/// 同一物理按键的按下/释放/自动重复事件（相同 class + 去掉末状态字节的
/// hex，如 `012801`/`012800` 身份同为 `0128`）在窗口内只转发第一条——
/// 按住触发固件自动重复时不再每事件唤醒 GUI；而**不同**按键（身份不同）
/// 即使 150ms 内连续按下也各自放行，捕获工具不会漏键。线程局部存储
/// （监听线程专用），身份首次出现必然放行。
const CAPTURE_FORWARD_MIN_MS: u128 = 150;

fn capture_event_gate(class: &str, normalized: &str) -> bool {
    thread_local! {
        static LAST_FORWARD: std::cell::RefCell<std::collections::HashMap<String, std::time::Instant>> =
            std::cell::RefCell::new(std::collections::HashMap::new());
    }
    // 键身份 = 类名 + 去掉末状态字节的 hex（与 keep_press_over_release 的
    // "去掉末字节后相同即同键码"判定一致）。长度不足 2 时退化为完整 hex。
    let identity = if normalized.len() >= 2 {
        format!("{}/{}", class, &normalized[..normalized.len() - 2])
    } else {
        format!("{}/{}", class, normalized)
    };
    LAST_FORWARD.with(|last| {
        let now = std::time::Instant::now();
        // 清理过期条目（窗口外的身份不再占用内存；身份数 = 窗口内不同键数，
        // 有界且极小）。
        last.borrow_mut()
            .retain(|_, t| now.duration_since(*t).as_millis() < CAPTURE_FORWARD_MIN_MS);
        let mut map = last.borrow_mut();
        let allow = match map.get(&identity) {
            Some(prev) => now.duration_since(*prev).as_millis() >= CAPTURE_FORWARD_MIN_MS,
            None => true,
        };
        if allow {
            map.insert(identity, now);
        }
        allow
    })
}

/// 与绑定表做前缀匹配并派发动作。命中第一条绑定即消费（与 Meow-Box 的
/// "first matching binding" 语义一致），`None` 动作的绑定同样消费（禁用）。
fn dispatch_bindings(
    class_name: &str,
    normalized: &str,
    bindings: &SharedBindings,
    cmd_tx: &mpsc::Sender<UiCommand>,
) -> bool {
    // 先复制出命中绑定的动作数据再释放读锁：避免在持锁期间做跨线程
    // spawn（失败即 panic，会毒化共享锁并永久杀死监听线程——L1 回归）。
    let matched: Option<(String, FnAction, Option<String>)> = lock_or_recover_bindings(bindings)
        .iter()
        .find(|b| {
            b.class == class_name && {
                let prefix = normalize_hex(&b.prefix);
                !prefix.is_empty() && normalized.starts_with(&prefix)
            }
        })
        .map(|b| (b.class.clone(), b.action, b.command.clone()));
    let Some((binding_class, action, command)) = matched else {
        return false;
    };
    log::info!(
        "Fn: matched {} / {} -> {}",
        binding_class,
        normalized,
        action.name()
    );
    // 自定义命令：以独立进程执行（不阻塞监听线程），无 UiCommand。
    if action == FnAction::RunCommand {
        run_external_command(command.as_deref());
        return true;
    }
    if let Some(cmd) = action.as_ui_command() {
        send_watcher_command(cmd_tx, cmd);
    } else {
        log::debug!("Fn: binding {} has no action; consumed", action.name());
    }
    true
}

/// 以**独立进程**执行自定义命令（`RunCommand` 动作），不阻塞 WMI 事件
/// 监听循环（修订 1.26 规划：`std::process::Command` 独立启动）。
///
/// 实现细节：
/// - 命令未配置（None/空白）时仅告警，不产生任何动作（空命令无意义，
///   且以空字符串启动进程属编程错误）；
/// - `cmd.exe /C <command>` 承载（Windows 惯例的进程启动器，天然支持
///   带引号路径 / 参数 / 批处理 / `start` 内建命令）；
/// - `CREATE_NO_WINDOW` 隐藏控制台窗口：本应用是 GUI 程序，直接启动
///   cmd.exe 会闪现黑框，影响体验；
/// - 以 `CreationFlags` 隐藏窗口 + 后台线程执行：`Command::spawn` 返回
///   后不等待子进程退出（detached 语义），长时间运行的脚本不会阻塞应用；
/// - **同命令防抖**（L2 回归，修订 1.30）：固件可能对一次物理按键重复
///   上报（或用户在系统设置开启键自动重复），每条事件各起一个线程 +
///   cmd.exe 会瞬间堆积进程。对**相同命令**在
///   `RUN_COMMAND_DEBOUNCE` 时间窗内的重复派发直接丢弃（不同命令仍可并发）。
///
/// **防抖状态是进程级全局的**：设计上每条命令对应的防抖窗口独立，简单
/// 的"全局最后命令"哈希表即可满足，且无需清理（键数量 = 绑定数，有界）。
/// `HashMap<String, Instant>` + `Mutex`，跨线程安全。
/// `RunCommand` 防抖窗口：同一命令在此窗口内不重复启动（毫秒）。
const RUN_COMMAND_DEBOUNCE_MS: u64 = 1000;

static LAST_RUN_COMMANDS: std::sync::OnceLock<
    std::sync::Mutex<HashMap<String, std::time::Instant>>,
> = std::sync::OnceLock::new();

fn last_run_commands() -> &'static std::sync::Mutex<HashMap<String, std::time::Instant>> {
    LAST_RUN_COMMANDS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// 判断命令是否处于防抖窗口（相同命令在 `RUN_COMMAND_DEBOUNCE_MS` 内的
/// 重复派发）。首次调用返回 false 并记录时间；窗口内重复返回 true。
/// 防抖锁毒化（罕见）时放行（不阻断命令执行）。
fn debounce_duplicate(command: &str) -> bool {
    let now = std::time::Instant::now();
    let duplicate = std::sync::Mutex::lock(last_run_commands())
        .map(|mut m| {
            if let Some(last) = m.get(command) {
                if now.duration_since(*last)
                    < std::time::Duration::from_millis(RUN_COMMAND_DEBOUNCE_MS)
                {
                    return true;
                }
            }
            m.insert(command.to_string(), now);
            false
        })
        .unwrap_or_else(|_| {
            log::warn!("Fn: last-run-command lock poisoned; proceeding");
            false
        });
    if duplicate {
        log::debug!(
            "Fn: external command debounced (repeated within {}ms)",
            RUN_COMMAND_DEBOUNCE_MS
        );
    }
    duplicate
}

fn run_external_command(command: Option<&str>) {
    let Some(command) = command.map(str::trim).filter(|c| !c.is_empty()) else {
        log::warn!("Fn: RunCommand binding with empty command; skipped");
        return;
    };
    // info 级脱敏（RUST_LOG=info 是默认，日志文件可能被转发/提交）；完整
    // 命令仅在用户显式开启 debug 时可见（RUST_LOG=debug），排查带引号路径
    // 等问题时再开。
    log::info!("Fn: running external command: {}", redact_command(command));
    log::debug!("Fn: running external command (full): {}", command);
    // 转换为拥有所有权的 String 再移入线程（闭包参数 Option<&str> 借用
    // 自调用方，跨线程 move 会触发 E0521；所有权的 String 无生命周期约束）。
    let command = command.to_owned();

    // 防抖：相同命令在 RUN_COMMAND_DEBOUNCE 内重复触发时丢弃。
    if debounce_duplicate(&command) {
        return;
    }

    // 线程名带命令前缀便于排查（调试器/任务管理器线程列表）；Builder 而非
    // `thread::spawn`：spawn 失败（OS 线程资源耗尽）时记录告警而非 panic
    // 传播——监听线程在该路径无 catch_unwind，panic 会静默杀死监听（L1）。
    let spawn_result = std::thread::Builder::new()
        .name("fn-runcommand".to_string())
        .spawn(move || {
            // CREATE_NO_WINDOW = 0x08000000：不创建控制台窗口。
            let mut cmd = std::process::Command::new("cmd.exe");
            cmd.arg("/C");
            cmd.arg(&command);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000);
            }
            match cmd.spawn() {
                Ok(_child) => {
                    // 分离执行，不等待子进程。
                    log::debug!("Fn: external command spawned");
                }
                Err(e) => log::warn!("Fn: external command spawn failed: {}", e),
            }
        });
    if let Err(e) = spawn_result {
        log::warn!("Fn: failed to spawn command thread: {}", e);
    }
}

/// 日志脱敏（修订 1.32/L3）：命令可能携带凭据（`net use`、`ssh -i` 等），
/// info 级日志永远写入日志文件，必须截断而非全文落盘。保留前 32 字符 +
/// 总长度提示，足够排查"哪条命令被执行"又不泄露完整内容；调试级仍可
/// 通过 RUST_LOG=debug 输出全文（用户主动开启时才可见）。
pub fn redact_command(command: &str) -> String {
    const MAX: usize = 32;
    if command.chars().count() <= MAX {
        return command.to_string();
    }
    let head: String = command.chars().take(MAX).collect();
    format!("{}... ({} chars)", head, command.chars().count())
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

/// 校验 Fn 绑定前缀是否安全可用。
///
/// 空串匹配一切事件、单字节（长度 1）前缀在归一化后大多以 `0` 开头会匹配
/// 几乎全部同类型事件（如 `"0"`），都属于危险配置，一律拒绝（修订 1.32，
/// M3 回归）。合法前缀必须能构成**至少一个完整字节**（偶数长度）。
pub fn valid_prefix(prefix: &str) -> bool {
    let p = normalize_hex(prefix);
    p.len() >= 2 && p.len().is_multiple_of(2)
}

/// 校验 Fn 绑定的 WMI 事件类名是否为合法 WQL 标识符。
///
/// 类名会被原样拼进 `SELECT * FROM {}`（WQL），只允许
/// `[A-Za-z_][A-Za-z0-9_]*`——否则手改配置注入 WHERE 子句可改变订阅
/// 语义、甚至让监听线程在管理员权限下订阅并记录任意事件流（修订 1.32，
/// M2 安全加固）。绑定动作由 serde 枚举保证合法。
pub fn valid_class(class: &str) -> bool {
    let mut chars = class.trim().chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
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

    /// 前缀校验（修订 1.32/M3）：空/单字节/奇数长度都是危险或无效配置；
    /// 至少一个完整字节才合法。类名校验（M2）：只允许合法 WQL 标识符。
    #[test]
    fn test_valid_prefix_and_class() {
        assert!(valid_prefix("012801"));
        assert!(valid_prefix("01-28-01"));
        assert!(valid_prefix("AB"));
        assert!(!valid_prefix(""));
        assert!(!valid_prefix("0"));
        assert!(!valid_prefix("012"));
        assert!(!valid_prefix("XYZ"));

        assert!(valid_class("HID_EVENT20"));
        assert!(valid_class("_MyClass2"));
        assert!(!valid_class(""));
        assert!(!valid_class("HID_EVENT20 WHERE Foo=1"));
        assert!(!valid_class("HID-EVENT20"));
        assert!(!valid_class("1CLASS"));
        assert!(!valid_class("HID EVENT"));
    }

    /// 捕获事件转发限流（修订 1.32/L2、1.33 按键维度）：同一键身份首次
    /// 必然放行、窗口内重复（自动重复/按下释放对）丢弃；**不同按键**即使
    /// 在窗口内也各自放行，捕获工具不因快速连按漏键。窗口判定是纯时间
    /// 比较，不 sleep。
    #[test]
    fn test_capture_event_gate_rate_limits() {
        assert!(
            capture_event_gate("HID_EVENT20", "012801"),
            "first event must pass"
        );
        // 同一键的按下/释放对（身份同为 0128）：窗口内释放被去重。
        assert!(
            !capture_event_gate("HID_EVENT20", "012800"),
            "release of same key within window must be dropped"
        );
        // 同一键的自动重复事件：窗口内丢弃。
        assert!(
            !capture_event_gate("HID_EVENT20", "012801"),
            "auto-repeat of same key within window must be dropped"
        );
        // **不同按键**：即使同窗口内也必须放行（回归修正：历史全局窗口
        // 会把 150ms 内按下的第二个键也吞掉，捕获工具漏键）。
        assert!(
            capture_event_gate("HID_EVENT20", "010701"),
            "a different key within the window must still pass"
        );
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
            command: None,
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
            command: None,
        }]));
        let (tx, rx) = mpsc::channel();
        assert!(dispatch_bindings("HID_EVENT20", "012301", &bindings, &tx));
        match rx.try_recv() {
            Ok(UiCommand::ToggleBatteryCare) => {}
            other => panic!("Expected ToggleBatteryCare, got {:?}", other),
        }
    }

    /// RunCommand 动作：命中后不派发 UiCommand（命令走独立进程），
    /// 空命令被跳过且不崩溃。非空命令不在此处验证（会真实启动进程），
    /// 由真实功能键事件在实机上验证。
    #[test]
    fn test_dispatch_run_command_consumes_without_ui_command() {
        let bindings: SharedBindings = std::sync::Arc::new(RwLock::new(vec![FnKeyBinding {
            class: "HID_EVENT20".into(),
            prefix: "0128".into(),
            action: FnAction::RunCommand,
            command: None,
        }]));
        let (tx, rx) = mpsc::channel();
        assert!(dispatch_bindings("HID_EVENT20", "012801", &bindings, &tx));
        assert!(
            rx.try_recv().is_err(),
            "RunCommand must not send a UiCommand"
        );

        // 空/空白命令：跳过且不崩溃（不启动进程）。
        let empty: SharedBindings = std::sync::Arc::new(RwLock::new(vec![FnKeyBinding {
            class: "HID_EVENT20".into(),
            prefix: "0107".into(),
            action: FnAction::RunCommand,
            command: Some("   ".into()),
        }]));
        let (tx2, _rx2) = mpsc::channel();
        assert!(dispatch_bindings("HID_EVENT20", "010701", &empty, &tx2));
    }

    /// 防抖（L2 回归，修订 1.30）：相同命令在防抖窗口内的重复派发必须被
    /// 丢弃，不同命令可并发；窗口过后同一命令再次放行。
    #[test]
    fn test_debounce_duplicate_same_command() {
        // 测试会污染进程级防抖表：清空后测，测完清空。
        if let Ok(mut m) = last_run_commands().lock() {
            m.clear();
        }
        assert!(!debounce_duplicate("echo a"), "first run must pass");
        assert!(
            debounce_duplicate("echo a"),
            "same command within window must be debounced"
        );
        assert!(
            !debounce_duplicate("echo b"),
            "different command must not be debounced"
        );
        // 不同前缀不互相干扰（等长/子串前缀）。
        assert!(
            !debounce_duplicate("echo ab"),
            "prefix-distinct command must pass"
        );

        // 清理防抖表，避免影响其它用例与真机运行。
        if let Ok(mut m) = last_run_commands().lock() {
            m.clear();
        }
    }

    /// FnKeyBinding 序列化向后兼容：旧配置（无 command 字段）反序列化为
    /// command=None；新配置含 command 时往返一致。
    #[test]
    fn test_binding_command_serde_backward_compat() {
        let old = r#"class = "HID_EVENT20"
prefix = "012801"
action = "CyclePerfMode""#;
        let b: FnKeyBinding = toml::from_str(old).expect("legacy binding must parse");
        assert_eq!(b.action, FnAction::CyclePerfMode);
        assert_eq!(b.command, None);

        let new = FnKeyBinding {
            class: "HID_EVENT20".into(),
            prefix: "012801".into(),
            action: FnAction::RunCommand,
            command: Some(r#"start "" "C:\path with space\tool.exe""#.into()),
        };
        let s = toml::to_string(&new).expect("serialize");
        let parsed: FnKeyBinding = toml::from_str(&s).expect("deserialize");
        assert_eq!(parsed, new);
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
                command: None,
            },
            FnKeyBinding {
                class: "HID_EVENT21".into(),
                prefix: "FF".into(),
                action: FnAction::ReapplyConfig,
                command: None,
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
            command: None,
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

    /// 真机验证（手动运行，非 CI）：`run_external_command` 实际启动一个
    /// 进程并把输出写到临时文件，验证 `cmd.exe /C` + `CREATE_NO_WINDOW`
    /// 的完整 spawn 路径可用。运行：`cargo test -- --ignored
    /// run_command_spawns_real_process`。
    #[test]
    #[ignore = "spawns a real child process (manual hardware verification)"]
    fn run_command_spawns_real_process() {
        let marker = std::env::temp_dir().join(format!("xmpl-fn-cmd-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let cmd = format!("echo FNK_OK> {}", marker.to_string_lossy());
        run_external_command(Some(&cmd));
        // 子进程是分离的：轮询等待其创建文件（上限 5s）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if marker.exists() {
                let content = std::fs::read_to_string(&marker).unwrap_or_default();
                assert!(
                    content.trim().contains("FNK_OK"),
                    "unexpected marker content: {:?}",
                    content
                );
                let _ = std::fs::remove_file(&marker);
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("external command did not produce marker file within 5s");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}
