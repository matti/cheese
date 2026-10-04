//! Target-specific, read-only VM probes, inspired by memtop's open-file identity.
//! Never use the current Docker context: it may belong to a different VM/host.
use crate::{bounded_command, record::now};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{io, path::PathBuf, process::Command, time::Duration};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub tool: String,
    pub profile: Option<String>,
    pub lima_home: Option<PathBuf>,
    pub instance: Option<String>,
    pub docker_socket: Option<PathBuf>,
    pub evidence: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub project: Option<String>,
    pub service: Option<String>,
    pub cpu_percent: Option<f64>,
    pub memory: String,
    pub net_io: String,
    pub block_io: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestProcess {
    pub pid: u32,
    pub ppid: u32,
    pub start_ticks: u64,
    pub name: String,
    pub cpu_percent: f64,
    pub rss_bytes: u64,
    pub container_id: Option<String>,
    pub cgroup: Option<String>,
    pub probe: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Guest {
    pub interval_s: f64,
    pub logical_cpus: Option<u32>,
    pub busy_percent: Option<f64>,
    pub observed_processes: usize,
    pub measured_processes: usize,
    pub unreadable_cgroups: usize,
    pub processes: Vec<GuestProcess>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inspection {
    pub pid: i32,
    pub start_abstime: Option<u64>,
    pub started_at: f64,
    pub finished_at: f64,
    pub identity: Option<Identity>,
    pub containers: Option<Vec<Container>>,
    pub guest: Option<Guest>,
    pub errors: Vec<String>,
}

pub fn is_vm(name: &str, path: Option<&str>) -> bool {
    name.contains("VirtualMachine")
        || name.starts_with("qemu-system")
        || name == "vfkit"
        || path.is_some_and(|p| p.contains("Virtualization.framework"))
}

/// Only paths of actual open VM disk/firmware files qualify as identity evidence.
/// Seeing a directory shared with the guest alone does not identify its owner.
pub fn identify(files: &str) -> Option<Identity> {
    let mut candidates = Vec::new();
    for file in files.lines().filter_map(|l| l.strip_prefix('n')) {
        for (token, tool) in [("/.colima/_lima/", "colima"), ("/.lima/", "Lima")] {
            let Some((home, rest)) = file.split_once(token) else {
                continue;
            };
            let Some((instance, leaf)) = rest.split_once('/') else {
                continue;
            };
            if !["diffdisk", "basedisk", "vz-efi", "cidata.iso"].contains(&leaf)
                || instance.starts_with('_')
            {
                continue;
            }
            let (lima_home, profile, docker_socket) = if tool == "colima" {
                let profile = if instance == "colima" {
                    "default"
                } else {
                    instance.strip_prefix("colima-")?
                };
                (
                    PathBuf::from(format!("{home}/.colima/_lima")),
                    Some(profile.to_string()),
                    Some(PathBuf::from(format!(
                        "{home}/.colima/{profile}/docker.sock"
                    ))),
                )
            } else {
                (
                    PathBuf::from(format!("{home}/.lima")),
                    Some(instance.to_string()),
                    Some(PathBuf::from(format!(
                        "{home}/.lima/{instance}/sock/docker.sock"
                    ))),
                )
            };
            let identity = Identity {
                tool: tool.into(),
                profile,
                lima_home: Some(lima_home),
                instance: Some(instance.into()),
                docker_socket,
                evidence: file.into(),
            };
            if !candidates.iter().any(|c: &Identity| {
                c.tool == identity.tool
                    && c.lima_home == identity.lima_home
                    && c.instance == identity.instance
            }) {
                candidates.push(identity);
            }
        }
        if let Some((home, _)) = file.split_once("/Library/Containers/com.docker.docker/")
            && file.ends_with("/Docker.raw")
        {
            candidates.push(Identity {
                tool: "Docker Desktop".into(),
                profile: None,
                lima_home: None,
                instance: None,
                docker_socket: Some(PathBuf::from(format!("{home}/.docker/run/docker.sock"))),
                evidence: file.into(),
            });
        }
    }
    // Ambiguous evidence must not direct probes into an arbitrary VM.
    if candidates.len() == 1 {
        candidates.pop()
    } else {
        None
    }
}

fn execute(c: &mut Command, timeout: u64) -> io::Result<String> {
    bounded_command(c, Duration::from_secs(timeout))
}

fn open_files(pid: i32) -> io::Result<String> {
    // A busy VM can hold hundreds of thousands of shared guest files open.
    // Enumerating them can exceed the shorter Docker/guest sample deadline.
    // Keep the complete scan: truncating it could hide a second VM's disks.
    execute(
        Command::new("/usr/sbin/lsof").args(["-nP", "-a", "-p", &pid.to_string(), "-Fn"]),
        30,
    )
    .map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("VM identification: open-file lookup for PID {pid} (30s limit): {e}"),
        )
    })
}
fn docker(socket: &std::path::Path) -> Command {
    let mut c = Command::new("docker");
    c.env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH");
    c.arg("--host").arg(format!("unix://{}", socket.display()));
    c
}
fn percent(s: &str) -> Option<f64> {
    s.trim()
        .trim_end_matches('%')
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.)
}
fn lines(text: &str) -> io::Result<Vec<Value>> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).map_err(io::Error::other))
        .collect()
}
pub fn parse_containers(stats: &str, metadata: &str) -> io::Result<Vec<Container>> {
    let meta = lines(metadata)?;
    let mut containers = Vec::new();
    for row in lines(stats)? {
        let id = row["ID"]
            .as_str()
            .ok_or_else(|| io::Error::other("Docker stats missing ID"))?
            .to_string();
        let m = meta.iter().find(|m| m["id"].as_str() == Some(&id));
        let text = |field: &str| row[field].as_str().unwrap_or("unknown").to_string();
        let label = |field: &str| {
            m.and_then(|m| m[field].as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        containers.push(Container {
            id,
            name: text("Name"),
            image: label("image").unwrap_or_else(|| "unknown".into()),
            project: label("project"),
            service: label("service"),
            cpu_percent: percent(&text("CPUPerc")),
            memory: text("MemUsage"),
            net_io: text("NetIO"),
            block_io: text("BlockIO"),
        });
    }
    containers.sort_by(|a, b| {
        b.cpu_percent
            .unwrap_or(-1.)
            .total_cmp(&a.cpu_percent.unwrap_or(-1.))
    });
    Ok(containers)
}
fn containers(id: &Identity) -> io::Result<Vec<Container>> {
    let socket = id
        .docker_socket
        .as_ref()
        .filter(|p| p.exists())
        .ok_or_else(|| io::Error::other("No Docker socket for this VM; container usage unknown"))?;
    let metadata=execute(docker(socket).args(["ps","--no-trunc","--format",r#"{"id":{{json .ID}},"image":{{json .Image}},"project":{{json (.Label "com.docker.compose.project")}},"service":{{json (.Label "com.docker.compose.service")}}}"#]),5)?;
    let stats = execute(
        docker(socket).args([
            "stats",
            "--no-stream",
            "--no-trunc",
            "--format",
            "{{json .}}",
        ]),
        8,
    )?;
    parse_containers(&stats, &metadata)
}
fn guest(id: &Identity) -> io::Result<Guest> {
    let home=id.lima_home.as_ref().ok_or_else(||io::Error::other("Guest process access is supported for Colima/Lima; Docker containers can still be shown"))?;
    let instance = id
        .instance
        .as_ref()
        .ok_or_else(|| io::Error::other("VM instance unknown"))?;
    let output = execute(
        Command::new("limactl").env("LIMA_HOME", home).args([
            "shell",
            instance,
            "--",
            "python3",
            "-c",
            include_str!("../scripts/guest_sample.py"),
        ]),
        8,
    )?;
    serde_json::from_str(&output).map_err(io::Error::other)
}

pub fn inspect(pid: i32, expected_start: Option<u64>) -> Inspection {
    let mut result = Inspection {
        pid,
        start_abstime: expected_start,
        started_at: now(),
        finished_at: now(),
        identity: None,
        containers: None,
        guest: None,
        errors: Vec::new(),
    };
    let actual = crate::samplers::proc_rusage::process_start(pid);
    if expected_start.is_some() && actual != expected_start {
        result
            .errors
            .push("Process exited or PID was reused; select the current VM again".into());
        return result;
    }
    result.start_abstime = actual;
    let work = || -> io::Result<Identity> {
        let files = open_files(pid)?;
        identify(&files).ok_or_else(||io::Error::other("Could not uniquely identify this VM from its open disk files; no default Docker context was queried"))
    };
    match work() {
        Ok(identity) => {
            // Independent queries overlap in time. Neither runs on the recorder/UI thread.
            let (c, g) = std::thread::scope(|s| {
                let c = s.spawn(|| containers(&identity));
                let g = guest(&identity);
                (
                    c.join()
                        .unwrap_or_else(|_| Err(io::Error::other("Container probe panicked"))),
                    g,
                )
            });
            match c {
                Ok(c) => result.containers = Some(c),
                Err(e) => result.errors.push(format!("Containers: {e}")),
            }
            match g {
                Ok(g) => result.guest = Some(g),
                Err(e) => result.errors.push(format!("Guest: {e}")),
            }
            result.identity = Some(identity);
        }
        Err(e) => result.errors.push(e.to_string()),
    }
    if result.start_abstime.is_some()
        && crate::samplers::proc_rusage::process_start(pid) != result.start_abstime
    {
        result.containers = None;
        result.guest = None;
        result
            .errors
            .push("VM process changed while probing; results discarded".into());
    }
    result.finished_at = now();
    result
}
impl Inspection {
    pub fn owner(&self, p: &GuestProcess) -> String {
        match &p.container_id {
            Some(id) => self
                .containers
                .as_ref()
                .and_then(|cs| cs.iter().find(|c| c.id == *id))
                .map(|c| c.name.clone())
                .unwrap_or_else(|| format!("container {}", &id[..12.min(id.len())])),
            None if p.cgroup.is_some() => "VM / unmapped cgroup".into(),
            None => "unknown (cgroup unreadable)".into(),
        }
    }
    pub fn report(&self) -> String {
        let identity = self
            .identity
            .as_ref()
            .map(|i| format!("{} / {}", i.tool, i.profile.as_deref().unwrap_or("default")))
            .unwrap_or_else(|| "unknown VM".into());
        let mut out = format!("Host PID {} → {}\n", self.pid, identity);
        if let Some(i) = &self.identity {
            out += &format!("Evidence: {}\n", i.evidence);
        }
        out += "CPU percentages below are guest CPU load (100% = one logical CPU), not measured container watts.\n";
        if let Some(cs) = &self.containers {
            out += "\nContainers (Docker interval; separate from guest process interval):\n";
            for c in cs {
                out += &format!(
                    "{:>7}%  {}  RAM {}\n          image={}  compose={}/{}\n",
                    crate::analysis::number(c.cpu_percent),
                    c.name,
                    c.memory,
                    c.image,
                    c.project.as_deref().unwrap_or("-"),
                    c.service.as_deref().unwrap_or("-")
                );
            }
            if cs.is_empty() {
                out += "No running containers reported; VM/host services may still consume CPU.\n";
            }
        }
        if let Some(g) = &self.guest {
            out += &format!(
                "\nLinux processes over {:.2}s; {}/{} measured; {} unreadable cgroups; system busy {}% of {} vCPUs:\n",
                g.interval_s,
                g.measured_processes,
                g.observed_processes,
                g.unreadable_cgroups,
                crate::analysis::number(g.busy_percent),
                g.logical_cpus.unwrap_or(0)
            );
            for p in g.processes.iter().take(25) {
                out += &format!(
                    "{:>8} {:>7.1}%  {:24} {}{}\n",
                    p.pid,
                    p.cpu_percent,
                    p.name,
                    self.owner(p),
                    if p.probe { " [probe]" } else { "" }
                );
            }
        }
        for e in &self.errors {
            out += &format!("\nUnavailable: {e}\n");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_and_custom_colima_use_their_own_socket() {
        let default=identify("p1298\nn/Users/alice/.colima/_lima/colima/diffdisk\nn/Users/alice/.colima/_lima/colima/vz-efi").unwrap();
        assert_eq!(default.profile.as_deref(), Some("default"));
        assert_eq!(
            default.docker_socket.unwrap(),
            PathBuf::from("/Users/alice/.colima/default/docker.sock")
        );
        let other = identify("n/Users/alice/.colima/_lima/colima-work/diffdisk").unwrap();
        assert_eq!(other.profile.as_deref(), Some("work"));
        assert_eq!(
            other.docker_socket.unwrap(),
            PathBuf::from("/Users/alice/.colima/work/docker.sock")
        );
    }
    #[test]
    fn shared_directory_and_ambiguous_disks_do_not_select_a_vm() {
        assert!(identify("n/Users/alice/.colima/default\nn/Users/alice/shared").is_none());
        assert!(identify("n/Users/alice/.colima/_lima/colima/diffdisk\nn/Users/alice/.colima/_lima/colima-other/diffdisk").is_none());
        assert!(identify("permission denied").is_none());
    }
    #[test]
    fn docker_stats_preserve_unknowns_and_join_full_ids() {
        let rows = parse_containers(
            r#"{"ID":"aa","Name":"busy","CPUPerc":"195.6%","MemUsage":"500MiB / 8GiB"}
{"ID":"bb","Name":"unknown","CPUPerc":"--"}"#,
            r#"{"id":"aa","image":"worker:1","project":"prod","service":"indexer"}"#,
        )
        .unwrap();
        assert_eq!(rows[0].cpu_percent, Some(195.6));
        assert_eq!(rows[0].service.as_deref(), Some("indexer"));
        assert_eq!(rows[1].cpu_percent, None);
        assert!(parse_containers("bad json", "").is_err());
    }
    #[test]
    fn vm_inspection_survives_record_replay() {
        let inspection = Inspection {
            pid: 123,
            start_abstime: Some(42),
            started_at: 1.,
            finished_at: 4.,
            identity: None,
            containers: Some(vec![]),
            guest: None,
            errors: vec!["guest unavailable".into()],
        };
        let record = crate::record::Record::Vm {
            elapsed_s: 4.,
            inspection: Box::new(inspection),
        };
        let encoded = serde_json::to_string(&record).unwrap();
        let decoded = serde_json::from_str(&encoded).unwrap();
        let mut ledger = crate::analysis::Ledger::default();
        ledger.apply(&decoded);
        assert!(ledger.report().contains("Host PID 123"));
        assert!(ledger.report().contains("guest unavailable"));
    }
    #[test]
    fn timeout_also_bounds_inherited_pipes() {
        let start = std::time::Instant::now();
        let result = crate::bounded_command(
            Command::new("/bin/sh").args(["-c", "sleep 20 & exit 0"]),
            Duration::from_millis(150),
        );
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
