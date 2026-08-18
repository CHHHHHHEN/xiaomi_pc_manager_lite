//! UTF-16 缓冲工具（Windows FFI 的宽字符串承载）。
//!
//! 历史实现 `to_pcwstr(s) -> (Vec<u16>, PCWSTR)` 把缓冲与指针拆开返回，
//! 调用方必须自行保证 `let (_buf, ptr) = ...` 的缓冲在 FFI 调用期间存活
//! ——缓冲一旦先于指针 drop，指针即为悬垂，在 `unsafe` FFI 中使用就是
//! use-after-free。`WideString` 将缓冲与指针绑定在同一所有权下，
//! `as_pcwstr()` 借用自身返回指针，编译器保证指针存活期内缓冲必然存在，
//! 从类型层面消除了该悬垂风险。

use windows::core::PCWSTR;

pub struct WideString(Vec<u16>);

impl WideString {
    pub fn new(s: &str) -> Self {
        Self(s.encode_utf16().chain(std::iter::once(0)).collect())
    }

    /// 从 `OsStr`（路径/命令行参数）直接构造 UTF-16 缓冲，**不经 `String`
    /// 中间层**。
    ///
    /// Windows 路径/argv 是 UTF-16，可能含未配对代理项（非合法 UTF-8）；
    /// `to_string_lossy` 会把它们替换成 U+FFFD，再编码回 UTF-16 就得到一条
    /// **不同的路径**——用于 `ShellExecuteW` 重启动（路径错 → 提权失败静默
    /// 继续）、DLL 加载（路径错 → 找不到文件）时是静默错误。直接用
    /// `encode_wide` 保留原始 UTF-16 单元（修订 1.46 安全加固）。
    #[cfg(windows)]
    pub fn from_os_str(s: &std::ffi::OsStr) -> Self {
        use std::os::windows::ffi::OsStrExt;
        Self(s.encode_wide().chain(std::iter::once(0)).collect())
    }

    /// UTF-16 缓冲（不含结尾 NUL）。用于构造 `BSTR`（`BSTR::from_wide` 会
    /// 自行加 NUL）等需要"内容不含终止符"的转换。
    #[cfg(windows)]
    pub fn units_no_nul(&self) -> &[u16] {
        // 缓冲恒以单个 0 结尾（new/from_os_str 都追加），安全截掉。
        debug_assert_eq!(self.0.last(), Some(&0));
        &self.0[..self.0.len() - 1]
    }

    /// 从**不含结尾 NUL 的 UTF-16 单元**构造（追加 NUL）。
    ///
    /// 用于提权命令行的宽域构建（`privilege::build_command_line` 直接在
    /// UTF-16 域拼接、再交回 `WideString` 持有），避免 `to_string_lossy`
    /// 往返（修订 1.46 审计，见 `from_os_str` 注释）。
    #[cfg(windows)]
    pub fn from_units(units: Vec<u16>) -> Self {
        let mut v = units;
        v.push(0);
        Self(v)
    }

    pub fn as_pcwstr(&self) -> PCWSTR {
        PCWSTR(self.0.as_ptr())
    }

    /// UTF-16 缓冲（含结尾 NUL）。用于需要**拷贝**到固定大小数组的场景
    /// （如 NOTIFYICONDATAW 的 szInfo/szInfoTitle），调用方负责截断到目标
    /// 数组容量。
    pub fn units(&self) -> &[u16] {
        &self.0
    }
}
