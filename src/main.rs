#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 二进制入口：启动流程（日志、提权、后端初始化、GUI 事件循环）全部在
//! 库目标的 `launch::run` 中，本文件只保留 `windows_subsystem` 属性与对
//! 库的调用。

fn main() {
    xiaomi_pc_manager_lite::run();
}
