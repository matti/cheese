//! Inspect measured consumers before asking either assistant to explain the cause.
use crate::{
    bounded_command,
    samplers::proc_rusage::process_start,
    types::{ProcRow, Sample},
    vm,
    wtf_ui::Progress,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{collections::HashMap, io, path::Path, process::Command, time::Duration};

#[derive(Clone, Serialize)]
pub struct Target {
    pid: i32,
    start_abstime: Option<u64>,
    name: String,
    executable: Option<String>,
    ppid: i32,
    mean_cpu_percent: f64,
    observed_intervals: usize,
    source: &'static str,
}

#[derive(Default)]
pub struct Consumers {
    latest: HashMap<i32, ProcRow>,
    cpu: HashMap<(i32, Option<u64>), (f64, f64, usize)>,
}

impl Consumers {
    pub fn observe(&mut self, sample: &Sample) {
        self.latest = sample.procs.iter().map(|p| (p.pid, p.clone())).collect();
        for p in &sample.procs {
            if let Some(cpu) = p.cpu_percent.filter(|c| c.is_finite() && *c >= 0.) {
                let entry = self.cpu.entry((p.pid, p.start_abstime)).or_default();
                entry.0 += cpu * sample.dt.as_secs_f64();
                entry.1 += sample.dt.as_secs_f64();
                entry.2 += 1;
            }
        }
    }

    pub fn targets(&self, top: &str) -> Vec<Target> {
        // top can read system processes whose native counters are unavailable.
        // Discard its first (non-interval) sample; never overwrite native data.
        let mut top_cpu: HashMap<i32, (f64, usize)> = HashMap::new();
        let mut interval = 0;
        for line in top.lines() {
            let mut fields = line.split_whitespace();
            let Some(pid) = fields.next() else { continue };
            if pid == "PID" {
                interval += 1;
                continue;
            }
            if interval < 2 {
                continue;
            }
            let Ok(pid) = pid.parse::<i32>() else {
                continue;
            };
            let Some(cpu) = fields.nth(1).and_then(|s| s.parse::<f64>().ok()) else {
                continue;
            };
            if cpu.is_finite() && cpu >= 0. {
                let entry = top_cpu.entry(pid).or_default();
                entry.0 += cpu;
                entry.1 += 1;
            }
        }
        let mut targets = Vec::new();
        for p in self.latest.values() {
            if p.pid == std::process::id() as i32 || p.pid <= 0 {
                continue;
            }
            let measured = self
                .cpu
                .get(&(p.pid, p.start_abstime))
                .filter(|(_, seconds, _)| *seconds > 0.);
            let (cpu, intervals, source) = if let Some((total, seconds, count)) = measured {
                (total / seconds, *count, "native interval CPU")
            } else if let Some((total, count)) = top_cpu.get(&p.pid) {
                (total / *count as f64, *count, "top interval CPU")
            } else {
                continue;
            };
            if cpu < 10. {
                continue;
            }
            targets.push(Target {
                pid: p.pid,
                start_abstime: p.start_abstime,
                name: p.name.clone(),
                executable: p.executable.clone(),
                ppid: p.ppid,
                mean_cpu_percent: cpu,
                observed_intervals: intervals,
                source,
            });
        }
        targets.sort_by(|a, b| {
            b.mean_cpu_percent
                .total_cmp(&a.mean_cpu_percent)
                .then(a.pid.cmp(&b.pid))
        });
        // Keep a busy VM in scope even when several short host jobs outrank it.
        let busy_vm = targets
            .iter()
            .find(|t| vm::is_vm(&t.name, t.executable.as_deref()))
            .cloned();
        targets.truncate(3);
        if let Some(vm) = busy_vm
            && !targets.iter().any(|t| t.pid == vm.pid)
        {
            if targets.len() == 3 {
                targets.pop();
            }
            targets.push(vm);
        }
        targets
    }
}

pub fn inspect(
    targets: Vec<Target>,
    evidence_path: &Path,
    ui: &mut Progress,
) -> io::Result<Vec<Value>> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    ui.status(
        2,
        &format!(
            "Inspecting {} busy processes and their workloads",
            targets.len()
        ),
    )?;
    std::thread::scope(|scope| {
        let workers: Vec<_> = targets
            .into_iter()
            .map(|target| scope.spawn(move || inspect_one(&target, evidence_path)))
            .collect();
        while workers.iter().any(|w| !w.is_finished()) {
            ui.wait(Duration::from_millis(80))?;
        }
        Ok(workers
            .into_iter()
            .map(|w| {
                w.join()
                    .unwrap_or_else(|_| json!({"unavailable": "Inspection worker panicked"}))
            })
            .collect())
    })
}

fn inspect_one(target: &Target, evidence: &Path) -> Value {
    let mut result = json!({"target": target});
    if !same_process(target) {
        result["unavailable"] = json!("Process exited or PID was reused before inspection");
        return result;
    }
    result["command_and_parent"] = match bounded_command(
        Command::new("/bin/ps").args([
            "-ww",
            "-p",
            &format!("{},{}", target.pid, target.ppid),
            "-o",
            "pid,ppid,etime,args",
        ]),
        Duration::from_secs(3),
    ) {
        Ok(s) => json!(s),
        Err(e) => json!({"unavailable": e.to_string()}),
    };
    if vm::is_vm(&target.name, target.executable.as_deref()) {
        let inspection = vm::inspect(target.pid, target.start_abstime);
        let path = evidence.with_extension(format!("vm-{}.json", target.pid));
        match serde_json::to_vec_pretty(&inspection)
            .map_err(io::Error::other)
            .and_then(|bytes| std::fs::write(&path, bytes))
        {
            Ok(()) => result["detail_file"] = json!(path),
            Err(e) => result["save_error"] = json!(e.to_string()),
        }
        // Includes container names/images/Compose services and guest CPU mapped
        // by full cgroup ID. The complete raw probe remains available on disk.
        result["vm"] = json!(inspection.report());
    } else {
        let path = evidence.with_extension(format!("stack-{}.txt", target.pid));
        let capture = bounded_command(
            Command::new("/usr/bin/sample")
                .arg(target.pid.to_string())
                .args(["1", "-file"])
                .arg(&path),
            Duration::from_secs(6),
        )
        .and_then(|_| std::fs::read_to_string(&path));
        match capture {
            Ok(stack) => {
                result["stack"] = json!(stack_excerpt(&stack));
                result["detail_file"] = json!(path);
            }
            Err(e) => result["unavailable"] = json!(e.to_string()),
        }
    }
    if !same_process(target) {
        return json!({"target": target, "unavailable": "Process exited or PID was reused during inspection; results discarded"});
    }
    result
}

fn same_process(target: &Target) -> bool {
    if let Some(expected) = target.start_abstime {
        process_start(target.pid) == Some(expected)
    } else {
        // Cross-user start counters can be inaccessible; verify the executable
        // as a best-effort identity check rather than inventing a start time.
        target.executable.is_some()
            && crate::samplers::procname::pidpath(target.pid) == target.executable
    }
}

fn stack_excerpt(stack: &str) -> String {
    let graph = stack.split("Binary Images:").next().unwrap_or(stack);
    let summary = graph.find("Sort by top of stack").map(|pos| &graph[pos..]);
    let mut excerpt = graph.lines().take(180).collect::<Vec<_>>().join("\n");
    if graph.lines().count() > 180 {
        excerpt.push_str("\n[Call graph excerpt; full report saved separately]\n");
        if let Some(summary) = summary {
            excerpt.push_str(&summary.lines().take(100).collect::<Vec<_>>().join("\n"));
        }
    }
    excerpt
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(pid: i32, name: &str, cpu: Option<f64>) -> ProcRow {
        serde_json::from_value(json!({"pid":pid,"ppid":1,"uid":501,"same_uid":true,"name":name,"start_abstime":pid as u64,"cpu_percent":cpu})).unwrap()
    }
    fn sample(procs: Vec<ProcRow>, seconds: u64) -> Sample {
        Sample {
            dt: Duration::from_secs(seconds),
            procs,
            battery: None,
            soc: None,
            thermal: None,
            gpu: None,
            io: None,
        }
    }
    #[test]
    fn targets_use_weighted_load_and_keep_a_busy_vm() {
        let mut c = Consumers::default();
        c.observe(&sample(
            vec![
                row(1, "spike", Some(400.)),
                row(2, "busy", Some(100.)),
                row(3, "VirtualMachine", Some(40.)),
                row(4, "other", Some(90.)),
                row(std::process::id() as i32, "battery", Some(999.)),
            ],
            1,
        ));
        c.observe(&sample(
            vec![
                row(1, "spike", Some(0.)),
                row(2, "busy", Some(100.)),
                row(3, "VirtualMachine", Some(40.)),
                row(4, "other", Some(90.)),
                row(std::process::id() as i32, "battery", Some(999.)),
            ],
            4,
        ));
        let targets = c.targets("");
        assert_eq!(
            targets.iter().map(|t| t.pid).collect::<Vec<_>>(),
            vec![2, 4, 3]
        );
        assert_eq!(targets[0].mean_cpu_percent, 100.);
    }
    #[test]
    fn top_skips_baseline_and_only_fills_missing_native_counters() {
        let mut c = Consumers::default();
        c.observe(&sample(
            vec![row(1, "system", None), row(2, "native", Some(20.))],
            2,
        ));
        let targets=c.targets("PID COMMAND %CPU\n1 system 900\n2 native 900\nPID COMMAND %CPU\n1 system 80\n2 native 800\nPID COMMAND %CPU\n1 system 100");
        assert_eq!(targets[0].mean_cpu_percent, 90.);
        assert_eq!(targets[1].mean_cpu_percent, 20.);
    }
    #[test]
    fn old_pid_energy_is_not_used_for_new_process() {
        let mut c = Consumers::default();
        c.observe(&sample(vec![row(1, "old", Some(800.))], 2));
        let mut new = row(1, "new", Some(0.));
        new.start_abstime = Some(999);
        c.observe(&sample(vec![new], 2));
        assert!(c.targets("").is_empty());
    }
    #[test]
    fn stack_excerpt_keeps_leaf_summary_but_omits_binary_inventory() {
        let text = format!(
            "{}\nSort by top of stack\n hot_function\nBinary Images:\nignored",
            "frame\n".repeat(200)
        );
        let excerpt = stack_excerpt(&text);
        assert!(excerpt.contains("hot_function"));
        assert!(!excerpt.contains("ignored"));
    }
}
