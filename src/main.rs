#![cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(unused))]
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("cheese currently supports Apple Silicon macOS only");
mod analysis;
mod analyze;
mod assistant;
mod deep;
mod host;
mod origin;
mod record;
mod sampler;
#[allow(dead_code)]
mod samplers;
mod signals;
mod system;
#[cfg(test)]
mod tests;
#[allow(dead_code)]
mod types;
mod ui;
mod vm;
mod wtf_ui;

use crate::{
    analysis::Ledger,
    record::{Record, Recorder, now},
    sampler::{BatterySampler, PowerSampler, ProcSampler, ThermalSampler},
    types::Sample,
};
use clap::{Parser, Subcommand};
use std::{
    io::{self, IsTerminal, Read},
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(
    version,
    about = "Why is my Mac cheesed? Diagnose a slow, hot or draining Mac in seconds, or record where the battery goes. No sudo.",
    long_about = None
)]
struct Args {
    #[command(subcommand)]
    command: Option<Action>,
    #[command(flatten)]
    diagnose: Diagnose,
    /// Directory for recordings and diagnosis evidence
    #[arg(long, global = true, default_value = "recordings")]
    dir: PathBuf,
}

/// Options of the default action: measure, inspect, then ask Codex (or Claude).
#[derive(clap::Args)]
struct Diagnose {
    /// Seconds to observe before asking Codex (or Claude) [default: 5]
    #[arg(long, value_parser = clap::value_parser!(u64).range(2..=120))]
    seconds: Option<u64>,
}
const DEFAULT_SECONDS: u64 = 5;

#[derive(clap::Args)]
struct RecordArgs {
    /// Seconds between samples
    #[arg(short, long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..=60))]
    interval: u64,
    /// Stop after this many seconds (otherwise runs until q / Ctrl-C)
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    duration: Option<u64>,
    /// Record without the terminal dashboard
    #[arg(long)]
    headless: bool,
}

#[derive(Subcommand)]
enum Action {
    /// Diagnose now (the default when no subcommand is given)
    Wtf(Diagnose),
    /// Record battery drain and CPU energy in a live terminal view
    Record(RecordArgs),
    /// Summarize a recording (latest recording if omitted)
    Report { file: Option<PathBuf> },
    /// List recordings
    Sessions,
    /// Inspect VM containers/Linux processes, or capture ordinary process stacks
    Inspect {
        #[arg(value_parser = clap::value_parser!(i32).range(1..))]
        pid: i32,
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u64).range(1..=30))]
        seconds: u64,
    },
}

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn main() {
    if let Err(e) = run() {
        eprintln!("cheese: {e}");
        std::process::exit(1);
    }
}
fn run() -> io::Result<()> {
    let args = Args::parse();
    match &args.command {
        None => analyze::run(&args.dir, args.diagnose.seconds.unwrap_or(DEFAULT_SECONDS)),
        Some(Action::Wtf(diagnose)) => analyze::run(
            &args.dir,
            diagnose
                .seconds
                .or(args.diagnose.seconds)
                .unwrap_or(DEFAULT_SECONDS),
        ),
        Some(_) if args.diagnose.seconds.is_some() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--seconds before a subcommand applies only to the diagnosis; put it after the subcommand",
        )),
        Some(Action::Record(options)) => record_session(&args.dir, options),
        Some(Action::Report { file }) => {
            let path = file
                .clone()
                .map(Ok)
                .unwrap_or_else(|| record::latest(&args.dir))?;
            let mut ledger = Ledger::default();
            record::read(&path, |r| ledger.apply(&r))?;
            println!("{}\n{}", path.display(), ledger.report());
            Ok(())
        }
        Some(Action::Sessions) => {
            let mut files: Vec<_> = std::fs::read_dir(&args.dir)?
                .filter_map(Result::ok)
                .map(|f| f.path())
                .filter(|p| record::is_recording(p))
                .collect();
            files.sort();
            for p in files {
                println!("{}", p.display());
            }
            Ok(())
        }
        Some(Action::Inspect { pid, seconds }) => {
            let path = samplers::procname::pidpath(*pid);
            if vm::is_vm(&samplers::procname::resolve_name(*pid), path.as_deref()) {
                let result = vm::inspect(*pid, samplers::proc_rusage::process_start(*pid));
                std::fs::create_dir_all(&args.dir)?;
                let path = args
                    .dir
                    .join(format!("vm-{pid}-{}.json", (now() * 1000.) as u64));
                std::fs::write(&path, serde_json::to_vec_pretty(&result)?)?;
                println!("{}\nSaved: {}", result.report(), path.display());
                return Ok(());
            }
            std::fs::create_dir_all(&args.dir)?;
            let path =
                std::fs::canonicalize(&args.dir)?.join(format!("stack-{pid}-{}.txt", now() as u64));
            let output = bounded_command(
                Command::new("/usr/bin/sample")
                    .arg(pid.to_string())
                    .arg(seconds.to_string())
                    .arg("-file")
                    .arg(&path),
                Duration::from_secs(seconds + 15),
            )?;
            println!("{output}\nStack sample: {}", path.display());
            Ok(())
        }
    }
}

fn record_session(dir: &std::path::Path, args: &RecordArgs) -> io::Result<()> {
    unsafe {
        libc::signal(libc::SIGINT, stop_signal as *const () as libc::sighandler_t);
        libc::signal(
            libc::SIGTERM,
            stop_signal as *const () as libc::sighandler_t,
        );
    }
    let mut recorder = Recorder::new(dir, args.interval as f64)?;
    let mut ledger = Ledger {
        recorder_pid: std::process::id(),
        ..Default::default()
    };
    let interactive = !args.headless && io::stdout().is_terminal() && io::stdin().is_terminal();
    println!("Recording → {}", recorder.path.display());
    let mut sensors = Sensors::new();
    sensors.prime();
    if sensors.power.is_none() {
        emit(
            &mut recorder,
            &mut ledger,
            Record::Notice {
                timestamp: now(),
                message: "IOReport unavailable: chip power unknown".into(),
            },
        )?;
    }
    let mut terminal = if interactive {
        Some(ratatui::try_init()?)
    } else {
        None
    };
    // Restore the terminal on every error, as well as on normal exit and panic.
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            if self.0 {
                ratatui::restore();
            }
        }
    }
    let _restore = Restore(interactive);
    let mut view = ui::View::default();
    let start_wall = now();
    let start = Instant::now();
    let interval = Duration::from_secs(args.interval);
    let mut last_wall = start_wall;
    let mut next = Instant::now() + interval;
    let mut context_due = Instant::now();
    let mut dirty = true;
    let (vm_tx, vm_rx) = std::sync::mpsc::channel();
    let mut vm_inflight = false;
    let mut vm_worker: Option<std::thread::JoinHandle<()>> = None;
    let mut vm_due = Instant::now();
    while !STOP.load(Ordering::Relaxed) {
        if args
            .duration
            .is_some_and(|s| start.elapsed() >= Duration::from_secs(s))
        {
            break;
        }
        while let Ok(result) = vm_rx.try_recv() {
            emit(
                &mut recorder,
                &mut ledger,
                Record::Vm {
                    elapsed_s: now() - start_wall,
                    inspection: Box::new(result),
                },
            )?;
            vm_inflight = false;
            if let Some(worker) = vm_worker.take() {
                let _ = worker.join();
            }
            vm_due = Instant::now() + Duration::from_secs(15);
            dirty = true;
        }
        if let Some((pid, start_abstime)) = view.vm_target
            && !vm_inflight
            && Instant::now() >= vm_due
        {
            let tx = vm_tx.clone();
            vm_inflight = true;
            vm_worker = Some(std::thread::spawn(move || {
                let _ = tx.send(vm::inspect(pid, start_abstime));
            }));
        }
        if Instant::now() >= next {
            let timestamp = now();
            let dt_s = timestamp - last_wall;
            let gap = dt_s <= 0. || dt_s > (args.interval as f64 * 3.).max(10.);
            let dt = Duration::from_secs_f64(dt_s.max(0.001));
            let cost = Instant::now();
            let sample = sensors.sample(dt);
            // A VM is opaque on the host, so automatically start inspecting the
            // first measurable VM. Its children will appear inline in the host
            // process tree when the asynchronous probe completes.
            if view.vm_target.is_none()
                && let Some(vm) = sample
                    .procs
                    .iter()
                    .find(|p| vm::is_vm(&p.name, p.executable.as_deref()))
            {
                view.vm_target = Some((vm.pid, vm.start_abstime));
                vm_due = Instant::now();
            }
            let sample_record = Record::Sample {
                timestamp,
                elapsed_s: timestamp - start_wall,
                gap,
                sampler_ms: cost.elapsed().as_secs_f64() * 1000.,
                sample: Box::new(sample),
            };
            emit(&mut recorder, &mut ledger, sample_record)?;
            last_wall = timestamp;
            next = Instant::now() + interval.saturating_sub(cost.elapsed());
            dirty = true;
            if !interactive {
                println!(
                    "+{:5.0}s  battery {} W  observed {:.4} Wh  CPU counters {}/{}",
                    ledger.elapsed,
                    analysis::number(ledger.rolling_w()),
                    ledger.battery_j / 3600.,
                    ledger.measured_procs,
                    ledger.total_procs
                );
            }
        }
        if Instant::now() >= context_due {
            let assertions = bounded_command(
                Command::new("/usr/bin/pmset").args(["-g", "assertions"]),
                Duration::from_secs(2),
            );
            match assertions {
                Ok(assertions) => emit(
                    &mut recorder,
                    &mut ledger,
                    Record::Context {
                        timestamp: now(),
                        elapsed_s: now() - start_wall,
                        assertions,
                    },
                )?,
                Err(e) => emit(
                    &mut recorder,
                    &mut ledger,
                    Record::Notice {
                        timestamp: now(),
                        message: format!("Sleep assertions unavailable: {e}"),
                    },
                )?,
            }
            context_due = Instant::now() + Duration::from_secs(30);
            dirty = true;
        }
        if let Some(t) = terminal.as_mut() {
            if dirty {
                t.draw(|f| ui::draw(f, &ledger, &recorder.path, &mut view))?;
                dirty = false;
            }
            if ratatui::crossterm::event::poll(Duration::from_millis(200))? {
                match view.event(ratatui::crossterm::event::read()?, &ledger) {
                    ui::Input::Quit => break,
                    ui::Input::Mark(label) => {
                        emit(
                            &mut recorder,
                            &mut ledger,
                            Record::Marker {
                                timestamp: now(),
                                elapsed_s: now() - start_wall,
                                label,
                            },
                        )?;
                    }
                    ui::Input::Dive => {
                        vm_due = Instant::now();
                    }
                    ui::Input::None => {}
                }
                dirty = true;
            }
        } else {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    STOP.store(true, Ordering::Relaxed);
    if let Some(worker) = vm_worker {
        let _ = worker.join();
    }
    while let Ok(inspection) = vm_rx.try_recv() {
        emit(
            &mut recorder,
            &mut ledger,
            Record::Vm {
                elapsed_s: now() - start_wall,
                inspection: Box::new(inspection),
            },
        )?;
    }
    emit(&mut recorder, &mut ledger, Record::End { timestamp: now() })?;
    drop(_restore);
    println!("Saved: {}\n{}", recorder.path.display(), ledger.report());
    Ok(())
}
fn emit(recorder: &mut Recorder, ledger: &mut Ledger, record: Record) -> io::Result<()> {
    recorder.write(&record)?;
    ledger.apply(&record);
    Ok(())
}

struct Sensors {
    procs: samplers::proc_rusage::ProcRusageSampler,
    battery: samplers::battery::AppleSmartBatterySampler,
    power: Option<samplers::ioreport::IoReportSampler>,
    thermal: samplers::thermal::ThermalSamplerImpl,
    gpu: samplers::agx::AgxSampler,
    io: samplers::io::IoSampler,
}
impl Sensors {
    fn new() -> Self {
        Self {
            procs: samplers::proc_rusage::ProcRusageSampler::new(),
            battery: samplers::battery::AppleSmartBatterySampler::new(),
            power: samplers::ioreport::IoReportSampler::new(),
            thermal: samplers::thermal::ThermalSamplerImpl::new(),
            gpu: samplers::agx::AgxSampler::new(),
            io: samplers::io::IoSampler::new(),
        }
    }
    fn prime(&mut self) {
        self.procs.tick(Duration::ZERO);
        if let Some(p) = &mut self.power {
            p.tick(Duration::ZERO);
        }
        self.io.tick(Duration::ZERO);
    }
    fn sample(&mut self, dt: Duration) -> Sample {
        let procs = self.procs.tick(dt);
        let battery = self.battery.read();
        let soc = self.power.as_mut().and_then(|p| p.tick(dt));
        self.gpu.begin();
        self.gpu.poll();
        let gpu = Some(self.gpu.finish(None));
        Sample {
            dt,
            procs,
            battery,
            soc,
            gpu,
            thermal: Some(self.thermal.read()),
            io: self.io.tick(dt),
        }
    }
}

fn bounded_command(command: &mut Command, timeout: Duration) -> io::Result<String> {
    use std::os::unix::process::CommandExt;
    let mut child = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let result = stdout.read_to_string(&mut s).map(|_| s);
        let _ = out_tx.send(result);
    });
    std::thread::spawn(move || {
        let mut s = String::new();
        let result = stderr.read_to_string(&mut s).map(|_| s);
        let _ = err_tx.send(result);
    });
    let start = Instant::now();
    let stop_child = |child: &mut std::process::Child| {
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    };
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() >= timeout || STOP.load(Ordering::Relaxed) {
            stop_child(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "command timed out or recording stopped",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // A descendant may retain stdout after the launcher exits. Its pipe must
    // obey the same deadline as the launcher, rather than blocking a join.
    let stdout = match out_rx.recv_timeout(timeout.saturating_sub(start.elapsed())) {
        Ok(s) => s?,
        Err(_) => {
            stop_child(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "command output timed out",
            ));
        }
    };
    let stderr = match err_rx.recv_timeout(timeout.saturating_sub(start.elapsed())) {
        Ok(s) => s?,
        Err(_) => {
            stop_child(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "command errors timed out",
            ));
        }
    };
    if !status.success() {
        return Err(io::Error::other(format!("{status}: {stderr}{stdout}")));
    }
    Ok(stdout)
}
