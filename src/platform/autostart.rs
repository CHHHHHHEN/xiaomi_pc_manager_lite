use windows::core::{Interface, BSTR};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
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

/// HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND) = 0x80070002：任务不存在。
/// GetTask / DeleteTask 对"任务不存在"返回该值（本机实测），与其它错误
/// （瞬态 RPC/提供者失败、权限不足等）以此区分。
const HRESULT_TASK_NOT_FOUND: i32 = -2147024894; // 0x80070002

/// 单次任务调度器操作的 COM 生命周期作用域。
///
/// 每个**操作**（task_state / enable / disable）在自己独立的
/// `ComScope::init()` 内执行并与其配对 `CoUninitialize`（Drop 时自动执行，
/// 出错提前返回也不会漏）。历史实现只 `CoInitializeEx` 不 `CoUninitialize`：
/// `SetAutostart` 串行 worker 线程每切换一次开关都会调用一次 enable/disable，
/// 该线程的 COM 公寓引用计数随之无界增长；且与 fnkey.rs 明确写下的
/// "init 与 uninit 严格配对"约定矛盾（见其 run_watcher_once 注释）。配对后
/// 每次操作引用计数回到 0，下轮操作重新初始化，行为确定。
struct ComScope;

impl ComScope {
    /// 在本线程初始化 MTA 公寓（操作期间持有，Drop 时归零）。
    fn init() -> Result<Self, String> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| format!("CoInitializeEx: {}", e))?;
        }
        Ok(Self)
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

fn task_service() -> Result<ITaskService, String> {
    // COM 已由调用方操作入口（task_state / enable / disable）的 ComScope
    // 初始化；本函数只负责创建服务对象。
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
///
/// 只把 `GetTask` 的"任务不存在"（HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)
/// = 0x80070002，本机实测）映射为 `Ok(None)`；**其余错误如实传播**。
/// 历史实现把一切错误都当"任务不存在"（`Err(_) => Ok(None)`），
/// 计划任务服务临时故障（RPC 中断、提供程序忙、权限变更）时会被误判为
/// 任务缺失——`sync` 据此重建任务（TASK_CREATE_OR_UPDATE），把一次瞬态
/// 错误放大成一次不必要的任务重写，且错误本身被静默吞掉。
fn task_state() -> Result<Option<IRegisteredTask>, String> {
    let _com = ComScope::init()?;
    let service = task_service()?;
    let folder = task_folder(&service)?;
    match unsafe { folder.GetTask(&BSTR::from(TASK_NAME)) } {
        Ok(task) => Ok(Some(task)),
        Err(e) if e.code().0 == HRESULT_TASK_NOT_FOUND => Ok(None),
        Err(e) => Err(format!("GetTask: {}", e)),
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
    let principal = unsafe { def.Principal().map_err(|e| format!("Principal: {}", e))? };
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
    let actions = unsafe { def.Actions().map_err(|e| format!("Actions: {}", e))? };
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
        log::warn!(
            "Autostart task path '{}' != current exe '{}'",
            bstr_to_string(&path),
            exe
        );
    }
    if !args_match {
        log::warn!(
            "Autostart task args '{}' != '--autostart'",
            bstr_to_string(&args)
        );
    }
    Ok(path_matches && args_match)
}

/// 注册（创建或更新）开机自启动任务。
///
/// 登录触发 + 当前用户交互令牌，普通权限即可注册（无需管理员）。
pub fn enable() -> Result<(), String> {
    let _com = ComScope::init()?;
    let exe = std::env::current_exe()
        .map_err(|e| format!("current_exe: {}", e))?
        .to_string_lossy()
        .to_string();

    let service = task_service()?;
    let folder = task_folder(&service)?;

    let def: ITaskDefinition =
        unsafe { service.NewTask(0).map_err(|e| format!("NewTask: {}", e))? };

    // 注册信息：名称 + 描述
    unsafe {
        let reg = def
            .RegistrationInfo()
            .map_err(|e| format!("RegistrationInfo: {}", e))?;
        reg.SetDescription(&BSTR::from(TASK_DESC))
            .map_err(|e| format!("SetDescription: {}", e))?;
        reg.SetAuthor(&BSTR::from(crate::util::APP_NAME))
            .map_err(|e| format!("SetAuthor: {}", e))?;
    }

    // 主体：运行级别设为**最高权限**（TASK_RUNLEVEL_HIGHEST）。
    // F-AUTO-02 / AC-AUTO-03：任务须以管理员权限静默启动、不弹 UAC。
    // 计划任务在登录时由 Task Scheduler 服务托管启动，已提权上下文注册的
    // 开放运行时不会触发 UAC 弹窗；若按默认级别（LUA，非管理员）运行，
    // 应用启动时 `elevate_self()` 会每次登录弹出 UAC，违背自启动"驻留托盘
    // 不打扰用户"的设计。
    unsafe {
        let principal = def.Principal().map_err(|e| format!("Principal: {}", e))?;
        principal
            .SetRunLevel(TASK_RUNLEVEL_HIGHEST)
            .map_err(|e| format!("SetRunLevel: {}", e))?;
    }

    // 触发器：登录时（TASK_TRIGGER_LOGON）
    unsafe {
        let triggers = def.Triggers().map_err(|e| format!("Triggers: {}", e))?;
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
        let actions = def.Actions().map_err(|e| format!("Actions: {}", e))?;
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
    log::info!(
        "Autostart task '{}' registered ({} --autostart)",
        TASK_NAME,
        exe
    );
    Ok(())
}

/// 删除开机自启动任务。
///
/// 任务不存在（被用户/其他工具手动删除）时视为成功：否则取消勾选自启动
/// 会错误地展示"设置开机自启动失败"（F-AUTO-03），而任务本来就没有。
/// 删除失败仅当任务存在但删除操作被拒绝（如权限不足）。
pub fn disable() -> Result<(), String> {
    let _com = ComScope::init()?;
    let service = task_service()?;
    let folder = task_folder(&service)?;
    match unsafe { folder.DeleteTask(&BSTR::from(TASK_NAME), 0) } {
        Ok(()) => {
            log::info!("Autostart task '{}' deleted", TASK_NAME);
            Ok(())
        }
        Err(e) if e.code().0 == HRESULT_TASK_NOT_FOUND => {
            log::info!(
                "Autostart task '{}' not found; nothing to delete",
                TASK_NAME
            );
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
    // ComScope 覆盖整个同步流程：`task_state` 返回的 IRegisteredTask 对象
    // 随后被 `task_matches` 使用，若在 task_state 内就 CoUninitialize，
    // 公寓销毁后对该 COM 对象的调用行为未定义（同步线程每次只跑一次，
    // 此处必须持有到全部对象用完）。
    let _com = ComScope::init()?;
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
            } else if !config_enabled {
                // 配置关闭但任务存在：按设计不自动删除（保守，交由用户操作）。
                // 记录一次 debug，便于排查"明明关掉了开机自启动，任务计划里
                // 怎么还有 XiaomiPcManagerLite"。
                log::debug!(
                    "Autostart task exists but config disables it; leaving untouched (user-managed)"
                );
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

    /// 回归测试（无条件兜底清理）："任务不存在"的判定 HRESULT 必须是
    /// HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)=0x80070002（本机实测
    /// ITaskFolder::GetTask 对缺失任务返回该值）。历史实现把 GetTask 的
    /// **一切**错误都当作"任务不存在"，任务计划服务瞬态故障会触发 sync
    /// 不必要的重建；此常量是 task_state 区分"不存在"与"其它错误"的基准，
    /// 锁定其值与位模式，防止未来改动引入漂移。
    #[test]
    fn test_task_not_found_hresult_is_error_file_not_found() {
        assert_eq!(HRESULT_TASK_NOT_FOUND, -2_147_024_894);
        assert_eq!(HRESULT_TASK_NOT_FOUND as u32, 0x8007_0002);
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
            eprintln!(
                "skipping live Task Scheduler test (set XIAOMI_LIVE_TASKSCHEDULER_TEST=1 to run)"
            );
            return;
        }
        let _ = env_logger::builder().is_test(true).try_init();
        // 清理历史残留
        let _ = disable();
        assert!(task_state().unwrap_or(None).is_none());

        enable().expect("enable must succeed");
        assert!(
            task_state().unwrap_or(None).is_some(),
            "task must exist after enable"
        );

        disable().expect("disable must succeed");
        assert!(
            task_state().unwrap_or(None).is_none(),
            "task must be gone after disable"
        );
    }
}
