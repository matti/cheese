//! Owned, plain-old-data types that cross sampler boundaries.
//!
//! Nothing here holds a CoreFoundation / IOKit reference, so these are all
//! `Send` and safe to ship to a UI thread in later milestones.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Whole-system battery / power-source state at one instant.
///
/// Ground truth for total drain: `system_power_mw = voltage_mv * amperage_ma / 1000`.
/// Negative power means the battery is discharging (running on battery).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct BatteryFrame {
    /// `Voltage * InstantAmperage / 1000`. Negative = discharging.
    pub system_power_mw: f64,
    /// Pack voltage in millivolts (always positive).
    pub voltage_mv: f64,
    /// Instantaneous current in milliamps (signed; negative = discharging).
    pub instant_amperage_ma: f64,
    /// State of charge as a percentage (0-100). On Apple Silicon this is
    /// already a percent, not a mAh value.
    pub soc_percent: f64,
    /// Battery pack temperature in degrees Celsius.
    pub temperature_c: f64,
    /// Gauge estimate of time-to-empty / time-to-full in minutes, if known.
    pub time_remaining_min: Option<i64>,
    /// Raw macOS `IsCharging` flag. It can stay set while the pack measurably
    /// discharges because the load exceeds the adapter, so it is not the charge
    /// direction: use [`BatteryFrame::state`] / [`BatteryState`] for that.
    #[serde(rename = "macos_is_charging_flag", alias = "is_charging")]
    pub is_charging: bool,
    /// True if an external power source (wall adapter) is connected.
    pub external_connected: bool,
    /// Rated adapter power from `AdapterDetails.Watts`, when an adapter is connected.
    #[serde(default)]
    pub adapter_watts: Option<f64>,
}

impl BatteryFrame {
    /// Charge direction from this frame's instantaneous measured pack power.
    pub fn state(&self) -> BatteryState {
        BatteryState::derive(
            self.external_connected,
            self.soc_percent,
            self.system_power_mw / 1000.,
        )
    }
}

/// Authoritative battery state, derived from the measured pack current rather
/// than macOS' `IsCharging` flag.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BatteryState {
    Charging,
    /// External power is connected but the load still pulls from the pack.
    DrainingOnAc,
    DrainingOnBattery,
    /// On external power, (near) zero pack current and nearly full.
    Full,
    /// On external power, (near) zero pack current below full: load, a charge
    /// limit or optimized charging is holding the level.
    NotChargingOnAc,
}

impl BatteryState {
    /// Pack power within this band (W) counts as neither charging nor draining.
    const IDLE_W: f64 = 0.5;

    /// `pack_w` is measured pack power: positive charges, negative drains.
    pub fn derive(external_connected: bool, soc_percent: f64, pack_w: f64) -> Self {
        if !external_connected {
            Self::DrainingOnBattery
        } else if pack_w < -Self::IDLE_W {
            Self::DrainingOnAc
        } else if pack_w > Self::IDLE_W {
            Self::Charging
        } else if soc_percent >= 95. {
            Self::Full
        } else {
            Self::NotChargingOnAc
        }
    }

    /// Short upper-case label for UI cards.
    pub fn label(self) -> &'static str {
        match self {
            Self::Charging => "CHARGING",
            Self::DrainingOnAc => "DRAINING ON AC",
            Self::DrainingOnBattery => "ON BATTERY",
            Self::Full | Self::NotChargingOnAc => "AC POWER",
        }
    }
}

/// Per-process deltas over one sampling interval.
///
/// SUDOLESS REALITY (verified on this M5, macOS 25.4): there is NO unprivileged
/// syscall that yields cross-uid per-process CPU time or energy. `ps`/`top`
/// only show it because they are setuid-root / hold the
/// `com.apple.system-task-ports.read` entitlement. So:
///   * Process identity - pid / uid / ppid / name - IS available for EVERY
///     process via `sysctl KERN_PROC_ALL` + `proc_pidpath` (cross-uid).
///   * CPU%, energy, disk and wakeups come from `proc_pidinfo` /
///     `proc_pid_rusage`, which are same-uid only. For other-uid processes
///     (root daemons, WindowServer's `_windowserver`, etc.) they are `None`.
///
/// Every process therefore still appears in the table by name and owner; the
/// metrics columns are simply blank for the ones the OS will not let us read.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProcRow {
    pub pid: i32,
    pub start_abstime: Option<u64>,
    pub executable: Option<String>,
    pub footprint_bytes: Option<u64>,
    /// Parent pid, used for the optional app+helpers roll-up.
    pub ppid: i32,
    /// Owning (effective) user id of the process.
    pub uid: u32,
    /// True if this process is owned by the current user, i.e. its CPU /
    /// energy / disk / wakeup counters are readable sudolessly.
    pub same_uid: bool,
    /// Best-effort process name (executable basename, falling back to comm).
    pub name: String,
    /// CPU utilisation over the interval, in percent of one core (100% = one
    /// core fully busy). `None` for other-uid processes (not readable
    /// sudolessly on modern macOS).
    pub cpu_percent: Option<f64>,
    /// Hardware-derived power draw attributed to this process, in milliwatts.
    /// `None` for other-uid processes.
    pub energy_mw: Option<f64>,
    /// Total energy this process has burned since it started, in joules
    /// (`ri_energy_nj`, a lifetime counter). This is the honest answer to "what
    /// has actually been heating this machine": a process pulling 5 W for a
    /// fortnight has put far more heat into the SoC than a compile that spiked
    /// to 40 W for a minute. `None` for other-uid processes.
    pub lifetime_energy_j: Option<f64>,
    /// Disk bytes read per second over the interval. `None` if not same-uid.
    pub disk_read_bps: Option<f64>,
    /// Disk bytes written per second over the interval. `None` if not same-uid.
    pub disk_write_bps: Option<f64>,
    /// Idle + interrupt wakeups per second over the interval. `None` if not
    /// same-uid.
    pub wakeups_per_s: Option<f64>,
}

impl ProcRow {
    /// Rank score for sorting. Prefers measured energy, then measured CPU%
    /// scaled to a nominal per-core power so CPU-only same-uid rows still rank
    /// sensibly. Other-uid rows (no metrics) rank lowest but still appear.
    pub fn rank_mw(&self, nominal_mw_per_core: f64) -> f64 {
        if let Some(e) = self.energy_mw {
            return e;
        }
        if let Some(c) = self.cpu_percent {
            return c / 100.0 * nominal_mw_per_core;
        }
        -1.0
    }
}

/// System-wide SoC power, integrated from IOReport "Energy Model" energy
/// channels over the interval. All fields are watts; `None` if the matching
/// channel was not found on this chip.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SocPower {
    pub cpu_w: Option<f64>,
    pub gpu_w: Option<f64>,
    pub ane_w: Option<f64>,
    pub dram_w: Option<f64>,
    /// SoC display block (`DISP`/`DISPEXT` energy channels): the display
    /// controller and pipeline. NOT the panel or its backlight, which sit
    /// outside the SoC and are only visible as the gap between SoC power and
    /// battery drain.
    pub display_w: Option<f64>,
    /// Sum of every energy channel power saw (CPU+GPU+ANE+DRAM+others).
    pub total_w: f64,
    /// CPU active-residency fraction (0..1) from the representative cluster
    /// channel. `None` if no CPU Stats residency channels were found.
    pub cpu_active_residency: Option<f64>,
    /// GPU active-residency fraction (0..1). `None` if not found.
    pub gpu_active_residency: Option<f64>,
    /// Highest active CPU DVFS P-state index reached this interval (1-based
    /// ordinal; higher = faster step). Labels carry no MHz on Apple Silicon,
    /// so this is an ordinal, not a frequency. `None` if not found.
    pub cpu_top_pstate: Option<u32>,
    /// Highest active GPU DVFS P-state index reached this interval.
    pub gpu_top_pstate: Option<u32>,
}

/// One same-uid process's apportioned GPU share.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GpuUser {
    pub pid: i32,
    pub name: String,
    /// Raw `task_gpu_utilisation` counter delta (relative weight).
    pub util: u64,
    /// Apportioned GPU power, in milliwatts (estimate, not a hardware counter).
    pub gpu_mw: f64,
}

/// GPU attribution from the AGXAccelerator IOService + per-task utilisation.
///
/// On modern macOS the only sudoless cross-uid GPU signal is *who submitted*
/// work (`AGCInfo.fLastSubmissionPID`) and system-wide busy% / memory. True
/// per-process GPU utilisation needs a task port (`task_for_pid`), which is
/// blocked for other processes, so `users` is normally just power itself.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct GpuFrame {
    /// PID that submitted the most GPU work over the sampled sub-intervals.
    pub top_submitter_pid: Option<i32>,
    /// Resolved name of the top submitter (works cross-uid for naming).
    pub top_submitter_name: Option<String>,
    /// Owning uid of the top submitter, for the owner label.
    pub top_submitter_uid: Option<u32>,
    /// How many of the sub-samples named the top submitter (vs total).
    pub top_submitter_hits: u32,
    pub total_samples: u32,
    /// System GPU "Device Utilization %" (Activity-Monitor-style busy).
    pub device_util_pct: Option<f64>,
    /// GPU in-use system memory in bytes.
    pub in_use_mem_bytes: Option<u64>,
    /// Same-uid processes with a readable GPU utilisation share + apportioned
    /// power (estimate). Usually only power itself, due to the task-port wall.
    pub users: Vec<GpuUser>,
}

/// System-wide I/O throughput over the interval (disk + network), summed
/// across all block-storage drivers and all network interfaces. Sudoless.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct IoFrame {
    pub disk_read_bps: f64,
    pub disk_write_bps: f64,
    /// Disk operations per second (read + write), i.e. IOPS.
    pub disk_iops: f64,
    pub net_rx_bps: f64,
    pub net_tx_bps: f64,
}

/// macOS thermal pressure level, from
/// `com.apple.system.thermalpressurelevel` (0..3). The OS's own verdict on how
/// close the machine is to throttling.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThermalPressure {
    #[default]
    Nominal,
    Fair,
    Serious,
    Critical,
    /// State token outside the documented 0..3 range.
    Unknown(u64),
}

impl ThermalPressure {
    pub fn label(&self) -> String {
        match self {
            ThermalPressure::Nominal => "NOMINAL".to_string(),
            ThermalPressure::Fair => "FAIR".to_string(),
            ThermalPressure::Serious => "SERIOUS".to_string(),
            ThermalPressure::Critical => "CRITICAL".to_string(),
            ThermalPressure::Unknown(v) => format!("UNKNOWN({v})"),
        }
    }
    /// True when the OS reports heat pressure (anything above nominal).
    pub fn is_elevated(&self) -> bool {
        !matches!(self, ThermalPressure::Nominal)
    }
}

/// Die temperatures and thermal pressure from the AppleSMC + notify(3).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ThermalFrame {
    /// Hottest CPU die sensor (max over Tp*/Te*/Ts* keys), in Celsius.
    pub cpu_die_max_c: Option<f64>,
    /// Mean CPU die temperature across the CPU sensors, in Celsius.
    pub cpu_die_mean_c: Option<f64>,
    /// Hottest GPU die sensor (max over Tg* keys), in Celsius.
    pub gpu_die_max_c: Option<f64>,
    /// Total system power from the SMC `PSTR` key, in watts (independent
    /// cross-check against IOReport SoC watts). `None` if the key is absent.
    pub pstr_w: Option<f64>,
    /// Number of CPU + GPU temperature sensors actually read.
    pub sensor_count: usize,
    /// OS thermal pressure verdict.
    pub pressure: ThermalPressure,
}

/// One complete sample tick: everything power measured this interval.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Sample {
    /// The interval between the two snapshots used to compute deltas.
    pub dt: Duration,
    pub battery: Option<BatteryFrame>,
    pub soc: Option<SocPower>,
    pub thermal: Option<ThermalFrame>,
    pub gpu: Option<GpuFrame>,
    pub io: Option<IoFrame>,
    pub procs: Vec<ProcRow>,
}
