use windows::core::PCWSTR;

/// 将 Rust 字符串转为 UTF-16 缓冲 + PCWSTR 指针。
/// 返回的 Vec 必须与指针一同存活（调用方持有）。
pub fn to_pcwstr(s: &str) -> (Vec<u16>, PCWSTR) {
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let ptr = PCWSTR(wide.as_ptr());
    (wide, ptr)
}
