//! Engine-cycle markers (Phase 4): TDC lines for each cylinder, in firing order, found from a missing-tooth crank
//! signal (e.g. 60-2) and optionally phased with the cam signal (720° cycle). Drawn on the crank channel's plot.
//! Child module of `ui`.

use super::*;

pub(super) struct Engine {
    pub(super) on: bool,
    crank: usize,
    /// cam channel for the 720° phase (cam high at the crank gap = first half of the cycle); None = no cam
    cam: Option<usize>,
    /// tooth positions on the wheel (incl. missing) and missing teeth: 60-2
    total: u32,
    missing: u32,
    /// TDC of cylinder 1, in tooth positions after the gap (from the manual / a timing-light check)
    tdc1: f32,
    /// firing order, e.g. "1-3-4-2"
    order: String,
}

impl Default for Engine {
    fn default() -> Self {
        Engine {
            on: false,
            crank: 0,
            cam: None,
            total: 60,
            missing: 2,
            tdc1: 0.0,
            order: "1-3-4-2".into(),
        }
    }
}

impl Engine {
    pub(super) fn encode(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}:{}",
            self.on as u8,
            self.crank,
            self.cam.map_or(-1, |c| c as i32),
            self.total,
            self.missing,
            self.tdc1,
            self.order
        )
    }
    pub(super) fn decode(s: &str) -> Option<Engine> {
        let p: Vec<&str> = s.split(':').collect();
        let cyl = p
            .get(6)?
            .split('-')
            .filter_map(|c| c.trim().parse::<u8>().ok())
            .count();
        (cyl >= 1).then_some(())?;
        Some(Engine {
            on: *p.first()? == "1",
            crank: p.get(1)?.parse::<usize>().ok()?.min(CHANNELS - 1),
            cam: p
                .get(2)?
                .parse::<i32>()
                .ok()
                .filter(|&c| c >= 0 && (c as usize) < CHANNELS)
                .map(|c| c as usize),
            total: p.get(3)?.parse::<u32>().ok()?.clamp(4, 400),
            missing: p.get(4)?.parse::<u32>().ok()?.min(3),
            tdc1: p.get(5)?.parse::<f32>().ok().filter(|v| v.is_finite())?,
            order: p.get(6)?.to_string(),
        })
    }
    /// Markers on for this crank (and optional cam) channel (guided tests).
    pub(super) fn set_channels(&mut self, crank: usize, cam: Option<usize>) {
        (self.on, self.crank, self.cam) = (true, crank, cam);
    }
    /// (tooth positions incl. missing, missing teeth) for the crank analyzer.
    pub(super) fn wheel(&self) -> (u32, u32) {
        (self.total, self.missing)
    }
    fn cylinders(&self) -> Vec<u8> {
        self.order
            .split('-')
            .filter_map(|c| c.trim().parse().ok())
            .collect()
    }
}

impl App {
    /// TDC markers on the crank channel's plot (captures and streams).
    pub(super) fn engine_markers(
        &self,
        painter: &egui::Painter,
        r: Rect,
        vr: Rect,
        pane: usize,
        snap: &Snapshot,
    ) {
        let e = &self.engine;
        if !e.on || !self.on[e.crank] || self.pane_for(e.crank) != pane {
            return;
        }
        let (Some(x), Some((a, b))) = (self.units_of(snap, e.crank), self.x_map(snap, e.crank, vr))
        else {
            return;
        };
        let present = e.total.saturating_sub(e.missing).max(1);
        let teeth = crate::dsp::crank_teeth(&x, present);
        let cyl = e.cylinders();
        if teeth.is_empty() || cyl.is_empty() {
            return;
        }
        // below ~5 samples per tooth the tooth numbering is unreliable: no markers rather than wrong ones (the crank
        // analyzer / info line explains it)
        let mut iv: Vec<usize> = teeth.windows(2).map(|w| w[1].0 - w[0].0).collect();
        iv.sort_unstable();
        if iv.get(iv.len() / 2).is_none_or(|&m| m < 4) {
            return;
        }
        let cam = e.cam.and_then(|c| self.units_of(snap, c));
        let cam_mid = cam.as_ref().map(|c| {
            let (lo, hi) = c
                .iter()
                .fold((f32::MAX, f32::MIN), |(l, h), &v| (l.min(v), h.max(v)));
            (lo + hi) / 2.0
        });
        // sample index of a wheel position `pos` (tooth positions after the gap) within the revolution starting at
        // teeth[k0] (tooth 0): interpolated between tooth edges, across the gap too
        let at = |k0: usize, pos: f32| -> Option<f32> {
            let pos = pos.rem_euclid(e.total as f32);
            let t = pos.floor() as u32;
            let idx = |tooth: u32| {
                teeth[k0..]
                    .iter()
                    .find(|x| x.1 == tooth)
                    .map(|x| x.0 as f32)
            };
            if t + 1 < present {
                let (s0, s1) = (idx(t)?, idx(t + 1)?);
                Some(s0 + (s1 - s0) * pos.fract())
            } else {
                // in the gap: from the last present tooth to the next revolution's tooth 0
                let s0 = idx(present - 1)?;
                let s1 = teeth[k0 + 1..]
                    .iter()
                    .find(|x| x.1 == 0)
                    .map(|x| x.0 as f32)?;
                Some(s0 + (s1 - s0) * (pos - (present - 1) as f32) / (e.total - present + 1) as f32)
            }
        };
        let per_rev = (cyl.len() as f32 / 2.0).max(1.0); // TDCs per crank revolution (4 cyl: 2)
        let step = e.total as f32 / per_rev;
        let col = Color32::from_gray(200); // neutral: not a channel colour
        for (k0, &(gap_at, tooth)) in teeth.iter().enumerate() {
            if tooth != 0 {
                continue;
            }
            // which half of the 720° cycle: cam high at the gap = first revolution (cylinder 1 fires in it)
            let half = match (&cam, cam_mid) {
                (Some(c), Some(m)) => Some(c.get(gap_at).is_some_and(|&v| v > m)),
                _ => None,
            };
            for j in 0..per_rev as usize {
                let Some(si) = at(k0, e.tdc1 + j as f32 * step) else {
                    continue;
                };
                let xpix = a + b * si;
                if xpix < r.left() || xpix > r.right() {
                    continue;
                }
                let label = match half {
                    Some(first) => {
                        let i = if first { j } else { j + per_rev as usize };
                        format!("TDC{}", cyl.get(i % cyl.len()).copied().unwrap_or(0))
                    }
                    None => format!(
                        "TDC{}/{}",
                        cyl[j % cyl.len()],
                        cyl[(j + per_rev as usize) % cyl.len()]
                    ),
                };
                painter.extend(Shape::dashed_line(
                    &[Pos2::new(xpix, r.top() + 16.0), Pos2::new(xpix, r.bottom())],
                    Stroke::new(1.0, col.gamma_multiply(0.6)),
                    4.0,
                    4.0,
                ));
                painter.text(
                    Pos2::new(xpix + 3.0, r.top() + 4.0),
                    Align2::LEFT_TOP,
                    label,
                    FontId::monospace(11.0),
                    col,
                );
            }
        }
    }

    /// Settings > Engine.
    pub(super) fn engine_settings(&mut self, ui: &mut egui::Ui) {
        let e = &mut self.engine;
        help(
            ui.checkbox(&mut e.on, "TDC markers from the crank signal"),
            "Finds the missing-tooth gap on the crank sensor signal and draws a TDC line per cylinder (firing order) on the crank channel's plot. With a cam channel the 720° cycle is resolved (e.g. TDC1 vs TDC4 on a 4-cylinder), else the two cylinders sharing a crank position are shown, e.g. 1/4.",
        );
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("eng_crank")
                .width(60.0)
                .selected_text(format!("crank CH{}", e.crank + 1))
                .show_ui(ui, |ui| {
                    for c in 0..CHANNELS {
                        ui.selectable_value(&mut e.crank, c, format!("CH{}", c + 1));
                    }
                });
            egui::ComboBox::from_id_salt("eng_cam")
                .width(70.0)
                .selected_text(
                    e.cam
                        .map_or("no cam".to_string(), |c| format!("cam CH{}", c + 1)),
                )
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut e.cam, None, "no cam");
                    for c in 0..CHANNELS {
                        ui.selectable_value(&mut e.cam, Some(c), format!("CH{}", c + 1));
                    }
                });
        });
        ui.horizontal(|ui| {
            help(ui.add(egui::DragValue::new(&mut e.total).range(4..=400).prefix("wheel ")), "Tooth positions on the trigger wheel including the missing ones (60 for a 60-2 wheel).");
            help(ui.add(egui::DragValue::new(&mut e.missing).range(0..=3).prefix("− ")), "Missing teeth (2 for 60-2).");
            help(
                ui.add(egui::DragValue::new(&mut e.tdc1).speed(0.1).range(-60.0..=400.0).prefix("TDC1 at tooth ")),
                "Cylinder 1 TDC, in tooth positions after the gap (1 position = 360°/wheel, 6° on 60-2). From the repair manual, or line it up once with a timing light.",
            );
        });
        ui.horizontal(|ui| {
            ui.label("firing order");
            help(
                ui.add(egui::TextEdit::singleline(&mut e.order).desired_width(80.0)),
                "e.g. 1-3-4-2 (common 4-cylinder), 1-5-3-6-2-4 (inline 6).",
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_settings_roundtrip() {
        let e = Engine {
            on: true,
            crank: 2,
            cam: Some(5),
            total: 60,
            missing: 2,
            tdc1: 14.5,
            order: "1-3-4-2".into(),
        };
        let d = Engine::decode(&e.encode()).unwrap();
        assert_eq!(
            (
                d.on,
                d.crank,
                d.cam,
                d.total,
                d.missing,
                d.tdc1,
                d.order.as_str()
            ),
            (true, 2, Some(5), 60, 2, 14.5, "1-3-4-2")
        );
        assert_eq!(Engine::decode("1:0:-1:60:2:0:x").map(|e| e.cam), None); // no cylinders → refused
    }
}
