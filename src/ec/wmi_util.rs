use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::System::Variant::VARIANT;

pub unsafe fn bstr_from_variant(val: &VARIANT) -> Option<String> {
    let vt = val.Anonymous.Anonymous.vt.0;
    if vt != 8 {
        return None;
    }
    let bstr = &*val.Anonymous.Anonymous.Anonymous.bstrVal;
    let ptr = bstr.as_ptr();
    if ptr.is_null() {
        return None;
    }
    let slice = std::slice::from_raw_parts(ptr, bstr.len());
    Some(String::from_utf16_lossy(slice))
}

pub unsafe fn bool_from_variant(val: &VARIANT) -> Option<bool> {
    let vt = val.Anonymous.Anonymous.vt.0;
    if vt != 11 {
        return None;
    }
    Some(val.Anonymous.Anonymous.Anonymous.boolVal != VARIANT_BOOL(0))
}
