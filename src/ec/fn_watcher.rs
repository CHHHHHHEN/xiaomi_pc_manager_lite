//! Fn 功能键 **WMI 事件监听**（适配器）：订阅 OEM ACPI 事件类并把匹配的
//! 动作经 `CommandSink` 回传 GUI。
//!
//! 历史实现把模型与监听混在 `ec::fnkey`，且 `spawn` 直接持有 `egui::Context`
//! 用于唤醒事件循环——领域/驱动层反向依赖 GUI 框架。重构后：
//! - 领域模型（绑定表、hex 匹配、去重）收敛在 `app::fnkey`（纯逻辑）；
//! - 本模块只做 WMI 订阅与派发：订阅绑定表中出现的事件类，事件报告与
//!   绑定表做**前缀匹配**，命中后经 `CommandSink` 派发对应 `UiCommand`；
//! - 唤醒事件循环由 `CommandSink::wake` 承担（GUI 层实现持有 `egui::Context`），
//!   本模块不依赖任何 GUI 类型。
//!
//! 事件类参考（Meow-Box / 本机 2025 RedmiBook Pro 14 实证）：HID_EVENT20
//! 承载 Fn+K 等按键报告；其余类（HID_EVENT21-23、WMIEvent）在不同机型/固件
//! 上承载不同的功能键事件。本实现只订阅绑定表中出现的事件类（绑定为空且
//! 未捕获时监听闲置，不浪费连接）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLockReadGuard};

use windows::core::BSTR;
use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayUnaccessData};
use windows::Win32::System::Variant::{VARENUM, VT_ARRAY, VT_UI1};
use windows::Win32::System::Wmi::*;

use crate::app::command::UiCommand;
use crate::app::fnkey::{
    capture_event_gate, normalize_hex, release_state_after_prefix, FnAction, FnKeyBinding,
    SharedBindings,
};
use crate::app::sink::{CommandSink, CommandSinkExt};
use crate::util::err_fmt;

/// `RunCommand` 防抖窗口：同一命令在此窗口内不重复启动（毫秒）。
const RUN_COMMAND_DEBOUNCE_MS: u64 = 1000;

struct SafeEnumerator(IEnumWbemClassObject);
// SAFETY: SafeEnumerator is only used on the dedicated Fn watcher thread.
// COM is initialized in MTA on that thread, and the enumerator is never
// accessed from any other thread.
unsafe impl Send for SafeEnumerator {}

/// 启动 Fn 功能键监听线程。
///
/// - `sink`：命令端口（发送命令并唤醒 GUI 事件循环）；
/// - `bindings`：共享绑定表（GUI 保存配置时同步更新，即时生效）；
/// - `capture`：捕获开关。开启后，收到的**每条**事件都以
///   `UiCommand::FnEventSeen { class, hex }` 发送给 GUI 展示，方便用户
///   观察真实键码、配置新绑定。
pub fn spawn(sink: Arc<dyn CommandSink>, bindings: SharedBindings, capture: Arc<AtomicBool>) {
    // 与托盘/电池健康/自启动/WMI 各后台线程共用 util::spawn_guarded 兜底：
    // Builder 防 spawn 失败 panic 传播到 GUI update 线程杀死应用；
    // catch_unwind 兜底——release 已移除 panic=abort，本线程 panic 只会静默
    // 终止监听而应用仍存活，用户毫无感知。run_watcher 内部已有错误重试，
    // 这里只兜 panic 级故障。
    if let Err(e) = crate::util::spawn_guarded("fn-watcher", move || {
        // Fn 监听线程生命周期日志：该线程本应无限运行（内部是无限重试
        // 循环），正常情况下只有进程退出才结束。
        log::info!("Fn watcher thread started");
        if let Err(e) = run_watcher(&*sink, &bindings, &capture) {
            log::error!("Fn watcher: {}", e);
        }
        log::info!("Fn watcher thread exited");
    }) {
        log::error!("failed to spawn Fn watcher thread: {}", e);
    }
}

/// 从绑定表提取需要订阅的事件类集合（去重、稳定排序）。
fn binding_classes(bindings: &[FnKeyBinding]) -> Vec<String> {
    let mut v: Vec<String> = bindings.iter().map(|b| b.class.clone()).collect();
    v.sort();
    v.dedup();
    v
}

/// 捕获模式下需要订阅的事件类集合：绑定表中的类 ∪ 全部已知类的并集
/// （`KNOWN_FN_KEYS` 的 class 去重、稳定排序）。
///
/// 捕获的目的正是"发现未绑定的新键"（F-FNK-12）：若只订阅绑定表中的类，
/// 用户删除全部绑定后捕获将收不到任何事件（实测修正，修订 1.22）。
fn capture_classes(bindings: &[FnKeyBinding]) -> Vec<String> {
    let mut v = binding_classes(bindings);
    for k in crate::app::fnkey::KNOWN_FN_KEYS {
        // 字符串比较用 &str 而非逐键分配 String（捕获热路径上零分配）。
        if !v.iter().any(|c| c == k.class) {
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
/// 这里完成。连接阶段的任何失败以及运行期连接失效都会返回 Err 由外层
/// run_watcher 延时重试。
fn run_watcher_once(
    sink: &dyn CommandSink,
    bindings: &SharedBindings,
    capture: &AtomicBool,
) -> Result<(), WatcherError> {
    // COM 公寓初始化/退出的配对统一交给 win::com::ComScope 的 RAII 作用域
    //（Drop 时自动 CoUninitialize）。
    crate::win::ComScope::init().map_err(|e| WatcherError::Reconnect(err_fmt("COM init", e)))?;
    run_watcher_loop(sink, bindings, capture)
}

fn run_watcher_loop(
    sink: &dyn CommandSink,
    bindings: &SharedBindings,
    capture: &AtomicBool,
) -> Result<(), WatcherError> {
    let services = crate::win::connect_root_wmi().map_err(WatcherError::Reconnect)?;

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
            // 单槽 Next 统一收敛在 win::com::next_instance。
            match unsafe { crate::win::next_instance(enumerator, 100) } {
                Ok(Some(obj)) => process_event(&obj, class_name, bindings, capture, sink),
                Ok(None) => continue,
                Err(e) => {
                    log::warn!(
                        "Fn: IEnumWbemClassObject::Next failed (hr=0x{:08X}); resubscribing in 1s",
                        e.code().0
                    );
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    resubscribe = true;
                    break;
                }
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
    sink: &dyn CommandSink,
    bindings: &SharedBindings,
    capture: &AtomicBool,
) -> Result<(), String> {
    let mut stale_cycles: u32 = 0;
    loop {
        match run_watcher_once(sink, bindings, capture) {
            Err(WatcherError::NoEventClasses) => {
                stale_cycles += 1;
                // 退避节奏：前 3 次 5s、随后 30s、持续无类后降为 60s 长探测
                //（硬件上没有这些 OEM 事件类时类永远不会出现，长探测保留
                // "驱动后来安装/类后来出现"的恢复通道）。
                let delay = crate::app::fnkey::no_event_classes_backoff_secs(stale_cycles);
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
fn lock_or_recover_bindings(bindings: &SharedBindings) -> RwLockReadGuard<'_, Vec<FnKeyBinding>> {
    crate::util::lock_read_or_recover(bindings, "fn bindings")
}

fn process_event(
    obj: &IWbemClassObject,
    class_name: &str,
    bindings: &SharedBindings,
    capture: &AtomicBool,
    sink: &dyn CommandSink,
) {
    let Some(report_hex) =
        get_detail_hex(obj).or_else(|| crate::win::get_string_prop(obj, "ReportHex"))
    else {
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
        // 转发限流：按住键触发固件自动重复时同一键身份（class + 去状态
        // 字节的 hex）的事件是连续流。窗口内同一身份只转发最新一条；
        // 不同按键不受影响。
        if capture_event_gate(class_name, &normalized) {
            sink.dispatch(UiCommand::FnEventSeen {
                class: class_name.to_string(),
                hex: normalized.clone(),
            });
        }
    }

    if dispatch_bindings(class_name, &normalized, bindings, sink) {
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
    sink: &dyn CommandSink,
) -> bool {
    // 先复制出命中绑定的动作数据再释放读锁：避免在持锁期间做跨线程
    // spawn（失败即 panic，会毒化共享锁并永久杀死监听线程）。
    let matched: Option<(String, FnAction, Option<String>)> = lock_or_recover_bindings(bindings)
        .iter()
        .find(|b| {
            b.class == class_name && {
                let prefix = normalize_hex(&b.prefix);
                !prefix.is_empty()
                    && normalized.starts_with(&prefix)
                    // F-FNK-06 释放事件守卫：固件对一次物理按键发送 按下(`...01`)
                    // 与 释放(`...00`) 两条事件。短于完整事件的部分前缀（如 2
                    // 字节 `0125`）会**同时命中按下与释放**——前缀之后紧跟 `00`
                    // 释放字节且未覆盖完整事件时跳过（显式绑定含状态字节的
                    // 完整事件仍放行）。
                    && !release_state_after_prefix(&prefix, normalized)
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
        sink.dispatch(cmd);
    } else {
        log::debug!("Fn: binding {} has no action; consumed", action.name());
    }
    true
}

/// 以**独立进程**执行自定义命令（`RunCommand` 动作），不阻塞 WMI 事件
/// 监听循环。
///
/// 实现细节：
/// - 命令未配置（None/空白）时仅告警，不产生任何动作；
/// - `cmd.exe /C <command>` 承载（Windows 惯例的进程启动器，天然支持带引号
///   路径 / 参数 / 批处理 / `start` 内建命令）；
/// - `CREATE_NO_WINDOW` 隐藏控制台窗口（本应用是 GUI 程序，直接启动 cmd.exe
///   会闪现黑框）；
/// - 后台线程执行 + 不等待子进程（detached 语义）；
/// - **同命令防抖**：固件可能对一次物理按键重复上报，对相同命令在
///   `RUN_COMMAND_DEBOUNCE_MS` 时间窗内的重复派发直接丢弃。
fn run_external_command(command: Option<&str>) {
    let Some(command) = command.map(str::trim).filter(|c| !c.is_empty()) else {
        log::warn!("Fn: RunCommand binding with empty command; skipped");
        return;
    };
    // info 级脱敏（RUST_LOG=info 是默认，日志文件可能被转发/提交）；完整
    // 命令仅在用户显式开启 debug 时可见。
    log::info!(
        "Fn: running external command: {}",
        crate::app::fnkey::redact_command(command)
    );
    log::debug!("Fn: running external command (full): {}", command);
    // 转换为拥有所有权的 String 再移入线程（闭包参数借用自调用方）。
    let command = command.to_owned();

    // 防抖：相同命令在 RUN_COMMAND_DEBOUNCE 内重复触发时丢弃。
    if debounce_duplicate(&command) {
        return;
    }

    // 线程名带命令前缀便于排查；Builder 而非 thread::spawn：spawn 失败时
    // 记录告警而非 panic 传播（监听线程在该路径无 catch_unwind）。
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

/// 进程级"最后执行的自定义命令"防抖表（见 `debounce_duplicate`）。
fn last_run_commands() -> &'static Mutex<HashMap<String, std::time::Instant>> {
    static LAST_RUN_COMMANDS: std::sync::OnceLock<Mutex<HashMap<String, std::time::Instant>>> =
        std::sync::OnceLock::new();
    LAST_RUN_COMMANDS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 判断命令是否处于防抖窗口（相同命令在 `RUN_COMMAND_DEBOUNCE_MS` 内的
/// 重复派发）。首次调用返回 false 并记录时间；窗口内重复返回 true。
/// 防抖锁毒化（罕见）时放行（不阻断命令执行）。
fn debounce_duplicate(command: &str) -> bool {
    let now = std::time::Instant::now();
    let duplicate = Mutex::lock(last_run_commands())
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
    let val = crate::win::get_property(obj, "EventDetail")?;
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
        // 边界查询失败（真实 COM 错误）时显式回退：先解除访问再返回 None。
        let len = match unsafe { crate::win::safe_array_len(sa) } {
            Ok(l) => l,
            Err(e) => {
                unsafe { SafeArrayUnaccessData(sa).ok() };
                log::warn!("Fn: {}", e);
                return None;
            }
        };
        let hex_str = bytes_to_hex(data as *const u8, len);
        unsafe { SafeArrayUnaccessData(sa).ok() };
        hex_str
    } else {
        unsafe { crate::win::bstr_from_variant(&val) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::RwLock;

    /// 测试用命令端口：把发出的命令记录到 Vec 供断言。
    #[derive(Clone)]
    struct RecordingSink(Arc<Mutex<Vec<UiCommand>>>);

    impl CommandSink for RecordingSink {
        fn send(&self, command: UiCommand) -> Result<(), std::sync::mpsc::SendError<UiCommand>> {
            self.0.lock().unwrap().push(command);
            Ok(())
        }
        fn wake(&self) {}
    }

    fn test_bindings() -> SharedBindings {
        std::sync::Arc::new(RwLock::new(crate::app::fnkey::default_bindings()))
    }

    fn recording() -> (RecordingSink, Arc<Mutex<Vec<UiCommand>>>) {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        (RecordingSink(recorded.clone()), recorded)
    }

    /// 绑定前缀匹配：按下命中、释放不命中（F-FNK-06）、类不匹配不命中。
    #[test]
    fn test_dispatch_binding_match_semantics() {
        let bindings = test_bindings();
        let (sink, _) = recording();

        // Fn+K 按下（012801）命中。
        assert!(dispatch_bindings(
            "HID_EVENT20",
            "012801FFFF",
            &bindings,
            &sink
        ));
        // 释放（012800）不命中按下前缀。
        assert!(!dispatch_bindings(
            "HID_EVENT20",
            "012800",
            &bindings,
            &sink
        ));
        // 类不匹配不命中。
        assert!(!dispatch_bindings(
            "HID_EVENT21",
            "012801",
            &bindings,
            &sink
        ));
        // 其它键（如 Fn+Esc 0107）不命中。
        assert!(!dispatch_bindings(
            "HID_EVENT20",
            "010701",
            &bindings,
            &sink
        ));
    }

    /// 命中绑定必须派发对应的 UiCommand。
    #[test]
    fn test_dispatch_sends_ui_command() {
        let bindings = test_bindings();
        let (sink, recorded) = recording();
        assert!(dispatch_bindings("HID_EVENT20", "012801", &bindings, &sink));
        let got = recorded.lock().unwrap();
        assert!(matches!(&got[..], [UiCommand::CyclePerfMode]));
    }

    /// 部分前缀 + 释放事件守卫（F-FNK-06 回归）：2 字节前缀 `0125` 命中
    /// 按下 `012501...` 但不命中释放 `012500...`——一次物理按键只派发一次。
    #[test]
    fn test_dispatch_partial_prefix_skips_release() {
        let bindings: SharedBindings = std::sync::Arc::new(RwLock::new(vec![FnKeyBinding {
            class: "HID_EVENT20".into(),
            prefix: "0125".into(),
            action: FnAction::ToggleBatteryCare,
            command: None,
        }]));
        let (sink, recorded) = recording();
        assert!(dispatch_bindings(
            "HID_EVENT20",
            "01250100",
            &bindings,
            &sink
        ));
        assert!(matches!(
            &recorded.lock().unwrap()[0],
            UiCommand::ToggleBatteryCare
        ));
        assert!(
            !dispatch_bindings("HID_EVENT20", "01250000", &bindings, &sink),
            "release event must not dispatch a partial prefix"
        );
        assert_eq!(recorded.lock().unwrap().len(), 1);
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
        let (sink, recorded) = recording();
        assert!(dispatch_bindings("HID_EVENT20", "010701", &bindings, &sink));
        assert!(recorded.lock().unwrap().is_empty());
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
        let (sink, recorded) = recording();
        assert!(dispatch_bindings("HID_EVENT20", "012301", &bindings, &sink));
        assert!(matches!(
            &recorded.lock().unwrap()[0],
            UiCommand::ToggleBatteryCare
        ));
    }

    /// RunCommand 动作：命中后不派发 UiCommand（命令走独立进程），空命令被
    /// 跳过且不崩溃。
    #[test]
    fn test_dispatch_run_command_consumes_without_ui_command() {
        let bindings: SharedBindings = std::sync::Arc::new(RwLock::new(vec![FnKeyBinding {
            class: "HID_EVENT20".into(),
            prefix: "0128".into(),
            action: FnAction::RunCommand,
            command: None,
        }]));
        let (sink, recorded) = recording();
        assert!(dispatch_bindings("HID_EVENT20", "012801", &bindings, &sink));
        assert!(
            recorded.lock().unwrap().is_empty(),
            "RunCommand must not send a UiCommand"
        );
    }

    /// 防抖：相同命令在防抖窗口内的重复派发必须被丢弃，不同命令可并发。
    #[test]
    fn test_debounce_duplicate_same_command() {
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
        assert!(
            !debounce_duplicate("echo ab"),
            "prefix-distinct command must pass"
        );
        if let Ok(mut c) = last_run_commands().lock() {
            c.clear();
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

    /// 捕获模式的订阅类 = 绑定类 ∪ 已知类（去重、排序）。
    #[test]
    fn test_capture_classes_include_known_when_bindings_empty() {
        let empty: Vec<FnKeyBinding> = Vec::new();
        let cap = capture_classes(&empty);
        assert!(
            cap.contains(&"HID_EVENT20".to_string()),
            "capture with empty bindings must still subscribe to known classes"
        );
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
        assert!(cap.contains(&"HID_EVENT21".to_string()));
        assert!(cap.contains(&"HID_EVENT20".to_string()));
    }

    /// 非捕获模式（capture 关闭）的订阅类只来自绑定表。
    #[test]
    fn test_non_capture_bindings_only() {
        let bindings = vec![FnKeyBinding::fn_k()];
        assert_eq!(binding_classes(&bindings), vec!["HID_EVENT20"]);
    }

    /// 空 SAFEARRAY（长度为 0、指针可能为空）不得构造 0 长度切片（UB）。
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

    /// 真机验证（手动运行，非 CI）：`run_external_command` 实际启动一个
    /// 进程并把输出写到临时文件。运行：`cargo test -- --ignored
    /// run_command_spawns_real_process`。
    #[test]
    #[ignore = "spawns a real child process (manual hardware verification)"]
    fn run_command_spawns_real_process() {
        let marker = std::env::temp_dir().join(format!("xmpl-fn-cmd-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let cmd = format!("echo FNK_OK> {}", marker.to_string_lossy());
        run_external_command(Some(&cmd));
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
