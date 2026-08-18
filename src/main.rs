#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod command;
mod ec;
mod embed;
mod gui;
mod platform;
mod startup;
#[cfg(test)]
mod testutil;
mod tray;
mod util;

use ec::config::ConfigStore;

/// 统一的 panic hook：无论构建类型都先把 panic 信息写入应用日志文件。
/// release 构建无控制台（windows_subsystem = "windows"），默认 panic 输出
/// 不可见，进程"无声消失"时日志是唯一线索；debug 构建额外暂停等待输入，
/// 便于直接在控制台阅读 panic 信息。
fn init_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("PANIC: {}", info);
        prev(info);
        #[cfg(debug_assertions)]
        {
            use std::io::Write;
            let _ = std::io::stdout().write_all(b"\n--- PANIC ---\nPress Enter to exit...");
            let _ = std::io::stdin().read_line(&mut String::new());
        }
    }));
}

/// 初始化日志：默认写入 `%TEMP%\XiaomiPcManagerLite\app.log`，
/// 可用 `XIAOMI_LOG_FILE` 覆盖路径（统一收敛在 `util::log_file_path`）。
///
/// 写入模式：**追加**（历史只 `File::create` 覆盖，每次运行把上一份日志
/// 抹掉——应用崩溃/异常退出后上次运行日志丢失，无法排查"上一次为什么挂"）。
/// 追加前做**按大小轮转**：日志超过阈值时把旧文件改名为 `app.log.1` 保留
/// 上一份，避免无界增长（`%TEMP%` 位于系统盘，长期运行可能写满）。
fn init_logging() {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    let log_path = crate::util::log_file_path();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    rotate_log_if_large(&log_path);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => {
            builder.target(env_logger::Target::Pipe(Box::new(file)));
        }
        Err(e) => {
            eprintln!("log file {}: {}", log_path.to_string_lossy(), e);
        }
    }
    let _ = builder.try_init();
    // 日志文件路径在此之后才可用：默认 `%TEMP%\XiaomiPcManagerLite\app.log`，
    // 排查问题时日志首行即告知日志落盘位置与版本号。
    log::info!(
        "===== {} v{} ====",
        crate::util::APP_NAME,
        crate::util::APP_VERSION
    );
    log::info!("Log file: {}", log_path.display());
}

/// 日志按大小轮转的阈值：超过即把旧文件改名 `app.log.1`。
const LOG_ROTATE_BYTES: u64 = 4 * 1024 * 1024;

/// 若日志文件超过阈值，先把它改名 `app.log.1`（覆盖旧的 `.1`），再让
/// 调用方新建/追加新日志。历史内容保留一份，避免无界增长。
fn rotate_log_if_large(path: &std::path::Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= LOG_ROTATE_BYTES {
        return;
    }
    let rotated = path.with_extension("log.1");
    match std::fs::rename(path, &rotated) {
        Ok(()) => log::info!("Log rotated: {:?} -> {:?}", path, rotated),
        Err(e) => eprintln!("log rotate {}: {}", path.to_string_lossy(), e),
    }
}

/// 为进程注册显式 AppUserModelID：托盘气泡通知（`NIF_INFO`/`Shell_NotifyIconW`）
/// 在 Windows 8+ 上依赖它才能可靠展示（无 ID 时通知可能被静默丢弃）。在启动
/// 时、任何通知弹出前调用一次即可（进程级全局设置）。ID 固定为产品名，
/// 与托盘图标/版本信息保持一致。失败不影响功能（仅通知展示可能受限），
/// 记录 debug 日志即可。
fn register_app_user_model_id() {
    let id = crate::util::WideString::new("XiaomiPcManagerLite");
    match unsafe {
        windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(id.as_pcwstr())
    } {
        Ok(()) => log::debug!("AppUserModelID registered: XiaomiPcManagerLite"),
        Err(e) => log::warn!("SetCurrentProcessExplicitAppUserModelID failed: {}", e),
    }
}

fn main() {
    init_logging();
    init_panic_hook();
    register_app_user_model_id();

    log::debug!(
        "args: {:?}",
        std::env::args_os()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    );

    // 单实例预检（F-AUTO-08）：必须在**提权之前**执行。若已有实例在运行
    // （如自启动驻留托盘中），直接唤醒其窗口并退出——否则每次手动启动都会
    // 先弹 UAC 提权、再被互斥体判定为第二实例，白白弹一次提权提示。
    // pre_flight 只探测不持有：临时取得的所有权立即释放，真正的互斥体
    // 所有权由下方提权完成后的 acquire() 取得。
    if crate::platform::single_instance::pre_flight() {
        log::info!("Existing instance running; activating its window");
        crate::platform::window::show_main_window();
        return;
    }
    log::debug!("Single-instance pre-flight: no conflict; proceeding");

    // 启动即提权：**WMI 与 WinRing0 都需要管理员权限**。本机实测
    // （受限令牌对照实验）：非管理员下 `SELECT * FROM MICommonInterface`
    // 直接返回拒绝访问（Access denied），WMI 后端完全不可用；WinRing0
    // 驱动加载同样需要管理员。用户拒绝 UAC 时继续以非管理员运行，
    // create_backend 会失败并回退，GUI 显示错误（见下方回退逻辑）。
    if crate::platform::privilege::is_admin() {
        log::info!("Running with administrator privileges");
    } else if crate::platform::privilege::elevate_self() {
        log::info!("Elevated instance relaunched; exiting this process");
        return;
    } else {
        log::warn!("Not running as administrator; WMI/WinRing0 may be unavailable");
    }

    // 单实例保护（F-AUTO-08）：提权完成后的最终实例在此取得互斥体所有权。
    // 已在运行的另一实例（如自启动驻留托盘中）存在时，把已有窗口调到前台
    // 并退出，避免双份托盘/热键/Fn+K 订阅同时写 EC。互斥体句柄必须持有至
    // 进程退出：**不能**放在 match 臂体内（臂体内绑定在臂结束时即被 drop，
    // 互斥体立即释放、单实例保护失效），用 let 绑定到 main 作用域末尾。
    let _instance_guard: Option<crate::platform::single_instance::SingleInstanceGuard> =
        match crate::platform::single_instance::acquire() {
            crate::platform::single_instance::SingleInstance::Acquired(guard) => {
                log::info!("Single-instance mutex acquired");
                Some(guard)
            }
            // 已有实例在运行：唤醒已有窗口后退出，不重复启动。
            crate::platform::single_instance::SingleInstance::Existing => {
                log::info!("Another instance is running; activating its window");
                crate::platform::window::show_main_window();
                return;
            }
            // API 异常无法确认冲突（如 CreateMutexW 罕见失败）：按文档契约
            // "防御性按无冲突处理，不阻塞启动"继续启动。历史实现把 Unknown
            // 与 Existing 一并处理，导致 API 异常时应用静默退出、绝不启动。
            crate::platform::single_instance::SingleInstance::Unknown => {
                log::warn!("Single instance check unavailable; proceeding");
                None
            }
        };

    let store = ConfigStore::new();
    log::info!("Config file: {}", store.path().display());
    let config = store.load();
    log::debug!("Loaded config: {:#?}", config);

    // 后端创建与启动应用在独立线程执行，并**阻塞等待**其完成：目的是把任何
    // 可能发生在后端初始化路径上的 COM 初始化（无论当前还是未来某后端）都
    // 挡在 GUI 主线程之外，主线程保持"从未初始化 COM"。WMI 后端自身的
    // CoInitializeEx(MTA) 实际发生在它专用的 wmi-worker 线程上（见
    // ec/wmi.rs 的线程模型注释），此处线程并不直接初始化 COM——隔离的是
    // "万一某个组件在该线程初始化了 COM"的边界，使 eframe/winit 及任何
    // 后续组件按需安全初始化。历史回归（21e0aaf）正是主线程先被初始化为
    // MTA 后，其它组件（当时 Tauri/tao 栈的 OleInitialize，要求 STA）再
    // 初始化 COM 时返回 RPC_E_CHANGED_MODE 崩溃。
    let thread_config = config.clone();
    let thread_store = store.clone();
    // F-AUTO-06: 开机自启动任务一致性校验（后台线程，不阻塞 GUI；
    // 该线程的 COM 由 autostart::sync 的 ComScope 自行初始化/配对回收）。
    {
        let cfg = thread_config.clone();
        std::thread::spawn(move || {
            if let Err(e) = platform::autostart::sync(cfg.auto_start_on_boot) {
                log::warn!("autostart sync: {}", e);
            }
        });
    }
    // 返回修改后的 config：启动同步（量化读回、矛盾兜底）发生在该线程的
    // config 副本上并已落盘；若不把该副本交还给 GUI，GUI 的 save_state()
    // 会把未同步的旧值（如 care=true+limit=100、85% 非预设值）重新写回
    // 磁盘，覆盖启动时验证过的配置，导致磁盘配置反复"复活"矛盾组合。
    //
    // 后端初始化耗时统计：最耗时的两步（WMI 握手最长 10s、WinRing0 驱动
    // 安装重试 3 次×500ms）都发生在这里。记录耗时便于排查"启动很慢"类
    // 问题——若耗时接近某个后端超时上限，日志数值本身即可指向卡点。
    let backend_init_start = std::time::Instant::now();
    // **线程内 panic 兜底**（M4 回归）：init_backend 内部涉及 COM/驱动 FFI
    // 边界，任一步 panic（如 FFI 在异常状态下返回意外状态导致 unwrap 触发）
    // 都会让线程 panic。历史实现 `.join().expect(...)` 把线程 panic 直接
    // 传播到主线程——release 构建无控制台（windows_subsystem="windows"），
    // 进程无声退出，连精心设计的 NullBackend 兜底都到不了。改为在线程内
    // `catch_unwind`：panic 被捕获后按"后端不可用"优雅降级（NullBackend +
    // 错误提示），GUI 照常启动并展示错误，仅功能不可用。
    // 注意：catch_unwind 是闭包内的**第一条**语句，线程不存在"进入闭包前
    // panic"的窗口，join 只会在闭包整体正常返回后成功；此处对 join 结果
    // 直接 expect（若真发生也属编程错误，此时无后端可用比静默消失更可排查）。
    let startup_result = std::thread::spawn(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            startup::init_backend(thread_store, thread_config)
        }))
    })
    .join()
    .expect("EC backend init thread panicked before catch_unwind");
    let startup::StartupResult {
        backend,
        config,
        init_error,
        effective_pref,
    } = match startup_result {
        Ok(result) => result,
        // 线程内 panic：降级为 NullBackend，让 GUI 正常启动并展示错误。
        Err(panic) => {
            let payload = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".into());
            log::error!("EC backend init panicked: {}", payload);
            let effective_pref = config.backend;
            startup::StartupResult {
                backend: Box::new(ec::backend::NullBackend),
                config,
                init_error: Some(format!("EC 后端初始化异常: {}", payload)),
                effective_pref,
            }
        }
    };
    log::info!(
        "EC backend init took {} ms",
        backend_init_start.elapsed().as_millis()
    );

    // F-AUTO-07: --autostart 启动时驻留托盘（首帧最小化）。
    // 用 args_os 而非 args：Windows 允许非 UTF-8 的命令行参数，args()
    // 在遇到非 UTF-8 参数时会 panic，args_os 则只做逐字节比较。
    let autostart = std::env::args_os().any(|a| a == std::ffi::OsStr::new("--autostart"));
    log::info!("Launching GUI (--autostart mode: {})", autostart);
    gui::run_app(
        store,
        backend,
        config,
        effective_pref,
        init_error,
        autostart,
    );
    // 主流程结束（GUI 事件循环正常返回后唯一路径）：进程即将退出。该行是
    // 生命周期日志链的终点——从日志第一行的版本号到这一行，能确认进程
    // "完整走完启动→运行→退出"；若缺失，说明进程被外部强杀（如任务管理器
    // 结束进程、tray 兜底 process::exit 前的超时）。
    log::info!("App exiting normally");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 日志轮转：超过阈值时旧文件被改名 `app.log.1`（保留上一份历史），
    /// 未超过时保持原样。改名前旧的 `.1` 会被覆盖（只保留最近两份）。
    #[test]
    fn test_rotate_log_if_large() {
        let dir = std::env::temp_dir().join(format!("xmpl-log-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create log dir");
        let path = dir.join("app.log");

        // 小文件（低于阈值）：不轮转，原文件保留。
        std::fs::write(&path, "small log").expect("write small");
        rotate_log_if_large(&path);
        assert!(path.exists(), "small log must not be rotated");
        assert!(!dir.join("app.log.1").exists());

        // 超阈值文件：改名 `app.log.1`，原路径不再存在。
        std::fs::write(&path, vec![b'x'; (LOG_ROTATE_BYTES + 1) as usize]).expect("write large");
        rotate_log_if_large(&path);
        assert!(!path.exists(), "oversized log must be renamed away");
        assert!(
            dir.join("app.log.1").exists(),
            "oversized log must become app.log.1"
        );

        // 已有旧 `.1` 时新轮转覆盖它（只保留最近一份历史）。
        std::fs::write(&path, vec![b'y'; (LOG_ROTATE_BYTES + 1) as usize]).expect("write large 2");
        std::fs::write(dir.join("app.log.1"), b"old backup").expect("write old backup");
        rotate_log_if_large(&path);
        let content = std::fs::read(dir.join("app.log.1")).expect("read rotated");
        assert_eq!(
            content.len(),
            (LOG_ROTATE_BYTES + 1) as usize,
            "new rotation must replace old backup"
        );

        // 缺失文件：无操作不报错。
        let missing = dir.join("missing.log");
        rotate_log_if_large(&missing);
        assert!(!missing.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
