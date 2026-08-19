//! Fn 功能键绑定的**领域模型**（纯逻辑，无 WMI/GUI 依赖）。
//!
//! 历史实现把模型与 WMI 事件监听混在 `ec::fnkey`（其中 `spawn` 直接依赖
//! `egui::Context`）。按职责切分：
//! - 本模块（`app::fnkey`）：绑定表模型、hex 归一化/校验、键身份/事件判定、
//!   捕获去重、退避节奏、命令脱敏——全部纯函数，可脱离平台单测；
//! - `ec::fn_watcher`：WMI 事件订阅与派发（适配器，经 `app::sink::CommandSink`
//!   回传命令）。
//!
//! 事件类参考（Meow-Box / 本机 2025 RedmiBook Pro 14 实证）：HID_EVENT20
//! 承载 Fn+K 等按键报告；其余类（HID_EVENT21-23、WMIEvent）在不同机型/固件
//! 上承载不同的功能键事件。

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::app::command::UiCommand;

/// Fn+K 所在的 OEM ACPI 事件类（F-FNK-01）。
pub const FN_K_WMI_CLASS: &str = "HID_EVENT20";

/// Fn+K 按下事件的 ReportHex 前缀：`01-28-01`（固定前缀 `01` + 键码
/// `28` + 按下状态 `01`，见 F-FNK-04）。释放事件（`012800`）不命中
/// 此前缀，一次物理按键恰好派发一次切换（F-FNK-06）。
pub const FN_K_PRESS_PREFIX: &str = "012801";

/// Fn 键可绑定的动作（枚举名即配置文本，不得破坏性改名）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FnAction {
    /// 循环切换性能模式（Smart → Quiet → Extreme → Smart）。
    CyclePerfMode,
    /// 切换电池养护启用状态。
    ToggleBatteryCare,
    /// 把持久化配置整份重新应用到硬件（与"电源切换时自动重设"同一路径）。
    ReapplyConfig,
    /// 运行用户自定义命令（脚本 / 打开程序，见 `FnKeyBinding.command`）。
    /// 命令在**独立进程**中执行（`std::process::Command`），不阻塞 WMI
    /// 事件监听循环。
    RunCommand,
    /// 绑定保留但禁用（无动作时被消费、不派发命令）。
    None,
}

impl FnAction {
    pub fn name(&self) -> &'static str {
        match self {
            Self::CyclePerfMode => "循环切换性能模式",
            Self::ToggleBatteryCare => "切换电池养护",
            Self::ReapplyConfig => "重新应用设置",
            Self::RunCommand => "运行自定义命令",
            Self::None => "无动作",
        }
    }

    pub fn all() -> &'static [FnAction] {
        &[
            Self::CyclePerfMode,
            Self::ToggleBatteryCare,
            Self::ReapplyConfig,
            Self::RunCommand,
            Self::None,
        ]
    }

    /// 动作对应的 UI 命令；`RunCommand`/`None` 时返回 None（绑定仅消费
    /// 事件，不派发——前者直接执行命令，后者无动作）。
    /// 按 Rust 惯例命名：`as_*` 表示"便宜的借用读取"（`&self`），而 `to_*`
    /// 保留给消耗型转换——这里返回轻量 `Option<UiCommand>`，用 `as_` 前缀
    /// 顺带消除 clippy 的 `wrong_self_convention` 告警。
    pub fn as_ui_command(&self) -> Option<UiCommand> {
        match self {
            Self::CyclePerfMode => Some(UiCommand::CyclePerfMode),
            Self::ToggleBatteryCare => Some(UiCommand::ToggleBatteryCare),
            // 用户主动绑定"重新应用设置"是手动动作，不应受"电源切换时自动
            // 重设"开关门控——开关关闭时被动重设（电源广播）静默忽略是对的，
            // 但用户按下绑定的功能键却毫无反应是不可接受。
            Self::ReapplyConfig => Some(UiCommand::ReapplyConfigManual),
            Self::RunCommand | Self::None => None,
        }
    }
}

/// 一条 Fn 功能键绑定：事件类 + 报告前缀 → 动作（可能带自定义命令）。
///
/// 前缀匹配（`normalize_hex` 后 starts_with）：绑定的 `prefix` 是归一化
/// 十六进制（如 `012801`），事件报告归一化后以此为前缀即命中。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FnKeyBinding {
    /// OEM ACPI 事件类（如 `HID_EVENT20`）。
    pub class: String,
    /// 归一化事件前缀（如 `012801`）。
    pub prefix: String,
    /// 命中后派发的动作。
    pub action: FnAction,
    /// `RunCommand` 动作的命令行（其余动作忽略）。`None`/空串 = 未配置；
    /// 配置向后兼容：旧配置文件无此字段时反序列化为 `None`（`#[serde(default)]`）。
    #[serde(default)]
    pub command: Option<String>,
}

impl FnKeyBinding {
    /// 默认的 Fn+K 绑定（与历史硬编码行为完全一致）。
    pub fn fn_k() -> Self {
        Self {
            class: FN_K_WMI_CLASS.to_string(),
            prefix: FN_K_PRESS_PREFIX.to_string(),
            action: FnAction::CyclePerfMode,
            command: None,
        }
    }

    /// GUI 展示标签，如 `HID_EVENT20 / 01-28-01`。
    pub fn label(&self) -> String {
        binding_label(&self.class, &self.prefix)
    }

    /// 归一化 hex → 带分隔符可读形式（`012801` → `01-28-01`），便于与
    /// 用户观察到的按键编码对照。
    pub fn display_prefix(prefix: &str) -> String {
        let p = normalize_hex(prefix);
        // normalize_hex 只保留 ASCII 十六进制字符，2 字节分块必为合法 UTF-8；
        // 不可达的 "??" 兜底只会掩盖编程错误，改为显式断言。
        p.as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).expect("normalized hex chunk is ASCII"))
            .collect::<Vec<_>>()
            .join("-")
    }
}

/// `class / dashed-prefix` 展示标签（如 `HID_EVENT20 / 01-28-01`）。
///
/// `FnKeyBinding::label`、捕获行（view.rs 的"最近捕获"）、绑定列表行各自
/// 手写过同一形状的 `format!`（其中绑定列表曾为取 label 而临时构造整条
/// `FnKeyBinding`）——统一收敛到此处（修订 1.50 整理）。
pub fn binding_label(class: &str, prefix: &str) -> String {
    format!("{} / {}", class, FnKeyBinding::display_prefix(prefix))
}

/// 默认的功能键绑定（Fn+K → 循环切换性能）。
pub fn default_bindings() -> Vec<FnKeyBinding> {
    vec![FnKeyBinding::fn_k()]
}

/// 共享绑定表：GUI 线程写（保存配置时同步更新）、监听线程读。
pub type SharedBindings = Arc<RwLock<Vec<FnKeyBinding>>>;

/// 已知的 Fn 键目录（用于 GUI"添加绑定"预设与捕获提示）。
///
/// 键码来自 Meow-Box 项目（Xiaomi Book Pro 14）与 F-FNK 文档（2025
/// RedmiBook Pro 14 实测）。同一事件类内不同键的编码；前缀取"按下"事件
/// 的最短可区分字节（含按下状态字节），避免释放/状态变化事件重复命中。
#[derive(Debug, Clone, Copy)]
pub struct KnownFnKey {
    /// 事件类名（`HID_EVENT20` 等）。
    pub class: &'static str,
    /// 归一化前缀。
    pub prefix: &'static str,
    /// 中文名。
    pub name: &'static str,
}

pub const KNOWN_FN_KEYS: &[KnownFnKey] = &[
    // Fn+K：与 FN_K_WMI_CLASS / FN_K_PRESS_PREFIX 常量同源（修订 1.50 收敛，
    // 见 test_known_fn_k_matches_constants 锁定）——历史实现把 "HID_EVENT20" /
    // "012801" 在此硬编码为字面量，常量改动不会同步到预设下拉。
    KnownFnKey {
        class: FN_K_WMI_CLASS,
        prefix: FN_K_PRESS_PREFIX,
        name: "Fn+K 性能模式",
    },
    KnownFnKey {
        class: "HID_EVENT20",
        prefix: "012501",
        name: "PC Manager 键",
    },
    KnownFnKey {
        class: "HID_EVENT20",
        prefix: "012301",
        name: "小爱同学 (F7)",
    },
    KnownFnKey {
        class: "HID_EVENT20",
        prefix: "011B01",
        name: "设置 (F9)",
    },
    KnownFnKey {
        class: "HID_EVENT20",
        prefix: "010101",
        name: "投影 (F8)",
    },
    KnownFnKey {
        class: "HID_EVENT20",
        prefix: "012101",
        name: "麦克风静音 (F4)",
    },
    KnownFnKey {
        class: "HID_EVENT20",
        prefix: "010701",
        name: "Fn 锁 (Fn+Esc)",
    },
    KnownFnKey {
        class: "HID_EVENT20",
        prefix: "010901",
        name: "大写锁定",
    },
];

/// 事件 hex 统一归一化：剔除所有非字母数字字符（如 "01-28-01" 的分隔符）
/// 并转大写。EventDetail 字节路径生成的是大写十六进制，但 ReportHex 字符串
/// 回退路径的字母大小写由固件决定，可能是小写——不归一化会导致小写报告
/// 永远匹配不上（F-FNK-04）。
///
/// 归一化 hex 到大写无分隔形式：剔除分隔符等非十六进制字符并转大写。
/// 只保留 `[0-9A-F]`——事件报告与绑定前缀都是 hex，混入其它字母
/// （如 G-Z）既不可能匹配真实事件，也会让"非 hex 输入"检测失效。
/// 不含任何十六进制字符时返回空串（调用方据此拒绝非法输入）。
pub fn normalize_hex(report_hex: &str) -> String {
    report_hex
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// 校验 Fn 绑定前缀是否安全可用。
///
/// 空串匹配一切事件、单字节（长度 1）前缀在归一化后大多以 `0` 开头会匹配
/// 几乎全部同类型事件（如 `"0"`），都属于危险配置，一律拒绝。合法前缀必须
/// 能构成**至少一个完整字节**（偶数长度）。
pub fn valid_prefix(prefix: &str) -> bool {
    let p = normalize_hex(prefix);
    p.len() >= 2 && p.len().is_multiple_of(2)
}

/// 校验 Fn 绑定的 WMI 事件类名是否为合法 WQL 标识符。
///
/// 类名会被原样拼进 `SELECT * FROM {}`（WQL），只允许
/// `[A-Za-z_][A-Za-z0-9_]*`——否则手改配置注入 WHERE 子句可改变订阅
/// 语义、甚至让监听线程在管理员权限下订阅并记录任意事件流。
///
/// 校验按 `trim()` 后的类名进行；存储方（config 消毒 / GUI 添加绑定）须
/// **先 trim 再存**，否则校验通过的带空白类名永远匹配不到 WMI 订阅类。
pub fn valid_class(class: &str) -> bool {
    let mut chars = class.trim().chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 事件报告的状态字节定义（本机 2025 RedmiBook Pro 14 实证：Fn+K 按下
/// `012801`、释放 `012800`）。
///
/// 事件报告统一为 `<01><键码><状态>`——**状态字节位于事件第 3 字节**（hex
/// 偏移 `4..6`），`01`=按下、`00`=释放。捕获去重与监听派发曾各自以不同
/// 谓词编码同一条规则（一处用 `ends_with`、一处用 `[4..6]`），报告格式一旦
/// 变化两处会不同步——统一收敛到这两个谓词。
pub fn is_press_report(normalized: &str) -> bool {
    normalized.len() >= 6 && &normalized[4..6] == "01"
}

/// 事件报告是否为"释放"事件（见 `is_press_report` 的报告格式约定）。
pub fn is_release_report(normalized: &str) -> bool {
    normalized.len() >= 6 && &normalized[4..6] == "00"
}

/// 事件报告的"键码身份"：去掉按下/释放状态字节（hex 偏移 `4..6`，见
/// `is_press_report`）后剩余部分。
///
/// 捕获去重与监听侧"保持按下事件"各自用"去掉**末**字节"比较同键码——状态
/// 字节位于第 3 字节，报告**恰好 3 字节**时末字节就是状态字节、两者重合；
/// 报告更长（如含扩展字段）时末字节并非状态字节，去重判定会漂移。统一
/// 收敛到本函数：精确去掉 `[4..6]` 区域，无论报告多长身份一致。长度不足
/// 6（异常输入）退化为原样返回。
pub fn key_without_status_byte(normalized: &str) -> String {
    if normalized.len() >= 6 {
        format!("{}{}", &normalized[..4], &normalized[6..])
    } else {
        normalized.to_string()
    }
}

/// 事件是否为"该绑定前缀的释放事件"（F-FNK-06 守卫）。
///
/// 绑定前缀若**短于 3 字节**（部分前缀，未覆盖状态字节，如 2 字节 `0125`），
/// 按下与释放均会以它为前缀命中——一次物理按键被派发两次动作（回归修复：
/// 预设键码曾用 2 字节前缀，绑定"切换电池养护"= 按下+释放恒 no-op、绑定
/// "循环性能模式"= 一次按键跳两档）。此时若状态字节为 `00`（释放）则跳过
/// （F-FNK-06：释放不得触发动作）；前缀 ≥3 字节（已含状态字节）时前缀本身
/// 即可区分按下/释放，无需守卫（显式绑定释放事件也放行）。
pub fn release_state_after_prefix(prefix: &str, normalized: &str) -> bool {
    prefix.len() <= 4 && is_release_report(normalized)
}

/// 捕获事件转发限流闸门。
///
/// 返回是否允许本次转发。**按"键身份"去重而非全局窗口**：同一物理按键的
/// 按下/释放/自动重复事件（相同 class + 去掉状态字节的 hex，如 `012801`/
/// `012800` 身份同为 `0128`）在窗口内只转发第一条——按住触发固件自动重复
/// 时不再每事件唤醒 GUI；而**不同**按键（身份不同）即使 150ms 内连续按下
/// 也各自放行，捕获工具不会漏键。线程局部存储（监听线程专用），身份首次
/// 出现必然放行。
const CAPTURE_FORWARD_MIN_MS: u128 = 150;

pub fn capture_event_gate(class: &str, normalized: &str) -> bool {
    thread_local! {
        static LAST_FORWARD: std::cell::RefCell<std::collections::HashMap<String, std::time::Instant>> =
            std::cell::RefCell::new(std::collections::HashMap::new());
    }
    // 键身份 = 类名 + 去掉状态字节的 hex（统一经 key_without_status_byte）。
    let identity = format!("{}/{}", class, key_without_status_byte(normalized));
    LAST_FORWARD.with(|last| {
        let now = std::time::Instant::now();
        // 清理过期条目（窗口外的身份不再占用内存；身份数 = 窗口内不同键数）。
        last.borrow_mut()
            .retain(|_, t| now.duration_since(*t).as_millis() < CAPTURE_FORWARD_MIN_MS);
        let mut map = last.borrow_mut();
        let allow = match map.get(&identity) {
            Some(prev) => now.duration_since(*prev).as_millis() >= CAPTURE_FORWARD_MIN_MS,
            None => true,
        };
        if allow {
            map.insert(identity, now);
        }
        allow
    })
}

/// Fn 无事件类时的退避节奏（秒数，F-FNK-07）。
///
/// 前 3 次连续无类 5s（服务刚启动/驱动加载较晚时快速重试）、随后 30s、
/// 超过 20 次降为 60s 长探测——硬件上确无这些 OEM 事件类（VM/非小米机型）时
/// 类永远不会出现，长探测保留恢复通道并压住日志频率。
/// 纯函数便于单测锁定节奏。
pub fn no_event_classes_backoff_secs(stale_cycles: u32) -> u64 {
    if stale_cycles <= 3 {
        5
    } else if stale_cycles <= 20 {
        30
    } else {
        60
    }
}

/// 日志脱敏：命令可能携带凭据（`net use`、`ssh -i` 等），info 级日志永远
/// 写入日志文件，必须截断而非全文落盘。保留前 32 字符 + 总长度提示，足够
/// 排查"哪条命令被执行"又不泄露完整内容；调试级仍可输出全文。
pub fn redact_command(command: &str) -> String {
    const MAX: usize = 32;
    if command.chars().count() <= MAX {
        return command.to_string();
    }
    let head: String = command.chars().take(MAX).collect();
    format!("{}... ({} chars)", head, command.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认绑定表必须包含 Fn+K → 循环切换性能（与历史行为一致）。
    #[test]
    fn test_default_bindings_has_fn_k() {
        let b = default_bindings();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].class, "HID_EVENT20");
        assert_eq!(b[0].prefix, "012801");
        assert_eq!(b[0].action, FnAction::CyclePerfMode);
    }

    /// 归一化必须能匹配带分隔符/大小写的报告。
    #[test]
    fn test_normalize_hex() {
        assert_eq!(normalize_hex("01-28-01 00 00"), "0128010000");
        assert_eq!(normalize_hex("012801ffff"), "012801FFFF");
        assert_eq!(normalize_hex(""), "");
    }

    /// 前缀校验：空/单字节/奇数长度都是危险或无效配置；至少一个完整字节
    /// 才合法。类名校验：只允许合法 WQL 标识符。
    #[test]
    fn test_valid_prefix_and_class() {
        assert!(valid_prefix("012801"));
        assert!(valid_prefix("01-28-01"));
        assert!(valid_prefix("AB"));
        assert!(!valid_prefix(""));
        assert!(!valid_prefix("0"));
        assert!(!valid_prefix("012"));
        assert!(!valid_prefix("XYZ"));

        assert!(valid_class("HID_EVENT20"));
        assert!(valid_class("_MyClass2"));
        assert!(!valid_class(""));
        assert!(!valid_class("HID_EVENT20 WHERE Foo=1"));
        assert!(!valid_class("HID-EVENT20"));
        assert!(!valid_class("1CLASS"));
        assert!(!valid_class("HID EVENT"));
    }

    /// 捕获事件转发限流：同一键身份首次必然放行、窗口内重复丢弃；**不同
    /// 按键**即使窗口内也各自放行。
    #[test]
    fn test_capture_event_gate_rate_limits() {
        assert!(
            capture_event_gate("HID_EVENT20", "012801"),
            "first event must pass"
        );
        assert!(
            !capture_event_gate("HID_EVENT20", "012800"),
            "release of same key within window must be dropped"
        );
        assert!(
            !capture_event_gate("HID_EVENT20", "012801"),
            "auto-repeat of same key within window must be dropped"
        );
        assert!(
            capture_event_gate("HID_EVENT20", "010701"),
            "a different key within the window must still pass"
        );
    }

    /// 状态字节定位：状态字节固定位于事件第 3 字节（hex 偏移 `4..6`），不是
    /// 末字节——报告恰好 3 字节时两者重合，更长报告去掉末字节会误判键身份。
    #[test]
    fn test_key_without_status_byte_handles_long_reports() {
        assert_eq!(key_without_status_byte("012801"), "0128");
        assert_eq!(key_without_status_byte("012800"), "0128");
        assert_eq!(key_without_status_byte("012801FF"), "0128FF");
        assert_eq!(key_without_status_byte("012800FF"), "0128FF");
        assert_eq!(key_without_status_byte("0128"), "0128");
        assert_eq!(key_without_status_byte(""), "");
    }

    /// 释放事件守卫的纯函数：前缀后紧跟 `00` 且前缀未覆盖完整事件 → 释放；
    /// 前缀与事件等长（完整事件绑定，含显式释放绑定）→ 非释放。
    #[test]
    fn test_release_state_after_prefix() {
        assert!(release_state_after_prefix("0125", "01250000"));
        assert!(!release_state_after_prefix("0125", "01250100"));
        assert!(!release_state_after_prefix("012501", "01250100"));
        assert!(!release_state_after_prefix("012800", "012800"));
        assert!(!release_state_after_prefix("012801", "012801"));
    }

    /// FnKeyBinding 序列化向后兼容：旧配置（无 command 字段）反序列化为
    /// command=None；新配置含 command 时往返一致。
    #[test]
    fn test_binding_command_serde_backward_compat() {
        let old = r#"class = "HID_EVENT20"
prefix = "012801"
action = "CyclePerfMode""#;
        let b: FnKeyBinding = toml::from_str(old).expect("legacy binding must parse");
        assert_eq!(b.action, FnAction::CyclePerfMode);
        assert_eq!(b.command, None);

        let new = FnKeyBinding {
            class: "HID_EVENT20".into(),
            prefix: "012801".into(),
            action: FnAction::RunCommand,
            command: Some(r#"start "" "C:\path with space\tool.exe""#.into()),
        };
        let s = toml::to_string(&new).expect("serialize");
        let parsed: FnKeyBinding = toml::from_str(&s).expect("deserialize");
        assert_eq!(parsed, new);
    }

    /// 展示形式：归一化前缀 → 带分隔符。
    #[test]
    fn test_display_prefix() {
        assert_eq!(FnKeyBinding::display_prefix("012801"), "01-28-01");
        assert_eq!(FnKeyBinding::display_prefix("0107"), "01-07");
    }

    /// 已知功能键目录：编码来自 Meow-Box（HID_EVENT20 类），不得为空。
    #[test]
    fn test_known_fn_keys_non_empty_and_distinct() {
        assert!(!KNOWN_FN_KEYS.is_empty());
        let mut set = std::collections::HashSet::new();
        for k in KNOWN_FN_KEYS {
            assert!(!k.prefix.is_empty());
            assert!(set.insert((k.class, k.prefix)));
        }
    }

    /// Fn+K 目录条目必须与 `FN_K_WMI_CLASS`/`FN_K_PRESS_PREFIX` 常量一致
    ///（修订 1.50 收敛）：常量是默认绑定的事实来源，目录是 GUI 预设——两者
    /// 漂移会让"默认绑定 Fn+K"与"预设下拉 Fn+K"行为不一致。
    #[test]
    fn test_known_fn_k_matches_constants() {
        let first = KNOWN_FN_KEYS[0];
        assert_eq!(first.class, FN_K_WMI_CLASS);
        assert_eq!(first.prefix, FN_K_PRESS_PREFIX);
        assert_eq!(first.name, "Fn+K 性能模式");
    }

    /// 无事件类的退避节奏：前 3 次 5s、第 4~20 次 30s、之后 60s。
    #[test]
    fn test_no_event_classes_backoff_schedule() {
        assert_eq!(no_event_classes_backoff_secs(1), 5);
        assert_eq!(no_event_classes_backoff_secs(3), 5);
        assert_eq!(no_event_classes_backoff_secs(4), 30);
        assert_eq!(no_event_classes_backoff_secs(20), 30);
        assert_eq!(no_event_classes_backoff_secs(21), 60);
        assert_eq!(no_event_classes_backoff_secs(1000), 60);
    }
}
