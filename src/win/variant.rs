//! WMI/VARIANT 属性访问工具：VARIANT 的 RAII 承载与各类型属性读取。
//!
//! 从 crate 根 `wmi_util.rs` 拆分而来（与 `win::com` 同一次分层收敛）：
//! COM/WMI 连接与查询样板在 `win::com`，本模块只负责"从 WMI 对象读取属性值"
//! 的 VARIANT/类型转换工具，供 `ec` 与 `platform` 复用。

use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_BSTR, VT_I4};
use windows::Win32::System::Wmi::IWbemClassObject;

/// VARIANT 的 RAII 包装：Drop 时自动释放（委托给 VARIANT 自身的 Drop）。
///
/// windows-rs 0.62 的 `VARIANT` **实现了 Drop**（自动 `VariantClear`，
/// 见 windows-rs 源码 `extensions/Win32/System/Variant.rs`）。历史实现误以为
/// 它"不实现 Drop"，于是包一层 `OwnedVariant` 再手动 `VariantClear`——同一
/// VARIANT 被清两次：先由本包装清、再把（此时已为 VT_EMPTY 的）VARIANT
/// 交给其自身 Drop 清一次。第二次清空是 no-op，无害，但重复清理掩盖了真实
/// 语义、且错误注释会诱导后续维护者"补一个 ManuallyDrop"或误删清理。
/// 修复：本包装**不再手动清零**，只依赖 VARIANT 自带的 Drop（唯一清理路径），
/// 注释与实现一致。持有者只管借用读取。
pub struct OwnedVariant(VARIANT);

impl OwnedVariant {
    pub fn new(v: VARIANT) -> Self {
        Self(v)
    }
}

impl std::ops::Deref for OwnedVariant {
    type Target = VARIANT;
    fn deref(&self) -> &VARIANT {
        &self.0
    }
}

// 无需显式 impl Drop：VARIANT 自身 Drop 会 VariantClear，此处保持默认。

/// 从 WMI 对象按名读取属性值（`IWbemClassObject::Get`）。
///
/// fnkey.rs 与 wmi.rs 各自重复实现过同样的样板（to_pcwstr → Get → 取
/// VARIANT），统一收敛到此处。返回的 VARIANT 由 `OwnedVariant` 在 Drop 时
/// 自动释放，调用方无需手动 `VariantClear`。
///
/// 属性缺失（`WBEM_E_NOT_FOUND`）时返回 None——这是各类事件对象属性可选
/// 的**正常**情形（如 `EventDetail` 与 `ReportHex` 二选一）。其它 `Get`
/// 失败（`WBEM_E_ACCESS_DENIED`/provider 错误等）说明对象本身或提供程序
/// 异常，历史上被 `.ok()?` 静默吞掉、与"属性不存在"无法区分，真实故障
/// 永远不可见——此处记录 warn 日志（仅此，不改变返回语义，调用方仍按
/// None 处理；异常值本身不携带可恢复信息，告警足以排查）。
pub fn get_property(obj: &IWbemClassObject, name: &str) -> Option<OwnedVariant> {
    let wname = crate::util::WideString::new(name);
    let mut val = VARIANT::default();
    let mut _type = 0i32;
    let mut _flavor = 0i32;
    let hr = unsafe {
        obj.Get(
            wname.as_pcwstr(),
            0,
            &mut val,
            Some(&mut _type as *mut i32),
            Some(&mut _flavor as *mut i32),
        )
    };
    if let Err(e) = hr {
        // WBEM_E_NOT_FOUND (0x80041002)：属性缺失，正常路径（部分事件对象
        // 只带 ReportHex 不带 EventDetail 等），静默返回 None。
        if (e.code().0 as u32) != (windows::Win32::System::Wmi::WBEM_E_NOT_FOUND.0 as u32) {
            log::warn!("WMI Get({}) failed: {}", name, e);
        }
        return None;
    }
    Some(OwnedVariant::new(val))
}

/// 从 VARIANT 读取 BSTR 字符串内容（属性为 VT_BSTR 时）。
///
/// # Safety
///
/// `val` 必须指向一个已初始化的 `VARIANT`（由 WMI 的 `Get` 或 `VARIANT::default`
/// 产生）；读取期间不得被其它线程并发修改。
pub unsafe fn bstr_from_variant(val: &VARIANT) -> Option<String> {
    let vt = val.Anonymous.Anonymous.vt.0;
    if vt != VT_BSTR.0 {
        return None;
    }
    // Take the address of the union member instead of forming a reference to
    // its possibly-null value; BSTR's Deref handles the null case safely.
    let bstr = &*std::ptr::addr_of!(val.Anonymous.Anonymous.Anonymous.bstrVal);
    if bstr.is_empty() {
        return None;
    }
    Some(String::from_utf16_lossy(bstr))
}

/// 从 WMI 对象读取字符串属性（属性缺失/类型不符时返回 None）。
///
/// fnkey.rs（ReportHex）与 wmi.rs（InstanceName 等）各自重复实现过同样的
/// `get_property + bstr_from_variant` 两步样板，统一收敛到此处。
pub fn get_string_prop(obj: &IWbemClassObject, name: &str) -> Option<String> {
    let val = get_property(obj, name)?;
    unsafe { bstr_from_variant(&val) }
}

/// 从 WMI 对象读取布尔属性（属性缺失/类型不符时返回 None）。
pub fn get_bool_prop(obj: &IWbemClassObject, name: &str) -> Option<bool> {
    let val = get_property(obj, name)?;
    unsafe { bool_from_variant(&val) }
}

/// 从 WMI 对象读取 u32 属性（属性缺失/类型不符时返回 None）。
/// 见 `uint_from_variant` 的类型说明（真机 root\WMI 电池容量返回 VT_I4）。
pub fn uint_prop(obj: &IWbemClassObject, name: &str) -> Option<u32> {
    let val = get_property(obj, name)?;
    unsafe { uint_from_variant(&val) }
}

/// 从 VARIANT 读取布尔值（属性为 VT_BOOL 时）。
///
/// # Safety
/// 与 [`bstr_from_variant`] 相同：`val` 必须指向一个已初始化的 `VARIANT`，
/// 读取期间不得被并发修改。
pub unsafe fn bool_from_variant(val: &VARIANT) -> Option<bool> {
    let vt = val.Anonymous.Anonymous.vt.0;
    if vt != VT_BOOL.0 {
        return None;
    }
    Some(val.Anonymous.Anonymous.Anonymous.boolVal != VARIANT_BOOL(0))
}

/// 读取整数属性（电池容量等 uint32 字段）为 u32。
///
/// WMI 提供程序对 uint32 属性的变体类型并不统一：
/// - `root\WMI` 的 ACPI 电池类（`BatteryStaticData.DesignedCapacity` /
///   `BatteryFullChargedCapacity.FullChargedCapacity`）在
///   **2025 RedmiBook Pro 14 实测返回 `VT_I4`（3）**——以有符号整型承载
///   非负数值（本机 DesignedCapacity=76990、FullChargedCapacity=77255）；
/// - 其它提供程序可能返回 `VT_UI4`（19）或 `VT_UINT`（22）。
///
/// 三种类型统一在此收敛：`VT_I4` 取 `lVal`（负数按"不可读"处理，容量类
/// 字段不可能为负）；`VT_UI4`/`VT_UINT` 取 `ulVal`。其它类型（字符串/
/// 布尔/浮点）返回 None，由调用方按"属性不可读"处理。
///
/// # Safety
/// 与 [`bstr_from_variant`] 相同：`val` 必须指向已初始化的 `VARIANT`，读取
/// 期间不得被并发修改。
pub unsafe fn uint_from_variant(val: &VARIANT) -> Option<u32> {
    let vt = val.Anonymous.Anonymous.vt.0;
    if vt == VT_I4.0 {
        let l = val.Anonymous.Anonymous.Anonymous.lVal;
        return (l >= 0).then_some(l as u32);
    }
    if vt != windows::Win32::System::Variant::VT_UI4.0
        && vt != windows::Win32::System::Variant::VT_UINT.0
    {
        return None;
    }
    Some(val.Anonymous.Anonymous.Anonymous.ulVal)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 属性变体类型三态（真机实证：root\WMI 电池容量返回 VT_I4）：
    /// VT_I4（正数）→ u32；VT_UI4 / VT_UINT → u32；VT_I4 负数 → None；
    /// 其它类型 → None。VARIANT 由 windows crate 的 From 构造。
    #[test]
    fn test_uint_from_variant() {
        unsafe {
            // VT_I4 正数（本机 BatteryStaticData.DesignedCapacity 实测类型）。
            assert_eq!(uint_from_variant(&VARIANT::from(76990i32)), Some(76990));
            // VT_UI4 / VT_UINT（u32 构造）。
            assert_eq!(uint_from_variant(&VARIANT::from(77255u32)), Some(77255));
            assert_eq!(uint_from_variant(&VARIANT::from(123u32)), Some(123));
            // VT_I4 负数：容量不可能为负，按"不可读"处理。
            assert_eq!(uint_from_variant(&VARIANT::from(-1i32)), None);
            // VT_EMPTY / 未初始化：类型不符。
            assert_eq!(uint_from_variant(&VARIANT::default()), None);
        }
    }
}
