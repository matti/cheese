//! Per-process sampler: identity for EVERY process (cross-uid) plus precise
//! CPU/energy/disk/wakeups for same-uid processes.
//!
//! Sudoless reality, VERIFIED on this M5 (macOS 25.4): there is no unprivileged
//! syscall for cross-uid per-process CPU time / energy. `proc_pidinfo` and
//! `proc_pid_rusage` both return EPERM for other-uid pids; the legacy
//! `kinfo_proc` fields (`p_pctcpu`, `p_uticks/p_sticks`) read 0 on modern
//! kernels; the Mach `processor_set_tasks` route needs the
//! `com.apple.system-task-ports.read` entitlement. `ps`/`top` only show
//! cross-uid CPU because they are setuid-root / entitled binaries. So we do
//! the maximum that is genuinely sudoless:
//!
//!   * `sysctl KERN_PROC_ALL` -> one `kinfo_proc` per process for the WHOLE
//!     system, carrying pid / effective uid / ppid / comm. Cross-uid. Every
//!     process (WindowServer, root daemons, ...) appears by name and owner.
//!   * `proc_pidinfo(PROC_PIDTASKINFO)` -> `pti_total_user/system` CPU time,
//!     same-uid only -> per-interval CPU%.
//!   * `proc_pid_rusage(RUSAGE_INFO_V6)` -> `ri_energy_nj`, disk bytes and
//!     wakeups, same-uid only.
//!
//! Other-uid rows carry `None` for every metric (shown blank), never fabricated.
//!
//! Names: for same-uid processes `KERN_PROCARGS2` gives the real invoked path
//! (e.g. `/opt/tool/bin/tool` -> "tool", instead of a versioned binary
//! basename like "2.1.158"). For cross-uid processes (where procargs is
//! denied) we fall back to `proc_pidpath`'s basename, then to `comm`.

use std::collections::HashMap;
use std::time::Duration;

use crate::sampler::{ProcSampler, Sampler};
use crate::types::ProcRow;

// ---------------------------------------------------------------------------
// FFI: constants and structs from the macOS SDK headers (verified on target).
// ---------------------------------------------------------------------------

/// `RUSAGE_INFO_V6` from `<sys/resource.h>`.
const RUSAGE_INFO_V6: libc::c_int = 6;

/// `PROC_PIDTASKINFO` from `<sys/proc_info.h>`.
const PROC_PIDTASKINFO: libc::c_int = 4;

// `sysctl` MIB pieces from `<sys/sysctl.h>`.
const CTL_KERN: libc::c_int = 1;
const KERN_PROC: libc::c_int = 14;
const KERN_PROC_ALL: libc::c_int = 0;
const KERN_PROC_PID: libc::c_int = 1;
const KERN_PROCARGS2: libc::c_int = 49;

// `kinfo_proc` field byte-offsets, taken from the compiler on this machine via
// `offsetof` (sizeof = 648). Parsing by offset avoids transcribing the entire
// fragile struct (which embeds many pointer-sized members and sub-structs).
const KINFO_PROC_SIZE: usize = 648;
const OFF_P_STARTTIME: usize = 0; // kp_proc.p_un.__p_starttime (struct timeval)
const OFF_P_PID: usize = 40; // kp_proc.p_pid            (pid_t / i32)
const OFF_P_COMM: usize = 243; // kp_proc.p_comm         (char[MAXCOMLEN+1] = 17)
const OFF_CR_UID: usize = 420; // kp_eproc.e_ucred.cr_uid (uid_t / u32)
const OFF_E_PPID: usize = 560; // kp_eproc.e_ppid        (pid_t / i32)
const COMM_LEN: usize = 17; // MAXCOMLEN + 1

/// `struct rusage_info_v6` from `<sys/resource.h>` (16-byte uuid + u64 fields).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct RUsageInfoV6 {
    ri_uuid: [u8; 16],
    ri_user_time: u64,
    ri_system_time: u64,
    ri_pkg_idle_wkups: u64,
    ri_interrupt_wkups: u64,
    ri_pageins: u64,
    ri_wired_size: u64,
    ri_resident_size: u64,
    ri_phys_footprint: u64,
    ri_proc_start_abstime: u64,
    ri_proc_exit_abstime: u64,
    ri_child_user_time: u64,
    ri_child_system_time: u64,
    ri_child_pkg_idle_wkups: u64,
    ri_child_interrupt_wkups: u64,
    ri_child_pageins: u64,
    ri_child_elapsed_abstime: u64,
    ri_diskio_bytesread: u64,
    ri_diskio_byteswritten: u64,
    ri_cpu_time_qos_default: u64,
    ri_cpu_time_qos_maintenance: u64,
    ri_cpu_time_qos_background: u64,
    ri_cpu_time_qos_utility: u64,
    ri_cpu_time_qos_legacy: u64,
    ri_cpu_time_qos_user_initiated: u64,
    ri_cpu_time_qos_user_interactive: u64,
    ri_billed_system_time: u64,
    ri_serviced_system_time: u64,
    ri_logical_writes: u64,
    ri_lifetime_max_phys_footprint: u64,
    ri_instructions: u64,
    ri_cycles: u64,
    ri_billed_energy: u64,
    ri_serviced_energy: u64,
    ri_interval_max_phys_footprint: u64,
    ri_runnable_time: u64,
    ri_flags: u64,
    ri_user_ptime: u64,
    ri_system_ptime: u64,
    ri_pinstructions: u64,
    ri_pcycles: u64,
    ri_energy_nj: u64,
    ri_penergy_nj: u64,
    ri_secure_time_in_system: u64,
    ri_secure_ptime_in_system: u64,
    ri_neural_footprint: u64,
    ri_lifetime_max_neural_footprint: u64,
    ri_interval_max_neural_footprint: u64,
    ri_reserved: [u64; 9],
}

impl Default for RUsageInfoV6 {
    fn default() -> Self {
        // SAFETY: an all-zero RUsageInfoV6 is a valid value (all integer fields).
        unsafe { std::mem::zeroed() }
    }
}

/// `struct proc_taskinfo` from `<sys/proc_info.h>`. Only CPU-time fields are
/// read; the rest must be present so the struct size (96) matches the kernel.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct ProcTaskInfo {
    pti_virtual_size: u64,
    pti_resident_size: u64,
    pti_total_user: u64,
    pti_total_system: u64,
    pti_threads_user: u64,
    pti_threads_system: u64,
    pti_policy: i32,
    pti_faults: i32,
    pti_pageins: i32,
    pti_cow_faults: i32,
    pti_messages_sent: i32,
    pti_messages_received: i32,
    pti_syscalls_mach: i32,
    pti_syscalls_unix: i32,
    pti_csw: i32,
    pti_threadnum: i32,
    pti_numrunning: i32,
    pti_priority: i32,
}

unsafe extern "C" {
    fn proc_pid_rusage(
        pid: libc::c_int,
        flavor: libc::c_int,
        buffer: *mut libc::c_void,
    ) -> libc::c_int;

    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        arg: u64,
        buffer: *mut libc::c_void,
        buffersize: libc::c_int,
    ) -> libc::c_int;
}

// ---------------------------------------------------------------------------
// Raw process listing parsed out of KERN_PROC_ALL.
// ---------------------------------------------------------------------------

struct ProcEntry {
    pid: i32,
    ppid: i32,
    uid: u32,
    comm: String,
}

/// Enumerate every process on the system via `sysctl KERN_PROC_ALL`, reading
/// pid / ppid / uid / comm by verified byte-offset. Sudoless and cross-uid.
fn list_all_procs() -> Vec<ProcEntry> {
    // Over-allocate slightly; the table can grow between the two calls.
    let Some(buf) = sysctl_mib(
        &mut [CTL_KERN, KERN_PROC, KERN_PROC_ALL],
        KINFO_PROC_SIZE * 16,
    ) else {
        return Vec::new();
    };
    let n = buf.len() / KINFO_PROC_SIZE;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let base = i * KINFO_PROC_SIZE;
        let pid = read_i32(&buf, base + OFF_P_PID);
        if pid <= 0 {
            continue;
        }
        let ppid = read_i32(&buf, base + OFF_E_PPID);
        let uid = read_u32(&buf, base + OFF_CR_UID);
        let comm = read_cstr(&buf[base + OFF_P_COMM..base + OFF_P_COMM + COMM_LEN]);
        out.push(ProcEntry {
            pid,
            ppid,
            uid,
            comm,
        });
    }
    out
}

fn read_i32(buf: &[u8], off: usize) -> i32 {
    i32::from_ne_bytes(buf[off..off + 4].try_into().unwrap())
}

fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap())
}

/// Read a NUL-terminated string out of a fixed byte slice.
fn read_cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

// ---------------------------------------------------------------------------
// Per-process reads.
// ---------------------------------------------------------------------------

/// Read `proc_taskinfo` for `pid`. Works cross-uid for CPU times.
fn read_taskinfo(pid: i32) -> Option<ProcTaskInfo> {
    let mut ti = ProcTaskInfo::default();
    let size = std::mem::size_of::<ProcTaskInfo>() as libc::c_int;
    let rc = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDTASKINFO,
            0,
            &mut ti as *mut ProcTaskInfo as *mut libc::c_void,
            size,
        )
    };
    if rc == size { Some(ti) } else { None }
}

/// Read RUSAGE_INFO_V6 for `pid`. Returns `None` on EPERM (other-uid) / dead.
fn read_rusage_v6(pid: i32) -> Option<RUsageInfoV6> {
    let mut buf = RUsageInfoV6::default();
    let rc = unsafe {
        proc_pid_rusage(
            pid,
            RUSAGE_INFO_V6,
            &mut buf as *mut RUsageInfoV6 as *mut libc::c_void,
        )
    };
    if rc == 0 { Some(buf) } else { None }
}

/// Best name for `pid`: prefer the real invoked path from KERN_PROCARGS2
/// (same-uid only), then the shared cross-uid `proc_pidpath` basename, then the
/// kinfo_proc `comm`. Both paths are de-versioned (e.g. ".../2.1.158" -> the
/// nearest meaningful component) via the shared `procname` helper.
fn resolve_name(pid: i32, comm: &str) -> String {
    if let Some(path) = procargs_exec_path(pid)
        && let Some(name) = crate::samplers::procname::meaningful_basename(&path)
    {
        return name;
    }
    let by_path = crate::samplers::procname::resolve_name(pid);
    if !by_path.starts_with("pid ") {
        return by_path;
    }
    if !comm.is_empty() {
        return comm.to_string();
    }
    format!("pid {pid}")
}

/// The invoked exec path (argv exec_path) via `sysctl KERN_PROCARGS2`. Returns
/// `None` for processes we may not inspect (other-uid) - that's expected.
fn procargs_exec_path(pid: i32) -> Option<String> {
    procargs(pid).map(|(exec_path, _)| exec_path)
}

/// Command-line arguments (argv) of a same-uid process. `None` when access is
/// denied (other-uid) or the process exited.
pub fn process_args(pid: i32) -> Option<Vec<String>> {
    procargs(pid).map(|(_, argv)| argv)
}

/// Raw `sysctl KERN_PROCARGS2` read, split into exec path and argv.
fn procargs(pid: i32) -> Option<(String, Vec<String>)> {
    sysctl_mib(&mut [CTL_KERN, KERN_PROCARGS2, pid], 0)
        .filter(|buf| buf.len() >= 4)
        .and_then(|buf| parse_procargs(&buf))
}

/// Size-then-read `sysctl` for a variable-length MIB value. `slack` extra bytes
/// absorb growth between the two calls.
fn sysctl_mib(mib: &mut [libc::c_int], slack: usize) -> Option<Vec<u8>> {
    let mut len: libc::size_t = 0;
    // SAFETY: a null buffer asks the kernel for the required length.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 {
        return None;
    }
    len += slack;
    let mut buf = vec![0u8; len];
    // SAFETY: buf holds `len` bytes; the kernel writes at most that many.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(len);
    Some(buf)
}

/// Layout: int32 argc, NUL-terminated exec_path, NUL padding, then argc
/// NUL-terminated argv strings (followed by the environment, ignored here).
fn parse_procargs(buf: &[u8]) -> Option<(String, Vec<String>)> {
    let argc = usize::try_from(read_i32(buf.get(..4)?, 0)).ok()?;
    let rest = &buf[4..];
    let exec_path = read_cstr(rest);
    if exec_path.is_empty() {
        return None;
    }
    let mut pos = exec_path.len();
    while rest.get(pos) == Some(&0) {
        pos += 1;
    }
    let argv = rest[pos.min(rest.len())..]
        .split(|b| *b == 0)
        .take(argc)
        // A process that rewrote its title (npm, postgres) leaves empty slots.
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    Some((exec_path, argv))
}

/// Wall-clock start time (Unix seconds) of any process, cross-uid, from
/// `kinfo_proc.kp_proc.p_starttime` via `sysctl KERN_PROC_PID`.
pub fn started_at(pid: i32) -> Option<f64> {
    let mut mib = [CTL_KERN, KERN_PROC, KERN_PROC_PID, pid];
    let mut buf = vec![0u8; KINFO_PROC_SIZE];
    let mut len: libc::size_t = buf.len();
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len < KINFO_PROC_SIZE || read_i32(&buf, OFF_P_PID) != pid {
        return None;
    }
    let seconds = i64::from_ne_bytes(buf[OFF_P_STARTTIME..OFF_P_STARTTIME + 8].try_into().ok()?);
    let micros = read_i32(&buf, OFF_P_STARTTIME + 8);
    (seconds > 0).then(|| seconds as f64 + micros as f64 / 1e6)
}

// ---------------------------------------------------------------------------
// Sampler
// ---------------------------------------------------------------------------

/// Previous-tick CPU snapshot per pid (mach abstime ticks).
#[derive(Clone, Copy)]
struct CpuSnap {
    total_ticks: u64,
}

pub struct ProcRusageSampler {
    uid: u32,
    timebase_num: f64,
    timebase_den: f64,
    prev_rusage: HashMap<i32, RUsageInfoV6>,
    prev_cpu: HashMap<i32, CpuSnap>,
    names: HashMap<i32, String>,
}

impl ProcRusageSampler {
    pub fn new() -> Self {
        // `pti_total_*` and `ri_user/system_time` are mach abstime ticks, NOT
        // nanoseconds (verified M5: 1 tick = numer/denom ns = 125/3). Convert
        // ticks -> ns via the timebase. `ri_energy_nj` is genuine nanojoules.
        let mut tb = mach2::mach_time::mach_timebase_info { numer: 1, denom: 1 };
        unsafe {
            mach2::mach_time::mach_timebase_info(&mut tb);
        }
        Self {
            uid: unsafe { libc::getuid() },
            timebase_num: tb.numer as f64,
            timebase_den: tb.denom as f64,
            prev_rusage: HashMap::new(),
            prev_cpu: HashMap::new(),
            names: HashMap::new(),
        }
    }

    fn name_for(&mut self, pid: i32, comm: &str) -> String {
        if let Some(n) = self.names.get(&pid) {
            return n.clone();
        }
        let n = resolve_name(pid, comm);
        self.names.insert(pid, n.clone());
        n
    }

    fn ticks_to_ns(&self, ticks: u64) -> f64 {
        ticks as f64 * self.timebase_num / self.timebase_den
    }
}

impl Sampler for ProcRusageSampler {
    fn name(&self) -> &'static str {
        "proc cpu+rusage"
    }
}

impl ProcSampler for ProcRusageSampler {
    fn tick(&mut self, dt: Duration) -> Vec<ProcRow> {
        let dt_secs = dt.as_secs_f64().max(1e-9);
        let entries = list_all_procs();

        let mut rows = Vec::with_capacity(entries.len());
        let mut cur_rusage: HashMap<i32, RUsageInfoV6> = HashMap::new();
        let mut cur_cpu: HashMap<i32, CpuSnap> = HashMap::with_capacity(entries.len());

        for e in &entries {
            let pid = e.pid;
            let same_uid = e.uid == self.uid;

            // --- CPU time (same-uid only via PROC_PIDTASKINFO) ---
            // For other-uid processes this is EPERM, so cpu_percent stays None;
            // the row still appears with name + owner.
            let mut cpu_percent = None;
            if same_uid && let Some(ti) = read_taskinfo(pid) {
                let total_ticks = ti.pti_total_user + ti.pti_total_system;
                cur_cpu.insert(pid, CpuSnap { total_ticks });
                if let Some(prev) = self.prev_cpu.get(&pid) {
                    let d_ticks = total_ticks.saturating_sub(prev.total_ticks);
                    cpu_percent = Some(self.ticks_to_ns(d_ticks) / 1e9 / dt_secs * 100.0);
                }
            }

            // --- Energy / disk / wakeups (same-uid only via rusage V6) ---
            let mut start_abstime = None;
            let mut footprint_bytes = None;
            let mut energy_mw = None;
            let mut lifetime_energy_j = None;
            let mut disk_read_bps = None;
            let mut disk_write_bps = None;
            let mut wakeups_per_s = None;

            if same_uid && let Some(cur) = read_rusage_v6(pid) {
                cur_rusage.insert(pid, cur);
                start_abstime = Some(cur.ri_proc_start_abstime);
                footprint_bytes = Some(cur.ri_phys_footprint);
                if self.prev_rusage.get(&pid).is_some_and(|p| {
                    p.ri_proc_start_abstime != cur.ri_proc_start_abstime || p.ri_uuid != cur.ri_uuid
                }) {
                    cpu_percent = None;
                    self.names.remove(&pid);
                }
                // Lifetime counter: available on the very first tick, no
                // delta needed.
                lifetime_energy_j = Some(cur.ri_energy_nj as f64 / 1e9);
                if let Some(prev) = self.prev_rusage.get(&pid).filter(|p| {
                    p.ri_proc_start_abstime == cur.ri_proc_start_abstime && p.ri_uuid == cur.ri_uuid
                }) {
                    let d_energy = cur.ri_energy_nj.saturating_sub(prev.ri_energy_nj);
                    let d_read = cur
                        .ri_diskio_bytesread
                        .saturating_sub(prev.ri_diskio_bytesread);
                    let d_written = cur
                        .ri_diskio_byteswritten
                        .saturating_sub(prev.ri_diskio_byteswritten);
                    let d_wakeups = (cur.ri_pkg_idle_wkups + cur.ri_interrupt_wkups)
                        .saturating_sub(prev.ri_pkg_idle_wkups + prev.ri_interrupt_wkups);
                    // nJ -> mW: nJ/1e6 = mJ; mJ/s = mW.
                    energy_mw = Some(d_energy as f64 / 1e6 / dt_secs);
                    disk_read_bps = Some(d_read as f64 / dt_secs);
                    disk_write_bps = Some(d_written as f64 / dt_secs);
                    wakeups_per_s = Some(d_wakeups as f64 / dt_secs);
                }
            }

            rows.push(ProcRow {
                pid,
                start_abstime,
                footprint_bytes,
                executable: crate::samplers::procname::pidpath(pid),
                ppid: e.ppid,
                uid: e.uid,
                same_uid,
                name: self.name_for(pid, &e.comm),
                cpu_percent,
                energy_mw,
                lifetime_energy_j,
                disk_read_bps,
                disk_write_bps,
                wakeups_per_s,
            });
        }

        // Roll snapshots forward; prune dead pids so the maps stay bounded.
        // "Live" = every pid we saw this tick (all uids), used for the name
        // cache; the CPU/rusage snapshot maps only ever hold same-uid pids.
        let live: std::collections::HashSet<i32> = rows.iter().map(|r| r.pid).collect();
        self.prev_rusage = cur_rusage;
        self.prev_cpu = cur_cpu;
        self.names.retain(|p, _| live.contains(p));

        rows
    }
}

/// Stable process birth counter for probes that can outlive a PID.
pub fn process_start(pid: i32) -> Option<u64> {
    read_rusage_v6(pid).map(|r| r.ri_proc_start_abstime)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn procargs_splits_exec_path_padding_and_argv_without_environment() {
        let mut buf = 3i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/usr/bin/node\0\0\0node\0test.js\0--workers=4\0HOME=/x\0");
        let (exec, argv) = parse_procargs(&buf).unwrap();
        assert_eq!(exec, "/usr/bin/node");
        assert_eq!(argv, ["node", "test.js", "--workers=4"]);
        let mut retitled = 3i32.to_ne_bytes().to_vec();
        retitled.extend_from_slice(b"/bin/node\0\0npm exec vite\0\0\0\0\0HOME=/x\0");
        assert_eq!(parse_procargs(&retitled).unwrap().1, ["npm exec vite"]);
        assert!(parse_procargs(&[1, 0]).is_none());
    }
    #[test]
    fn own_start_time_is_in_the_recent_past() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let start = started_at(std::process::id() as i32).unwrap();
        assert!(start <= now && now - start < 3600., "{start} vs {now}");
        let argv = process_args(std::process::id() as i32).unwrap();
        assert!(!argv.is_empty());
    }
}
