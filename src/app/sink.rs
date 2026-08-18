//! 后台线程 → GUI 事件循环的命令端口。
//!
//! 托盘/Fn 监听/电池健康/WMI 恢复探测等后台线程需要把 `UiCommand` 投递给
//! GUI 线程并唤醒其事件循环。历史实现把这两个能力写成两个入参
//! （`mpsc::Sender<UiCommand>` + `egui::Context`），使后台/领域线程直接依赖
//! GUI 框架（egui）。`CommandSink` 把"发送命令 + 唤醒"收敛为单个端口，
//! 由 GUI 层提供实现（持有发送端与 `egui::Context`），后台线程只依赖本 trait。
//!
//! 所有后台线程（托盘 / Fn 监听 / 电池健康）统一经本端口与 GUI 通信；
//! `send` 返回 `Result`，使需要感知"GUI 已销毁"的生产者（如电池健康线程
//! 借此优雅停止轮询）与只需投递的消费者（托盘/Fn 的 `dispatch`）共用同一端口。

use crate::app::command::UiCommand;

/// 后台线程发送 UI 命令并唤醒 GUI 事件循环的能力。
///
/// `send` 只投递命令；`wake` 请求立即重绘（egui 的 mpsc 不唤醒事件循环，
/// 投递后不 `request_repaint` 则命令最长要等一个 500ms 定时帧才被消费）。
/// 实现方把两者绑定在同一对象上，调用方无需关心唤醒细节。
pub trait CommandSink: Send + Sync {
    /// 投递命令。`Ok(())` = 已投递；`Err(SendError)` = 通道已关闭（GUI 已销毁）。
    ///
    /// 返回结果供需要感知"GUI 是否仍存活"的生产者（如电池健康线程借此
    /// 停止轮询）使用；只投递不关心结果的调用方用 `CommandSinkExt::dispatch`。
    fn send(&self, command: UiCommand) -> Result<(), std::sync::mpsc::SendError<UiCommand>>;
    fn wake(&self);
}

/// 便捷方法：投递并唤醒（托盘/监听线程发送命令的标准语义，忽略投递结果）。
pub trait CommandSinkExt: CommandSink {
    fn dispatch(&self, command: UiCommand) {
        let _ = self.send(command);
        self.wake();
    }
}

impl<T: CommandSink + ?Sized> CommandSinkExt for T {}
