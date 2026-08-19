use crate::util::err_fmt;
use windows::core::{Interface, BSTR};
use windows::Win32::Foundation::{VARIANT_FALSE, VARIANT_TRUE};
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
/// - 任务设置显式关闭"电池供电时停止任务 / 禁止电池下启动"，且执行时长设为
///   无限（PT0S）——Windows 默认 `StopIfGoingOnBatteries=TRUE`（切到电池即
///   终止任务）与 `ExecutionTimeLimit=PT72H`（运行满 72h 强制结束）都会杀掉
///   常驻托盘的应用进程，必须以显式设置覆盖（见 F-AUTO-11）
/// - 执行命令：`<exe 绝对路径> --autostart`（启动后驻留托盘）
// 任务名与其它机器标识（AppUserModelID/配置目录/单实例互斥体）同源，
// 统一收敛到 util::APP_ID（修订 1.50）。
const TASK_NAME: &str = crate::util::APP_ID;
const TASK_DESC: &str = "Xiaomi PC Manager Lite - 开机自启动";

/// 执行时长不限：计划任务默认 `ExecutionTimeLimit=PT72H`，任务运行超过 72 小时
/// 会被任务计划服务强制终止——托盘常驻进程必须显式设为 PT0S 禁用该上限。
/// 锁定该字符串，供 `task_matches` 校验与 `enable` 写入共用。
const TASK_EXEC_TIME_LIMIT_DISABLED: &str = "PT0S";

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
///
/// 与 battery_health 共用 `win::com::ComScope`（修订 1.46 审计收敛）。
type ComScope = crate::win::ComScope;

fn task_service() -> Result<ITaskService, String> {
    // COM 已由调用方操作入口（task_state / enable / disable）的 ComScope
    // 初始化；本函数只负责创建服务对象。
    let service: ITaskService = unsafe {
        windows::Win32::System::Com::CoCreateInstance(
            &TaskScheduler,
            None,
            windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
        )
        .map_err(|e| err_fmt("CoCreateInstance ITaskService", e))?
    };
    unsafe {
        service
            .Connect(
                &VARIANT::default(),
                &VARIANT::default(),
                &VARIANT::default(),
                &VARIANT::default(),
            )
            .map_err(|e| err_fmt("ITaskService::Connect", e))?
    };
    Ok(service)
}

fn task_folder(service: &ITaskService) -> Result<ITaskFolder, String> {
    unsafe {
        service
            .GetFolder(&BSTR::from("\\"))
            .map_err(|e| err_fmt("ITaskService::GetFolder", e))
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
        Err(e) => Err(err_fmt("GetTask", e)),
    }
}

fn bstr_to_string(b: &BSTR) -> String {
    String::from_utf16_lossy(&b[..])
}

/// 从 Task Scheduler 任务 XML 中提取首个 `<tag>...</tag>` 的文本内容。
///
/// 用途：F-AUTO-09 校验任务动作（`<Exec><Command>路径</Command>
/// <Arguments>--autostart</Arguments></Exec>`）时，`IActionCollection::get_Item`
/// 在 windows-rs 0.62 中生成的包装是坏的（把 `VARIANT` 参数误标为 `i32`，
/// 本机实测恒返回 0x80070057/0x80004005），改从任务定义 XML（
/// `ITaskDefinition::XmlText`，纯 BSTR 读取无 ABI 问题）解析。
///
/// 支持 XML 转义还原（`&amp; &lt; &gt; &quot; &apos;`）与 CDATA 包裹；自闭合
/// 标签（`<tag />`）或缺失返回 None。
fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)? + open.len();
    let rest = xml.get(start..)?;
    let end = rest.find(&close)?;
    let raw = rest.get(..end)?.trim();
    // CDATA 包裹：`<![CDATA[内容]]>`，内容原样（内部不做实体转义）。
    if let Some(inner) = raw
        .strip_prefix("<![CDATA[")
        .and_then(|r| r.strip_suffix("]]>"))
    {
        return Some(inner.to_string());
    }
    let mut out = String::with_capacity(raw.len());
    let mut iter = raw.chars().peekable();
    while let Some(c) = iter.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        // 读实体（到分号为止；上限防异常长输入）。
        let mut ent = String::from("&");
        for c2 in iter.by_ref() {
            ent.push(c2);
            if c2 == ';' {
                break;
            }
            if ent.len() > 12 {
                break;
            }
        }
        let replaced = match ent.as_str() {
            "&amp;" => Some('&'),
            "&lt;" => Some('<'),
            "&gt;" => Some('>'),
            "&quot;" => Some('"'),
            "&apos;" => Some('\''),
            _ => None,
        };
        if let Some(r) = replaced {
            out.push(r);
        } else {
            out.push_str(&ent);
        }
    }
    Some(out)
}

/// 校验已注册任务是否符合当前预期（F-AUTO-09）：
/// - 运行级别为最高权限（TASK_RUNLEVEL_HIGHEST）——旧版本注册的任务用
///   默认级别（LUA，非管理员）运行，启动时 `elevate_self()` 会在每次登录
///   弹出 UAC，必须重建避免打扰用户；
/// - 电池/时长设置符合预期——历史版本注册的任务未显式设置，`StopIfGoingOnBatteries`
///   沿用默认 TRUE（拔电即被任务计划服务终止），`ExecutionTimeLimit` 沿用默认
///   PT72H（常驻运行满 72 小时被终止）；这两项不满足都必须重建，否则升级后
///   托盘应用仍会被平台杀掉；
/// - 执行路径为当前可执行文件绝对路径、参数为 `--autostart`——exe 被移动/
///   升级后旧任务指向失效路径，应立即重建。
fn task_matches(task: &IRegisteredTask) -> Result<bool, String> {
    let def = unsafe {
        task.Definition()
            .map_err(|e| err_fmt("ITaskDefinition", e))?
    };

    // 运行级别必须是最高权限。
    let principal = unsafe { def.Principal().map_err(|e| err_fmt("Principal", e))? };
    let mut runlevel = TASK_RUNLEVEL_LUA;
    unsafe {
        principal
            .RunLevel(&mut runlevel)
            .map_err(|e| err_fmt("RunLevel", e))?;
    }
    if runlevel != TASK_RUNLEVEL_HIGHEST {
        log::warn!("Autostart task run level is not highest (LUA); needs rebase");
        return Ok(false);
    }

    // 首个动作必须是 `<exe> --autostart`。
    //
    // 读取方式：任务定义 XML（`ITaskDefinition::XmlText`），而非
    // `IActionCollection::get_Item`——后者在 windows-rs 0.62 生成的包装是
    // **坏的**（把 COM 的 `VARIANT` 参数误标为 `i32`，本机实测每次调用
    // 恒返回 E_INVALIDARG(0x80070057)/E_FAIL(0x80004005)，且因 ABI 不符
    // 直接 AV）。该 bug 使 F-AUTO-09 的路径校验静默失效、并在每次启动时
    // 刷一条 `autostart sync` 告警。XML 走纯 BSTR 获取（无 ABI 问题），
    // `<Exec><Command>/<Arguments>` 是 Task Scheduler 的稳定结构。
    let mut xml = BSTR::new();
    unsafe {
        def.XmlText(&mut xml)
            .map_err(|e| err_fmt("ITaskDefinition::XmlText", e))?;
    }
    let xml_text = bstr_to_string(&xml);
    let action_command = extract_xml_tag(&xml_text, "Command");
    let action_args = extract_xml_tag(&xml_text, "Arguments");
    let exe = std::env::current_exe()
        .map_err(|e| err_fmt("current_exe", e))?
        .to_string_lossy()
        .to_string();
    let path_matches = match &action_command {
        Some(p) => p.eq_ignore_ascii_case(&exe),
        None => false,
    };
    let args_match = action_args.as_deref() == Some("--autostart");
    if !path_matches || !args_match {
        log::warn!(
            "Autostart task action mismatch (command={:?}, args={:?}, current exe='{}'); needs rebase",
            action_command,
            action_args,
            exe
        );
    }

    // 任务设置：电池供电时不得停止任务、电池下可启动、执行时长无限。
    // 历史版本注册的任务未显式设置这三项（`StopIfGoingOnBatteries` 默认
    // TRUE、`ExecutionTimeLimit` 默认 PT72H），与预期不符时必须重建——
    // 否则升级后拔电/常驻超时仍会被任务计划服务终止。
    let settings = unsafe {
        def.Settings()
            .map_err(|e| err_fmt("ITaskDefinition::Settings", e))?
    };
    let mut stop_on_battery = VARIANT_TRUE;
    let mut disallow_on_battery = VARIANT_TRUE;
    let mut exec_time = BSTR::new();
    unsafe {
        settings
            .StopIfGoingOnBatteries(&mut stop_on_battery)
            .map_err(|e| err_fmt("StopIfGoingOnBatteries", e))?;
        settings
            .DisallowStartIfOnBatteries(&mut disallow_on_battery)
            .map_err(|e| err_fmt("DisallowStartIfOnBatteries", e))?;
        settings
            .ExecutionTimeLimit(&mut exec_time)
            .map_err(|e| err_fmt("ExecutionTimeLimit", e))?;
    }
    let stop_ok = stop_on_battery == VARIANT_FALSE;
    let disallow_ok = disallow_on_battery == VARIANT_FALSE;
    let exec_ok = bstr_to_string(&exec_time) == TASK_EXEC_TIME_LIMIT_DISABLED;
    if !stop_ok || !disallow_ok || !exec_ok {
        log::warn!(
            "Autostart task battery/exec settings stale (stop_on_battery={}, disallow_on_battery={}, exec_time_limit='{}'); needs rebase",
            stop_on_battery == VARIANT_TRUE,
            disallow_on_battery == VARIANT_TRUE,
            bstr_to_string(&exec_time)
        );
        return Ok(false);
    }
    Ok(path_matches && args_match)
}

/// 注册（创建或更新）开机自启动任务。
///
/// 登录触发 + 当前用户交互令牌，普通权限即可注册（无需管理员）。
pub fn enable() -> Result<(), String> {
    let _com = ComScope::init()?;
    let exe = std::env::current_exe().map_err(|e| err_fmt("current_exe", e))?;
    // 路径经 OsStr → UTF-16 直构（不经 to_string_lossy）：注册进计划任务的
    // 必须是真实 Windows 路径——lossy 会把非 UTF-8 的 UTF-16 路径替换成
    // U+FFFD，任务启动时执行错误路径而静默失败（修订 1.46 安全加固，与
    // util::WideString::from_os_str 同源问题）。
    let exe_wide = crate::util::WideString::from_os_str(exe.as_os_str());
    let exe_bstr = windows::core::BSTR::from_wide(exe_wide.units_no_nul());

    let service = task_service()?;
    let folder = task_folder(&service)?;

    let def: ITaskDefinition = unsafe { service.NewTask(0).map_err(|e| err_fmt("NewTask", e))? };

    // 注册信息：名称 + 描述
    unsafe {
        let reg = def
            .RegistrationInfo()
            .map_err(|e| err_fmt("RegistrationInfo", e))?;
        reg.SetDescription(&BSTR::from(TASK_DESC))
            .map_err(|e| err_fmt("SetDescription", e))?;
        reg.SetAuthor(&BSTR::from(crate::util::APP_NAME))
            .map_err(|e| err_fmt("SetAuthor", e))?;
    }

    // 主体：运行级别设为**最高权限**（TASK_RUNLEVEL_HIGHEST）。
    // F-AUTO-02 / AC-AUTO-03：任务须以管理员权限静默启动、不弹 UAC。
    // 计划任务在登录时由 Task Scheduler 服务托管启动，已提权上下文注册的
    // 开放运行时不会触发 UAC 弹窗；若按默认级别（LUA，非管理员）运行，
    // 应用启动时 `elevate_self()` 会每次登录弹出 UAC，违背自启动"驻留托盘
    // 不打扰用户"的设计。
    unsafe {
        let principal = def.Principal().map_err(|e| err_fmt("Principal", e))?;
        principal
            .SetRunLevel(TASK_RUNLEVEL_HIGHEST)
            .map_err(|e| err_fmt("SetRunLevel", e))?;
    }

    // 触发器：登录时（TASK_TRIGGER_LOGON）
    unsafe {
        let triggers = def.Triggers().map_err(|e| err_fmt("Triggers", e))?;
        let trigger: ITrigger = triggers
            .Create(TASK_TRIGGER_LOGON)
            .map_err(|e| err_fmt("Triggers::Create", e))?;
        let logon: ILogonTrigger = trigger
            .cast()
            .map_err(|e| err_fmt("trigger to ILogonTrigger", e))?;
        logon
            .SetUserId(&BSTR::new())
            .map_err(|e| err_fmt("SetUserId", e))?;
    }

    // 动作：执行当前 exe，携带 --autostart
    unsafe {
        let actions = def.Actions().map_err(|e| err_fmt("Actions", e))?;
        let action: IAction = actions
            .Create(TASK_ACTION_EXEC)
            .map_err(|e| err_fmt("Actions::Create", e))?;
        let exec: IExecAction = action
            .cast()
            .map_err(|e| err_fmt("action to IExecAction", e))?;
        exec.SetPath(&exe_bstr).map_err(|e| err_fmt("SetPath", e))?;
        exec.SetArguments(&BSTR::from("--autostart"))
            .map_err(|e| err_fmt("SetArguments", e))?;
    }

    // 任务设置：电池供电时**不得**停止任务（默认 `StopIfGoingOnBatteries`
    // =TRUE，拔电即被任务计划服务终止，正是"电池供电时计划任务终止运行"
    // 的根因），并允许在电池供电时启动（登录即触发、无 AC 依赖）；执行
    // 时长设为无限（默认 `ExecutionTimeLimit=PT72H`，常驻应用运行满 72 小时
    // 会被强制终止）。
    unsafe {
        let settings = def.Settings().map_err(|e| err_fmt("Settings", e))?;
        settings
            .SetStopIfGoingOnBatteries(VARIANT_FALSE)
            .map_err(|e| err_fmt("SetStopIfGoingOnBatteries", e))?;
        settings
            .SetDisallowStartIfOnBatteries(VARIANT_FALSE)
            .map_err(|e| err_fmt("SetDisallowStartIfOnBatteries", e))?;
        settings
            .SetExecutionTimeLimit(&BSTR::from(TASK_EXEC_TIME_LIMIT_DISABLED))
            .map_err(|e| err_fmt("SetExecutionTimeLimit", e))?;
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
            .map_err(|e| err_fmt("RegisterTaskDefinition", e))?;
    }
    log::info!(
        "Autostart task '{}' registered ({} --autostart)",
        TASK_NAME,
        exe.display()
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
        Err(e) => Err(err_fmt("DeleteTask", e)),
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

    /// 回归测试：任务执行时长不限的 ISO-8601 时长必须是 PT0S（Task Scheduler
    /// 约定 0 时长 = 无限）。`enable` 写入、`task_matches` 校验共用该常量，
    /// 锁定其值防止漂移——默认的 PT72H 会在常驻运行满 3 天时被任务计划服务
    /// 强制终止。
    #[test]
    fn test_exec_time_limit_disabled_is_pt0s() {
        assert_eq!(TASK_EXEC_TIME_LIMIT_DISABLED, "PT0S");
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

    /// 从任务 XML 提取 `<Command>`/`<Arguments>` 的单元测试。
    #[test]
    fn test_extract_xml_tag_basic() {
        // Task Scheduler 生成 XML 的典型结构（含缩进与首行编码声明）。
        let xml = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task>
  <Actions Context="Author">
    <Exec>
      <Command>D:\SavedFiles\Tools\MiPcManagerLite\xiaomi-pc-manager-lite.exe</Command>
      <Arguments>--autostart</Arguments>
    </Exec>
  </Actions>
</Task>"#;
        assert_eq!(
            extract_xml_tag(xml, "Command").as_deref(),
            Some(r"D:\SavedFiles\Tools\MiPcManagerLite\xiaomi-pc-manager-lite.exe")
        );
        assert_eq!(
            extract_xml_tag(xml, "Arguments").as_deref(),
            Some("--autostart")
        );
        // 缺失标签 → None。
        assert_eq!(extract_xml_tag(xml, "Missing"), None);
    }

    /// XML 实体转义还原（路径含 `&`/`<`/`"` 等字符时的 Task Scheduler 输出）。
    #[test]
    fn test_extract_xml_tag_unescape() {
        let xml = r#"<Task><Exec><Command>C:\a&amp;b\&lt;x&gt;.exe</Command><Arguments>&quot;--flag&quot;</Arguments></Exec></Task>"#;
        assert_eq!(
            extract_xml_tag(xml, "Command").as_deref(),
            Some(r"C:\a&b\<x>.exe")
        );
        assert_eq!(
            extract_xml_tag(xml, "Arguments").as_deref(),
            Some("\"--flag\"")
        );
    }

    /// CDATA 包裹（部分系统把命令写进 CDATA）。
    #[test]
    fn test_extract_xml_tag_cdata() {
        let xml = r#"<Exec><Command><![CDATA[D:\my path\app.exe]]></Command></Exec>"#;
        assert_eq!(
            extract_xml_tag(xml, "Command").as_deref(),
            Some("D:\\my path\\app.exe")
        );
    }

    /// 自闭合标签（`<Arguments />` 表示空参数）→ None。
    #[test]
    fn test_extract_xml_tag_self_closing() {
        let xml = "<Exec><Command>app.exe</Command><Arguments /></Exec>";
        assert_eq!(extract_xml_tag(xml, "Arguments"), None);
    }

    /// 真机验证（手动运行，非 CI）：`task_matches` 现在经任务定义 XML 读取
    /// 首个动作——本机任务路径与当前 exe 不同（开发路径 vs 部署路径），应
    /// 返回 `Ok(false)`（触发按 F-AUTO-09 重建），且**不再**因
    /// `IActionCollection::get_Item` 的 windows-rs 包装 bug 返回 Err。
    /// 运行：`XIAOMI_LIVE_TASKSCHEDULER_TEST=1 cargo test -- --ignored
    /// task_matches_live_no_error`。
    #[test]
    #[ignore = "live Task Scheduler verification"]
    fn task_matches_live_no_error() {
        if std::env::var_os("XIAOMI_LIVE_TASKSCHEDULER_TEST").is_none() {
            eprintln!("skipping (set XIAOMI_LIVE_TASKSCHEDULER_TEST=1 to run)");
            return;
        }
        let _ = env_logger::builder().is_test(true).try_init();
        let _com = ComScope::init().expect("com init");
        let Some(task) = task_state().expect("task_state") else {
            eprintln!("task not found; skipping");
            return;
        };
        // 关键断言：不再把 get_Item 的 0x80070057 作为错误传播（旧代码每次
        // 启动都会刷 `autostart sync: Actions::get_Item` 告警并使 F-AUTO-09
        // 失效）。返回值是"是否与当前 exe 匹配"的布尔，读取路径必须 Ok。
        match task_matches(&task) {
            Ok(_) => {}
            Err(e) => panic!(
                "task_matches returned Err (get_Item bug regression?): {}",
                e
            ),
        }
    }

    /// 真实环境验证（本机）：注册任务 → 查询存在 → 删除。
    /// 任务注册无需管理员（交互令牌、非最高权限）。
    ///
    /// **破坏性**：`disable()` 会删除机器上已存在的同名任务——若开发者
    /// 实际启用了开机自启动，`cargo test` 会静默删除其真实任务。必须显式
    /// 设置 `XIAOMI_LIVE_TASKSCHEDULER_TEST=1` 且传入 `-- --ignored` 才运行。
    /// 标记 `#[ignore]` 与同模块的 `task_matches_live_no_error` 一致：仅靠
    /// env 门控不够——开发 shell 若恰好导出该变量，普通 `cargo test` 会静默
    /// 删除真实任务（修订 1.47 审计）。
    #[test]
    #[ignore = "destructive live Task Scheduler roundtrip"]
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
