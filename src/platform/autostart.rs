use windows::core::{BSTR, Interface};
use windows::Win32::System::TaskScheduler::*;
use windows::Win32::System::Variant::VARIANT;

/// 开机自启动（F-AUTO）：通过 Windows 计划任务实现。
///
/// 任务约定（见需求文档 3.12）：
/// - 任务名固定为 `XiaomiPcManagerLite`
/// - 登录时触发（TASK_TRIGGER_LOGON）
/// - 以当前用户交互令牌运行（普通权限，**不**要求管理员）——
///   符合提权策略：WMI/Auto 后端无需管理员
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

/// 计划任务当前是否存在（F-AUTO-06 的查询基础）。
pub fn task_exists() -> Result<bool, String> {
    let service = task_service()?;
    let folder = task_folder(&service)?;
    match unsafe { folder.GetTask(&BSTR::from(TASK_NAME)) } {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
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
pub fn disable() -> Result<(), String> {
    let service = task_service()?;
    let folder = task_folder(&service)?;
    unsafe {
        folder
            .DeleteTask(&BSTR::from(TASK_NAME), 0)
            .map_err(|e| format!("DeleteTask: {}", e))?;
    }
    log::info!("Autostart task '{}' deleted", TASK_NAME);
    Ok(())
}

/// 同步任务状态与配置（F-AUTO-06）：配置开启但任务缺失时重建。
/// 配置关闭但任务存在时删除（保守：不自动删除，交由用户操作）。
pub fn sync(config_enabled: bool) -> Result<(), String> {
    let exists = task_exists()?;
    if config_enabled && !exists {
        log::warn!("Autostart task missing but config enabled; re-registering");
        enable()?;
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
    #[test]
    fn test_enable_exists_disable_roundtrip() {
        let _ = env_logger::builder().is_test(true).try_init();
        // 清理历史残留
        let _ = disable();
        assert!(!task_exists().unwrap_or(false));

        enable().expect("enable must succeed");
        assert!(task_exists().unwrap_or(false), "task must exist after enable");

        disable().expect("disable must succeed");
        assert!(!task_exists().unwrap_or(false), "task must be gone after disable");
    }
}
