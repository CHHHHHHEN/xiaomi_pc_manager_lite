//! WMI EC 后端 — MICommonInterface.MiInterface 协议
//!
//! 模块结构：
//! - `protocol`：MiInterface 线协议（命令构造/常量/百分比映射/实例选择/
//!   熔断判定，纯逻辑，无 COM 依赖）；
//! - 本文件：状态与线程——`WmiWorker`（独占 COM 对象）、`WmiBackend`
//!   （命令通道代理）与 `EcBackend` 实现。
//!
//! # 线程模型（重要）
//!
//! 所有 COM 调用（连接、枚举、方法调用）都固定在**专用 worker 线程**上
//! 执行：`WmiBackend` 只是命令通道代理，每次 EcBackend 调用经 mpsc 发送
//! 命令并在 worker 线程执行后同步等待结果。
//!
//! 原因（本机 2025 RedmiBook Pro 14 实测）：IWbemServices 连接在创建线程
//! 之外被调用时，在本项目 exe 环境下 100% 触发 STATUS_ACCESS_VIOLATION
//! 崩溃（同一代码在 cargo test 进程不崩、exe 进程必崩——与加载器/DLL 环境
//! 相关，机制未明）；且该跨线程崩溃点（GetObject）与输入无关。worker 模式
//! 从架构上根除跨线程 COM。注意：调用方（GUI 线程）仍会在 `call()` 中
//! **同步阻塞**等待 worker 应答——GetResultObject 的最长阻塞
//! （GET_RESULT_TIMEOUT_MS）确实发生在 worker 线程上，但调用线程阻塞在
//! `recv()` 的时长与之相同（正常 5~16ms，故障时最坏约 3s）。因此 worker
//! 模式解决的是 COM 线程亲和与崩溃问题，**不解决** GUI 冻结。

use std::sync::mpsc;
use std::sync::Mutex;

use super::backend::EcBackend;
use crate::app::battery;
use crate::app::ec::EcError;

use windows::core::{BSTR, PCWSTR};
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Ole::SafeArrayCreateVector;
use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayDestroy, SafeArrayUnaccessData};
use windows::Win32::System::Variant::{VariantClear, VARENUM, VARIANT, VT_ARRAY, VT_UI1};
use windows::Win32::System::Wmi::*;

// MiInterface 线协议（命令构造/常量/熔断判定等纯逻辑）在 `protocol`
// 子模块，经 `use protocol::*` 对本模块可见（`pub(super)`）。
pub(crate) mod protocol;
use protocol::*;

/// GetResultObject 等待上限。健康固件上单次调用 5~16ms 即可返回。
/// 超时阻塞发生在 worker 线程，不影响调用线程。
pub(crate) const GET_RESULT_TIMEOUT_MS: i32 = 3000;

/// `WmiWorker::connect`（含 COM 初始化、ConnectServer、预探测）的握手
/// 等待上限。WMI 服务异常时 `CoCreateInstance`/`ConnectServer`/`ExecQuery`
/// 可能长时间无响应——若无超时，`WmiBackend::new()` 会无限期阻塞调用方
/// （main 的后端初始化线程），GUI 永远无法启动。超时后返回错误，由调用方
/// 走既有回退路径（FallbackPreference / NullBackend）。
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// `WmiWorker::connect` 的连接重试上限（F-BUG 回归）。
///
/// 应用随登录自启动（F-AUTO）时，WinMgmt 服务可能尚未就绪、或
/// `MICommonInterface` 提供程序还在注册加载——单次连接失败即返回错误，启动
/// 直接回退 WinRing0，表现为"WMI 总是不可用、手动切换却能用"。在握手预算内
/// 做有界重试，等服务就绪。fnkey 监听线程对同一瞬态已有无限重试
/// （`run_watcher` 的 Reconnect 循环），此处补齐后端初始化这一处。
const CONNECT_ATTEMPTS: u32 = 4;

/// 连接重试间的固定退避。总退避 3×2s=6s，加上各次连接耗时仍明显小于
/// `HANDSHAKE_TIMEOUT`（10s），不会突破上层的启动等待预算。
const CONNECT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// 单次调用的应答等待上限（T1 熔断，见 `WmiBackend::recv_reply`）。
/// worker 侧 WMI 调用自身有 GET_RESULT_TIMEOUT_MS=3000ms 上限，健康路径
/// 应答在 5~16ms 内到达；本值只需显著大于 3s 即可容纳 worker 正常超时，
/// 同时保证 WMI 服务彻底卡死时 GUI 不会被永久冻结。
/// `pub(crate)`：托盘退出的强制退出宽限期（QUIT_FALLBACK_MS）按此值 +
/// 单条命令最多 4 次调用的 worker 侧上限来编译期锁定（见 tray/worker.rs）。
pub(crate) const CALL_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// 等待 worker 线程的握手应答（worker 完成 connect 后发送）。
///
/// 带超时：WMI 服务卡死时 `recv` 不能无限阻塞（见 HANDSHAKE_TIMEOUT）。
/// 超时或通道断开时返回 Err（断开即 worker 已退出，继续等待无意义）。
/// 握手应答是 (seq, reply)：忽略 seq（握手只有一次，取任何应答），
/// 校验 reply 载荷。
fn await_handshake(
    res_rx: &mpsc::Receiver<(u64, WmiReply)>,
    timeout: std::time::Duration,
) -> Result<(), EcError> {
    match res_rx.recv_timeout(timeout) {
        Ok((_, WmiReply::Unit(Ok(())))) => Ok(()),
        Ok((_, WmiReply::Unit(Err(e)))) => Err(e),
        Ok(_) => Err(EcError::WmiConnect("WMI worker 握手响应异常".into())),
        Err(_) => Err(EcError::WmiConnect("WMI worker 握手超时".into())),
    }
}

/// 初始化当前线程的 COM 公寓（MTA）。仅在 worker 线程调用一次。
fn ensure_com() -> Result<(), EcError> {
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    match hr.0 {
        // S_OK：本线程首次初始化；S_FALSE：本线程已初始化过，无需任何处理。
        0 => {
            log::info!("COM initialized (MTA)");
            Ok(())
        }
        1 => Ok(()),
        _ if hr.0 == RPC_E_CHANGED_MODE.0 => {
            let err = EcError::WmiConnect(format!(
                "COM already initialized with a different mode on this thread (need MTA, hr=0x{:08X})",
                hr.0
            ));
            log::error!("{}", err);
            Err(err)
        }
        _ => {
            let err = EcError::WmiConnect(format!("COM init: {}", hr));
            log::error!("COM init failed: {}", err);
            Err(err)
        }
    }
}

// ---------------------------------------------------------------------------
// Worker：独占全部 COM 状态与调用
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum WmiCmd {
    GetBatteryState,
    SetBatteryCare(bool),
    SetChargeLimit(u8),
    GetPerfMode,
    SetPerfMode(u8),
    Quit,
}

impl WmiCmd {
    /// 命令的短名（&'static str，零分配）：日志分支（send 失败、慢调用告警）
    /// 用短名而非 `format!("{:?}")`——`cmd` 会被 `send` 移走，format 必须
    /// 在 send 前执行，健康路径（5~16ms）每次调用都会白分配一个 String。
    /// 短名是固定编译期字符串，捕获进变量后即可在 send 之后安全引用。
    fn name(&self) -> &'static str {
        match self {
            WmiCmd::GetBatteryState => "GetBatteryState",
            WmiCmd::SetBatteryCare(_) => "SetBatteryCare",
            WmiCmd::SetChargeLimit(_) => "SetChargeLimit",
            WmiCmd::GetPerfMode => "GetPerfMode",
            WmiCmd::SetPerfMode(_) => "SetPerfMode",
            WmiCmd::Quit => "Quit",
        }
    }
}

#[derive(Debug)]
enum WmiReply {
    Unit(Result<(), EcError>),
    BatteryState(Result<(bool, u8), EcError>),
    PerfMode(Result<u8, EcError>),
}

struct WmiWorker {
    services: IWbemServices,
    /// MiInterface 目标实例路径（如 `MICommonInterface.InstanceName=
    /// "ACPI\\PNP0C14\\MIFS_0"`）。首次 resolve_target 后缓存，worker 独占。
    target: Option<String>,
    /// 确定性致命错误熔断：首次失败后后续调用立即返回，worker 独占。
    fatal: Option<EcError>,
    /// 首次成功解析后缓存的 MiInterface 方法签名类与输入参数名。
    ///
    /// 方法签名（类对象 → in_sig）与参数名由**类 schema** 决定，进程生命周期
    /// 内不变化。历史实现在每次 `mi_interface_call` 都重新 `GetObject` +
    /// `GetMethod` + `GetNames` 遍历 schema——每次 EC 操作白白多做两次
    /// provider 往返与一次数组遍历。首次解析后缓存，此后每次调用直接
    /// `SpawnInstance`，只保留真正属于单次调用的开销。三个对象均在 worker
    /// 线程创建并只在该线程使用（COM 线程亲和约定，与 `services`/`target`
    /// 相同），引用随 worker 生命周期释放。
    in_sig: Option<IWbemClassObject>,
    /// `in_sig` 对应的实际输入参数名（`param_name_from_schema` 发现或回退
    /// `"InData"`）。独立缓存避免每次调用重复遍历 schema。
    in_param_name: Option<String>,
    /// 输出参数名（`"OutData"` 的回退名）：与输入参数名一样由**方法签名**
    /// 决定、进程生命周期内不变。历史实现每次调用都重新
    /// `param_name_from_schema(&out_params, "OutData")` 遍历一遍 schema
    /// （GetNames + SafeArray 访问），与 `in_param_name` 的缓存设计自相矛盾
    /// ——首次调用后缓存，此后每次调用直接复用。
    out_param_name: Option<String>,
}

impl WmiWorker {
    fn connect() -> Result<Self, EcError> {
        // 注意：COM 初始化已由 worker 线程闭包统一执行（ensure_com +
        // CoUninitialize 配对，见 WmiBackend::new 的 spawn 闭包），此处
        // **不再重复调用** ensure_com——历史实现在 connect 内初始化导致
        // COM 生命周期散落两处、且无对应 CoUninitialize（修订 1.46 审计）。
        // 连接 + 目标实例解析是可能因"服务/提供程序未就绪"而瞬态失败的整体
        // （见 CONNECT_ATTEMPTS 注释）。确定性失败 `WmiInterfaceNotFound`
        // 也纳入重试：提供程序尚未注册时 ExecQuery 同样返回空实例，与"本机
        // 没有该接口"无法从错误本身区分；重试至预算的代价仅限启动期间，且
        // 上限受 HANDSHAKE_TIMEOUT 约束。
        let mut last_err = EcError::WmiConnect("WMI 连接未尝试".into());
        for attempt in 1..=CONNECT_ATTEMPTS {
            match Self::connect_once() {
                Ok(worker) => return Ok(worker),
                Err(e) => {
                    log::warn!(
                        "WMI: connect attempt {}/{} failed ({})",
                        attempt,
                        CONNECT_ATTEMPTS,
                        e
                    );
                    last_err = e;
                    if attempt < CONNECT_ATTEMPTS {
                        std::thread::sleep(CONNECT_RETRY_DELAY);
                    }
                }
            }
        }
        Err(last_err)
    }

    /// 单次连接尝试：连接 root\wmi 并解析 MiInterface 调用目标实例。
    fn connect_once() -> Result<Self, EcError> {
        // 连接样板（CoCreateInstance → ConnectServer → CoSetProxyBlanket）
        // 与 fnkey.rs 共用，见 `win::com::connect_root_wmi`。
        let services = crate::win::connect_root_wmi().map_err(EcError::WmiConnect)?;
        let mut worker = Self {
            services,
            target: None,
            fatal: None,
            in_sig: None,
            in_param_name: None,
            out_param_name: None,
        };
        // 预探测 MICommonInterface 目标实例：本机没有该接口（如非小米机型）
        // 时在**创建阶段**就返回 WmiInterfaceNotFound，使 create_backend(Wmi)
        // 失败并触发自动回退（WinRing0 或错误提示），而不是创建一个"连接成功
        // 但每次调用都报错"的后端让 GUI 一直显示读取失败。
        worker.resolve_target()?;
        Ok(worker)
    }

    fn run(mut self, rx: mpsc::Receiver<(u64, WmiCmd)>, tx: mpsc::Sender<(u64, WmiReply)>) {
        while let Ok((seq, cmd)) = rx.recv() {
            let reply = match cmd {
                WmiCmd::Quit => break,
                WmiCmd::GetBatteryState => WmiReply::BatteryState(self.get_battery_state_impl()),
                WmiCmd::SetBatteryCare(en) => WmiReply::Unit(self.set_battery_care_impl(en)),
                WmiCmd::SetChargeLimit(pct) => WmiReply::Unit(self.set_charge_limit_impl(pct)),
                WmiCmd::GetPerfMode => WmiReply::PerfMode(self.get_perf_impl()),
                WmiCmd::SetPerfMode(mode) => WmiReply::Unit(self.set_perf_impl(mode)),
            };
            // 应答必须回显请求序号：调用方以 seq 配对，超时丢弃的过期应答
            // 不会污染后续调用（见 WmiBackend::call 的时序注释）。
            if tx.send((seq, reply)).is_err() {
                break;
            }
        }
        log::info!("WMI: worker thread exiting");
    }

    fn maybe_latch(&mut self, hr: Option<u32>, err: EcError) -> EcError {
        latch_into(&mut self.fatal, hr, err)
    }

    /// 枚举 `MICommonInterface` 实例并确定 MiInterface 的调用目标。
    ///
    /// **必须在实例上调用方法**（2025 RedmiBook Pro 14 实测）：对类路径
    /// （`"MICommonInterface"`）调用 ExecMethod 会被 WinMgmt 以
    /// WBEM_E_INVALID_METHOD_PARAMETERS (0x8004102F) 拒绝，与输入缓冲区的
    /// 长度/内容无关（1~64 字节、读/写命令、空参数全部复现）；对实例路径
    /// （如 `MICommonInterface.InstanceName="ACPI\\PNP0C14\\MIFS_0"`）调用
    /// 一切正常（5~16ms，ReturnCode=0，OutData 30 字节）。Meow-Box
    /// （Xiaomi Book Pro 14 2026 机型）采用相同方式：枚举实例 → 优先选择
    /// Active 且 InstanceName 含 "MIFS" 的实例（否则取第一个）→ 对实例
    /// 路径调用。
    fn resolve_target(&mut self) -> Result<String, EcError> {
        if let Some(t) = &self.target {
            return Ok(t.clone());
        }
        let enumerator = crate::win::exec_query(&self.services, "SELECT * FROM MICommonInterface")
            .map_err(|e| EcError::WmiConnect(format!("ExecQuery instances: {}", e)))?;

        // (instance_name, active, is_mifs)：收集全部实例后按 Meow-Box 的
        // 选择策略挑选：active 且含 MIFS 优先，否则取第一个。
        let mut instances: Vec<(String, bool, bool)> = Vec::new();
        loop {
            // 统一收敛在 `win::com::next_instance`（单槽 Next 样板）；枚举耗尽
            // 或 Next 失败（与历史一致）都结束收集。
            let Ok(Some(obj)) = (unsafe { crate::win::next_instance(&enumerator, 500) }) else {
                break;
            };
            // InstanceName 缺失的实例必须**跳过**而非默认成空串：历史实现
            // `unwrap_or_default()` 会留下 `InstanceName=""` 的伪实例，若它
            // 恰好被选中（唯一实例 / first 回退），后续每个 EC 调用都因目标
            // 路径非法返回难以理解的 WMI 错误。跳过 + 显式告警更清晰。
            let Some(name) = crate::win::get_string_prop(&obj, "InstanceName") else {
                log::warn!("WMI: MICommonInterface instance missing InstanceName; skipping");
                continue;
            };
            // Active 属性缺失/类型不符（如固件以 VT_I4 承载）时保守按
            // inactive 处理并告警：该实例不会命中"active && mifs"首选，
            // 但仍保留在 first 候选池——与 InstanceName 缺失直接跳过不同，
            // 这里若跳过会把唯一可用实例也丢弃（Active 不是路径构造成分）。
            // 显式告警让"选择逻辑异常"在日志可见，而不是静默吞掉。
            let active = match crate::win::get_bool_prop(&obj, "Active") {
                Some(a) => a,
                None => {
                    log::warn!(
                        "WMI: instance '{}' Active not readable; treating as inactive",
                        name
                    );
                    false
                }
            };
            let is_mifs = name.to_ascii_uppercase().contains("MIFS");
            log::info!(
                "WMI: MICommonInterface instance '{}' (active={}, mifs={})",
                name,
                active,
                is_mifs
            );
            instances.push((name, active, is_mifs));
        }
        if instances.is_empty() {
            return Err(EcError::WmiInterfaceNotFound);
        }
        let Some(name) = select_target_instance(&instances) else {
            // 实例列表非空（上方已检查），防御性分支不可达——但避免 expect
            // 把"未来重构引入的空列表"变成 panic（修订 1.47 清理）。
            return Err(EcError::WmiInterfaceNotFound);
        };
        let path = format!(
            "MICommonInterface.InstanceName=\"{}\"",
            escape_instance_name(name)
        );
        self.target = Some(path.clone());
        log::info!("WMI: MiInterface target instance -> '{}'", path);
        Ok(path)
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
    ///
    /// BSTR 数组通过 SafeArrayAccessData 直接读取元素（**不拷贝**），
    /// 数组由 SafeArrayDestroy 统一释放（唯一释放方）。历史实现用
    /// SafeArrayGetElement 获取"深拷贝"BSTR 并 from_raw 释放——若该 API
    /// 返回数组内部指针而非拷贝，from_raw 会释放数组内部的 BSTR，
    /// 随后 SafeArrayDestroy 再次释放 = 双重释放，堆损坏概率性爆发。
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
        // 边界查询失败是真实 COM 错误，不能静默当成"空数组"（历史实现
        // `unwrap_or((0,-1))` 会伪造 len=0 掩盖错误）——释放数组并回退。
        let len = match unsafe { crate::win::safe_array_len(sa) } {
            Ok(l) => l,
            Err(e) => {
                let _ = SafeArrayDestroy(sa);
                log::warn!("WMI: param_name_from_schema: {}", e);
                return fallback.to_string();
            }
        };
        if len == 0 {
            let _ = SafeArrayDestroy(sa);
            return fallback.to_string();
        }
        // 真实方法签名类的属性数远小于 64（本机 MiInterface 仅 1 个入参 +
        // ReturnValue + 系统属性）；上限兜底"宿主类提供者返回荒谬元素数"的
        // 场景，避免 `from_raw_parts` 按伪造的 len 构造越界切片（同一数组
        // 的 BSTR 元素读取上限，与 mi_interface_call 的 len.min(32) 同理）。
        let len = len.min(MAX_SCHEMA_PROPERTY_NAMES);
        let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
        if SafeArrayAccessData(sa, &mut data).is_err() {
            let _ = SafeArrayDestroy(sa);
            return fallback.to_string();
        }
        let elems = std::slice::from_raw_parts(data as *const windows::core::BSTR, len);
        let mut chosen: Option<String> = None;
        for bstr in elems {
            // BSTR 元素可为空指针；Deref 对 null 安全（空字符串）。
            let name = String::from_utf16_lossy(&bstr[..]);
            // 系统属性（__*）与返回值属性（ReturnValue/ReturnCode）都不是
            // 数据参数，必须整体跳过；若全是这两类则说明该 schema 没有用户
            // 参数，回退到约定名（InData/OutData）。绝不能把 ReturnValue
            // 当参数名返回——Get("ReturnValue") 拿到的是方法返回值，
            // 不是 32 字节数组，必然失败。
            if name.starts_with("__") || is_return_value_prop(&name) {
                continue;
            }
            chosen = Some(name);
            break;
        }
        SafeArrayUnaccessData(sa).ok();
        let _ = SafeArrayDestroy(sa);
        chosen.unwrap_or_else(|| fallback.to_string())
    }

    /// 惰性解析并缓存 MiInterface 方法签名（类对象 + in_sig + 输入参数名）。
    ///
    /// schema 由类定义决定、进程生命周期内不变，故只解析一次（见
    /// `WmiWorker::in_sig` 字段注释）。失败路径保持与历史实现一致：确定性
    /// HRESULT 写入熔断，接口缺失（None）按必然失败熔断。
    unsafe fn ensure_schema(&mut self) -> Result<(), EcError> {
        if self.in_sig.is_some() {
            return Ok(());
        }
        // 必须在**实例**上调用方法：对类路径调用 ExecMethod 被 WinMgmt
        // 拒绝（0x8004102F，详见 resolve_target）。但方法**签名**定义在
        // 类对象上——对实例对象 GetMethod 返回 WBEM_E_INVALID_OPERATION
        // (0x8004101E)。因此：GetMethod 用类对象，ExecMethod 用实例路径。
        let mut class: Option<IWbemClassObject> = None;
        self.services
            .GetObject(
                &BSTR::from("MICommonInterface"),
                WBEM_FLAG_RETURN_WBEM_COMPLETE,
                None::<&IWbemContext>,
                Some(&mut class as *mut Option<IWbemClassObject>),
                None,
            )
            .map_err(|e| {
                let hr = e.code().0 as u32;
                self.maybe_latch(
                    Some(hr),
                    EcError::WmiConnect(format!("获取类对象失败: {}", e)),
                )
            })?;
        let class = match class {
            Some(c) => c,
            None => return Err(self.maybe_latch(None, EcError::WmiInterfaceNotFound)),
        };

        let mut in_sig: Option<IWbemClassObject> = None;
        let mut out_sig: Option<IWbemClassObject> = None;
        let method_name = crate::util::WideString::new("MiInterface");
        class
            .GetMethod(method_name.as_pcwstr(), 0, &mut in_sig, &mut out_sig)
            .map_err(|e| {
                let hr = e.code().0 as u32;
                self.maybe_latch(
                    Some(hr),
                    EcError::WmiConnect(format!("获取方法签名失败: {}", e)),
                )
            })?;

        let in_sig = match in_sig {
            Some(s) => s,
            None => return Err(self.maybe_latch(None, EcError::WmiInterfaceNotFound)),
        };
        let in_param_name = Self::param_name_from_schema(&in_sig, "InData");
        log::debug!("WMI: MiInterface input parameter -> '{}'", in_param_name);
        self.in_sig = Some(in_sig);
        self.in_param_name = Some(in_param_name);
        Ok(())
    }

    /// Send a 32-byte command buffer via MiInterface and receive the response.
    ///
    /// Command buffer layout (per F-HAL-05):
    ///   fun1(2B) + fun2(2B) + fun3(2B) + fun4(4B) + zero-padding = 32 bytes
    ///
    /// Response buffer layout (per F-HAL-08):
    ///   Status(2B) + Function(2B) + Data0(2B) + Data1(4B) + Data2(4B) + Data3(4B)
    ///
    /// 注意：响应数组**不是 32 字节**——本机（2025 RedmiBook Pro 14）实测
    /// OutData 为 30 字节（MOF 声明 OutData MAX=30），有效字段仅前 18 字节。
    unsafe fn mi_interface_call(
        &mut self,
        buffer: &[u8; CMD_BUF_LEN],
    ) -> Result<[u8; CMD_BUF_LEN], EcError> {
        // 熔断检查：确定性失败后不再发起任何 WMI 调用，直接返回缓存错误。
        if let Some(err) = &self.fatal {
            log::warn!("WMI: MiInterface latched as failed ({}); failing fast", err);
            return Err(err.clone());
        }

        // 必须在**实例**上调用方法：对类路径调用 ExecMethod 被 WinMgmt
        // 拒绝（0x8004102F，详见 resolve_target）。方法**签名**与输入参数名
        // 由类 schema 决定、进程生命周期内不变——首次调用时经 ensure_schema
        // 解析并缓存，此后每次调用直接复用，避免重复的
        // GetObject + GetMethod + GetNames provider 往返。
        let target = self.resolve_target()?;
        unsafe { self.ensure_schema()? };
        // 这两个字段由 ensure_schema 在 Ok 返回时无条件写入（见其末尾），
        // 理论上到达此处必为 Some。但 worker 线程内的 expect 在不变式被未来
        // 重构破坏时会**静默终止本线程**（panic 被 spawn_guarded 捕获、后端
        // 熔断，无任何可操作的错误信息）——用 Err 显式失败，把"内部状态
        // 损坏"如实透传给调用方而不是无声 panic（修订 1.47 审计）。
        let in_sig = self
            .in_sig
            .as_ref()
            .ok_or_else(|| EcError::WmiConnect("WMI 方法签名未初始化（内部状态损坏）".into()))?;
        // ensure_schema 每次 Ok 返回都无条件写入 in_param_name（见其末尾），
        // 上面的 ? 已保证到达此处时它必为 Some——与 in_sig 同一不变式。
        // 历史实现的 `unwrap_or_else("InData")` 死兜底不可达，且会把未来
        // 重构引入的 bug 静默掩盖成错误参数名。
        let in_param_name = self
            .in_param_name
            .clone()
            .ok_or_else(|| EcError::WmiConnect("WMI 输入参数名未初始化（内部状态损坏）".into()))?;

        let in_params = in_sig
            .SpawnInstance(0)
            .map_err(|e| EcError::WmiConnect(format!("创建方法参数实例失败: {}", e)))?;
        // 注意：此处**尚不**包 ManuallyDrop。在 Put 成功之前的各失败分支
        // （SafeArray 创建/访问失败、Put 失败），对象从未交给提供程序，
        // 正常 Drop（Release）是安全且应当的（修订 1.47 审计：历史实现
        // 在这些分支上依赖自动 Drop 释放，无泄漏）。
        let sa = SafeArrayCreateVector(VT_UI1, 0, CMD_BUF_LEN as u32);
        if sa.is_null() {
            return Err(EcError::WmiConnect("WMI 命令数组创建失败".into()));
        }

        let mut data_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        if SafeArrayAccessData(sa, &mut data_ptr).is_err() {
            SafeArrayDestroy(sa).ok();
            return Err(EcError::WmiConnect("WMI 命令数组访问失败".into()));
        }
        std::ptr::copy_nonoverlapping(buffer.as_ptr(), data_ptr as *mut u8, CMD_BUF_LEN);
        if SafeArrayUnaccessData(sa).is_err() {
            SafeArrayDestroy(sa).ok();
            return Err(EcError::WmiConnect("SafeArrayUnaccessData failed".into()));
        }

        // 输入 VARIANT 必须**整体**包 ManuallyDrop：windows-rs 的 VARIANT
        // 实现了 Drop（自动 VariantClear），若不加 ManuallyDrop，函数结束时
        // 外层 Drop 仍会释放 sa——而提供程序对 sa 的引用存活到连接关闭，
        // 提前释放即堆损坏（本机实测：启动闪退 STATUS_ACCESS_VIOLATION，
        // 根因正是 v 的 Drop 释放了已被 Put 交给提供程序的数组）。释放
        // 责任全部改为显式（见下方各分支：Put 失败才 SafeArrayDestroy；
        // 其余路径永不释放，宁泄漏不崩溃）。
        let v = core::mem::ManuallyDrop::new(VARIANT {
            Anonymous: windows::Win32::System::Variant::VARIANT_0 {
                Anonymous: core::mem::ManuallyDrop::new(
                    windows::Win32::System::Variant::VARIANT_0_0 {
                        vt: VARENUM(VT_ARRAY.0 | VT_UI1.0),
                        wReserved1: 0,
                        wReserved2: 0,
                        wReserved3: 0,
                        Anonymous: windows::Win32::System::Variant::VARIANT_0_0_0 { parray: sa },
                    },
                ),
            },
        });

        let in_name = crate::util::WideString::new(&in_param_name);
        if let Err(e) = in_params.Put(in_name.as_pcwstr(), 0, &*v as *const VARIANT, 0) {
            // Put 失败：数组从未交给提供程序，此处是唯一释放点
            // （v 已 ManuallyDrop，不会二次释放；in_params 尚未包
            // ManuallyDrop，正常 Drop 即 Release，不会泄漏）。
            SafeArrayDestroy(sa).ok();
            return Err(EcError::WmiConnect(format!(
                "Put '{}': {}",
                in_param_name, e
            )));
        }
        // **移交点**：Put 成功后提供程序对输入数组持有引用、存活到连接
        // 关闭——输入参数对象自此转入"永不释放"语义（修订 1.47 重构）。
        // 历史实现靠 4 处 `std::mem::forget(in_params)` 分散维持同一不变式，
        // 漏写/新增返回路径即堆损坏；改为 ManuallyDrop 包裹后，"不 drop"
        // 成为默认行为，只有显式 `ManuallyDrop::drop` 才会释放。
        let in_params = core::mem::ManuallyDrop::new(in_params);
        // 关键：Put 之后**不能** SafeArrayDestroy(sa)，也不能让 v 的 Drop
        // 释放它。实测验证（含 HeapValidate 逐步检测）IWbemClassObject::Put
        // 对 SAFEARRAY 保留引用而非深拷贝，且提供程序对数组的内部引用
        // **存活到连接关闭**：任何时机释放（成功返回后、失败返回后、
        // 延迟到下一次调用、连接关闭时）都会触发 OLE 堆损坏，进程以
        // STATUS_HEAP_CORRUPTION / STATUS_ACCESS_VIOLATION 退出。
        // 全程不释放则零崩溃。因此输入数组**永不释放**：每次调用泄漏
        // 一个约 32 字节的数组，有界且无害；宁泄漏不崩溃。

        let mut call_result: Option<IWbemCallResult> = None;
        if let Err(e) = self.services.ExecMethod(
            &BSTR::from(&target),
            &BSTR::from("MiInterface"),
            WBEM_FLAG_RETURN_IMMEDIATELY,
            None::<&IWbemContext>,
            &*in_params,
            None,
            Some(&mut call_result as *mut Option<IWbemCallResult>),
        ) {
            // ExecMethod 同步返回错误意味着异步调用**从未启动**——但 Put
            // 已执行，提供程序可能已获得数组引用，因此同样**不释放**输入
            // 数组（与 GetResultObject 失败分支同策略，宁泄漏不崩溃；
            // in_params 为 ManuallyDrop，无需也不能在此 forget）。
            let hr = e.code().0 as u32;
            return Err(self.maybe_latch(Some(hr), EcError::WmiCallHResult(hr)));
        }

        let call_result = match call_result {
            Some(cr) => cr,
            None => {
                // 理论上 ExecMethod(RETURN_IMMEDIATELY) 成功必然返回
                // call result；防御性处理。此时异步调用**已启动**、输入
                // 数组已交给提供程序：不得释放（ManuallyDrop 保证），
                // 宁可泄漏也不崩溃。
                return Err(EcError::WmiCallFailed(0));
            }
        };

        log::debug!("WMI: GetResultObject waiting...");
        let out_params = match call_result.GetResultObject(GET_RESULT_TIMEOUT_MS) {
            Ok(p) => p,
            Err(e) => {
                let hr = e.code().0 as u32;
                log::error!("WMI: GetResultObject failed: hr=0x{:08X}", hr);
                // 调用失败时**绝不**释放输入数组。实测（本机 2025
                // RedmiBook Pro 14，含半同步调用对照实验）：提供程序在
                // 错误返回后仍会访问输入数组（其对数组的内部引用存活到
                // 连接关闭），任何时机释放——失败后立即释放、延迟到下一次
                // 调用、甚至等到连接关闭时释放——都会触发 OLE 堆损坏，
                // 进程以 STATUS_HEAP_CORRUPTION 退出（概率性乃至确定性
                // 复现）；全程不释放则零崩溃（该堆损坏经 PowerShell/C#
                // 调用同样复现，属提供程序缺陷，客户端无法安全释放）。
                // 代价：失败调用每次泄漏一个约 32 字节的数组——正确调用
                // 下失败罕见，泄漏有界且无害；宁泄漏不崩溃。
                // （sa 为裸指针无析构器，不调用 SafeArrayDestroy 即不
                // 释放；in_params 为 ManuallyDrop，不会自动 Release。）
                return Err(self.maybe_latch(Some(hr), EcError::WmiCallHResult(hr)));
            }
        };

        // 异步调用成功完成（GetResultObject 返回）后**同样不得**释放输入
        // 数组。实测（本机 2025 RedmiBook Pro 14，首次真机成功调用）：
        // 提供程序对输入数组的内部引用**存活到连接关闭**——perf read
        // 成功返回后按旧逻辑 drop(in_params)+SafeArrayDestroy(sa) 释放
        // 数组，下一次调用时进程以 STATUS_HEAP_CORRUPTION 崩溃。
        // 与下方失败路径采取相同策略：**永不释放输入数组**（in_params
        // 为 ManuallyDrop，永不 drop；不调用 SafeArrayDestroy）。
        // 代价：每次调用泄漏一个约 32 字节的数组，有界且无害；
        // 宁泄漏不崩溃。

        // 输出参数名由**方法签名**决定、进程生命周期内不变：首次调用解析后
        // 缓存（`out_param_name`），此后每次调用直接复用，避免每次调用都
        // 重复 GetNames + SafeArray 遍历一遍 schema（与 `in_param_name` 的
        // 缓存设计一致）。
        let out_param_name = match &self.out_param_name {
            Some(name) => name.clone(),
            None => {
                let name = Self::param_name_from_schema(&out_params, "OutData");
                log::debug!("WMI: MiInterface output parameter -> '{}'", name);
                self.out_param_name = Some(name.clone());
                name
            }
        };

        let out_name = crate::util::WideString::new(&out_param_name);
        let mut out_val = VARIANT::default();
        let mut out_type = 0i32;
        let mut out_flavor = 0i32;
        if let Err(e) = out_params.Get(
            out_name.as_pcwstr(),
            0,
            &mut out_val,
            Some(&mut out_type as *mut i32),
            Some(&mut out_flavor as *mut i32),
        ) {
            return Err(EcError::WmiConnect(format!(
                "Get '{}': {}",
                out_param_name, e
            )));
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

        // 响应结构需要 18 字节（Status 2 + Function 2 + Data0 2 + Data1 4
        // + Data2 4 + Data3 4）。本机实测 OutData 为 **30 字节**
        // （MOF OutData MAX=30）——历史实现对类路径调用且要求 ≥32 字节，
        // 把成功响应全部误判为失败。只要 ≥18 字节即可安全读取全部有效
        // 字段；超过 32 字节的部分忽略。
        // 边界查询失败是真实 COM 错误，显式报错而非伪造 (0,-1) 边界。
        let len = match unsafe { crate::win::safe_array_len(out_sa) } {
            Ok(l) => l,
            Err(e) => {
                VariantClear(&mut out_val).ok();
                return Err(EcError::WmiConnect(format!("WMI: {}", e)));
            }
        };
        if len < MIN_OUTPUT_LEN {
            log::error!("WMI: output array too short ({} bytes)", len);
            VariantClear(&mut out_val).ok();
            return Err(EcError::WmiCallFailed(0));
        }

        let mut out_data: *mut core::ffi::c_void = std::ptr::null_mut();
        if SafeArrayAccessData(out_sa, &mut out_data).is_err() {
            VariantClear(&mut out_val).ok();
            return Err(EcError::WmiConnect("读取 WMI 响应数组失败".into()));
        }

        let mut result = [0u8; CMD_BUF_LEN];
        let copy_len = len.min(CMD_BUF_LEN);
        std::ptr::copy_nonoverlapping(out_data as *const u8, result.as_mut_ptr(), copy_len);

        SafeArrayUnaccessData(out_sa).ok();
        // Release the output VARIANT (and the SafeArray it owns).
        // 注意：out_val 是默认构造的 VARIANT，其 Drop 会再次 VariantClear，
        // 但首次 VariantClear 后 vt 已置为 VT_EMPTY，二次调用为 no-op。
        VariantClear(&mut out_val).ok();

        // F-HAL-08: 响应前 2 字节为 Status。本机实测（2025 RedmiBook
        // Pro 14）：**0x8000 = 成功**（所有成功调用的恒常返回值，
        // 含读写操作；Meow-Box 同款响应），0x0000 = 失败（如写入无效
        // 充电上限 raw code 0xFF 时返回 0x0000 且状态未变）。历史实现
        // 把"非 0 即失败"当作判定，导致每次成功调用都被误判为失败。
        let status = u16::from_le_bytes([result[0], result[1]]);
        if status != WMI_STATUS_SUCCESS {
            log::error!("WMI: MiInterface returned status {:#x}", status);
            return Err(EcError::WmiCallFailed(status));
        }

        Ok(result)
    }

    /// Build a read command buffer.
    /// Layout: fun1=0xFA00, fun2=selector, fun3=sub-op, fun4=0
    /// Per F-HAL-06: 充电读 fun3=0x0002, 性能读 fun3=0x0000
    fn read_battery(&mut self) -> Result<[u8; CMD_BUF_LEN], EcError> {
        let buf = read_battery_cmd();
        unsafe { self.mi_interface_call(&buf) }
    }

    /// Build a write command buffer for battery.
    /// Layout: fun1=0xFB00, fun2=0x1000, fun3=0x0002, fun4=raw_code
    /// Per F-HAL-07: 充电写 fun3=0x0002, fun4=充电上限 raw code
    fn write_battery(&mut self, raw_code: u8) -> Result<(), EcError> {
        let buf = write_battery_cmd(raw_code);
        unsafe { self.mi_interface_call(&buf)? };
        Ok(())
    }

    fn read_perf(&mut self) -> Result<[u8; CMD_BUF_LEN], EcError> {
        let buf = read_perf_cmd();
        unsafe { self.mi_interface_call(&buf) }
    }

    /// Build a write command buffer for performance.
    /// Layout: fun1=0xFB00, fun2=0x0800, fun3=mode, fun4=0
    /// Per F-HAL-07: 性能写 fun3=模式 raw code, fun4=0
    fn write_perf(&mut self, mode: u8) -> Result<(), EcError> {
        let buf = write_perf_cmd(mode);
        unsafe { self.mi_interface_call(&buf)? };
        Ok(())
    }

    fn get_battery_state_impl(&mut self) -> Result<(bool, u8), EcError> {
        // B-WMI-1: 养护位与上限来自同一条读命令的同一响应字段（Data1），
        // 一次往返同时返回两者；默认实现会发起两次相同的 WMI 往返。
        // 未知 raw code 如实报错而不是静默当成 100%：历史实现
        // `wmi_rawcode_to_percent(raw).unwrap_or(100)` 把未定义代码伪装成
        // "成功读到 100%"，写后回读路径会据此把用户设置（如 60% 养护）静默
        // 持久化为关闭（与 winring0::get_charge_limit 的同类问题，见其注释）。
        // 刷新路径收到该错误会显示"读取电池状态失败"而非荒谬的 100%。
        let buf = self.read_battery()?;
        let raw = buf[6]; // Data1 = 充电上限 raw code
        let percent = match wmi_rawcode_to_percent(raw) {
            Some(p) => p,
            None => {
                return Err(EcError::InvalidData(format!(
                    "WMI 充电上限 raw code 0x{:02x} 未定义",
                    raw
                )))
            }
        };
        log::debug!(
            "WMI: battery state -> care {}, limit {}%",
            battery::care_enabled_from_limit(percent),
            percent
        );
        Ok((battery::care_enabled_from_limit(percent), percent))
    }

    fn set_battery_care_impl(&mut self, enabled: bool) -> Result<(), EcError> {
        // B-WMI-3: WMI 没有独立的电池养护位（养护 = 充电上限 < 100%，见
        // get_battery_state 的推导），因此这里是契约性 no-op——全部调用方
        // （GUI 切换、启动应用、电源重设）都已先显式 set_charge_limit 设置
        // 上限。曾在 !enabled 时重复 set_charge_limit(100)，与调用方刚写过
        // 的 100% 完全相同，每次关闭养护浪费一次完整 WMI 往返。
        log::debug!(
            "WMI: set battery care -> {} (no-op; derived from charge limit)",
            if enabled { "enabled" } else { "disabled" }
        );
        Ok(())
    }

    fn set_charge_limit_impl(&mut self, percent: u8) -> Result<(), EcError> {
        // 写入前统一校验（0 拒绝 / >100 钳制），见 battery::validate_charge_limit_write。
        let percent = battery::validate_charge_limit_write(percent)?;
        // 映射必然命中的不变量由测试锁定；万一未来编辑表格打破，如实报错
        // 而不是把输入静默当成 100%（历史实现 unwrap_or(0)）。
        let raw = wmi_rawcode_for_percent(percent).ok_or_else(|| {
            EcError::InvalidData(format!("无法将充电上限 {}% 映射到 WMI raw code", percent))
        })?;
        log::info!("WMI: set charge limit -> {}% (raw {:#x})", percent, raw);
        self.write_battery(raw)
    }

    fn get_perf_impl(&mut self) -> Result<u8, EcError> {
        let buf = self.read_perf()?;
        log::debug!("WMI: read perf mode -> {:#x}", buf[4]);
        Ok(buf[4])
    }

    fn set_perf_impl(&mut self, mode: u8) -> Result<(), EcError> {
        log::info!("WMI: set perf mode -> {:#x}", mode);
        self.write_perf(mode)
    }
}

// ---------------------------------------------------------------------------
// 对外代理：命令通道 + 同步等待
// ---------------------------------------------------------------------------

pub struct WmiBackend {
    tx: mpsc::Sender<(u64, WmiCmd)>,
    res: Mutex<mpsc::Receiver<(u64, WmiReply)>>,
    /// 请求序号：每次 call 递增，worker 应答回显。调用方以 seq 配对，
    /// 超时丢弃的过期应答不会被后续调用误配（见 call 的时序注释）。
    next_seq: std::sync::atomic::AtomicU64,
    /// 熔断：连续应答超时后置位，后续调用快速失败（不阻塞 GUI），
    /// 由调用方（切换后端/重建）恢复。
    wedged: std::sync::atomic::AtomicBool,
}

// `WmiBackend` 不含任何 COM 指针——所有 COM 对象归 worker 线程独占；
// 共享状态仅 mpsc 通道（`Sender<WmiCmd>` 为 Send+Sync）与
// `Mutex<Receiver<WmiReply>>`（`Receiver` 为 Send，`Mutex` 使其满足 Sync），
// 因此 `Send + Sync` 由字段自动推导，无需 unsafe 实现（历史 unsafe impl
// 是冗余的——见下方 test_wmi_backend_is_send_sync 的编译期断言）。

/// WMI 熔断根因日志闩：worker 死亡/通道断开/应答超时后，后续每次调用都在
/// wedged 快速失败分支短路，若逐条记录会刷屏（与 power.rs 的
/// warn_unknown_once 同源问题）。根因只在**首次**发生时记录一次，调用方
/// 对每个返回错误各自记录（GUI 展示）。
static WEDGE_CAUSE_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 记录熔断根因（仅首次，之后静默）。
fn log_wedge_cause_once(message: impl std::fmt::Display) {
    crate::util::log_once(
        log::Level::Error,
        &WEDGE_CAUSE_LOGGED,
        format_args!("WMI: {}", message),
    );
}

impl Drop for WmiBackend {
    fn drop(&mut self) {
        // 后端被销毁（切后端/进程退出）时通知 worker 线程退出。记录该
        // 事件，便于确认 worker 生命周期与后端一致（worker 退出日志见
        // WmiWorker::run）。
        log::info!("WMI: backend dropped; notifying worker to quit");
        let _ = self.tx.send((0, WmiCmd::Quit));
    }
}

impl WmiBackend {
    pub fn new() -> Result<Self, EcError> {
        // 新建后端 = 新一轮熔断周期：复位根因日志闩（修订 1.47 审计）。
        // 历史实现闩为进程级一次性——首次熔断（如 worker 死亡）记录后，同一
        // 会话内**下一次不同根因**（如应答超时）永久静默。后端重建（GUI 切
        // 换/WMI 恢复）即复位，每个新 worker 的首次根因都能被记录一次。
        WEDGE_CAUSE_LOGGED.store(false, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = mpsc::channel::<(u64, WmiCmd)>();
        let (res_tx, res_rx) = mpsc::channel::<(u64, WmiReply)>();
        // 与托盘/Fn/电池健康/自启动各后台线程共用 util::spawn_guarded 兜底
        //（修订 1.40 + 1.47 收敛）：release 已无 panic=abort，worker 内
        // COM/FFI 边界的 panic 只会静默终止本线程，进程继续运行但后端全错
        // 且无法自动恢复——panic 被捕获记录语义化错误；随后 rx 随闭包 drop，
        // 调用方 send 失败 → wedged 置位（见 call 的通道关闭分支）→ GUI
        // 单次点击即可重建。Builder 防 spawn 失败 panic 传播（此处转 Err）。
        crate::util::spawn_guarded("wmi-worker", move || {
            // COM 公寓生命周期：先 ensure_com（幂等，
            // S_FALSE 时视为已初始化），connect+run 结束统一
            // CoUninitialize——**仅在 ensure_com 成功后**配对
            // （修订 1.46 审计 + 1.47 修正）：CoInitializeEx 返回
            // 非 S_OK/S_FALSE 错误时本线程并未成功初始化 MTA
            // （RPC_E_CHANGED_MODE 意味着线程已有其它公寓），此时
            // 调用 CoUninitialize 会撤销**别处**的初始化——若该错误
            // 分支继续配对，可能与既有公寓计数错配。失败分支直接
            // 回传错误并结束，不做无配对的 CoUninitialize。
            if let Err(e) = ensure_com() {
                let _ = res_tx.send((0, WmiReply::Unit(Err(e))));
                return;
            }
            match WmiWorker::connect() {
                Ok(worker) => {
                    let _ = res_tx.send((0, WmiReply::Unit(Ok(()))));
                    worker.run(rx, res_tx);
                }
                Err(e) => {
                    let _ = res_tx.send((0, WmiReply::Unit(Err(e))));
                }
            }
            unsafe {
                CoUninitialize();
            }
        })
        .map_err(|e| EcError::WmiConnect(format!("spawn worker thread: {}", e)))?;
        match await_handshake(&res_rx, HANDSHAKE_TIMEOUT) {
            Ok(()) => Ok(Self {
                tx,
                res: Mutex::new(res_rx),
                next_seq: std::sync::atomic::AtomicU64::new(1),
                wedged: std::sync::atomic::AtomicBool::new(false),
            }),
            Err(e) => Err(e),
        }
    }

    /// 后端是否处于超时熔断状态（`recv_reply` 超时后置位）。
    ///
    /// GUI 切换后端的 no-op 判定（"已是该后端则跳过重建"）需要区分普通
    /// 后端与熔断后端：熔断后唯一恢复途径是**重建**（create_backend 生成
    /// 全新 worker），若仍按"同种后端 no-op"处理，WMI-only 机器上后端将
    /// 永久卡死在熔断态（F2 回归，见 gui::commands::try_switch_backend）。
    pub fn is_wedged(&self) -> bool {
        self.wedged.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn call(&self, cmd: WmiCmd) -> WmiReply {
        // 熔断：之前有调用应答超时（WMI 服务卡死），本调用直接快速失败，
        // 不再浪费 CALL_REPLY_TIMEOUT 的等待时间。用户可在 GUI 切换后端
        // 重建（create_backend 生成全新 worker），恢复后自动解除熔断。
        if self.wedged.load(std::sync::atomic::Ordering::Relaxed) {
            // 熔断后的每次调用都走这里快速失败，但**根因已在首次熔断时记录**
            // （见 wedged 置位的三处 error 日志），此处重复 warn 会刷屏——
            // 调用方对每个返回错误各自记录（GUI 会展示），本条只留 debug 痕迹。
            log::debug!("WMI: backend wedged; failing call fast");
            return WmiReply::Unit(Err(EcError::BackendUnavailable(
                "WMI worker 无响应（超时熔断，请切换后端重试）".into(),
            )));
        }
        // 命令发送与应答接收必须在同一把锁内串行完成：应答通道是单一的，
        // 若只锁 recv，两个并发调用线程的发送可能交错（A/B 先后 send，
        // worker 按命令 FIFO 产生应答），而锁只保护接收端——B 抢到锁后
        // 可能收到 A 的命令的应答，错误被静默错配到另一个调用方
        // （WmiReply 不带请求关联标识）。当前 GUI 是唯一后端调用方，
        // 该问题潜伏；把锁覆盖 send+recv 即从架构上根除错配可能。
        // 耗时在 debug 级别记录：GUI 线程阻塞在 recv() 的时长与硬件调用
        // 时长一致，定位"界面冻结/卡顿"时凭此区分具体卡在哪个命令上。
        // 命令描述字符串只在慢调用日志分支需要——健康路径（5~16ms）不
        // 预先 format，避免每次调用都分配一个不使用的 String（唯一热路径
        // 分配，见审计）。
        let start = std::time::Instant::now();
        let cmd_name = cmd.name();
        let seq = self
            .next_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let guard = crate::util::lock_or_recover(&self.res, "WMI");
        if self.tx.send((seq, cmd)).is_err() {
            // worker 已退出（Quit 未及处理/线程崩溃/通道对端全部 drop）：
            // 之后所有调用都会在此快速失败。这是"后端突然全部报错"的根因，
            // 由 log_wedge_cause_once 在**首次**发生时记录一次（后续调用经
            // 上方 wedged 快速失败，各自错误由调用方记录），避免刷屏。
            //
            // **熔断置位**（修订 1.40 回归）：死 worker 与超时熔断同样只能靠
            // 重建恢复——若 wedged 不置位，`needs_rebuild()` 返回 false，
            // GUI"同种后端 no-op"判定会拒绝重建、延迟恢复探测也因
            // `preference()==Wmi` 不再发起，恢复需要手动切到 WinRing0 再切回
            // 两步。置位后单次点击即可重建。
            return WmiReply::Unit(Err(self.wedge(
                format!("worker channel closed; {cmd_name} cannot be dispatched (worker dead)"),
                "WMI worker 已退出",
            )));
        }
        let reply = match self.recv_reply(&guard, seq) {
            Ok(r) => r,
            Err(e) => return WmiReply::Unit(Err(e)),
        };
        // 调用耗时：健康固件单次调用 5~16ms，卡死时最长约 3000ms（超时）。
        // 默认日志级别为 info：低于阈值只留 debug 痕迹，超过阈值
        // （"界面冻结/卡顿"高发区间）升级为 warn，在默认日志里直接看到
        // "哪条命令卡了多久"，无需翻 debug 日志定位 GUI 卡顿来源。
        let elapsed_ms = start.elapsed().as_millis();
        if elapsed_ms > SLOW_CALL_WARN_MS {
            log::warn!(
                "WMI call {} took {} ms (>{SLOW_CALL_WARN_MS} ms; UI likely stalling)",
                cmd_name,
                elapsed_ms
            );
        } else {
            log::debug!("WMI call {} took {} ms", cmd_name, elapsed_ms);
        }
        reply
    }

    /// 等待与 `seq` 匹配的应答，带超时与过期应答清理。
    ///
    /// **为什么超时**（T1 回归）：worker 的 WMI 调用上限是
    /// GET_RESULT_TIMEOUT_MS=3000ms，但 WMI 服务卡死时（休眠唤醒/提供者
    /// 死锁）该超时不保证兑现——历史实现 `guard.recv()` 无限阻塞，单个
    /// wedged worker 永久冻结所有后端调用（锁被持死），GUI 彻底无响应。
    /// 改为 `recv_timeout`：超过 `CALL_REPLY_TIMEOUT` 即熔断，后续调用
    /// 快速失败（见 wedged），GUI 保持响应，由上层（切换后端）恢复。
    ///
    /// **过期应答清理**：超时返回后，那个命令的应答可能在之后才到达，
    /// 堆积在通道里。若不清除，下一次 call 会把它误当自己的应答。用
    /// seq 配对：每次 recv 拿到非本序号的应答直接丢弃并继续等，直到
    /// 本 seq 应答或超时——过期应答永远不会污染后续调用。
    fn recv_reply(
        &self,
        guard: &std::sync::MutexGuard<'_, mpsc::Receiver<(u64, WmiReply)>>,
        seq: u64,
    ) -> Result<WmiReply, EcError> {
        let deadline = std::time::Instant::now() + CALL_REPLY_TIMEOUT;
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            match guard.recv_timeout(deadline - now) {
                Ok((got_seq, r)) if got_seq == seq => return Ok(r),
                Ok((stale_seq, _)) => {
                    log::warn!(
                        "WMI: discarding stale reply for seq {} (expected {}; from a timed-out call)",
                        stale_seq,
                        seq
                    );
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    // worker 退出导致通道断开：与 send 失败同因，同样熔断
                    //（修订 1.40 回归，见 call 的通道关闭分支注释）。
                    return Err(self.wedge(
                        "worker disconnected while waiting for reply",
                        "WMI worker 无响应",
                    ));
                }
            }
        }
        Err(self.wedge(
            format_args!(
                "reply timeout after {} ms; wedging backend",
                CALL_REPLY_TIMEOUT.as_millis()
            ),
            "WMI worker 无响应（超时熔断）",
        ))
    }

    /// 熔断：置位 wedged + 记录根因（首次）+ 返回统一的 BackendUnavailable。
    ///
    /// 三条熔断路径（send 失败 / 等待应答时通道断开 / 应答超时）此前各自
    /// 手写同一段 `wedged.store + log_wedge_cause_once + Err(BackendUnavailable)`
    ///（修订 1.48 收敛）——统一入口保证未来的新熔断路径不会被遗漏。
    fn wedge(&self, cause: impl std::fmt::Display, err_msg: &str) -> EcError {
        self.wedged
            .store(true, std::sync::atomic::Ordering::Relaxed);
        log_wedge_cause_once(cause);
        EcError::BackendUnavailable(err_msg.into())
    }

    /// 统一应答分派核心：期望应答变体由 `take` 提取。
    ///
    /// - `Unit(Err)`（熔断/通道关闭路径）**先于提取器**拦截并如实透传具体
    ///   错误（如"无响应（超时熔断，请切换后端重试）"），不退化成语义
    ///   掩盖可操作的提示（F3）；
    /// - 提取器命中期望变体 → 返回其 `Result`；
    /// - 其余类型不匹配（worker 异常）→ 统一 `BackendUnavailable`。
    ///
    /// 历史实现 `unit`/`battery`/`perf` 各自手写上述三件套，修订 1.49 收敛。
    fn reply<T>(
        &self,
        reply: WmiReply,
        take: impl FnOnce(WmiReply) -> Option<Result<T, EcError>>,
    ) -> Result<T, EcError> {
        match reply {
            WmiReply::Unit(Err(e)) => Err(e),
            r => match take(r) {
                Some(res) => res,
                None => Err(EcError::BackendUnavailable("WMI worker 响应异常".into())),
            },
        }
    }

    fn unit(&self, cmd: WmiCmd) -> Result<(), EcError> {
        self.reply(self.call(cmd), |r| match r {
            WmiReply::Unit(r) => Some(r),
            _ => None,
        })
    }

    fn battery(&self, cmd: WmiCmd) -> Result<(bool, u8), EcError> {
        self.reply(self.call(cmd), |r| match r {
            WmiReply::BatteryState(r) => Some(r),
            _ => None,
        })
    }

    fn perf(&self, cmd: WmiCmd) -> Result<u8, EcError> {
        self.reply(self.call(cmd), |r| match r {
            WmiReply::PerfMode(r) => Some(r),
            _ => None,
        })
    }
}

impl EcBackend for WmiBackend {
    fn name(&self) -> &'static str {
        "WMI (MICommonInterface)"
    }

    fn preference(&self) -> crate::app::config::BackendPreference {
        crate::app::config::BackendPreference::Wmi
    }

    /// 超时熔断后必须重建才能恢复（F2）：`is_wedged` 的 trait 化入口，
    /// 供 GUI 后端切换逻辑在"同种后端 no-op"判定前识别熔断态。
    fn needs_rebuild(&self) -> bool {
        self.is_wedged()
    }

    fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
        self.battery(WmiCmd::GetBatteryState).map(|(care, _)| care)
    }

    fn get_charge_limit(&self) -> Result<u8, EcError> {
        self.battery(WmiCmd::GetBatteryState)
            .map(|(_, limit)| limit)
    }

    fn get_battery_state(&self) -> Result<(bool, u8), EcError> {
        self.battery(WmiCmd::GetBatteryState)
    }

    fn set_battery_care(&self, enabled: bool) -> Result<(), EcError> {
        self.unit(WmiCmd::SetBatteryCare(enabled))
    }

    fn set_charge_limit(&self, percent: u8) -> Result<(), EcError> {
        self.unit(WmiCmd::SetChargeLimit(percent))
    }

    fn get_performance_mode(&self) -> Result<u8, EcError> {
        self.perf(WmiCmd::GetPerfMode)
    }

    fn set_performance_mode(&self, mode: u8) -> Result<(), EcError> {
        self.unit(WmiCmd::SetPerfMode(mode))
    }

    fn supports_continuous_charge_limit(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 编译期断言：WmiBackend 必须是 Send + Sync（无需 unsafe 实现，
    /// 由字段自动推导）。`Box<dyn EcBackend>` 要求该约束；若未来字段
    /// 类型变化导致约束不满足，此处会直接编译失败。
    #[test]
    fn test_wmi_backend_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WmiBackend>();
    }

    /// 实例名转义：WMI 对象路径中反斜杠与引号必须加倍，
    /// 否则路径解析失败（Meow-Box 同款路径格式）。
    #[test]
    fn test_escape_instance_name() {
        assert_eq!(escape_instance_name("MIFS"), "MIFS");
        assert_eq!(
            escape_instance_name("ACPI\\PNP0C14\\MIFS_0"),
            "ACPI\\\\PNP0C14\\\\MIFS_0"
        );
        assert_eq!(escape_instance_name("a\"b"), "a\\\"b");
        assert_eq!(escape_instance_name("A\\B\"C"), "A\\\\B\\\"C");
    }

    /// 目标实例选择策略（F-HAL-08c）：active + 含 MIFS 的实例优先，否则
    /// 取第一个；空列表返回 None（调用方已先判空）。
    #[test]
    fn test_select_target_instance_policy() {
        let mk = |n: &str, a: bool, m: bool| (n.to_string(), a, m);
        let v = vec![
            mk("ACPI\\PNP0C14\\MII_0", true, false),
            mk("ACPI\\PNP0C14\\MIFS_0", true, true),
            mk("ACPI\\PNP0C14\\Y_0", false, true),
        ];
        assert_eq!(
            select_target_instance(&v),
            Some("ACPI\\PNP0C14\\MIFS_0"),
            "active+MIFS must win over first instance"
        );
        // 无 active+MIFS：取第一个。
        let v2 = vec![mk("first", true, false), mk("second", false, true)];
        assert_eq!(select_target_instance(&v2), Some("first"));
        // 单个实例（如 inactive）：仍返回它（first 回退）。
        let v3 = vec![mk("only", false, false)];
        assert_eq!(select_target_instance(&v3), Some("only"));
        // 空：None。
        assert_eq!(select_target_instance(&[]), None);
    }

    /// 命令缓冲的小端写入：字节序错误会让 WMI 把充电上限/命令参数解析成
    /// 错误值（本机实证过 raw code 乱序问题）。直接锁定字节布局。
    #[test]
    fn test_put_le16_put_le32_byte_layout() {
        let mut buf = [0u8; 32];
        put_le16(&mut buf, 0, 0x1234);
        assert_eq!(&buf[0..2], &[0x34, 0x12], "u16 must be little-endian");
        put_le32(&mut buf, 2, 0x12345678);
        assert_eq!(
            &buf[2..6],
            &[0x78, 0x56, 0x34, 0x12],
            "u32 must be little-endian"
        );
        // 偏移写入互不覆盖。
        assert_eq!(buf[0], 0x34);
        assert_eq!(buf[1], 0x12);
    }

    /// 命令缓冲组合（F-HAL-01/02/07 字节布局回归）：四条命令的 fun1/fun2/
    /// fun3/fun4 必须落在固定偏移——字段错位曾在真机造成限值解析错乱。
    #[test]
    fn test_command_buffer_composition() {
        // 电池充电读：fun1=读(0xFA00) fun2=电池(0x1000) fun3=充电读(0x0002) fun4=0
        let bat_read = read_battery_cmd();
        assert_eq!(&bat_read[0..2], &[0x00, 0xFA]);
        assert_eq!(&bat_read[2..4], &[0x00, 0x10]);
        assert_eq!(&bat_read[4..6], &[0x02, 0x00]);
        assert!(bat_read[6..].iter().all(|&b| b == 0));

        // 电池充电写：fun1=写(0xFB00) fun2=电池(0x1000) fun3=充电写(0x0002)
        // fun4=raw code（0x01 = 80%），LE 4 字节。
        let bat_write = write_battery_cmd(0x01);
        assert_eq!(&bat_write[0..2], &[0x00, 0xFB]);
        assert_eq!(&bat_write[2..4], &[0x00, 0x10]);
        assert_eq!(&bat_write[4..6], &[0x02, 0x00]);
        assert_eq!(&bat_write[6..10], &[0x01, 0x00, 0x00, 0x00]);
        assert!(bat_write[10..].iter().all(|&b| b == 0));

        // 性能读：fun1=读 fun2=性能(0x0800) fun3=性能读(0x0000) fun4=0
        let perf_read = read_perf_cmd();
        assert_eq!(&perf_read[0..2], &[0x00, 0xFA]);
        assert_eq!(&perf_read[2..4], &[0x00, 0x08]);
        assert_eq!(&perf_read[4..6], &[0x00, 0x00]);
        assert!(perf_read[6..].iter().all(|&b| b == 0));

        // 性能写：fun1=写(0xFB00) fun2=性能(0x0800) fun3=模式 raw code
        //（如 Smart 0x09）fun4=0。
        let perf_write = write_perf_cmd(0x09);
        assert_eq!(&perf_write[0..2], &[0x00, 0xFB]);
        assert_eq!(&perf_write[2..4], &[0x00, 0x08]);
        assert_eq!(&perf_write[4..6], &[0x09, 0x00]);
        assert!(perf_write[6..].iter().all(|&b| b == 0));
    }

    /// 回归测试（本机实证）：响应 Status 成功值为 0x8000 而非 0。
    #[test]
    fn test_status_success_is_0x8000() {
        assert_eq!(WMI_STATUS_SUCCESS, 0x8000);
        // 成功响应（实测 perf read）：00 80 00 08 09 00 ...
        let out = [0x00u8, 0x80, 0x00, 0x08, 0x09, 0x00];
        let status = u16::from_le_bytes([out[0], out[1]]);
        assert_eq!(status, WMI_STATUS_SUCCESS);
        // 失败响应（实测非法充电值写入）：00 00 ...
        let fail = [0x00u8, 0x00];
        assert_ne!(u16::from_le_bytes([fail[0], fail[1]]), WMI_STATUS_SUCCESS);
    }

    /// 回归测试（本机实证）：输出数组为 30 字节（MOF MAX=30），
    /// 有效字段 18 字节；历史实现要求 ≥32 字节导致成功响应全被误判。
    #[test]
    fn test_output_min_length_is_18() {
        // 编译期断言：MIN_OUTPUT_LEN 恒为 18（2+2+2+4+4+4 的字段布局，
        // 见 F-HAL-08）。作为回归测试同时锁定"实测 30>18 必须通过长度校验"
        // 的关系。
        const _: () = assert!(MIN_OUTPUT_LEN == 18);
        const _: () = assert!(2 + 2 + 2 + 4 + 4 + 4 == MIN_OUTPUT_LEN);
        const _: () = assert!(30 >= MIN_OUTPUT_LEN);
    }

    /// 回归测试（B-WMI-2）：确定性致命错误必须熔断——坏固件上每次调用都
    /// 返回相同的确定性错误，不熔断则每次调用都重复等待完整超时。
    /// 0x8004102F（WBEM_E_INVALID_METHOD_PARAMETERS）是对类路径调用的
    /// 拒绝错误；WBEM_E_PROVIDER_FAILURE 是另一个确定性错误。
    #[test]
    fn test_is_latching_hresult_deterministic_errors() {
        assert!(is_latching_hresult(
            WBEM_E_INVALID_METHOD_PARAMETERS.0 as u32
        ));
        assert!(is_latching_hresult(WBEM_E_PROVIDER_FAILURE.0 as u32));
        assert!(is_latching_hresult(WBEM_E_INVALID_CLASS.0 as u32));
        assert!(is_latching_hresult(WBEM_E_NOT_FOUND.0 as u32));
        assert!(is_latching_hresult(WBEM_E_INVALID_METHOD.0 as u32));
        assert!(is_latching_hresult(WBEM_E_NOT_SUPPORTED.0 as u32));
        assert!(is_latching_hresult(WBEM_E_INVALID_PARAMETER.0 as u32));
    }

    /// 回归测试（B-WMI-2）：瞬态错误不得熔断，否则 WMI 服务重启、休眠
    /// 唤醒等临时故障会永久禁用后端。
    #[test]
    fn test_is_latching_hresult_transient_errors() {
        // GetResultObject 超时经 windows-rs from_abi(NULL) 掩盖为 E_FAIL；
        // 均属瞬态，不得熔断。
        assert!(!is_latching_hresult(0x80004003)); // E_POINTER
        assert!(!is_latching_hresult(0x80004005)); // E_FAIL
        assert!(!is_latching_hresult(WBEM_E_TIMED_OUT.0 as u32));
        assert!(!is_latching_hresult(0x80041017)); // WBEM_E_OUT_OF_MEMORY
        assert!(!is_latching_hresult(0x800401F0)); // CO_E_NOTINITIALIZED
        assert!(!is_latching_hresult(0x800706BA)); // RPC_S_SERVER_UNAVAILABLE
    }

    /// 回归测试（B-WMI-2）：熔断只保存首个错误，后续错误不覆盖。
    #[test]
    fn test_latch_into_stores_first_error_only() {
        let mut state = None::<EcError>;
        let first = EcError::WmiCallHResult(WBEM_E_PROVIDER_FAILURE.0 as u32);
        let returned = latch_into(
            &mut state,
            Some(WBEM_E_PROVIDER_FAILURE.0 as u32),
            first.clone(),
        );
        assert_eq!(returned.to_string(), first.to_string());

        let second = EcError::WmiCallHResult(WBEM_E_INVALID_CLASS.0 as u32);
        let _ = latch_into(&mut state, Some(WBEM_E_INVALID_CLASS.0 as u32), second);
        let latched = state.as_ref().expect("latched").to_string();
        assert_eq!(latched, first.to_string());
    }

    /// 回归测试（B-WMI-2）：接口缺失（无 hr）视为必然失败，必须熔断。
    #[test]
    fn test_latch_into_force_on_missing_interface() {
        let mut state = None::<EcError>;
        let err = EcError::WmiInterfaceNotFound;
        let _ = latch_into(&mut state, None, err);
        assert!(state.is_some());
    }

    /// 回归测试（B-WMI-2）：瞬态错误不写入熔断状态。
    #[test]
    fn test_latch_into_ignores_transient() {
        let mut state = None::<EcError>;
        let _ = latch_into(
            &mut state,
            Some(0x80004005),
            EcError::WmiCallHResult(0x80004005),
        );
        assert!(state.is_none());
    }

    /// 回归测试（本机实证）：COM 是**每线程**初始化的。worker 模式下
    /// ensure_com 仅在 worker 线程调用；此处验证其可重复调用。
    #[test]
    fn test_ensure_com_initializes_on_any_thread() {
        let t1 = std::thread::spawn(ensure_com);
        let t2 = std::thread::spawn(ensure_com);
        assert!(t1.join().unwrap().is_ok());
        assert!(t2.join().unwrap().is_ok());
    }

    /// 回归测试（本机实证）：WmiBackend 为线程亲和 worker 代理——
    /// 任意线程调用都经命令通道在 worker 线程执行，天然支持跨线程。
    /// 本机没有 MICommonInterface 实例或固件拒绝协议时，调用会以
    /// 类不存在/状态错误等其它原因失败——只要不是 0x800401F0 即说明
    /// 调用路径已自行完成 COM 初始化，断言通过；无 WMI 环境直接跳过。
    #[test]
    fn test_wmi_backend_callable_from_foreign_thread() {
        let backend = match WmiBackend::new() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skip: WMI unavailable ({})", e);
                return;
            }
        };
        let handle = std::thread::spawn(move || match backend.get_performance_mode() {
            Ok(_) => Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("0x800401F0") {
                    Err(format!(
                        "calling thread COM not initialized by call path: {}",
                        msg
                    ))
                } else {
                    Ok(())
                }
            }
        });
        assert!(
            handle.join().unwrap().is_ok(),
            "WMI backend calls must initialize COM on the calling thread"
        );
    }

    /// 回归测试（修订 1.40）：worker 线程死亡（通道关闭）后，下一次调用必须
    /// **熔断置位**（`needs_rebuild()` 返回 true）——否则 GUI"同种后端 no-op"
    /// 判定拒绝重建、延迟恢复探测也不发起，恢复需要手动切到 WinRing0 再切回
    /// 两步。构造通道对端已 drop 的后端，调用一次即应置位。
    #[test]
    fn test_dead_worker_wedges_backend() {
        // 本测试触发 log_wedge_cause_once，会置位进程级根因日志闩（WEDGE_CAUSE_LOGGED）
        //——save/restore 避免测试运行后生产/其它测试的熔断根因日志被永久静默
        //（与 power.rs 的 warn_unknown_once 测试恢复模式一致）。
        let prev_wedge_cause = WEDGE_CAUSE_LOGGED.load(std::sync::atomic::Ordering::Relaxed);
        WEDGE_CAUSE_LOGGED.store(false, std::sync::atomic::Ordering::Relaxed);
        let (tx, _rx) = mpsc::channel::<(u64, WmiCmd)>();
        drop(_rx); // 模拟 worker 已退出：命令通道对端已 drop。
        let (res_tx, res_rx) = mpsc::channel::<(u64, WmiReply)>();
        drop(res_tx); // 应答通道也关闭（recv 不会走到，send 已先失败）。
        let backend = WmiBackend {
            tx,
            res: std::sync::Mutex::new(res_rx),
            next_seq: std::sync::atomic::AtomicU64::new(1),
            wedged: std::sync::atomic::AtomicBool::new(false),
        };
        assert!(!backend.is_wedged());
        match backend.call(WmiCmd::GetBatteryState) {
            WmiReply::Unit(Err(EcError::BackendUnavailable(_))) => {}
            other => panic!("expected BackendUnavailable, got {:?}", other),
        }
        assert!(backend.is_wedged(), "dead worker must wedge the backend");
        assert!(
            backend.needs_rebuild(),
            "needs_rebuild() must be true so the GUI can rebuild in one click"
        );
        WEDGE_CAUSE_LOGGED.store(prev_wedge_cause, std::sync::atomic::Ordering::Relaxed);
    }

    #[test]
    fn test_wmi_rawcode_for_percent_exact() {
        assert_eq!(wmi_rawcode_for_percent(100), Some(0));
        assert_eq!(wmi_rawcode_for_percent(80), Some(1));
        assert_eq!(wmi_rawcode_for_percent(90), Some(4));
        assert_eq!(wmi_rawcode_for_percent(70), Some(5));
        assert_eq!(wmi_rawcode_for_percent(60), Some(6));
        assert_eq!(wmi_rawcode_for_percent(50), Some(7));
        assert_eq!(wmi_rawcode_for_percent(40), Some(8));
    }

    /// 回归测试（B-WMI-4）：并发调用后端时，应答必须按命令正确配对——
    /// 不能出现线程 A 收到线程 B 的命令应答。历史实现只锁 recv（不锁
    /// send），两个线程并发调用时 B 抢到锁后可能收到 A 的命令的应答，
    /// 错误被静默错配。修复后锁覆盖 send+recv 全程串行，配对确定。
    /// 用延迟应答的仿真 worker 放大竞争窗口，两个线程各自反复调用不同
    /// 方法，校验各自收到的应答类型与值。
    #[test]
    fn test_concurrent_calls_pair_replies() {
        use std::sync::Arc;

        let (tx, rx) = mpsc::channel::<(u64, WmiCmd)>();
        let (res_tx, res_rx) = mpsc::channel::<(u64, WmiReply)>();
        let backend = Arc::new(WmiBackend {
            tx,
            res: Mutex::new(res_rx),
            next_seq: std::sync::atomic::AtomicU64::new(1),
            wedged: std::sync::atomic::AtomicBool::new(false),
        });

        // 仿真 worker：按命令 FIFO 应答，每次应答前微眠放大并发窗口。
        std::thread::spawn(move || {
            while let Ok((seq, cmd)) = rx.recv() {
                let reply = match cmd {
                    WmiCmd::GetPerfMode => WmiReply::PerfMode(Ok(0x99)),
                    WmiCmd::GetBatteryState => WmiReply::BatteryState(Ok((true, 0x55))),
                    _ => WmiReply::Unit(Err(EcError::BackendUnavailable("unexpected".into()))),
                };
                std::thread::sleep(std::time::Duration::from_micros(150));
                if res_tx.send((seq, reply)).is_err() {
                    break;
                }
            }
        });

        const ITERS: usize = 40;
        let a = backend.clone();
        let thread_a = std::thread::spawn(move || {
            let mut got = Vec::with_capacity(ITERS);
            for _ in 0..ITERS {
                got.push(backend_get_perf(&a));
            }
            got
        });
        let b = backend.clone();
        let thread_b = std::thread::spawn(move || {
            let mut got = Vec::with_capacity(ITERS);
            for _ in 0..ITERS {
                got.push(backend_get_battery(&b));
            }
            got
        });

        let got_a = thread_a.join().expect("thread A panicked");
        let got_b = thread_b.join().expect("thread B panicked");

        assert!(
            got_a.iter().all(|&m| m == 0x99),
            "A got wrong replies: {:?}",
            got_a
        );
        assert!(
            got_b.iter().all(|&(c, l)| c && l == 0x55),
            "B got wrong replies: {:?}",
            got_b
        );
    }

    fn backend_get_perf(b: &WmiBackend) -> u8 {
        match b.get_performance_mode() {
            Ok(m) => m,
            Err(e) => panic!("perf call failed: {}", e),
        }
    }

    fn backend_get_battery(b: &WmiBackend) -> (bool, u8) {
        match b.get_battery_state() {
            Ok(s) => s,
            Err(e) => panic!("battery call failed: {}", e),
        }
    }

    /// 回归测试：`recv_reply` 的**过期应答清理**——通道中先到达的过期应答
    /// （旧 seq）必须被丢弃、继续等待本 seq 的应答，绝不把过期应答误配给
    /// 当前调用（T1 熔断的唯一防错配保护，修订 1.25）。
    ///
    /// 覆盖场景：调用 seq=7 时，通道里残留着上一次超时调用的应答
    /// (seq=5) 与本调用应答 (seq=7)。`recv_reply` 必须先跳过 seq=5 再返回
    /// seq=7；若不清除，会静默把 seq=5 的应答错配给 seq=7 的调用方。
    #[test]
    fn test_recv_reply_discards_stale_seq() {
        // 直接构造 WmiBackend，用发送端预置应答序列（不真正起 worker，
        // 也不走 call()——只验证 recv_reply 的 seq 配对逻辑）。
        let (res_tx, res_rx) = mpsc::channel::<(u64, WmiReply)>();
        let backend = WmiBackend {
            tx: mpsc::channel::<(u64, WmiCmd)>().0, // 测试不发送命令
            res: Mutex::new(res_rx),
            next_seq: std::sync::atomic::AtomicU64::new(1),
            wedged: std::sync::atomic::AtomicBool::new(false),
        };
        // 预置：过期应答（seq=5）先于当前应答（seq=7）到达通道。
        res_tx
            .send((5, WmiReply::PerfMode(Ok(0x77))))
            .expect("preload stale");
        res_tx
            .send((7, WmiReply::PerfMode(Ok(0x99))))
            .expect("preload current");
        // 等待 seq=7 时，seq=5 的过期应答必须被丢弃，最终返回 seq=7 的
        // 应答（0x99）而非 0x77。
        let guard = crate::util::lock_or_recover(&backend.res, "WMI");
        let reply = backend
            .recv_reply(&guard, 7)
            .expect("seq=7 reply must be returned");
        match reply {
            WmiReply::PerfMode(Ok(mode)) => assert_eq!(mode, 0x99, "stale reply must be discarded"),
            other => panic!("unexpected reply: {:?}", other),
        }
    }

    #[test]
    fn test_wmi_rawcode_for_percent_nearest() {
        assert_eq!(wmi_rawcode_for_percent(85), Some(1)); // 80%（与最近预设一致）
        assert_eq!(wmi_rawcode_for_percent(55), Some(6)); // 60%
        assert_eq!(wmi_rawcode_for_percent(95), Some(0)); // 100%
        assert_eq!(wmi_rawcode_for_percent(45), Some(7)); // 50%
    }

    /// 回归测试（握手超时）：worker 未在时限内应答时，await_handshake 必须
    /// 返回 Err 而非无限阻塞——WMI 服务卡死时，`WmiBackend::new()` 不能
    /// 让 main 的后端初始化线程永久挂起、GUI 永远无法启动。
    #[test]
    fn test_await_handshake_times_out() {
        let (_tx, rx) = mpsc::channel::<(u64, WmiReply)>();
        let err = await_handshake(&rx, std::time::Duration::from_millis(50))
            .expect_err("no handshake reply must time out");
        assert!(
            err.to_string().contains("握手超时"),
            "unexpected error: {}",
            err
        );
    }

    /// 握手成功：worker 发送 Ok 后 await_handshake 立即返回 Ok。
    #[test]
    fn test_await_handshake_success() {
        let (tx, rx) = mpsc::channel::<(u64, WmiReply)>();
        tx.send((0, WmiReply::Unit(Ok(())))).unwrap();
        await_handshake(&rx, std::time::Duration::from_secs(5)).expect("success handshake");
    }

    /// 握手失败：worker 上报连接错误时必须原样透传，供调用方回退。
    #[test]
    fn test_await_handshake_propagates_error() {
        let (tx, rx) = mpsc::channel::<(u64, WmiReply)>();
        tx.send((0, WmiReply::Unit(Err(EcError::WmiInterfaceNotFound))))
            .unwrap();
        let err = await_handshake(&rx, std::time::Duration::from_secs(5)).expect_err("must fail");
        assert!(
            err.to_string().contains("MICommonInterface"),
            "unexpected: {}",
            err
        );
    }

    /// 握手应答类型不符（异常）：必须返回错误而不是静默成功。
    #[test]
    fn test_await_handshake_wrong_reply_kind() {
        let (tx, rx) = mpsc::channel::<(u64, WmiReply)>();
        tx.send((0, WmiReply::PerfMode(Ok(0)))).unwrap();
        let err = await_handshake(&rx, std::time::Duration::from_secs(5)).expect_err("must fail");
        assert!(
            err.to_string().contains("握手响应异常"),
            "unexpected: {}",
            err
        );
    }

    /// 回归测试（F-BUG）：连接重试的总退避必须明显小于 HANDSHAKE_TIMEOUT，
    /// 否则重试尚未结束父端已超时放弃，重试逻辑形同虚设。锁定常量值防漂移。
    #[test]
    fn test_connect_retry_budget_within_handshake_timeout() {
        // 常量断言：重试次数是编译期常量，clippy 视运行期 assert 为
        // "恒定断言"（NFR-MNT-03）。用 const 块保持同等的编译期保障。
        const _: () = assert!(CONNECT_ATTEMPTS >= 2, "retry loop must be meaningful");
        let total_backoff = (CONNECT_ATTEMPTS - 1) as u128 * CONNECT_RETRY_DELAY.as_millis();
        // 预留单次连接（ConnectServer + ExecQuery）的耗时余量：总退避须小于
        // 握手上限的 80%，避免极端情况下超时截止前连最后一试都轮不到。
        assert!(
            total_backoff * 10 < HANDSHAKE_TIMEOUT.as_millis() * 8,
            "retry backoff {}ms too close to handshake timeout {}ms",
            total_backoff,
            HANDSHAKE_TIMEOUT.as_millis()
        );
    }
}
