#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod command;
mod ec;
mod embed;
mod gui;
mod tray;
mod util;

use ec::backend::EcBackend;
use ec::config::AppConfig;

/// In debug builds, set up a panic hook that pauses before exit so
/// the user can read panic messages in the console.
#[cfg(debug_assertions)]
fn init_pause_on_panic() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        prev(info);
        use std::io::Write;
        let _ = std::io::stdout().write_all(b"\n--- PANIC ---\nPress Enter to exit...");
        let _ = std::io::stdin().read_line(&mut String::new());
    }));
}

#[cfg(not(debug_assertions))]
fn init_pause_on_panic() {}

/// If the current process is a debug build, block until the user
/// presses Enter so the console window does not disappear.
#[cfg(debug_assertions)]
fn pause_on_exit() {
    use std::io::Write;
    let _ = std::io::stdout().write_all(b"\nPress Enter to exit...");
    let _ = std::io::stdin().read_line(&mut String::new());
}

#[cfg(not(debug_assertions))]
fn pause_on_exit() {}

fn is_admin() -> bool {
    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        if windows::Win32::System::Threading::OpenProcessToken(
            windows::Win32::System::Threading::GetCurrentProcess(),
            windows::Win32::Security::TOKEN_QUERY,
            &mut token,
        )
        .is_err()
        {
            return false;
        }
        let mut elevation = windows::Win32::Security::TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let ok = windows::Win32::Security::GetTokenInformation(
            token,
            windows::Win32::Security::TokenElevation,
            Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<windows::Win32::Security::TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Returns `true` if a new elevated instance was launched and the
/// caller should exit this process (instead of running the GUI).
fn elevate() -> bool {
    if is_admin() {
        return false;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            log::warn!("current_exe: {}", e);
            return false;
        }
    };
    let (_verb_buf, verb) = crate::util::to_pcwstr("runas");
    let (_path_buf, path) = crate::util::to_pcwstr(&exe.to_string_lossy());
    let ret = unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            verb,
            path,
            windows::core::PCWSTR::null(),
            windows::core::PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWDEFAULT,
        )
    };
    // ShellExecuteW returns a value > 32 on success (HINSTANCE cast), and an
    // error code on failure. When the user **declines** the UAC prompt it
    // returns ERROR_CANCELLED (1223) — an error code greater than 32 — so a
    // bare "> 32" check would mistake the decline for a successful launch,
    // exit this process, and the app would silently disappear without any
    // instance running. Treat 1223 as "declined": continue without admin
    // (WinRing0 unavailable, WMI fallback).
    const ERROR_CANCELLED: isize = 1223;
    let ret_val = ret.0 as isize;
    if ret_val > 32 && ret_val != ERROR_CANCELLED {
        log::info!("Elevated instance launched; exiting non-admin process.");
        true
    } else if ret_val == ERROR_CANCELLED {
        log::warn!("Elevation declined by the user; continuing without admin. WinRing0 will be unavailable.");
        false
    } else {
        log::warn!("Elevation failed (ret={}); continuing without admin. WinRing0 will be unavailable.", ret_val);
        false
    }
}

fn main() {
    env_logger::init();
    init_pause_on_panic();

    // Relaunch as admin if not already elevated — WinRing0 requires
    // administrative rights for port I/O access.
    if elevate() {
        pause_on_exit();
        return;
    }

    let config = AppConfig::load();

    // 后端创建与启动应用在后台线程执行：WMI 后端会在此线程调用
    // CoInitializeEx(MTA) 初始化 COM。GUI 主线程因此不携带任何 COM 初始化
    // 状态——21e0aaf 修复的回归正是主线程先被初始化为 MTA 后，其它组件
    // （当时 Tauri/tao 栈的 OleInitialize，要求 STA）再初始化 COM 时返回
    // RPC_E_CHANGED_MODE 崩溃；保持主线程"未初始化 COM"可让 eframe/winit
    // 及任何后续组件按需安全初始化。
    let thread_config = config.clone();
    // 返回修改后的 config：启动同步（量化读回、矛盾兜底）发生在该线程的
    // config 副本上并已落盘；若不把该副本交还给 GUI，GUI 的 save_state()
    // 会把未同步的旧值（如 care=true+limit=100、85% 非预设值）重新写回
    // 磁盘，覆盖启动时验证过的配置，导致磁盘配置反复"复活"矛盾组合。
    let (backend, config, init_error) =
        std::thread::spawn(move || -> (Box<dyn EcBackend>, AppConfig, Option<String>) {
        let mut config = thread_config;
        let (backend, mut init_error): (Box<dyn EcBackend>, Option<String>) =
            match ec::backend::create_backend(config.backend) {
                Ok(b) => {
                    log::info!("EC backend: {} (preference: {:?})", b.name(), config.backend);
                    (b, None)
                }
                Err(_) => {
                    log::warn!("Configured backend {:?} unavailable; falling back to Auto", config.backend);
                    match ec::backend::create_backend(ec::config::BackendPreference::Auto) {
                        Ok(b) => {
                            let name = b.name().to_string();
                            log::info!("Fallback EC backend: {}", name);
                            (b, Some(format!("优先后端不可用，已自动切换至 {}", name)))
                        }
                        Err(e) => {
                            log::error!("Failed to create any EC backend: {}", e);
                            (Box::new(ec::backend::NullBackend), Some(e.to_string()))
                        }
                    }
                }
            };

        if config.auto_apply_on_startup {
            let outcome = apply_startup_config(&*backend, &config);

            // F-START-04: 自动应用失败的错误除了记录日志，还要在 GUI 中展示。
            if !outcome.errors.is_empty() {
                let apply_err = format!("启动应用设置失败: {}", outcome.errors.join("; "));
                init_error = Some(match init_error.take() {
                    Some(e) => format!("{}; {}", e, apply_err),
                    None => apply_err,
                });
            }

            // Only sync the stored config to the verified hardware state when it
            // was actually applied.  Otherwise the saved user preferences would be
            // silently overwritten by whatever the hardware currently reports.
            if outcome.perf_mode_ok && outcome.perf_mode_written == config.performance_mode {
                if let Ok(mode) = backend.get_performance_mode() {
                    config.performance_mode = mode;
                }
            }
            if outcome.battery_care_ok {
                if let Ok(enabled) = backend.get_battery_care_enabled() {
                    config.battery_care_enabled = enabled;
                    // When care is disabled the hardware limit is 100% by definition;
                    // keep the stored limit as the user's desired value for when care
                    // is re-enabled.
                    if enabled && outcome.charge_limit_ok {
                        if let Ok(limit) = backend.get_charge_limit() {
                            config.battery_charge_limit = limit;
                        }
                    }
                }
            }

            if let Err(e) = config.save() {
                log::warn!("save initial config: {}", e);
            }
        }

        (backend, config, init_error)
    })
    .join()
    .expect("EC backend init thread panicked");

    gui::run_app(backend, config, init_error);
    pause_on_exit();
}

/// 启动应用的结果：逐项记录是否成功写入硬件。
struct StartupApplyOutcome {
    charge_limit_ok: bool,
    battery_care_ok: bool,
    perf_mode_ok: bool,
    /// 实际写入 EC 的性能模式 raw code（经交流电源保护降级后的值）。
    perf_mode_written: u8,
    /// 失败项的中文描述（每项一条），用于在 GUI 中向用户展示。
    errors: Vec<String>,
}

impl Default for StartupApplyOutcome {
    fn default() -> Self {
        Self {
            charge_limit_ok: true,
            battery_care_ok: true,
            perf_mode_ok: true,
            perf_mode_written: 0,
            errors: Vec::new(),
        }
    }
}

fn apply_startup_config(backend: &dyn EcBackend, config: &AppConfig) -> StartupApplyOutcome {
    let mut outcome = StartupApplyOutcome::default();
    if !config.auto_apply_on_startup {
        return outcome;
    }
    log::info!("Applying config on startup");
    // Keep battery care and charge limit coherent: when care is disabled
    // the limit must be 100%, otherwise backends that derive the care bit
    // from the limit would report it as enabled.  The limit is written
    // first because some EC firmware auto-syncs the care bit from it.
    let desired_limit = if config.battery_care_enabled && config.battery_charge_limit >= 100 {
        // 旧版本/手改配置可能残留 care=true + limit=100 的矛盾组合
        // （旧版 refresh_from_backend 曾把硬件状态写回 config）。
        // 与 GUI 切换路径（set_battery_care_internal）保持一致，兜底为
        // 80%，否则 100% 写进硬件后养护实际失效、配置被静默改写。
        log::warn!(
            "Incoherent config: battery care on with limit {}%; using 80%",
            config.battery_charge_limit
        );
        80
    } else {
        config.battery_charge_limit
    };
    if config.battery_care_enabled {
        outcome.charge_limit_ok = match backend.set_charge_limit(desired_limit) {
            Ok(_) => true,
            Err(e) => {
                log::warn!("apply charge limit on startup: {}", e);
                outcome.errors.push(format!("充电上限: {}", e));
                false
            }
        };
    } else if let Err(e) = backend.set_charge_limit(100) {
        log::warn!("apply charge limit on startup: {}", e);
        outcome.charge_limit_ok = false;
        outcome.errors.push(format!("充电上限: {}", e));
    }
    outcome.battery_care_ok = match backend.set_battery_care(config.battery_care_enabled) {
        Ok(_) => true,
        Err(e) => {
            log::warn!("apply battery care on startup: {}", e);
            outcome.errors.push(format!("电池养护: {}", e));
            false
        }
    };
    // 狂暴模式需要交流电源：写入时按电源状态选择实际 raw code，但用户的
    // 选择仍保存在 config 中，待接入电源后通过 ReapplyConfig 恢复。
    let raw =
        ec::performance::effective_ec_value(config.performance_mode, ec::performance::ac_power_status());
    outcome.perf_mode_written = raw;
    outcome.perf_mode_ok = match backend.set_performance_mode(raw) {
        Ok(_) => true,
        Err(e) => {
            log::warn!("apply perf mode on startup: {}", e);
            outcome.errors.push(format!("性能模式: {}", e));
            false
        }
    };
    outcome
}
