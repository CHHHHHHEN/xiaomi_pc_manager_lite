//! WMI EC backend — MICommonInterface.MiInterface protocol
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
//! 从架构上根除跨线程 COM，并顺带把 GetResultObject 的最长阻塞
//! （GET_RESULT_TIMEOUT_MS）从调用线程移到 worker 线程，GUI 不再冻结。

use std::sync::mpsc;
use std::sync::Mutex;

use super::backend::EcBackend;
use super::battery;
use super::error::EcError;

use windows::Win32::System::Com::{
    CoInitializeEx, CoSetProxyBlanket, CoCreateInstance, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Ole::SafeArrayCreateVector;
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayDestroy,
    SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayUnaccessData,
};
use windows::Win32::System::Variant::{VARIANT, VARENUM, VT_ARRAY, VT_UI1, VariantClear};
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

/// GetResultObject 等待上限。健康固件上单次调用 5~16ms 即可返回。
/// 超时阻塞发生在 worker 线程，不影响调用线程。
const GET_RESULT_TIMEOUT_MS: i32 = 3000;

/// MiInterface 响应 Status 成功值（本机 2025 RedmiBook Pro 14 实测：
/// 所有成功调用恒返回 0x8000；写入无效值返回 0x0000）。
const WMI_STATUS_SUCCESS: u16 = 0x8000;

/// MiInterface 响应有效字段长度：Status(2)+Function(2)+Data0(2)+
/// Data1(4)+Data2(4)+Data3(4) = 18 字节。本机实测 OutData 为 30 字节
/// （MOF OutData MAX=30），历史实现要求 ≥32 字节导致成功响应全被误判。
const MIN_OUTPUT_LEN: usize = 18;

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

fn to_le16(buf: &mut [u8; 32], offset: usize, val: u16) {
    buf[offset] = (val & 0xFF) as u8;
    buf[offset + 1] = ((val >> 8) & 0xFF) as u8;
}

/// 将百分比换算为 WMI 充电上限 raw code：精确匹配优先，否则取最近的预设值。
fn wmi_rawcode_for_percent(percent: u8) -> u8 {
    battery::percent_to_wmi_rawcode(percent)
        .or_else(|| battery::percent_to_wmi_rawcode(battery::nearest_wmi_percent(percent)))
        .unwrap_or(0)
}

/// WMI 对象路径中的字符串值转义：反斜杠与引号需加倍（Meow-Box 的实例路径
/// 亦为 `MICommonInterface.InstanceName="ACPI\\PNP0C14\\MIFS_0"` 形式）。
fn escape_instance_name(name: &str) -> String {
    name.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 是否属于确定性致命错误（应熔断）：固件/提供程序层面的确定性失败，
/// 重试必然再次失败。瞬态错误（超时、服务忙、连接中断等）不熔断，
/// 否则 WMI 服务重启等临时故障会永久禁用后端。
fn is_latching_hresult(hr: u32) -> bool {
    const FATAL: &[u32] = &[
        // WBEM_E_INVALID_METHOD_PARAMETERS (0x8004102F)：对**类路径**调用
        // MiInterface 时恒被 WinMgmt 以此拒绝（1~64 字节输入全部复现）。
        // 正确实现（实例调用）不会出现该错误，保留在列表作为防御。
        WBEM_E_INVALID_METHOD_PARAMETERS.0 as u32,
        WBEM_E_PROVIDER_FAILURE.0 as u32,
        // 类/方法层面不存在或不受支持：机器不支持该接口，重试不会成功。
        WBEM_E_INVALID_CLASS.0 as u32,
        WBEM_E_NOT_FOUND.0 as u32,
        WBEM_E_INVALID_METHOD.0 as u32,
        WBEM_E_NOT_SUPPORTED.0 as u32,
        WBEM_E_INVALID_PARAMETER.0 as u32,
    ];
    FATAL.contains(&hr)
}

/// 将错误写入熔断状态（hr 为确定性致命错误或 None 表示必然失败时），
/// 返回原始错误。worker 线程独占 state，无需锁。
fn latch_into(state: &mut Option<EcError>, hr: Option<u32>, err: EcError) -> EcError {
    let fatal = match hr {
        None => true,
        Some(hr) => is_latching_hresult(hr),
    };
    if fatal && state.is_none() {
        log::error!("WMI: latching fatal error '{}'; subsequent calls fail fast", err);
        *state = Some(err.clone());
    }
    err
}

// ---------------------------------------------------------------------------
// Worker：独占全部 COM 状态与调用
// ---------------------------------------------------------------------------

enum WmiCmd {
    GetBatteryState,
    SetBatteryCare(bool),
    SetChargeLimit(u8),
    GetPerfMode,
    SetPerfMode(u8),
    Quit,
}

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
}

impl WmiWorker {
    fn connect() -> Result<Self, EcError> {
        ensure_com()?;
        let locator: IWbemLocator = unsafe {
            CoCreateInstance(&CLSID_WMI_LOCATOR, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| EcError::WmiConnect(format!("CoCreateInstance: {}", e)))?
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
                .map_err(|e| EcError::WmiConnect(format!("ConnectServer: {}", e)))?
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
            .map_err(|_| EcError::WmiConnect("CoSetProxyBlanket failed".into()))?
        };
        let mut worker = Self {
            services,
            target: None,
            fatal: None,
        };
        // 预探测 MICommonInterface 目标实例：本机没有该接口（如非小米机型）
        // 时在**创建阶段**就返回 WmiInterfaceNotFound，使 create_backend(Wmi)
        // 失败并触发自动回退（WinRing0 或错误提示），而不是创建一个"连接成功
        // 但每次调用都报错"的后端让 GUI 一直显示读取失败。
        worker.resolve_target()?;
        Ok(worker)
    }

    fn run(mut self, rx: mpsc::Receiver<WmiCmd>, tx: mpsc::Sender<WmiReply>) {
        while let Ok(cmd) = rx.recv() {
            let reply = match cmd {
                WmiCmd::Quit => break,
                WmiCmd::GetBatteryState => WmiReply::BatteryState(self.get_battery_state_impl()),
                WmiCmd::SetBatteryCare(en) => WmiReply::Unit(self.set_battery_care_impl(en)),
                WmiCmd::SetChargeLimit(pct) => WmiReply::Unit(self.set_charge_limit_impl(pct)),
                WmiCmd::GetPerfMode => WmiReply::PerfMode(self.get_perf_impl()),
                WmiCmd::SetPerfMode(mode) => WmiReply::Unit(self.set_perf_impl(mode)),
            };
            if tx.send(reply).is_err() {
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
        let enumerator = unsafe {
            self.services
                .ExecQuery(
                    &BSTR::from("WQL"),
                    &BSTR::from("SELECT * FROM MICommonInterface"),
                    WBEM_FLAG_RETURN_IMMEDIATELY | WBEM_FLAG_FORWARD_ONLY,
                    None::<&IWbemContext>,
                )
                .map_err(|e| EcError::WmiConnect(format!("ExecQuery instances: {}", e)))?
        };

        // (instance_name, active, is_mifs)：收集全部实例后按 Meow-Box 的
        // 选择策略挑选：active 且含 MIFS 优先，否则取第一个。
        let mut instances: Vec<(String, bool, bool)> = Vec::new();
        loop {
            let mut objects: [Option<IWbemClassObject>; 1] = [None];
            let mut returned: u32 = 0;
            let hr = unsafe { enumerator.Next(500, &mut objects, &mut returned as *mut u32) };
            if hr.is_err() || returned == 0 {
                break;
            }
            if let Some(ref obj) = objects[0] {
                let name = Self::get_str_prop(obj, "InstanceName").unwrap_or_default();
                let active = Self::get_bool_prop(obj, "Active").unwrap_or(false);
                let is_mifs = name.to_ascii_uppercase().contains("MIFS");
                log::info!(
                    "WMI: MICommonInterface instance '{}' (active={}, mifs={})",
                    name, active, is_mifs
                );
                instances.push((name, active, is_mifs));
            }
        }
        if instances.is_empty() {
            return Err(EcError::WmiInterfaceNotFound);
        }
        let (name, _, _) = instances
            .iter()
            .find(|(_, active, is_mifs)| *active && *is_mifs)
            .or_else(|| instances.first())
            .expect("instances is non-empty");
        let path = format!(
            "MICommonInterface.InstanceName=\"{}\"",
            escape_instance_name(name)
        );
        self.target = Some(path.clone());
        log::info!("WMI: MiInterface target instance -> '{}'", path);
        Ok(path)
    }

    fn get_str_prop(obj: &IWbemClassObject, name: &str) -> Option<String> {
        let mut val = VARIANT::default();
        let mut _type = 0i32;
        let mut _flavor = 0i32;
        let (_wide, prop_name) = crate::util::to_pcwstr(name);
        unsafe {
            obj.Get(prop_name, 0, &mut val, Some(&mut _type as *mut i32), Some(&mut _flavor as *mut i32))
                .ok()?;
        }
        let result = unsafe { crate::ec::wmi_util::bstr_from_variant(&val) };
        unsafe { VariantClear(&mut val).ok() };
        result
    }

    fn get_bool_prop(obj: &IWbemClassObject, name: &str) -> Option<bool> {
        let mut val = VARIANT::default();
        let mut _type = 0i32;
        let mut _flavor = 0i32;
        let (_wide, prop_name) = crate::util::to_pcwstr(name);
        unsafe {
            obj.Get(prop_name, 0, &mut val, Some(&mut _type as *mut i32), Some(&mut _flavor as *mut i32))
                .ok()?;
        }
        let result = unsafe { crate::ec::wmi_util::bool_from_variant(&val) };
        unsafe { VariantClear(&mut val).ok() };
        result
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
        let lbound = SafeArrayGetLBound(sa, 1).unwrap_or(0);
        let ubound = SafeArrayGetUBound(sa, 1).unwrap_or(-1);
        let len = ubound.saturating_sub(lbound).saturating_add(1) as usize;
        if len == 0 {
            let _ = SafeArrayDestroy(sa);
            return fallback.to_string();
        }
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
    unsafe fn mi_interface_call(&mut self, buffer: &[u8; 32]) -> Result<[u8; 32], EcError> {
        // 熔断检查：确定性失败后不再发起任何 WMI 调用，直接返回缓存错误。
        if let Some(err) = &self.fatal {
            log::warn!("WMI: MiInterface latched as failed ({}); failing fast", err);
            return Err(err.clone());
        }

        // 必须在**实例**上调用方法：对类路径调用 ExecMethod 被 WinMgmt
        // 拒绝（0x8004102F，详见 resolve_target）。但方法**签名**定义在
        // 类对象上——对实例对象 GetMethod 返回 WBEM_E_INVALID_OPERATION
        // (0x8004101E)。因此：GetMethod 用类对象，ExecMethod 用实例路径。
        let target = self.resolve_target()?;

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
                self.maybe_latch(Some(hr), EcError::WmiConnect(format!("GetObject: {}", e)))
            })?;
        let class = match class {
            Some(c) => c,
            None => return Err(self.maybe_latch(None, EcError::WmiInterfaceNotFound)),
        };

        let mut in_sig: Option<IWbemClassObject> = None;
        let mut out_sig: Option<IWbemClassObject> = None;
        let (_mn_buf, method_name) = crate::util::to_pcwstr("MiInterface");
        class
            .GetMethod(method_name, 0, &mut in_sig, &mut out_sig)
            .map_err(|e| {
                let hr = e.code().0 as u32;
                self.maybe_latch(Some(hr), EcError::WmiConnect(format!("GetMethod: {}", e)))
            })?;

        let in_sig = match in_sig {
            Some(s) => s,
            None => return Err(self.maybe_latch(None, EcError::WmiInterfaceNotFound)),
        };
        let in_param_name = Self::param_name_from_schema(&in_sig, "InData");
        log::info!("WMI: MiInterface input parameter -> '{}'", in_param_name);

        let in_params = in_sig
            .SpawnInstance(0)
            .map_err(|e| EcError::WmiConnect(format!("SpawnInstance: {}", e)))?;

        let sa = SafeArrayCreateVector(VT_UI1, 0, 32);
        if sa.is_null() {
            return Err(EcError::WmiConnect("SafeArrayCreateVector failed".into()));
        }

        let mut data_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        if SafeArrayAccessData(sa, &mut data_ptr).is_err() {
            SafeArrayDestroy(sa).ok();
            return Err(EcError::WmiConnect("SafeArrayAccessData failed".into()));
        }
        std::ptr::copy_nonoverlapping(buffer.as_ptr(), data_ptr as *mut u8, 32);
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
                Anonymous: core::mem::ManuallyDrop::new(windows::Win32::System::Variant::VARIANT_0_0 {
                    vt: VARENUM(VT_ARRAY.0 | VT_UI1.0),
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: windows::Win32::System::Variant::VARIANT_0_0_0 { parray: sa },
                }),
            },
        });

        let (_in_buf, in_pcwstr) = crate::util::to_pcwstr(&in_param_name);
        if let Err(e) = in_params.Put(in_pcwstr, 0, &*v as *const VARIANT, 0) {
            // Put 失败：数组从未交给提供程序，此处是唯一释放点
            // （v 已 ManuallyDrop，不会二次释放）。
            SafeArrayDestroy(sa).ok();
            return Err(EcError::WmiConnect(format!("Put '{}': {}", in_param_name, e)));
        }
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
            &in_params,
            None,
            Some(&mut call_result as *mut Option<IWbemCallResult>),
        ) {
            // ExecMethod 同步返回错误意味着异步调用**从未启动**——但 Put
            // 已执行，提供程序可能已获得数组引用，因此同样**不释放**输入
            // 数组（与 GetResultObject 失败分支同策略，宁泄漏不崩溃）。
            std::mem::forget(in_params);
            let hr = e.code().0 as u32;
            return Err(self.maybe_latch(Some(hr), EcError::WmiCallHResult(hr)));
        }

        let call_result = match call_result {
            Some(cr) => cr,
            None => {
                // 理论上 ExecMethod(RETURN_IMMEDIATELY) 成功必然返回
                // call result；防御性处理。此时异步调用**已启动**、输入
                // 数组已交给提供程序：不得释放（原因见下方 GetResultObject
                // 失败分支），宁可泄漏也不崩溃。
                std::mem::forget(in_params);
                return Err(EcError::WmiCallFailed(0));
            }
        };

        log::info!("WMI: GetResultObject waiting...");
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
                // 释放；in_params 是 COM 对象，forget 防止自动 Release。）
                std::mem::forget(in_params);
                return Err(self.maybe_latch(Some(hr), EcError::WmiCallHResult(hr)));
            }
        };

        // 异步调用成功完成（GetResultObject 返回）后**同样不得**释放输入
        // 数组。实测（本机 2025 RedmiBook Pro 14，首次真机成功调用）：
        // 提供程序对输入数组的内部引用**存活到连接关闭**——perf read
        // 成功返回后按旧逻辑 drop(in_params)+SafeArrayDestroy(sa) 释放
        // 数组，下一次调用时进程以 STATUS_HEAP_CORRUPTION 崩溃。
        // 与下方失败路径采取相同策略：**永不释放输入数组**（forget
        // in_params 防止自动 Release，不调用 SafeArrayDestroy）。
        // 代价：每次调用泄漏一个约 32 字节的数组，有界且无害；
        // 宁泄漏不崩溃。
        std::mem::forget(in_params);

        let out_param_name = Self::param_name_from_schema(&out_params, "OutData");
        log::info!("WMI: MiInterface output parameter -> '{}'", out_param_name);

        let (_out_buf, out_pcwstr) = crate::util::to_pcwstr(&out_param_name);
        let mut out_val = VARIANT::default();
        let mut out_type = 0i32;
        let mut out_flavor = 0i32;
        if let Err(e) = out_params.Get(
            out_pcwstr,
            0,
            &mut out_val,
            Some(&mut out_type as *mut i32),
            Some(&mut out_flavor as *mut i32),
        ) {
            return Err(EcError::WmiConnect(format!("Get '{}': {}", out_param_name, e)));
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
        let lbound = SafeArrayGetLBound(out_sa, 1).unwrap_or(0);
        let ubound = SafeArrayGetUBound(out_sa, 1).unwrap_or(-1);
        let len = ubound.saturating_sub(lbound).saturating_add(1) as usize;
        if len < MIN_OUTPUT_LEN {
            log::error!("WMI: output array too short ({} bytes)", len);
            VariantClear(&mut out_val).ok();
            return Err(EcError::WmiCallFailed(0));
        }

        let mut out_data: *mut core::ffi::c_void = std::ptr::null_mut();
        if SafeArrayAccessData(out_sa, &mut out_data).is_err() {
            VariantClear(&mut out_val).ok();
            return Err(EcError::WmiConnect("SafeArrayAccessData out failed".into()));
        }

        let mut result = [0u8; 32];
        let copy_len = len.min(32);
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
    fn read_battery(&mut self) -> Result<[u8; 32], EcError> {
        let mut buf = [0u8; 32];
        to_le16(&mut buf, 0, CMD_READ);       // fun1
        to_le16(&mut buf, 2, FUN2_BATTERY);   // fun2
        to_le16(&mut buf, 4, 0x0002);          // fun3 = 子操作(充电读)
        // fun4 保持 0x00000000
        unsafe { self.mi_interface_call(&buf) }
    }

    /// Build a write command buffer for battery.
    /// Layout: fun1=0xFB00, fun2=0x1000, fun3=0x0002, fun4=raw_code
    /// Per F-HAL-07: 充电写 fun3=0x0002, fun4=充电上限 raw code
    fn write_battery(&mut self, raw_code: u8) -> Result<(), EcError> {
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
        unsafe { self.mi_interface_call(&buf)? };
        Ok(())
    }

    fn read_perf(&mut self) -> Result<[u8; 32], EcError> {
        let mut buf = [0u8; 32];
        to_le16(&mut buf, 0, CMD_READ);       // fun1
        to_le16(&mut buf, 2, FUN2_PERF);      // fun2
        to_le16(&mut buf, 4, 0x0000);          // fun3 = 子操作(性能读=0x0000)
        // fun4 保持 0x00000000
        unsafe { self.mi_interface_call(&buf) }
    }

    /// Build a write command buffer for performance mode.
    /// Layout: fun1=0xFB00, fun2=0x0800, fun3=mode, fun4=0
    /// Per F-HAL-07: 性能写 fun3=模式 raw code, fun4=0
    fn write_perf(&mut self, mode: u8) -> Result<(), EcError> {
        let mut buf = [0u8; 32];
        to_le16(&mut buf, 0, CMD_WRITE);      // fun1
        to_le16(&mut buf, 2, FUN2_PERF);      // fun2
        to_le16(&mut buf, 4, mode as u16);     // fun3 = 参数(模式 raw code)
        // fun4 保持 0x00000000
        unsafe { self.mi_interface_call(&buf)? };
        Ok(())
    }

    fn get_battery_state_impl(&mut self) -> Result<(bool, u8), EcError> {
        // B-WMI-1: 养护位与上限来自同一条读命令的同一响应字段（Data1），
        // 一次往返同时返回两者；默认实现会发起两次相同的 WMI 往返。
        let buf = self.read_battery()?;
        let raw = buf[6]; // Data1 = 充电上限 raw code
        let percent = battery::wmi_rawcode_to_percent(raw).unwrap_or(100);
        log::info!("WMI: battery state -> care {}, limit {}%", percent < 100, percent);
        Ok((percent < 100, percent))
    }

    fn set_battery_care_impl(&mut self, enabled: bool) -> Result<(), EcError> {
        // B-WMI-3: WMI 没有独立的电池养护位（养护 = 充电上限 < 100%，见
        // get_battery_state 的推导），因此这里是契约性 no-op——全部调用方
        // （GUI 切换、启动应用、电源重设）都已先显式 set_charge_limit 设置
        // 上限。曾在 !enabled 时重复 set_charge_limit(100)，与调用方刚写过
        // 的 100% 完全相同，每次关闭养护浪费一次完整 WMI 往返。
        log::info!(
            "WMI: set battery care -> {} (no-op; derived from charge limit)",
            if enabled { "enabled" } else { "disabled" }
        );
        Ok(())
    }

    fn set_charge_limit_impl(&mut self, percent: u8) -> Result<(), EcError> {
        let percent = percent.min(100);
        let raw = wmi_rawcode_for_percent(percent);
        log::info!("WMI: set charge limit -> {}% (raw {:#x})", percent, raw);
        self.write_battery(raw)
    }

    fn get_perf_impl(&mut self) -> Result<u8, EcError> {
        let buf = self.read_perf()?;
        log::info!("WMI: read perf mode -> {:#x}", buf[4]);
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
    tx: mpsc::Sender<WmiCmd>,
    res: Mutex<mpsc::Receiver<WmiReply>>,
}

// `WmiBackend` 不含任何 COM 指针——所有 COM 对象归 worker 线程独占；
// 共享状态仅 mpsc 通道（`Sender<WmiCmd>` 为 Send+Sync）与
// `Mutex<Receiver<WmiReply>>`（`Receiver` 为 Send，`Mutex` 使其满足 Sync），
// 因此 `Send + Sync` 由字段自动推导，无需 unsafe 实现（历史 unsafe impl
// 是冗余的——见下方 test_wmi_backend_is_send_sync 的编译期断言）。

impl Drop for WmiBackend {
    fn drop(&mut self) {
        let _ = self.tx.send(WmiCmd::Quit);
    }
}

impl WmiBackend {
    pub fn new() -> Result<Self, EcError> {
        let (tx, rx) = mpsc::channel::<WmiCmd>();
        let (res_tx, res_rx) = mpsc::channel::<WmiReply>();
        std::thread::Builder::new()
            .name("wmi-worker".into())
            .spawn(move || {
                match WmiWorker::connect() {
                    Ok(worker) => {
                        let _ = res_tx.send(WmiReply::Unit(Ok(())));
                        worker.run(rx, res_tx);
                    }
                    Err(e) => {
                        let _ = res_tx.send(WmiReply::Unit(Err(e)));
                    }
                }
            })
            .map_err(|e| EcError::WmiConnect(format!("spawn worker thread: {}", e)))?;
        match res_rx.recv() {
            Ok(WmiReply::Unit(Ok(()))) => Ok(Self {
                tx,
                res: Mutex::new(res_rx),
            }),
            Ok(WmiReply::Unit(Err(e))) => Err(e),
            _ => Err(EcError::WmiConnect("WMI worker handshake failed".into())),
        }
    }

    fn call(&self, cmd: WmiCmd) -> WmiReply {
        // 命令发送与应答接收必须在同一把锁内串行完成：应答通道是单一的，
        // 若只锁 recv，两个并发调用线程的发送可能交错（A/B 先后 send，
        // worker 按命令 FIFO 产生应答），而锁只保护接收端——B 抢到锁后
        // 可能收到 A 的命令的应答，错误被静默错配到另一个调用方
        // （WmiReply 不带请求关联标识）。当前 GUI 是唯一后端调用方，
        // 该问题潜伏；把锁覆盖 send+recv 即从架构上根除错配可能。
        let guard = self.res.lock().unwrap_or_else(|e| e.into_inner());
        if self.tx.send(cmd).is_err() {
            return WmiReply::Unit(Err(EcError::BackendUnavailable(
                "WMI worker 已退出".into(),
            )));
        }
        guard
            .recv()
            .unwrap_or(WmiReply::Unit(Err(EcError::BackendUnavailable(
                "WMI worker 无响应".into(),
            ))))
    }
}

impl EcBackend for WmiBackend {
    fn name(&self) -> &'static str {
        "WMI (MICommonInterface)"
    }

    fn preference(&self) -> super::config::BackendPreference {
        super::config::BackendPreference::Wmi
    }

    fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
        match self.call(WmiCmd::GetBatteryState) {
            WmiReply::BatteryState(r) => r.map(|(care, _)| care),
            _ => Err(EcError::BackendUnavailable("WMI worker 响应异常".into())),
        }
    }

    fn get_charge_limit(&self) -> Result<u8, EcError> {
        match self.call(WmiCmd::GetBatteryState) {
            WmiReply::BatteryState(r) => r.map(|(_, limit)| limit),
            _ => Err(EcError::BackendUnavailable("WMI worker 响应异常".into())),
        }
    }

    fn get_battery_state(&self) -> Result<(bool, u8), EcError> {
        match self.call(WmiCmd::GetBatteryState) {
            WmiReply::BatteryState(r) => r,
            _ => Err(EcError::BackendUnavailable("WMI worker 响应异常".into())),
        }
    }

    fn set_battery_care(&self, enabled: bool) -> Result<(), EcError> {
        match self.call(WmiCmd::SetBatteryCare(enabled)) {
            WmiReply::Unit(r) => r,
            _ => Err(EcError::BackendUnavailable("WMI worker 响应异常".into())),
        }
    }

    fn set_charge_limit(&self, percent: u8) -> Result<(), EcError> {
        match self.call(WmiCmd::SetChargeLimit(percent)) {
            WmiReply::Unit(r) => r,
            _ => Err(EcError::BackendUnavailable("WMI worker 响应异常".into())),
        }
    }

    fn get_performance_mode(&self) -> Result<u8, EcError> {
        match self.call(WmiCmd::GetPerfMode) {
            WmiReply::PerfMode(r) => r,
            _ => Err(EcError::BackendUnavailable("WMI worker 响应异常".into())),
        }
    }

    fn set_performance_mode(&self, mode: u8) -> Result<(), EcError> {
        match self.call(WmiCmd::SetPerfMode(mode)) {
            WmiReply::Unit(r) => r,
            _ => Err(EcError::BackendUnavailable("WMI worker 响应异常".into())),
        }
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
        assert!(is_latching_hresult(WBEM_E_INVALID_METHOD_PARAMETERS.0 as u32));
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
        let returned = latch_into(&mut state, Some(WBEM_E_PROVIDER_FAILURE.0 as u32), first.clone());
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
        let _ = latch_into(&mut state, Some(0x80004005), EcError::WmiCallHResult(0x80004005));
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

    #[test]
    fn test_wmi_rawcode_for_percent_exact() {
        assert_eq!(wmi_rawcode_for_percent(100), 0);
        assert_eq!(wmi_rawcode_for_percent(80), 1);
        assert_eq!(wmi_rawcode_for_percent(90), 4);
        assert_eq!(wmi_rawcode_for_percent(70), 5);
        assert_eq!(wmi_rawcode_for_percent(60), 6);
        assert_eq!(wmi_rawcode_for_percent(50), 7);
        assert_eq!(wmi_rawcode_for_percent(40), 8);
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

        let (tx, rx) = mpsc::channel::<WmiCmd>();
        let (res_tx, res_rx) = mpsc::channel::<WmiReply>();
        let backend = Arc::new(WmiBackend {
            tx,
            res: Mutex::new(res_rx),
        });

        // 仿真 worker：按命令 FIFO 应答，每次应答前微眠放大并发窗口。
        std::thread::spawn(move || {
            while let Ok(cmd) = rx.recv() {
                let reply = match cmd {
                    WmiCmd::GetPerfMode => WmiReply::PerfMode(Ok(0x99)),
                    WmiCmd::GetBatteryState => WmiReply::BatteryState(Ok((true, 0x55))),
                    _ => {
                        WmiReply::Unit(Err(EcError::BackendUnavailable("unexpected".into())))
                    }
                };
                std::thread::sleep(std::time::Duration::from_micros(150));
                if res_tx.send(reply).is_err() {
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

        assert!(got_a.iter().all(|&m| m == 0x99), "A got wrong replies: {:?}", got_a);
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

    #[test]
    fn test_wmi_rawcode_for_percent_nearest() {
        assert_eq!(wmi_rawcode_for_percent(85), 1); // 80%（与最近预设一致）
        assert_eq!(wmi_rawcode_for_percent(55), 6); // 60%
        assert_eq!(wmi_rawcode_for_percent(95), 0); // 100%
        assert_eq!(wmi_rawcode_for_percent(45), 7); // 50%
    }
}
