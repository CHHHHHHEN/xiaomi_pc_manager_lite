//! 测试共用的临时配置目录工具（仅测试编译）。

use crate::app::config::ConfigStore;

/// 返回指向独立临时目录的 `ConfigStore`。
///
/// gui/app.rs 与 gui/commands.rs 的测试各自实现过一份"静态序号 + 按调用次数
/// 唯一命名的临时目录"样板，存在漂移风险——统一收敛到此处。目录按调用次数
/// 唯一命名：cargo test 并行运行多个用例，若共用同一目录，各用例的 config
/// save 会在同一 config.toml 上交错写入（虽然写入已原子化，目录共享仍会
/// 造成用例间互相污染与潜在 flaky）。`save_state` 永不触碰用户的真实配置。
pub(crate) fn temp_store(prefix: &str) -> ConfigStore {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("xmpl-{}-{}-{}", prefix, std::process::id(), seq));
    ConfigStore::from_dir(dir)
}
