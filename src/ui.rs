use crate::{
    analysis::{Ledger, number},
    types::ProcRow,
};
use ratatui::{
    Frame,
    crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Color, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{Axis, Block, Chart, Dataset, GraphType, Paragraph, Row, Table, TableState, Wrap},
};
use std::path::Path;

#[derive(Default)]
pub struct View {
    selected: usize,
    totals: bool,
    marker: Option<String>,
    detail: bool,
    state: TableState,
    tree_selected: usize,
    pub vm_target: Option<(i32, Option<u64>)>,
    vm_focus: bool,
    vm_processes: bool,
    vm_selected: usize,
    vm_state: TableState,
}
pub enum Input {
    None,
    Quit,
    Mark(String),
    Dive,
}
impl View {
    pub fn event(&mut self, e: Event, ledger: &Ledger) -> Input {
        let Event::Key(key) = e else {
            return Input::None;
        };
        if key.kind == KeyEventKind::Release {
            return Input::None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Input::Quit;
        }
        if self.vm_target.is_some() && self.vm_focus {
            match key.code {
                KeyCode::Esc | KeyCode::Backspace => {
                    self.vm_focus = false;
                }
                KeyCode::Char('q') => return Input::Quit,
                KeyCode::Char('d') => return Input::Dive,
                KeyCode::Tab => {
                    self.vm_processes = !self.vm_processes;
                    self.vm_selected = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.vm_selected = self.vm_selected.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.vm_selected = self.vm_selected.saturating_sub(1)
                }
                _ => {}
            }
            return Input::None;
        }
        if let Some(text) = &mut self.marker {
            match key.code {
                KeyCode::Enter => {
                    let label = text.trim().to_string();
                    self.marker = None;
                    if !label.is_empty() {
                        return Input::Mark(label);
                    }
                }
                KeyCode::Esc => self.marker = None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) if !c.is_control() && text.len() < 200 => text.push(c),
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Input::Quit,
                KeyCode::Char('m') => self.marker = Some(String::new()),
                KeyCode::Char('s') => {
                    self.totals = !self.totals;
                    self.selected = 0;
                    self.tree_selected = 0;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.tree_selected = self.tree_selected.saturating_add(1);
                    if self.tree_selected < current(ledger).len() {
                        self.selected = self.tree_selected;
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.tree_selected = self.tree_selected.saturating_sub(1);
                    self.selected = self.selected.min(self.tree_selected);
                }
                KeyCode::PageDown => self.tree_selected = self.tree_selected.saturating_add(10),
                KeyCode::PageUp => self.tree_selected = self.tree_selected.saturating_sub(10),
                KeyCode::Char('g') => {
                    self.tree_selected = 0;
                    self.selected = 0;
                }
                KeyCode::Char('G') => {
                    self.tree_selected = usize::MAX;
                }
                KeyCode::Enter | KeyCode::Char('d') => {
                    if !self.totals
                        && let Some(p) = current(ledger).get(self.selected)
                        && crate::vm::is_vm(&p.name, p.executable.as_deref())
                    {
                        self.vm_target = Some((p.pid, p.start_abstime));
                        self.vm_focus = true;
                        self.vm_selected = 0;
                        return Input::Dive;
                    }
                    self.detail = !self.detail;
                }
                _ => {}
            }
        }
        Input::None
    }
}
fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .title(title)
        .border_style(Style::default().fg(Color::DarkGray))
}
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}
fn current(ledger: &Ledger) -> Vec<&ProcRow> {
    let mut rows: Vec<_> = ledger
        .latest
        .as_ref()
        .map(|s| s.procs.iter().collect())
        .unwrap_or_default();
    rows.sort_by(|a, b| {
        b.energy_mw
            .unwrap_or(-1.)
            .total_cmp(&a.energy_mw.unwrap_or(-1.))
            .then(
                b.cpu_percent
                    .unwrap_or(-1.)
                    .total_cmp(&a.cpu_percent.unwrap_or(-1.)),
            )
    });
    rows
}
pub fn draw(f: &mut Frame, ledger: &Ledger, path: &Path, view: &mut View) {
    let area = f.area();
    if area.width < 65 || area.height < 20 {
        f.render_widget(Paragraph::new(format!("● RECORDING  {:.3} Wh\nBattery {} W (60s)\nEnlarge terminal to 65×20 for dashboard.\nq quit · m mark change\n{}",ledger.battery_j/3600.,number(ledger.rolling_w()),path.display())).wrap(Wrap{trim:false}),area);
        return;
    }
    if view.vm_target.is_some() && view.vm_focus {
        draw_vm(f, ledger, path, view);
        return;
    }
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(5),
        Constraint::Length(if view.detail || area.height < 24 {
            0
        } else if area.height < 32 {
            4
        } else {
            7
        }),
        Constraint::Min(5),
        Constraint::Length(if view.detail { 7 } else { 4 }),
        Constraint::Length(2),
    ])
    .split(area);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(" ● RECORDING ", Style::default().fg(Color::Green).bold()),
                Span::raw(format!(
                    "DUDE, WHERE'S MY BATTERY?   +{:.0}s   {:.3} Wh observed",
                    ledger.elapsed,
                    ledger.battery_j / 3600.
                )),
            ]),
            Line::from(format!(" {}", path.display())),
        ]),
        rows[0],
    );
    let s = ledger.latest.as_ref();
    let b = s.and_then(|s| s.battery.as_ref());
    let soc = s.and_then(|s| s.soc.as_ref());
    let thermal = s.and_then(|s| s.thermal.as_ref());
    let state = b
        .map(|b| {
            if b.external_connected {
                "AC connected"
            } else {
                "on battery"
            }
        })
        .unwrap_or("battery unavailable");
    f.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                "Battery {} W (60s) · {}% · {} · SMC system {} W",
                number(ledger.rolling_w()),
                number(b.map(|b| b.soc_percent)),
                state,
                number(thermal.and_then(|t| t.pstr_w))
            )),
            Line::from(format!(
                "Chip: CPU {} W · GPU {} W · total {} W · CPU {}°C · {}",
                number(soc.and_then(|s| s.cpu_w)),
                number(soc.and_then(|s| s.gpu_w)),
                number(soc.map(|s| s.total_w)),
                number(thermal.and_then(|t| t.cpu_die_max_c)),
                thermal
                    .map(|t| t.pressure.label())
                    .unwrap_or_else(|| "unknown".into())
            )),
            Line::from(format!(
                "CPU energy counters {}/{} · sampler {:.0}ms · {} gaps excluded",
                ledger.measured_procs, ledger.total_procs, ledger.sampler_ms, ledger.gaps
            )),
        ])
        .block(panel(
            " Whole machine · process CPU energy excludes screen / GPU ",
        )),
        rows[1],
    );
    let points: Vec<_> = ledger
        .history
        .iter()
        .filter_map(|p| p.w.map(|w| (p.t, w)))
        .collect();
    let system: Vec<_> = ledger.system_history.iter().copied().collect();
    let max = points
        .iter()
        .chain(system.iter())
        .map(|p| p.1)
        .fold(10., f64::max)
        * 1.15;
    let chart = Chart::new(vec![
        Dataset::default()
            .data(&system)
            .marker(Marker::Braille)
            .graph_type(GraphType::Scatter)
            .style(Style::default().fg(Color::Yellow)),
        Dataset::default()
            .data(&points)
            .marker(Marker::Braille)
            .graph_type(GraphType::Scatter)
            .style(Style::default().fg(Color::Cyan)),
    ])
    .block(panel(
        " Last 5 min · battery: cyan · SMC system: yellow · gaps: blank ",
    ))
    .x_axis(Axis::default().bounds([(ledger.elapsed - 300.).max(0.), ledger.elapsed.max(1.)]))
    .y_axis(
        Axis::default()
            .bounds([0., max])
            .labels(["0".to_string(), format!("{:.0}W", max)]),
    );
    f.render_widget(chart, rows[2]);
    let current = current(ledger);
    let totals = ledger.ranked();
    let count = if view.totals {
        totals.len()
    } else {
        current.len()
    };
    view.selected = view.selected.min(count.saturating_sub(1));
    let (headers, widths, body, title) = if view.totals {
        let body: Vec<Row> = totals
            .iter()
            .map(|p| {
                Row::new(vec![
                    clean(&p.name),
                    if p.energy_samples > 0 {
                        format!("{:.5}", p.cpu_j / 3600.)
                    } else {
                        "—".into()
                    },
                    format!("{:.1}", p.cpu_s),
                    format!("{:.0}", p.wakeups),
                    format!("{:.1}", p.disk_bytes / 1e6),
                ])
            })
            .collect();
        (
            vec![
                "APP / EXECUTABLE",
                "CPU Wh",
                "CPU sec",
                "Wakeups",
                "Disk MB",
            ],
            vec![
                Constraint::Min(20),
                Constraint::Length(12),
                Constraint::Length(12),
                Constraint::Length(12),
                Constraint::Length(12),
            ],
            body,
            " Since recording started · includes exited processes · grouped by app / name ",
        )
    } else {
        let mut body: Vec<Row> = Vec::new();
        for p in &current {
            body.push(Row::new(vec![
                p.pid.to_string(),
                format!(
                    "{}{}",
                    clean(&p.name),
                    if p.pid as u32 == ledger.recorder_pid {
                        " [recorder]"
                    } else {
                        ""
                    }
                ),
                number(p.energy_mw.map(|w| w / 1000.)),
                number(p.cpu_percent),
                number(p.wakeups_per_s),
                number(p.footprint_bytes.map(|b| b as f64 / 1048576.)),
            ]));

            // Keep the host process list intact, but expand a VM directly under
            // its host row once its asynchronous inspection is available.
            if let Some(vm) = ledger.vms.get(&p.pid) {
                if let Some(containers) = &vm.containers {
                    for c in containers {
                        body.push(
                            Row::new(vec![
                                "".into(),
                                format!("  ↳ container {}", clean(&c.name)),
                                "guest".into(),
                                number(c.cpu_percent),
                                "".into(),
                                clean(&c.memory),
                            ])
                            .style(Style::default().fg(Color::Yellow)),
                        );
                    }
                }
                if let Some(guest) = &vm.guest {
                    let active: Vec<_> = guest
                        .processes
                        .iter()
                        .filter(|child| child.cpu_percent >= 1.0)
                        .take(20)
                        .collect();
                    for child in active {
                        body.push(
                            Row::new(vec![
                                "".into(),
                                format!("    ↳ {}", clean(&child.name)),
                                "Linux".into(),
                                format!("{:.1}", child.cpu_percent),
                                "".into(),
                                format!("{:.1}", child.rss_bytes as f64 / 1048576.),
                            ])
                            .style(Style::default().fg(Color::Gray)),
                        );
                    }
                    let hidden = guest
                        .processes
                        .iter()
                        .filter(|child| child.cpu_percent < 1.0)
                        .count();
                    if hidden > 0 {
                        body.push(
                            Row::new(vec![
                                "".into(),
                                format!("    ↳ {hidden} idle Linux processes hidden (<1% CPU)"),
                                "Linux".into(),
                                "".into(),
                                "".into(),
                                "".into(),
                            ])
                            .style(Style::default().fg(Color::DarkGray)),
                        );
                    }
                }
            }
        }
        (
            vec!["PID", "PROCESS", "CPU W", "CPU %", "Wake/s", "RAM MiB"],
            vec![
                Constraint::Length(8),
                Constraint::Min(20),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(10),
            ],
            body,
            " Now · sorted by CPU energy · VM children show active guest CPU (idle <1% collapsed) · — unavailable ",
        )
    };
    // TableState is what makes ratatui keep the selected row visible. Keep
    // selecting nested rows even though Enter's process detail remains tied to
    // the nearest host row.
    view.tree_selected = view.tree_selected.min(body.len().saturating_sub(1));
    view.state
        .select((!body.is_empty()).then_some(view.tree_selected));
    let table = Table::new(body, widths)
        .header(Row::new(headers).style(Style::default().fg(Color::Cyan)))
        .block(panel(title))
        .row_highlight_style(Style::default().bg(Color::DarkGray));
    f.render_stateful_widget(table, rows[3], &mut view.state);
    let mut detail = Vec::new();
    if view.detail && !view.totals {
        if let Some(p) = current.get(view.selected) {
            detail.push(Line::from(format!(
                "{} · PID {} · parent {} · uid {}",
                clean(&p.name),
                p.pid,
                p.ppid,
                p.uid
            )));
            detail.push(Line::from(clean(
                p.executable.as_deref().unwrap_or("Executable unavailable"),
            )));
            detail.push(Line::from(format!("Inspect: ./battery inspect {}", p.pid)));
        }
    } else {
        if let Some(c) = ledger.comparisons.last() {
            detail.push(Line::from(clean(&c.description())));
        } else {
            detail.push(Line::from(
                "m: mark one change → make it → wait 75s. Repeat to confirm.",
            ));
        }
    }
    let blockers: Vec<_> = ledger
        .assertions
        .lines()
        .filter(|l| l.contains("pid ") && (l.contains("Sleep") || l.contains("NoIdleSleep")))
        .map(|s| s.trim().split(":").next().unwrap_or(s.trim()))
        .collect();
    detail.push(Line::from(format!(
        "Sleep blockers: {}",
        clean(&blockers.join(" | "))
    )));
    let gpu = s.and_then(|s| s.gpu.as_ref());
    if view.detail {
        detail.push(Line::from(format!(
            "GPU last submitter (hint only): {} · busy {}% · {} sleep/delay gaps",
            clean(
                gpu.and_then(|g| g.top_submitter_name.as_deref())
                    .unwrap_or("unknown")
            ),
            number(gpu.and_then(|g| g.device_util_pct)),
            ledger.gaps
        )));
        if let Some(io) = s.and_then(|s| s.io.as_ref()) {
            detail.push(Line::from(format!(
                "System disk R/W {:.1}/{:.1} MB/s · network RX/TX {:.1}/{:.1} MB/s",
                io.disk_read_bps / 1e6,
                io.disk_write_bps / 1e6,
                io.net_rx_bps / 1e6,
                io.net_tx_bps / 1e6
            )));
        }
    }
    f.render_widget(
        Paragraph::new(detail).block(panel(" Evidence & experiments ")),
        rows[4],
    );
    let footer = if let Some(text) = &view.marker {
        format!("Change: {text}▏  Enter save · Esc cancel (recording continues)")
    } else {
        format!("q stop & save · s now / session · ↑↓/j/k scroll · PgUp/PgDn · g/G top/bottom · Enter details / VM · m mark change\n{}",ledger.notices.last().map(String::as_str).unwrap_or("Battery Wh is integrated over observed discharge intervals; AC and sleep gaps are excluded."))
    };
    f.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::Yellow)),
        rows[5],
    );
}

fn draw_vm(f: &mut Frame, ledger: &Ledger, path: &Path, view: &mut View) {
    let pid = view.vm_target.unwrap().0;
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(4),
        Constraint::Min(5),
        Constraint::Length(4),
        Constraint::Length(2),
    ])
    .split(f.area());
    f.render_widget(
        Paragraph::new(format!(
            "● RECORDING · VM deep dive · host PID {pid}\n{}",
            path.display()
        ))
        .style(Style::default().fg(Color::Green)),
        areas[0],
    );
    let Some(vm) = ledger
        .vms
        .get(&pid)
        .filter(|v| v.start_abstime == view.vm_target.unwrap().1)
    else {
        f.render_widget(Paragraph::new("Identifying VM from open disks, then sampling its containers and Linux processes…\nHost recording continues. No sudo or software installation.").wrap(Wrap{trim:false}),areas[1]);
        f.render_widget(Paragraph::new("Esc back · q stop recording"), areas[4]);
        return;
    };
    if vm.identity.is_none() {
        f.render_widget(
            Paragraph::new("VM identification failed · retrying 15s after each completed probe")
                .style(Style::default().fg(Color::Yellow)),
            areas[1],
        );
        let mut errors = vec![Line::from(
            "Container and Linux process metrics are unavailable until the VM is identified.",
        )];
        errors.extend(vm.errors.iter().map(|e| Line::from(clean(e))));
        f.render_widget(
            Paragraph::new(errors)
                .wrap(Wrap { trim: false })
                .block(panel(" VM probe unavailable ")),
            areas[2],
        );
        f.render_widget(
            Paragraph::new("d retry · Esc back · q stop recording\nHost recording continues."),
            areas[4],
        );
        return;
    }
    let identity = vm
        .identity
        .as_ref()
        .map(|i| format!("{} / {}", i.tool, i.profile.as_deref().unwrap_or("default")))
        .unwrap_or_else(|| "unidentified".into());
    f.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                "{identity} · refreshed {:.0}s ago · refresh every 15s",
                (crate::record::now() - vm.finished_at).max(0.)
            )),
            Line::from(
                "Guest CPU: 100% = one logical CPU. Container watts cannot be measured here.",
            ),
            Line::from(
                vm.guest
                    .as_ref()
                    .map(|g| {
                        format!(
                            "Linux busy {}% of {} vCPUs · process CPU coverage {}/{}",
                            number(g.busy_percent),
                            g.logical_cpus.unwrap_or(0),
                            g.measured_processes,
                            g.observed_processes
                        )
                    })
                    .unwrap_or_else(|| "Linux process metrics unavailable".into()),
            ),
        ]),
        areas[1],
    );
    let mut details = Vec::new();
    if view.vm_processes {
        let ps: Vec<_> = vm
            .guest
            .as_ref()
            .map(|g| g.processes.iter().collect())
            .unwrap_or_default();
        view.vm_selected = view.vm_selected.min(ps.len().saturating_sub(1));
        view.vm_state
            .select((!ps.is_empty()).then_some(view.vm_selected));
        let rows = ps.iter().map(|p| {
            Row::new(vec![
                p.pid.to_string(),
                format!("{:.1}", p.cpu_percent),
                clean(&p.name),
                clean(&vm.owner(p)),
                format!("{:.1}", p.rss_bytes as f64 / 1048576.),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(9),
                Constraint::Length(8),
                Constraint::Length(22),
                Constraint::Min(18),
                Constraint::Length(10),
            ],
        )
        .header(
            Row::new([
                "LINUX PID",
                "CPU %",
                "PROCESS",
                "CONTAINER / OWNER",
                "RSS MiB",
            ])
            .style(Style::default().fg(Color::Cyan)),
        )
        .block(panel(" Linux processes · current two-second interval "))
        .row_highlight_style(Style::default().bg(Color::DarkGray));
        f.render_stateful_widget(table, areas[2], &mut view.vm_state);
        if let Some(p) = ps.get(view.vm_selected) {
            details.push(Line::from(clean(&format!(
                "{} · parent {} · {}",
                p.name,
                p.ppid,
                vm.owner(p)
            ))));
            details.push(Line::from(clean(
                p.cgroup.as_deref().unwrap_or("cgroup unreadable"),
            )));
        }
    } else {
        let cs: Vec<_> = vm
            .containers
            .as_ref()
            .map(|cs| cs.iter().collect())
            .unwrap_or_default();
        view.vm_selected = view.vm_selected.min(cs.len().saturating_sub(1));
        view.vm_state
            .select((!cs.is_empty()).then_some(view.vm_selected));
        let rows = cs.iter().map(|c| {
            Row::new(vec![
                number(c.cpu_percent),
                clean(&c.name),
                clean(&c.memory),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(8),
                Constraint::Min(25),
                Constraint::Length(26),
            ],
        )
        .header(Row::new(["CPU %", "CONTAINER", "MEMORY"]).style(Style::default().fg(Color::Cyan)))
        .block(panel(
            " Containers · Docker sample · Tab for Linux processes ",
        ))
        .row_highlight_style(Style::default().bg(Color::DarkGray));
        f.render_stateful_widget(table, areas[2], &mut view.vm_state);
        if let Some(c) = cs.get(view.vm_selected) {
            details.push(Line::from(clean(&format!(
                "Image: {} · Compose: {} / {}",
                c.image,
                c.project.as_deref().unwrap_or("—"),
                c.service.as_deref().unwrap_or("—")
            ))));
            details.push(Line::from(format!(
                "Network: {} · disk: {} (lifetime I/O totals)",
                c.net_io, c.block_io
            )));
        }
    }
    if let Some(e) = vm.errors.first() {
        details.push(Line::from(clean(e)));
    }
    f.render_widget(Paragraph::new(details), areas[3]);
    f.render_widget(Paragraph::new("Tab containers / Linux processes · ↑↓ select · d refresh · Esc back · q stop\nHost samples and completed VM inspections are saved while this view is open.").style(Style::default().fg(Color::Yellow)),areas[4]);
}
