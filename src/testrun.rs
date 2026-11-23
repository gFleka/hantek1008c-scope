//! Test runs (Phase 4): analyse every capture of the analysed channels for a while (injector test, coil test, crank /
//! wheel sensor test, …) and report events, failures (failed injections, misfires, irregular teeth), verdict counts
//! and min / mean / max of the readouts, with the share of time actually checked. Child module of `ui`.

use super::*;
use crate::analyze::{analyze_with, Analysis, Kind, Tag, KINDS};
use std::collections::BTreeMap;
use std::time::SystemTime;

#[derive(Default)]
struct ChanStats {
    kind: Option<Kind>,
    captures: u64,
    ok: u64,
    fail: u64,
    /// failure reason (verdict text without numbers) → count
    reasons: BTreeMap<String, u64>,
    events: u64,
    failures: u64,
    /// captures without a signal of this type (not judged)
    nosignal: u64,
    /// readout name → (min, sum, max, n, unit)
    readouts: BTreeMap<String, (f64, f64, f64, u64, String)>,
}

pub(super) struct TestRun {
    pub(super) duration_s: f32,
    started: Option<(Instant, SystemTime)>,
    seq: u64,
    last_stream: Option<Instant>,
    captures: u64,
    covered_s: f64,
    chans: [ChanStats; CHANNELS],
    report: Option<String>,
}

impl Default for TestRun {
    fn default() -> Self {
        TestRun {
            duration_s: 60.0,
            started: None,
            seq: 0,
            last_stream: None,
            captures: 0,
            covered_s: 0.0,
            chans: Default::default(),
            report: None,
        }
    }
}

/// What the events / failures of a signal type are called.
fn event_names(kind: Kind) -> Option<(&'static str, &'static str)> {
    match kind {
        Kind::Injector => Some(("injections", "failed injections (no flyback spike)")),
        Kind::IgnitionPrimary => Some(("dwell periods", "misfires (no burn line)")),
        Kind::CrankVr | Kind::WheelSpeed => {
            Some(("captures with teeth", "irregular / missing teeth"))
        }
        Kind::PulseHigh | Kind::PulseLow => Some(("pulses", "—")),
        _ => None,
    }
}

/// Events and failures in one analysis (complete events only; ones cut by the capture edge are left out).
fn count_events(kind: Kind, a: &Analysis, n: usize) -> (u64, u64) {
    let whole = |p: &crate::analyze::Phase| p.s0 > 0 && p.s1 < n;
    let active: Vec<_> = a
        .phases
        .iter()
        .filter(|p| p.tag == Tag::Active && whole(p))
        .collect();
    match kind {
        Kind::Injector => {
            let fails = active
                .iter()
                .filter(|o| !a.phases.iter().any(|p| p.tag == Tag::Spike && p.s0 == o.s1))
                .count();
            (active.len() as u64, fails as u64)
        }
        Kind::IgnitionPrimary => {
            // only dwells whose burn window (≤ 6 ms) is inside the capture; burn right after the dwell / spike
            let win = (6e-3 / a.dt.max(1e-12)) as usize;
            let judged: Vec<_> = active.iter().filter(|d| d.s1 + win < n).collect();
            let fails = judged
                .iter()
                .filter(|d| {
                    !a.phases
                        .iter()
                        .any(|p| p.tag == Tag::Burn && p.s0 >= d.s1 && p.s0 <= d.s1 + win)
                })
                .count();
            (judged.len() as u64, fails as u64)
        }
        Kind::CrankVr | Kind::WheelSpeed => (
            1,
            a.phases.iter().filter(|p| p.tag == Tag::Warn).count() as u64,
        ),
        Kind::PulseHigh | Kind::PulseLow => (active.len() as u64, 0),
        _ => (0, 0),
    }
}

impl App {
    /// The last test run's report (Markdown), if any (tests).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn test_report(&self) -> Option<&str> {
        self.test.report.as_deref()
    }

    pub(super) fn test_running(&self) -> bool {
        self.test.started.is_some()
    }

    pub(super) fn test_start(&mut self) {
        let d = self.test.duration_s;
        self.test = TestRun {
            duration_s: d,
            started: Some((Instant::now(), SystemTime::now())),
            seq: self.last_seq,
            ..Default::default()
        };
        if self.run != Run::Running {
            self.set_run(Run::Running);
        }
        self.test_shown_once = false; // this test's report opens by itself when it ends
    }

    pub(super) fn test_finish(&mut self) {
        let Some((t0, wall)) = self.test.started.take() else {
            return;
        };
        let elapsed = t0.elapsed().as_secs_f64();
        let t = &self.test;
        let mut md = format!(
            "# Test run report\n\n- started: {}\n- duration: {}\n- captures analysed: {}\n- time actually checked: {:.0} % (captures have gaps between them; events in the gaps are not seen)\n\n",
            crate::rec::utc(wall),
            fmt_time(elapsed),
            t.captures,
            (t.covered_s / elapsed.max(1e-9) * 100.0).min(100.0)
        );
        for (ch, c) in t.chans.iter().enumerate() {
            let Some(kind) = c.kind else { continue };
            let name = KINDS.iter().find(|k| k.0 == kind).map_or("", |k| k.1);
            md += &format!("## CH{} — {name}\n\n", ch + 1);
            md += &format!(
                "- captures: {} — OK {} · CHECK {}\n",
                c.captures, c.ok, c.fail
            );
            if c.nosignal > 0 {
                md += &format!("- captures without a signal (not judged): {}\n", c.nosignal);
            }
            if let Some((ev, fl)) = event_names(kind) {
                md += &format!("- {ev}: {}\n", c.events);
                if fl != "—" {
                    md += &format!(
                        "- **{fl}: {}**{}\n",
                        c.failures,
                        if c.events > 0 && kind != Kind::CrankVr && kind != Kind::WheelSpeed {
                            format!(" ({:.2} %)", c.failures as f64 / c.events as f64 * 100.0)
                        } else {
                            String::new()
                        }
                    );
                }
            }
            if !c.readouts.is_empty() {
                md += "\n| reading | min | mean | max |\n|---|---|---|---|\n";
                for (k, (mn, sum, mx, n, unit)) in &c.readouts {
                    md += &format!(
                        "| {k} | {mn:.2} {unit} | {:.2} {unit} | {mx:.2} {unit} |\n",
                        sum / *n as f64
                    );
                }
            }
            if !c.reasons.is_empty() {
                md += "\nReasons for CHECK:\n";
                for (r, n) in &c.reasons {
                    md += &format!("- {n} × {r}\n");
                }
            }
            md += "\n";
        }
        if t.chans.iter().all(|c| c.kind.is_none()) {
            md +=
                "No channel had a signal type set — choose one in Automotive tests (side panel).\n";
        }
        self.test.report = Some(md);
    }

    /// Every new capture during a test run: analyse the analysed channels and accumulate.
    pub(super) fn test_tick(&mut self, snap: &Snapshot) {
        let Some((t0, _)) = self.test.started else {
            return;
        };
        if t0.elapsed().as_secs_f32() >= self.test.duration_s {
            self.test_finish();
            return;
        }
        // not a new capture, stopped, or one taken with the settings from before a change
        if snap.frame.seq == self.test.seq
            || self.run != Run::Running
            || (!snap.roll
                && snap
                    .cfg
                    .as_ref()
                    .is_some_and(|c| *c != self.wanted_config(snap)))
        {
            return;
        }
        // streams: once per screen, so overlapping windows aren't counted twice
        if snap.roll
            && self
                .test
                .last_stream
                .is_some_and(|t| t.elapsed().as_secs_f64() < self.span_s())
        {
            return;
        }
        self.test.seq = snap.frame.seq;
        if snap.roll {
            self.test.last_stream = Some(Instant::now());
        }
        let wheel = self.engine.wheel();
        let mut covered = 0.0f64;
        for ch in 0..CHANNELS {
            let kind = self.kinds[ch];
            if kind == Kind::None || !self.on[ch] {
                continue;
            }
            let Some(x) = self.units_of(snap, ch) else {
                continue;
            };
            let dt = self.sample_dt(snap, x.len());
            // a different signal type mid-run: start this channel's statistics over (no mix of two analysers)
            if self.test.chans[ch].kind.is_some_and(|k| k != kind) {
                self.test.chans[ch] = ChanStats::default();
            }
            // no signal of this type: counted as such, not judged (an open input would pile up CHECKs)
            let noise = self.noise_floor(snap, ch);
            if crate::analyze::waiting_for(kind, &x, noise, &self.probes[ch].unit).is_some() {
                let c = &mut self.test.chans[ch];
                (c.kind, c.nosignal) = (Some(kind), c.nosignal + 1);
                continue;
            }
            covered = covered.max(x.len() as f64 * dt);
            let a = analyze_with(kind, &x, dt, wheel);
            let (ev, fl) = count_events(kind, &a, x.len());
            let c = &mut self.test.chans[ch];
            c.kind = Some(kind);
            c.captures += 1;
            c.events += ev;
            c.failures += fl;
            match &a.verdict {
                Some((true, _)) => c.ok += 1,
                Some((false, why)) => {
                    c.fail += 1;
                    let key: String = why
                        .chars()
                        .filter(|ch| !ch.is_ascii_digit() && *ch != '.')
                        .collect();
                    *c.reasons
                        .entry(key.split_whitespace().collect::<Vec<_>>().join(" "))
                        .or_insert(0) += 1;
                }
                None => {}
            }
            for (k, v) in &a.readouts {
                let mut it = v.split_whitespace();
                let Some(num) = it.next().and_then(|n| n.parse::<f64>().ok()) else {
                    continue;
                };
                let unit = it.next().unwrap_or("").to_string();
                let e = c
                    .readouts
                    .entry(k.clone())
                    .or_insert((f64::MAX, 0.0, f64::MIN, 0, unit));
                (e.0, e.1, e.2, e.3) = (e.0.min(num), e.1 + num, e.2.max(num), e.3 + 1);
            }
        }
        self.test.captures += 1;
        self.test.covered_s += covered;
    }

    /// Controls in the Automotive tests section.
    pub(super) fn test_controls(&mut self, ui: &mut egui::Ui) {
        let any = self.kinds.iter().any(|k| *k != Kind::None);
        ui.horizontal(|ui| {
            if let Some((t0, _)) = self.test.started {
                let left = (self.test.duration_s - t0.elapsed().as_secs_f32()).max(0.0);
                if help(ui.button(format!("Stop test ({left:.0} s left)")), "End the test run now and show the report.").clicked() {
                    self.test_finish();
                }
            } else {
                let b = ui.add_enabled(any, egui::Button::new("Start test run"));
                if help(b, "Analyse every capture of the analysed channels for the set time (e.g. injector or coil test): counts failed injections / misfires / irregular teeth, the readings' min / mean / max and the share of time actually checked; a report at the end (save as a file).").on_disabled_hover_text("Choose a signal type for a channel first.").clicked() {
                    self.test_start();
                }
                help(ui.add(egui::DragValue::new(&mut self.test.duration_s).range(5.0..=3600.0).speed(1.0).suffix(" s")), "How long the test runs.");
                if self.test.report.is_some() && help(ui.small_button("last report"), "Show the last test report again.").clicked() {
                    self.test_show = true;
                }
            }
        });
    }

    /// Info-line badge text while a test runs.
    pub(super) fn test_badge(&self) -> Option<String> {
        let (t0, _) = self.test.started?;
        let check: u64 = self.test.chans.iter().map(|c| c.fail).sum();
        let failed: u64 = self.test.chans.iter().map(|c| c.failures).sum();
        Some(format!(
            "TEST {:.0}/{:.0} s · {} captures · {check} CHECK · {failed} failed events",
            t0.elapsed().as_secs_f32(),
            self.test.duration_s,
            self.test.captures
        ))
    }

    /// Report window (opens when a test ends).
    pub(super) fn test_window(&mut self, ctx: &egui::Context) {
        // debug aid like HANTEK_SHOT: HANTEK_TEST=<seconds> starts a test run at launch
        static ENV: std::sync::Once = std::sync::Once::new();
        let mut env = None;
        ENV.call_once(|| {
            env = std::env::var("HANTEK_TEST")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
        });
        if let Some(s) = env {
            self.test.duration_s = s;
            self.test_start();
        }
        if self.test.report.is_some() && self.test.started.is_none() && !self.test_shown_once {
            self.test_show = true;
            self.test_shown_once = true;
        }
        if !self.test_show {
            return;
        }
        let Some(md) = self.test.report.clone() else {
            return;
        };
        let mut open = true;
        let mut save = false;
        egui::Window::new("Test run report")
            .open(&mut open)
            .default_width(520.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(ctx.content_rect().height() * 0.6)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(&md).monospace());
                    });
                save = ui.button("Save report (.md)").clicked();
            });
        if save {
            let dir = self.files.dir();
            let p = dir.join(format!("test_{}.md", crate::rec::stamp(SystemTime::now())));
            match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&p, &md)) {
                Ok(()) => {
                    self.files_msg(Level::Info, format!("Test report saved: {}", p.display()))
                }
                Err(e) => self.files_msg(Level::Warn, format!("Test report: {e}")),
            }
        }
        self.test_show = open;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injector_events_and_failed_injections() {
        // two complete openings, the second without a flyback spike
        let dt = 1e-5;
        let x: Vec<f32> = (0..4000)
            .map(|i| {
                let t = i as f64 * dt;
                if (0.005..0.008).contains(&t) || (0.025..0.028).contains(&t) {
                    0.4
                } else if (0.008..0.0082).contains(&t) {
                    70.0
                } else {
                    13.8
                }
            })
            .collect();
        let a = analyze_with(Kind::Injector, &x, dt, (60, 2));
        assert_eq!(count_events(Kind::Injector, &a, x.len()), (2, 1));
    }
}
