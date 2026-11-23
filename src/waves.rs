//! Waveform phase analysis on screen (Phase 4): per channel a signal type (side panel / guided tests); the analysis
//! (`crate::analyze`, pure + tested) is drawn as tinted phase bands with labels over the trace and a readout box with
//! a verdict. Child module of `ui`.

use super::*;
use crate::analyze::{
    analyze_with, bands, model, waiting_for, waiting_hint, Analysis, Hint, Kind, Tag, KINDS,
};

/// Analysis of one channel across captures: run once per new capture (not per frame), with a memory so the display
/// is calm — 'waiting for a signal' until 3 captures in a row have one (and back), a new verdict only after it repeats
/// 3 times, readout numbers at most 4× per second.
#[derive(Default)]
pub(super) struct ChanAna {
    kind: Option<Kind>,
    key: (u64, bool, Option<u64>, usize, u64),
    at: Option<Instant>,
    pub(super) present: bool,
    flips: u8,
    pub(super) waiting: Option<String>,
    /// latest analysis (phases belong to the capture on screen)
    res: Analysis,
    pub(super) verdict: Option<(bool, String)>,
    cand: Option<(bool, String)>,
    cand_n: u8,
    readouts: Vec<(String, String)>,
    readouts_at: Option<Instant>,
    /// min / max of the latest capture (signal units), for 'Fit to signal'
    pub(super) lohi: (f32, f32),
}

impl ChanAna {
    /// Readout values as shown (held), for screenshots.
    pub(super) fn readouts(&self) -> &[(String, String)] {
        if self.present {
            &self.readouts
        } else {
            &[]
        }
    }

    /// One new analysis: presence flips after 3 captures in a row (at once when browsing / stopped), a different verdict
    /// shows after it came 3 times in a row, numbers refresh at most every 0.25 s.
    fn step(&mut self, waiting: Option<String>, res: Analysis, browsing: bool, now: Instant) {
        let a = self;
        if waiting.is_none() != a.present {
            a.flips += 1;
            if a.flips >= 3 || browsing {
                a.present = !a.present;
                (a.flips, a.verdict, a.cand, a.cand_n) = (0, None, None, 0);
                a.readouts.clear();
                a.readouts_at = None;
            }
        } else {
            a.flips = 0;
        }
        if waiting.is_some() {
            a.waiting = waiting;
            if a.present {
                // no signal in this capture but not flipped yet (needs 3 in a row): keep the last results, no bands
                a.res.phases.clear();
                return;
            }
        }
        a.res = res;
        if !a.present {
            return;
        }
        let refresh = browsing
            || a.readouts_at
                .is_none_or(|t| now.duration_since(t).as_secs_f32() >= 0.25);
        // the box shows one analysis: its verdict and its numbers together (numbers from a capture with another
        // verdict would contradict the verdict, e.g. 'no dwell found' next to 'OK: dwell, spike and burn')
        let new = verdict_key(&a.res.verdict);
        let take = if a.verdict.is_none() || browsing {
            true
        } else if new == verdict_key(&a.verdict) {
            // the shown verdict came again: a pending different one has to start over (3 in a row)
            (a.cand, a.cand_n) = (None, 0);
            refresh // same verdict: fresher numbers, at most 4× per second
        } else if new == verdict_key(&a.cand) {
            a.cand_n += 1;
            a.cand_n >= 2
        } else {
            (a.cand, a.cand_n) = (a.res.verdict.clone(), 0);
            false
        };
        if take {
            a.verdict = a.res.verdict.clone();
            a.readouts = a.res.readouts.clone();
            (a.readouts_at, a.cand_n) = (Some(now), 0);
        }
    }
}

fn verdict_key(v: &Option<(bool, String)>) -> Option<(bool, String)> {
    v.as_ref().map(|(ok, t)| {
        (
            *ok,
            t.chars()
                .filter(|c| !c.is_ascii_digit() && *c != '.')
                .collect(),
        )
    })
}

fn tag_color(t: Tag) -> Color32 {
    match t {
        Tag::Active => Color32::from_rgb(90, 170, 255),
        Tag::Spike => Color32::from_rgb(255, 90, 90),
        Tag::Burn => Color32::from_rgb(255, 170, 60),
        Tag::Ringing => Color32::from_rgb(200, 140, 255),
        Tag::Event => Color32::from_rgb(255, 255, 120),
        Tag::Gap => Color32::from_rgb(120, 230, 160),
        Tag::Rich => Color32::from_rgb(255, 120, 90),
        Tag::Lean => Color32::from_rgb(90, 160, 255),
        Tag::Dip => Color32::from_rgb(255, 120, 120),
        Tag::Hump => Color32::from_rgb(140, 220, 255),
        Tag::Warn => Color32::from_rgb(255, 60, 60),
    }
}

pub(super) fn kind_index(k: Kind) -> usize {
    KINDS.iter().position(|x| x.0 == k).unwrap_or(0)
}

impl App {
    /// The hardware noise floor of a channel in its unit: an open input shows a few counts of noise, 20 counts on its
    /// range is the floor (as for the info line's 'no signal').
    pub(super) fn noise_floor(&self, snap: &Snapshot, ch: usize) -> f32 {
        snap.cfg.as_ref().map_or(0.0, |c| {
            let vs = c.vscale[ch];
            (20.0 * self.uconv(ch, vs, snap.zero[vs.index()][ch]).1).abs()
        })
    }

    /// Once per new capture (streams ≤ 10×/s): analyse the analysed channels and update their calm state.
    pub(super) fn analysis_tick(&mut self, snap: &Snapshot) {
        let wheel = self.engine.wheel();
        let browsing = self.view.is_some() || self.run != Run::Running;
        // live captures still in flight from before a settings change (e.g. the previous guided test's range) are
        // not judged: their numbers would flash wrong values (browsing the history judges what is shown)
        let stale = !browsing
            && !snap.roll
            && snap
                .cfg
                .as_ref()
                .is_some_and(|c| *c != self.wanted_config(snap));
        for ch in 0..CHANNELS {
            let kind = self.kinds[ch];
            if kind == Kind::None || !self.on[ch] {
                if self.ana[ch].kind.is_some() {
                    self.ana[ch] = ChanAna::default();
                }
                continue;
            }
            if stale {
                continue;
            }
            // key first (cheap): the capture, and what changes its meaning — probe scale / offset / unit, trigger wheel
            let extra = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                let p = &self.probes[ch];
                (p.scale.to_bits(), p.offset.to_bits(), &p.unit, wheel).hash(&mut h);
                h.finish()
            };
            let key = (
                snap.frame.seq,
                snap.roll,
                self.view,
                snap.frame.data[ch].len(),
                extra,
            );
            let same_kind = self.ana[ch].kind == Some(kind);
            if same_kind
                && (self.ana[ch].key == key
                    || (snap.roll
                        && self.ana[ch]
                            .at
                            .is_some_and(|t| t.elapsed().as_secs_f32() < 0.1)))
            {
                continue;
            }
            let Some(x) = self.units_of(snap, ch) else {
                continue;
            };
            let dt = self.sample_dt(snap, x.len());
            let noise = self.noise_floor(snap, ch);
            let waiting = waiting_for(kind, &x, noise, &self.probes[ch].unit);
            let res = analyze_with(kind, &x, dt, wheel);
            let lohi = x
                .iter()
                .fold((f32::MAX, f32::MIN), |(l, h), &v| (l.min(v), h.max(v)));
            let a = &mut self.ana[ch];
            if !same_kind {
                *a = ChanAna {
                    kind: Some(kind),
                    present: waiting.is_none(),
                    ..Default::default()
                };
            }
            (a.key, a.at, a.lohi) = (key, Some(Instant::now()), lohi);
            a.step(waiting, res, browsing, Instant::now());
        }
    }

    /// Phase bands + readouts for the analysed channels of this plot.
    pub(super) fn analysis_overlay(
        &self,
        painter: &egui::Painter,
        r: Rect,
        vr: Rect,
        pane: usize,
        snap: &Snapshot,
    ) {
        let mut row = 0.0;
        let mut strip_k = 0.0; // one strip row per analysed channel
                               // readout boxes start under all strip + label rows of this plot (they used to cover the second label row)
        let n_strips = (0..CHANNELS)
            .filter(|&c| self.kinds[c] != Kind::None && self.on[c] && self.pane_for(c) == pane)
            .count() as f32;
        let box_top = r.top() + 34.0 + n_strips * 32.0 + 4.0;
        let mut thumb_k = 0.0; // one 'typical' thumbnail per analysed channel with a model
        let mut typical_label = false; // the 'typical (dashed)' note once per plot
        for ch in 0..CHANNELS {
            let kind = self.kinds[ch];
            if kind == Kind::None || !self.on[ch] || self.pane_for(ch) != pane {
                continue;
            }
            // no conversion of the whole channel per frame: min / max come from the analysis state (speed review)
            if snap.frame.data[ch].len() < 2 || !snap.frame.active[ch] {
                continue;
            }
            let Some((a, b)) = self.x_map(snap, ch, vr) else {
                continue;
            };
            let st = &self.ana[ch];
            let res = &st.res;
            let name = KINDS[kind_index(kind)].1;
            let font = FontId::monospace(11.0);
            if !st.present {
                // nothing to analyse yet: one calm box instead of results computed from noise
                let lines: Vec<(String, Color32)> = vec![
                    (format!("CH{} {name}", ch + 1), COLORS[ch]),
                    (
                        "waiting for a signal".into(),
                        Color32::from_rgb(255, 200, 90),
                    ),
                    (
                        st.waiting.clone().unwrap_or_default(),
                        Color32::from_gray(200),
                    ),
                    (waiting_hint(kind).into(), Color32::from_rgb(170, 240, 255)),
                ];
                let w = lines
                    .iter()
                    .map(|l| l.0.chars().count())
                    .max()
                    .unwrap_or(10) as f32
                    * 6.7
                    + 12.0;
                let bx = Rect::from_min_size(
                    Pos2::new(r.right() - w - 30.0, box_top + row * 14.0),
                    Vec2::new(w, lines.len() as f32 * 14.0 + 6.0),
                );
                painter.rect_filled(bx, 4.0, Color32::from_black_alpha(170));
                for (i, (t, c)) in lines.iter().enumerate() {
                    painter.text(
                        Pos2::new(bx.left() + 6.0, bx.top() + 3.0 + i as f32 * 14.0),
                        Align2::LEFT_TOP,
                        t,
                        font.clone(),
                        *c,
                    );
                }
                row += lines.len() as f32 + 1.0;
                continue;
            }
            // built-in typical waveform: trigger-aligned shape (captures), or limit lines (any mode)
            if self.typical_on {
                let (y0, dyv) = (
                    vr.center().y - self.offset[ch] * vr.height() / DIV_Y,
                    vr.height() / DIV_Y,
                );
                let ud = self.udiv(ch);
                let y_of = |v: f32| y0 - v / ud * dyv;
                // light cyan: no channel colour, visible on top of a trace that matches it
                let dash = Stroke::new(1.5, Color32::from_rgba_unmultiplied(170, 240, 255, 190));
                for &(v, label) in bands(kind) {
                    let y = y_of(v);
                    if (r.top()..=r.bottom()).contains(&y) {
                        painter.extend(Shape::dashed_line(
                            &[Pos2::new(r.left(), y), Pos2::new(r.right(), y)],
                            dash,
                            6.0,
                            4.0,
                        ));
                        painter.text(
                            Pos2::new(r.left() + 4.0, y - 1.0),
                            Align2::LEFT_BOTTOM,
                            label,
                            FontId::monospace(10.0),
                            Color32::from_white_alpha(140),
                        );
                    }
                }
                if !snap.roll {
                    let num = |name: &str| {
                        res.readouts
                            .iter()
                            .find(|r| r.0 == name)
                            .and_then(|r| r.1.split_whitespace().next()?.parse::<f64>().ok())
                    };
                    let x_trig = vr.left() + vr.width() * self.trig_pos as f32 / 1000.0;
                    let s_per_px = self.span_s() / vr.width() as f64;
                    let wheel = self.engine.wheel();
                    let (lo, hi) = self.ana[ch].lohi;
                    let h = Hint {
                        period: match kind {
                            Kind::CrankVr => num("rpm")
                                .filter(|&r| r > 0.0)
                                .map(|r| 60.0 / (r * wheel.0.max(1) as f64)),
                            _ => num("frequency").filter(|&f| f > 0.0).map(|f| 1.0 / f),
                        },
                        lo,
                        hi,
                        // first tooth after the gap, in seconds from the trigger point
                        gap_t: res
                            .phases
                            .iter()
                            .find(|p| p.tag == Tag::Gap)
                            .map(|p| (a + b * p.s1 as f32 - x_trig) as f64 * s_per_px),
                        wheel,
                    };
                    let pts: Vec<Pos2> = (r.left() as i32..r.right() as i32)
                        .filter_map(|px| {
                            let v = model(kind, (px as f32 - x_trig) as f64 * s_per_px, &h)?;
                            Some(Pos2::new(px as f32, y_of(v).clamp(r.top(), r.bottom())))
                        })
                        .collect();
                    // thumbnail: the healthy shape at its own time scale, always readable (the overlay hides under a matching trace)
                    let span = match kind {
                        Kind::Injector | Kind::IgnitionPrimary => Some((-1.0e-3, 7.0e-3)),
                        Kind::CrankVr => h.period.map(|p| (-p * 10.0, p * 14.0)), // the teeth around the gap
                        Kind::PulseHigh | Kind::PulseLow | Kind::WheelSpeed => {
                            h.period.map(|p| (-p * 0.25, p * 2.75))
                        }
                        _ => None,
                    };
                    if let Some((t0, t1)) = span {
                        let hc = Hint {
                            gap_t: Some(0.0),
                            ..h
                        };
                        let n = 160;
                        let v: Vec<f32> = (0..n)
                            .filter_map(|i| {
                                model(kind, t0 + (t1 - t0) * i as f64 / (n - 1) as f64, &hc)
                            })
                            .collect();
                        if v.len() == n {
                            let (vl, vh) = v
                                .iter()
                                .fold((f32::MAX, f32::MIN), |(l, h), &x| (l.min(x), h.max(x)));
                            // side by side along the bottom, right to left (stacking upwards ran into the readouts)
                            let tb = Rect::from_min_size(
                                Pos2::new(r.right() - 250.0 - thumb_k * 176.0, r.bottom() - 112.0),
                                egui::vec2(170.0, 62.0),
                            );
                            thumb_k += 1.0;
                            painter.rect_filled(tb, 3.0, Color32::from_black_alpha(190));
                            painter.text(
                                tb.left_top() + egui::vec2(4.0, 2.0),
                                Align2::LEFT_TOP,
                                format!("CH{} typical", ch + 1),
                                FontId::monospace(10.0),
                                Color32::from_rgb(170, 240, 255),
                            );
                            let inner =
                                tb.shrink2(egui::vec2(6.0, 8.0)).with_min_y(tb.top() + 16.0);
                            let pts: Vec<Pos2> = v
                                .iter()
                                .enumerate()
                                .map(|(i, &x)| {
                                    Pos2::new(
                                        inner.left() + inner.width() * i as f32 / (n - 1) as f32,
                                        inner.bottom()
                                            - (x - vl) / (vh - vl).max(1e-6) * inner.height(),
                                    )
                                })
                                .collect();
                            painter.add(Shape::line(
                                pts,
                                Stroke::new(1.2, Color32::from_rgb(170, 240, 255)),
                            ));
                        }
                    }
                    if pts.len() > 1 {
                        painter.extend(Shape::dashed_line(&pts, dash, 3.0, 3.0));
                        if !typical_label {
                            typical_label = true;
                            painter.text(
                                Pos2::new(x_trig + 4.0, r.bottom() - 34.0),
                                Align2::LEFT_BOTTOM,
                                "typical (dashed)",
                                FontId::monospace(10.0),
                                Color32::from_rgb(170, 240, 255),
                            );
                        }
                    }
                }
            }
            // bands: a faint tint over the trace and a strip under the top edge (switchable; readouts stay). Every strip
            // gets its label: inside when it fits, else right after the strip when that space is free, else on a second
            // label row with a tick; hovering a strip shows its label and duration (short phases: spike, gap …).
            let strip_y = r.top() + 34.0 + strip_k * 32.0; // strip row + a second label row per analysed channel
            strip_k += 1.0;
            if self.highlight_on {
                const CW: f32 = 6.2; // width of a 10 px monospace glyph
                                     // phases in time order; neighbours of the same kind closer than a pixel are merged into one strip
                                     // (a full stream screen has tens of thousands of phases: drawing each one cost ~20 ms per frame —
                                     // speed review 2026-11-23)
                let mut strips: Vec<(Rect, &str, Color32, f64)> = Vec::new();
                let mut order: Vec<&crate::analyze::Phase> = res.phases.iter().collect();
                order.sort_by_key(|p| p.s0);
                for p in order {
                    let (x0, x1) = (
                        (a + b * p.s0 as f32).max(r.left()),
                        (a + b * p.s1 as f32).min(r.right()),
                    );
                    if x1 <= x0 {
                        continue;
                    }
                    let c = tag_color(p.tag);
                    let dur = p.s1.saturating_sub(p.s0) as f64 * res.dt;
                    // same kind touching, or a sub-pixel phase in a pixel column that already has a strip: one strip
                    // (at most one strip per pixel column; its kind is the first one in that column)
                    if let Some(last) = strips.last_mut().filter(|l| {
                        (l.2 == c && x0 <= l.0.right() + 1.0)
                            || (x1 - x0 < 1.0 && x0 < l.0.right() + 1.0)
                    }) {
                        last.0.max.x = last.0.max.x.max(x1.max(x0 + 2.0));
                        last.3 += dur;
                        continue;
                    }
                    strips.push((
                        Rect::from_x_y_ranges(x0..=x1.max(x0 + 2.0), strip_y..=strip_y + 14.0),
                        p.label.as_str(),
                        c,
                        dur,
                    ));
                }
                for (strip, _, c, _) in &strips {
                    painter.rect_filled(
                        Rect::from_x_y_ranges(strip.x_range(), r.y_range()),
                        0.0,
                        c.gamma_multiply(0.06),
                    );
                    painter.rect_filled(*strip, 2.0, c.gamma_multiply(0.35));
                }
                // labels (linear: a label only meets its neighbours); with very many strips none — hover names them
                let (mut row1_end, mut row2_end) = (f32::MIN, f32::MIN);
                for (k, (strip, label, c, _)) in
                    strips.iter().enumerate().filter(|_| strips.len() <= 150)
                {
                    let tw = label.chars().count() as f32 * CW + 6.0;
                    if strip.width() > tw {
                        painter.text(
                            strip.center(),
                            Align2::CENTER_CENTER,
                            *label,
                            FontId::monospace(10.0),
                            Color32::WHITE,
                        );
                        row1_end = row1_end.max(strip.right());
                        continue;
                    }
                    let boxed = |rect: Rect| {
                        painter.rect_filled(rect, 2.0, Color32::from_black_alpha(170));
                        painter.text(
                            rect.center(),
                            Align2::CENTER_CENTER,
                            *label,
                            FontId::monospace(10.0),
                            c.gamma_multiply(1.6),
                        );
                    };
                    // 1) on the strip row, right after the strip, if the next strip leaves room
                    let next_left = strips.get(k + 1).map_or(r.right(), |n| n.0.left());
                    let right = Rect::from_min_size(
                        Pos2::new(strip.right() + 2.0, strip.top()),
                        Vec2::new(tw, 14.0),
                    );
                    if right.right() <= next_left - 2.0
                        && right.right() <= r.right()
                        && right.left() > row1_end
                    {
                        boxed(right);
                        row1_end = right.right();
                        continue;
                    }
                    // 2) second row, under the strip, with a tick
                    let x = strip.left().max(row2_end + 4.0).min(r.right() - tw);
                    if x >= strip.left() - 40.0 {
                        let low = Rect::from_min_size(
                            Pos2::new(x, strip.bottom() + 2.0),
                            Vec2::new(tw, 14.0),
                        );
                        painter.line_segment(
                            [
                                Pos2::new(strip.center().x, strip.bottom()),
                                Pos2::new(strip.center().x, low.top()),
                            ],
                            Stroke::new(1.0, c.gamma_multiply(0.8)),
                        );
                        boxed(low);
                        row2_end = low.right();
                    }
                    // 3) else: hover shows it
                }
                // hover over a strip (or its label row): label and duration next to the pointer
                if let Some(hp) = painter
                    .ctx()
                    .pointer_hover_pos()
                    .filter(|p| r.contains(*p) && (strip_y..=strip_y + 30.0).contains(&p.y))
                {
                    if let Some((_, label, c, dur)) = strips
                        .iter()
                        .filter(|s| (s.0.left() - 2.0..=s.0.right() + 2.0).contains(&hp.x))
                        .min_by(|a, b| a.0.width().total_cmp(&b.0.width()))
                    {
                        let t = format!("{label} · {}", fmt_time(*dur));
                        let w = t.chars().count() as f32 * 6.8 + 10.0;
                        let tip = Rect::from_min_size(
                            Pos2::new((hp.x + 12.0).min(r.right() - w), hp.y + 14.0),
                            Vec2::new(w, 18.0),
                        );
                        painter.rect_filled(tip, 3.0, Color32::from_black_alpha(220));
                        painter.rect_stroke(
                            tip,
                            3.0,
                            Stroke::new(1.0, c.gamma_multiply(0.9)),
                            egui::StrokeKind::Inside,
                        );
                        painter.text(
                            tip.center(),
                            Align2::CENTER_CENTER,
                            t,
                            FontId::monospace(11.0),
                            Color32::WHITE,
                        );
                    }
                }
            }
            // readouts box, top right, one block per analysed channel (held values: no digit flicker)
            let mut lines: Vec<(String, Color32)> =
                vec![(format!("CH{} {name}", ch + 1), COLORS[ch])];
            for (k, v) in &st.readouts {
                lines.push((format!("{k}: {v}"), Color32::from_gray(220)));
            }
            if let Some((ok, t)) = &st.verdict {
                lines.push((
                    format!("{} {t}", if *ok { "OK:" } else { "CHECK:" }),
                    if *ok {
                        Color32::from_rgb(120, 220, 140)
                    } else {
                        Color32::from_rgb(255, 120, 100)
                    },
                ));
            }
            let w = lines
                .iter()
                .map(|l| l.0.chars().count())
                .max()
                .unwrap_or(10) as f32
                * 6.7
                + 12.0;
            let top = box_top + row * 14.0;
            let bx = Rect::from_min_size(
                Pos2::new(r.right() - w - 30.0, top),
                Vec2::new(w, lines.len() as f32 * 14.0 + 6.0),
            );
            painter.rect_filled(bx, 4.0, Color32::from_black_alpha(170));
            for (i, (t, c)) in lines.iter().enumerate() {
                painter.text(
                    Pos2::new(bx.left() + 6.0, bx.top() + 3.0 + i as f32 * 14.0),
                    Align2::LEFT_TOP,
                    t,
                    font.clone(),
                    *c,
                );
            }
            row += lines.len() as f32 + 1.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn res(ok: bool, t: &str) -> Analysis {
        Analysis {
            verdict: Some((ok, t.into())),
            readouts: vec![("amplitude".into(), t.into())],
            ..Default::default()
        }
    }

    #[test]
    fn noise_near_the_threshold_does_not_flicker() {
        let t0 = Instant::now();
        let mut a = ChanAna {
            present: false,
            ..Default::default()
        };
        // open input whose noise sometimes looks like 'a signal': alternating every capture → stays 'waiting'
        for k in 0..40 {
            let w = (k % 2 == 0).then(|| "flat".to_string());
            a.step(
                w,
                res(false, "irregular"),
                false,
                t0 + Duration::from_millis(30 * k),
            );
            assert!(!a.present, "capture {k}");
            assert!(a.verdict.is_none());
        }
        // a real signal: present after 3 captures in a row
        for k in 0..3 {
            a.step(
                None,
                res(true, "regular 1.0"),
                false,
                t0 + Duration::from_millis(2000 + 30 * k),
            );
        }
        assert!(a.present && a.verdict.as_ref().unwrap().0);
        // one odd capture doesn't change the verdict; the same verdict with new numbers doesn't count as a change
        a.step(
            None,
            res(false, "irregular"),
            false,
            t0 + Duration::from_millis(2100),
        );
        a.step(
            None,
            res(true, "regular 1.1"),
            false,
            t0 + Duration::from_millis(2130),
        );
        assert!(a.verdict.as_ref().unwrap().0);
        // a lasting change shows after 3 in a row
        for k in 0..3 {
            a.step(
                None,
                res(false, "irregular"),
                false,
                t0 + Duration::from_millis(2200 + 30 * k),
            );
        }
        assert!(!a.verdict.as_ref().unwrap().0);
        // a pending different verdict starts over when the shown one comes again: OK, CHECK, OK, CHECK, CHECK stays OK
        for (k, ok) in [false, true, false, false].into_iter().enumerate() {
            a.step(
                None,
                res(ok, if ok { "regular" } else { "irregular" }),
                false,
                t0 + Duration::from_millis(2400 + 30 * k as u64),
            );
        }
        assert!(!a.verdict.as_ref().unwrap().0, "was CHECK before");
        for (k, ok) in [true, false, true, true, false].into_iter().enumerate() {
            a.step(
                None,
                res(ok, if ok { "regular" } else { "irregular" }),
                false,
                t0 + Duration::from_millis(2600 + 30 * k as u64),
            );
        }
        assert!(!a.verdict.as_ref().unwrap().0, "OK never came 3 in a row");
        // signal lost: until the flip (3 captures) the last results stay, without bands from noise
        a.res.phases.push(crate::analyze::Phase {
            s0: 0,
            s1: 1,
            tag: Tag::Gap,
            label: "x".into(),
        });
        a.step(
            Some("flat".into()),
            res(false, "noise"),
            false,
            t0 + Duration::from_millis(2800),
        );
        assert!(a.present && a.res.phases.is_empty() && !a.verdict.as_ref().unwrap().0);
        // the numbers belong to the shown verdict: an odd capture's readouts don't appear under it
        let mut b = ChanAna {
            present: true,
            ..Default::default()
        };
        b.step(None, res(true, "dwell ok"), false, t0);
        b.step(
            None,
            Analysis {
                verdict: Some((false, "no dwell".into())),
                readouts: vec![("ignition".into(), "no dwell found".into())],
                ..Default::default()
            },
            false,
            t0 + Duration::from_secs(1),
        );
        assert!(b.verdict.as_ref().unwrap().0);
        assert_eq!(b.readouts()[0].1, "dwell ok");
        // browsing / stopped: follows each capture at once
        a.step(
            None,
            res(true, "regular"),
            true,
            t0 + Duration::from_millis(3000),
        );
        assert!(a.verdict.as_ref().unwrap().0);
    }
}
