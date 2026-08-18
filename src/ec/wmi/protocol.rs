//! MiInterface 线协议（wire format）：纯构造与映射，无 COM 依赖。
//!
//! 从 `wmi/mod.rs`（原单文件 wmi.rs）抽出：命令常量、命令缓冲构造、百分比
//! ⇔ raw code 映射、实例名转义、实例选择策略与熔断判定全部是与 WMI 对象
//! 无关的纯逻辑，可脱离 COM 单独测试。状态（`WmiWorker`）与线程（代理）
//! 留在 `mod.rs`。
//!
//! 本模块还承载 `app::battery` 中原有的 **WMI 充电上限 raw code ⇔ 百分比**
//! 映射（`WMI_CHARGE_LIMITS`/`WMI_PRESET_PERCENTS` 及三个转换函数）——它们
//! 是 WMI 适配器的协议知识，领域层（`app`）不应了解具体后端的线格式。迁移
//! 后 `app::battery` 不再依赖本协议的细节，`ec::wmi` 自洽地拥有该映射。

use crate::app::ec::EcError;

use windows::Win32::System::Wmi::{
    WBEM_E_INVALID_CLASS, WBEM_E_INVALID_METHOD, WBEM_E_INVALID_METHOD_PARAMETERS,
    WBEM_E_INVALID_PARAMETER, WBEM_E_NOT_FOUND, WBEM_E_NOT_SUPPORTED, WBEM_E_PROVIDER_FAILURE,
};

/// WMI rawCode ⇔ 充电限制百分比映射。
/// WMI 仅支持预设值，WinRing0 支持 0-100 连续值。
pub const WMI_CHARGE_LIMITS: &[(u8, u8)] = &[
    (0, 100),
    (1, 80),
    (4, 90),
    (5, 70),
    (6, 60),
    (7, 50),
    (8, 40),
];

/// GUI 以固定档位按钮展示的 WMI 预设充电上限（升序）。
///
/// 与 `WMI_CHARGE_LIMITS` 的 percent 集合完全一致（一致性由测试锁定），
/// 但显式给出 GUI 展示顺序。历史实现把同一份档位列表硬编码在 view.rs，
/// 与映射表重复为两个事实来源——统一收敛到此处。
pub const WMI_PRESET_PERCENTS: &[u8] = &[40, 50, 60, 70, 80, 90, 100];

/// 将 WMI 充电上限 raw code 映射为百分比。
pub fn wmi_rawcode_to_percent(rawcode: u8) -> Option<u8> {
    WMI_CHARGE_LIMITS
        .iter()
        .find(|(r, _)| *r == rawcode)
        .map(|(_, p)| *p)
}

/// 将百分比映射为 WMI 充电上限 raw code（仅预设值可精确映射）。
pub fn percent_to_wmi_rawcode(percent: u8) -> Option<u8> {
    WMI_CHARGE_LIMITS
        .iter()
        .find(|(_, p)| *p == percent)
        .map(|(r, _)| *r)
}

/// 找到最接近的 WMI 预设值。
pub fn nearest_wmi_percent(percent: u8) -> u8 {
    WMI_CHARGE_LIMITS
        .iter()
        .map(|(_, p)| *p)
        .min_by_key(|p| (*p as i16 - percent as i16).abs())
        .expect("WMI_CHARGE_LIMITS is a non-empty compile-time constant")
}

/// MiInterface 命令缓冲长度（字节）。`put_*`/`mi_interface_call`/
/// `SafeArrayCreateVector` 等 10 余处曾散落字面量 `32`，一处修改其余漂移——
/// 统一收敛到此处。
pub(crate) const CMD_BUF_LEN: usize = 32;

/// MiInterface command constants (little-endian bytes)
pub(super) const CMD_READ: u16 = 0xFA00;
pub(super) const CMD_WRITE: u16 = 0xFB00;
pub(super) const FUN2_BATTERY: u16 = 0x1000;
pub(super) const FUN2_PERF: u16 = 0x0800;

/// MiInterface 响应 Status 成功值（本机 2025 RedmiBook Pro 14 实测：
/// 所有成功调用恒返回 0x8000；写入无效值返回 0x0000）。
pub(super) const WMI_STATUS_SUCCESS: u16 = 0x8000;

/// MiInterface 响应有效字段长度：Status(2)+Function(2)+Data0(2)+
/// Data1(4)+Data2(4)+Data3(4) = 18 字节。本机实测 OutData 为 30 字节
/// （MOF OutData MAX=30），历史实现要求 ≥32 字节导致成功响应全被误判。
pub(super) const MIN_OUTPUT_LEN: usize = 18;

/// 方法签名 schema 属性名数组的读取上限：`param_name_from_schema` 从
/// GetNames 的 SAFEARRAY 读取 BSTR 属性名，元素数由提供者声明——真实类
/// 的属性数远小于 64，上限用于拒绝荒谬的元素数声明（避免越界构造切片）。
pub(super) const MAX_SCHEMA_PROPERTY_NAMES: usize = 64;

/// 单次 WMI 调用的"慢"阈值（毫秒）。健康固件 5~16ms，卡死时最长约 3s
/// （超时）——超过该阈值即升级为 warn（"界面冻结/卡顿"高发区间），
/// 便于默认日志直接定位卡在哪条命令（修订 1.47 清理：阈值与日志文案
/// 曾散落同一函数两处）。
pub(super) const SLOW_CALL_WARN_MS: u128 = 500;

/// 向命令缓冲写入小端 u16（`u16::to_le_bytes` 的具名版本，替代手工移位）。
///
/// `buf` 固定为 32 字节（MiInterface 命令缓冲），字段布局由各命令的
/// `put_*` 调用点用编译期常量给出偏移——偏移越界是编程错误，显式断言暴露
/// 而非让切片 panic 以笼统的"index out of bounds"出现。
pub(super) fn put_le16(buf: &mut [u8; CMD_BUF_LEN], offset: usize, val: u16) {
    assert!(
        offset + 2 <= buf.len(),
        "put_le16 offset {offset} out of 32-byte command buffer"
    );
    buf[offset..offset + 2].copy_from_slice(&val.to_le_bytes());
}

/// 向命令缓冲写入小端 u32（如 `write_battery` 的充电上限 raw code）。
pub(super) fn put_le32(buf: &mut [u8; CMD_BUF_LEN], offset: usize, val: u32) {
    assert!(
        offset + 4 <= buf.len(),
        "put_le32 offset {offset} out of 32-byte command buffer"
    );
    buf[offset..offset + 4].copy_from_slice(&val.to_le_bytes());
}

/// 构建 MiInterface 命令缓冲：fun1（读/写命令字）、fun2（功能域，电池/
/// 性能）、fun3（子功能）、可选 fun4（u32 载荷）。
///
/// F-HAL-01/02：读命令 fun1=读(0xFA00)；F-HAL-07：写命令 fun1=写(0xFB00)。
/// 历史实现 `read_battery_cmd`/`write_battery_cmd`/`read_perf_cmd`/
/// `write_perf_cmd` 四个近同函数（`[0u8;32]` + 三次/四次 put_*）各自重复，
/// 统一收敛到此处。纯函数便于单测锁定字节布局（命令字段错位曾在真机
/// 造成限值解析错乱）。
pub(super) fn build_cmd(fun1: u16, fun2: u16, fun3: u16, fun4: u32) -> [u8; CMD_BUF_LEN] {
    let mut buf = [0u8; CMD_BUF_LEN];
    put_le16(&mut buf, 0, fun1);
    put_le16(&mut buf, 2, fun2);
    put_le16(&mut buf, 4, fun3);
    if fun4 != 0 {
        put_le32(&mut buf, 6, fun4);
    }
    buf
}

pub(super) fn read_battery_cmd() -> [u8; CMD_BUF_LEN] {
    build_cmd(CMD_READ, FUN2_BATTERY, 0x0002, 0)
}

pub(super) fn write_battery_cmd(raw_code: u8) -> [u8; CMD_BUF_LEN] {
    build_cmd(CMD_WRITE, FUN2_BATTERY, 0x0002, raw_code as u32)
}

pub(super) fn read_perf_cmd() -> [u8; CMD_BUF_LEN] {
    build_cmd(CMD_READ, FUN2_PERF, 0x0000, 0)
}

pub(super) fn write_perf_cmd(mode: u8) -> [u8; CMD_BUF_LEN] {
    build_cmd(CMD_WRITE, FUN2_PERF, mode as u16, 0)
}

/// 将百分比换算为 WMI 充电上限 raw code：`nearest_wmi_percent` 恒返回预设值
/// （WMI_CHARGE_LIMITS 的 percent 集合），因此映射必然命中——历史实现先做
/// 一次精确匹配、失败再就近匹配，而精确匹配的结果与"就近匹配取该值"完全相同
/// （预设值经 min_by_key 原样返回），第一分支是冗余的。
///
/// 返回 `Option` 而非直接解包：映射必然成功的先决条件由测试
/// （test_wmi_preset_percents_match_mapping_table 等）锁定。若未来编辑表格
/// 打破不变量，这里如实返回 `None` 由调用方产生 `InvalidData` 错误，而不是
/// 像历史实现那样 `.unwrap_or(0)` 静默把输入当成 100%（raw code 0）写入。
pub(super) fn wmi_rawcode_for_percent(percent: u8) -> Option<u8> {
    percent_to_wmi_rawcode(nearest_wmi_percent(percent))
}

/// WMI 对象路径中的字符串值转义：反斜杠与引号需加倍（Meow-Box 的实例路径
/// 亦为 `MICommonInterface.InstanceName="ACPI\\PNP0C14\\MIFS_0"` 形式）。
pub(super) fn escape_instance_name(name: &str) -> String {
    name.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 目标实例选择策略（F-HAL-08c）：**active 且 InstanceName 含 "MIFS"**
/// 的实例优先（Meow-Box 同款策略，本机实测 MIFS_0），否则取第一个。
/// 纯函数便于单测锁定选择策略（此前只在真机 `#[ignore]` 冒烟测试覆盖）。
pub(super) fn select_target_instance(instances: &[(String, bool, bool)]) -> Option<&str> {
    instances
        .iter()
        .find(|(_, active, is_mifs)| *active && *is_mifs)
        .or_else(|| instances.first())
        .map(|(name, _, _)| name.as_str())
}

/// 是否属于确定性致命错误（应熔断）：固件/提供程序层面的确定性失败，
/// 重试必然再次失败。瞬态错误（超时、服务忙、连接中断等）不熔断，
/// 否则 WMI 服务重启等临时故障会永久禁用后端。
pub(super) fn is_latching_hresult(hr: u32) -> bool {
    const FATAL: &[u32] = &[
        // WBEM_E_INVALID_METHOD_PARAMETERS (0x8004102F)：对**类路径**调用
        // MiInterface 时恒被 WinMgmt 以此拒绝（1~64 字节输入全部复现）。
        // 正确实现（实例调用）不会出现该错误，保留在列表作为防御。
        WBEM_E_INVALID_METHOD_PARAMETERS.0 as u32,
        WBEM_E_PROVIDER_FAILURE.0 as u32,
        // 类/方法层面不存在或不受支持：机器不支持该接口，重试不会成功。
        WBEM_E_INVALID_CLASS.0 as u32,
        WBEM_E_NOT_FOUND.0 as u32,
        WBEM_E_INVALID_METHOD.0 as u32,
        WBEM_E_NOT_SUPPORTED.0 as u32,
        WBEM_E_INVALID_PARAMETER.0 as u32,
    ];
    FATAL.contains(&hr)
}

/// 将错误写入熔断状态（hr 为确定性致命错误或 None 表示必然失败时），
/// 返回原始错误。worker 线程独占 state，无需锁。
pub(super) fn latch_into(state: &mut Option<EcError>, hr: Option<u32>, err: EcError) -> EcError {
    let fatal = match hr {
        None => true,
        Some(hr) => is_latching_hresult(hr),
    };
    if fatal && state.is_none() {
        log::error!(
            "WMI: latching fatal error '{}'; subsequent calls fail fast",
            err
        );
        *state = Some(err.clone());
    }
    err
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WMI raw code → 百分比：预设代码全部命中。
    #[test]
    fn test_wmi_rawcode_to_percent_valid() {
        assert_eq!(wmi_rawcode_to_percent(0), Some(100));
        assert_eq!(wmi_rawcode_to_percent(1), Some(80));
        assert_eq!(wmi_rawcode_to_percent(4), Some(90));
        assert_eq!(wmi_rawcode_to_percent(5), Some(70));
        assert_eq!(wmi_rawcode_to_percent(6), Some(60));
        assert_eq!(wmi_rawcode_to_percent(7), Some(50));
        assert_eq!(wmi_rawcode_to_percent(8), Some(40));
    }

    /// WMI raw code → 百分比：未定义代码返回 None。
    #[test]
    fn test_wmi_rawcode_to_percent_invalid() {
        assert_eq!(wmi_rawcode_to_percent(2), None);
        assert_eq!(wmi_rawcode_to_percent(3), None);
        assert_eq!(wmi_rawcode_to_percent(9), None);
        assert_eq!(wmi_rawcode_to_percent(10), None);
        assert_eq!(wmi_rawcode_to_percent(0xFF), None);
    }

    /// 百分比 → WMI raw code：仅预设值可精确映射。
    #[test]
    fn test_percent_to_wmi_rawcode_valid() {
        assert_eq!(percent_to_wmi_rawcode(100), Some(0));
        assert_eq!(percent_to_wmi_rawcode(80), Some(1));
        assert_eq!(percent_to_wmi_rawcode(90), Some(4));
        assert_eq!(percent_to_wmi_rawcode(70), Some(5));
        assert_eq!(percent_to_wmi_rawcode(60), Some(6));
        assert_eq!(percent_to_wmi_rawcode(50), Some(7));
        assert_eq!(percent_to_wmi_rawcode(40), Some(8));
    }

    /// 百分比 → WMI raw code：非预设值返回 None。
    #[test]
    fn test_percent_to_wmi_rawcode_invalid() {
        assert_eq!(percent_to_wmi_rawcode(0), None);
        assert_eq!(percent_to_wmi_rawcode(10), None);
        assert_eq!(percent_to_wmi_rawcode(30), None);
        assert_eq!(percent_to_wmi_rawcode(55), None);
        assert_eq!(percent_to_wmi_rawcode(85), None);
        assert_eq!(percent_to_wmi_rawcode(95), None);
        assert_eq!(percent_to_wmi_rawcode(100), Some(0));
    }

    /// 就近映射到预设值：精确命中。
    #[test]
    fn test_nearest_wmi_percent_exact() {
        assert_eq!(nearest_wmi_percent(40), 40);
        assert_eq!(nearest_wmi_percent(50), 50);
        assert_eq!(nearest_wmi_percent(60), 60);
        assert_eq!(nearest_wmi_percent(70), 70);
        assert_eq!(nearest_wmi_percent(80), 80);
        assert_eq!(nearest_wmi_percent(90), 90);
        assert_eq!(nearest_wmi_percent(100), 100);
    }

    /// 就近预设到预设值：四舍五入行为。
    #[test]
    fn test_nearest_wmi_percent_rounding() {
        assert_eq!(nearest_wmi_percent(85), 80);
        assert_eq!(nearest_wmi_percent(84), 80);
        assert_eq!(nearest_wmi_percent(86), 90);
        assert_eq!(nearest_wmi_percent(45), 50);
        assert_eq!(nearest_wmi_percent(55), 60);
        assert_eq!(nearest_wmi_percent(65), 70);
        assert_eq!(nearest_wmi_percent(75), 80);
        assert_eq!(nearest_wmi_percent(95), 100);
    }

    /// 就近映射边界：0 与超上限值。
    #[test]
    fn test_nearest_wmi_percent_boundary() {
        assert_eq!(nearest_wmi_percent(0), 40);
        assert_eq!(nearest_wmi_percent(200), 100);
    }

    /// 映射表完整性：7 个 raw code 与 7 个百分比，均互不重复。
    #[test]
    fn test_wmi_charge_limits_table_completeness() {
        assert_eq!(WMI_CHARGE_LIMITS.len(), 7);
        let codes: std::collections::HashSet<u8> =
            WMI_CHARGE_LIMITS.iter().map(|(r, _)| *r).collect();
        assert_eq!(codes.len(), 7);
        let percents: std::collections::HashSet<u8> =
            WMI_CHARGE_LIMITS.iter().map(|(_, p)| *p).collect();
        assert_eq!(percents.len(), 7);
    }

    /// 双向映射一致性：每个 (raw code, percent) 对两个方向都命中。
    #[test]
    fn test_wmi_rawcode_to_percent_bidirectional() {
        for (rawcode, percent) in WMI_CHARGE_LIMITS {
            assert_eq!(percent_to_wmi_rawcode(*percent), Some(*rawcode));
            assert_eq!(wmi_rawcode_to_percent(*rawcode), Some(*percent));
        }
    }

    /// 回归测试：GUI 展示的 WMI 预设档位（WMI_PRESET_PERCENTS）必须与
    /// 映射表（WMI_CHARGE_LIMITS）的 percent 集合完全一致。历史实现把
    /// 同一份档位列表硬编码在 view.rs，与映射表重复为两个事实来源，
    /// 一处修改另一处漂移。
    #[test]
    fn test_wmi_preset_percents_match_mapping_table() {
        let table: std::collections::HashSet<u8> =
            WMI_CHARGE_LIMITS.iter().map(|(_, p)| *p).collect();
        let presets: std::collections::HashSet<u8> = WMI_PRESET_PERCENTS.iter().copied().collect();
        assert_eq!(
            presets, table,
            "view presets must equal mapping table percents"
        );
    }
}
