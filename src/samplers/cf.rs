//! Shared CoreFoundation / IOKit FFI helpers used by the IOKit-backed samplers
//! (IOReport, SMC, battery). Keeping these in one place avoids duplicating the
//! CFString conversions and the AppleSMC connection dance across samplers.
//!
//! These touch raw CF pointers, so callers must respect ownership: anything
//! created here that the docs say is "+1" must be released by the caller.

use std::ffi::{CStr, CString};

use core_foundation_sys::base::{CFRelease, CFTypeRef, kCFAllocatorDefault};
use core_foundation_sys::dictionary::{CFDictionaryGetValueIfPresent, CFDictionaryRef};
use core_foundation_sys::number::{
    CFNumberGetValue, CFNumberRef, kCFNumberDoubleType, kCFNumberSInt64Type,
};
use core_foundation_sys::string::{
    CFStringCreateWithCString, CFStringGetCString, CFStringRef, kCFStringEncodingUTF8,
};
use io_kit_sys::{
    IOObjectRelease, IORegistryEntryCreateCFProperties, IOServiceGetMatchingService,
    IOServiceMatching, kIOMasterPortDefault,
};

/// Create a CFString from a Rust str. Caller owns it (CFRelease when done).
///
/// Uses `CFStringCreateWithCString` (a real copy) rather than the
/// `...NoCopy` trick some references use, which produces broken objects for
/// strings longer than a few bytes.
pub fn cfstr(s: &str) -> CFStringRef {
    let c = CString::new(s).unwrap();
    unsafe { CFStringCreateWithCString(kCFAllocatorDefault, c.as_ptr(), kCFStringEncodingUTF8) }
}

/// Read a (borrowed) CFStringRef into an owned Rust String. Returns an empty
/// string for null / unconvertible refs.
pub fn from_cfstr(s: CFStringRef) -> String {
    if s.is_null() {
        return String::new();
    }
    let mut buf = [0i8; 256];
    let ok = unsafe {
        CFStringGetCString(
            s,
            buf.as_mut_ptr(),
            buf.len() as isize,
            kCFStringEncodingUTF8,
        )
    };
    if ok == 0 {
        return String::new();
    }
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

// `CFBooleanRef` is a `CFTypeRef`; compare against the shared true value.
unsafe extern "C" {
    static kCFBooleanTrue: CFTypeRef;
}

/// An owned IOKit service property dictionary with typed, key-by-name getters.
/// Shared by the battery and AGX samplers so neither hand-rolls CFNumber reads.
pub struct CFProps {
    dict: CFDictionaryRef,
}

impl CFProps {
    /// Match the first IOKit service of `class_name` and copy its properties.
    /// Returns `None` if the service is absent (e.g. no GPU / no battery).
    pub fn for_service(class_name: &str) -> Option<Self> {
        let cls = CString::new(class_name).ok()?;
        // SAFETY: IOServiceMatching consumes the dict it returns; the matched
        // service must be IOObjectRelease'd after we copy its properties.
        let service = unsafe {
            let matching = IOServiceMatching(cls.as_ptr());
            if matching.is_null() {
                return None;
            }
            IOServiceGetMatchingService(kIOMasterPortDefault, matching)
        };
        if service == 0 {
            return None;
        }
        let mut dict: CFDictionaryRef = std::ptr::null();
        let rc = unsafe {
            IORegistryEntryCreateCFProperties(
                service,
                &mut dict as *mut CFDictionaryRef as *mut _,
                kCFAllocatorDefault,
                0,
            )
        };
        unsafe { IOObjectRelease(service) };
        if rc != 0 || dict.is_null() {
            return None;
        }
        Some(Self { dict })
    }

    /// Wrap an already-copied, owned (+1) property dictionary - for callers that
    /// ran `IORegistryEntryCreateCFProperties` themselves while iterating
    /// several services (e.g. all IOBlockStorageDrivers). Released on drop.
    pub fn from_owned_dict(dict: CFDictionaryRef) -> Self {
        Self { dict }
    }

    /// Borrowed value for a string key (valid while `self` lives).
    fn raw(&self, key: &str) -> CFTypeRef {
        let k = cfstr(key);
        let mut value: *const std::ffi::c_void = std::ptr::null();
        let found = unsafe {
            CFDictionaryGetValueIfPresent(self.dict, k as *const std::ffi::c_void, &mut value)
        };
        unsafe { CFRelease(k as CFTypeRef) };
        if found != 0 {
            value as CFTypeRef
        } else {
            std::ptr::null()
        }
    }

    /// The nested dictionary at `key`, if present (borrowed).
    pub fn dict(&self, key: &str) -> Option<CFProps> {
        let r = self.raw(key);
        if r.is_null() {
            return None;
        }
        // Borrowed sub-dict: retain so the returned CFProps owns a reference and
        // its Drop balances. (CFRetain via CFRelease pairing.)
        unsafe { core_foundation_sys::base::CFRetain(r) };
        Some(CFProps {
            dict: r as CFDictionaryRef,
        })
    }

    pub fn f64(&self, key: &str) -> Option<f64> {
        let r = self.raw(key);
        if r.is_null() {
            return None;
        }
        let mut out: f64 = 0.0;
        let ok = unsafe {
            CFNumberGetValue(
                r as CFNumberRef,
                kCFNumberDoubleType,
                &mut out as *mut f64 as *mut std::ffi::c_void,
            )
        };
        if ok { Some(out) } else { None }
    }

    /// Read as a signed 64-bit value (correct sign for fields like
    /// AppleSmartBattery `InstantAmperage`, printed unsigned by ioreg).
    pub fn i64(&self, key: &str) -> Option<i64> {
        let r = self.raw(key);
        if r.is_null() {
            return None;
        }
        let mut out: i64 = 0;
        let ok = unsafe {
            CFNumberGetValue(
                r as CFNumberRef,
                kCFNumberSInt64Type,
                &mut out as *mut i64 as *mut std::ffi::c_void,
            )
        };
        if ok { Some(out) } else { None }
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        let r = self.raw(key);
        if r.is_null() {
            return None;
        }
        Some(r == unsafe { kCFBooleanTrue })
    }
}

/// A property value for building test dictionaries.
#[cfg(test)]
pub enum Value {
    Int(i64),
    Bool(bool),
    Dict(CFProps),
}

#[cfg(test)]
impl CFProps {
    /// Build an owned property dictionary, as IOKit would return it.
    pub fn from_pairs(pairs: &[(&str, Value)]) -> Self {
        use core_foundation_sys::dictionary::{
            CFDictionaryCreate, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
        };
        use core_foundation_sys::number::CFNumberCreate;
        unsafe extern "C" {
            static kCFBooleanFalse: CFTypeRef;
        }
        let keys: Vec<CFTypeRef> = pairs.iter().map(|(k, _)| cfstr(k) as CFTypeRef).collect();
        let created: Vec<Option<CFTypeRef>> = pairs
            .iter()
            .map(|(_, v)| match v {
                Value::Int(n) => Some(unsafe {
                    CFNumberCreate(
                        kCFAllocatorDefault,
                        kCFNumberSInt64Type,
                        n as *const i64 as *const std::ffi::c_void,
                    )
                } as CFTypeRef),
                _ => None,
            })
            .collect();
        let values: Vec<CFTypeRef> = pairs
            .iter()
            .zip(&created)
            .map(|((_, v), c)| match v {
                Value::Int(_) => c.unwrap(),
                Value::Bool(true) => unsafe { kCFBooleanTrue },
                Value::Bool(false) => unsafe { kCFBooleanFalse },
                Value::Dict(d) => d.dict as CFTypeRef,
            })
            .collect();
        // SAFETY: the dictionary retains keys and values; release our +1 refs.
        let dict = unsafe {
            CFDictionaryCreate(
                kCFAllocatorDefault,
                keys.as_ptr() as *mut _,
                values.as_ptr() as *mut _,
                keys.len() as isize,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        };
        for r in keys.into_iter().chain(created.into_iter().flatten()) {
            unsafe { CFRelease(r) };
        }
        Self { dict }
    }
}

impl Drop for CFProps {
    fn drop(&mut self) {
        if !self.dict.is_null() {
            unsafe { CFRelease(self.dict as CFTypeRef) };
        }
    }
}
