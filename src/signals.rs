//! System-level findings collected alongside the process samples, plus the
//! compact one-line summaries shown in the terminal and given to the assistant.
use crate::{
    origin::Attribution,
    system::{Load, Memory, Power},
};
use serde::Serialize;

#[derive(Serialize, Default)]
pub struct Signals {
    pub load: Option<Load>,
    pub memory: Option<Memory>,
    pub power: Option<Power>,
    #[serde(skip)]
    pub attribution: Attribution,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Level {
    Ok,
    Warn,
}

#[derive(Clone, Debug)]
pub struct Headline {
    pub label: &'static str,
    pub text: String,
    pub level: Level,
}

impl Headline {
    fn new(label: &'static str, text: String, warn: bool) -> Self {
        let level = if warn { Level::Warn } else { Level::Ok };
        Self { label, text, level }
    }
}

impl std::fmt::Display for Headline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.label, self.text)
    }
}

impl Signals {
    pub fn headlines(&self) -> Vec<Headline> {
        let mut lines = Vec::new();
        if let Some(l) = &self.load {
            let clusters = l
                .perf_levels
                .iter()
                .map(|p| format!("{} {}", p.logical_cpus, p.name))
                .collect::<Vec<_>>()
                .join(" + ");
            lines.push(Headline::new(
                "LOAD",
                format!(
                    "{:.0} on {} CPUs ({:.1}x, {}) · 5/15 min {:.0}/{:.0}{}",
                    l.one,
                    l.logical_cpus,
                    l.ratio,
                    l.verdict,
                    l.five,
                    l.fifteen,
                    if clusters.is_empty() {
                        String::new()
                    } else {
                        format!(" · {clusters}")
                    }
                ),
                l.ratio >= 1.,
            ));
        }
        if let Some(m) = &self.memory {
            let gb = |v: Option<f64>| v.map_or("?".into(), |v| format!("{v:.1}"));
            let mut text = format!(
                "{} · pressure {} · compressor {} G · swap {}/{} G",
                m.verdict,
                m.pressure,
                gb(m.compressor_gb),
                gb(m.swap_used_gb),
                gb(m.swap_total_gb)
            );
            if let (Some(out), Some(inn)) = (m.swapouts, m.swapins)
                && out + inn > 0
            {
                text += &format!(" · {out} out/{inn} in pages swapped");
            }
            lines.push(Headline::new(
                "MEMORY",
                text,
                !matches!(m.verdict, "normal" | "unknown"),
            ));
        }
        if let Some(p) = &self.power {
            let hot = p
                .thermal_pressure
                .as_deref()
                .is_some_and(|t| t != "NOMINAL");
            let thermal = p
                .thermal_pressure
                .as_deref()
                .filter(|_| hot)
                .map_or(String::new(), |t| format!(" · thermal {t}"));
            lines.push(Headline::new(
                "POWER",
                format!("{}{thermal}", p.note),
                hot || p.note.starts_with("on AC but"),
            ));
        }
        let groups = &self.attribution.groups;
        if !groups.is_empty() {
            let text = groups
                .iter()
                .take(3)
                .map(|g| format!("{} {}p {:.0}%", g.name, g.processes, g.cpu_percent))
                .collect::<Vec<_>>()
                .join(" · ");
            lines.push(Headline::new("ORIGIN", text, false));
        }
        let (busy, idle): (Vec<_>, Vec<_>) = self
            .attribution
            .detached
            .iter()
            .filter(|d| d.likely_orphan)
            .partition(|d| d.cpu_percent.is_some_and(|c| c >= 1.));
        let mut orphans: Vec<_> = busy
            .iter()
            .map(|d| {
                format!(
                    "{} pid {}{}{}",
                    d.name,
                    d.pid,
                    d.age.as_deref().map_or(String::new(), |a| format!(" {a}")),
                    d.cpu_percent.map_or(String::new(), |c| format!(" {c:.0}%"))
                )
            })
            .collect();
        if !idle.is_empty() {
            orphans.push(format!("{} idle", idle.len()));
        }
        if !orphans.is_empty() {
            lines.push(Headline::new(
                "ORPHANS",
                orphans.join(" · "),
                !busy.is_empty(),
            ));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        origin::{Detached, Group},
        system::PerfLevel,
    };

    #[test]
    fn headlines_flag_overload_pressure_and_orphans() {
        let signals = Signals {
            load: Some(Load {
                one: 136.,
                five: 120.,
                fifteen: 90.,
                logical_cpus: 18,
                perf_levels: vec![PerfLevel {
                    name: "Super".into(),
                    logical_cpus: 6,
                }],
                ratio: 136. / 18.,
                verdict: "severely oversubscribed",
            }),
            memory: Some(Memory {
                total_gb: Some(48.),
                pressure: "warning",
                compressor_gb: Some(16.),
                swap_used_gb: Some(2.2),
                swap_total_gb: Some(3.),
                window_s: 5.,
                pageouts: Some(0),
                swapins: Some(3),
                swapouts: Some(40),
                verdict: "warning",
            }),
            power: None,
            attribution: Attribution {
                groups: vec![Group {
                    root_pid: 5,
                    name: "playwright".into(),
                    command: String::new(),
                    launched_from: None,
                    processes: 17,
                    cpu_percent: 640.,
                    cpu_unknown: 0,
                    memory_mb: 0.,
                    members: String::new(),
                    age: None,
                    likely_orphan: false,
                }],
                detached: vec![Detached {
                    pid: 9,
                    name: "ls-sim".into(),
                    command: String::new(),
                    age: Some("1h15m".into()),
                    cpu_percent: Some(99.6),
                    memory_mb: None,
                    likely_orphan: true,
                }],
            },
        };
        let text: Vec<String> = signals.headlines().iter().map(|h| h.to_string()).collect();
        assert_eq!(
            text,
            [
                "LOAD: 136 on 18 CPUs (7.6x, severely oversubscribed) · 5/15 min 120/90 · 6 Super",
                "MEMORY: warning · pressure warning · compressor 16.0 G · swap 2.2/3.0 G · 40 out/3 in pages swapped",
                "ORIGIN: playwright 17p 640%",
                "ORPHANS: ls-sim pid 9 1h15m 100%",
            ]
        );
        assert!(
            signals
                .headlines()
                .iter()
                .all(|h| h.level == Level::Warn || h.label == "ORIGIN")
        );
    }
}
