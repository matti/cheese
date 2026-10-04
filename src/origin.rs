//! Attribute processes to the command a person or agent launched, and flag
//! likely orphans, so "16 chrome processes" reads as "1 playwright test run".
//!
//! Heuristic: from each process, climb the parent chain and stop below the
//! first *boundary* parent. The last process below it is the group's root.
//! A parent is a boundary when it is
//! - launchd/kernel (pid <= 1) or missing from the process table;
//! - owned by a different user (root `login`, `sudo`, system daemons);
//! - a session process: `login`, `sshd`, `tmux`, `screen`, `mosh-server`, `sudo`, `su`;
//! - an interactive or `-c` shell (a terminal prompt or an agent's command
//!   wrapper). A shell running a script file is part of the command instead;
//! - an `.app` bundle launching an executable outside that bundle (Claude.app
//!   starting a CLI). Helpers inside the same bundle stay with their app.
//!
//! Interpreters, helpers and workers (node, python, chrome-headless-shell,
//! ngspice) are therefore absorbed into the command that started them.
use crate::{
    samplers::{proc_rusage, procname},
    types::ProcRow,
};
use serde::Serialize;
use std::collections::HashMap;

const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "nu", "elvish", "xonsh", "pwsh",
];
const SESSIONS: &[&str] = &[
    "launchd",
    "login",
    "sshd",
    "sshd-session",
    "tmux",
    "screen",
    "mosh-server",
    "sudo",
    "su",
];
const INTERPRETERS: &[&str] = &["node", "bun", "deno", "ruby", "perl", "php"];
const SYSTEM_DIRS: &[&str] = &[
    "/System/",
    "/usr/libexec/",
    "/usr/sbin/",
    "/usr/lib/",
    "/sbin/",
    "/Library/Apple/",
];
const MAX_DEPTH: usize = 64;
const COMMAND_CHARS: usize = 200;

/// Lookups that need the live system; replaced by fixtures in tests.
pub trait Host {
    fn args(&self, pid: i32) -> Option<Vec<String>>;
    fn session(&self, pid: i32) -> Option<i32>;
    fn age_s(&self, pid: i32) -> Option<f64>;
}

pub struct Native;
impl Host for Native {
    fn args(&self, pid: i32) -> Option<Vec<String>> {
        proc_rusage::process_args(pid)
    }
    fn session(&self, pid: i32) -> Option<i32> {
        // SAFETY: getsid has no memory-safety preconditions.
        let sid = unsafe { libc::getsid(pid) };
        (sid >= 0).then_some(sid)
    }
    fn age_s(&self, pid: i32) -> Option<f64> {
        proc_rusage::started_at(pid).map(|start| (crate::record::now() - start).max(0.))
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Group {
    pub root_pid: i32,
    pub name: String,
    /// Root command line (truncated); executable path when arguments are unreadable.
    pub command: String,
    /// Nearest non-shell ancestor above the root: a terminal app, agent or launchd.
    pub launched_from: Option<String>,
    pub processes: usize,
    /// Sum of members' mean CPU over the window (100 = one core).
    pub cpu_percent: f64,
    /// Members without a CPU reading (other users' processes outside top's list).
    pub cpu_unknown: usize,
    /// Sum of readable members' physical memory footprints.
    pub memory_mb: f64,
    pub members: String,
    pub age: Option<String>,
    pub likely_orphan: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct Detached {
    pub pid: i32,
    pub name: String,
    pub command: String,
    pub age: Option<String>,
    pub cpu_percent: Option<f64>,
    pub memory_mb: Option<f64>,
    /// The terminal/agent session that started it is gone or detached
    /// (rather than a launchd-managed service).
    pub likely_orphan: bool,
}

#[derive(Serialize, Default, Debug)]
pub struct Attribution {
    pub groups: Vec<Group>,
    pub detached: Vec<Detached>,
}

struct Tree<'a> {
    rows: HashMap<i32, &'a ProcRow>,
    host: &'a dyn Host,
    argv: HashMap<i32, Option<Vec<String>>>,
}

impl<'a> Tree<'a> {
    fn new(procs: &'a [ProcRow], host: &'a dyn Host) -> Self {
        Self {
            rows: procs.iter().map(|p| (p.pid, p)).collect(),
            host,
            argv: HashMap::new(),
        }
    }

    fn argv(&mut self, pid: i32) -> Option<&[String]> {
        let host = self.host;
        self.argv
            .entry(pid)
            .or_insert_with(|| host.args(pid))
            .as_deref()
    }

    fn is_shell_boundary(&mut self, p: &ProcRow) -> bool {
        // Unreadable arguments: assume an interactive shell.
        is_shell(&p.name) && !self.argv(p.pid).is_some_and(shell_runs_script)
    }

    fn is_boundary(&mut self, parent: Option<&'a ProcRow>, child: &ProcRow) -> bool {
        let Some(parent) = parent.filter(|p| p.pid > 1) else {
            return true;
        };
        if parent.uid != child.uid || SESSIONS.contains(&bare(&parent.name)) {
            return true;
        }
        let parent_app = parent.executable.as_deref().and_then(gui_app);
        if parent_app.is_some() && parent_app != child.executable.as_deref().and_then(gui_app) {
            return true;
        }
        self.is_shell_boundary(parent)
    }

    fn root(&mut self, pid: i32) -> i32 {
        let mut current = pid;
        for _ in 0..MAX_DEPTH {
            let Some(child) = self.rows.get(&current).copied() else {
                break;
            };
            let parent = self.rows.get(&child.ppid).copied();
            if self.is_boundary(parent, child) {
                break;
            }
            current = child.ppid;
        }
        current
    }

    /// Climb past shells and session processes to whoever started the root.
    fn launched_from(&mut self, root: i32) -> Option<String> {
        let mut current = self.rows.get(&root)?.ppid;
        for _ in 0..MAX_DEPTH {
            if current <= 1 {
                return Some("launchd".into());
            }
            let p = self.rows.get(&current).copied()?;
            if !is_shell(&p.name) && !SESSIONS.contains(&bare(&p.name)) {
                return Some(
                    p.executable
                        .as_deref()
                        .and_then(gui_app)
                        .and_then(|app| app.rsplit('/').next())
                        .map_or_else(|| p.name.clone(), |app| app.trim_end_matches(".app").into()),
                );
            }
            current = p.ppid;
        }
        None
    }

    fn describe(&mut self, p: &ProcRow) -> (String, String) {
        match self.argv(p.pid) {
            Some(argv) if !argv.is_empty() => (
                display_name(&p.name, argv),
                truncate(&argv.join(" "), COMMAND_CHARS),
            ),
            _ => (
                p.name.clone(),
                truncate(p.executable.as_deref().unwrap_or(&p.name), COMMAND_CHARS),
            ),
        }
    }
}

/// Group the processes of one sample by origin and list detached processes.
/// `cpu` maps pid to mean CPU over the window where measurable.
pub fn attribute(procs: &[ProcRow], cpu: &HashMap<i32, f64>, host: &dyn Host) -> Attribution {
    // SAFETY: getuid has no preconditions.
    let me = unsafe { libc::getuid() };
    attribute_for(procs, cpu, host, me, std::process::id() as i32)
}

fn attribute_for(
    procs: &[ProcRow],
    cpu: &HashMap<i32, f64>,
    host: &dyn Host,
    me: u32,
    self_pid: i32,
) -> Attribution {
    let mut tree = Tree::new(procs, host);
    let detached = detached(&mut tree, procs, cpu, me, self_pid);
    let mut members: HashMap<i32, Vec<&ProcRow>> = HashMap::new();
    for p in procs {
        members.entry(tree.root(p.pid)).or_default().push(p);
    }
    members.remove(&self_pid);
    let mut groups: Vec<_> = members
        .into_iter()
        .filter_map(|(root, rows)| {
            let root_row = tree.rows.get(&root).copied()?;
            let (mut name, command) = tree.describe(root_row);
            if crate::vm::is_vm(&root_row.name, root_row.executable.as_deref()) {
                // The deep inspection names the VM (Colima, Lima) and its containers.
                name = "VM".into();
            }
            let mut counts: HashMap<&str, usize> = HashMap::new();
            for r in &rows {
                *counts.entry(&r.name).or_default() += 1;
            }
            Some(Group {
                root_pid: root,
                name,
                command,
                launched_from: tree.launched_from(root),
                processes: rows.len(),
                cpu_percent: rows.iter().filter_map(|r| cpu.get(&r.pid)).sum(),
                cpu_unknown: rows.iter().filter(|r| !cpu.contains_key(&r.pid)).count(),
                memory_mb: rows.iter().filter_map(|r| r.footprint_bytes).sum::<u64>() as f64
                    / 1048576.,
                members: summarize_counts(counts),
                age: host.age_s(root).map(format_age),
                likely_orphan: detached.iter().any(|d| d.pid == root && d.likely_orphan),
            })
        })
        .collect();
    Attribution {
        groups: select_groups(&mut groups),
        detached,
    }
}

/// Busiest origins by CPU, plus the largest by memory if not already listed.
fn select_groups(groups: &mut [Group]) -> Vec<Group> {
    groups.sort_by(|a, b| {
        b.cpu_percent
            .total_cmp(&a.cpu_percent)
            .then(b.processes.cmp(&a.processes))
            .then(a.root_pid.cmp(&b.root_pid))
    });
    let mut chosen: Vec<Group> = groups
        .iter()
        .filter(|g| g.cpu_percent >= 2.)
        .take(8)
        .cloned()
        .collect();
    let mut by_memory: Vec<&Group> = groups
        .iter()
        .filter(|g| g.memory_mb >= 1024. && !chosen.iter().any(|c| c.root_pid == g.root_pid))
        .collect();
    by_memory.sort_by(|a, b| b.memory_mb.total_cmp(&a.memory_mb));
    chosen.extend(by_memory.into_iter().take(3).cloned());
    chosen
}

/// User-owned processes reparented to launchd: likely orphans when their
/// original session is gone, otherwise listed only when busy (it may be a
/// launchd-managed service). App bundles and system daemons are skipped
/// unless their session shows they were started from a terminal.
fn detached(
    tree: &mut Tree,
    procs: &[ProcRow],
    cpu: &HashMap<i32, f64>,
    me: u32,
    self_pid: i32,
) -> Vec<Detached> {
    let mut found: Vec<Detached> = procs
        .iter()
        .filter(|p| p.uid == me && p.ppid == 1 && p.pid != self_pid)
        .filter(|p| {
            !p.executable
                .as_deref()
                .is_some_and(|e| SYSTEM_DIRS.iter().any(|d| e.starts_with(d)))
        })
        .filter_map(|p| {
            let likely_orphan = tree
                .host
                .session(p.pid)
                .is_some_and(|sid| sid > 1 && sid != p.pid);
            let busy = cpu.get(&p.pid).is_some_and(|c| *c >= 5.);
            let bundled = p.executable.as_deref().is_some_and(is_bundled);
            let listed = likely_orphan || (busy && !bundled);
            if !listed {
                return None;
            }
            let (name, command) = tree.describe(p);
            Some(Detached {
                pid: p.pid,
                name,
                command,
                age: tree.host.age_s(p.pid).map(format_age),
                cpu_percent: cpu.get(&p.pid).copied(),
                memory_mb: p.footprint_bytes.map(|b| b as f64 / 1048576.),
                likely_orphan,
            })
        })
        .collect();
    found.sort_by(|a, b| {
        b.likely_orphan.cmp(&a.likely_orphan).then(
            b.cpu_percent
                .unwrap_or(0.)
                .total_cmp(&a.cpu_percent.unwrap_or(0.)),
        )
    });
    found.truncate(8);
    found
}

fn bare(name: &str) -> &str {
    name.trim_start_matches('-')
}

fn is_shell(name: &str) -> bool {
    SHELLS.contains(&bare(name))
}

/// The `.app` bundle of a GUI application. Bundles inside frameworks (Homebrew's
/// `Python.framework/.../Python.app`) are interpreters, not applications.
fn gui_app(path: &str) -> Option<&str> {
    procname::app_bundle(path).filter(|app| !app.contains(".framework/"))
}

fn is_bundled(path: &str) -> bool {
    [".app/", ".appex/", ".xpc/", ".framework/"]
        .iter()
        .any(|b| path.contains(b))
}

/// True when a shell's argv names a script file (rather than `-c` or a prompt).
pub fn shell_runs_script(argv: &[String]) -> bool {
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            return args.next().is_some();
        }
        if let Some(flags) = arg.strip_prefix('-') {
            if !flags.starts_with('-') && flags.contains('c') {
                return false;
            }
            continue;
        }
        if arg.starts_with('+') {
            continue;
        }
        return true;
    }
    false
}

/// Name a script by its file rather than its interpreter: `node .../playwright
/// test` is "playwright", `python3 -m pytest` is "pytest". Otherwise use the
/// invoked name (argv[0]), which survives multi-call binaries and retitling.
pub fn display_name(name: &str, argv: &[String]) -> String {
    let lower = name.to_ascii_lowercase();
    let interpreted = INTERPRETERS.contains(&lower.as_str())
        || lower.starts_with("python")
        || (is_shell(name) && shell_runs_script(argv));
    if interpreted && let Some(script) = script_name(argv) {
        return script;
    }
    match argv.first().map(|a| a.trim()) {
        Some(a0) if !a0.is_empty() && !a0.contains(' ') => {
            procname::meaningful_basename(a0).unwrap_or_else(|| a0.into())
        }
        // Retitled: "npm exec vite --port 5180" is reported as one argument.
        Some(a0) if argv.len() == 1 && !a0.starts_with('/') => {
            a0.split_whitespace().next().unwrap_or(name).to_string()
        }
        _ => name.to_string(),
    }
}

fn script_name(argv: &[String]) -> Option<String> {
    let interpreter = argv
        .first()
        .and_then(|a| procname::meaningful_basename(a))?;
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-m" => return args.next().cloned(),
            "-c" | "-e" | "--eval" | "-p" | "--print" => return None,
            a if a.starts_with('-') => continue,
            "run" | "exec" | "x" if interpreter == "bun" || interpreter == "deno" => continue,
            script => {
                let base = procname::meaningful_basename(script)?;
                let stem = [
                    ".js", ".mjs", ".cjs", ".ts", ".py", ".rb", ".pl", ".php", ".sh", ".zsh",
                ]
                .iter()
                .find_map(|ext| base.strip_suffix(ext))
                .unwrap_or(&base);
                return Some(stem.to_string());
            }
        }
    }
    None
}

fn summarize_counts(counts: HashMap<&str, usize>) -> String {
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let mut parts: Vec<String> = counts
        .iter()
        .take(4)
        .map(|(name, n)| format!("{name} x{n}"))
        .collect();
    if counts.len() > 4 {
        parts.push(format!("+{} more kinds", counts.len() - 4));
    }
    parts.join(", ")
}

pub fn format_age(seconds: f64) -> String {
    let s = seconds.max(0.) as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d{:02}h", s / 86400, s % 86400 / 3600),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(3)).collect();
    cut.push_str("...");
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fake {
        args: HashMap<i32, Vec<&'static str>>,
        sessions: HashMap<i32, i32>,
    }
    impl Host for Fake {
        fn args(&self, pid: i32) -> Option<Vec<String>> {
            self.args
                .get(&pid)
                .map(|a| a.iter().map(|s| s.to_string()).collect())
        }
        fn session(&self, pid: i32) -> Option<i32> {
            self.sessions.get(&pid).copied()
        }
        fn age_s(&self, _: i32) -> Option<f64> {
            Some(4500.)
        }
    }

    fn row(pid: i32, ppid: i32, uid: u32, name: &str, exe: &str) -> ProcRow {
        serde_json::from_value(json!({"pid":pid,"ppid":ppid,"uid":uid,"same_uid":uid==501,
            "name":name,"executable":exe,"start_abstime":null,"footprint_bytes":104857600}))
        .unwrap()
    }

    /// iTerm2 -> login -> zsh -> claude -> zsh -c -> node playwright -> 3 chrome;
    /// zsh -> python pcb sim -> 2 ngspice; Chrome.app with a helper; an orphan.
    fn fixture() -> (Vec<ProcRow>, Fake) {
        let procs = vec![
            row(
                10,
                1,
                501,
                "iTerm2",
                "/Applications/iTerm.app/Contents/MacOS/iTerm2",
            ),
            row(11, 10, 0, "login", "/usr/bin/login"),
            row(12, 11, 501, "-zsh", "/bin/zsh"),
            row(13, 12, 501, "claude", "/Users/u/.local/bin/claude"),
            row(14, 13, 501, "zsh", "/bin/zsh"),
            row(15, 14, 501, "node", "/opt/homebrew/bin/node"),
            row(
                16,
                15,
                501,
                "chrome-headless-shell",
                "/c/chrome-headless-shell",
            ),
            row(
                17,
                15,
                501,
                "chrome-headless-shell",
                "/c/chrome-headless-shell",
            ),
            row(
                18,
                15,
                501,
                "chrome-headless-shell",
                "/c/chrome-headless-shell",
            ),
            row(
                20,
                12,
                501,
                "Python",
                "/opt/homebrew/Frameworks/Python.framework/Versions/3.14/Resources/Python.app/Contents/MacOS/Python",
            ),
            row(21, 20, 501, "ngspice", "/opt/homebrew/bin/ngspice"),
            row(22, 20, 501, "ngspice", "/opt/homebrew/bin/ngspice"),
            row(
                30,
                1,
                501,
                "Google Chrome",
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            ),
            row(
                31,
                30,
                501,
                "Google Chrome Helper",
                "/Applications/Google Chrome.app/Contents/Frameworks/H.app/Contents/MacOS/Google Chrome Helper",
            ),
            row(40, 1, 501, "ls-sim", "/Users/u/dev/target/release/ls-sim"),
            row(41, 1, 501, "ollama", "/opt/homebrew/bin/ollama"),
            row(42, 1, 501, "cfprefsd", "/usr/sbin/cfprefsd"),
            row(50, 12, 501, "bash", "/bin/bash"),
            row(51, 50, 501, "make", "/usr/bin/make"),
        ];
        let fake = Fake {
            args: HashMap::from([
                (12, vec!["-zsh"]),
                (14, vec!["/bin/zsh", "-c", "-l", "npx playwright test"]),
                (
                    15,
                    vec![
                        "node",
                        "/p/node_modules/.bin/playwright",
                        "test",
                        "--workers",
                        "4",
                    ],
                ),
                (20, vec!["python3", "/p/pcb.py", "sim"]),
                (40, vec!["ls-sim", "--port", "9000"]),
                (50, vec!["bash", "build.sh"]),
            ]),
            sessions: HashMap::from([(40, 12), (41, 1), (42, 1)]),
        };
        (procs, fake)
    }

    #[test]
    fn helpers_and_workers_group_under_the_launched_command() {
        let (procs, fake) = fixture();
        let mut tree = Tree::new(&procs, &fake);
        for chrome in [16, 17, 18] {
            assert_eq!(tree.root(chrome), 15, "playwright run owns its browsers");
        }
        assert_eq!(
            tree.root(21),
            20,
            "framework Python.app is not a GUI app boundary"
        );
        assert_eq!(tree.root(31), 30, "same-bundle helper stays with its app");
        assert_eq!(tree.root(14), 13, "agent's -c wrapper belongs to the agent");
        assert_eq!(tree.root(13), 13, "interactive shell is a boundary");
        assert_eq!(tree.root(12), 12, "root login is an owner boundary");
        assert_eq!(tree.root(51), 50, "a script shell is part of the command");
        assert_eq!(tree.launched_from(15).as_deref(), Some("claude"));
        assert_eq!(tree.launched_from(13).as_deref(), Some("iTerm"));
        assert_eq!(tree.launched_from(40).as_deref(), Some("launchd"));
    }

    #[test]
    fn groups_sum_cpu_and_describe_the_root_command() {
        let (procs, fake) = fixture();
        let cpu = HashMap::from([
            (15, 20.),
            (16, 90.),
            (17, 95.),
            (18, 80.),
            (21, 99.),
            (40, 100.),
        ]);
        let a = attribute_for(&procs, &cpu, &fake, 501, 99);
        let top = &a.groups[0];
        assert_eq!((top.root_pid, top.processes), (15, 4));
        assert_eq!(top.name, "playwright");
        assert_eq!(top.cpu_percent, 285.);
        assert_eq!(top.members, "chrome-headless-shell x3, node x1");
        assert!(top.command.contains("playwright test --workers 4"));
        assert_eq!(top.launched_from.as_deref(), Some("claude"));
        assert_eq!(top.age.as_deref(), Some("1h15m"));
        let orphan = a.groups.iter().find(|g| g.root_pid == 40).unwrap();
        assert!(orphan.likely_orphan);
        assert_eq!(
            a.groups.iter().find(|g| g.root_pid == 20).unwrap().name,
            "pcb"
        );
    }

    #[test]
    fn orphans_need_a_dead_session_or_busy_non_service_process() {
        let (procs, fake) = fixture();
        let quiet = attribute_for(&procs, &HashMap::new(), &fake, 501, 99);
        assert_eq!(
            quiet.detached.iter().map(|d| d.pid).collect::<Vec<_>>(),
            [40]
        );
        assert!(quiet.detached[0].likely_orphan);
        let busy = attribute_for(
            &procs,
            &HashMap::from([(41, 300.), (42, 50.)]),
            &fake,
            501,
            99,
        );
        let pids: Vec<_> = busy
            .detached
            .iter()
            .map(|d| (d.pid, d.likely_orphan))
            .collect();
        assert_eq!(
            pids,
            [(40, true), (41, false)],
            "system daemons are never listed"
        );
    }

    #[test]
    fn shell_script_detection() {
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(shell_runs_script(&argv(&["bash", "build.sh"])));
        assert!(shell_runs_script(&argv(&["zsh", "-e", "x.zsh"])));
        assert!(!shell_runs_script(&argv(&["zsh", "-lc", "make"])));
        assert!(!shell_runs_script(&argv(&["-zsh"])));
        assert!(!shell_runs_script(&argv(&["bash", "-l"])));
    }

    #[test]
    fn interpreter_scripts_are_named_by_script() {
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            display_name("node", &argv(&["node", "--inspect", "/x/vite.mjs"])),
            "vite"
        );
        assert_eq!(
            display_name("python3.12", &argv(&["python3", "-m", "pytest"])),
            "pytest"
        );
        assert_eq!(display_name("node", &argv(&["node", "-e", "1"])), "node");
        assert_eq!(display_name("cargo", &argv(&["cargo", "build"])), "cargo");
        assert_eq!(
            display_name("node", &argv(&["npm exec vite --port 1"])),
            "npm"
        );
        assert_eq!(
            display_name("bash", &argv(&["/bin/bash", "bin/e2e"])),
            "e2e"
        );
        assert_eq!(display_name("zsh", &argv(&["zsh", "-c", "make"])), "zsh");
        assert_eq!(display_name("versions", &argv(&["ugrep", "-G"])), "ugrep");
        assert_eq!(
            display_name("Helper", &argv(&["/A b.app/Helper x", "--type=gpu"])),
            "Helper"
        );
    }

    #[test]
    fn ages_and_truncation() {
        assert_eq!(format_age(42.), "42s");
        assert_eq!(format_age(75. * 60.), "1h15m");
        assert_eq!(format_age(2. * 86400. + 7200.), "2d02h");
        assert_eq!(truncate("abcdef", 5), "ab...");
        assert_eq!(truncate("abc", 5), "abc");
    }
}
