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

/// 把 `src` 写入固定大小 UTF-16 目标缓冲（`dst`）并保证 **NUL 结尾**。
///
/// NUL 是 Windows 字符串 API（`Shell_NotifyIconW` 的 szTip/szInfo 等）读取
/// 缓冲的**唯一终止信号**：不保证 NUL 结尾会让 API 越过缓冲读入相邻字段，
/// 显示垃圾字符。语义：写入 `src` 的 UTF-16 编码，最多 `max_units` 单元
/// （再受 `dst.len()-1` 物理上限约束），末尾恒补一个 0；NUL 之后的单元
/// 保持调用方原值（字符串读取在 NUL 处停止，无需整体清零）。
///
/// 托盘 notify.rs（szInfo/szInfoTitle）与 worker.rs（szTip）此前各自手写
/// "encode_utf16 + take + 追加 NUL"的样板，截断/清零策略各不相同——统一
/// 收敛到此处（修订 1.50 整理）。`max_units` 与数组容量解耦：如
/// `NOTIFYICONDATAW.szInfo` 实际为 256 单元，但 NIF_INFO 正文约定只用到
/// 63 单元，调用方显式传 `max_units`，缓冲物理容量由 `dst.len()` 兜底。
pub fn write_utf16_capped(dst: &mut [u16], max_units: usize, src: &str) {
    let max = max_units.min(dst.len().saturating_sub(1));
    let units: Vec<u16> = src.encode_utf16().take(max).collect();
    dst[..units.len()].copy_from_slice(&units);
    // 截断后的下一个单元置 NUL：NUL 之前是内容、之后由调用方决定。
    dst[units.len()] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UTF-16 截断 + NUL 结尾（修订 1.50 收敛）：短文本原样写入并在内容后
    /// NUL 结尾、NUL 之后保持调用方原值；超长文本截断到上限单元数且 NUL
    /// 结尾；max_units 超过物理容量时受数组容量兜底。
    #[test]
    fn test_write_utf16_capped() {
        // 短文本：内容 + NUL，NUL 之后保持原值。
        let mut dst = [0xAAAAu16; 8];
        write_utf16_capped(&mut dst, 8, "ab");
        let mut expect = vec![b'a' as u16, b'b' as u16, 0];
        expect.extend([0xAAAAu16; 5]);
        assert_eq!(dst, expect.as_slice());
        // 超长文本：截断到 max_units（7 单元），NUL 位于第 7 个槽。
        write_utf16_capped(&mut dst, 7, "abcdefgh");
        assert_eq!(
            &dst[..7],
            vec![
                b'a' as u16,
                b'b' as u16,
                b'c' as u16,
                b'd' as u16,
                b'e' as u16,
                b'f' as u16,
                b'g' as u16
            ]
            .as_slice()
        );
        assert_eq!(dst[7], 0, "NUL must terminate the truncated text");
        // max_units 大于物理容量：内容最多 dst.len()-1（127 语义的防御）。
        let mut small = [0u16; 4];
        write_utf16_capped(&mut small, 100, "toolong");
        assert_eq!(&small[..3], &[b't' as u16, b'o' as u16, b'o' as u16]);
        assert_eq!(small[3], 0);
        // 空串：仅 NUL。
        let mut empty = [0xBBu16; 3];
        write_utf16_capped(&mut empty, 2, "");
        assert_eq!(empty[0], 0);
        assert_eq!(empty[1], 0xBB, "NUL 之后保持原值");
    }
}
