//! Windows 互操作基础设施层（最低层，只依赖 `util`，供 `ec`/`platform`
//! 两个适配器层复用）。
//!
//! 历史遗留：crate 根存在 `wmi_util.rs`（连接样板 + VARIANT 工具混放），
//! `ec`（WMI 后端、Fn 监听）与 `platform`（电池健康、自启动）各自跨到 crate
//! 根依赖它——共享依赖没有归属层，分层图无法表达。收敛为：
//! - [`com`]：COM 公寓生命周期（`ComScope`）、`root\wmi` 连接工厂、WQL 查询
//!   与枚举、SAFEARRAY 边界工具；
//! - [`variant`]：VARIANT 的 RAII 承载与各类型属性读取（字符串/布尔/uint）。
//!
//! 依赖方向：`ec` → `win` → `util`，`platform` → `win` → `util`，均单向无环。

pub mod com;
pub mod variant;

pub use com::{
    connect_root_wmi, exec_query, next_instance, safe_array_len, select_all_wql, ComScope,
};
pub use variant::{
    bstr_from_variant, get_bool_prop, get_property, get_string_prop, uint_prop, uint_rate_prop,
};
