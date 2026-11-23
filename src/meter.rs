//! Meter mode (Phase 4): a multimeter-style view — per shown channel a big DC average in its unit, min / max since
//! reset, RMS, frequency, duty and a small trend line of the last minutes. Readable from a distance (engine bay,
//! passenger seat). Same acquisition, measurements and units as the scope view. Child module of `ui`.

use super::*;
use std::collections::VecDeque;

/// Trend points kept per channel (one per update; updates ≤ 4/s → ≈ 5 min).
const TREND_N: usize = 1200;

#[derive(Default)]
pub(super) struct Meter {
    pub(super) on: bool,
    min: [Option<f32>; CHANNELS],
    max: [Option<f32>; CHANNELS],
    last: [Option<Meas>; CHANNELS],
    trend: [VecDeque<f32>; CHANNELS],
    seq: u64,
    at: Option<Instant>,
    /// meter mode switched a slow capture timebase to streaming (switched back when leaving meter mode)
    pub(super) forced_stream: bool,
}

/// Stream readings cover the last this many seconds (frequency down to ~1 Hz, quick to react).
const STREAM_WINDOW_S: f64 = 2.0;

impl App {
    /// Feed the meter from the snapshot on screen (≤ 4 updates/s so the digits are readable).
    fn meter_update(&mut self, snap: &Snapshot) {
        let due = self
            .meter
            .at
            .is_none_or(|t| t.elapsed().as_secs_f32() >= 0.25);
        if snap.frame.seq == self.meter.seq || !due {
            return;
        }
        let Some(cfg) = snap.cfg.as_ref() else { return };
        self.meter.seq = snap.frame.seq;
        self.meter.at = Some(Instant::now());
        for ch in 0..CHANNELS {
            let full = &snap.frame.data[ch];
            // stream: only the newest STREAM_WINDOW_S (the screen can hold minutes)
            let keep = if snap.roll {
                ((self.shown_rate * STREAM_WINDOW_S) as usize).max(8)
            } else {
                full.len()
            };
            let d = &full[full.len().saturating_sub(keep)..];
            if !self.on[ch] || !snap.frame.active[ch] || d.len() < 8 {
                self.meter.last[ch] = None;
                continue;
            }
            let vs = cfg.vscale[ch];
            let (zu, vu) = self.uconv(ch, vs, snap.zero[vs.index()][ch]);
            let dt = if snap.roll {
                1.0 / self.shown_rate.max(1e-9)
            } else {
                capture_seconds(cfg.timebase_id) / d.len() as f64
            };
            let m = measure(d, zu, vu, Some(dt));
            let mm = &mut self.meter;
            mm.min[ch] = Some(mm.min[ch].map_or(m.min, |v| v.min(m.min)));
            mm.max[ch] = Some(mm.max[ch].map_or(m.max, |v| v.max(m.max)));
            mm.trend[ch].push_back(m.mean);
            if mm.trend[ch].len() > TREND_N {
                mm.trend[ch].pop_front();
            }
            mm.last[ch] = Some(m);
        }
    }

    /// The meter view (replaces the plots while meter mode is on).
    pub(super) fn meter_view(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        self.meter_update(snap);
        let shown: Vec<usize> = (0..CHANNELS).filter(|&c| self.on[c]).collect();
        let avail = ui.available_size();
        let cols = if shown.len() <= 1 {
            1
        } else if shown.len() <= 4 {
            2
        } else {
            3
        };
        let rows = shown.len().div_ceil(cols).max(1);
        let tile = Vec2::new(
            avail.x / cols as f32 - 8.0,
            (avail.y - 34.0) / rows as f32 - 8.0,
        );
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Meter").strong());
            if help(ui.button("Reset min / max"), "Start min / max and the trend again from now.").clicked() {
                let on = self.meter.on;
                self.meter = Meter { on, ..Default::default() };
            }
            let what = if snap.roll {
                format!("streaming: each reading = last {STREAM_WINDOW_S:.0} s, 4× per second")
            } else {
                let cap = snap.cfg.as_ref().map_or(0.0, |c| capture_seconds(c.timebase_id));
                if cap > 1.0 {
                    format!("one reading per capture ({}) — a faster timebase or a stream rate updates more often", fmt_time(cap))
                } else {
                    "each reading = one capture, up to 4× per second".to_string()
                }
            };
            ui.label(egui::RichText::new(format!("M = back to the scope view · {what}")).weak());
        });
        egui::Grid::new("meter").spacing([8.0, 8.0]).show(ui, |ui| {
            for (k, &ch) in shown.iter().enumerate() {
                let (r, _) = ui.allocate_exact_size(tile, Sense::hover());
                self.meter_tile(ui.painter(), r, ch);
                if (k + 1) % cols == 0 {
                    ui.end_row();
                }
            }
        });
    }

    fn meter_tile(&self, p: &egui::Painter, r: Rect, ch: usize) {
        let unit = self.unit(ch);
        let col = COLORS[ch];
        p.rect_filled(r, 6.0, Color32::from_rgb(16, 18, 22));
        p.rect_stroke(
            r,
            6.0,
            Stroke::new(1.0, col.gamma_multiply(0.4)),
            egui::StrokeKind::Inside,
        );
        p.text(
            r.left_top() + Vec2::new(10.0, 6.0),
            Align2::LEFT_TOP,
            format!("CH{}", ch + 1),
            FontId::proportional(18.0),
            col,
        );
        let Some(m) = self.meter.last[ch] else {
            p.text(
                r.center(),
                Align2::CENTER_CENTER,
                "--",
                FontId::monospace(40.0),
                Color32::GRAY,
            );
            return;
        };
        let main = fmt_u(m.mean, unit);
        // monospace glyphs are ~0.6 em wide: fit the tile's width as well as its height
        let big = (r.height() * 0.28)
            .min((r.width() - 24.0) / (main.chars().count() as f32 * 0.62))
            .clamp(14.0, 96.0);
        p.text(
            Pos2::new(r.right() - 12.0, r.top() + r.height() * 0.36),
            Align2::RIGHT_CENTER,
            main,
            FontId::monospace(big),
            col,
        );
        let small = FontId::monospace((big * 0.32).clamp(12.0, 22.0));
        let line = |i: f32, t: String| {
            p.text(
                Pos2::new(
                    r.left() + 12.0,
                    r.top() + r.height() * 0.58 + i * small.size * 1.3,
                ),
                Align2::LEFT_CENTER,
                t,
                small.clone(),
                Color32::from_gray(200),
            );
        };
        let (lo, hi) = (
            self.meter.min[ch].unwrap_or(m.min),
            self.meter.max[ch].unwrap_or(m.max),
        );
        line(
            0.0,
            format!("min {}   max {}", fmt_u(lo, unit), fmt_u(hi, unit)),
        );
        // AC part RMS (ripple): sqrt(RMS² − mean²)
        let ac = (m.rms * m.rms - m.mean * m.mean).max(0.0).sqrt();
        line(
            1.0,
            format!("RMS {}   AC {}", fmt_u(m.rms, unit), fmt_u(ac, unit)),
        );
        line(
            2.0,
            match (m.freq, m.duty) {
                (Some(f), Some(d)) => format!("{}   duty {:.1} %", fmt_freq(f), d * 100.0),
                (Some(f), None) => fmt_freq(f),
                _ => "no frequency (DC / too slow)".into(),
            },
        );
        // trend of the average: last minutes, own scale, along the bottom of the tile
        let t = &self.meter.trend[ch];
        if t.len() > 1 {
            let tr = Rect::from_min_max(
                Pos2::new(r.left() + 10.0, r.bottom() - r.height() * 0.14 - 4.0),
                Pos2::new(r.right() - 10.0, r.bottom() - 6.0),
            );
            let (a, b) = t
                .iter()
                .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
            let span = (b - a).max(1e-6);
            let pts: Vec<Pos2> = t
                .iter()
                .enumerate()
                .map(|(i, &v)| {
                    Pos2::new(
                        tr.left() + i as f32 / (TREND_N - 1) as f32 * tr.width(),
                        tr.bottom() - (v - a) / span * tr.height(),
                    )
                })
                .collect();
            p.add(Shape::line(pts, Stroke::new(1.2, col.gamma_multiply(0.7))));
        }
    }
}
