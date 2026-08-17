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

use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;

/// 会话级命名互斥体（`Local\` 前缀，仅当前登录会话可见）。
/// 同用户的提权进程与非提权进程共享同一会话，可访问同一个互斥体。
const INSTANCE_MUTEX_NAME: &str = "Local\\XiaomiPcManagerLite";

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
pub fn acquire() -> SingleInstance {
    let (_buf, name) = crate::util::to_pcwstr(INSTANCE_MUTEX_NAME);
    unsafe {
        // bInitialOwner=true：新创建则当前线程即所有者；已存在则
        // GetLastError 返回 ERROR_ALREADY_EXISTS。
        let handle = match CreateMutexW(None, true, name) {
            Ok(h) => h,
            Err(_) => {
                log::warn!("Single instance mutex: CreateMutexW failed; proceeding");
                return SingleInstance::Unknown;
            }
        };
        let already_exists = GetLastError().0 == ERROR_ALREADY_EXISTS.0;
        if already_exists {
            // 已有实例持有互斥体（或持有者崩溃后对象尚未清理——内核会在
            // 持有进程退出时释放对象，此处短暂的重叠窗口忽略）。
            let _ = CloseHandle(handle);
            log::info!("Another instance is already running");
            SingleInstance::Existing
        } else {
            SingleInstance::Acquired(SingleInstanceGuard(handle))
        }
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

    /// 同一进程内两次 acquire：第一次拿到所有权，第二次必须判定为
    /// "已有实例"。若测试环境里恰好跑着一个真实实例（互斥体被占用），
    /// 第一次即返回 Existing，断言自动跳过，不产生假阳性。
    #[test]
    fn test_second_acquire_detects_existing_instance() {
        let _serial = TEST_SERIALIZER.lock().unwrap_or_else(|e| e.into_inner());
        match acquire() {
            SingleInstance::Acquired(guard) => {
                let second = acquire();
                assert!(
                    matches!(second, SingleInstance::Existing),
                    "second acquire in the same process must conflict"
                );
                drop(guard);
            }
            SingleInstance::Existing => {
                eprintln!("skip: a real app instance is running in this session");
            }
            SingleInstance::Unknown => {
                eprintln!("skip: mutex API unavailable");
            }
        }
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
            other => panic!("expected acquired after release, got {:?}", other_kind(&other)),
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