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

    // Let InitializeOls handle driver installation (like the C version)
    log::info!("WinRing0: calling InitializeOls...");
    if unsafe { init() } == 0 {
        let err = unsafe { GetLastError() };
        log::warn!("WinRing0: InitializeOls returned 0 (failed, last_error={})", err.0);
        return Err(EcError::InitFailed(format!("错误码: {:#x}", err.0)));
    }
    log::info!("WinRing0: InitializeOls succeeded");

    let rp: ReadPort = *unsafe { lib.get(b"ReadIoPortByte") }
        .map_err(|e| EcError::DllLoad(e.to_string()))?;

    let wp: WritePort = *unsafe { lib.get(b"WriteIoPortByte") }
        .map_err(|e| EcError::DllLoad(e.to_string()))?;

    Ok((lib, rp, wp))
}

/// Remove any stale WinRing0 service from previous runs.
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

        // 1. Try current working directory (like the C version)
        if let Some(backend) = try_load_all(name) {
            return Ok(backend);
        }

        // 2. Try alongside the EXE
        if let Ok(exe) = std::env::current_exe() {
            if let Some(exe_dir) = exe.parent() {
                let path = exe_dir.join(name);
                if let Some(backend) = try_load_all(&path.to_string_lossy()) {
                    return Ok(backend);
                }
            }
        }

        // 3. Fall back to extracting embedded binaries
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
            "{} not found. Tried CWD and embedded extraction",
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
        ec_wait_write(self.rp)?;
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
        let val = self.read_byte(ec_addr::BATTERY_CARE)?;
        log::info!("WinRing0: read battery care -> {:#x}", val);
        Ok(val & 0x01 != 0)
    }

    fn get_charge_limit(&self) -> Result<u8, EcError> {
        let limit = self.read_byte(ec_addr::CHARGE_LIMIT)?;
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
