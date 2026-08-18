use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::app::fnkey::{default_bindings, FnKeyBinding};
use crate::app::limits::{coherent_charge_limit, DEFAULT_CHARGE_LIMIT, FULL_CHARGE_LIMIT};
use crate::app::performance::PerfMode;

/// 后端偏好的类型定义见 `app::ec`（硬件访问端口模块）；此处保持历史导入
/// 路径 `app::config::BackendPreference` 可用，同时避免重复定义。
pub use crate::app::ec::BackendPreference;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub battery_care_enabled: bool,
    pub battery_charge_limit: u8,
    pub performance_mode: u8,
    pub auto_apply_on_startup: bool,
    pub auto_reapply_on_power_change: bool,
    pub auto_start_on_boot: bool,
    /// 电池供电时自动切换到指定性能模式（节能）。`false` 保持用户所选模式
    /// （狂暴在电池下仍按既有规则降级为极速）。
    pub auto_switch_to_quiet_on_battery: bool,
    /// 充电达到养护上限时弹托盘通知（默认关闭，可选项——部分用户不想被打扰）。
    pub notify_on_charge_limit: bool,
    pub backend: BackendPreference,
    /// Fn 功能键绑定表（默认 Fn+K → 循环切换性能模式）。
    #[serde(default = "default_bindings")]
    pub fn_key_bindings: Vec<FnKeyBinding>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            battery_care_enabled: false,
            battery_charge_limit: DEFAULT_CHARGE_LIMIT,
            performance_mode: PerfMode::Smart.ec_value(),
            auto_apply_on_startup: true,
            auto_reapply_on_power_change: true,
            auto_start_on_boot: false,
            // 电池自动切换节能：默认关闭（保持用户所选模式）。
            auto_switch_to_quiet_on_battery: false,
            // 充电到上限通知：默认关闭（可选功能，不主动打扰）。
            notify_on_charge_limit: false,
            // 默认使用 WMI 后端（本机 2025 RedmiBook Pro 14 实测可用；
            // Auto 模式同样 WMI 优先）。
            backend: BackendPreference::Wmi,
            fn_key_bindings: default_bindings(),
        }
    }
}

/// 配置文件的目录与读写。路径在**构造时解析一次**，load/save 不再读取
/// 全局状态——历史实现每次 I/O 都重读 `XIAOMI_PC_MANAGER_CONFIG_DIR`
/// 环境变量，测试不得不用全局互斥锁串行化对该变量的 `set_var`，生产代码
/// 也被测试需求污染。`from_dir` 让测试直接用独立临时目录，无需环境变量。
#[derive(Debug, Clone)]
pub struct ConfigStore {
    dir: PathBuf,
}

impl ConfigStore {
    /// 默认目录：`XIAOMI_PC_MANAGER_CONFIG_DIR`（可覆盖），否则系统配置目录
    /// 下的 `XiaomiPcManagerLite`。
    ///
    /// `dirs::config_dir()` 返回 `None`（Windows 上 Roaming AppData 查询失败）
    /// 时回退到当前目录——该回退是**不得已**的降级：配置会写到进程 CWD 下，
    /// 持久性与路径都不理想，因此必须记录告警而非静默落盘，否则用户会看到
    /// "设置不生效"却无法从日志排查。
    pub fn new() -> Self {
        let dir = std::env::var_os("XIAOMI_PC_MANAGER_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| match dirs::config_dir() {
                Some(dir) => dir.join("XiaomiPcManagerLite"),
                None => {
                    log::warn!(
                        "config_dir() unavailable; falling back to current directory for config"
                    );
                    PathBuf::from(".").join("XiaomiPcManagerLite")
                }
            });
        Self { dir }
    }

    /// 显式指定配置目录（仅测试用），不依赖任何全局状态。
    #[cfg(test)]
    pub fn from_dir(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("config.toml")
    }

    /// 清理 `save()` 崩溃后残留的临时文件（形如 `config.toml.<pid>.<seq>.tmp`）。
    ///
    /// 原子保存先写唯一临时文件再 rename；若进程在两步之间崩溃（断电/强杀），
    /// 临时文件会永久残留在配置目录。每次加载时清理由本进程在**启动阶段**执行：
    /// 此时尚无并发保存者，唯一命名的临时文件不可能属于一个正在进行中的写入，
    /// 删除是安全的。测试用目录同样可覆盖（崩溃残留不会跨启动累积）。
    fn cleanup_stale_tmp_files(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("config.toml.") && name.ends_with(".tmp") {
                log::debug!("Config: removing stale temp file {}", name);
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    pub fn load(&self) -> AppConfig {
        // 清理上次崩溃的残留临时文件（见 cleanup_stale_tmp_files）。
        self.cleanup_stale_tmp_files();
        let path = self.path();
        match std::fs::read_to_string(&path) {
            Ok(s) => match toml::from_str::<AppConfig>(&s) {
                Ok(mut cfg) => {
                    let before = cfg.clone();
                    cfg.sanitize();
                    // 消毒修改了配置（损坏/手改值被修正）时立即落盘：否则
                    // auto_apply_on_startup 关闭时不会重新保存，磁盘上的
                    // 损坏值会每次启动重复告警、永不修复（仅在内存中被纠正）。
                    if cfg != before {
                        log::debug!("Config sanitize corrected values; persisting");
                        if let Err(e) = self.save(&cfg) {
                            log::warn!("Persist sanitized config: {}", e);
                        }
                    }
                    log::info!("Config loaded from {}", self.path().display());
                    cfg
                }
                Err(e) => {
                    log::warn!(
                        "Config parse error at {:?}: {}; using degraded defaults",
                        path,
                        e
                    );
                    degraded_defaults()
                }
            },
            // AC-CFG-02：配置文件缺失（首次运行/被删除）时返回默认值，并
            // 立即落盘重建——避免用户删除 config.toml 后重启应用文件不会
            // 重新生成，直到下一次 GUI 修改设置才被创建。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::debug!(
                    "Config file missing; creating defaults at {}",
                    path.display()
                );
                let cfg = AppConfig::default();
                if let Err(e) = self.save(&cfg) {
                    log::warn!("Persist default config: {}", e);
                }
                cfg
            }
            Err(e) => {
                log::warn!(
                    "Config load error at {:?}: {}; using degraded defaults",
                    path,
                    e
                );
                degraded_defaults()
            }
        }
    }

    pub fn save(&self, cfg: &AppConfig) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let s = toml::to_string_pretty(cfg).map_err(|e| e.to_string())?;

        // Atomic write: write to a temporary file, then rename (NFR-REL-04)。
        // 临时文件名必须**唯一**（pid + 进程内自增序号）：固定名（如
        // config.toml.tmp）在并发保存时存在撕裂重命名风险——写入者 A 完成
        // write 后、rename 前，写入者 B 对同一 tmp 文件重新 truncate+write，
        // A 的 rename 会把 B 尚未写完的 tmp 改名成 config，最终配置文件内容
        // 被撕裂（下轮启动解析失败回退默认）。唯一名 + 原子 rename 保证目标
        // 文件永远是某一次完整写入的产物。清理：write/rename 失败时删除
        // 本次残留的临时文件。
        //
        // **落盘前 fsync**（修订 1.36）：`fs::write` 只关闭句柄，不保证数据
        // 块先于目录项 rename 落盘——断电/强杀时可能出现"config.toml 目录项
        // 已更新而数据块未写"的 0 长度/撕裂文件，下轮启动解析失败回退
        // degraded_defaults()，用户设置（充电上限/性能模式）静默失效。写后
        // `sync_all` 确保数据与元数据刷盘后再 rename。注：Windows NTFS 的
        // rename 原子性由文件系统日志保证，目录项本身的持久化不需要额外
        // 目录句柄 fsync（与 POSIX 语义不同），此处已覆盖本平台可实现性。
        let path = self.path();
        static SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SAVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp_path = path.with_extension(format!("toml.{}.{}.tmp", std::process::id(), seq));
        let write_result = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp_path)?;
            f.write_all(s.as_bytes())?;
            f.sync_all()?;
            Ok(())
        })();
        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e.to_string());
        }
        if let Err(e) = std::fs::rename(&tmp_path, &path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e.to_string());
        }

        log::debug!("Config saved to {}", path.display());
        Ok(())
    }
}

impl Default for ConfigStore {
    fn default() -> Self {
        Self::new()
    }
}

/// 配置损坏/不可读时的降级默认值。
///
/// 与 `AppConfig::default()` 的唯一区别：`auto_apply_on_startup = false`。
/// 历史实现在这两条路径直接返回默认配置（auto_apply 默认 true），损坏的
/// 配置文件会让启动同步把**默认值**（养护关、上限 100%、智能模式）写入硬件，
/// 静默覆盖用户原有的硬件设置。降级模式只读不改硬件；用户后续任一次保存
/// 都会重建一个有效配置文件。
fn degraded_defaults() -> AppConfig {
    AppConfig {
        auto_apply_on_startup: false,
        ..AppConfig::default()
    }
}

impl AppConfig {
    /// 规范化并读入的配置，防止手改/损坏的配置把垃圾值写入 EC 硬件：
    /// - `performance_mode` 必须是已知模式，否则回退默认（智能 0x09）。
    ///   历史版本曾把配置里的任意字节（如 0xFF）直接 `set_performance_mode`
    ///   写到 EC 寄存器 0x68，向硬件写入未定义代码。
    /// - `battery_charge_limit` 越界时夹紧到 [0, 100]；0 视为未设置回退 80
    ///   （把 0 写入充电上限寄存器几乎总是错误）。
    /// - 养护开启但上限 ≥100（矛盾组合）时，统一兜底为 80%——与 GUI 切换
    ///   路径（set_battery_care_internal）和启动应用路径（apply_startup_config）
    ///   的规则一致，避免"开着养护却 100%"的配置在启动时被静默改写。
    fn sanitize(&mut self) {
        if PerfMode::from_ec_value(self.performance_mode).is_none() {
            log::warn!(
                "Config: invalid performance_mode {:#x}; resetting to {}",
                self.performance_mode,
                PerfMode::Smart.name()
            );
            self.performance_mode = PerfMode::Smart.ec_value();
        }
        // `orig_limit` 记录**消毒前**的原始值（修订 1.46 审计）：随后 0 归一 /
        // >100 钳制会改写 `self.battery_charge_limit`，矛盾的 200% 组合若在
        // 钳制后才记录会误报 "100%"，与钳制告警的 "200%" 对不上、两条日志
        // 无法串联出完整事实。必须在任何改写之前捕获。
        let orig_limit = self.battery_charge_limit;
        if self.battery_charge_limit == 0 {
            log::warn!(
                "Config: charge limit 0 is invalid; using default {}%",
                DEFAULT_CHARGE_LIMIT
            );
            self.battery_charge_limit = DEFAULT_CHARGE_LIMIT;
        } else if self.battery_charge_limit > FULL_CHARGE_LIMIT {
            log::warn!(
                "Config: charge limit {}% out of range; clamping to 100%",
                self.battery_charge_limit
            );
            self.battery_charge_limit = FULL_CHARGE_LIMIT;
        }
        // 养护开启但上限 ≥100（矛盾组合）：与 coherent_charge_limit 的
        // 兜底共用同一份规则，诊断由一次 warn 覆盖。0 的兜底已在上面单独
        // 告警（"charge limit 0 is invalid"）并归一为 80——此处只负责 ≥100
        // 的矛盾组合。记录的是消毒前原始值（orig_limit），见上。
        let before_limit = self.battery_charge_limit;
        self.battery_charge_limit =
            coherent_charge_limit(self.battery_care_enabled, self.battery_charge_limit);
        if self.battery_care_enabled && before_limit >= 100 {
            log::warn!(
                "Config: battery care on with limit {}% (incoherent); using {}%",
                orig_limit,
                self.battery_charge_limit
            );
        }

        // Fn 绑定消毒：丢弃空类/空前缀的条目（手改配置可能留下残缺绑定；
        // 前缀为空时匹配一切事件，属于危险配置，宁可丢弃）。前缀必须是
        // 至少一个完整字节（归一化后偶数长度，修订 1.32/M3）：单字节前缀
        // 如 "0" 会匹配几乎全部该事件流（各报告大多以 0 开头），等同危险
        // 配置；类名必须是合法 WQL 标识符（防 WQL 注入，修订 1.32/M2）。
        // 绑定动作由 serde 枚举保证合法（未知枚举名直接解析失败走降级配置）。
        //
        // **类名规范化**（修订 1.47 审计）：`valid_class` 按 `trim()` 后的
        // 类名校验，但存储的是原始串——带首尾空白的类名通过校验却永不匹配
        // WMI 订阅类（监听线程按 trim 前拼 WQL，类不存在被跳过），形成
        // "校验通过但绑定永死"的静默配置。此处先 trim 再存，保证校验与
        // 实际使用的是同一个类名。
        let before = std::mem::take(&mut self.fn_key_bindings);
        self.fn_key_bindings = before
            .into_iter()
            .map(|mut b| {
                b.class = b.class.trim().to_string();
                b
            })
            .filter(|b| {
                let valid = crate::app::fnkey::valid_class(&b.class)
                    && crate::app::fnkey::valid_prefix(&b.prefix);
                if !valid {
                    log::warn!("Config: dropping invalid fn key binding {:?}", b);
                }
                valid
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_values() {
        let cfg = AppConfig::default();
        assert!(!cfg.battery_care_enabled);
        assert_eq!(cfg.battery_charge_limit, 80);
        assert_eq!(cfg.performance_mode, 0x09);
        assert!(cfg.auto_apply_on_startup);
        assert!(cfg.auto_reapply_on_power_change);
        assert!(!cfg.auto_start_on_boot);
        assert!(!cfg.auto_switch_to_quiet_on_battery);
        assert_eq!(cfg.backend, BackendPreference::Wmi);
    }

    #[test]
    fn test_backend_preference_default() {
        assert_eq!(BackendPreference::default(), BackendPreference::Auto);
    }

    #[test]
    fn test_serialization_roundtrip_all_fields() {
        let cfg = AppConfig {
            battery_care_enabled: true,
            battery_charge_limit: 60,
            performance_mode: 0x02,
            auto_apply_on_startup: false,
            auto_reapply_on_power_change: false,
            auto_start_on_boot: true,
            auto_switch_to_quiet_on_battery: true,
            notify_on_charge_limit: true,
            backend: BackendPreference::Wmi,
            fn_key_bindings: crate::app::fnkey::default_bindings(),
        };
        let s = toml::to_string_pretty(&cfg).expect("serialize");
        let deserialized: AppConfig = toml::from_str(&s).expect("deserialize");
        assert_eq!(cfg.battery_care_enabled, deserialized.battery_care_enabled);
        assert_eq!(cfg.battery_charge_limit, deserialized.battery_charge_limit);
        assert_eq!(cfg.performance_mode, deserialized.performance_mode);
        assert_eq!(
            cfg.auto_apply_on_startup,
            deserialized.auto_apply_on_startup
        );
        assert_eq!(
            cfg.auto_reapply_on_power_change,
            deserialized.auto_reapply_on_power_change
        );
        assert_eq!(cfg.auto_start_on_boot, deserialized.auto_start_on_boot);
        assert_eq!(cfg.backend, deserialized.backend);
        assert_eq!(
            cfg.fn_key_bindings, deserialized.fn_key_bindings,
            "fn key bindings must round-trip"
        );
    }

    #[test]
    fn test_serialization_backend_preference_auto() {
        let cfg = AppConfig {
            backend: BackendPreference::Auto,
            ..Default::default()
        };
        let s = toml::to_string_pretty(&cfg).expect("serialize");
        let deserialized: AppConfig = toml::from_str(&s).expect("deserialize");
        assert_eq!(deserialized.backend, BackendPreference::Auto);
    }

    #[test]
    fn test_serialization_backend_preference_winring0() {
        let cfg = AppConfig {
            backend: BackendPreference::WinRing0,
            ..Default::default()
        };
        let s = toml::to_string_pretty(&cfg).expect("serialize");
        let deserialized: AppConfig = toml::from_str(&s).expect("deserialize");
        assert_eq!(deserialized.backend, BackendPreference::WinRing0);
    }

    #[test]
    fn test_deserialize_invalid_toml_returns_error() {
        let result: Result<AppConfig, _> = toml::from_str("invalid toml content {{{}");
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_partial_content_uses_defaults() {
        let s = r#"battery_care_enabled = true"#;
        let cfg: AppConfig = toml::from_str(s).expect("deserialize partial");
        assert!(cfg.battery_care_enabled);
        assert_eq!(cfg.battery_charge_limit, 80);
        assert_eq!(cfg.performance_mode, 0x09);
        assert!(cfg.auto_apply_on_startup);
        assert!(cfg.auto_reapply_on_power_change);
        assert!(!cfg.auto_start_on_boot);
        assert_eq!(cfg.backend, BackendPreference::Wmi);
    }

    #[test]
    fn test_serialization_contains_all_fields() {
        let cfg = AppConfig::default();
        let s = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(s.contains("battery_care_enabled"));
        assert!(s.contains("battery_charge_limit"));
        assert!(s.contains("performance_mode"));
        assert!(s.contains("auto_apply_on_startup"));
        assert!(s.contains("auto_reapply_on_power_change"));
        assert!(s.contains("auto_start_on_boot"));
        assert!(s.contains("backend"));
    }

    #[test]
    fn test_debug_impl() {
        let cfg = AppConfig::default();
        let debug = format!("{:?}", cfg);
        assert!(debug.contains("battery_care_enabled"));
        assert!(debug.contains("performance_mode"));
    }

    #[test]
    fn test_clone_impl() {
        let cfg = AppConfig::default();
        let cloned = cfg.clone();
        assert_eq!(cfg.battery_care_enabled, cloned.battery_care_enabled);
        assert_eq!(cfg.battery_charge_limit, cloned.battery_charge_limit);
        assert_eq!(cfg.performance_mode, cloned.performance_mode);
        assert_eq!(cfg.auto_apply_on_startup, cloned.auto_apply_on_startup);
        assert_eq!(
            cfg.auto_reapply_on_power_change,
            cloned.auto_reapply_on_power_change
        );
        assert_eq!(cfg.auto_start_on_boot, cloned.auto_start_on_boot);
        assert_eq!(cfg.backend, cloned.backend);
    }

    /// 回归测试：手改/损坏的配置文件不得把未知性能模式（如 0xFF）写入 EC
    /// 硬件——启动应用/电源重设路径会原样 set_performance_mode。加载时必须
    /// 回退到已知模式（智能 0x09）。
    #[test]
    fn test_sanitize_resets_invalid_performance_mode() {
        let mut cfg = AppConfig {
            performance_mode: 0xFF,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.performance_mode, 0x09);

        let mut cfg = AppConfig {
            performance_mode: 0x00,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.performance_mode, 0x09);

        // 合法模式不受影响。
        let mut cfg = AppConfig {
            performance_mode: 0x02,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.performance_mode, 0x02);
    }

    /// 越界充电上限必须在加载时夹紧，否则会被原样写进 EC 寄存器。
    #[test]
    fn test_sanitize_clamps_charge_limit() {
        let mut cfg = AppConfig {
            battery_charge_limit: 200,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.battery_charge_limit, 100);

        // 0 视为未设置，回退默认 80%。
        let mut cfg = AppConfig {
            battery_charge_limit: 0,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.battery_charge_limit, 80);
    }

    /// 养护开启但上限 100%（矛盾组合）必须在加载时兜底为 80%，与 GUI/启动
    /// 应用路径的规则一致，防止矛盾配置在磁盘上反复"复活"。
    #[test]
    fn test_sanitize_normalizes_incoherent_care_and_limit() {
        let mut cfg = AppConfig {
            battery_care_enabled: true,
            battery_charge_limit: 100,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.battery_charge_limit, 80);
        assert!(cfg.battery_care_enabled);

        // 养护关闭 + 100% 上限是合法组合，不得改动。
        let mut cfg = AppConfig {
            battery_care_enabled: false,
            battery_charge_limit: 100,
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.battery_charge_limit, 100);
    }

    /// 回归测试（AC-CFG-02）：配置文件缺失时 `load()` 必须立即重建默认配置
    /// 并落盘——历史实现只在文件存在时返回默认值，删除 config.toml 后重启
    /// 应用文件不会重新生成，直到下一次修改设置才被创建。
    #[test]
    fn test_load_regenerates_missing_config_file() {
        let dir = std::env::temp_dir().join(format!("xmpl-cfg-test-{}", std::process::id()));
        let store = ConfigStore::from_dir(dir.clone());
        let path = store.path();
        let _ = std::fs::remove_dir_all(&dir);

        // 文件不存在：load 返回默认值并重新生成文件。
        let cfg = store.load();
        assert_eq!(cfg.battery_charge_limit, 80);
        assert!(path.exists(), "missing config file must be regenerated");

        // 重建的文件可被重新加载。
        let reloaded = store.load();
        assert_eq!(reloaded, cfg);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 并发保存的原子性（NFR-REL-04）：多线程同时 save() 时，目标
    /// config.toml 必须始终是某一次完整写入的产物——可被成功解析，
    /// 且最终不留任何临时文件残留。
    ///
    /// 回归测试（历史实现）：固定名 tmp（config.toml.tmp）在并发保存时，
    /// 写入者 A 完成 write 后、rename 前，写入者 B 对同一 tmp 重新
    /// truncate+write，A 的 rename 会把 B 尚未写完的 tmp 改名成 config，
    /// 导致配置文件内容撕裂、下轮启动解析失败回退默认值。修复后每个
    /// 保存使用唯一 tmp 名（pid+自增序号），该竞态从根上消失。
    #[test]
    fn test_concurrent_saves_are_atomic() {
        let dir = std::env::temp_dir().join(format!("xmpl-save-test-{}", std::process::id()));
        let store = ConfigStore::from_dir(dir.clone());
        let _ = std::fs::remove_dir_all(&dir);

        const THREADS: usize = 8;
        std::thread::scope(|s| {
            for i in 0..THREADS {
                let store = store.clone();
                s.spawn(move || {
                    let cfg = AppConfig {
                        battery_care_enabled: i % 2 == 0,
                        battery_charge_limit: (40 + i as u8 * 8) % 101,
                        ..Default::default()
                    };
                    // 多轮写：放大并发窗口。
                    for _ in 0..20 {
                        store.save(&cfg).expect("save must succeed");
                    }
                });
            }
        });

        // 目标文件必须可解析（未撕裂）。
        let path = store.path();
        let content = std::fs::read_to_string(&path).expect("config must exist");
        let parsed: AppConfig = toml::from_str(&content).expect("config must parse");
        assert!(
            parsed.battery_charge_limit <= 100,
            "parsed limit must be valid"
        );

        // 不得残留任何临时文件。
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("read config dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "config.toml")
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temp files may remain after concurrent saves: {:?}",
            leftovers
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 崩溃残留清理：模拟"原子保存写临时文件后、rename 前进程崩溃"的残留
    /// （`config.toml.<pid>.<seq>.tmp`），`load()` 必须将其清除——否则每次
    /// 崩溃都留下一个文件，永久累积在配置目录。
    #[test]
    fn test_load_cleans_stale_tmp_files() {
        let dir = std::env::temp_dir().join(format!("xmpl-tmp-clean-{}", std::process::id()));
        let store = ConfigStore::from_dir(dir.clone());
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create config dir");

        // 模拟两次崩溃残留 + 一个正常运行遗留的临时文件。
        std::fs::write(dir.join("config.toml.1234.0.tmp"), "partial").expect("write stale 1");
        std::fs::write(dir.join("config.toml.1234.1.tmp"), "partial").expect("write stale 2");

        // 不在清理范围内的其它文件不得被误删。
        std::fs::write(dir.join("config.toml"), "should survive").expect("write config");

        store.load();

        assert!(
            !dir.join("config.toml.1234.0.tmp").exists(),
            "stale temp file must be removed"
        );
        assert!(
            !dir.join("config.toml.1234.1.tmp").exists(),
            "stale temp file must be removed"
        );
        assert!(
            dir.join("config.toml").exists(),
            "config file itself must never be touched"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 消毒丢弃残缺的 Fn 绑定（空类/空前缀会被前缀匹配"匹配一切"，属于
    /// 危险配置），合法的绑定保持原样。
    #[test]
    fn test_sanitize_drops_invalid_fn_bindings() {
        let mut cfg = AppConfig {
            fn_key_bindings: vec![
                crate::app::fnkey::FnKeyBinding {
                    class: "".into(),
                    prefix: "012801".into(),
                    action: crate::app::fnkey::FnAction::CyclePerfMode,
                    command: None,
                },
                crate::app::fnkey::FnKeyBinding {
                    class: "HID_EVENT20".into(),
                    prefix: "   ".into(),
                    action: crate::app::fnkey::FnAction::None,
                    command: None,
                },
                crate::app::fnkey::FnKeyBinding {
                    class: "HID_EVENT20".into(),
                    prefix: "0107".into(),
                    action: crate::app::fnkey::FnAction::ReapplyConfig,
                    command: None,
                },
            ],
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.fn_key_bindings.len(), 1);
        assert_eq!(cfg.fn_key_bindings[0].class, "HID_EVENT20");
        assert_eq!(cfg.fn_key_bindings[0].prefix, "0107");
    }

    /// 修订 1.32/M3：消毒必须一并丢弃**单字节前缀**（归一化后长度 1，如
    /// "0" 匹配几乎全部该事件流）与非法类名（WQL 注入面），与 GUI 侧
    /// add_fn_binding 同一套校验规则。
    #[test]
    fn test_sanitize_drops_single_digit_prefix_and_bad_class() {
        let mut cfg = AppConfig {
            fn_key_bindings: vec![
                crate::app::fnkey::FnKeyBinding {
                    class: "HID_EVENT20".into(),
                    prefix: "0".into(),
                    action: crate::app::fnkey::FnAction::None,
                    command: None,
                },
                crate::app::fnkey::FnKeyBinding {
                    class: "HID_EVENT20 WHERE Foo=1".into(),
                    prefix: "012801".into(),
                    action: crate::app::fnkey::FnAction::None,
                    command: None,
                },
                crate::app::fnkey::FnKeyBinding {
                    class: "HID_EVENT20".into(),
                    prefix: "012".into(),
                    action: crate::app::fnkey::FnAction::None,
                    command: None,
                },
                crate::app::fnkey::FnKeyBinding {
                    class: "HID_EVENT20".into(),
                    prefix: "012801".into(),
                    action: crate::app::fnkey::FnAction::ReapplyConfig,
                    command: None,
                },
            ],
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.fn_key_bindings.len(), 1);
        assert_eq!(cfg.fn_key_bindings[0].prefix, "012801");
    }

    /// 旧版本配置文件（无 fn_key_bindings 字段）反序列化时，必须回退到
    /// 默认的 Fn+K 绑定（`#[serde(default = "default_bindings")]`）。
    #[test]
    fn test_deserialize_legacy_config_keeps_default_fn_bindings() {
        let s = r#"battery_care_enabled = true
battery_charge_limit = 80
performance_mode = 9
auto_apply_on_startup = true
auto_reapply_on_power_change = true
auto_start_on_boot = false
backend = "Wmi""#;
        let cfg: AppConfig = toml::from_str(s).expect("legacy config must parse");
        assert_eq!(cfg.fn_key_bindings, default_bindings());
    }

    /// 消毒对合法配置必须是幂等且无副作用的。
    #[test]
    fn test_sanitize_idempotent_on_valid_config() {
        let cfg = AppConfig {
            battery_care_enabled: true,
            battery_charge_limit: 80,
            performance_mode: 0x09,
            ..Default::default()
        };
        let mut once = cfg.clone();
        once.sanitize();
        let mut twice = once.clone();
        twice.sanitize();
        assert_eq!(once.battery_charge_limit, twice.battery_charge_limit);
        assert_eq!(once.performance_mode, twice.performance_mode);
        assert_eq!(cfg.battery_charge_limit, once.battery_charge_limit);
        assert_eq!(cfg.performance_mode, once.performance_mode);
    }

    /// 降级默认值只改动 auto_apply_on_startup，其余字段与默认值一致。
    #[test]
    fn test_degraded_defaults_disables_auto_apply() {
        let cfg = degraded_defaults();
        assert!(
            !cfg.auto_apply_on_startup,
            "degraded config must not auto-apply to hardware"
        );
        assert_eq!(cfg.battery_charge_limit, 80);
        assert!(!cfg.battery_care_enabled);
        assert_eq!(cfg.performance_mode, 0x09);
        assert_eq!(
            cfg.fn_key_bindings,
            default_bindings(),
            "degraded defaults keep default fn bindings"
        );
    }

    /// 回归测试：配置文件损坏（TOML 解析失败）时，返回的配置必须禁用
    /// 启动自动应用——历史实现返回默认配置（auto_apply=true），启动同步
    /// 会把默认值（养护关、上限 100%）写入硬件，静默覆盖用户设置。
    #[test]
    fn test_load_parse_error_returns_degraded_config() {
        let dir = std::env::temp_dir().join(format!("xmpl-degraded-{}", std::process::id()));
        let store = ConfigStore::from_dir(dir.clone());
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create config dir");
        std::fs::write(store.path(), "this is not valid toml {{{").expect("write broken config");

        let cfg = store.load();
        assert!(
            !cfg.auto_apply_on_startup,
            "broken config must not be auto-applied to hardware"
        );
        // 损坏的文件原样保留，不落盘覆盖（避免丢失用户数据）。
        let content = std::fs::read_to_string(store.path()).expect("read config");
        assert_eq!(content, "this is not valid toml {{{");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
