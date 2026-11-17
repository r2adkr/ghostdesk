//! A thin, safe wrapper over the Accessibility (AX) API.
//!
//! - Every call is a blocking mach message to the target app. From async code, call it inside
//!   `tokio::task::spawn_blocking`.
//! - AX return codes lie a lot (Success without effect, failure with effect), so always read
//!   important changes back to verify them.
//! - Setting AXValue / AXSelectedTextRange / AXFocused and AXPress on controls work even while
//!   the app is hidden (verified on this Mac). AXRaise, AXMinimized=false and unhiding the app
//!   all put windows on the user's screen, so this module never uses them.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_application_services::{
    AXCopyMultipleAttributeOptions, AXError, AXIsProcessTrusted, AXUIElement, AXValue, AXValueType,
};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFNumber, CFRange, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize,
};

/// An AX element handle that can move between threads.
///
/// `AXUIElementRef` is an immutable token and the whole API is thread-safe, so it is treated
/// as `Send + Sync`.
#[derive(Clone)]
pub struct El(pub CFRetained<AXUIElement>);
unsafe impl Send for El {}
unsafe impl Sync for El {}

impl std::fmt::Debug for El {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "El({})", self.role())
    }
}

impl PartialEq for El {
    fn eq(&self, other: &Self) -> bool {
        // CFEqual is true when both refer to the same UI element
        let a: &CFType = &self.0;
        let b: &CFType = &other.0;
        a == b
    }
}

pub fn is_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

fn cfs(s: &str) -> CFRetained<CFString> {
    CFString::from_str(s)
}

/// Converts a `CFType` to a readable string (strings, numbers and booleans only).
pub fn cf_to_string(v: &CFType) -> Option<String> {
    if let Some(s) = v.downcast_ref::<CFString>() {
        return Some(s.to_string());
    }
    if let Some(n) = v.downcast_ref::<CFNumber>() {
        if let Some(i) = n.as_i64() {
            if n.as_f64().map(|f| f.fract() == 0.0).unwrap_or(true) {
                return Some(i.to_string());
            }
        }
        return n.as_f64().map(|f| format!("{f:.4}").trim_end_matches('0').trim_end_matches('.').to_string());
    }
    if let Some(b) = v.downcast_ref::<CFBoolean>() {
        return Some(if b.as_bool() { "1".into() } else { "0".into() });
    }
    None
}

fn ax_value_of(v: &CFType) -> Option<&AXValue> {
    let av = v.downcast_ref::<AXValue>()?;
    if unsafe { av.r#type() } == AXValueType::AXError {
        return None;
    }
    Some(av)
}

pub fn cf_point(v: &CFType) -> Option<CGPoint> {
    let av = ax_value_of(v)?;
    let mut p = CGPoint { x: 0.0, y: 0.0 };
    unsafe { av.value(AXValueType::CGPoint, NonNull::from(&mut p).cast()) }.then_some(p)
}

pub fn cf_size(v: &CFType) -> Option<CGSize> {
    let av = ax_value_of(v)?;
    let mut s = CGSize { width: 0.0, height: 0.0 };
    unsafe { av.value(AXValueType::CGSize, NonNull::from(&mut s).cast()) }.then_some(s)
}

pub fn cf_range(v: &CFType) -> Option<CFRange> {
    let av = ax_value_of(v)?;
    let mut r = CFRange { location: 0, length: 0 };
    unsafe { av.value(AXValueType::CFRange, NonNull::from(&mut r).cast()) }.then_some(r)
}

pub fn cf_elements(v: &CFType) -> Vec<El> {
    let Some(arr) = v.downcast_ref::<CFArray>() else { return vec![] };
    let arr: &CFArray<CFType> = unsafe { arr.cast_unchecked() };
    arr.iter()
        .filter_map(|o| o.downcast::<AXUIElement>().ok().map(El))
        .collect()
}

pub fn range_value(r: CFRange) -> Option<CFRetained<AXValue>> {
    unsafe { AXValue::new(AXValueType::CFRange, NonNull::from(&r).cast()) }
}

pub fn point_value(p: CGPoint) -> Option<CFRetained<AXValue>> {
    unsafe { AXValue::new(AXValueType::CGPoint, NonNull::from(&p).cast()) }
}

extern "C" {
    fn _AXUIElementGetWindow(el: *const AXUIElement, wid: *mut u32) -> i32;
}

impl El {
    /// The app (process) root element. Uses a short messaging timeout so a hung app can't block us
    /// for long.
    pub fn app(pid: i32) -> El {
        let a = unsafe { AXUIElement::new_application(pid) };
        unsafe { a.set_messaging_timeout(2.5) };
        El(a)
    }

    pub fn get_raw(&self, attr: &str) -> Result<CFRetained<CFType>, AXError> {
        let mut v: *const CFType = std::ptr::null();
        let e = unsafe { self.0.copy_attribute_value(&cfs(attr), NonNull::from(&mut v)) };
        if e == AXError::Success && !v.is_null() {
            Ok(unsafe { CFRetained::from_raw(NonNull::new_unchecked(v as *mut CFType)) })
        } else {
            if !v.is_null() {
                unsafe { drop(CFRetained::from_raw(NonNull::new_unchecked(v as *mut CFType))) };
            }
            Err(if e == AXError::Success { AXError::NoValue } else { e })
        }
    }

    pub fn get(&self, attr: &str) -> Option<CFRetained<CFType>> {
        self.get_raw(attr).ok()
    }

    pub fn string(&self, attr: &str) -> Option<String> {
        self.get(attr).and_then(|v| cf_to_string(&v)).filter(|s| !s.is_empty())
    }

    pub fn role(&self) -> String {
        self.string("AXRole").unwrap_or_default()
    }

    pub fn bool(&self, attr: &str) -> Option<bool> {
        let v = self.get(attr)?;
        if let Some(b) = v.downcast_ref::<CFBoolean>() {
            return Some(b.as_bool());
        }
        v.downcast_ref::<CFNumber>().and_then(|n| n.as_i64()).map(|i| i != 0)
    }

    pub fn number(&self, attr: &str) -> Option<f64> {
        self.get(attr)?.downcast_ref::<CFNumber>()?.as_f64()
    }

    pub fn frame(&self) -> Option<CGRect> {
        let pv = self.get("AXPosition")?;
        let p = cf_point(&pv)?;
        let sv = self.get("AXSize")?;
        let s = cf_size(&sv)?;
        Some(CGRect { origin: p, size: s })
    }

    pub fn range(&self, attr: &str) -> Option<CFRange> {
        let v = self.get(attr)?;
        cf_range(&v)
    }

    pub fn elements(&self, attr: &str) -> Vec<El> {
        self.get(attr).map(|v| cf_elements(&v)).unwrap_or_default()
    }

    pub fn element(&self, attr: &str) -> Option<El> {
        self.get(attr)?.downcast::<AXUIElement>().ok().map(El)
    }

    pub fn children(&self) -> Vec<El> {
        self.elements("AXChildren")
    }

    pub fn parent(&self) -> Option<El> {
        self.element("AXParent")
    }

    /// Reads several attributes in one IPC round trip. Failed attributes are `None`.
    pub fn multi(&self, attrs: &[&str]) -> Vec<Option<CFRetained<CFType>>> {
        let names: Vec<CFRetained<CFString>> = attrs.iter().map(|a| cfs(a)).collect();
        let arr = CFArray::from_retained_objects(&names);
        let arr_any: &CFArray = arr.as_opaque();
        let mut out: *const CFArray = std::ptr::null();
        let e = unsafe {
            self.0
                .copy_multiple_attribute_values(arr_any, AXCopyMultipleAttributeOptions(0), NonNull::from(&mut out))
        };
        if e != AXError::Success || out.is_null() {
            return attrs.iter().map(|a| self.get(a)).collect();
        }
        let res: CFRetained<CFArray> = unsafe { CFRetained::from_raw(NonNull::new_unchecked(out as *mut CFArray)) };
        let res: &CFArray<CFType> = unsafe { res.cast_unchecked() };
        (0..attrs.len())
            .map(|i| {
                let v = res.get(i)?;
                if let Some(av) = v.downcast_ref::<AXValue>() {
                    if unsafe { av.r#type() } == AXValueType::AXError {
                        return None;
                    }
                }
                Some(v)
            })
            .collect()
    }

    pub fn set(&self, attr: &str, v: &CFType) -> AXError {
        unsafe { self.0.set_attribute_value(&cfs(attr), v) }
    }

    pub fn set_bool(&self, attr: &str, b: bool) -> AXError {
        self.set(attr, CFBoolean::new(b))
    }

    pub fn set_string(&self, attr: &str, s: &str) -> AXError {
        self.set(attr, &cfs(s))
    }

    pub fn set_number(&self, attr: &str, n: f64) -> AXError {
        self.set(attr, &CFNumber::new_f64(n))
    }

    pub fn set_range(&self, attr: &str, r: CFRange) -> AXError {
        match range_value(r) {
            Some(v) => self.set(attr, &v),
            None => AXError::Failure,
        }
    }

    pub fn settable(&self, attr: &str) -> bool {
        let mut b: u8 = 0;
        let e = unsafe { self.0.is_attribute_settable(&cfs(attr), NonNull::from(&mut b)) };
        e == AXError::Success && b != 0
    }

    pub fn perform(&self, action: &str) -> AXError {
        unsafe { self.0.perform_action(&cfs(action)) }
    }

    pub fn actions(&self) -> Vec<String> {
        let mut v: *const CFArray = std::ptr::null();
        let e = unsafe { self.0.copy_action_names(NonNull::from(&mut v)) };
        if e != AXError::Success || v.is_null() {
            return vec![];
        }
        let arr: CFRetained<CFArray> = unsafe { CFRetained::from_raw(NonNull::new_unchecked(v as *mut CFArray)) };
        let arr: &CFArray<CFType> = unsafe { arr.cast_unchecked() };
        arr.iter().filter_map(|s| cf_to_string(&s)).collect()
    }

    pub fn attribute_names(&self) -> Vec<String> {
        let mut v: *const CFArray = std::ptr::null();
        let e = unsafe { self.0.copy_attribute_names(NonNull::from(&mut v)) };
        if e != AXError::Success || v.is_null() {
            return vec![];
        }
        let arr: CFRetained<CFArray> = unsafe { CFRetained::from_raw(NonNull::new_unchecked(v as *mut CFArray)) };
        let arr: &CFArray<CFType> = unsafe { arr.cast_unchecked() };
        arr.iter().filter_map(|s| cf_to_string(&s)).collect()
    }

    /// A parameterized attribute (e.g. AXRangeForPosition, AXStringForRange).
    pub fn param(&self, attr: &str, p: &CFType) -> Option<CFRetained<CFType>> {
        let mut v: *const CFType = std::ptr::null();
        let e = unsafe { self.0.copy_parameterized_attribute_value(&cfs(attr), p, NonNull::from(&mut v)) };
        if e == AXError::Success && !v.is_null() {
            Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(v as *mut CFType)) })
        } else {
            None
        }
    }

    /// Character range at a global point, in points (text elements only).
    pub fn range_for_position(&self, p: CGPoint) -> Option<CFRange> {
        let pv = point_value(p)?;
        let r = self.param("AXRangeForPosition", &pv)?;
        cf_range(&r)
    }

    /// AX window element → CGWindowID (private `_AXUIElementGetWindow`).
    pub fn window_id(&self) -> Option<u32> {
        let mut wid: u32 = 0;
        let r = unsafe { _AXUIElementGetWindow(CFRetained::as_ptr(&self.0).as_ptr(), &mut wid) };
        (r == 0 && wid != 0).then_some(wid)
    }

    pub fn pid(&self) -> Option<i32> {
        let mut p: libc::pid_t = 0;
        (unsafe { self.0.pid(NonNull::from(&mut p)) } == AXError::Success).then_some(p)
    }

    /// The element the app reports at a global point (the app's own hit test).
    pub fn element_at(&self, x: f32, y: f32) -> Option<El> {
        let mut out: *const AXUIElement = std::ptr::null();
        let e = unsafe { self.0.copy_element_at_position(x, y, NonNull::from(&mut out)) };
        if e == AXError::Success && !out.is_null() {
            Some(El(unsafe { CFRetained::from_raw(NonNull::new_unchecked(out as *mut AXUIElement)) }))
        } else {
            None
        }
    }

    pub fn raw_ptr(&self) -> *const c_void {
        CFRetained::as_ptr(&self.0).as_ptr() as *const c_void
    }
}

pub fn err_name(e: AXError) -> &'static str {
    match e.0 {
        0 => "Success",
        -25200 => "Failure",
        -25201 => "IllegalArgument",
        -25202 => "InvalidUIElement",
        -25203 => "InvalidUIElementObserver",
        -25204 => "CannotComplete",
        -25205 => "AttributeUnsupported",
        -25206 => "ActionUnsupported",
        -25207 => "NotificationUnsupported",
        -25208 => "NotImplemented",
        -25209 => "NotificationAlreadyRegistered",
        -25210 => "NotificationNotRegistered",
        -25211 => "APIDisabled",
        -25212 => "NoValue",
        -25213 => "ParameterizedAttributeUnsupported",
        -25214 => "NotEnoughPrecision",
        _ => "Unknown",
    }
}
