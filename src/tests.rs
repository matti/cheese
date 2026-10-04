use crate::{
    analysis::Ledger,
    record::{self, Record},
    types::*,
};
use std::time::Duration;
fn sample(t: f64, dt: f64, watts: f64, ac: bool, gap: bool) -> Record {
    Record::Sample {
        timestamp: t,
        elapsed_s: t,
        gap,
        sampler_ms: 1.,
        sample: Box::new(Sample {
            dt: Duration::from_secs_f64(dt),
            battery: Some(BatteryFrame {
                voltage_mv: 12000.,
                system_power_mw: -watts * 1000.,
                external_connected: ac,
                ..Default::default()
            }),
            soc: None,
            thermal: None,
            gpu: None,
            io: None,
            procs: vec![],
        }),
    }
}
#[test]
fn integrates_trapezoid_and_excludes_ac_transitions_and_sleep() {
    let mut l = Ledger::default();
    l.apply(&sample(2., 2., 10., false, false)); // first endpoint only
    l.apply(&sample(4., 2., 20., false, false)); // 15W × 2s
    l.apply(&sample(6., 2., 40., true, false)); // connected to AC
    l.apply(&sample(8., 2., 10., false, false)); // transition: unknown
    l.apply(&sample(100., 92., 100., false, true)); // asleep/delayed
    assert_eq!(l.battery_j, 30.);
    assert_eq!(l.battery_seconds, 2.);
    assert_eq!(l.gaps, 1);
}
#[test]
fn unavailable_battery_is_not_zero() {
    let mut r = sample(4., 2., 10., false, false);
    if let Record::Sample { sample, .. } = &mut r {
        sample.battery = None;
    }
    let mut l = Ledger::default();
    l.apply(&r);
    assert_eq!(l.rolling_w(), None);
    assert_eq!(l.battery_seconds, 0.);
}
#[test]
fn marker_windows_are_weighted_and_wait_for_settling() {
    let mut l = Ledger::default();
    for t in (2..=64).step_by(2) {
        l.apply(&sample(t as f64, 2., 20., false, false));
    }
    l.apply(&Record::Marker {
        timestamp: 65.,
        elapsed_s: 65.,
        label: "dim screen".into(),
    });
    for t in (66..=140).step_by(2) {
        l.apply(&sample(t as f64, 2., 10., false, false));
    }
    let c = &l.comparisons[0];
    assert!(c.ready());
    assert_eq!(c.before.seconds, 60.);
    assert_eq!(c.after.seconds, 60.);
    assert!((c.before.mean().unwrap() - 20.).abs() < 0.1);
    assert_eq!(c.after.mean(), Some(10.));
}
#[test]
fn a_second_change_does_not_mix_two_interventions() {
    let mut l = Ledger::default();
    for t in 1..=65 {
        l.apply(&sample(t as f64, 1., 20., false, false));
    }
    l.apply(&Record::Marker {
        timestamp: 65.,
        elapsed_s: 65.,
        label: "one".into(),
    });
    for t in 66..=90 {
        l.apply(&sample(t as f64, 1., 10., false, false));
    }
    l.apply(&Record::Marker {
        timestamp: 90.,
        elapsed_s: 90.,
        label: "two".into(),
    });
    for t in 91..=180 {
        l.apply(&sample(t as f64, 1., 5., false, false));
    }
    assert!(!l.comparisons[0].ready());
    assert_eq!(l.comparisons[0].after.seconds, 10.);
}
#[test]
fn session_cpu_energy_does_not_include_process_lifetime() {
    let mut r = sample(2., 2., 10., false, false);
    if let Record::Sample { sample, .. } = &mut r {
        sample.procs.push(ProcRow {
            pid: 42,
            ppid: 1,
            uid: 501,
            same_uid: true,
            start_abstime: Some(100),
            executable: Some(
                "/Applications/Browser.app/Contents/Helpers/Renderer.app/Contents/MacOS/Renderer"
                    .into(),
            ),
            footprint_bytes: None,
            name: "Renderer".into(),
            cpu_percent: Some(50.),
            energy_mw: Some(1000.),
            lifetime_energy_j: Some(999999.),
            disk_read_bps: None,
            disk_write_bps: None,
            wakeups_per_s: Some(10.),
        });
    }
    let mut l = Ledger::default();
    l.apply(&r);
    l.apply(&sample(4., 2., 10., false, false)); // process exited; ledger persists
    let g = &l.groups["Browser"];
    assert_eq!(g.cpu_j, 2.);
    assert_eq!(g.cpu_s, 1.);
    assert_eq!(g.wakeups, 20.);
}
#[test]
fn recording_roundtrip_and_interrupted_trailer() {
    let dir = std::env::temp_dir().join(format!(
        "cheese-test-{}-{}",
        std::process::id(),
        record::now()
    ));
    let mut recorder = record::Recorder::new(&dir, 2.).unwrap();
    recorder.write(&sample(2., 2., 10., false, false)).unwrap();
    let path = recorder.path.clone();
    drop(recorder);
    use std::io::Write;
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(b"{\"type\":\"end\",\"timestamp\":0}\n")
        .unwrap();
    let compressed = gzip.finish().unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&compressed[..compressed.len() / 2])
        .unwrap();
    let mut ledger = Ledger::default();
    assert_eq!(record::read(&path, |r| ledger.apply(&r)).unwrap(), 2);
    assert_eq!(ledger.samples, 1);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn dashboard_renders_in_small_and_large_terminals() {
    use ratatui::{Terminal, backend::TestBackend};
    for (w, h) in [(40, 10), (80, 24), (160, 48)] {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut ledger = Ledger::default();
        ledger.apply(&sample(2., 2., 10., false, false));
        terminal
            .draw(|f| {
                crate::ui::draw(
                    f,
                    &ledger,
                    std::path::Path::new("test.jsonl"),
                    &mut Default::default(),
                )
            })
            .unwrap();
    }
}
