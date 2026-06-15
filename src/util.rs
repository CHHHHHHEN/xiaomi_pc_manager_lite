use windows::core::PCWSTR;

pub fn to_pcwstr(s: &str) -> (Vec<u16>, PCWSTR) {
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let ptr = PCWSTR(wide.as_ptr());
    (wide, ptr)
}
