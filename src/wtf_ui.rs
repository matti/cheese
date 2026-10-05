//! A small inline dashboard; backend chatter never reaches the terminal.
use crate::{
    STOP,
    signals::{Headline, Level},
    types::Sample,
};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, LineGauge, Paragraph, Widget},
};
use std::{
    io::{self, IsTerminal},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

const CYAN: Color = Color::Rgb(94, 234, 212);
const PURPLE: Color = Color::Rgb(192, 132, 252);
const MUTED: Color = Color::Rgb(148, 163, 184);
const GOLD: Color = Color::Rgb(251, 191, 36);
const WHITE: Color = Color::Rgb(226, 232, 240);
/// Rows reserved for system signals (load, memory, power, origins, orphans).
const SIGNAL_ROWS: u16 = 5;

#[derive(Default)]
struct Metrics {
    cpu: Option<(f64, f64)>,
    watts: f64,
    watt_samples: usize,
    battery: Option<f64>,
    power: &'static str,
}

pub struct Progress {
    terminal: Option<Terminal<CrosstermBackend<io::Stderr>>>,
    start: Instant,
    measurement_start: Instant,
    seconds: u64,
    phase: u8,
    message: String,
    metrics: Metrics,
    signals: Vec<Headline>,
}

impl Progress {
    pub fn new(seconds: u64) -> io::Result<Self> {
        let colorful = io::stdout().is_terminal()
            && io::stderr().is_terminal()
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").as_deref() != Ok("dumb");
        let terminal = if colorful {
            Some(Terminal::with_options(
                CrosstermBackend::new(io::stderr()),
                TerminalOptions {
                    viewport: Viewport::Inline(12 + SIGNAL_ROWS),
                },
            )?)
        } else {
            None
        };
        let mut ui = Self {
            terminal,
            start: Instant::now(),
            measurement_start: Instant::now(),
            seconds,
            phase: 0,
            message: String::new(),
            metrics: Metrics::default(),
            signals: Vec::new(),
        };
        ui.status(0, "Checking sign-in")?;
        Ok(ui)
    }

    pub fn status(&mut self, phase: u8, message: &str) -> io::Result<()> {
        if phase == 1 && self.phase != 1 {
            self.measurement_start = Instant::now();
        }
        self.phase = phase;
        self.message = message.into();
        if self.terminal.is_none() {
            eprintln!("cheese: {message}");
        }
        self.draw()
    }

    pub fn sample(&mut self, sample: &Sample) -> io::Result<()> {
        if let Some(t) = &sample.thermal {
            if let Some(c) = t.cpu_die_max_c {
                let (low, high) = self.metrics.cpu.unwrap_or((c, c));
                self.metrics.cpu = Some((low.min(c), high.max(c)));
            }
            if let Some(w) = t.pstr_w {
                self.metrics.watts += w;
                self.metrics.watt_samples += 1;
            }
        }
        if let Some(b) = &sample.battery {
            self.metrics.battery = Some(b.soc_percent);
            self.metrics.power = b.state().label();
        }
        self.draw()
    }

    /// Replace the system signal lines (load, memory, origins...).
    pub fn signals(&mut self, lines: Vec<Headline>) -> io::Result<()> {
        self.signals = lines;
        self.draw()
    }

    pub fn wait(&mut self, duration: Duration) -> io::Result<()> {
        let start = Instant::now();
        while start.elapsed() < duration {
            if STOP.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "Interrupted"));
            }
            self.draw()?;
            std::thread::sleep(
                Duration::from_millis(80).min(duration.saturating_sub(start.elapsed())),
            );
        }
        Ok(())
    }

    fn draw(&mut self) -> io::Result<()> {
        let elapsed = self.start.elapsed();
        let measured = self.measurement_start.elapsed().as_secs_f64();
        let Some(terminal) = &mut self.terminal else {
            return Ok(());
        };
        terminal.draw(|frame| {
            let area = frame.area();
            let block = panel(" MEASURE · INSPECT · EXPLAIN ");
            let inner = block.inner(area);
            frame.render_widget(block, area);
            let rows = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Length(SIGNAL_ROWS + 1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(inner);
            frame.render_widget(Paragraph::new(brand(false)), rows[0]);
            render_metrics(&self.metrics, rows[2], frame.buffer_mut());
            frame.render_widget(Paragraph::new(signal_lines(&self.signals)), rows[3]);
            let steps = Line::from(vec![
                Span::styled(
                    " 01 MEASURE ",
                    style(if self.phase <= 1 { CYAN } else { MUTED }),
                ),
                Span::styled(" → ", style(MUTED)),
                Span::styled(
                    " 02 INSPECT ",
                    style(if self.phase == 2 { PURPLE } else { MUTED }),
                ),
                Span::styled(" → ", style(MUTED)),
                Span::styled(
                    "03 ANALYZE",
                    style(if self.phase == 3 { PURPLE } else { MUTED }),
                ),
            ]);
            frame.render_widget(Paragraph::new(steps), rows[4]);
            let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
                [(elapsed.as_millis() / 100 % 10) as usize];
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!(" {spinner}  "), style(PURPLE)),
                    Span::styled(self.message.clone(), style(WHITE)),
                    Span::styled(format!("   {} s", elapsed.as_secs()), style(MUTED)),
                ])),
                rows[5],
            );
            let ratio = match self.phase {
                0 => 0.,
                1 => (measured / self.seconds as f64).min(1.),
                _ => 1.,
            };
            frame.render_widget(
                LineGauge::default()
                    .ratio(ratio)
                    .filled_style(style(CYAN))
                    .unfilled_style(style(Color::DarkGray))
                    .label(format!("{} s sample", self.seconds)),
                rows[6],
            );
        })?;
        Ok(())
    }

    pub fn finish(&mut self, answer: &str, provider: &str) -> io::Result<()> {
        let Some(terminal) = &mut self.terminal else {
            for line in &self.signals {
                eprintln!("cheese: {line}");
            }
            println!("{}", answer.trim());
            return Ok(());
        };
        let width = terminal.size()?.width;
        let lines = report_lines(answer, width.saturating_sub(6).max(1) as usize);
        let signals = signal_lines(&self.signals);
        let answer_y = 6 + signals.len() as u16 + u16::from(!signals.is_empty());
        let height = (lines.len() + 4 + answer_y as usize).min(u16::MAX as usize) as u16;
        let footer = format!(
            " {provider} · {} s sample · {} s total",
            self.seconds,
            self.start.elapsed().as_secs()
        );
        terminal.insert_before(height, |buffer| {
            let area = buffer.area;
            let block = panel(" ANALYSIS ");
            let inner = block.inner(area);
            block.render(area, buffer);
            Paragraph::new(brand(true)).render(Rect::new(inner.x, inner.y, inner.width, 1), buffer);
            render_metrics(
                &self.metrics,
                Rect::new(inner.x, inner.y + 2, inner.width, 3),
                buffer,
            );
            Paragraph::new(signals).render(
                Rect::new(inner.x, inner.y + 6, inner.width, answer_y - 6),
                buffer,
            );
            Paragraph::new(lines).render(
                Rect::new(
                    inner.x + 1,
                    inner.y + answer_y,
                    inner.width.saturating_sub(2),
                    height.saturating_sub(answer_y + 3),
                ),
                buffer,
            );
            Paragraph::new(footer).style(style(MUTED)).render(
                Rect::new(inner.x, area.bottom() - 2, inner.width, 1),
                buffer,
            );
        })?;
        terminal.draw(|frame| frame.set_cursor_position(frame.area().as_position()))?;
        terminal.show_cursor()?;
        Ok(())
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if let Some(terminal) = &mut self.terminal {
            let _ = terminal.draw(|frame| frame.set_cursor_position(frame.area().as_position()));
            let _ = terminal.show_cursor();
        }
    }
}

fn style(color: Color) -> Style {
    Style::default().fg(color)
}
fn panel(title: &'static str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(style(CYAN))
        .title_bottom(Line::from(title).style(style(MUTED)).right_aligned())
}
fn brand(done: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(" ▰▰▰ ", style(CYAN)),
        Span::styled("CHEESE", style(WHITE).add_modifier(Modifier::BOLD)),
        Span::styled(" / WTF", style(PURPLE).add_modifier(Modifier::BOLD)),
        Span::styled(
            if done {
                "   ✓ DONE"
            } else {
                "   WHY IS MY MAC CHEESED?"
            },
            style(if done { CYAN } else { MUTED }),
        ),
    ])
}
fn signal_lines(signals: &[Headline]) -> Vec<Line<'static>> {
    signals
        .iter()
        .take(SIGNAL_ROWS as usize)
        .map(|h| {
            let color = if h.level == Level::Warn { GOLD } else { WHITE };
            Line::from(vec![
                Span::styled(format!(" {:<8}", h.label), style(MUTED)),
                Span::styled(h.text.clone(), style(color)),
            ])
        })
        .collect()
}
fn render_metrics(metrics: &Metrics, area: Rect, buffer: &mut Buffer) {
    let cells = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(area);
    let cpu = metrics
        .cpu
        .map_or("-".into(), |(a, b)| format!("{a:.0}-{b:.0} °C"));
    let watts = if metrics.watt_samples > 0 {
        format!("{:.0} W", metrics.watts / metrics.watt_samples as f64)
    } else {
        "-".into()
    };
    let battery = metrics.battery.map_or("-".into(), |b| format!("{b:.0} %"));
    let heat_color = if metrics.cpu.is_some_and(|(_, hi)| hi >= 90.) {
        GOLD
    } else {
        CYAN
    };
    for (index, (label, value, color)) in [
        ("CPU · HOTTEST SENSOR", cpu, heat_color),
        ("POWER · SMC AVERAGE", watts, PURPLE),
        (metrics.power, battery, CYAN),
    ]
    .into_iter()
    .enumerate()
    {
        Paragraph::new(vec![
            Line::from(Span::styled(format!(" {label}"), style(MUTED))),
            Line::from(Span::styled(
                format!(" {value}"),
                style(color).add_modifier(Modifier::BOLD),
            )),
        ])
        .render(cells[index], buffer);
    }
}

// Render the small Markdown subset requested from the model, wrapping by
// terminal cell width and stripping controls before anything reaches the UI.
fn report_lines(answer: &str, width: usize) -> Vec<Line<'static>> {
    let clean: String = answer
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect();
    let mut result = Vec::new();
    for source in clean.lines() {
        let source = source.trim().trim_start_matches('#').trim();
        let mut line = Line::default();
        let mut bold = false;
        let mut code = false;
        for word in source.split_whitespace() {
            let emphasis = bold || code || word.contains("**") || word.contains('`');
            if word.matches("**").count() % 2 == 1 {
                bold = !bold;
            }
            if word.matches('`').count() % 2 == 1 {
                code = !code;
            }
            let word = word.replace("**", "").replace('`', "");
            let word = if word == "-" { "•".to_string() } else { word };
            let span = Span::styled(
                word,
                style(if emphasis { WHITE } else { MUTED }).add_modifier(if emphasis {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            );
            if line.width() > 0 && line.width() + span.width() + 1 > width {
                result.push(line);
                line = Line::default();
            }
            if line.width() > 0 {
                line.spans.push(Span::raw(" "));
            }
            line.spans.push(span);
        }
        result.push(line);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn report_wraps_text_and_removes_markdown_and_controls() {
        let lines = report_lines(
            "**Hot Mac:** check `ffmpeg` at 90 °C.\n\n- Load 300 %\u{0007}",
            22,
        );
        assert!(lines.iter().all(|l| l.width() <= 22));
        let text = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains(['*', '`', '\u{0007}']));
        assert!(text.contains("• Load"));
        assert!(
            lines
                .iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        );
    }
}
