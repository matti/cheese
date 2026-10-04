//! Cross-uid process-name resolution, shared by the process and GPU samplers.
//!
//! `proc_pidpath` works cross-uid for *naming* (even when CPU/energy are
//! denied), so any sampler that has a PID can get a human-readable name. The
//! version-stripping mirrors what `ps` shows: a binary living at
//! `.../tool/versions/2.1.158` resolves to "tool", not "2.1.158".

const PROC_PIDPATHINFO_MAXSIZE: usize = 4 * 1024;

unsafe extern "C" {
    fn proc_pidpath(pid: libc::c_int, buffer: *mut libc::c_void, buffersize: u32) -> libc::c_int;
}

/// Executable path for `pid` via `proc_pidpath` (cross-uid). `None` on failure.
pub fn pidpath(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
    let rc = unsafe { proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if rc > 0 {
        buf.truncate(rc as usize);
        if let Some(end) = buf.iter().position(|b| *b == 0) {
            buf.truncate(end);
        }
        Some(String::from_utf8_lossy(&buf).into_owned())
    } else {
        None
    }
}

/// Best-effort name for `pid`: the executable basename, de-versioned so a
/// versioned binary path yields a meaningful component. Falls back to
/// `pid <n>` when the path is unavailable.
pub fn resolve_name(pid: i32) -> String {
    pidpath(pid)
        .as_deref()
        .and_then(meaningful_basename)
        .unwrap_or_else(|| format!("pid {pid}"))
}

/// Basename of a path, walking up past bare version-number components and a
/// `versions` directory holding them (`.../claude/versions/2.1.3` -> "claude").
pub fn meaningful_basename(path: &str) -> Option<String> {
    let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let mut skipped_version = false;
    for c in comps.iter().rev() {
        if looks_like_version(c) {
            skipped_version = true;
        } else if !(skipped_version && *c == "versions") {
            return Some((*c).to_string());
        }
    }
    comps.last().map(|c| (*c).to_string())
}

/// True if a path component is only digits and dots (a version number).
fn looks_like_version(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.') && s.contains('.')
}

/// Path of the outermost `.app` bundle containing `path`, if any, e.g.
/// `/Applications/Google Chrome.app` for one of Chrome's nested helpers.
pub fn app_bundle(path: &str) -> Option<&str> {
    path.find(".app/").map(|end| &path[..end + 4])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn basenames_skip_version_directories() {
        let name = |p| meaningful_basename(p).unwrap();
        assert_eq!(name("/opt/tool/bin/tool"), "tool");
        assert_eq!(
            name("/Users/u/.local/share/claude/versions/2.1.286"),
            "claude"
        );
        assert_eq!(name("/x/tool/2.0.1"), "tool");
        assert_eq!(name("/x/versions/bin"), "bin");
        assert_eq!(
            app_bundle("/Applications/A.app/Contents/Frameworks/H.app/Contents/MacOS/H"),
            Some("/Applications/A.app")
        );
    }
}
