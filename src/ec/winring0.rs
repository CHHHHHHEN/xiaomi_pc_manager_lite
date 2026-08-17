use std::sync::Mutex;

use super::backend::EcBackend;
use super::error::EcError;
use super::addr as ec_addr;
use libloading::Library;

use windows::Win32::Foundation::GetLastError;
use windows::Win32::System::Services::*;
use windows::core::PCWSTR;

type ReadPort = unsafe extern "system" fn(u16) -> u8;
type WritePort = unsafe extern "system" fn(u16, u8);

fn ec_wait_write(rp: ReadPort) -> Result<(), EcError> {
    for i in 0..1000 {
        if unsafe { rp(ec_addr::EC_CMD) } & 0x02 == 0 {
            return Ok(());
        }
        if i < 100 {
            core::hint::spin_loop();
        } else {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    Err(EcError::Timeout(ec_addr::EC_CMD))
}

fn ec_wait_read(rp: ReadPort) -> Result<(), EcError> {
    for i in 0..1000 {
        if unsafe { rp(ec_addr::EC_CMD) } & 0x01 != 0 {
            return Ok(());
        }
        if i < 100 {
            core::hint::spin_loop();
        } else {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    Err(EcError::Timeout(ec_addr::EC_CMD))
}

pub struct WinRing0Backend {
    rp: ReadPort,
    wp: WritePort,
    lib: Library,
    lock: Mutex<()>,
}

impl Drop for WinRing0Backend {
    fn drop(&mut self) {
        if let Ok(deinit) = unsafe { self.lib.get(b"DeinitializeOls") } {
            let deinit: unsafe extern "system" fn() = *deinit;
            unsafe { deinit() };
        }
    }
}

fn dll_name() -> &'static str {
    if cfg!(target_pointer_width = "64") {
        "WinRing0x64.dll"
    } else {
        "WinRing0.dll"
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
    cleanup_service();

    let init: unsafe extern "system" fn() -> i32 =
        *unsafe { lib.get(b"InitializeOls") }
            .map_err(|e| EcError::DllLoad(e.to_string()))?;

    // Let InitializeOls handle driver installation (like the C version).
    // 失败重试：驱动的安装/加载可能因时序问题首次失败（例如刚解压的文件
    // 被 Defender 实时扫描锁定、服务清理尚未完成、驱动卸载未结束），
    // 稍作延时重试即可成功——历史实现只尝试一次，导致"首次切换 WinRing0
    // 显示失败、反复切换多次后才成功"。
    let mut init_error = 0u32;
    let mut init_ok = false;
    for attempt in 0..3 {
        log::info!("WinRing0: calling InitializeOls (attempt {})...", attempt + 1);
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

    let rp: ReadPort = *unsafe { lib.get(b"ReadIoPortByte") }
        .map_err(|e| EcError::DllLoad(e.to_string()))?;

    let wp: WritePort = *unsafe { lib.get(b"WriteIoPortByte") }
        .map_err(|e| EcError::DllLoad(e.to_string()))?;

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
            Err(_) => return,
        };
        let (_id_buf, id) = crate::util::to_pcwstr("WinRing0_1_2_0");
        if let Ok(svc) = OpenServiceW(scm, id, SERVICE_ALL_ACCESS) {
            let _ = ControlService(svc, SERVICE_CONTROL_STOP, std::ptr::null_mut());
            let _ = DeleteService(svc);
            let _ = CloseServiceHandle(svc);
            // 最多等待 3 秒：服务从 SCM 数据库中消失即认为清理完成。
            for _ in 0..30 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                match OpenServiceW(scm, id, SERVICE_ALL_ACCESS) {
                    Ok(h) => {
                        let _ = CloseServiceHandle(h);
                    }
                    Err(_) => break,
                }
            }
        }
        let _ = CloseServiceHandle(scm);
    }
}

/// Copy the .sys file to the EXE directory so that InitializeOls's internal
/// Initialize() can find it (it uses GetModuleFileName(NULL) which returns
/// the EXE path, then looks for .sys in the EXE directory).
fn ensure_sys_in_exe_dir(dll_path: &str) {
    let dll = std::path::Path::new(dll_path);
    let sys_name = dll.file_name()
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

fn try_load_all(dll_path: &str) -> Option<WinRing0Backend> {
    try_load(dll_path).ok().map(|(lib, rp, wp)| WinRing0Backend {
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
        let exe_dll = exe_dir.join(name);
        if let Some(backend) = try_load_all(&exe_dll.to_string_lossy()) {
            return Ok(backend);
        }

        // 2. Fall back to extracting the embedded binaries into the EXE
        //    directory and loading that copy (initialize behind it, so it
        //    finds the freshly written .sys next to it).
        match crate::embed::extract_winring0() {
            Ok(extracted_path) => {
                let path_str = extracted_path.to_string_lossy().to_string();
                match try_load_all(&path_str) {
                    Some(backend) => return Ok(backend),
                    None => log::warn!("WinRing0: load extracted DLL failed"),
                }
            }
            Err(e) => log::warn!("WinRing0: extract: {}", e),
        }

        Err(EcError::DllLoad(format!(
            "{} not found. Tried EXE directory and embedded extraction",
            name
        )))
    }
}

impl EcBackend for WinRing0Backend {
    fn name(&self) -> &'static str {
        "WinRing0 (I/O Port)"
    }

    fn read_byte(&self, addr: u16) -> Result<u8, EcError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| {
            log::warn!("WinRing0 mutex was poisoned, recovering");
            e.into_inner()
        });
        ec_wait_write(self.rp)?;
        unsafe { (self.wp)(ec_addr::EC_CMD, 0x80) };
        ec_wait_write(self.rp)?;
        unsafe { (self.wp)(ec_addr::EC_DATA, addr as u8) };
        ec_wait_read(self.rp)?;
        Ok(unsafe { (self.rp)(ec_addr::EC_DATA) })
    }

    fn write_byte(&self, addr: u16, value: u8) -> Result<(), EcError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| {
            log::warn!("WinRing0 mutex was poisoned, recovering");
            e.into_inner()
        });
        ec_wait_write(self.rp)?;
        unsafe { (self.wp)(ec_addr::EC_CMD, 0x81) };
        ec_wait_write(self.rp)?;
        unsafe { (self.wp)(ec_addr::EC_DATA, addr as u8) };
        ec_wait_write(self.rp)?;
        unsafe { (self.wp)(ec_addr::EC_DATA, value) };
        ec_wait_write(self.rp)?;
        Ok(())
    }

    // ── High-level battery ──

    fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
        // Derive from charge limit — EC may auto-sync BATTERY_CARE from
        // CHARGE_LIMIT on real hardware, so reading 0xA4 directly is unreliable.
        let limit = self.get_charge_limit()?;
        log::info!("WinRing0: battery care enabled by charge limit -> {}%", limit);
        Ok(limit < 100)
    }

    fn get_charge_limit(&self) -> Result<u8, EcError> {
        let limit = self.read_byte(ec_addr::CHARGE_LIMIT)?;
        // 寄存器直读可能返回损坏/未初始化的值：0xFF（255）或 0x00（未写入）
        // 都是垃圾值。>100 钳到 100；0 视为"未设置限制"，同样按 100（充满）
        // 处理——避免 GUI 显示 "0%/255%" 之类的荒谬数据、滑块溢出，以及
        // 养护位被错误推导（limit<100 的判定把垃圾值当成"养护开启"）。
        // 合法语义下 0 不可能出现：GUI 滑块下限 40，WMI 预设下限 40，
        // 配置消毒也会把 0 归一化为 80。
        let limit = if limit == 0 || limit > 100 { 100 } else { limit };
        log::info!("WinRing0: read charge limit -> {}%", limit);
        Ok(limit)
    }

    fn set_battery_care(&self, enabled: bool) -> Result<(), EcError> {
        let val = if enabled { 0x01 } else { 0x00 };
        log::info!("WinRing0: set battery care -> {:#x}", val);
        self.write_byte(ec_addr::BATTERY_CARE, val)
    }

    fn set_charge_limit(&self, percent: u8) -> Result<(), EcError> {
        let pct = percent.min(100);
        log::info!("WinRing0: set charge limit -> {}%", pct);
        self.write_byte(ec_addr::CHARGE_LIMIT, pct)
    }

    fn get_battery_state(&self) -> Result<(bool, u8), EcError> {
        // 单次端口读：养护位由限值推导（见 get_battery_care_enabled），
        // 避免默认实现再读一次限值（B-WMI-1）。
        let limit = self.get_charge_limit()?;
        Ok((limit < 100, limit))
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
