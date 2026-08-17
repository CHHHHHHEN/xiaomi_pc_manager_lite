use windows::core::{BSTR, Interface};
use windows::Win32::System::TaskScheduler::*;
use windows::Win32::System::Variant::VARIANT;

/// 开机自启动（F-AUTO）：通过 Windows 计划任务实现。
///
/// 任务约定（见需求文档 3.12）：
/// - 任务名固定为 `XiaomiPcManagerLite`
/// - 登录时触发（TASK_TRIGGER_LOGON）
/// - 以当前用户交互令牌运行，运行级别设为**最高权限**（TASK_RUNLEVEL_HIGHEST，
///   见 F-AUTO-02）——否则应用启动时 `elevate_self()` 会在每次登录弹出 UAC，
///   违背自启动"驻留托盘不打扰用户"的设计
/// - 执行命令：`<exe 绝对路径> --autostart`（启动后驻留托盘）
const TASK_NAME: &str = "XiaomiPcManagerLite";
const TASK_DESC: &str = "Xiaomi PC Manager Lite - 开机自启动";

fn task_service() -> Result<ITaskService, String> {
    // 调用方均为后台线程（UiCommand::SetAutostart 的线程、main.rs 的 sync
    // 线程），本线程初始化 COM 不会污染 GUI 线程（见 21e0aaf 的教训）。
    unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
        .ok()
        .map_err(|e| format!("CoInitializeEx: {}", e))?;
    }
    let service: ITaskService = unsafe {
        windows::Win32::System::Com::CoCreateInstance(
            &TaskScheduler,
            None,
            windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
        )
        .map_err(|e| format!("CoCreateInstance ITaskService: {}", e))?
    };
    unsafe {
        service
            .Connect(
                &VARIANT::default(),
                &VARIANT::default(),
                &VARIANT::default(),
                &VARIANT::default(),
            )
            .map_err(|e| format!("ITaskService::Connect: {}", e))?
    };
    Ok(service)
}

fn task_folder(service: &ITaskService) -> Result<ITaskFolder, String> {
    unsafe {
        service
            .GetFolder(&BSTR::from("\\"))
            .map_err(|e| format!("ITaskService::GetFolder: {}", e))
    }
}

/// 已注册的计划任务（F-AUTO-06 的查询与校验基础）。
fn task_state() -> Result<Option<IRegisteredTask>, String> {
    let service = task_service()?;
    let folder = task_folder(&service)?;
    match unsafe { folder.GetTask(&BSTR::from(TASK_NAME)) } {
        Ok(task) => Ok(Some(task)),
        Err(_) => Ok(None),
    }
}

fn bstr_to_string(b: &BSTR) -> String {
    String::from_utf16_lossy(&b[..])
}

/// 校验已注册任务是否符合当前预期（F-AUTO-09）：
/// - 运行级别为最高权限（TASK_RUNLEVEL_HIGHEST）——旧版本注册的任务用
///   默认级别（LUA，非管理员）运行，启动时 `elevate_self()` 会在每次登录
///   弹出 UAC，必须重建避免打扰用户；
/// - 执行路径为当前可执行文件绝对路径、参数为 `--autostart`——exe 被移动/
///   升级后旧任务指向失效路径，应立即重建。
fn task_matches(task: &IRegisteredTask) -> Result<bool, String> {
    let def = unsafe {
        task.Definition()
            .map_err(|e| format!("ITaskDefinition: {}", e))?
    };

    // 运行级别必须是最高权限。
    let principal = unsafe {
        def.Principal()
            .map_err(|e| format!("Principal: {}", e))?
    };
    let mut runlevel = TASK_RUNLEVEL_LUA;
    unsafe {
        principal
            .RunLevel(&mut runlevel)
            .map_err(|e| format!("RunLevel: {}", e))?;
    }
    if runlevel != TASK_RUNLEVEL_HIGHEST {
        log::warn!("Autostart task run level is not highest (LUA); needs rebase");
        return Ok(false);
    }

    // 首个动作必须是 `<exe> --autostart`。
    let actions = unsafe {
        def.Actions()
            .map_err(|e| format!("Actions: {}", e))?
    };
    let mut count = 0i32;
    unsafe {
        actions
            .Count(&mut count)
            .map_err(|e| format!("Actions::Count: {}", e))?;
    }
    if count < 1 {
        log::warn!("Autostart task has no actions; needs rebase");
        return Ok(false);
    }
    let action = unsafe {
        actions
            .get_Item(0)
            .map_err(|e| format!("Actions::get_Item: {}", e))?
    };
    let exec: IExecAction = action
        .cast()
        .map_err(|e| format!("action to IExecAction: {}", e))?;
    let mut path = BSTR::new();
    let mut args = BSTR::new();
    unsafe {
        exec.Path(&mut path)
            .map_err(|e| format!("IExecAction::Path: {}", e))?;
        exec.Arguments(&mut args)
            .map_err(|e| format!("IExecAction::Arguments: {}", e))?;
    }
    let exe = std::env::current_exe()
        .map_err(|e| format!("current_exe: {}", e))?
        .to_string_lossy()
        .to_string();
    let path_matches = bstr_to_string(&path).eq_ignore_ascii_case(&exe);
    let args_match = bstr_to_string(&args) == "--autostart";
    if !path_matches {
        log::warn!("Autostart task path '{}' != current exe '{}'", bstr_to_string(&path), exe);
    }
    if !args_match {
        log::warn!("Autostart task args '{}' != '--autostart'", bstr_to_string(&args));
    }
    Ok(path_matches && args_match)
}

/// 注册（创建或更新）开机自启动任务。
///
/// 登录触发 + 当前用户交互令牌，普通权限即可注册（无需管理员）。
pub fn enable() -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("current_exe: {}", e))?
        .to_string_lossy()
        .to_string();

    let service = task_service()?;
    let folder = task_folder(&service)?;

    let def: ITaskDefinition = unsafe {
        service
            .NewTask(0)
            .map_err(|e| format!("NewTask: {}", e))?
    };

    // 注册信息：名称 + 描述
    unsafe {
        let reg = def
            .RegistrationInfo()
            .map_err(|e| format!("RegistrationInfo: {}", e))?;
        reg.SetDescription(&BSTR::from(TASK_DESC))
            .map_err(|e| format!("SetDescription: {}", e))?;
        reg.SetAuthor(&BSTR::from("Xiaomi PC Manager Lite"))
            .map_err(|e| format!("SetAuthor: {}", e))?;
    }

    // 主体：运行级别设为**最高权限**（TASK_RUNLEVEL_HIGHEST）。
    // F-AUTO-02 / AC-AUTO-03：任务须以管理员权限静默启动、不弹 UAC。
    // 计划任务在登录时由 Task Scheduler 服务托管启动，已提权上下文注册的
    // 开放运行时不会触发 UAC 弹窗；若按默认级别（LUA，非管理员）运行，
    // 应用启动时 `elevate_self()` 会每次登录弹出 UAC，违背自启动"驻留托盘
    // 不打扰用户"的设计。
    unsafe {
        let principal = def
            .Principal()
            .map_err(|e| format!("Principal: {}", e))?;
        principal
            .SetRunLevel(TASK_RUNLEVEL_HIGHEST)
            .map_err(|e| format!("SetRunLevel: {}", e))?;
    }

    // 触发器：登录时（TASK_TRIGGER_LOGON）
    unsafe {
        let triggers = def
            .Triggers()
            .map_err(|e| format!("Triggers: {}", e))?;
        let trigger: ITrigger = triggers
            .Create(TASK_TRIGGER_LOGON)
            .map_err(|e| format!("Triggers::Create: {}", e))?;
        let logon: ILogonTrigger = trigger
            .cast()
            .map_err(|e| format!("trigger to ILogonTrigger: {}", e))?;
        logon
            .SetUserId(&BSTR::new())
            .map_err(|e| format!("SetUserId: {}", e))?;
    }

    // 动作：执行当前 exe，携带 --autostart
    unsafe {
        let actions = def
            .Actions()
            .map_err(|e| format!("Actions: {}", e))?;
        let action: IAction = actions
            .Create(TASK_ACTION_EXEC)
            .map_err(|e| format!("Actions::Create: {}", e))?;
        let exec: IExecAction = action
            .cast()
            .map_err(|e| format!("action to IExecAction: {}", e))?;
        exec.SetPath(&BSTR::from(&exe))
            .map_err(|e| format!("SetPath: {}", e))?;
        exec.SetArguments(&BSTR::from("--autostart"))
            .map_err(|e| format!("SetArguments: {}", e))?;
    }

    // 注册任务：创建或覆盖；以当前用户交互令牌运行（普通权限）
    unsafe {
        let _task = folder
            .RegisterTaskDefinition(
                &BSTR::from(TASK_NAME),
                &def,
                TASK_CREATE_OR_UPDATE.0,
                &VARIANT::default(),
                &VARIANT::default(),
                TASK_LOGON_INTERACTIVE_TOKEN,
                &VARIANT::default(),
            )
            .map_err(|e| format!("RegisterTaskDefinition: {}", e))?;
    }
    log::info!("Autostart task '{}' registered ({} --autostart)", TASK_NAME, exe);
    Ok(())
}

/// 删除开机自启动任务。
///
/// 任务不存在（被用户/其他工具手动删除）时视为成功：否则取消勾选自启动
/// 会错误地展示"设置开机自启动失败"（F-AUTO-03），而任务本来就没有。
/// 删除失败仅当任务存在但删除操作被拒绝（如权限不足）。
pub fn disable() -> Result<(), String> {
    let service = task_service()?;
    let folder = task_folder(&service)?;
    // HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)=0x80070002：任务不存在。
    const HRESULT_TASK_NOT_FOUND: i32 = -2147024894; // 0x80070002
    match unsafe { folder.DeleteTask(&BSTR::from(TASK_NAME), 0) } {
        Ok(()) => {
            log::info!("Autostart task '{}' deleted", TASK_NAME);
            Ok(())
        }
        Err(e) if e.code().0 == HRESULT_TASK_NOT_FOUND => {
            log::info!("Autostart task '{}' not found; nothing to delete", TASK_NAME);
            Ok(())
        }
        Err(e) => Err(format!("DeleteTask: {}", e)),
    }
}

/// 同步任务状态与配置（F-AUTO-06 / F-AUTO-09）：
/// - 配置开启但任务缺失时重建；
/// - 配置开启且任务存在但过期（运行级别非最高权限 → 登录弹 UAC；执行路径/
///   参数与当前 exe 不符 → 任务失效）时重建；
/// - 配置关闭但任务存在时删除（保守：不自动删除，交由用户操作）。
pub fn sync(config_enabled: bool) -> Result<(), String> {
    let task = task_state()?;
    match task {
        None => {
            if config_enabled {
                log::warn!("Autostart task missing but config enabled; re-registering");
                enable()?;
            }
        }
        Some(t) => {
            if config_enabled && !task_matches(&t)? {
                log::warn!("Autostart task is stale; re-registering");
                enable()?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 任务名必须固定（F-AUTO 约定），与文档一致。
    #[test]
    fn test_task_name_is_stable() {
        assert_eq!(TASK_NAME, "XiaomiPcManagerLite");
    }

    /// 真实环境验证（本机）：注册任务 → 查询存在 → 删除。
    /// 任务注册无需管理员（交互令牌、非最高权限）。
    ///
    /// **破坏性**：`disable()` 会删除机器上已存在的同名任务——若开发者
    /// 实际启用了开机自启动，`cargo test` 会静默删除其真实任务。必须显式
    /// 设置 `XIAOMI_LIVE_TASKSCHEDULER_TEST=1` 才运行，默认跳过。
    #[test]
    fn test_enable_exists_disable_roundtrip() {
        if std::env::var_os("XIAOMI_LIVE_TASKSCHEDULER_TEST").is_none() {
            eprintln!("skipping live Task Scheduler test (set XIAOMI_LIVE_TASKSCHEDULER_TEST=1 to run)");
            return;
        }
        let _ = env_logger::builder().is_test(true).try_init();
        // 清理历史残留
        let _ = disable();
        assert!(task_state().unwrap_or(None).is_none());

        enable().expect("enable must succeed");
        assert!(task_state().unwrap_or(None).is_some(), "task must exist after enable");

        disable().expect("disable must succeed");
        assert!(task_state().unwrap_or(None).is_none(), "task must be gone after disable");
    }
}
