//! 电池健康监测（`root\WMI`：设计容量 / 当前满充容量）。
//!
//! 小米官方 PC Manager 会在主界面展示"电池健康度"（满充容量 ÷ 设计容量），
//! 本模块把同等信息带给本应用：从 ACPI 电池驱动暴露的 WMI 类读取两组容量：
//!
//! - `BatteryStaticData.DesignedCapacity`（设计容量，mWh）
//! - `BatteryFullChargedCapacity.FullChargedCapacity`（当前满充容量，mWh）
//!
//! 健康度 = 满充容量 / 设计容量 × 100%。新电池可能略高于 100%（测量/标定
//! 误差），如实展示；长期使用随电池衰减缓慢下降——这正是用户关心的磨损指标。
//!
//! 注意：这两组类是 Windows 的标准 ACPI 电池类（`Win32_Perf` 派生），与
//! 小米 EC 的 `MICommonInterface`（`ec/wmi.rs`）**无关**，任何有电池的
//! 机器（不限于小米机型）都可读；因此该读取不依赖 EC 后端，作为独立后台
//! 线程常驻运行（GUI 主线程从不初始化 COM，见 `main.rs` 注释）。
//!
//! 线程模型：专用线程内 `CoInitializeEx(MTA)` → `connect_root_wmi` →
//! 周期性轮询 → `CoUninitialize`。查询失败（WMI 服务未就绪/连接失效）按
//! 退避重连；无电池数据（台式机/VM/驱动未加载）低频探测不刷屏。读取结果
//! 以 `UiCommand::BatteryHealthUpdated` 回传 GUI。

use std::sync::Arc;

use windows::Win32::System::Wmi::{IWbemClassObject, IWbemServices, WBEM_E_INVALID_CLASS};

use crate::app::command::UiCommand;
use crate::app::sink::CommandSink;
use crate::util::err_fmt;

/// `BatteryStaticData.DesignedCapacity`：电芯出厂设计容量（mWh）。
const CLASS_STATIC: &str = "BatteryStaticData";
const PROP_DESIGNED: &str = "DesignedCapacity";
/// `BatteryFullChargedCapacity.FullChargedCapacity`：当前标定的满充容量（mWh）。
/// 随电池老化逐步低于设计值——健康度的分子。
const CLASS_FULL_CHARGED: &str = "BatteryFullChargedCapacity";
const PROP_FULL_CHARGED: &str = "FullChargedCapacity";
/// `BatteryStatus`：实时充放电状态（剩余容量 / 充放电速率 / 充放电标记），
/// 用于预计剩余/充满时长（修订 1.37）。速率单位为 mW（与容量 mWh 同系，
/// 本机 2025 RedmiBook Pro 14 实证：RemainingCapacity=UInt32、
/// ChargeRate/DischargeRate=Int32、Charging/Discharging=Boolean）。
const CLASS_STATUS: &str = "BatteryStatus";
const PROP_REMAINING: &str = "RemainingCapacity";
const PROP_CHARGE_RATE: &str = "ChargeRate";
const PROP_DISCHARGE_RATE: &str = "DischargeRate";
const PROP_CHARGING: &str = "Charging";
const PROP_DISCHARGING: &str = "Discharging";

/// 轮询间隔：健康容量（满充/设计）变化很慢，但**充放电速率**（ETA 的
/// 输入）随负载实时变化——30s 一次既足够刷新时长估计，又保持连接活性、
/// 拾取容量标定后的新值。单次查询开销为毫秒级。
const POLL_OK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
/// 本机无电池数据（类不存在/实例为空/容量为 0）时的探测间隔：该类情况
/// 不会自愈（除非装上电池），60s 一次的低频探测保留恢复通道而不刷屏。
const POLL_NO_DATA_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// 连接/查询失败（WMI 服务未就绪、连接失效）后的重建退避。
const RECONNECT_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);
/// 快速重连阶段的连续失败次数上限：前 `FAST_RECONNECT_ATTEMPTS` 次失败按
/// `RECONNECT_BACKOFF` 快速重试（WMI 服务刚启动/休眠唤醒后的瞬态故障），
/// 超过后拉开到 `SLOW_RECONNECT_BACKOFF` 防刷屏（修订 1.47 命名，原为
/// 字面量 `failures <= 3` 与 `30s`）。
const FAST_RECONNECT_ATTEMPTS: u32 = 3;
/// 慢速重连退避：连续失败超过 `FAST_RECONNECT_ATTEMPTS` 后每轮等待时长。
const SLOW_RECONNECT_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

/// `BatteryStatus` 连续读取失败达到此次数后返回 Err 驱动整条连接重建。
///
/// 与 `read_health` 的失败语义对齐：健康读数成功但 `BatteryStatus` 连续失败
/// 说明连接对后者已不可用（提供程序异常/连接半死），仅跳过单次 ETA 会永久
/// 失去自愈能力。偶发单次失败（不影响健康读数）不触发重建。
const ETA_FAIL_RECONNECT_THRESHOLD: u32 = 3;

/// ETA 估算的速率下限（mW）：低于此值视作异常读数（未充放电/垃圾值）。
///
/// 笔记本电池的充放电速率实测在数千 mW 量级（充电 20~90W、放电空闲
/// 5~20W）；100 mW 以下只可能来自驱动异常/未初始化字段。真实速率不会
/// 低于此值，用作"无有效数据"的干净阈值（避免垃圾小速率算出数百小时的
/// 荒谬 ETA）。
const MIN_RATE_MW: u32 = 100;

/// 电池健康读数（设计容量与当前满充容量，mWh）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryHealth {
    /// 设计容量（出厂额定，mWh）。
    pub designed_mwh: u32,
    /// 当前满充容量（mWh）。
    pub full_mwh: u32,
}

impl BatteryHealth {
    /// 电池健康度（满充容量 ÷ 设计容量 × 100%）。
    ///
    /// 设计容量为 0（无有效数据）时返回 None，由调用方展示"未知"。
    /// 满充容量可为 0（读数异常），此时健康度为 0%，如实展示不崩溃。
    pub fn health_percent(&self) -> Option<f32> {
        if self.designed_mwh == 0 {
            return None;
        }
        Some(self.full_mwh as f32 / self.designed_mwh as f32 * 100.0)
    }

    /// 展示用整数百分比（四舍五入，钳制到 [0, 255]）。
    pub fn health_percent_u8(&self) -> Option<u8> {
        self.health_percent()
            .map(|p| p.round().clamp(0.0, 255.0) as u8)
    }
}

/// 放电剩余时长估算（纯函数，便于测试）：`remaining_mwh / discharge_rate_mw`
/// = 可用分钟。速率 ≤ 0（未放电、异常读数）、低于 `MIN_RATE_MW`（垃圾值）
/// 或剩余为 0 时返回 None（无估算依据，展示"未知"）。容量单位 mWh、速率
/// mW 时 `容量/速率` 天然为小时，×60 转分钟。
pub fn eta_discharge_minutes(remaining_mwh: u32, discharge_rate_mw: u32) -> Option<u64> {
    if discharge_rate_mw < MIN_RATE_MW || remaining_mwh == 0 {
        return None;
    }
    Some(remaining_mwh as u64 * 60 / discharge_rate_mw as u64)
}

/// 充电充满时长估算（纯函数，便于测试）：`(full_mwh - remaining_mwh) /
/// charge_rate_mw` = 充满分钟。速率低于 `MIN_RATE_MW`（停充/异常）、缺口为 0
/// （已充满）或估算结果为 0（速率过大/缺口过小）时返回 None。满充容量未知
/// （`full_mwh = 0`）时缺口饱和为 0 → None。
pub fn eta_charge_minutes(remaining_mwh: u32, charge_rate_mw: u32, full_mwh: u32) -> Option<u64> {
    if charge_rate_mw < MIN_RATE_MW {
        return None;
    }
    let gap = full_mwh.saturating_sub(remaining_mwh);
    if gap == 0 {
        return None;
    }
    let minutes = gap as u64 * 60 / charge_rate_mw as u64;
    (minutes > 0).then_some(minutes)
}

/// 分钟 → "Xh Ym"（如 `2h 45m`）。`eta_*_minutes` 的展示辅助。
pub fn format_minutes(mins: u64) -> String {
    let h = mins / 60;
    let m = mins % 60;
    match (h, m) {
        (0, m) => format!("{} 分钟", m),
        (h, 0) => format!("{} 小时", h),
        (h, m) => format!("{} 小时 {} 分钟", h, m),
    }
}

/// 启动电池健康监测线程。
///
/// 与 Fn 监听/托盘 worker 一致地以 `catch_unwind` 包裹（修订 1.33 兜底）：
/// 线程内 panic 被捕获并记录语义化错误，不静默终止监听。
///
/// 命令端口与其他后台线程统一为 `CommandSink`（发送 + 唤醒由 GUI 侧实现，
/// 见 `app::sink`）；`send` 的 `Err`（GUI 已销毁）经 `send_or_finish` 转为
/// 线程停止信号。
pub fn spawn(sink: Arc<dyn CommandSink>) {
    // 与托盘/Fn 监听/自启动/WMI 共用 util::spawn_guarded 兜底（修订 1.33 +
    // 1.47 收敛）：线程内 panic 被捕获并记录语义化错误，不静默终止监听。
    if let Err(e) = crate::util::spawn_guarded("battery-health", move || {
        log::info!("Battery health thread started");
        // run 内部整条循环已再包一层 catch_unwind（修订 1.46 回归）：一次
        // panic 只是一次"连接失败"退避重连，线程继续存活；这里是最外层
        // 兜底（run 永不正常返回，只有 panic 才落到外层）。
        if let Err(e) = run(sink.as_ref()) {
            log::error!("Battery health thread: {}", e);
        }
        log::info!("Battery health thread exited");
    }) {
        log::warn!("failed to spawn battery health thread: {}", e);
    }
}

/// 主循环：建立连接后持续轮询；连接/查询失败或线程内 panic 时返回 Err 由
/// 外层退避重试。正常返回 `Ok(())` 仅发生在 GUI 命令通道关闭（进程退出/
/// 界面已销毁）时——此时不再重连，线程随进程结束（修订 1.47：历史实现
/// 声称"正常返回"但从未真正发生，见 `send_or_finish`）。
fn run(sink: &dyn CommandSink) -> Result<(), String> {
    let mut failures: u32 = 0;
    // 本轮连接是否至少成功完成过一次轮询（修订 1.50 修复）：退避计数
    // `failures` 只增不减，一次瞬态连击进入慢速退避后**永久**停留——之后即使
    // 已健康运行数小时，任何一次偶然失败仍直接走 30s 慢退避。记录"本轮
    // 发生过成功轮询"，失败时先归零再计，恢复后重新按 快速→慢速 阶梯走。
    // 与 `poll_connected` 内 `eta_failures` 的"成功清零"语义同源。
    let mut made_progress = false;
    loop {
        // 每一轮连接生命周期独立 catch_unwind：单个 COM/FFI panic 不能杀死
        // 整个监测线程——捕获后按"连接失败"退避重连（见 spawn 注释）。
        let round = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            poll_loop(sink, &mut made_progress)
        }));
        let result = match round {
            Ok(r) => r,
            Err(panic) => {
                let payload = crate::util::panic_message(&*panic);
                Err(err_fmt("panic", payload))
            }
        };
        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                // 本轮连接成功轮询过（恢复后的首次失败）：归零再计，回到
                // 快速退避档；连续失败则累积（慢速档）。
                if made_progress {
                    failures = 0;
                }
                made_progress = false;
                failures += 1;
                // 退避节奏（与 Fn watcher 的 NoEventClasses 类似）：连续失败
                // 前期快速重试（WMI 服务刚启动）、随后拉开间隔防刷屏。失败
                // 多为瞬态（WMI 服务重启/休眠唤醒），保持重试即可自愈。
                let delay = if failures <= FAST_RECONNECT_ATTEMPTS {
                    RECONNECT_BACKOFF
                } else {
                    SLOW_RECONNECT_BACKOFF
                };
                log::warn!(
                    "Battery health poll failed (attempt #{}): {}; retrying in {}s",
                    failures,
                    e,
                    delay.as_secs()
                );
                std::thread::sleep(delay);
            }
        }
    }
}

/// 一轮连接生命周期：COM 初始化 → 连接 → 无限轮询 → 退出时 CoUninitialize。
/// COM 初始化与清理严格配对（同 Fn watcher 的 run_watcher_once）。
///
/// 用 `ComScope` RAII（修订 1.46 审计）：`poll_connected` 内部可能 panic
/// （COM/FFI 边界），历史实现 `poll_loop` 的 `CoUninitialize` 在 panic 展开
/// 时被跳过——每轮 panic 泄漏一次公寓引用计数，且外层 catch_unwind 会继续
/// 重连。RAII 保证 panic 展开也执行 CoUninitialize（与 autostart 的 ComScope
/// 同源收敛于 `win::com`）。
fn poll_loop(sink: &dyn CommandSink, made_progress: &mut bool) -> Result<(), String> {
    let _com = crate::win::ComScope::init()?;
    poll_connected(sink, made_progress)
}

/// 向 GUI 发送命令；通道关闭（进程退出/界面已销毁）时返回 `false` 通知外层
/// **终止本轮轮询**（`poll_connected` 的 Ok(()) 分支 → `run` 视作正常退出，
/// 不再退避重连——GUI 已消失后继续轮询 WMI 毫无意义，修订 1.47 清理：
/// 历史实现返回 `Ok(())` 仅"假装"优雅退出，loop 无 break、`?` 继续轮询，
/// 文档宣称的"优雅结束"从未真正生效；健康与 ETA 两处曾各自手写同一
/// send+is_err 样板）。
///
/// 经 `CommandSink::send` 的 `Err` 区分"通道已关闭"与投递成功：投递成功后
/// 额外 `wake` 立即唤醒事件循环（与托盘/Fn 的 `dispatch` 一致，隐藏态下
/// 电池数据更新无需等 500ms 定时帧）。
fn send_or_finish(sink: &dyn CommandSink, cmd: UiCommand) -> bool {
    let delivered = sink.send(cmd).is_ok();
    if delivered {
        sink.wake();
    }
    delivered
}

fn poll_connected(sink: &dyn CommandSink, made_progress: &mut bool) -> Result<(), String> {
    let services = crate::win::connect_root_wmi()?;
    let mut last_health_sent: Option<(u32, u32)> = None;
    let mut last_eta_sent: Option<(u32, u32, u32, u32, bool, bool)> = None;
    // 无电池数据的告警去重：台式机/VM 上这些电池类**永久**不存在，逐次
    // 记录 warn 会每 60s 刷一条无价值的日志（与 power.rs 的
    // warn_unknown_once 同源问题）——首次出现记录一次，之后静默。
    let mut no_data_warned = false;
    // BatteryStatus 连续失败计数（修订 1.46）：达到阈值后把 Err 上抛触发
    // 整条连接重建（见 ETA_FAIL_RECONNECT_THRESHOLD 注释）。
    let mut eta_failures: u32 = 0;
    loop {
        match read_health(&services) {
            Ok(Some(h)) => {
                // 本轮连接成功轮询（供外层退避计数归零，见 run 的注释）。
                *made_progress = true;
                no_data_warned = false;
                let key = (h.designed_mwh, h.full_mwh);
                if last_health_sent != Some(key) {
                    log::info!(
                        "Battery health: designed={} mWh, full_charged={} mWh ({:?})",
                        h.designed_mwh,
                        h.full_mwh,
                        h.health_percent_u8()
                    );
                    if !send_or_finish(
                        sink,
                        UiCommand::BatteryHealthUpdated {
                            designed_mwh: h.designed_mwh,
                            full_mwh: h.full_mwh,
                        },
                    ) {
                        return Ok(());
                    }
                    last_health_sent = Some(key);
                }
                // 充放电状态（BatteryStatus）：用于预计剩余/充满时长。随负载
                // 变化的速率是唯一的快速变化字段——以 (remaining, charge,
                // discharge, charging, discharging) 五元组变化驱动发送，稳态下
                // 不再重复发。
                //
                // 去重键**额外包含满充容量**（full_mwh）：GUI 的充电时长 =
                // (full - remaining) / rate，满充容量重新标定（健康读数的
                // 分子变化）时即使五元组不变，估算结果也变了——漏掉它会让
                // ETA 文案长期停留在旧容量下计算的陈旧值。
                //
                // 速率无变化时估算值也无变化，不发是对的。
                //
                // `read_battery_status` 的 Err（BatteryStatus 类瞬时查询失败）
                // **不中断本轮**：健康读数已成功，仅跳过本次 ETA——偶发失败
                // 不应触发整条连接的 CoUninitialize + 重连 + 退避。
                match read_battery_status(&services) {
                    Ok(Some(s)) => {
                        eta_failures = 0;
                        let eta_key = eta_key_for(h.full_mwh, &s);
                        if last_eta_sent != Some(eta_key) {
                            last_eta_sent = Some(eta_key);
                            if !send_or_finish(
                                sink,
                                UiCommand::BatteryEtaUpdated {
                                    remaining_mwh: s.remaining_mwh,
                                    charge_rate_mw: s.charge_rate_mw,
                                    discharge_rate_mw: s.discharge_rate_mw,
                                    charging: s.charging,
                                    discharging: s.discharging,
                                },
                            ) {
                                return Ok(());
                            }
                        }
                    }
                    Ok(None) => {
                        log::debug!("Battery ETA: no BatteryStatus data this tick");
                    }
                    Err(e) => {
                        // 连续失败计数（修订 1.46）：单次失败只跳过本轮 ETA
                        // （健康读数已成功），连续达到阈值说明连接对
                        // BatteryStatus 已半死——返回 Err 走外层重建，避免
                        // 永久静默丢失 ETA 且不自愈。
                        eta_failures += 1;
                        if eta_failures == 1 {
                            log::warn!("Battery ETA read failed (skipping this tick): {}", e);
                        } else {
                            log::debug!(
                                "Battery ETA read failed ({}/{}): {}",
                                eta_failures,
                                ETA_FAIL_RECONNECT_THRESHOLD,
                                e
                            );
                        }
                        if eta_failures >= ETA_FAIL_RECONNECT_THRESHOLD {
                            return Err(format!(
                                "BatteryStatus failed {} consecutive times: {}",
                                eta_failures, e
                            ));
                        }
                    }
                }
                std::thread::sleep(POLL_OK_INTERVAL);
            }
            Ok(None) => {
                // 无电池数据：不当作错误反复刷屏，低频探测（有电池的机器上
                // 恢复后下一轮即读到数据）。无电池场景首次告警一次。
                *made_progress = true;
                if !no_data_warned {
                    no_data_warned = true;
                    log::warn!(
                        "Battery health: no battery capacity data (no battery or WMI classes unavailable)"
                    );
                }
                std::thread::sleep(POLL_NO_DATA_INTERVAL);
            }
            Err(e) => return Err(e),
        }
    }
}

/// 读取一次电池健康数据。
///
/// `Ok(Some)` = 有效读数；`Ok(None)` = 本机无电池/类不存在/容量为 0（不是
/// 错误，正常机器无电池也属此类）；`Err` = WMI 连接或查询本身失败（瞬态，
/// 需要重连）。
fn read_health(services: &IWbemServices) -> Result<Option<BatteryHealth>, String> {
    let designed = query_first_u32(services, CLASS_STATIC, PROP_DESIGNED)?;
    let full = query_first_u32(services, CLASS_FULL_CHARGED, PROP_FULL_CHARGED)?;
    match (designed, full) {
        (Some(d), Some(f)) if d > 0 && f > 0 => Ok(Some(BatteryHealth {
            designed_mwh: d,
            full_mwh: f,
        })),
        _ => Ok(None),
    }
}

/// 查询某个类的第一个实例。
///
/// - 类不存在 / 无实例：返回 Ok(None)（正常：如无电池的机器上这些电池类
///   根本不存在，逐类重连毫无意义——`WBEM_E_INVALID_CLASS` 被映射为
///   Ok(None) 而非 Err，否则无电池机器会陷入"每 30s 重连 + 告警"的循环）；
/// - 查询/枚举失败（服务未就绪、访问被拒等）：返回 Err 触发重连。
fn query_first_instance(
    services: &IWbemServices,
    class: &str,
) -> Result<Option<IWbemClassObject>, String> {
    // WQL 语句统一经 win::com::select_all_wql 构造（修订 1.50 收敛，与
    // fn_watcher 的事件订阅同一形状）。
    let query = crate::win::select_all_wql(class);
    let enumerator = match crate::win::exec_query(services, &query) {
        Ok(e) => e,
        // 类不存在（本机没有其他 WMI 类，如台式机/VM 无电池类）= 正常无数据；
        // 其余错误（服务未就绪等瞬态错误）触发重连。
        Err(e) if (e.code().0 as u32) == (WBEM_E_INVALID_CLASS.0 as u32) => {
            return Ok(None);
        }
        Err(e) => return Err(format!("ExecQuery {}: {}", class, e)),
    };
    // 单次 Next：仅读取第一个实例即返回（无后续记录需要时）。统一收敛在
    // win::com::next_instance（含"returned>0 但空槽"病态防御，修订 1.46）。
    // 枚举器真实失败（连接失效等瞬态）：触发外层重连——若按 Ok(None)
    // 处理会被降级为 60s 低频探测，连接坏死期间等一分钟才恢复。
    match unsafe { crate::win::next_instance(&enumerator, 500) } {
        Ok(Some(obj)) => Ok(Some(obj)),
        // 枚举耗尽或病态空槽（next_instance 已告警）都视作"本轮无数据"。
        Ok(None) => Ok(None),
        Err(e) => Err(format!("{}::Next: {}", class, e)),
    }
}

/// 从类查询第一个实例并读取 u32 属性值。
fn query_first_u32(
    services: &IWbemServices,
    class: &str,
    prop: &str,
) -> Result<Option<u32>, String> {
    let Some(obj) = query_first_instance(services, class)? else {
        return Ok(None);
    };
    Ok(crate::win::uint_prop(&obj, prop))
}

/// 充电/放电状态读数（`BatteryStatus`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BatteryStatusInfo {
    remaining_mwh: u32,
    charge_rate_mw: u32,
    discharge_rate_mw: u32,
    charging: bool,
    discharging: bool,
}

/// ETA 消息的发送去重键。
///
/// 除充放电五元组外**包含满充容量**（full_mwh）：GUI 的充电时长 =
/// (full - remaining) / rate，满充容量重新标定（健康读数的分子变化）时即使
/// 五元组不变，估算结果也变了——漏掉它会让 ETA 文案长期停留在旧容量下
/// 计算的陈旧值。
fn eta_key_for(full_mwh: u32, s: &BatteryStatusInfo) -> (u32, u32, u32, u32, bool, bool) {
    (
        full_mwh,
        s.remaining_mwh,
        s.charge_rate_mw,
        s.discharge_rate_mw,
        s.charging,
        s.discharging,
    )
}

/// 读取一次充放电状态（ETA 的输入）。
fn read_battery_status(services: &IWbemServices) -> Result<Option<BatteryStatusInfo>, String> {
    let Some(obj) = query_first_instance(services, CLASS_STATUS)? else {
        return Ok(None);
    };
    // 内部闭包返回 Option（任意属性缺失 → None）：BatteryStatus 是标准类，
    // 属性齐全时读取；个别属性缺失视作本机无完整数据（返回 Ok(None)）。
    // 充放电速率用 `uint_rate_prop`（修订 1.50）：固件以有符号 Int32 承载，
    // "该方向未充放电"可能上报负值——钳为 0 而不是把整条记录判为不可读
    //（否则一条字段的负号会连带丢失另一方向的 ETA，见 win/variant.rs）。
    let read = || -> Option<BatteryStatusInfo> {
        let remaining = crate::win::uint_prop(&obj, PROP_REMAINING)?;
        let charge_rate = crate::win::uint_rate_prop(&obj, PROP_CHARGE_RATE)?;
        let discharge_rate = crate::win::uint_rate_prop(&obj, PROP_DISCHARGE_RATE)?;
        let charging = crate::win::get_bool_prop(&obj, PROP_CHARGING)?;
        let discharging = crate::win::get_bool_prop(&obj, PROP_DISCHARGING)?;
        Some(BatteryStatusInfo {
            remaining_mwh: remaining,
            charge_rate_mw: charge_rate,
            discharge_rate_mw: discharge_rate,
            charging,
            discharging,
        })
    };
    Ok(read())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// 健康度计算：满充/设计 × 100（浮点比较用绝对误差）。设计容量 0 时
    /// 返回 None（未知）；满充为 0 时健康度为 0%（如实展示，不崩溃）。
    #[test]
    fn test_health_percent() {
        let h = BatteryHealth {
            designed_mwh: 76990,
            full_mwh: 77255,
        };
        let p = h.health_percent().unwrap();
        assert!((p - 100.3442).abs() < 0.01, "got {}", p);

        assert_eq!(
            BatteryHealth {
                designed_mwh: 76990,
                full_mwh: 60000
            }
            .health_percent_u8(),
            Some(78)
        );
        // 设计容量 0：无有效数据，返回 None（展示"未知"）。
        assert_eq!(
            BatteryHealth {
                designed_mwh: 0,
                full_mwh: 70000
            }
            .health_percent(),
            None
        );
        // 满充为 0（无电池读数）：健康度为 0%，不崩溃。
        assert_eq!(
            BatteryHealth {
                designed_mwh: 76990,
                full_mwh: 0
            }
            .health_percent_u8(),
            Some(0)
        );
        // 新电池可能略高于 100%（测量/标定误差）：如实展示并钳制到 u8 上限。
        assert_eq!(
            BatteryHealth {
                designed_mwh: 100000,
                full_mwh: 100500
            }
            .health_percent_u8(),
            Some(101)
        );
    }

    /// 预计时长计算（修订 1.37，1.43 拆分为放电/充电两个纯函数）：
    /// - 放电：剩余容量 / 放电速率 → 分钟；
    /// - 充电：缺口（满充-剩余）/ 充电速率 → 分钟；
    /// - 速率低于 MIN_RATE_MW（停充/异常/垃圾值）或无缺口：None；
    /// - 估算结果为 0 分钟（速率过大/缺口过小）：None（无展示价值）。
    #[test]
    fn test_eta_minutes() {
        // 放电：剩余 60000 mWh，速率 12000 mW → 5h = 300 分钟。
        assert_eq!(eta_discharge_minutes(60000, 12000), Some(300));
        // 放电速率 0：无估算依据。
        assert_eq!(eta_discharge_minutes(60000, 0), None);
        // 放电速率低于 MIN_RATE_MW（垃圾小速率）：无估算依据（修订 1.46，
        // 历史实现会把 1 mW 算出 1000 小时的荒谬 ETA）。
        assert_eq!(eta_discharge_minutes(60000, 99), None);
        // 放电剩余 0：无可估算的余量。
        assert_eq!(eta_discharge_minutes(0, 12000), None);
        // 充电：满充 77255，剩余 40000，速率 20000 mW → 37255/20000=1.86h
        // ≈ 111.77 分钟，整数截断为 111。
        assert_eq!(eta_charge_minutes(40000, 20000, 77255), Some(111));
        // 已充满（缺口 0）：无需再充。
        assert_eq!(eta_charge_minutes(77255, 20000, 77255), None);
        // 满充为 0（无健康数据时的防御）：缺口饱和为 0 → None。
        assert_eq!(eta_charge_minutes(40000, 20000, 0), None);
        // 充电速率 0 / 低于 MIN_RATE_MW：无估算依据。
        assert_eq!(eta_charge_minutes(40000, 0, 77255), None);
        assert_eq!(eta_charge_minutes(40000, 99, 77255), None);
        // 缺口过小（速率 20000 mW 下 1 mWh → 0 分钟）：无展示价值 → None。
        assert_eq!(eta_charge_minutes(77254, 20000, 77255), None);
    }

    /// 分钟 → "Xh Ym" 展示。
    #[test]
    fn test_format_minutes() {
        assert_eq!(format_minutes(300), "5 小时");
        assert_eq!(format_minutes(112), "1 小时 52 分钟");
        assert_eq!(format_minutes(45), "45 分钟");
    }

    /// ETA 去重键（修订 1.43）：必须包含满充容量——五元组不变但 full_mwh
    /// 变化（健康读数重新标定）时，充电时长估算值随之变化，键必须反映它
    /// 否则 ETA 文案停留陈旧值。
    #[test]
    fn test_eta_key_includes_full_mwh() {
        let s = BatteryStatusInfo {
            remaining_mwh: 40000,
            charge_rate_mw: 20000,
            discharge_rate_mw: 0,
            charging: true,
            discharging: false,
        };
        let k1 = eta_key_for(77255, &s);
        let k2 = eta_key_for(77000, &s);
        assert_ne!(k1, k2, "full_mwh must distinguish ETA dedup keys");
        // 五元组变化同样改变键（基础去重语义保留）。
        let mut s2 = s;
        s2.charge_rate_mw = 25000;
        assert_ne!(eta_key_for(77255, &s), eta_key_for(77255, &s2));
        // 完全相同时键相等（稳态不重复发送）。
        assert_eq!(eta_key_for(77255, &s), eta_key_for(77255, &s));
    }

    /// 真机验证（手动运行，非 CI）：本机读取真实电池容量并验证健康度
    /// 落在合理区间。运行：`cargo test -- --ignored
    /// battery_health_real_hardware_read`。
    #[test]
    #[ignore = "reads real battery WMI (manual hardware verification)"]
    fn battery_health_real_hardware_read() {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // COM 需在本线程初始化（同生产路径的 poll_loop，经 ComScope RAII
            // 配对回收——panic/提前返回都不会泄漏公寓引用计数）。
            let _com = match crate::win::ComScope::init() {
                Ok(com) => com,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            let result = run_connected_test(&tx);
            if let Err(e) = result {
                let _ = tx.send(Err(e));
            }
        });
        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(Ok((d, f))) => {
                assert!(d > 0 && f > 0, "capacities must be positive: {} / {}", d, f);
                let pct = (f as f32 / d as f32 * 100.0).round() as u8;
                assert!(
                    (50..=150).contains(&pct),
                    "health {}% out of sane range (design={}, full={})",
                    pct,
                    d,
                    f
                );
            }
            Ok(Err(e)) => panic!("battery health read failed: {}", e),
            Err(e) => panic!("battery health read timed out: {}", e),
        }
    }

    fn run_connected_test(tx: &mpsc::Sender<Result<(u32, u32), String>>) -> Result<(), String> {
        let services = crate::win::connect_root_wmi()?;
        let h = read_health(&services)?.ok_or("no battery health data")?;
        // 充放电状态（修订 1.37）也必须能读到：本机 BatteryStatus 应返回
        // 完整的剩余容量/速率/标记集合（当前接 AC 停充时速率可为 0，但字段
        // 必须齐全且 remaining > 0）。
        let s = read_battery_status(&services)?.ok_or("no battery status data")?;
        assert!(s.remaining_mwh > 0, "remaining capacity must be positive");
        tx.send(Ok((h.designed_mwh, h.full_mwh)))
            .map_err(|e| err_fmt("send", e))
    }
}
