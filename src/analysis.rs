use crate::{
    record::Record,
    types::{ProcRow, Sample},
};
use std::collections::{HashMap, VecDeque};

#[derive(Default, Clone)]
pub struct Consumer {
    pub name: String,
    pub cpu_j: f64,
    pub cpu_s: f64,
    pub wakeups: f64,
    pub disk_bytes: f64,
    pub samples: u64,
    pub energy_samples: u64,
}
#[derive(Clone)]
pub struct Point {
    pub t: f64,
    pub dt: f64,
    pub w: Option<f64>,
}
#[derive(Default, Clone)]
pub struct Window {
    pub joules: f64,
    pub seconds: f64,
}
impl Window {
    pub fn mean(&self) -> Option<f64> {
        (self.seconds > 0.).then(|| self.joules / self.seconds)
    }
    fn add(&mut self, p: &Point, from: f64, to: f64) {
        let overlap = (p.t.min(to) - (p.t - p.dt).max(from)).max(0.);
        if let Some(w) = p.w {
            self.joules += w * overlap;
            self.seconds += overlap;
        }
    }
}
#[derive(Clone)]
pub struct Comparison {
    pub t: f64,
    pub label: String,
    pub before: Window,
    pub after: Window,
}
impl Comparison {
    pub fn ready(&self) -> bool {
        self.before.seconds >= 59. && self.after.seconds >= 59.
    }
    pub fn description(&self) -> String {
        if self.ready() {
            let a = self.before.mean().unwrap();
            let b = self.after.mean().unwrap();
            format!(
                "{}: {:.1} → {:.1} W ({:+.1} W). Association; repeat to confirm.",
                self.label,
                a,
                b,
                b - a
            )
        } else {
            format!(
                "{}: battery coverage before {:.0}/60 s, after {:.0}/60 s (15 s settling).",
                self.label,
                self.before.seconds.min(60.),
                self.after.seconds.min(60.)
            )
        }
    }
}

#[derive(Default)]
pub struct Ledger {
    pub elapsed: f64,
    pub battery_j: f64,
    pub battery_seconds: f64,
    pub gaps: u64,
    pub peak_w: f64,
    pub peak_at: f64,
    pub peak_consumers: String,
    pub groups: HashMap<String, Consumer>,
    pub history: VecDeque<Point>,
    pub system_history: VecDeque<(f64, f64)>,
    pub system_energy: Window,
    pub comparisons: Vec<Comparison>,
    pub latest: Option<Sample>,
    pub assertions: String,
    pub notices: Vec<String>,
    pub sampler_ms: f64,
    pub measured_procs: usize,
    pub total_procs: usize,
    pub recorder_pid: u32,
    pub samples: u64,
    pub ended: bool,
    pub vms: HashMap<i32, crate::vm::Inspection>,
}

pub fn group(p: &ProcRow) -> String {
    p.executable
        .as_deref()
        .and_then(crate::samplers::procname::app_bundle)
        .map(|app| {
            let name = app.rsplit('/').next().unwrap_or(app);
            name.strip_suffix(".app").unwrap_or(name).to_string()
        })
        .unwrap_or_else(|| p.name.clone())
}

pub fn discharge(sample: &Sample) -> Option<f64> {
    let b = sample.battery.as_ref()?;
    if b.external_connected || b.is_charging || b.system_power_mw > 0. || b.voltage_mv <= 0. {
        return None;
    }
    let w = -b.system_power_mw / 1000.;
    w.is_finite().then_some(w)
}

impl Ledger {
    pub fn apply(&mut self, record: &Record) {
        match record {
            Record::Session { recorder_pid, .. } => self.recorder_pid = *recorder_pid,
            Record::Notice { message, .. } => self.notices.push(message.clone()),
            Record::Context { assertions, .. } => self.assertions = assertions.clone(),
            Record::End { .. } => self.ended = true,
            Record::Vm { inspection, .. } => {
                self.vms.insert(inspection.pid, (**inspection).clone());
            }
            Record::Marker {
                elapsed_s, label, ..
            } => {
                let mut before = Window::default();
                for p in &self.history {
                    before.add(p, elapsed_s - 60., *elapsed_s);
                }
                // A new intervention invalidates an unfinished previous after-window.
                self.comparisons.push(Comparison {
                    t: *elapsed_s,
                    label: label.clone(),
                    before,
                    after: Window::default(),
                });
            }
            Record::Sample {
                elapsed_s,
                gap,
                sample,
                sampler_ms,
                ..
            } => {
                self.samples += 1;
                self.elapsed = *elapsed_s;
                self.sampler_ms = *sampler_ms;
                let dt = sample.dt.as_secs_f64();
                let previous_battery = self.latest.as_ref().and_then(discharge);
                let current_battery = discharge(sample);
                let w = if *gap {
                    None
                } else {
                    previous_battery
                        .zip(current_battery)
                        .map(|(a, b)| (a + b) / 2.)
                };
                if !gap {
                    let smc = |s: &Sample| {
                        s.thermal
                            .as_ref()
                            .and_then(|t| t.pstr_w)
                            .filter(|w| w.is_finite() && *w >= 0.)
                    };
                    if let Some(w) = smc(sample) {
                        self.system_history.push_back((*elapsed_s, w));
                        if let Some(prev) = self.latest.as_ref().and_then(smc) {
                            self.system_energy.joules += (prev + w) / 2. * dt;
                            self.system_energy.seconds += dt;
                        }
                    }
                }
                while self
                    .system_history
                    .front()
                    .is_some_and(|p| p.0 < self.elapsed - 300.)
                {
                    self.system_history.pop_front();
                }
                let point = Point {
                    t: *elapsed_s,
                    dt,
                    w,
                };
                if *gap {
                    self.gaps += 1;
                }
                if let Some(w) = w {
                    self.battery_j += w * dt;
                    self.battery_seconds += dt;
                    if w > self.peak_w {
                        self.peak_w = w;
                        self.peak_at = *elapsed_s;
                        let mut rows: Vec<_> = sample
                            .procs
                            .iter()
                            .filter(|p| p.energy_mw.is_some())
                            .collect();
                        rows.sort_by(|a, b| {
                            b.energy_mw
                                .unwrap_or(0.)
                                .total_cmp(&a.energy_mw.unwrap_or(0.))
                        });
                        self.peak_consumers = rows
                            .iter()
                            .take(3)
                            .map(|p| {
                                format!("{} {:.2} CPU W", p.name, p.energy_mw.unwrap_or(0.) / 1000.)
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                    }
                }
                for c in &mut self.comparisons {
                    // Account for a sample straddling a marker entered between ticks.
                    if point.t > c.t {
                        c.before.add(&point, c.t - 60., c.t);
                    }
                }
                if let Some(c) = self.comparisons.last_mut() {
                    c.after.add(&point, c.t + 15., c.t + 75.);
                }
                self.history.push_back(point);
                while self
                    .history
                    .front()
                    .is_some_and(|p| p.t < self.elapsed - 300.)
                {
                    self.history.pop_front();
                }
                self.total_procs = sample.procs.len();
                self.measured_procs = sample
                    .procs
                    .iter()
                    .filter(|p| p.energy_mw.is_some())
                    .count();
                if !gap {
                    for p in &sample.procs {
                        if p.cpu_percent.is_none() && p.energy_mw.is_none() {
                            continue;
                        }
                        let name = group(p);
                        let c = self.groups.entry(name.clone()).or_insert_with(|| Consumer {
                            name,
                            ..Default::default()
                        });
                        if let Some(w) = p.energy_mw {
                            c.cpu_j += w / 1000. * dt;
                            c.energy_samples += 1;
                        }
                        c.cpu_s += p.cpu_percent.unwrap_or(0.) / 100. * dt;
                        c.wakeups += p.wakeups_per_s.unwrap_or(0.) * dt;
                        c.disk_bytes +=
                            (p.disk_read_bps.unwrap_or(0.) + p.disk_write_bps.unwrap_or(0.)) * dt;
                        c.samples += 1;
                    }
                }
                self.latest = Some(*sample.clone());
            }
        }
    }
    pub fn ranked(&self) -> Vec<&Consumer> {
        let mut rows: Vec<_> = self.groups.values().collect();
        rows.sort_by(|a, b| {
            b.cpu_j
                .total_cmp(&a.cpu_j)
                .then(b.cpu_s.total_cmp(&a.cpu_s))
        });
        rows
    }
    pub fn rolling_w(&self) -> Option<f64> {
        let mut w = Window::default();
        for p in &self.history {
            w.add(p, self.elapsed - 60., self.elapsed);
        }
        w.mean()
    }
    pub fn report(&self) -> String {
        let mut out = format!(
            "Recorded {:.0}s, {} samples. {}\nBattery: {:.3} Wh / {:.0}s observed on battery, mean {} W, {} gaps excluded.\n",
            self.elapsed,
            self.samples,
            if self.ended {
                "Clean stop."
            } else {
                "Live or interrupted recording."
            },
            self.battery_j / 3600.,
            self.battery_seconds,
            number((self.battery_seconds > 0.).then(|| self.battery_j / self.battery_seconds)),
            self.gaps
        );
        out += &format!(
            "SMC system power: mean {} W over {:.0}s (also measured on AC).\n",
            number(self.system_energy.mean()),
            self.system_energy.seconds
        );
        if self.battery_seconds > 0. {
            out += &format!(
                "Peak interval: {:.1} W at +{:.0}s. Concurrent CPU users: {}\n",
                self.peak_w, self.peak_at, self.peak_consumers
            );
        }
        out += "\nCPU energy during recording (not whole-app battery energy):\n";
        for c in self.ranked().iter().take(20) {
            out += &format!(
                "{:30} {:>9} CPU Wh  {:8.1} CPU s  {:9.0} wakeups\n",
                c.name,
                if c.energy_samples > 0 {
                    format!("{:.5}", c.cpu_j / 3600.)
                } else {
                    "-".into()
                },
                c.cpu_s,
                c.wakeups
            );
        }
        out += &format!(
            "\nLatest CPU energy coverage: {}/{} processes. Missing counters are unknown.\n",
            self.measured_procs, self.total_procs
        );
        for c in &self.comparisons {
            out += &format!("\nChange +{:.0}s: {}\n", c.t, c.description());
        }
        if !self.assertions.is_empty() {
            out += &format!("\nLatest sleep assertions:\n{}\n", self.assertions);
        }
        for vm in self.vms.values() {
            out += &format!(
                "\nLatest VM inspection (Unix {:.0}):\n{}",
                vm.finished_at,
                vm.report()
            );
        }
        for n in &self.notices {
            out += &format!("Notice: {n}\n");
        }
        out
    }
}
pub fn number(n: Option<f64>) -> String {
    n.map(|n| format!("{n:.1}")).unwrap_or_else(|| "-".into())
}
