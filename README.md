# Dude, where's my cheese?

Why is my Mac cheesed? (Finnish "kone on juustossa": the machine is jammed,
slow or hot.) `cheese` diagnoses a slow, hot or battery-draining Apple Silicon
Mac in a few seconds, and can record where the battery goes over a longer
session. No sudo, server, browser, or background daemon.

```sh
./cheese                          # build if needed; 5 s diagnosis, then exit
./cheese --seconds 30             # longer observation
./cheese wtf                      # same as ./cheese (kept as an alias)
./cheese record                   # record + live terminal view of battery drain
./cheese record --headless --duration 300
./cheese report                   # summarize the latest recording
./cheese report recordings/session-....jsonl.gz
./cheese sessions
./cheese inspect 12345            # 3-second stack sample of a chosen process
```

Requires an Apple Silicon Mac, Xcode Command Line Tools and Rust (edition 2024).
Tested on Apple M5 Pro / macOS 26. The launcher builds an optimized binary into
`target/release/cheese`; `cargo install --path .` also works. With the launcher,
relative paths are relative to this repository; an installed binary uses your
current directory. `--dir PATH` (before or after the subcommand) changes the
recording and evidence directory. `--seconds` before a subcommand applies only to
the diagnosis; `cheese --seconds 30 record` is rejected. `./battery` is kept as a
compatibility shim that runs `./cheese` with the same arguments.

## Diagnosis (`cheese`)

It samples for 5 seconds and collects, using native sysctl/mach/libproc reads:

- **Load**: 1/5/15 minute load averages against logical CPUs and core clusters
  (`hw.perflevelN`, e.g. 6 Super + 12 Performance). The 1-minute ratio is
  classified as normal (< 0.75), busy (< 1), oversubscribed (< 2) or severely
  oversubscribed: runnable threads are queueing for cores.
- **Memory**: kernel pressure (`kern.memorystatus_vm_pressure_level`), compressor
  size, swap use (`vm.swapusage`) and pageout/swapin/swapout deltas over the
  sample window (`host_statistics64`). Verdict: critical/warning from the
  kernel, else swapping (pages swapped during the window), else heavily
  compressed (compressor at least 25% of RAM), else normal.
- **Power**: AC or battery, charge level, mean pack power, thermal pressure and
  a one-line note, e.g. "on AC but battery 8% and charging slowly".
- **Origin groups**: every process is attributed to the command a person or
  agent launched, so "16 chrome processes" becomes "1 playwright test run".
  From each process the parent chain is climbed and stops below the first
  boundary: launchd or a missing parent; a parent owned by another user (root
  `login`, `sudo`); a session process (`login`, `sshd`, `tmux`, `screen`,
  `mosh-server`, `sudo`, `su`); an interactive or `-c` shell (a terminal prompt
  or an agent's command wrapper); or a GUI `.app` launching an executable outside
  its own bundle. Shells running a script file, interpreters, helpers and workers
  are folded into the command that started them; app helpers stay with their
  app. Each group reports process count, summed mean CPU (native per-process
  counters, `top` for other users' processes), memory footprint of readable
  members, member kinds, root command line (truncated), the terminal app or agent
  it was launched from, and age. The busiest eight by CPU plus up to three large
  memory users are reported. Scripts are named by their file (`node
  .../playwright test` is "playwright", `python -m pytest` is "pytest").
- **Detached processes**: your processes reparented to launchd (ppid 1) outside
  system directories. A *likely orphan* has a session ID that is neither
  launchd's nor its own: it was started from a terminal or agent session that has
  since ended (a leftover dev server, simulator or `tail -F`). Other busy ppid-1
  processes are listed without that flag because they may be launchd services.
  App bundles are only listed when their session marks them as orphans.

It also captures per-process samples, `ps`/`top` snapshots and macOS power and
sleep-assertion status, then deep-inspects up to three busy processes in
parallel. This uses measured interval CPU (including system processes from
`top`) and keeps a busy VM in scope. VM probes reuse `cheese inspect`'s
disk-based identification, container/Compose metrics and guest Linux process
sampling. Other processes get a one-second stack sample plus their command and
parent process context. Detailed VM JSON and stack reports are saved beside the
measurement. These bounded probes can add time beyond the observation; the UI
shows the inspection stage. Exited/reused PIDs, permission failures and
unsupported VMs are reported as unavailable rather than guessed.

The assistant then answers in English, limited in the prompt to 110 words: the
main cause, up to four consumers by origin group, and one or two concrete
actions (such as which command or PID to stop). The terminal panel shows live
temperature, power and battery cards, the load/memory/power/origin/orphan lines,
an animated progress indicator, then the formatted diagnosis. It exits
automatically and leaves the result in scrollback. When redirected (or with
`NO_COLOR`), the answer is plain text on stdout, with status and signal lines on
stderr. Codex/Claude hooks, retries and other diagnostics are saved to
`analyze-*.codex.log` and `analyze-*.claude.log` beside the evidence rather than
printed on screen.

### Assistant login

The diagnosis uses a ChatGPT subscription login through the `codex` CLI
(`codex login`). If Codex is unavailable, not signed in with ChatGPT, or fails,
it automatically uses `klaude --print` when the `klaude` account manager is on
`PATH`, otherwise `claude --print`. Both the login check and analysis go through
the same command, so existing klaude subscription accounts work without another
login. Klaude runs without an account menu and selects its active automatic
account; cheese disables klaude's daemon auto-setup. Plain Claude uses its own
subscription login. At least one CLI must be installed and signed in.
It exits successfully when either CLI produces an answer and reports failure if
neither works. Interrupting an analysis cancels it without starting a fallback.
Both attempts reuse the same measurements.
API-key environment overrides are removed from the child processes. Codex is
restricted to ChatGPT login and its OpenAI provider; Claude uses claude.ai login
with settings-based API credentials disabled. Codex runs with a read-only sandbox;
Claude interprets the captured evidence with tools disabled. Neither asks for
approval or changes workloads or settings; actions are only recommended.

Existing `CODEX_HOME` and `CLAUDE_CONFIG_DIR` are respected. For custom login
profiles that should work from every terminal, optionally save their paths in
`~/.config/cheese/config.json` (`$XDG_CONFIG_HOME/cheese/config.json` if set;
the pre-rename `battery/config.json` is still read when no `cheese` file exists):

```json
{"codex_home": "/absolute/path/to/codex-profile", "claude_config_dir": "/absolute/path/to/claude-profile"}
```

Either field can be omitted. Explicit environment variables take precedence.
This file stores paths only; credentials stay in the CLIs' own stores.

Evidence and the prompt are saved as `recordings/analyze-*.txt` (or under
`cheese --dir PATH`). The JSON starts with `headline` (the same lines the UI
shows), `system` (load, memory, power), `origin_groups` and
`detached_processes`. Unlike ordinary local recordings, this evidence is sent
through Codex, or Claude on fallback. It includes process names, command
arguments, paths, sensor readings and sleep assertions. The short observation
supports a current-load diagnosis, not proof of how long a workload has been hot.

## How to actually find the drain (`cheese record`)

1. Run `./cheese record` **on battery**. Give it a few minutes of normal use. The 60s
   battery average and session Wh quantify the problem. `s` switches between
   individual processes now and accumulated CPU energy grouped by app/executable.
2. Follow sustained consumers, not only momentary CPU spikes. Arrow keys select
   a row; Enter shows its path, parent PID, owner, GPU hint and I/O context.
   `./cheese inspect PID` in a second terminal captures stacks for further
   investigation. Sampling may be denied by macOS; the command reports failure.
3. Change **one thing**. Press `m`, describe it (for example "dimmed display" or
   "closed Chrome"), press Enter and immediately make that change. The recorder
   keeps running while you type. Wait 75 seconds before making another change.
4. The comparison uses 60s before the marker, skips 15s for settling and measures
   60s after that. It requires sufficient on-battery coverage on both sides.
   Reverse the change and repeat. A consistent wattage reduction is much more
   useful evidence than a guessed per-app percentage. Background activity can
   still confound any single comparison; this is an association, not proof.
5. Press `q` or Ctrl-C. The recording stays on disk and the summary prints.
   `./cheese report` reconstructs the same totals and comparisons later.

If CPU energy is low but total drain is high, test display brightness, external
peripherals, rendering/video and network workloads individually. If the Mac
should be sleeping, inspect the displayed sleep assertions and their owners.
An assertion identifies who requested that the Mac remain awake; it does not
measure the energy that request caused.

## What the numbers mean

- **Battery W / Wh**: pack voltage × signed current from AppleSmartBattery;
  watts are integrated with the trapezoidal rule between observed endpoints.
  This is sampled battery drain, not an exact coulomb-counter energy total.
  The first sample is a baseline. AC, charging, missing readings and intervals
  over max(10s, 3× sampling period) are excluded. A sleep/wake gap is not filled
  with invented watts. This version does not account for energy used asleep.
- **CPU W / CPU Wh**: deltas of `ri_energy_nj` for readable processes. This is
  **CPU energy, not all energy attributable to an application**. GPU, display,
  radios and indirect work in other processes are not included. Session totals
  start at zero; they never import energy consumed before recording started.
  Exited processes remain in the accumulated totals. Processes entirely between
  samples, and activity before a new process's first baseline, can be missed.
- **Coverage**: number of processes with a readable interval CPU-energy counter
  out of all listed processes. Other users' processes are generally unreadable.
  `-` means unknown, not zero. A supported counter may also remain zero on some
  OS/hardware combinations. CPU time remains a separate corroborating signal.
- **Chip model**: IOReport energy-model channels; CPU/GPU and the aggregate are
  model estimates. Channel names and coverage depend on the chip/OS. The tool
  does not force them to sum to battery power and does not label the difference
  as "screen watts." **SMC system W** is an independent sensor cross-check, also graphed in yellow
  while on AC (battery drain is cyan).
- **GPU last submitter**: one snapshot per sample, only a hint about a recent GPU
  client. It is not a measure of that process's GPU utilization or wattage.
- **Wake/s, RAM, I/O**: interval wakeups, physical memory footprint and disk
  counters provide context, not conversions into watts. System network counters
  can include virtual interfaces; do not interpret them as radio energy.
- **Grouping**: first `.app` bundle in the executable path, otherwise executable
  name. Separate CLI jobs with the same name are combined; the current-process
  view and raw recording retain individual identities. PID start time protects
  interval energy deltas against PID reuse. Short-lived cross-uid identities
  and exec/reparenting remain best-effort.

Default interval: 2s (`cheese record --interval 1` through `--interval 60`). The screen redraws
only on samples or input. The recorder itself is labelled in the process table;
its observed CPU energy is included. `sampler ms` measures the collection's wall
time, not the total overhead including rendering and writes.

## Recordings

`recordings/session-TIMESTAMP-PID.jsonl.gz` is gzip-compressed, versioned newline-delimited JSON:
`session`, `sample`, `context`, `marker`, `notice`, and `end` events. Every sample
contains all observed process rows, subsystem readings, elapsed time and gap
status. Sleep assertions are captured every 30s. Each event is stored as an independent gzip member, flushed and
synced; disk-write failure stops the program visibly. `report` can read a live
file and ignores an incomplete final line/gzip member left by an interrupted write; corrupt
complete records and unknown schema versions are errors.

Data stays local. Records contain process names, executable paths and sleep
assertion descriptions. Stack captures may include additional process details.
There is no automatic retention/deletion. A test with ~830 processes used about
80 MB/hour at the default interval; size varies with process count and activity.
Both gzip and uncompressed JSONL recordings can be read by `report`. Standard
`gzip -dc FILE` extracts the JSONL for other tools.
No process is terminated, suspended or automatically sampled by the recorder.

## Development and provenance

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

The low-level sampler modules, shared types and host helpers were adapted from
Matti Paksula's MIT-licensed [power](https://github.com/matti/power), revision
`234179c8084d1ad222a6c1c451f5f191e5af7e4e`. They are included here so the tool does
not depend on a sibling checkout. Recording, interval accounting, experiments,
reports and UI are separate. The grouping approach also follows
[memtop](https://github.com/matti/memtop). The IOReport bindings in `power` derive
from the MIT-licensed [macmon](https://github.com/vladkens/macmon) bindings.

CPU-energy semantics: [Apple XNU Recount documentation](https://github.com/apple-oss-distributions/xnu/blob/main/doc/observability/recount.md).
Private IOReport/SMC interfaces and the inherited `kinfo_proc` layout can change;
readings on other hardware need validation. See [LICENSE](LICENSE).

## Inside a virtual machine

In `cheese record`, when a VM appears, the recorder starts its deep probe
automatically after the first host sample. The main process list stays visible:
the VM row expands in place with indented containers and Linux processes. You do not need to find a row or
press Enter. The recording continues while a worker identifies the VM from its
open disk images, then samples the VM's own Docker socket and Linux `/proc`
counters. The view refreshes 15s after each completed probe. **Tab** switches between
Linux processes; **Esc** returns to the host. Container rows include CPU, memory,
image and Compose project/service. Linux rows show CPU over a two-second interval
and the owning container, joined by the full cgroup container ID. An unmapped or
unreadable cgroup is labelled explicitly. The probe itself is labelled in reports.
The inline tree shows active guest processes (at least 1% CPU) and replaces the
idle remainder with one collapsed count, so sleeping Linux kernel threads cannot
bury the useful rows.

`./cheese inspect 1298` performs the same probe once and saves `recordings/vm-*.json`.
For ordinary processes, `inspect` still captures host stacks. Completed interactive
probes are stored as `vm` events in the session; `report` includes the latest probe
for each inspected VM. Docker and Linux intervals overlap but are not identical,
so their percentages should not be subtracted to calculate virtualization overhead.
No per-container battery watts are invented from CPU shares.

Supported: conventional Colima profiles and Lima instances (requires `limactl`
and Python 3 already in the guest); Docker Desktop container metrics via its
local user socket. No guest packages are installed. Unrecognized/ambiguous VM
identities, missing tools, unavailable Docker sockets and guest permission errors
are reported. The global Docker context is never used as a fallback; it might
refer to a completely different machine. Probes query existing instances only,
use no sudo, and do not start/stop containers. Network/disk figures from Docker
are lifetime I/O totals, not rates. Stopping a recording cancels outstanding
probe commands; a cancelled probe can be recorded with an availability error.

Reference: [Docker stats](https://docs.docker.com/reference/cli/docker/container/stats/),
[Lima shell](https://lima-vm.io/docs/reference/limactl_shell/).
