//! System-wide pressure evidence: CPU oversubscription, memory pressure, swap
//! and power context. Native sysctl/mach reads only; nothing is shelled out.
use crate::{
    host::{sysctl_string, sysctl_value},
    types::{BatteryFrame, BatteryState, Sample, ThermalFrame},
};
use serde::Serialize;

const GIB: f64 = 1024. * 1024. * 1024.;

#[derive(Serialize, Clone, Debug)]
pub struct PerfLevel {
    pub name: String,
    pub logical_cpus: u32,
}

#[derive(Serialize, Clone, Debug)]
pub struct Load {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
    pub logical_cpus: u32,
    /// Core clusters, fastest first (`hw.perflevelN`), e.g. Performance/Efficiency.
    pub perf_levels: Vec<PerfLevel>,
    /// 1-minute load average divided by logical CPUs.
    pub ratio: f64,
    pub verdict: &'static str,
}

/// Load averages from `getloadavg` versus the logical CPU count.
pub fn load() -> Option<Load> {
    let mut avg = [0f64; 3];
    // SAFETY: the buffer holds the three requested doubles.
    if unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) } != 3 {
        return None;
    }
    let logical_cpus = u32::try_from(sysctl_value::<i32>("hw.logicalcpu")?)
        .ok()
        .filter(|n| *n > 0)?;
    let perf_levels = (0..sysctl_value::<i32>("hw.nperflevels").unwrap_or(0))
        .filter_map(|i| {
            Some(PerfLevel {
                name: sysctl_string(&format!("hw.perflevel{i}.name"))
                    .unwrap_or_else(|| format!("level {i}")),
                logical_cpus: u32::try_from(sysctl_value::<i32>(&format!(
                    "hw.perflevel{i}.logicalcpu"
                ))?)
                .ok()?,
            })
        })
        .collect();
    let ratio = avg[0] / logical_cpus as f64;
    Some(Load {
        one: avg[0],
        five: avg[1],
        fifteen: avg[2],
        logical_cpus,
        perf_levels,
        ratio,
        verdict: load_verdict(ratio),
    })
}

/// Runnable threads per logical CPU: above 1 means work is queueing for a core.
pub fn load_verdict(ratio: f64) -> &'static str {
    match ratio {
        r if !r.is_finite() || r < 0. => "unknown",
        r if r < 0.75 => "normal",
        r if r < 1. => "busy",
        r if r < 2. => "oversubscribed",
        _ => "severely oversubscribed",
    }
}

/// Cumulative VM counters (pages, except `compressor_bytes`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VmCounters {
    pub pageouts: u64,
    pub swapins: u64,
    pub swapouts: u64,
    pub compressor_bytes: u64,
}

unsafe extern "C" {
    // Declared here because libc marks its binding deprecated in favour of
    // mach2, which (0.4) does not provide it.
    fn mach_host_self() -> libc::mach_port_t;
}

/// `host_statistics64(HOST_VM_INFO64)`: the counters behind `vm_stat`.
pub fn vm_counters() -> Option<VmCounters> {
    let mut info = std::mem::MaybeUninit::<libc::vm_statistics64>::zeroed();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: `info` has room for HOST_VM_INFO64_COUNT integers; the host port
    // right obtained here is released again below.
    let rc = unsafe {
        let host = mach_host_self();
        let rc = libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            info.as_mut_ptr().cast(),
            &mut count,
        );
        mach2::mach_port::mach_port_deallocate(mach2::traps::mach_task_self(), host);
        rc
    };
    if rc != libc::KERN_SUCCESS {
        return None;
    }
    // SAFETY: filled by the successful call above.
    let info = unsafe { info.assume_init() };
    // SAFETY: sysconf has no preconditions.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    Some(VmCounters {
        pageouts: info.pageouts,
        swapins: info.swapins,
        swapouts: info.swapouts,
        compressor_bytes: u64::from(info.compressor_page_count) * page,
    })
}

#[derive(Serialize, Clone, Debug)]
pub struct Memory {
    pub total_gb: Option<f64>,
    /// `kern.memorystatus_vm_pressure_level`: the kernel's own verdict.
    pub pressure: &'static str,
    /// Physical memory occupied by the compressor (what Activity Monitor calls Compressed).
    pub compressor_gb: Option<f64>,
    pub swap_used_gb: Option<f64>,
    pub swap_total_gb: Option<f64>,
    /// Page counter deltas over the observation window.
    pub window_s: f64,
    pub pageouts: Option<u64>,
    pub swapins: Option<u64>,
    pub swapouts: Option<u64>,
    pub verdict: &'static str,
}

pub fn memory(before: Option<VmCounters>, after: Option<VmCounters>, window_s: f64) -> Memory {
    let level = sysctl_value::<i32>("kern.memorystatus_vm_pressure_level");
    let total = sysctl_value::<u64>("hw.memsize");
    let swap = sysctl_value::<libc::xsw_usage>("vm.swapusage");
    let delta = before.zip(after).map(|(b, a)| VmCounters {
        pageouts: a.pageouts.saturating_sub(b.pageouts),
        swapins: a.swapins.saturating_sub(b.swapins),
        swapouts: a.swapouts.saturating_sub(b.swapouts),
        compressor_bytes: a.compressor_bytes,
    });
    let compressor = after.map(|a| a.compressor_bytes);
    Memory {
        total_gb: total.map(|t| t as f64 / GIB),
        pressure: pressure_name(level),
        compressor_gb: compressor.map(|c| c as f64 / GIB),
        swap_used_gb: swap.map(|s| s.xsu_used as f64 / GIB),
        swap_total_gb: swap.map(|s| s.xsu_total as f64 / GIB),
        window_s,
        pageouts: delta.map(|d| d.pageouts),
        swapins: delta.map(|d| d.swapins),
        swapouts: delta.map(|d| d.swapouts),
        verdict: memory_verdict(
            level,
            delta.map(|d| d.swapins + d.swapouts),
            compressor.zip(total).map(|(c, t)| c as f64 / t as f64),
        ),
    }
}

pub fn pressure_name(level: Option<i32>) -> &'static str {
    match level {
        Some(1) => "normal",
        Some(2) => "warning",
        Some(4) => "critical",
        _ => "unknown",
    }
}

/// Kernel pressure first; then active swapping during the window; then a
/// compressor holding a quarter or more of RAM.
pub fn memory_verdict(
    level: Option<i32>,
    swapped_pages: Option<u64>,
    compressor_share: Option<f64>,
) -> &'static str {
    match pressure_name(level) {
        "critical" => "critical",
        "warning" => "warning",
        _ if swapped_pages.is_some_and(|p| p > 0) => "swapping",
        _ if compressor_share.is_some_and(|s| s >= 0.25) => "heavily compressed",
        "unknown" => "unknown",
        _ => "normal",
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Power {
    pub source: &'static str,
    /// Authoritative charge direction, derived from the measured mean pack power
    /// (`battery_w`), not from macOS' charging flag.
    pub battery_state: BatteryState,
    pub battery_percent: f64,
    /// Mean pack power over the window: positive charges the battery, negative drains it.
    pub battery_w: f64,
    /// Rated power of the connected adapter (AdapterDetails.Watts), if known.
    pub adapter_watts: Option<f64>,
    /// Raw macOS IsCharging flag (also behind pmset's "charging"). It can stay
    /// true while the pack measurably drains; `battery_state` wins.
    pub macos_is_charging_flag: bool,
    pub time_remaining_min: Option<i64>,
    pub thermal_pressure: Option<String>,
    pub hottest_cpu_c: Option<f64>,
    pub smc_system_w: Option<f64>,
    pub note: String,
}

/// Accumulates power and thermal readings across the observation window.
#[derive(Default)]
pub struct PowerWindow {
    battery: Option<BatteryFrame>,
    thermal: Option<ThermalFrame>,
    pack_w: Vec<f64>,
    smc_w: Vec<f64>,
}

impl PowerWindow {
    pub fn observe(&mut self, sample: &Sample) {
        if let Some(b) = &sample.battery {
            self.pack_w.push(b.system_power_mw / 1000.);
            self.battery = Some(b.clone());
        }
        if let Some(t) = &sample.thermal {
            self.smc_w.extend(t.pstr_w);
            self.thermal = Some(t.clone());
        }
    }

    pub fn summary(&self) -> Option<Power> {
        let b = self.battery.as_ref()?;
        let battery_w = mean(&self.pack_w)?;
        let battery_state = BatteryState::derive(b.external_connected, b.soc_percent, battery_w);
        Some(Power {
            source: if b.external_connected {
                "AC"
            } else {
                "battery"
            },
            battery_state,
            battery_percent: b.soc_percent,
            battery_w,
            adapter_watts: b.adapter_watts,
            macos_is_charging_flag: b.is_charging,
            time_remaining_min: b.time_remaining_min,
            thermal_pressure: self.thermal.as_ref().map(|t| t.pressure.label()),
            hottest_cpu_c: self.thermal.as_ref().and_then(|t| t.cpu_die_max_c),
            smc_system_w: mean(&self.smc_w),
            note: power_note(battery_state, b.soc_percent, battery_w, b.adapter_watts),
        })
    }
}

fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

/// One-line reading of the power situation, e.g. a low battery on AC that
/// barely charges because the load consumes most of the adapter's power.
pub fn power_note(
    state: BatteryState,
    soc: f64,
    pack_w: f64,
    adapter_watts: Option<f64>,
) -> String {
    match state {
        BatteryState::DrainingOnBattery => {
            format!("on battery at {soc:.0}%, draining {:.1} W", -pack_w)
        }
        BatteryState::DrainingOnAc => match adapter_watts {
            Some(a) => format!(
                "on AC but still draining {:.1} W at {soc:.0}%: the {a:.0} W adapter cannot cover the load; use a higher-wattage charger",
                -pack_w
            ),
            None => format!(
                "on AC but still draining {:.1} W at {soc:.0}%: the adapter cannot cover the load",
                -pack_w
            ),
        },
        BatteryState::Charging if soc < 80. && pack_w < 15. => format!(
            "on AC but battery {soc:.0}% and charging slowly ({pack_w:.1} W into the pack); load may be using most of the adapter's power"
        ),
        BatteryState::Charging => format!("on AC, charging at {pack_w:.1} W ({soc:.0}%)"),
        BatteryState::NotChargingOnAc if soc < 80. => format!(
            "on AC but not charging at {soc:.0}% (load, a charge limit or optimized charging)"
        ),
        BatteryState::NotChargingOnAc | BatteryState::Full => format!("on AC, battery {soc:.0}%"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_ratio_classification() {
        assert_eq!(load_verdict(0.2), "normal");
        assert_eq!(load_verdict(0.9), "busy");
        assert_eq!(load_verdict(1.5), "oversubscribed");
        assert_eq!(load_verdict(136. / 18.), "severely oversubscribed");
        assert_eq!(load_verdict(f64::NAN), "unknown");
    }

    #[test]
    fn memory_pressure_classification() {
        assert_eq!(pressure_name(Some(1)), "normal");
        assert_eq!(pressure_name(Some(3)), "unknown");
        assert_eq!(memory_verdict(Some(4), Some(0), Some(0.)), "critical");
        assert_eq!(memory_verdict(Some(2), None, None), "warning");
        assert_eq!(memory_verdict(Some(1), Some(12), Some(0.)), "swapping");
        assert_eq!(
            memory_verdict(Some(1), Some(0), Some(16. / 48.)),
            "heavily compressed"
        );
        assert_eq!(memory_verdict(Some(1), Some(0), Some(0.1)), "normal");
        assert_eq!(memory_verdict(None, None, None), "unknown");
    }

    fn note(soc: f64, ac: bool, pack_w: f64, adapter: Option<f64>) -> String {
        power_note(BatteryState::derive(ac, soc, pack_w), soc, pack_w, adapter)
    }

    #[test]
    fn battery_state_follows_measured_pack_power() {
        use BatteryState::*;
        assert_eq!(BatteryState::derive(true, 44., -60.5), DrainingOnAc);
        assert_eq!(BatteryState::derive(true, 44., 20.), Charging);
        assert_eq!(BatteryState::derive(false, 44., -12.), DrainingOnBattery);
        assert_eq!(BatteryState::derive(false, 100., 0.), DrainingOnBattery);
        assert_eq!(BatteryState::derive(true, 100., 0.1), Full);
        assert_eq!(BatteryState::derive(true, 60., -0.2), NotChargingOnAc);
        let json = serde_json::to_value(DrainingOnAc).unwrap();
        assert_eq!(json, "draining_on_ac");
    }

    #[test]
    fn power_notes_explain_ac_and_charging_state() {
        assert!(note(8., true, 4., None).contains("charging slowly"));
        assert!(note(8., true, -3., None).contains("still draining 3.0 W"));
        assert_eq!(
            note(44., true, -58., Some(60.)),
            "on AC but still draining 58.0 W at 44%: the 60 W adapter cannot cover the load; use a higher-wattage charger"
        );
        assert!(note(50., false, -12., None).starts_with("on battery at 50%"));
        assert!(note(60., true, 0., None).contains("not charging"));
        assert_eq!(note(100., true, 0., None), "on AC, battery 100%");
    }

    #[test]
    fn summary_reports_measured_drain_over_charging_flag() {
        let mut window = PowerWindow::default();
        window.observe(&Sample {
            battery: Some(BatteryFrame {
                system_power_mw: -58_000.,
                soc_percent: 44.,
                is_charging: true,
                external_connected: true,
                adapter_watts: Some(60.),
                ..Default::default()
            }),
            dt: std::time::Duration::from_secs(1),
            soc: None,
            thermal: None,
            gpu: None,
            io: None,
            procs: vec![],
        });
        let p = window.summary().unwrap();
        assert_eq!(p.battery_state, BatteryState::DrainingOnAc);
        assert!(p.macos_is_charging_flag);
        let json = serde_json::to_value(&p).unwrap();
        assert!(json.get("charging").is_none());
        assert_eq!(json["battery_state"], "draining_on_ac");
        assert_eq!(json["adapter_watts"], 60.);
    }

    #[test]
    fn native_readings_are_available_on_this_mac() {
        let load = load().unwrap();
        assert!(load.logical_cpus > 0 && load.one >= 0.);
        let counters = vm_counters().unwrap();
        let memory = memory(Some(counters), vm_counters(), 0.);
        assert_ne!(memory.pressure, "unknown");
        assert!(memory.total_gb.unwrap() > 1.);
        assert!(memory.swap_total_gb.is_some());
    }
}
