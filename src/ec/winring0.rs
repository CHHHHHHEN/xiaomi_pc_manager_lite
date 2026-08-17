use std::sync::Mutex;

use super::addr as ec_addr;
use super::backend::EcBackend;
use super::error::EcError;
use libloading::Library;

use windows::core::PCWSTR;
use windows::Win32::Foundation::GetLastError;
use windows::Win32::System::Services::*;

type ReadPort = unsafe extern "system" fn(u16) -> u8;
type WritePort = unsafe extern "system" fn(u16, u8);

/// 轮询 EC 命令/数据端口直到满足 `(port & mask) == expected`，或超时。
///
/// 读/写两步共用同一轮询节奏：前 100 次忙自旋（端口通常几十次内就绪），
/// 之后转入 1ms 睡眠等待——自旋避免不必要的上下文切换，睡眠避免烧 CPU。
/// 超时上限约 1 秒，返回 `Timeout`。
///
/// `step` 是当前等待所在的协议阶段（如 "OBF 就绪(读数据)"），超时日志会
/// 带上它、实测端口值与耗时——"EC 操作超时 (地址: 0x66)" 这类笼统报错无法
/// 区分是**哪一步**卡住（命令/地址/数据端口被占、IBF 未清、OBF 未置），
/// 排查粒度不足（用户反馈"读取性能模式: EC 操作超时"仍偶发）。
fn ec_wait_status(rp: ReadPort, step: &'static str, mask: u8, expected: u8) -> Result<(), EcError> {
    let start = std::time::Instant::now();
    for i in 0..1000 {
        let status = unsafe { rp(ec_addr::EC_CMD) };
        if status & mask == expected {
            return Ok(());
        }
        if i < 100 {
            core::hint::spin_loop();
        } else {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    // 超时说明 EC 忙/无响应（驱动加载异常、EC 固件卡死、端口被其它程序占用）。
    // 只返回 Err 会让上层只看到一句笼统的"超时"；这里记下具体等待条件、
    // 实测端口值与耗时，定位"偶发超时"时能确认卡在协议的哪一步。
    log::warn!(
        "EC wait status timed out: step={} mask={:#x} expected={:#x} observed={:#x} elapsed={}ms",
        step,
        mask,
        expected,
        unsafe { rp(ec_addr::EC_CMD) },
        start.elapsed().as_millis()
    );
    Err(EcError::Timeout(ec_addr::EC_CMD))
}

/// `ec_wait_status` 的具名别名：调用点传语义化步骤名（如 "读:数据 OBF 就绪"），
/// 超时日志据此定位卡在协议哪一步。内部直接转发。
fn step_wait(rp: ReadPort, step: &'static str, mask: u8, expected: u8) -> Result<(), EcError> {
    ec_wait_status(rp, step, mask, expected)
}

/// EC 操作偶发超时的**一次**瞬态重试。
///
/// EC 处于忙/被其它程序占用时，单次约 1s 等待可能恰好落在忙窗口内（实测
/// 偶发 EC 操作超时，地址 0x66）。重试一次（再等待 ~1s）绝大多数瞬态即
/// 恢复；持续两次超时才是真故障，按原错误如实上报，不掩盖问题。仅重试
/// 一次避免 GUI 长时间阻塞（读/写各自至多 ~2s）。
fn retry_transient<T>(
    what: &str,
    addr: u16,
    mut op: impl FnMut() -> Result<T, EcError>,
) -> Result<T, EcError> {
    match op() {
        Ok(v) => Ok(v),
        Err(e) => {
            log::warn!(
                "WinRing0: {} (0x{:02x}) transient {}; retrying once",
                what,
                addr,
                e
            );
            match op() {
                Ok(v) => Ok(v),
                Err(e2) => {
                    log::error!(
                        "WinRing0: {} (0x{:02x}) failed on retry: {}",
                        what,
                        addr,
                        e2
                    );
                    Err(e2)
                }
            }
        }
    }
}

/// 本进程当前存活的 WinRing0 后端实例数（驱动已由成功创建的实例加载）。
///
/// cleanup_service 的语义是清理**跨进程残留**的陈旧驱动服务（上次运行崩溃/
/// 未正常卸载遗留）。当本进程已有一个存活的后端在用它时，删除该服务会同时
/// 拆掉正在使用的驱动：随后的 InitializeOls 一旦失败（Defender 锁文件、
/// 服务清理时序等已知瞬态），现有后端立刻失效、所有端口读写报错，直到重启。
/// 因此仅在无存活实例时执行清理。
static WINRING0_INSTANCES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub struct WinRing0Backend {
    rp: ReadPort,
    wp: WritePort,
    lib: Library,
    lock: Mutex<()>,
}

impl Drop for WinRing0Backend {
    fn drop(&mut self) {
        // 卸载驱动：后端被销毁（切换后端/进程退出）时触发。记录该事件，
        // "驱动加载/卸载状态异常"类问题可从日志确认 DeinitializeOls 是否
        // 被执行过。
        log::info!("WinRing0: deinitializing driver (DeinitializeOls)");
        if let Ok(deinit) = unsafe { self.lib.get(b"DeinitializeOls") } {
            let deinit: unsafe extern "system" fn() = *deinit;
            unsafe { deinit() };
        }
        WINRING0_INSTANCES.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn dll_name() -> &'static str {
    arch_file_names().0
}

/// 当前架构下的 WinRing0 驱动文件名对 (DLL, SYS)。
///
/// `embed.rs` 的提取路径与 WinRing0 的加载路径各自硬编码过同一组文件名，
/// 存在漂移风险——统一收敛到此处作为唯一事实来源。
pub fn arch_file_names() -> (&'static str, &'static str) {
    if cfg!(target_pointer_width = "64") {
        ("WinRing0x64.dll", "WinRing0x64.sys")
    } else {
        ("WinRing0.dll", "WinRing0.sys")
    }
}

fn try_load(dll_path: &str) -> Result<(Library, ReadPort, WritePort), EcError> {
    let lib = match unsafe { Library::new(dll_path) } {
        Ok(l) => l,
        Err(e) => {
            log::warn!("WinRing0: Library::new({}) failed: {}", dll_path, e);
            return Err(EcError::DllLoad(e.to_string()));
        }
    };

    log::info!("WinRing0: loaded DLL from {}", dll_path);

    // InitializeOls internally calls GetModuleFileName(NULL) to get the
    // EXE path, then looks for the .sys file in the EXE directory.
    // Copy the .sys alongside the EXE so it can be found.
    ensure_sys_in_exe_dir(dll_path);

    // Clean up any stale service from a previous run so that
    // InitializeOls's internal ManageDriver can create a fresh one.
    // 注意：仅当本进程**没有存活**的 WinRing0 后端时才清理——若当前活动后端
    // 正是 WinRing0，这里停/删的是它正在使用的驱动服务；随后 InitializeOls
    // 一旦失败（重试也失败），活动后端立即失效（端口读写全部报错，直到重启）。
    if WINRING0_INSTANCES.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        cleanup_service();
    } else {
        log::info!("WinRing0: live backend in this process; skipping service cleanup");
    }

    let init: unsafe extern "system" fn() -> i32 =
        *unsafe { lib.get(b"InitializeOls") }.map_err(|e| EcError::DllLoad(e.to_string()))?;

    // **符号必须在 InitializeOls 之前全部解析。** InitializeOls 成功会加载驱动，
    // 而驱动一旦加载，本进程就必须靠 DeinitializeOls 卸载（只能由 WinRing0Backend
    // 的 Drop 触发）。若在此之后再出现任何可失败的步骤（如缺失
    // ReadIoPortByte/WriteIoPortByte 符号）返回 Err，会留下三种互相耦合的坏状态：
    //   1. 驱动保持加载、从未 DeinitializeOls（本函数没有 WinRing0Backend 可 drop）；
    //   2. WINRING0_INSTANCES 已递增、无人递减，cleanup_service 在剩余进程生命周期
    //      内被永久跳过——后续每次 WinRing0Backend::new() 都不会清理陈旧服务；
    //   3. 第二次 try_load 对已加载的驱动再调 InitializeOls 会因服务名冲突失败，
    //      把"一次符号缺失"放大成"后端永久不可用"。
    // 把全部符号解析提前到 init 之前后，init 之后不再有任何可失败步骤，计数器
    // 的递增与后端的成功构造一一对应。
    let rp: ReadPort =
        *unsafe { lib.get(b"ReadIoPortByte") }.map_err(|e| EcError::DllLoad(e.to_string()))?;

    let wp: WritePort =
        *unsafe { lib.get(b"WriteIoPortByte") }.map_err(|e| EcError::DllLoad(e.to_string()))?;

    // Let InitializeOls handle driver installation (like the C version).
    // 失败重试：驱动的安装/加载可能因时序问题首次失败（例如刚解压的文件
    // 被 Defender 实时扫描锁定、服务清理尚未完成、驱动卸载未结束），
    // 稍作延时重试即可成功——历史实现只尝试一次，导致"首次 InitializeOls
    // 显示失败、反复切换多次后才成功"。
    let mut init_error = 0u32;
    let mut init_ok = false;
    for attempt in 0..3 {
        log::info!(
            "WinRing0: calling InitializeOls (attempt {})...",
            attempt + 1
        );
        if unsafe { init() } != 0 {
            init_ok = true;
            break;
        }
        init_error = unsafe { GetLastError().0 };
        log::warn!(
            "WinRing0: InitializeOls returned 0 (attempt {}, last_error={:#x}); retrying",
            attempt + 1,
            init_error
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if !init_ok {
        return Err(EcError::InitFailed(format!("错误码: {:#x}", init_error)));
    }
    log::info!("WinRing0: InitializeOls succeeded");
    WINRING0_INSTANCES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    Ok((lib, rp, wp))
}

/// Remove any stale WinRing0 service from previous runs.
///
/// `DeleteService` 是**异步**的：服务停止后驱动卸载需要时间，句柄/服务记录
/// 短暂残留。若立即调用 InitializeOls 重建**同名**服务，会因名称冲突失败
/// （CreateService/StartService 报错）——这正是"首次切换 WinRing0 失败、
/// 反复切换多次才成功"的根因。因此删除后必须**轮询等待服务真正消失**。
fn cleanup_service() {
    unsafe {
        let scm = match OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS) {
            Ok(h) => h,
            Err(_) => {
                // 无管理员权限/服务控制管理器不可达：清理跳过。try_load 的
                // 调用方在 InitializeOls 失败时会有自己的告警，这里只记录
                // debug 避免重复刷屏。
                log::debug!("WinRing0: OpenSCManager failed; skipping stale service cleanup");
                return;
            }
        };
        let id = crate::util::WideString::new("WinRing0_1_2_0");
        if let Ok(svc) = OpenServiceW(scm, id.as_pcwstr(), SERVICE_ALL_ACCESS) {
            log::info!("WinRing0: removing stale service WinRing0_1_2_0");
            let _ = ControlService(svc, SERVICE_CONTROL_STOP, std::ptr::null_mut());
            let _ = DeleteService(svc);
            let _ = CloseServiceHandle(svc);
            // 最多等待 3 秒：服务从 SCM 数据库中消失即认为清理完成。
            let mut gone = false;
            for _ in 0..30 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                match OpenServiceW(scm, id.as_pcwstr(), SERVICE_ALL_ACCESS) {
                    Ok(h) => {
                        let _ = CloseServiceHandle(h);
                    }
                    Err(_) => {
                        gone = true;
                        break;
                    }
                }
            }
            if !gone {
                // 服务删除后 3s 仍未消失：SCM 记录残留，后续 InitializeOls
                // 重建同名服务可能因此失败——记录告警，让"首次切换 WinRing0
                // 失败/反复失败"类问题能在日志中直接看到根因提示。
                log::warn!(
                    "WinRing0: stale service did not disappear within 3s; \
                     InitializeOls may fail on name conflict"
                );
            }
        } else {
            log::debug!("WinRing0: no stale WinRing0_1_2_0 service to clean");
        }
        let _ = CloseServiceHandle(scm);
    }
}

/// Copy the .sys file to the EXE directory so that InitializeOls's internal
/// Initialize() can find it (it uses GetModuleFileName(NULL) which returns
/// the EXE path, then looks for .sys in the EXE directory).
fn ensure_sys_in_exe_dir(dll_path: &str) {
    let dll = std::path::Path::new(dll_path);
    let sys_name = dll
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .map(|n| n.to_lowercase().replace(".dll", ".sys"))
        .unwrap_or_else(|| dll_name().replace(".dll", ".sys"));

    let sys_src = dll.with_file_name(&sys_name);
    if !sys_src.exists() {
        log::warn!("WinRing0: .sys not found at {:?}", sys_src);
        return;
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let sys_dst = exe_dir.join(&sys_name);
            if sys_dst.exists() && sys_dst == sys_src {
                return;
            }
            match std::fs::copy(&sys_src, &sys_dst) {
                Ok(_) => log::info!("WinRing0: copied .sys to {:?}", sys_dst),
                Err(e) => log::warn!("WinRing0: copy .sys to EXE dir: {}", e),
            }
        }
    }
}

fn try_load_all(dll_path: &str) -> Result<WinRing0Backend, EcError> {
    try_load(dll_path).map(|(lib, rp, wp)| WinRing0Backend {
        rp,
        wp,
        lib,
        lock: Mutex::new(()),
    })
}

impl WinRing0Backend {
    pub fn new() -> Result<Self, EcError> {
        let name = dll_name();

        // 安全要求：本进程是提权（管理员）进程，**绝不能用裸模块名加载**。
        // 裸名会走 Windows 标准 DLL 搜索顺序（exe 目录 → System32 → CWD →
        // PATH），任何位于 CWD 或 System32 中同名 DLL 都会在提权上下文内被
        // 加载——攻击者只需在启动目录放一个恶意的 WinRing0x64.dll 即可提权。
        // 因此全部改为**绝对路径**加载：优先 EXE 同级目录，否则提取嵌入式
        // 副本到同一目录再加载。
        let exe_dir = std::env::current_exe()
            .map_err(|e| EcError::DllLoad(format!("current_exe: {}", e)))?
            .parent()
            .ok_or_else(|| EcError::DllLoad("executable has no parent directory".into()))?
            .to_path_buf();

        // 兼容性提示：历史版本支持用户把自定义 DLL 放到当前工作目录。
        // 出于安全考虑已不再加载该路径，检测到存在时给出明确日志以免
        // 用户困惑"为什么我的驱动没生效"。
        if std::path::Path::new(name).exists() {
            log::warn!(
                "WinRing0: ignoring '{}' in the current working directory: \
                 loading DLLs by bare name from CWD is disabled for security (caller must use the EXE directory)",
                name
            );
        }

        // 1. Try alongside the EXE (absolute path)
        // 失败原因暂存（如 DLL 缺失 / InitializeOls 失败 / 驱动加载被拒绝）：
        // 步骤 2 也失败时把最具体的错误带进最终返回，避免笼统的
        // "not found" 掩盖真实根因（如 DLL 在但驱动初始化失败）。
        let exe_dll = exe_dir.join(name);
        match try_load_all(&exe_dll.to_string_lossy()) {
            Ok(backend) => return Ok(backend),
            Err(e) => log::warn!("WinRing0: load from EXE dir failed: {}", e),
        }

        // 2. Fall back to extracting the embedded binaries into the EXE
        //    directory and loading that copy (initialize behind it, so it
        //    finds the freshly written .sys next to it).
        let extracted_path = match crate::embed::extract_winring0() {
            Ok(p) => p,
            Err(e) => return Err(EcError::DllLoad(format!("{} 提取失败: {}", name, e))),
        };
        let path_str = extracted_path.to_string_lossy().to_string();
        try_load_all(&path_str)
    }
}

/// 把 u16 寄存器地址安全截断为端口写入的 u8。
///
/// EC 寄存器地址域是 8 位（本机常量 0x68/0xA4/0xA7 均 < 0x100）；`addr as u8`
/// 对 ≥0x100 的值会**静默回绕**成另一个寄存器（如 0x168 → 0x68），读错数据
/// 且无任何报错（R2 回归）。防御：显式拒绝越界地址，绝不静默写错寄存器。
fn ec_addr_u8(addr: u16) -> Result<u8, EcError> {
    u8::try_from(addr)
        .map_err(|_| EcError::InvalidData(format!("EC 寄存器地址 0x{:04x} 超出 8 位范围", addr)))
}

impl WinRing0Backend {
    /// 低层 EC 寄存器访问（端口 I/O）。仅本后端内部使用——WMI 后端没有
    /// 寄存器语义，通过 MiInterface 协议映射反而是误导，故不放在 trait 上。
    fn read_byte(&self, addr: u16) -> Result<u8, EcError> {
        // 偶发超时的瞬态重试（见 retry_transient 注释）。
        retry_transient("read_byte", addr, || self.read_byte_once(addr))
    }

    fn read_byte_once(&self, addr: u16) -> Result<u8, EcError> {
        let addr = ec_addr_u8(addr)?;
        let _guard = crate::util::lock_or_recover(&self.lock, "WinRing0");
        // IBF（bit1）空闲后才能写命令/地址/数据；OBF（bit0）就绪后才能读数。
        // 各等待步骤带语义名，超时时能区分卡在哪一步（见 step_wait 注释）。
        step_wait(self.rp, "读:命令前 IBF 清", 0x02, 0)?;
        unsafe { (self.wp)(ec_addr::EC_CMD, 0x80) };
        step_wait(self.rp, "读:地址前 IBF 清", 0x02, 0)?;
        unsafe { (self.wp)(ec_addr::EC_DATA, addr) };
        // 最终 OBF 等待失败后，EC 可能刚好在超时瞬间置 OBF——数据已就绪在
        // 数据端口。此时返回 Err 但**不读走**该字节，残留值会使下一次
        // `read_byte` 的 OBF 等待立即命中、返回陈旧数据（R1 回归）。
        // 超时路径把数据端口清掉（read 会清 OBF），让后续读取从干净状态开始。
        step_wait(self.rp, "读:数据 OBF 就绪", 0x01, 0x01).inspect_err(|_| {
            let stale = unsafe { (self.rp)(ec_addr::EC_DATA) };
            log::warn!(
                "WinRing0: OBF timeout after address 0x{:02x}; drained stale data 0x{:02x}",
                addr,
                stale
            );
        })?;
        Ok(unsafe { (self.rp)(ec_addr::EC_DATA) })
    }

    fn write_byte(&self, addr: u16, value: u8) -> Result<(), EcError> {
        // 与 read_byte 相同的瞬态重试策略：EC 偶发忙导致写序列超时时重试一次。
        retry_transient("write_byte", addr, || self.write_byte_once(addr, value))
    }

    fn write_byte_once(&self, addr: u16, value: u8) -> Result<(), EcError> {
        let addr = ec_addr_u8(addr)?;
        let _guard = crate::util::lock_or_recover(&self.lock, "WinRing0");
        step_wait(self.rp, "写:命令前 IBF 清", 0x02, 0)?;
        unsafe { (self.wp)(ec_addr::EC_CMD, 0x81) };
        step_wait(self.rp, "写:地址前 IBF 清", 0x02, 0)?;
        unsafe { (self.wp)(ec_addr::EC_DATA, addr) };
        step_wait(self.rp, "写:值前 IBF 清", 0x02, 0)?;
        unsafe { (self.wp)(ec_addr::EC_DATA, value) };
        step_wait(self.rp, "写:完成 IBF 清", 0x02, 0)?;
        Ok(())
    }
}

impl EcBackend for WinRing0Backend {
    fn name(&self) -> &'static str {
        "WinRing0 (I/O Port)"
    }

    fn preference(&self) -> super::config::BackendPreference {
        super::config::BackendPreference::WinRing0
    }

    // ── High-level battery ──

    fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
        // Derive from charge limit — EC may auto-sync BATTERY_CARE from
        // CHARGE_LIMIT on real hardware, so reading 0xA4 directly is unreliable.
        let limit = self.get_charge_limit()?;
        log::info!(
            "WinRing0: battery care enabled by charge limit -> {}%",
            limit
        );
        Ok(super::battery::care_enabled_from_limit(limit))
    }

    fn get_charge_limit(&self) -> Result<u8, EcError> {
        let raw = self.read_byte(ec_addr::CHARGE_LIMIT)?;
        // 寄存器直读可能返回损坏/未初始化的值：0xFF（255）或 0x00（未写入）
        // 都是垃圾值。历史实现把垃圾值**钳到 100 并返回 Ok(100)**——副作用
        // 是写入后回读把"回读失败"伪装成"成功写了 100%"：用户设置 60% 养护时，
        // 一次垃圾回读会让 GUI 按 care=false 持久化，下次启动强制写 100%，
        // 用户设置被静默摧毁（见 battery::apply_battery_state 的读回失败分支）。
        // 合法语义下 0 不可能出现：GUI 滑块下限 40，WMI 预设下限 40，配置
        // 消毒也会把 0 归一化为 80。因此非法值直接返回错误，由调用方决定
        // 处理（刷新展示错误、写后回读走"保留写入值"的兜底），绝不冒充有效
        // 的 100% 状态。
        if raw == 0 || raw > 100 {
            return Err(EcError::InvalidData(format!(
                "充电上限寄存器值 0x{:02x} 非法",
                raw
            )));
        }
        log::info!("WinRing0: read charge limit -> {}%", raw);
        Ok(raw)
    }

    fn set_battery_care(&self, enabled: bool) -> Result<(), EcError> {
        let val = if enabled { 0x01 } else { 0x00 };
        log::info!("WinRing0: set battery care -> {:#x}", val);
        self.write_byte(ec_addr::BATTERY_CARE, val)
    }

    fn set_charge_limit(&self, percent: u8) -> Result<(), EcError> {
        // 写入前统一校验（0 拒绝 / >100 钳制），见 battery::validate_charge_limit_write。
        let pct = super::battery::validate_charge_limit_write(percent)?;
        log::info!("WinRing0: set charge limit -> {}%", pct);
        self.write_byte(ec_addr::CHARGE_LIMIT, pct)
    }

    fn get_battery_state(&self) -> Result<(bool, u8), EcError> {
        // 单次端口读：养护位由限值推导（见 get_battery_care_enabled），
        // 避免默认实现再读一次限值（B-WMI-1）。
        let limit = self.get_charge_limit()?;
        Ok((super::battery::care_enabled_from_limit(limit), limit))
    }

    // ── High-level performance mode ──

    fn get_performance_mode(&self) -> Result<u8, EcError> {
        let mode = self.read_byte(ec_addr::PERF_MODE)?;
        log::info!("WinRing0: read perf mode -> {:#x}", mode);
        Ok(mode)
    }

    fn set_performance_mode(&self, mode: u8) -> Result<(), EcError> {
        log::info!("WinRing0: set perf mode -> {:#x}", mode);
        self.write_byte(ec_addr::PERF_MODE, mode)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// 静态模拟 EC 端口读（thread_local 变量经 raw fn 无法访问，用
    /// static AtomicU8 模拟"当前端口值"）。
    static MOCK_EC_STATUS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
    /// 串行化共享静态的测试：并行用例会互相覆盖 MOCK_EC_STATUS 导致
    /// 结果不确定（一个用例写 0 另一个写 2，读侧可能读到对方的值）。
    static TEST_SERIALIZER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    unsafe extern "system" fn mock_read(_port: u16) -> u8 {
        MOCK_EC_STATUS.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 端口立即可用时（掩码匹配）应无超时返回。
    #[test]
    fn test_ec_wait_status_immediate_success() {
        let _serial = TEST_SERIALIZER.lock().unwrap_or_else(|e| e.into_inner());
        MOCK_EC_STATUS.store(0x02, std::sync::atomic::Ordering::SeqCst);
        let rp: ReadPort = mock_read;
        let result = ec_wait_status(rp, "测试", 0x02, 0x02);
        assert!(result.is_ok(), "mask-matched status must succeed");
    }

    /// 端口状态为 0（OBF/IBF 均空闲）时等待 mask=0x02/expected=0x02
    /// 必须超时（EC 无响应场景）。
    #[test]
    fn test_ec_wait_status_timeout_when_busy() {
        let _serial = TEST_SERIALIZER.lock().unwrap_or_else(|e| e.into_inner());
        MOCK_EC_STATUS.store(0x00, std::sync::atomic::Ordering::SeqCst);
        let rp: ReadPort = mock_read;
        let result = ec_wait_status(rp, "测试", 0x02, 0x02);
        assert!(
            matches!(result, Err(EcError::Timeout(_))),
            "never-ready EC status must time out"
        );
    }

    /// 目标位以外的其它位不应影响匹配（掩码只比较关心位）。
    #[test]
    fn test_ec_wait_status_masked_compare() {
        let _serial = TEST_SERIALIZER.lock().unwrap_or_else(|e| e.into_inner());
        MOCK_EC_STATUS.store(0x03, std::sync::atomic::Ordering::SeqCst);
        let rp: ReadPort = mock_read;
        // mask=0x02, expected=0x02：0x03 & 0x02 == 0x02 命中。
        assert!(ec_wait_status(rp, "测试", 0x02, 0x02).is_ok());
        // mask=0x04, expected=0x04：0x03 & 0x04 == 0 ≠ 0x04 → 超时。
        assert!(matches!(
            ec_wait_status(rp, "测试", 0x04, 0x04),
            Err(EcError::Timeout(_))
        ));
    }

    /// 架构文件名对必须与进程位数一致且非空（嵌入提取依赖该名字）。
    #[test]
    fn test_arch_file_names_consistent() {
        let (dll, sys) = arch_file_names();
        assert!(!dll.is_empty() && !sys.is_empty());
        if cfg!(target_pointer_width = "64") {
            assert_eq!(dll, "WinRing0x64.dll");
            assert_eq!(sys, "WinRing0x64.sys");
        } else {
            assert_eq!(dll, "WinRing0.dll");
            assert_eq!(sys, "WinRing0.sys");
        }
    }

    /// 瞬态重试语义：首次失败（偶发 EC 忙）后重试成功应返回成功，
    /// 且重试仍失败时如实上报最后一次错误（不掩盖真故障）。
    #[test]
    fn test_retry_transient_recovers_or_reports() {
        // 首次失败、重试成功。
        let mut calls = 0;
        let ok = retry_transient("test", 0x68, || {
            calls += 1;
            if calls == 1 {
                Err(EcError::Timeout(0x66))
            } else {
                Ok(42u8)
            }
        });
        assert_eq!(ok.unwrap(), 42);
        assert_eq!(calls, 2, "must retry exactly once after transient failure");

        // 两次都失败 → 返回真实错误（最后那次）。
        let mut calls = 0;
        let err: Result<u8, EcError> = retry_transient("test", 0x68, || {
            calls += 1;
            Err(EcError::Timeout(0x66))
        });
        assert!(matches!(err, Err(EcError::Timeout(0x66))));
        assert_eq!(calls, 2, "must not retry endlessly");

        // 首次即成功 → 不重试。
        let mut calls = 0;
        let ok = retry_transient("test", 0x68, || {
            calls += 1;
            Ok(1u8)
        });
        assert_eq!(ok.unwrap(), 1);
        assert_eq!(calls, 1);
    }

    /// 寄存器地址必须是 8 位可表示值（R2 回归）：合法地址原样通过，
    /// ≥0x100 的越界地址必须报错而非静默回绕成另一个寄存器。
    #[test]
    fn test_ec_addr_u8_rejects_overflow() {
        assert_eq!(ec_addr_u8(0x68).unwrap(), 0x68);
        assert_eq!(ec_addr_u8(0xA7).unwrap(), 0xA7);
        assert_eq!(ec_addr_u8(0x00).unwrap(), 0x00);
        assert_eq!(ec_addr_u8(0xFF).unwrap(), 0xFF);
        // 越界：绝不静默写错寄存器。
        assert!(matches!(ec_addr_u8(0x100), Err(EcError::InvalidData(_))));
        assert!(matches!(ec_addr_u8(0x168), Err(EcError::InvalidData(_))));
    }
}
