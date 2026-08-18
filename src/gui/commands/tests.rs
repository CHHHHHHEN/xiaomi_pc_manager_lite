use super::*;

use crate::app::config::AppConfig;
use crate::app::ec::EcError;
use crate::ec::mock::MockBackend;

/// 每个用例独立的临时配置目录：save_state() 永不触碰用户的真实配置。
fn test_store() -> crate::app::config::ConfigStore {
    crate::testutil::temp_store("test")
}

/// 回归测试：当前后端已是 WMI（Auto 探测的必然结果）时，切换到 Auto 必须
/// 是 no-op——历史实现会 create_backend(Auto) 重建一个 WMI 代理（每次请求
/// 都多一次完整连接握手），此处校验不触发任何重建。
#[test]
fn test_try_switch_backend_auto_when_already_wmi_is_noop() {
    let store = test_store();
    let mut app = XiaomiApp::new(
        store,
        Box::new(MockBackend::all_fail("pref-wmi", BackendPreference::Wmi)),
        AppConfig::default(),
        BackendPreference::Wmi,
        None,
        false,
    );
    // 构造时的启动刷新会产生读取错误，先清空以便聚焦本用例。
    app.error_msg = None;
    let backend_before = app.backend.name();

    let ok = app.try_switch_backend(BackendPreference::Auto);
    assert!(ok, "Auto switch on an already-WMI backend must succeed");
    assert_eq!(
        app.backend.name(),
        backend_before,
        "backend must not be recreated"
    );
    assert_eq!(app.current_pref, BackendPreference::Auto);
    assert_eq!(app.config.backend, BackendPreference::Auto);
    assert!(
        app.error_msg.is_none(),
        "no-op switch must not produce errors"
    );
}

/// 回归测试：当前后端是 WinRing0 时切换到 Auto。WMI 不可用时必须**保留**
/// 现有 WinRing0 后端而不是重建——重建会创建新实例后再 drop 旧实例，
/// 触发 DeinitializeOls 卸载驱动，使新 WinRing0 后端的端口读写全部失效
/// （只能重启恢复）。WMI 可用时按 Auto 语义切到 WMI。两条路径都不得
/// 留下损坏的后端或产生错误。后端创建已在后台线程（修订 1.36 异步化），
/// 测试需等待结果经命令通道送达。
#[test]
fn test_try_switch_backend_auto_from_winring0_keeps_or_switches() {
    let store = test_store();
    let mut app = XiaomiApp::new(
        store,
        Box::new(MockBackend::all_fail(
            "pref-winring0",
            BackendPreference::WinRing0,
        )),
        AppConfig::default(),
        BackendPreference::WinRing0,
        None,
        false,
    );
    app.error_msg = None;
    let backend_before = app.backend.name();
    let ctx = egui::Context::default();

    let ok = app.try_switch_backend(BackendPreference::Auto);
    assert!(ok, "Auto switch must be accepted");
    // 后台创建结果经 BackendSwitchResult 送达。**不能**先 try_recv 再
    // process_commands——try_recv 会把消息消费掉、process 时通道已空，
    // 切换结果静默丢失。改为反复 process_commands 直到 pending 被消费
    // 清除（= 结果已应用/确认）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        app.process_commands(&ctx);
        if app.pending_backend_switch.is_none() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "backend switch result not delivered within 15s"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if app.backend.name() == backend_before {
        // WMI 不可用：现有 WinRing0 后端必须被原样保留（未被重建）。
        assert_eq!(app.backend.preference(), BackendPreference::WinRing0);
    } else {
        // WMI 可用：Auto 优先 WMI，应切换到 WMI 后端。
        assert_eq!(app.backend.preference(), BackendPreference::Wmi);
    }
    assert_eq!(app.current_pref, BackendPreference::Auto);
    assert_eq!(app.config.backend, BackendPreference::Auto);
    assert!(app.error_msg.is_none(), "switch must not leave errors");
}

/// 回归测试：后端切换结果过期（发起后用户改选/确认了其它后端）时必须丢弃，
/// 不得把用户已放弃的切换应用到活动后端。修订 1.36 异步化的 pending
/// 令牌语义：`pending_backend_switch != Some(user_pref)` 即过期。
#[test]
fn test_backend_switch_result_stale_dropped() {
    let store = test_store();
    let mut app = XiaomiApp::new(
        store,
        Box::new(MockBackend::all_fail("active", BackendPreference::WinRing0)),
        AppConfig::default(),
        BackendPreference::WinRing0,
        None,
        false,
    );
    app.error_msg = None;
    let backend_before = app.backend.name();
    // 模拟：发起 Auto 切换后用户又切到 WinRing0（pending 变为 WinRing0）。
    app.pending_backend_switch = Some(BackendPreference::WinRing0);

    app.handle_backend_switch_result(
        BackendPreference::Auto,
        Ok(Box::new(MockBackend::all_fail(
            "late",
            BackendPreference::Wmi,
        ))),
    );
    assert_eq!(
        app.backend.name(),
        backend_before,
        "stale result must not replace the active backend"
    );
    assert_eq!(
        app.pending_backend_switch,
        Some(BackendPreference::WinRing0),
        "pending must survive a stale result"
    );
    assert_eq!(app.current_pref, BackendPreference::WinRing0);
}

/// 单飞回归（修订 1.39）：一个后端切换**创建中**时拒绝新的切换请求——
/// 两个并发 `create_backend(WinRing0)` 会让后到的结果被丢弃时执行
/// DeinitializeOls 卸载驱动，拆掉正在使用的后端（只能重启恢复）。
#[test]
fn test_backend_switch_rejected_while_pending() {
    let store = test_store();
    let mut app = XiaomiApp::new(
        store,
        Box::new(MockBackend::all_fail("active-wmi", BackendPreference::Wmi)),
        AppConfig::default(),
        BackendPreference::Wmi,
        None,
        false,
    );
    app.error_msg = None;
    app.pending_backend_switch = Some(BackendPreference::WinRing0);

    let ok = app.try_switch_backend(BackendPreference::WinRing0);
    assert!(
        !ok,
        "switch must be rejected while another switch is pending"
    );
    assert_eq!(
        app.pending_backend_switch,
        Some(BackendPreference::WinRing0),
        "pending must be preserved on rejection"
    );
    assert_eq!(app.backend.preference(), BackendPreference::Wmi);
}

/// 单飞与 no-op 的交互（修订 1.39）：pending 期间用户点击"当前已激活的
/// 后端"应清空 pending（表示放弃进行中的切换）——随后的 in-flight 结果
/// 因过期被丢弃，用户停留在当前后端。
#[test]
fn test_backend_noop_confirm_clears_pending() {
    let store = test_store();
    let mut app = XiaomiApp::new(
        store,
        Box::new(MockBackend::all_fail("active", BackendPreference::Wmi)),
        AppConfig::default(),
        BackendPreference::Wmi,
        None,
        false,
    );
    app.error_msg = None;
    app.pending_backend_switch = Some(BackendPreference::WinRing0);

    let ok = app.try_switch_backend(BackendPreference::Wmi);
    assert!(ok, "no-op confirm must succeed");
    assert_eq!(
        app.pending_backend_switch, None,
        "no-op confirm must cancel an in-flight switch"
    );
    assert_eq!(app.current_pref, BackendPreference::Wmi);
}

/// 回归测试：请求切换到"当前已经激活的同种后端"必须是 no-op。
/// 历史实现会重建后端：WinRing0 的重建路径先 cleanup_service 停/删当前
/// 驱动服务，若后续 InitializeOls 失败，正在工作的后端立即失效。
/// no-op 分支不创建新后端（不触碰真实硬件），因此这里的 WMI 偏好切换
/// 必须返回 true 且后端实例保持不变。
#[test]
fn test_try_switch_backend_same_kind_is_noop() {
    let store = test_store();
    let mut app = XiaomiApp::new(
        store,
        Box::new(MockBackend::all_fail("pref-wmi", BackendPreference::Wmi)),
        AppConfig::default(),
        BackendPreference::Wmi,
        None,
        false,
    );
    // 构造时的启动刷新会产生读取错误，先清空以便聚焦本用例。
    app.error_msg = None;
    let backend_before = app.backend.name();

    let ok = app.try_switch_backend(BackendPreference::Wmi);
    assert!(ok, "same-kind switch must be a no-op that succeeds");
    assert_eq!(
        app.backend.name(),
        backend_before,
        "backend must not be recreated"
    );
    assert_eq!(app.current_pref, BackendPreference::Wmi);
    assert_eq!(app.config.backend, BackendPreference::Wmi);
    assert!(
        app.error_msg.is_none(),
        "no-op switch must not produce errors"
    );
}

fn failing_app() -> XiaomiApp {
    XiaomiApp::new(
        test_store(),
        Box::new(MockBackend::all_fail("failing", BackendPreference::Auto)),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    )
}

fn test_app() -> XiaomiApp {
    XiaomiApp::new(
        test_store(),
        Box::new(MockBackend::default()),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    )
}

#[test]
fn test_perf_mode_selection_is_preserved_on_battery() {
    let mut app = test_app();
    // 电池供电下选择狂暴：硬件按电源状态写入降级值（极速），但
    // config 保留用户选择的狂暴（插电/重启后恢复）；runtime 是"硬件
    // 当前认知"，必须与实际写入一致（修订 1.25 回归测试：历史实现把
    // runtime 存成用户选择，GUI/托盘谎报狂暴而硬件实为极速）。
    app.set_perf_mode_internal(PerfMode::Extreme);
    let applied = app::battery::effective_perf_for_power(
        PerfMode::Extreme.ec_value(),
        crate::platform::power::power_status(),
    );
    assert_eq!(app.runtime.performance_mode, applied);
    assert_eq!(app.config.performance_mode, PerfMode::Extreme as u8);
}

/// 托盘子菜单直接指定模式：SetPerfMode 命令按值切换；未知值安全忽略。
#[test]
fn test_set_perf_mode_command_direct_and_invalid() {
    let mut app = test_app();
    let ctx = egui::Context::default();

    app.cmd_tx
        .send(UiCommand::SetPerfMode(PerfMode::Quiet as u8))
        .unwrap();
    app.process_commands(&ctx);
    assert_eq!(app.runtime.performance_mode, PerfMode::Quiet as u8);
    assert_eq!(app.config.performance_mode, PerfMode::Quiet as u8);

    // 未知模式（如损坏的 0xFF）：忽略，不改变当前状态。
    app.cmd_tx.send(UiCommand::SetPerfMode(0xFF)).unwrap();
    app.process_commands(&ctx);
    assert_eq!(app.runtime.performance_mode, PerfMode::Quiet as u8);
    assert!(app.error_msg.is_none(), "invalid mode must not error");
}

/// 已知模式下 CyclePerfMode 命令按循环序列推进（Smart → Quiet）。
#[test]
fn test_cycle_perf_mode_known_progresses() {
    let mut app = test_app();
    let ctx = egui::Context::default();
    app.runtime.performance_mode = PerfMode::Smart as u8;
    app.cmd_tx.send(UiCommand::CyclePerfMode).unwrap();
    app.process_commands(&ctx);
    assert_eq!(app.runtime.performance_mode, PerfMode::Quiet as u8);
    assert_eq!(app.config.performance_mode, PerfMode::Quiet as u8);
}

/// 回归测试（修订 1.45）：CyclePerfMode 遇到未知当前模式（硬件读回未定义
/// 代码，如损坏/未初始化 EC 数据）时按循环领域语义回到首项 Smart——
/// 历史实现静默把未知当成 Smart 再取下一项（写出 Quiet），既违反循环
/// 契约又无日志。修复后显式告警并直接写 Smart。
#[test]
fn test_cycle_perf_mode_unknown_current_writes_smart() {
    let mut app = test_app();
    let ctx = egui::Context::default();
    // 模拟硬件读回未定义模式。
    app.runtime.performance_mode = 0xFF;
    app.cmd_tx.send(UiCommand::CyclePerfMode).unwrap();
    app.process_commands(&ctx);
    assert_eq!(
        app.runtime.performance_mode,
        PerfMode::Smart as u8,
        "unknown current mode must cycle to the first cycle item (Smart), not the second (Quiet)"
    );
    assert_eq!(app.config.performance_mode, PerfMode::Smart as u8);
    // 该路径写回 Smart 是确定性结果，不产生用户可见错误。
    assert!(app.error_msg.is_none());
}

/// 回归测试（F-PWR-04，修订 1.33/1.45）：电源/后台刷新
/// `refresh_from_backend` 改写 `runtime.charge_limit` 时**不得**碰触滑块
/// 拖动中的工作值 `charge_limit_drag`——否则拖动途中被后台刷新"拽回"
/// 跳变（历史回归：离电瞬间滑块数值跳变）。
#[test]
fn test_refresh_does_not_clobber_slider_drag_value() {
    let mut app = test_app();
    app.runtime.charge_limit = 80;
    // 模拟拖动进行中：工作值已持久到 self。
    app.charge_limit_drag = Some(42);
    app.refresh_from_backend();
    assert_eq!(
        app.charge_limit_drag,
        Some(42),
        "in-flight slider drag value must survive a backend refresh"
    );
}

/// 回归测试（F-GUI-21，修订 1.33/1.45）：托盘"退出"命令置位 quitting
///（使下一帧 close_requested 放行、eframe 正常退出并执行各组件 Drop
/// 清理）+ 退出前兜底保存一次配置。
#[test]
fn test_quit_command_sets_flag_and_saves() {
    let mut app = test_app();
    let ctx = egui::Context::default();
    app.cmd_tx.send(UiCommand::Quit).unwrap();
    app.process_commands(&ctx);
    assert!(app.quitting, "Quit must open the exit door (quitting flag)");
    // 兜底保存路径正常（无错误展示）；save_state 失败会经 error_msg 暴露。
    assert!(
        app.error_msg.is_none(),
        "Quit must not produce an error on the normal path"
    );
}

#[test]
fn test_perf_mode_normal_selection_not_changed() {
    let mut app = test_app();
    app.set_perf_mode_internal(PerfMode::Quiet);
    assert_eq!(app.runtime.performance_mode, PerfMode::Quiet as u8);
    assert_eq!(app.config.performance_mode, PerfMode::Quiet as u8);
}

/// 回归测试（M3）：开机自启动请求必须**即时持久化**期望值，而不是等
/// worker 结果回传才写配置——否则任务注册完成、应用在结果到达前退出时，
/// 配置保持旧值而计划任务已是新状态，下次启动 sync 不删任务，任务永久
/// 残留（与配置矛盾）。请求即落盘使中途退出时配置 = 用户最终意图。
#[test]
fn test_set_autostart_persists_requested_state_immediately() {
    let mut app = test_app();
    // 与 UiCommand::SetAutostart 等价的处理路径（persist_autostart_request，
    // 不触发真实 worker）：请求后 config 必须立即反映新值并落盘。
    app.persist_autostart_request(true);
    assert!(
        app.config.auto_start_on_boot,
        "config must reflect the request immediately"
    );
    // 结果成功：不再重复写回，但状态保持。
    let ctx = egui::Context::default();
    app.cmd_tx
        .send(UiCommand::SetAutostartResult(true, Ok(())))
        .unwrap();
    app.process_commands(&ctx);
    assert!(app.config.auto_start_on_boot);
    assert!(app.error_msg.is_none(), "success must not error");

    // 关闭并请求回滚：enable 失败时复选框必须回滚为未勾选（F-AUTO-10）。
    app.persist_autostart_request(false);
    assert!(!app.config.auto_start_on_boot);
    app.cmd_tx
        .send(UiCommand::SetAutostartResult(false, Ok(())))
        .unwrap();
    app.process_commands(&ctx);
    assert!(!app.config.auto_start_on_boot);
    app.persist_autostart_request(true);
    app.cmd_tx
        .send(UiCommand::SetAutostartResult(true, Err("测试失败".into())))
        .unwrap();
    app.process_commands(&ctx);
    assert!(
        !app.config.auto_start_on_boot,
        "enable failure must revert checkbox to unchecked"
    );
    let err = app.error_msg.as_deref().unwrap_or_default();
    assert!(err.contains("设置开机自启动失败"), "unexpected: {}", err);
}

/// 回归测试（修订 1.45 审计 + 1.46 自愈）：worker 通道已死（enable/disable
/// 内部 panic 等）时请求投递失败——enable 请求必须回滚勾选并展示错误
/// （任务不会创建，配置不得停留在"已勾选"），disable 请求保持用户选择
/// （F-AUTO-03）但也必须展示错误（任务未删除、开机仍会自启，静默背离
/// 不可接受）。**1.46 改进**：失败后重置 `autostart_worker` 槽位——死
/// worker 不滞留，下一次请求重建 worker 自愈（此前本会话内自启动永久
/// 失效，唯一恢复途径是重启应用）。
#[test]
fn test_set_autostart_dead_worker_reverts_and_errors() {
    let mut app = test_app();
    // 构造通道对端已 drop 的"死 worker"。
    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    drop(rx);
    app.autostart_worker = Some(tx);

    // enable 请求：配置回滚为未勾选 + 错误展示 + 槽位重置（可自愈）。
    app.set_autostart(true);
    assert!(
        !app.config.auto_start_on_boot,
        "enable with dead worker must revert config to unchecked"
    );
    let err = app.error_msg.as_deref().unwrap_or_default();
    assert!(err.contains("设置开机自启动失败"), "unexpected: {}", err);
    assert!(
        app.autostart_worker.is_none(),
        "dead worker slot must be cleared so the next request can self-heal"
    );

    // 再次制造死 worker，验证 disable 路径：配置保持用户选择（关）+
    // 错误展示（任务未删除）。
    let (tx2, rx2) = std::sync::mpsc::channel::<bool>();
    drop(rx2);
    app.autostart_worker = Some(tx2);
    app.error_msg = None;
    app.set_autostart(false);
    assert!(
        !app.config.auto_start_on_boot,
        "disable with dead worker keeps user's choice (off)"
    );
    let err = app.error_msg.as_deref().unwrap_or_default();
    assert!(err.contains("设置开机自启动失败"), "unexpected: {}", err);
    assert!(app.autostart_worker.is_none());
}

/// 回归测试（修订 1.47）：死 worker 的 send 失败路径必须**清零在飞计数**。
/// 历史实现只重置 `autostart_worker` 槽位，`autostart_in_flight` 若残留
/// （worker 处理中途 panic 时无回执可减），之后重建的 worker 结果回传时
/// `is_latest` 恒 false——enable 失败回滚本会话内永久失效。
#[test]
fn test_autostart_dead_worker_resets_in_flight_counter() {
    let mut app = test_app();
    // 模拟残留计数：某个 worker 在回执前 panic，计数泄漏。
    app.autostart_in_flight = 3;
    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    drop(rx);
    app.autostart_worker = Some(tx);

    // 死 worker 发送失败 → fail_autostart_operation 必须清零计数。
    app.set_autostart(true);
    assert_eq!(
        app.autostart_in_flight, 0,
        "stale in-flight must be drained"
    );
    assert!(app.autostart_worker.is_none());
}

/// 回归测试（F2/1.1）：**过期的**失败结果不得覆盖更新的用户意图。
/// 串行 worker 中先发的 enable#1 失败结果可能晚于 disable#2 已落盘之后
/// 到达——此时配置反映的是更新的意图（关），回滚会把它覆盖回旧值，
/// 重新制造"任务在而配置关"的背离。
#[test]
fn test_autostart_stale_failure_does_not_revert_latest_intent() {
    let mut app = test_app();
    let ctx = egui::Context::default();

    // 快速连点 ON→OFF：先请求 enable（落盘 true），随即请求 disable
    // （落盘 false）。enable 失败结果**迟到**。
    app.persist_autostart_request(true);
    app.persist_autostart_request(false);
    assert!(!app.config.auto_start_on_boot, "latest intent is off");

    // enable#1 失败结果迟到：不得回滚（config 已是最新意图 false）。
    app.cmd_tx
        .send(UiCommand::SetAutostartResult(
            true,
            Err("迟到的失败".into()),
        ))
        .unwrap();
    app.process_commands(&ctx);
    assert!(
        !app.config.auto_start_on_boot,
        "stale failure must not revert the newer OFF intent"
    );

    // 反向：ON 已是最新意图时，迟到的 disable 失败不得把配置回滚成关。
    app.persist_autostart_request(true);
    app.cmd_tx
        .send(UiCommand::SetAutostartResult(
            false,
            Err("迟到的失败".into()),
        ))
        .unwrap();
    app.process_commands(&ctx);
    assert!(
        app.config.auto_start_on_boot,
        "stale disable failure must not revert the newer ON intent"
    );
}

/// 回归测试（修订 1.44，评审第 5 轮）：ON→OFF→ON 快速连点时，**旧**的
/// enable 失败结果不得回滚**新**的 ON 意图。历史实现的回滚判定只看
/// `enabled && config.auto_start_on_boot`——配置在每次请求时即时落盘，
/// ON#3 已把 config 改成 true，enable#1 的迟到失败会据此误判"这是最新
/// 意图"而回滚成 false，重新制造"任务在而配置关"的背离。修复：在飞
/// 请求计数归零才允许回滚（本结果对应最新请求）。
#[test]
fn test_autostart_on_off_on_stale_enable_failure_keeps_latest_on() {
    let mut app = test_app();
    let ctx = egui::Context::default();

    // 模拟串行 worker 的请求流（直接操作计数器与配置，不触发真实 worker）：
    // ON#1 → OFF#2 → ON#3，三个请求都在 worker 排队，各自落盘期望值。
    app.persist_autostart_request(true);
    app.autostart_in_flight = 1;
    app.persist_autostart_request(false);
    app.autostart_in_flight = 2;
    app.persist_autostart_request(true);
    app.autostart_in_flight = 3;
    assert!(app.config.auto_start_on_boot, "latest intent is ON");

    // enable#1 的**迟到失败**回传：计数 3→2 未归零，本结果不是最新请求，
    // 不得回滚——config 必须保持 ON#3 的意图。
    app.cmd_tx
        .send(UiCommand::SetAutostartResult(
            true,
            Err("enable#1 迟到失败".into()),
        ))
        .unwrap();
    app.process_commands(&ctx);
    assert!(
        app.config.auto_start_on_boot,
        "stale enable#1 failure must not revert ON#3 (latest intent)"
    );

    // OFF#2 结果回传：3→1 仍未归零，同样不得回滚（且 disable 本就不回滚）。
    app.cmd_tx
        .send(UiCommand::SetAutostartResult(false, Ok(())))
        .unwrap();
    app.process_commands(&ctx);
    assert!(app.config.auto_start_on_boot);

    // ON#3 结果成功回传：计数归零，确认最终状态仍为 ON。
    app.cmd_tx
        .send(UiCommand::SetAutostartResult(true, Ok(())))
        .unwrap();
    app.process_commands(&ctx);
    assert!(app.config.auto_start_on_boot);
}

/// 回归测试（修订 1.32/F-AUTO-03）：**删除失败不得回滚配置**——取消勾选
/// 后任务删除失败（临时权限/占用）时，配置必须仍按用户选择保存（关），
/// 仅在 GUI 展示错误。历史实现把 disable 失败回滚为勾选（true），与需求
/// 文档"删除失败时配置仍按用户选择保存"直接冲突，用户明确要关闭却因
/// 临时失败被翻回开启。
#[test]
fn test_autostart_disable_failure_keeps_user_choice() {
    let mut app = test_app();
    let ctx = egui::Context::default();

    app.persist_autostart_request(false);
    assert!(!app.config.auto_start_on_boot);
    app.cmd_tx
        .send(UiCommand::SetAutostartResult(false, Err("删除失败".into())))
        .unwrap();
    app.process_commands(&ctx);
    assert!(
        !app.config.auto_start_on_boot,
        "disable failure must keep config at user's choice (off)"
    );
    let err = app.error_msg.as_deref().unwrap_or_default();
    assert!(err.contains("设置开机自启动失败"), "unexpected: {}", err);
}

/// 回归测试（修订 1.25 M3）：开启养护 → 关闭养护（上限写 100%）→ 再开启，
/// 关闭期间持久化的期望上限必须保留（不被硬件读回的 100% 覆盖），
/// 重新开启时恢复到用户期望值。
#[test]
fn test_battery_care_toggle_preserves_desired_limit() {
    let mut app = test_app();
    app.config.battery_charge_limit = 60;

    app.set_battery_care_internal(true);
    assert!(app.runtime.battery_care_enabled);
    assert_eq!(app.runtime.charge_limit, 60);
    assert_eq!(app.config.battery_charge_limit, 60);

    // Disabling raises the hardware limit to 100% but must keep the
    // desired limit so it is not lost.
    app.set_battery_care_internal(false);
    assert!(!app.runtime.battery_care_enabled);
    assert_eq!(app.runtime.charge_limit, 100);
    assert_eq!(app.config.battery_charge_limit, 60);

    // Re-enabling must restore the desired limit, not the 100% hardware
    // limit left behind by the disable.
    app.set_battery_care_internal(true);
    assert!(app.runtime.battery_care_enabled);
    assert_eq!(app.runtime.charge_limit, 60);
    assert_eq!(app.config.battery_charge_limit, 60);
}

#[test]
fn test_battery_care_enable_falls_back_to_80_when_limit_is_100() {
    let mut app = test_app();
    app.config.battery_charge_limit = 100;

    app.set_battery_care_internal(true);
    assert!(app.runtime.battery_care_enabled);
    assert_eq!(app.runtime.charge_limit, 80);
    assert_eq!(app.config.battery_charge_limit, 80);
}

#[test]
fn test_charge_limit_syncs_battery_care_flag() {
    let mut app = test_app();
    app.runtime.battery_care_enabled = true;

    // 100% limit means battery care is off.
    app.set_charge_limit_internal(100);
    assert!(!app.runtime.battery_care_enabled);
    assert_eq!(app.runtime.charge_limit, 100);
    // 关闭养护时保留用户期望值（默认 80），供重新开启养护时恢复。
    assert_eq!(app.config.battery_charge_limit, 80);

    // A limit below 100% turns battery care back on.
    app.set_charge_limit_internal(90);
    assert!(app.runtime.battery_care_enabled);
    assert_eq!(app.runtime.charge_limit, 90);
    assert_eq!(app.config.battery_charge_limit, 90);
}

/// 回归测试：把上限拖到 100%（养护关闭）不得把 config 中用户期望的上限
/// 覆盖为 100%。历史实现无条件写回 config.battery_charge_limit=applied，
/// 用户从 60% 拖到 100% 后期望值被永久改写为 100，重新开启养护时只能走
/// "≥100 兜底 80%"分支，60% 的原设置丢失——与 set_battery_care_internal
/// 关闭路径（保留期望上限）和 sync_startup_config 的约定不一致。
#[test]
fn test_charge_limit_to_100_preserves_desired_limit() {
    let mut app = test_app();
    // 用户养护开启、期望上限 60%。
    app.config.battery_charge_limit = 60;
    app.runtime.charge_limit = 60;
    app.runtime.battery_care_enabled = true;

    // 拖到 100%：硬件提升到 100（养护关闭），但 config 期望值保留 60。
    app.set_charge_limit_internal(100);
    assert!(!app.runtime.battery_care_enabled);
    assert_eq!(app.runtime.charge_limit, 100);
    assert_eq!(
        app.config.battery_charge_limit, 60,
        "desired limit must be preserved"
    );

    // 重新开启养护：恢复 60% 而不是兜底 80%。
    app.set_battery_care_internal(true);
    assert!(app.runtime.battery_care_enabled);
    assert_eq!(app.runtime.charge_limit, 60);
    assert_eq!(app.config.battery_charge_limit, 60);
}

/// 回归测试：运行时养护状态与持久化配置不一致（auto_apply 关闭且硬件
/// 状态被外部改动时，refresh_from_backend 只更新运行时）时，拖动上限后
/// 持久化配置必须与限值保持自洽。否则下次启动 apply_startup_config 会
/// 按 care=false 强制写 100%，静默摧毁用户设置的充电上限。
#[test]
fn test_charge_limit_sync_persists_care_when_runtime_diverged() {
    let store = test_store();
    let mock = MockBackend::default();
    let mut app = XiaomiApp::new(
        store,
        Box::new(mock.clone()),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    // 模拟：硬件养护已开启（limit=60），持久化配置仍是旧值
    // care=false, limit=100（如 auto_apply 关闭时外部改动硬件）。
    mock.charge_limit
        .store(60, std::sync::atomic::Ordering::Relaxed);
    mock.battery_care
        .store(true, std::sync::atomic::Ordering::Relaxed);
    app.refresh_from_backend();
    assert!(app.runtime.battery_care_enabled);
    assert!(!app.config.battery_care_enabled);

    // 用户把上限拖到 80：运行时 care 未变化（无写入分支），但持久化
    // 配置必须同步为 care=true, limit=80，保持自洽。
    app.set_charge_limit_internal(80);
    assert!(app.config.battery_care_enabled);
    assert_eq!(app.config.battery_charge_limit, 80);

    // 模拟下次启动 apply_startup_config：care=true → 写回 80%，
    // 用户设置不会被 100% 覆盖。
    assert_eq!(
        mock.charge_limit.load(std::sync::atomic::Ordering::Relaxed),
        80
    );
}

#[test]
fn test_refresh_from_backend_keeps_config_untouched() {
    let store = test_store();
    let mock = MockBackend::default();
    let mut app = XiaomiApp::new(
        store,
        Box::new(mock.clone()),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    app.config.battery_charge_limit = 60;
    app.config.battery_care_enabled = false;

    // Mock backend reports a different hardware state than the config.
    mock.battery_care
        .store(true, std::sync::atomic::Ordering::Relaxed);
    mock.charge_limit
        .store(80, std::sync::atomic::Ordering::Relaxed);
    mock.perf_mode
        .store(PerfMode::Quiet as u8, std::sync::atomic::Ordering::Relaxed);

    app.refresh_from_backend();
    assert!(app.runtime.battery_care_enabled);
    assert_eq!(app.runtime.charge_limit, 80);
    assert_eq!(app.runtime.performance_mode, PerfMode::Quiet as u8);
    assert!(app.error_msg.is_none());

    // The persisted desired settings must not be overwritten.
    assert_eq!(app.config.battery_charge_limit, 60);
    assert!(!app.config.battery_care_enabled);
}

/// NFR-REL-03：连续硬件读取失败达到阈值后，GUI 错误提示必须包含
/// "连续读取失败 N 次"（驱动失效/EC 掉线等持续故障不再静默无限重试）；
/// 任意一次成功读取清零计数并移除持久提示。措辞为"连续失败 N 次"
/// 而非"已暂停自动重试"（修订 1.33：刷新由用户/启动/电源事件触发，
/// 不存在周期性自动重试循环，故"暂停"是误导）。
#[test]
fn test_consecutive_failures_pause_retry_and_reset_on_success() {
    let store = test_store();
    let mock = MockBackend::all_fail("hw-pause", BackendPreference::Auto);
    let mut app = XiaomiApp::new(
        store,
        Box::new(mock.clone()),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    // `XiaomiApp::new()` 已执行一次初始刷新：failing 后端下计 1 次失败。
    assert_eq!(app.consecutive_read_failures, 1);

    // 再失败一次（累计 2）：错误展示但不带"连续失败"提示。
    app.refresh_from_backend();
    assert_eq!(app.consecutive_read_failures, 2);
    let msg = app.error_msg.as_deref().unwrap_or_default();
    assert!(!msg.contains("连续读取失败"), "before threshold: {}", msg);

    // 再失败一次（累计 3）：达到阈值，错误消息必须带连续失败提示。
    app.refresh_from_backend();
    assert_eq!(app.consecutive_read_failures, 3);
    let msg = app.error_msg.as_deref().unwrap_or_default();
    assert!(msg.contains("连续读取失败"), "at threshold: {}", msg);

    // 一次成功读取：计数清零、持久提示移除。
    mock.read_fails
        .store(false, std::sync::atomic::Ordering::Relaxed);
    app.refresh_from_backend();
    assert_eq!(app.consecutive_read_failures, 0);
    assert!(app.error_msg.is_none());
}

/// 回归测试：损坏的 EC 读值（充电上限 >100，如寄存器返回 0xFF）不得
/// 显示为荒谬百分比或使 UI 状态溢出——刷新时必须钳制到 100%。
///
/// 注意（修订 1.46 契约对齐后）：真实后端（winring0/wmi）与 mock 都已在
/// get_battery_state **自身**把 0/>100 判为 Err 返回，refresh 的钳制是
/// **纵深防御**（未来某个后端未做读回校验时的兜底）。mock 已无法注入
/// Ok(>100)，用内联"垃圾后端"显式构造该防御路径，保持测试真实覆盖
/// （历史 mock 曾返回 Ok(150) 触发钳制，改契约后测试落入 Err 分支、
/// 断言静默通过却不再覆盖钳制代码）。
#[test]
fn test_refresh_clamps_charge_limit_above_100() {
    struct GarbageBackend;
    impl crate::ec::backend::EcBackend for GarbageBackend {
        fn name(&self) -> &'static str {
            "garbage"
        }
        fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
            Ok(false)
        }
        fn get_charge_limit(&self) -> Result<u8, EcError> {
            Ok(150)
        }
        fn get_battery_state(&self) -> Result<(bool, u8), EcError> {
            Ok((false, 150))
        }
        fn set_battery_care(&self, _enabled: bool) -> Result<(), EcError> {
            Ok(())
        }
        fn set_charge_limit(&self, _percent: u8) -> Result<(), EcError> {
            Ok(())
        }
        fn get_performance_mode(&self) -> Result<u8, EcError> {
            Ok(0x09)
        }
        fn set_performance_mode(&self, _mode: u8) -> Result<(), EcError> {
            Ok(())
        }
    }
    let store = test_store();
    let mut app = XiaomiApp::new(
        store,
        Box::new(GarbageBackend),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );

    app.refresh_from_backend();

    assert_eq!(app.runtime.charge_limit, 100);
    assert!(!app.runtime.battery_care_enabled);
}

/// 回归测试（M5）：读回 care=true + limit>100（垃圾值场景）时，钳制后
/// 必须以**上限**为权威重新推导养护位，杜绝"电池养护: 开启 · 充电上限:
/// 100%"的矛盾组合展示。历史实现把 care 原样写入 runtime，钳制仅作用
/// 于 limit，两个字段对同一硬件状态给出相反含义。
///
/// 同 `test_refresh_clamps_charge_limit_above_100`：mock 已把 0/>100 判为
/// Err（真实契约），用内联垃圾值后端显式构造该纵深防御场景。
#[test]
fn test_refresh_rebases_care_from_clamped_limit() {
    struct GarbageCareBackend;
    impl crate::ec::backend::EcBackend for GarbageCareBackend {
        fn name(&self) -> &'static str {
            "garbage-care"
        }
        fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
            Ok(true)
        }
        fn get_charge_limit(&self) -> Result<u8, EcError> {
            Ok(150)
        }
        fn get_battery_state(&self) -> Result<(bool, u8), EcError> {
            Ok((true, 150))
        }
        fn set_battery_care(&self, _enabled: bool) -> Result<(), EcError> {
            Ok(())
        }
        fn set_charge_limit(&self, _percent: u8) -> Result<(), EcError> {
            Ok(())
        }
        fn get_performance_mode(&self) -> Result<u8, EcError> {
            Ok(0x09)
        }
        fn set_performance_mode(&self, _mode: u8) -> Result<(), EcError> {
            Ok(())
        }
    }
    let store = test_store();
    let mut app = XiaomiApp::new(
        store,
        Box::new(GarbageCareBackend),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    // 垃圾值场景：care 位=true 但上限 150%（读回 0xFF 之类）。

    app.refresh_from_backend();

    assert_eq!(app.runtime.charge_limit, 100);
    assert!(
        !app.runtime.battery_care_enabled,
        "care must be rebased from clamped limit (100 => care off)"
    );
}

/// 回归测试（B-WMI-1）：刷新必须通过 get_battery_state 单次往返获取电池
/// 数据。旧实现分别调用 get_battery_care_enabled + get_charge_limit，
/// 在 WMI 后端下每次刷新做两次请求相同数据的完整 WMI 往返。
#[test]
fn test_refresh_uses_single_battery_roundtrip() {
    let store = test_store();
    let mock = MockBackend::default();
    let calls = mock.battery_state_calls.clone();
    let mut app = XiaomiApp::new(
        store,
        Box::new(mock.clone()),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    // 构造时已刷新过一次，清零后验证显式刷新只发一次电池往返。
    calls.store(0, std::sync::atomic::Ordering::Relaxed);
    mock.battery_care
        .store(true, std::sync::atomic::Ordering::Relaxed);
    mock.charge_limit
        .store(80, std::sync::atomic::Ordering::Relaxed);

    app.refresh_from_backend();

    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(app.runtime.battery_care_enabled);
    assert_eq!(app.runtime.charge_limit, 80);
    assert!(app.error_msg.is_none());
}

/// 回归测试：切换后端后，新后端读取失败产生的错误必须保留在 GUI 中展示
/// （F-ERR-03），不得被切换逻辑清空（曾因 refresh 后无条件 error_msg=None
/// 导致切换到一个读取全部失败的后端时错误信息被立即抹掉）。
#[test]
fn test_switch_backend_preserves_read_errors() {
    let store = test_store();
    let mut app = XiaomiApp::new(
        store,
        Box::new(MockBackend::default()),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    assert!(app.error_msg.is_none());

    // 切换到读取全部失败的后端：refresh_from_backend 会设置错误信息，
    // 切换逻辑不得将其清空。
    app.apply_backend_switch(
        Box::new(MockBackend::all_fail("failing", BackendPreference::Auto)),
        BackendPreference::Wmi,
    );
    assert_eq!(app.backend.name(), "failing");
    let err = app.error_msg.as_deref().unwrap_or_default();
    assert!(err.contains("读取性能模式"), "unexpected: {}", err);
    assert!(err.contains("读取电池状态"), "unexpected: {}", err);
}

/// 回归测试：电源切换重设失败时，错误必须合并进 GUI 展示（F-ERR-03），
/// 且不得被 refresh_from_backend 成功时的 error_msg 清空逻辑吞掉。
#[test]
fn test_reapply_config_reports_write_errors() {
    let mut app = failing_app();
    app.config.auto_reapply_on_power_change = true;

    let ctx = egui::Context::default();
    app.cmd_tx.send(UiCommand::ReapplyConfig).unwrap();
    app.process_commands(&ctx);

    let err = app.error_msg.as_deref().unwrap_or_default();
    assert!(err.contains("重设充电上限失败"), "unexpected: {}", err);
    assert!(err.contains("重设电池养护失败"), "unexpected: {}", err);
    assert!(err.contains("重设性能模式失败"), "unexpected: {}", err);
    assert!(
        err.contains("读取性能模式"),
        "read errors must be preserved: {}",
        err
    );
}

/// 回归测试（M3，修订 1.30）：Fn 绑定"重新应用设置"（ReapplyConfigManual）
/// 必须**不受** `auto_reapply_on_power_change` 开关门控——用户主动按下
/// 绑定的功能键时应无条件重设，开关关闭时静默忽略是电源广播被动路径
/// 的语义，不该作用在用户手动动作上。历史实现 Fn 动作复用
/// `UiCommand::ReapplyConfig`，开关关闭时按键毫无反应（仅 debug 日志）。
#[test]
fn test_fn_reapply_manual_ignores_reapply_switch() {
    let store = test_store();
    let mock = MockBackend::default();
    let hw_perf = mock.perf_mode.clone();
    let mut app = XiaomiApp::new(
        store,
        Box::new(mock.clone()),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    // 重设开关关闭：电源路径会忽略，但 Fn 手动路径必须仍生效。
    app.config.auto_reapply_on_power_change = false;
    app.config.performance_mode = 0x04; // 狂暴
    hw_perf.store(0x09, std::sync::atomic::Ordering::Relaxed);

    let ctx = egui::Context::default();
    app.cmd_tx.send(UiCommand::ReapplyConfigManual).unwrap();
    app.process_commands(&ctx);

    assert!(
        hw_perf.load(std::sync::atomic::Ordering::Relaxed) != 0x09,
        "manual Fn reapply must rewrite hardware despite reapply switch off"
    );
}

/// 回归测试（修订 1.31）：开启"电池供电时自动切换节能"但关闭"电源切换
/// 时自动重设"时，电源广播路径的 ReapplyConfig 仍须生效——自动切节能
/// 依赖电源变化触发，若被重设开关一起关掉，用户明确开启的功能就静默
/// 失效（F-PWR-07 语义）。历史实现两者同门，自动切节能成了死配置。
#[test]
fn test_reapply_runs_when_auto_quiet_on_despite_reapply_switch_off() {
    let store = test_store();
    let mock = MockBackend::default();
    let hw_perf = mock.perf_mode.clone();
    let mut app = XiaomiApp::new(
        store,
        Box::new(mock.clone()),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    // 重设开关关闭 + 自动切节能开启：电源广播仍须重设。
    app.config.auto_reapply_on_power_change = false;
    app.config.auto_switch_to_quiet_on_battery = true;
    app.config.performance_mode = 0x04; // 狂暴
    hw_perf.store(0x09, std::sync::atomic::Ordering::Relaxed);

    let ctx = egui::Context::default();
    app.cmd_tx.send(UiCommand::ReapplyConfig).unwrap();
    app.process_commands(&ctx);

    assert!(
        hw_perf.load(std::sync::atomic::Ordering::Relaxed) != 0x09,
        "auto-quiet needs the power-reapply path to run despite reapply switch off"
    );

    // 两开关都关闭：电源路径才真正跳过（不写硬件）。
    let store2 = test_store();
    let mock2 = MockBackend::default();
    let hw_perf2 = mock2.perf_mode.clone();
    let mut app2 = XiaomiApp::new(
        store2,
        Box::new(mock2.clone()),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    app2.config.auto_reapply_on_power_change = false;
    app2.config.auto_switch_to_quiet_on_battery = false;
    app2.config.performance_mode = 0x04;
    hw_perf2.store(0x09, std::sync::atomic::Ordering::Relaxed);
    let ctx = egui::Context::default();
    app2.cmd_tx.send(UiCommand::ReapplyConfig).unwrap();
    app2.process_commands(&ctx);
    assert_eq!(
        hw_perf2.load(std::sync::atomic::Ordering::Relaxed),
        0x09,
        "both switches off must keep the power path inert"
    );
}

/// 回归测试：`apply_config_and_sync`（用户主动重设，如勾选自动切节能）
/// 必须**不受** `auto_reapply_on_power_change` 开关约束——主动操作无条件
/// 应用。历史实现把两者绑在一起，开关关闭时用户勾选"电池供电自动切节能"
/// 静默不生效。
#[test]
fn test_apply_config_and_sync_ignores_reapply_switch() {
    let store = test_store();
    let mock = MockBackend::default();
    let hw_perf = mock.perf_mode.clone();
    let mut app = XiaomiApp::new(
        store,
        Box::new(mock.clone()),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    // 重设开关关闭：电源切换/唤醒路径会忽略，但主动应用必须仍生效。
    app.config.auto_reapply_on_power_change = false;
    app.config.performance_mode = 0x04; // 狂暴
    hw_perf.store(0x09, std::sync::atomic::Ordering::Relaxed);

    app.apply_config_and_sync();

    assert!(
        hw_perf.load(std::sync::atomic::Ordering::Relaxed) != 0x09,
        "hardware perf mode must be rewritten despite reapply switch off"
    );
}

/// 回归测试：开启养护时 set_charge_limit 成功、但 set_battery_care 失败
/// （如 EC 拒绝写入养护位）时，硬件限值已生效，UI/配置必须按限值保持
/// 自洽（限值是两种后端判定养护状态的权威依据）。否则下次启动会按旧的
/// care=false 强制写 100%，静默覆盖用户设置的限值。
#[test]
fn test_battery_care_enable_partial_failure_keeps_config_coherent() {
    let store = test_store();
    let mut app = XiaomiApp::new(
        store,
        Box::new(MockBackend::partial_care("partial-care")),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    // 模拟用户养护关闭、期望上限 60%。
    app.config.battery_charge_limit = 60;
    app.config.battery_care_enabled = false;
    app.runtime.battery_care_enabled = false;
    app.runtime.charge_limit = 100;

    // 开启养护：limit 写入成功（60%），care 位写入失败。
    app.set_battery_care_internal(true);

    // 限值已生效：状态与持久化配置必须按限值自洽（养护开启、上限 60），
    // 并且错误要在 GUI 展示。
    assert!(app.runtime.battery_care_enabled);
    assert!(app.config.battery_care_enabled);
    assert_eq!(app.runtime.charge_limit, 60);
    assert_eq!(app.config.battery_charge_limit, 60);
    assert!(app
        .error_msg
        .as_deref()
        .unwrap_or_default()
        .contains("设置电池养护失败"));

    // 模拟下次启动 apply_startup_config：care=true → set_charge_limit(60)，
    // 用户设置的 60% 不再被覆盖为 100%。
    let cfg = app.config.clone();
    assert!(cfg.battery_care_enabled);
    assert_eq!(cfg.battery_charge_limit, 60);
}

/// 回归测试：电源切换重设时，若旧版本/手改配置残留 care=true +
/// limit=100 的矛盾组合，必须按 GUI 切换路径的规则兜底为 80% 写入
/// 硬件——否则 WMI 会把 100% 写进硬件使养护失效，WinRing0 则会出现
/// 养护位开启但上限 100% 的矛盾状态，且配置被静默改写。
#[test]
fn test_reapply_config_normalizes_incoherent_limit() {
    let store = test_store();
    let mock = MockBackend::default();
    let mut app = XiaomiApp::new(
        store,
        Box::new(mock.clone()),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    app.config.battery_care_enabled = true;
    app.config.battery_charge_limit = 100;

    let ctx = egui::Context::default();
    app.cmd_tx.send(UiCommand::ReapplyConfig).unwrap();
    app.process_commands(&ctx);

    // 配置与硬件都按 80% 处理，养护保持开启。
    assert_eq!(app.config.battery_charge_limit, 80);
    assert_eq!(
        mock.charge_limit.load(std::sync::atomic::Ordering::Relaxed),
        80
    );
    assert!(mock.battery_care.load(std::sync::atomic::Ordering::Relaxed));
    assert!(app.error_msg.is_none());
}

#[test]
fn test_reapply_config_write_failure_keeps_original_limit() {
    let mut app = failing_app();
    app.config.auto_reapply_on_power_change = true;
    // 用户配置 care=true + limit=100（矛盾组合），但写入全部失败。
    app.config.battery_care_enabled = true;
    app.config.battery_charge_limit = 100;

    let ctx = egui::Context::default();
    app.cmd_tx.send(UiCommand::ReapplyConfig).unwrap();
    app.process_commands(&ctx);

    // 写入失败时不得把 config 静默改写为 80%（与 set_battery_care_internal
    // 的兜底规则一致），否则用户选择被破坏。
    assert_eq!(
        app.config.battery_charge_limit, 100,
        "config must not be normalized when the write failed"
    );
}

/// 回归测试：电源重设成功写入时，若后端量化（WMI 85%→80%），持久化配置
/// 必须跟随硬件实际生效值，与 set_charge_limit_internal / 启动同步的读回
/// 约定保持一致。历史实现把请求值（85%）直接持久化，config 与硬件长期
/// 背离（每次电源切换重复量化写入，UI 滑块显示硬件值 80 而配置仍是 85）。
#[test]
fn test_reapply_config_syncs_quantized_limit() {
    let store = test_store();
    let backend = MockBackend::quantizing();
    let hw_limit = backend.charge_limit.clone();
    let mut app = XiaomiApp::new(
        store,
        Box::new(backend),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    app.config.auto_reapply_on_power_change = true;
    app.config.battery_care_enabled = true;
    app.config.battery_charge_limit = 85;

    let ctx = egui::Context::default();
    app.cmd_tx.send(UiCommand::ReapplyConfig).unwrap();
    app.process_commands(&ctx);

    assert_eq!(hw_limit.load(std::sync::atomic::Ordering::Relaxed), 80);
    assert_eq!(
        app.config.battery_charge_limit, 80,
        "config must follow the hardware-applied value after quantization"
    );
    assert!(app.error_msg.is_none());
}

#[test]
fn test_failed_charge_limit_write_keeps_state_and_reports_error() {
    let mut app = failing_app();
    app.set_charge_limit_internal(60);

    // 写入失败：UI 状态与持久化配置必须保持原样，错误需在 GUI 展示。
    assert_eq!(app.runtime.charge_limit, 80);
    assert_eq!(app.config.battery_charge_limit, 80);
    assert!(app
        .error_msg
        .as_deref()
        .unwrap_or_default()
        .contains("设置充电上限失败"));
}

#[test]
fn test_failed_battery_care_write_keeps_state_and_reports_error() {
    let mut app = failing_app();
    app.set_battery_care_internal(true);

    assert!(!app.runtime.battery_care_enabled);
    assert!(!app.config.battery_care_enabled);
    assert!(app
        .error_msg
        .as_deref()
        .unwrap_or_default()
        .contains("设置电池养护失败"));
}

/// 回归测试：开启养护时，配置上限 ≥100 触发的 80% 兜底只能作用在成功
/// 路径；写入失败时，config 与 UI 必须保持原样，不允许内存中被提前改写
/// 成 80（否则后续任何 save_state 都会把"用户期望 100% 但写入失败"的
/// 状态静默持久化，破坏用户设置）。
#[test]
fn test_battery_care_fallback_write_failure_keeps_original_limit() {
    let store = test_store();
    let mut app = XiaomiApp::new(
        store,
        Box::new(MockBackend::all_fail("failing", BackendPreference::Auto)),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    // 用户期望 100%（触发兜底分支），写入全部失败。
    app.config.battery_charge_limit = 100;
    app.runtime.charge_limit = 100;

    app.set_battery_care_internal(true);

    // config 不得被提前改写为 80：写入失败时保持用户原值。
    assert_eq!(app.config.battery_charge_limit, 100);
    assert_eq!(app.runtime.charge_limit, 100);
    assert!(!app.runtime.battery_care_enabled);
    assert!(!app.config.battery_care_enabled);
    assert!(app
        .error_msg
        .as_deref()
        .unwrap_or_default()
        .contains("设置充电上限失败"));
}

#[test]
fn test_failed_perf_mode_write_keeps_state_and_reports_error() {
    let mut app = failing_app();
    app.set_perf_mode_internal(PerfMode::Quiet);

    assert_eq!(app.runtime.performance_mode, PerfMode::Smart as u8);
    assert_eq!(app.config.performance_mode, PerfMode::Smart as u8);
    assert!(app
        .error_msg
        .as_deref()
        .unwrap_or_default()
        .contains("设置性能模式失败"));
}

/// 回归测试：设置充电上限成功、但联动养护位写入失败时，配置必须保持
/// 自洽（care 由限值推导），不允许出现 care=false + limit=60 的矛盾组合
/// ——否则下次启动 auto_apply 会按 care=false 强制写 100%，
/// 用户选择的 60% 充电上限被静默摧毁。
#[test]
fn test_charge_limit_care_sync_failure_keeps_config_coherent() {
    let store = test_store();
    let mut app = XiaomiApp::new(
        store,
        Box::new(MockBackend::partial_care("partial-care")),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    // 模拟用户当前养护关闭、上限 100%（运行时与配置一致）。
    app.runtime.battery_care_enabled = false;
    app.config.battery_care_enabled = false;
    app.runtime.charge_limit = 100;
    app.config.battery_charge_limit = 100;

    // 用户把上限拖到 60%：limit 写入成功，但联动开启的 care 位写入失败。
    app.set_charge_limit_internal(60);

    // 限值是两个后端判定养护状态的权威依据：即使 care 位写失败，
    // 状态与持久化配置也必须按限值保持一致，且错误要在 GUI 展示。
    assert_eq!(app.runtime.charge_limit, 60);
    assert!(app.runtime.battery_care_enabled);
    assert_eq!(app.config.battery_charge_limit, 60);
    assert!(app.config.battery_care_enabled);
    assert!(app
        .error_msg
        .as_deref()
        .unwrap_or_default()
        .contains("同步电池养护状态失败"));

    // 模拟下次启动的 apply_startup_config 路径：
    // care=true → set_charge_limit(60)，用户选择的 60% 不再被覆盖为 100%。
    let cfg = app.config.clone();
    let mut recorded = Vec::new();
    if cfg.battery_care_enabled {
        recorded.push(("set_charge_limit".to_string(), cfg.battery_charge_limit));
    } else {
        recorded.push(("set_charge_limit".to_string(), 100));
    }
    assert_eq!(recorded, vec![("set_charge_limit".to_string(), 60)]);
}

/// 回归测试：WMI 后端下，养护开启时若 config 中的上限不是预设值（例如
/// 之前用 WinRing0 保存的 85%），硬件实际写入就近预设 80%。UI 与持久化
/// 配置必须显示硬件实际生效的 80%，而不是请求的 85%（AC-BAT-04）。
#[test]
fn test_wmi_quantization_readback_keeps_ui_in_sync() {
    let store = test_store();
    let backend = MockBackend::quantizing();
    let hw_limit = backend.charge_limit.clone();
    let mut app = XiaomiApp::new(
        store,
        Box::new(backend),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    // 模拟之前用 WinRing0 保存的非预设上限。
    app.config.battery_charge_limit = 85;
    app.runtime.battery_care_enabled = false;

    app.set_battery_care_internal(true);

    // UI 与持久化配置与硬件实际生效值一致（80%），而非请求值（85%）。
    assert_eq!(app.runtime.charge_limit, 80);
    assert_eq!(app.config.battery_charge_limit, 80);
    assert!(app.runtime.battery_care_enabled);
    assert!(app.config.battery_care_enabled);
    assert_eq!(hw_limit.load(std::sync::atomic::Ordering::Relaxed), 80);
}

/// 回归测试：WMI 后端下直接拖动上限到非预设值，同样需要读回硬件实际
/// 生效值，防止 UI/配置与硬件状态不一致。
#[test]
fn test_wmi_quantization_readback_on_charge_limit_set() {
    let store = test_store();
    let backend = MockBackend::quantizing();
    let hw_limit = backend.charge_limit.clone();
    let mut app = XiaomiApp::new(
        store,
        Box::new(backend),
        AppConfig::default(),
        crate::app::config::BackendPreference::Auto,
        None,
        false,
    );
    app.config.battery_charge_limit = 100;
    app.runtime.battery_care_enabled = false;

    app.set_charge_limit_internal(85);

    assert_eq!(app.runtime.charge_limit, 80);
    assert_eq!(app.config.battery_charge_limit, 80);
    assert!(app.runtime.battery_care_enabled);
    assert!(app.config.battery_care_enabled);
    assert_eq!(hw_limit.load(std::sync::atomic::Ordering::Relaxed), 80);
}

/// Fn 绑定：修改动作必须同步进共享绑定表（监听线程即时生效）。
#[test]
fn test_set_fn_binding_action_updates_shared_state() {
    let mut app = test_app();
    app.set_fn_binding_action(0, crate::app::fnkey::FnAction::ToggleBatteryCare);
    assert_eq!(
        app.config.fn_key_bindings[0].action,
        crate::app::fnkey::FnAction::ToggleBatteryCare
    );
    let snapshot = app.fn_bindings.read().unwrap().clone();
    assert_eq!(
        snapshot[0].action,
        crate::app::fnkey::FnAction::ToggleBatteryCare
    );
    // 越界 index 必须被安全忽略（不 panic、不改状态）。
    app.set_fn_binding_action(99, crate::app::fnkey::FnAction::ReapplyConfig);
    assert_eq!(app.config.fn_key_bindings.len(), 1);
}

/// Fn 绑定：RunCommand 命令文本保存与同步共享状态；越界安全忽略。
#[test]
fn test_set_fn_binding_command_updates_shared_state() {
    let mut app = test_app();
    app.set_fn_binding_command(0, r#"start "" "C:\Program Files\tool.exe""#);
    assert_eq!(
        app.config.fn_key_bindings[0].command.as_deref(),
        Some(r#"start "" "C:\Program Files\tool.exe""#)
    );
    let snapshot = app.fn_bindings.read().unwrap().clone();
    assert_eq!(
        snapshot[0].command.as_deref(),
        Some(r#"start "" "C:\Program Files\tool.exe""#)
    );

    // 空白命令允许保存（监听线程遇空白命令跳过并告警，见 run_external_command）。
    app.set_fn_binding_command(0, "");
    assert_eq!(app.config.fn_key_bindings[0].command.as_deref(), Some(""));

    // 越界 index 安全忽略。
    app.set_fn_binding_command(99, "ignored");
    assert_eq!(app.config.fn_key_bindings.len(), 1);
}

/// Fn 绑定：add 相同 (class,prefix) 不重复，只更新动作。
#[test]
fn test_add_fn_binding_dedup_and_normalize() {
    let mut app = test_app();
    // 带分隔符/小写输入归一化后与默认 Fn+K 相同 → 只更新动作。
    app.add_fn_binding(
        "HID_EVENT20",
        "01-28-01",
        crate::app::fnkey::FnAction::None,
        "",
    );
    assert_eq!(app.config.fn_key_bindings.len(), 1);
    assert_eq!(
        app.config.fn_key_bindings[0].action,
        crate::app::fnkey::FnAction::None
    );

    // 新键码追加。
    app.add_fn_binding(
        "HID_EVENT20",
        "0107",
        crate::app::fnkey::FnAction::ReapplyConfig,
        "",
    );
    assert_eq!(app.config.fn_key_bindings.len(), 2);
    assert_eq!(app.config.fn_key_bindings[1].prefix, "0107");
}

/// Fn 绑定：RunCommand 动作添加时携带命令；非 RunCommand 动作不存命令。
#[test]
fn test_add_fn_binding_run_command_carries_command() {
    let mut app = test_app();
    app.add_fn_binding(
        "HID_EVENT20",
        "0107",
        crate::app::fnkey::FnAction::RunCommand,
        r#"start "" "C:\Tools\app.exe""#,
    );
    assert_eq!(app.config.fn_key_bindings.len(), 2);
    assert_eq!(
        app.config.fn_key_bindings[1].command.as_deref(),
        Some(r#"start "" "C:\Tools\app.exe""#)
    );

    // 非 RunCommand 动作：即使传了命令也不保存（避免误存）。
    app.add_fn_binding(
        "HID_EVENT20",
        "0123",
        crate::app::fnkey::FnAction::ToggleBatteryCare,
        "should-not-stick",
    );
    assert_eq!(app.config.fn_key_bindings[2].command, None);
}

/// Fn 绑定：删除与共享状态同步。
#[test]
fn test_remove_fn_binding() {
    let mut app = test_app();
    app.remove_fn_binding(0);
    assert!(app.config.fn_key_bindings.is_empty());
    assert!(app.fn_bindings.read().unwrap().is_empty());
    // 越界安全。
    app.remove_fn_binding(0);
}

/// Fn 捕获开关：开启后切换标记并记录最近事件。
#[test]
fn test_toggle_fn_capture() {
    let mut app = test_app();
    assert!(!app.fn_capture.load(std::sync::atomic::Ordering::Relaxed));
    app.toggle_fn_capture();
    assert!(app.fn_capture.load(std::sync::atomic::Ordering::Relaxed));

    // FnEventSeen 命令更新最近捕获。
    let ctx = egui::Context::default();
    app.cmd_tx
        .send(UiCommand::FnEventSeen {
            class: "HID_EVENT20".into(),
            hex: "012801".into(),
        })
        .unwrap();
    app.process_commands(&ctx);
    assert_eq!(
        app.last_fn_event,
        Some(("HID_EVENT20".to_string(), "012801".to_string()))
    );

    app.toggle_fn_capture();
    assert!(!app.fn_capture.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(app.last_fn_event, None, "capture off clears last event");
}

/// 回归测试（修订 1.31）：捕获模式下固件先发按下（`012801`）后发释放
/// （`012800`），释放总是后到——`last_fn_event` 必须保留按下事件，否则
/// "最近捕获"显示释放码、"使用此键"绑定 `012800`，下一次物理按键的
/// `012801` 不再命中（F-FNK-06 按下/释放语义冲突）。
#[test]
fn test_capture_keeps_press_over_release() {
    let mut app = test_app();
    let ctx = egui::Context::default();

    // 按下事件先到。
    app.cmd_tx
        .send(UiCommand::FnEventSeen {
            class: "HID_EVENT20".into(),
            hex: "012801".into(),
        })
        .unwrap();
    app.process_commands(&ctx);
    assert_eq!(
        app.last_fn_event,
        Some(("HID_EVENT20".to_string(), "012801".to_string()))
    );

    // 同键码释放事件后到：不得覆盖按下事件。
    app.cmd_tx
        .send(UiCommand::FnEventSeen {
            class: "HID_EVENT20".into(),
            hex: "012800".into(),
        })
        .unwrap();
    app.process_commands(&ctx);
    assert_eq!(
        app.last_fn_event,
        Some(("HID_EVENT20".to_string(), "012801".to_string())),
        "release event must not overwrite the press event"
    );

    // 不同键码（新按下）正常更新：不误伤后续独立按键。
    app.cmd_tx
        .send(UiCommand::FnEventSeen {
            class: "HID_EVENT20".into(),
            hex: "010701".into(),
        })
        .unwrap();
    app.process_commands(&ctx);
    assert_eq!(
        app.last_fn_event,
        Some(("HID_EVENT20".to_string(), "010701".to_string()))
    );

    // 只有释放事件（无先前按下）时允许记录（用户只按了释放/键码以 00
    // 结尾的真实按键），不因误过滤而丢失。
    app.last_fn_event = None;
    app.cmd_tx
        .send(UiCommand::FnEventSeen {
            class: "HID_EVENT20".into(),
            hex: "012800".into(),
        })
        .unwrap();
    app.process_commands(&ctx);
    assert_eq!(
        app.last_fn_event,
        Some(("HID_EVENT20".to_string(), "012800".to_string())),
        "standalone release (no prior press) must still be captured"
    );
}

/// 延迟恢复探测结果（UiCommand::WmiAvailable）应用：用户偏好仍是
/// WMI/Auto（希望 WMI 生效）且当前是回退后端时，探测到的 WMI 后端被
/// 切换为活动后端。
#[test]
fn test_wmi_available_applies_when_preference_wants_wmi() {
    let store = test_store();
    // 构造时后端为 WinRing0（模拟首次启动 WMI 失败回退），偏好保持
    // AppConfig 默认 Wmi（AUTO 语义下当前实例实际是 WinRing0）。
    let mut app = XiaomiApp::new(
        store,
        Box::new(MockBackend::all_fail(
            "pref-winring0",
            BackendPreference::WinRing0,
        )),
        AppConfig::default(),
        BackendPreference::WinRing0,
        None,
        false,
    );
    app.error_msg = None;
    assert_eq!(app.config.backend, BackendPreference::Wmi);

    let ctx = egui::Context::default();
    app.cmd_tx
        .send(UiCommand::WmiAvailable(Box::new(MockBackend::all_fail(
            "pref-wmi",
            BackendPreference::Wmi,
        ))))
        .unwrap();
    app.process_commands(&ctx);

    assert_eq!(app.backend.preference(), BackendPreference::Wmi);
    // 偏好（config.backend）保持不变，仅活动后端切换为 WMI。
    assert_eq!(app.config.backend, BackendPreference::Wmi);
    assert_eq!(app.current_pref, BackendPreference::Wmi);
    assert!(
        app.wmi_recover_at.is_none(),
        "recovery must stop after successful switch"
    );
}

/// Auto 偏好下的延迟恢复：config.backend=Auto（实际后端 WinRing0）时，
/// 探测成功后活动后端切为 WMI，偏好仍保持 Auto（current_pref=Auto）。
#[test]
fn test_wmi_available_applies_keeping_auto_preference() {
    let store = test_store();
    let mut app = XiaomiApp::new(
        store,
        Box::new(MockBackend::all_fail(
            "pref-winring0",
            BackendPreference::WinRing0,
        )),
        AppConfig {
            backend: BackendPreference::Auto,
            ..Default::default()
        },
        BackendPreference::WinRing0,
        None,
        false,
    );
    app.error_msg = None;
    assert_eq!(app.config.backend, BackendPreference::Auto);

    let ctx = egui::Context::default();
    app.cmd_tx
        .send(UiCommand::WmiAvailable(Box::new(MockBackend::all_fail(
            "pref-wmi",
            BackendPreference::Wmi,
        ))))
        .unwrap();
    app.process_commands(&ctx);

    assert_eq!(app.backend.preference(), BackendPreference::Wmi);
    assert_eq!(app.config.backend, BackendPreference::Auto);
    assert_eq!(app.current_pref, BackendPreference::Auto);
}

/// 探测结果过期：探测期间用户手动把偏好切到 WinRing0，迟到的 WMI
/// 探测结果必须被丢弃，不得覆盖用户的最新选择。
#[test]
fn test_wmi_available_discarded_when_user_picked_winring0() {
    let store = test_store();
    let mut app = XiaomiApp::new(
        store,
        Box::new(MockBackend::all_fail(
            "pref-winring0",
            BackendPreference::WinRing0,
        )),
        AppConfig {
            backend: BackendPreference::WinRing0,
            ..Default::default()
        },
        BackendPreference::WinRing0,
        None,
        false,
    );
    app.error_msg = None;
    let backend_before = app.backend.name();

    let ctx = egui::Context::default();
    app.cmd_tx
        .send(UiCommand::WmiAvailable(Box::new(MockBackend::all_fail(
            "pref-wmi",
            BackendPreference::Wmi,
        ))))
        .unwrap();
    app.process_commands(&ctx);

    assert_eq!(
        app.backend.name(),
        backend_before,
        "probed backend must be dropped when user switched preference"
    );
    assert_eq!(app.config.backend, BackendPreference::WinRing0);
}

/// 回归测试：当前后端已经是 WMI 时，迟到的探测结果必须被丢弃（避免
/// 重复切换把正在使用的后端重建一遍）。
#[test]
fn test_wmi_available_discarded_when_already_wmi() {
    let store = test_store();
    let mut app = XiaomiApp::new(
        store,
        Box::new(MockBackend::all_fail("pref-wmi", BackendPreference::Wmi)),
        AppConfig::default(),
        BackendPreference::Wmi,
        None,
        false,
    );
    app.error_msg = None;
    let backend_before = app.backend.name();

    let ctx = egui::Context::default();
    app.cmd_tx
        .send(UiCommand::WmiAvailable(Box::new(MockBackend::all_fail(
            "pref-wmi",
            BackendPreference::Wmi,
        ))))
        .unwrap();
    app.process_commands(&ctx);

    assert_eq!(
        app.backend.name(),
        backend_before,
        "must not recreate an already-active WMI backend"
    );
}
