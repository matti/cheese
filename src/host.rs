//! Host identity and `sysctl` helpers: the marketing chip name and machine
//! model label the live view (and make it obvious when running on a chip that
//! has never been seen); typed reads feed the system-load evidence.

use std::ffi::CString;

/// Read a string `sysctl` by name, e.g. `hw.model` -> `"Mac17,8"`.
pub fn sysctl_string(name: &str) -> Option<String> {
    let cname = CString::new(name).ok()?;
    let mut len: libc::size_t = 0;
    // SAFETY: passing a null buffer asks the kernel for the required length.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    // SAFETY: buf is `len` bytes, exactly what the sizing call asked for.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    // The value is NUL-terminated; drop the terminator and anything after it.
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).ok()
}

/// Marketing chip name, e.g. `"Apple M5 Pro"`.
pub fn chip() -> String {
    sysctl_string("machdep.cpu.brand_string").unwrap_or_else(|| "unknown SoC".to_string())
}

/// Machine model identifier, e.g. `"Mac17,8"`.
pub fn model() -> String {
    sysctl_string("hw.model").unwrap_or_else(|| "unknown model".to_string())
}

/// Read a fixed-size `sysctl` value by name (integers or a plain C struct such
/// as `xsw_usage`). `None` if the name is unknown or the size differs.
pub fn sysctl_value<T: Copy>(name: &str) -> Option<T> {
    let cname = CString::new(name).ok()?;
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let mut len: libc::size_t = std::mem::size_of::<T>();
    // SAFETY: the kernel writes at most `len` bytes into `value`; the result is
    // only used when it filled exactly size_of::<T>() bytes of a POD type.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            value.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    // SAFETY: fully initialized by the successful, exact-size read above.
    (rc == 0 && len == std::mem::size_of::<T>()).then(|| unsafe { value.assume_init() })
}
