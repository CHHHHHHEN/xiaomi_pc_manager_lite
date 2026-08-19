//! 单实例保护（F-AUTO-08）：通过命名互斥体保证同一时刻只运行一个实例。
//!
//! 必要性（本机实测过的真实缺陷）：缺少单实例保护时，自启动实例已在运行、
//! 用户再次手动启动（或连点两次 exe）会并存两个进程——两份托盘图标、
//! 两份全局热键（Ctrl+Alt+B/P 一次按键触发两次翻转/循环）、两份 Fn+K
//! WMI 事件订阅（一次 Fn+K 循环切换两次性能模式），且两个后端会同时
//! 竞争写入 EC。
//!
//! 实现：命名互斥体（`CreateMutexW` + `bInitialOwner=true`）。首个创建者
//! 获得所有权；后续进程 `CreateMutexW` 返回 `ERROR_ALREADY_EXISTS` 判定
//! 为"已有实例"。持有者进程退出时内核自动释放互斥体对象，崩溃遗留不会
//! 卡死后续启动。
//!
//! **必须在 `elevate_self()` 之后调用**：自我提权会重新启动一个新的提升
//! 进程，若在提权前持有互斥体，旧进程尚未退出时新进程会被误判为"第二实例"
//! 而退出，破坏提权重启动。

use windows::Win32::Foundation::{
    CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};

/// 会话级命名互斥体名：`Local\` 前缀（仅当前登录会话可见）+
/// `util::APP_ID`（修订 1.50 与计划任务名/AppUserModelID/配置目录同源——
/// 历史实现把 `"XiaomiPcManagerLite"` 手写在此处，任一处漂移会导致双实例
/// 并存或互斥体互不识别）。
/// 同用户的提权进程与非提权进程共享同一会话，可访问同一个互斥体。
fn instance_mutex_name() -> String {
    format!("Local\\{}", crate::util::APP_ID)
}

/// 互斥体句柄持有者：进程存活期间持有，Drop 时关闭句柄。
pub struct SingleInstanceGuard(HANDLE);

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub enum SingleInstance {
    /// 本进程是当前唯一的实例，可继续启动。
    Acquired(SingleInstanceGuard),
    /// 已有另一实例在运行（互斥体已被持有）。
    Existing,
    /// API 调用失败，无法确认：防御性按"无冲突"处理，不阻塞启动。
    Unknown,
}

/// 尝试取得单实例互斥体所有权。
///
/// 所有权判定不用 `GetLastError`（历史实现）：`CreateMutexW` 成功时
/// `GetLastError` 是"上次错误"值，`main()` 在此前已执行过若干 Win32 调用
/// （提权检测、AppUserModelID、日志文件打开），其中任一次把 last-error 残留
/// 为 `ERROR_ALREADY_EXISTS` 就会让**首次启动**被误判为"已有实例"而退出。
/// 改用 `WaitForSingleObject(handle, 0)` 的返回码做所有权探测（零超时、无阻塞）：
/// - `WAIT_OBJECT_0`：本进程获得所有权（互斥体此前无人持有或已释放）；
/// - `WAIT_ABANDONED`：原持有者进程崩溃/退出未释放——内核将所有权转移给
///   本进程，按"已获得"处理（崩溃遗留不会卡死后续启动）；
/// - `WAIT_TIMEOUT`：互斥体被另一实例持有 → 判定"已有实例"；
/// - 其它值：API 异常，按"无法确认"处理（不阻塞启动）。
pub fn acquire() -> SingleInstance {
    let name = crate::util::WideString::new(&instance_mutex_name());
    unsafe {
        let handle = match CreateMutexW(None, true, name.as_pcwstr()) {
            Ok(h) => h,
            Err(_) => {
                log::warn!("Single instance mutex: CreateMutexW failed; proceeding");
                return SingleInstance::Unknown;
            }
        };
        match WaitForSingleObject(handle, 0) {
            WAIT_OBJECT_0 | WAIT_ABANDONED => {
                // 本进程取得所有权（或接管崩溃遗留，内核语义等效）。
                SingleInstance::Acquired(SingleInstanceGuard(handle))
            }
            WAIT_TIMEOUT => {
                // 已有实例持有互斥体（或持有者崩溃后对象尚未清理——内核
                // 会在持有进程退出时释放对象，此处短暂的重叠窗口忽略）。
                let _ = CloseHandle(handle);
                log::info!("Another instance is already running");
                SingleInstance::Existing
            }
            _ => {
                let _ = CloseHandle(handle);
                log::warn!("Single instance mutex: WaitForSingleObject failed; proceeding");
                SingleInstance::Unknown
            }
        }
    }
}

/// 在**提权之前**的预检：是否已有另一实例在运行（F-AUTO-08）。
///
/// 必要性：`main()` 先于单实例检查执行 `elevate_self()`（启动即提权）。
/// 若已有实例驻留托盘、用户再次启动（双击 exe / 运行快捷方式），当前
/// 流程会先弹 UAC、再被互斥体判定为"第二实例"退出——每次手动启动都
/// 白白弹一次 UAC。把"已有实例 → 唤醒窗口并退出"提前到提权之前即可
/// 免掉这次无意义的提权提示。
///
/// 语义：**只探测不持有**——进程刚创建的新互斥体在这里立刻释放（Drop），
/// 所有权由提权之后的 `acquire()` 正式取得；否则自我提权重启的新进程会
/// 因旧进程短暂持有的互斥体被误判为"第二实例"而退出（见模块顶部注释）。
/// API 异常（Unknown）时按"无冲突"返回 false，不阻塞启动。
pub fn pre_flight() -> bool {
    match acquire() {
        SingleInstance::Existing => true,
        SingleInstance::Acquired(guard) => {
            drop(guard);
            false
        }
        SingleInstance::Unknown => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两个单实例测试共享**进程级**命名互斥体，`cargo test` 默认并行线程运行
    /// 用例：test_second_acquire_detects_existing_instance 持有时，
    /// test_acquire_after_release_succeeds 的第二次 acquire 会返回 Existing
    /// 而 panic（flaky）。用进程内互斥串行化两个测试，行为确定。
    static TEST_SERIALIZER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 模拟"另一实例正在运行"：在**后台线程**持有单实例互斥体。
    ///
    /// 注意必须用独立线程而非同一线程两次 acquire：Windows 命名互斥体是
    /// **线程粒度**的所有权——同一线程重复 `WaitForSingleObject` 对已拥有
    /// 的互斥体会**立即成功**（递归获取，与 POSIX pthread 不同），因此同一
    /// 线程上第二个 `acquire()` 恒返回 `Acquired`，永远测不出 `Existing`。
    /// 历史测试在单线程里二次 acquire，只有当**恰好有真实 app 实例在跑**
    /// （外部进程持锁）时才走入 skip 分支而"假绿"——杀掉实例后测试立刻
    /// 暴露（修订 1.46）。改为真实跨线程持有：主线程的 acquire 才会看到
    /// 被其它线程占用的互斥体并返回 `Existing`。
    ///
    /// 通过 `std::sync::mpsc` 通道同步：持锁线程**先发"已持锁"信号再开始
    /// 持有**，主线程 `recv` 等到信号后才继续断言——不用固定 sleep 猜测
    /// 时序（sleep 在慢机器上可能主线程先于持锁线程 acquire，测出错误的
    /// `Acquired`）。返回的 Sender 由调用方在断言后 `send(())` 释放持锁
    /// 线程（join 等待其退出），确保测试结束前互斥体确实被释放（否则下一
    /// 条用例会因残留锁被误判 skip）。
    ///
    /// 返回 `(release_tx, holder, actually_held)`：`actually_held` 指示本线程
    /// 是否真实拿到所有权——**外部真实实例持锁时**（acquire 返回 Existing，
    /// 或本线程拿不到）为 false，调用方据此跳过"释放后无锁"类断言（此时
    /// 锁何时释放取决于外部实例，测试无法控制，修订 1.46 审计）。
    fn hold_mutex_in_worker() -> (
        std::sync::mpsc::Sender<()>,
        std::thread::JoinHandle<()>,
        bool,
    ) {
        let (hold_tx, hold_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::Builder::new()
            .name("single-instance-test-holder".into())
            .spawn(move || {
                // 持锁直到调用方经 release 通道放行。guard 随本线程退出释放。
                let _held = match acquire() {
                    SingleInstance::Acquired(guard) => Some(guard),
                    // 外部恰好有真实实例（或 API 不可用）：主线程断言的是
                    // "Existing"，此时同样成立。所有分支都经 hold_tx 发就绪
                    // 信号（见下），主线程不会永久阻塞等待。
                    SingleInstance::Existing => {
                        eprintln!("note: real instance holds the mutex; simulating hold");
                        None
                    }
                    SingleInstance::Unknown => None,
                };
                let _ = hold_tx.send(_held.is_some());
                // 真正持锁的分支才等待 release：外部实例/Unknown 场景没有
                // 本线程持有的 guard，立即结束即可。
                if _held.is_some() {
                    let _ = release_rx.recv();
                }
            })
            .expect("spawn holder thread");
        // 等待持锁线程就绪（已持锁或已确认外部实例）：所有分支都会 send
        // hold，正常路径毫秒级返回。兜底超时防止异常时主线程永久阻塞。
        let actually_held = hold_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("holder thread must report readiness within 5s");
        (release_tx, holder, actually_held)
    }

    /// 释放持锁线程并等待其退出：保证互斥体确实已释放（否则后续"放行"
    /// 断言会在持锁线程尚未退出时看到残留锁而失败——修订 1.46 消除竞态）。
    fn release_and_join(release: std::sync::mpsc::Sender<()>, holder: std::thread::JoinHandle<()>) {
        drop(release); // 关闭通道 → 持锁线程 recv 返回 Err → 退出、释放 guard。
        holder.join().expect("holder thread panicked");
    }

    /// 同一进程内两次 acquire：**跨线程**第一次拿到所有权后，主线程再次
    /// acquire 必须判定为"已有实例"。若测试环境里恰好跑着一个真实实例，
    /// 持有者线程拿不到锁（走 skip 分支），主线程同样看到 Existing——
    /// 断言仍然成立。
    #[test]
    fn test_second_acquire_detects_existing_instance() {
        let _serial = TEST_SERIALIZER.lock().unwrap_or_else(|e| e.into_inner());
        let (release, holder, _actually_held) = hold_mutex_in_worker();
        let second = acquire();
        match second {
            SingleInstance::Existing => {}
            other => panic!(
                "second acquire with mutex held by another thread must be Existing, got {:?}",
                other_kind(&other)
            ),
        }
        release_and_join(release, holder);
    }

    /// 持有者释放后，同一进程可再次获得所有权（对应"崩溃/退出后重启"场景）。
    #[test]
    fn test_acquire_after_release_succeeds() {
        let _guard = TEST_SERIALIZER.lock().unwrap_or_else(|e| e.into_inner());
        match acquire() {
            SingleInstance::Acquired(guard) => drop(guard),
            _ => {
                eprintln!("skip: mutex busy or unavailable");
                return;
            }
        }
        // 释放后应能再次取得。
        match acquire() {
            SingleInstance::Acquired(guard) => drop(guard),
            other => panic!(
                "expected acquired after release, got {:?}",
                other_kind(&other)
            ),
        }
    }

    /// 回归测试：已有实例运行时 pre_flight 必须返回 true（提权前预检），
    /// 释放后必须返回 false——否则每次双击启动都会先弹 UAC 再被互斥体
    /// 挡下，白白弹一次提权提示。
    ///
    /// 仅当本测试线程**真正持有**互斥体（`actually_held`）时，释放后才断言
    /// `false`：外部真实实例持锁时锁的释放时机不受测试控制（修订 1.46
    /// 审计——此前在真实实例运行时"释放后无锁"断言必失败，测试环境假红）。
    #[test]
    fn test_pre_flight_detects_running_app() {
        let _guard = TEST_SERIALIZER.lock().unwrap_or_else(|e| e.into_inner());
        let (release, holder, actually_held) = hold_mutex_in_worker();
        // 持锁线程占用期间：预检必须命中"已有实例"。
        assert!(
            pre_flight(),
            "pre_flight must see the mutex held by another thread"
        );
        release_and_join(release, holder);
        if actually_held {
            // 持锁线程已退出、互斥体释放：预检必须放行（且不遗留所有权）。
            assert!(!pre_flight(), "pre_flight must pass after release");
        } else {
            // 外部实例（或 API 不可用）持锁：本测试无法控制其释放时机，
            // 只验证"持锁期间命中"的语义，跳过释放后断言。
            eprintln!("note: skip post-release assert (external lock holder)");
        }
    }

    fn other_kind(s: &SingleInstance) -> &'static str {
        match s {
            SingleInstance::Acquired(_) => "Acquired",
            SingleInstance::Existing => "Existing",
            SingleInstance::Unknown => "Unknown",
        }
    }
}
