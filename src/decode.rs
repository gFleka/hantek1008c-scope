//! Serial decode (Phase 4): LIN and K-line (UART-style, idle high) bytes shown above the decoded channel's trace.
//! Decoding is `dsp::uart_decode` (pure, tested); this child module of `ui` only draws and holds the settings.

use super::*;
use crate::dsp::{can_decode, lin_pid, uart_decode, CanFrame, SerialItem};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Proto {
    Lin,
    KLine,
    /// CAN on CAN-H (dominant high) or CAN-L (dominant low)
    CanH,
    CanL,
}

pub(super) struct Decode {
    on: bool,
    ch: usize,
    proto: Proto,
    baud: f64,
    /// CAN log: (unix time s, frame) of every CRC-correct frame decoded while running; last capture seen
    log: Vec<(f64, CanFrame)>,
    log_seq: u64,
    /// experimental: capture only the CAN channel (2.4 MS/s → ~4.8 samples per bit at 500 kbit/s); not saved
    pub(super) fast: bool,
}

/// Frames kept in the CAN log (≈ 1 M × ~40 B).
const CAN_LOG_MAX: usize = 1_000_000;

impl Default for Decode {
    fn default() -> Self {
        Decode {
            on: false,
            ch: 0,
            proto: Proto::Lin,
            baud: 19_200.0,
            log: Vec::new(),
            log_seq: 0,
            fast: false,
        }
    }
}

impl Decode {
    /// Empty the CAN log (Clear button).
    pub(super) fn clear_log(&mut self) {
        self.log.clear();
    }
    pub(super) fn off(&mut self) {
        (self.on, self.fast) = (false, false);
    }
    /// The only channel to capture in the experimental fast CAN mode.
    pub(super) fn fast_channel(&self) -> Option<usize> {
        (self.on && self.fast && matches!(self.proto, Proto::CanH | Proto::CanL)).then_some(self.ch)
    }
    /// Decoding on for a channel with a protocol given as "lin:19200" / "canh:100000" (guided tests).
    pub(super) fn set(&mut self, ch: usize, bus: &str) {
        if let Some(mut d) = Decode::decode(&format!("1:{ch}:{bus}")) {
            d.log = std::mem::take(&mut self.log);
            *self = d;
        }
    }
    pub(super) fn encode(&self) -> String {
        let p = match self.proto {
            Proto::Lin => "lin",
            Proto::KLine => "kline",
            Proto::CanH => "canh",
            Proto::CanL => "canl",
        };
        format!("{}:{}:{p}:{}", self.on as u8, self.ch, self.baud)
    }
    pub(super) fn decode(s: &str) -> Option<Decode> {
        let p: Vec<&str> = s.split(':').collect();
        Some(Decode {
            on: *p.first()? == "1",
            ch: p.get(1)?.parse::<usize>().ok()?.min(CHANNELS - 1),
            proto: match *p.get(2)? {
                "kline" => Proto::KLine,
                "canh" => Proto::CanH,
                "canl" => Proto::CanL,
                _ => Proto::Lin,
            },
            baud: p
                .get(3)?
                .parse::<f64>()
                .ok()
                .filter(|b| (300.0..=1_000_000.0).contains(b))?,
            log: Vec::new(),
            log_seq: 0,
            fast: false,
        })
    }
}

impl App {
    /// Decoded bytes above the decoded channel's trace (only on its plot).
    pub(super) fn decode_overlay(
        &self,
        painter: &egui::Painter,
        r: Rect,
        vr: Rect,
        pane: usize,
        snap: &Snapshot,
    ) {
        let d = &self.decoder;
        if !d.on || !self.on[d.ch] || self.pane_for(d.ch) != pane {
            return;
        }
        let (Some(x), Some((a, b))) = (self.units_of(snap, d.ch), self.x_map(snap, d.ch, vr))
        else {
            return;
        };
        let dt = self.sample_dt(snap, x.len());
        let can = matches!(d.proto, Proto::CanH | Proto::CanL);
        let spb = 1.0 / (d.baud * dt);
        if spb < if can { 1.8 } else { 3.0 } {
            return; // explained in the info line (decode_notice)
        }
        if can {
            let col = COLORS[d.ch];
            let y = r.top() + 20.0;
            for (s0, s1, fr) in can_decode(&x, dt, d.baud, d.proto == Proto::CanH) {
                let (x0, x1) = (a + b * s0 as f32, a + b * s1 as f32);
                if x1 < r.left() || x0 > r.right() {
                    continue;
                }
                let (text, bad) = match fr {
                    None => ("CAN ?".to_string(), true),
                    Some(f) => {
                        let id = if f.ext {
                            format!("{:08X}", f.id)
                        } else {
                            format!("{:03X}", f.id)
                        };
                        let body = if f.rtr {
                            "RTR".to_string()
                        } else {
                            f.data
                                .iter()
                                .map(|v| format!("{v:02X}"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        };
                        (
                            format!(
                                "ID {id} [{}] {body}{}",
                                f.data.len(),
                                if f.crc_ok { "" } else { "  CRC!" }
                            ),
                            !f.crc_ok,
                        )
                    }
                };
                let c = if bad {
                    Color32::from_rgb(255, 90, 90)
                } else {
                    col
                };
                let rect = Rect::from_min_max(
                    Pos2::new(x0.max(r.left()), y - 8.0),
                    Pos2::new(x1.min(r.right()), y + 8.0),
                );
                painter.rect_stroke(
                    rect,
                    3.0,
                    Stroke::new(1.0, c.gamma_multiply(0.7)),
                    egui::StrokeKind::Inside,
                );
                painter.with_clip_rect(rect.intersect(r)).text(
                    Pos2::new(rect.left() + 4.0, y),
                    Align2::LEFT_CENTER,
                    text,
                    FontId::monospace(10.0),
                    c,
                );
            }
            return;
        }
        let items = uart_decode(&x, dt, d.baud);
        let col = COLORS[d.ch];
        let y = r.top() + 20.0;
        let mut after_break = None::<usize>; // LIN: bytes since the last break (0 = sync, 1 = protected id)
        for (s0, s1, item) in items {
            let (x0, x1) = (a + b * s0 as f32, a + b * s1 as f32);
            let (text, bad) = match item {
                SerialItem::Break => {
                    after_break = Some(0);
                    ("BRK".to_string(), false)
                }
                SerialItem::FrameError => ("ERR".to_string(), true),
                SerialItem::Byte(v) => {
                    let t = match (d.proto, after_break) {
                        (Proto::Lin, Some(0)) => (
                            if v == 0x55 {
                                "SYNC".to_string()
                            } else {
                                format!("{v:02X}≠55")
                            },
                            v != 0x55,
                        ),
                        (Proto::Lin, Some(1)) => {
                            let (id, ok) = lin_pid(v);
                            (format!("ID {id:02X}{}", if ok { "" } else { "!" }), !ok)
                        }
                        _ => (format!("{v:02X}"), false),
                    };
                    after_break = after_break.map(|n| n + 1);
                    t
                }
            };
            if x1 < r.left() || x0 > r.right() {
                continue;
            }
            let c = if bad {
                Color32::from_rgb(255, 90, 90)
            } else {
                col
            };
            let rect = Rect::from_min_max(
                Pos2::new(x0.max(r.left()), y - 8.0),
                Pos2::new(x1.min(r.right()), y + 8.0),
            );
            painter.rect_stroke(
                rect,
                3.0,
                Stroke::new(1.0, c.gamma_multiply(0.7)),
                egui::StrokeKind::Inside,
            );
            if rect.width() > 14.0 {
                painter.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    text,
                    FontId::monospace(10.0),
                    c,
                );
            }
        }
    }

    /// Side panel "Protocol decoding": LIN / K-line / CAN on one channel, CAN log, the bus guided tests.
    pub(super) fn protocol_panel(&mut self, ui: &mut egui::Ui) {
        self.presets_button(ui, true);
        let d = &mut self.decoder;
        ui.horizontal(|ui| {
            ui.label("Bus");
            let name = if !d.on {
                "Off"
            } else {
                match d.proto {
                    Proto::Lin => "LIN",
                    Proto::KLine => "K-line",
                    Proto::CanH => "CAN-H",
                    Proto::CanL => "CAN-L",
                }
            };
            let cb = egui::ComboBox::from_id_salt("bus_proto").width(70.0).selected_text(name).show_ui(ui, |ui| {
                if ui.selectable_label(!d.on, "Off").clicked() {
                    d.on = false;
                }
                for (p, n, baud) in [(Proto::Lin, "LIN", 19_200.0), (Proto::KLine, "K-line", 10_400.0), (Proto::CanH, "CAN-H", 100_000.0), (Proto::CanL, "CAN-L", 100_000.0)] {
                    if ui.selectable_label(d.on && d.proto == p, n).clicked() {
                        (d.on, d.proto, d.baud) = (true, p, baud);
                    }
                }
            });
            help(cb.response, "Decode a serial bus on one channel: bytes / frames are drawn above its trace. CAN: probe CAN-H (or CAN-L).");
            if d.on {
                egui::ComboBox::from_id_salt("bus_ch").width(48.0).selected_text(format!("CH{}", d.ch + 1)).show_ui(ui, |ui| {
                    for c in 0..CHANNELS {
                        ui.selectable_value(&mut d.ch, c, format!("CH{}", c + 1));
                    }
                });
                help(ui.add(egui::DragValue::new(&mut d.baud).range(300.0..=1_000_000.0).speed(100.0).suffix(" bit/s")), "Bit rate: LIN 19 200, K-line 10 400, low-speed / body CAN 100 000 / 125 000.");
            }
        });
        if d.on && matches!(d.proto, Proto::CanH | Proto::CanL) {
            let fast = help(
                ui.checkbox(&mut d.fast, "fast 1-channel mode (experimental, 500 kbit/s)"),
                "Captures only the CAN channel, so it gets the scope's full 2.4 MS/s (≈ 4.8 samples per bit at 500 kbit/s high-speed CAN). EXPERIMENTAL: the 1008C firmware's fewer-than-8-channel path has a known glitch (a hole / seam in some captures), so some frames will fail the CRC or be missing, and the other channels pause while it is on. Check important results with a real CAN tool.",
            );
            // 500 k once when ticked; any rate stays possible afterwards (e.g. 125 k with the cleanest sampling)
            if fast.changed() && d.fast && d.baud < 200_000.0 {
                d.baud = 500_000.0;
            }
            let n = d.log.len();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("CAN log {n} frames")).weak());
                if help(ui.add_enabled(n > 0, egui::Button::new("Export .asc").small()), "Save the logged frames as Vector ASC (CANalyzer, SavvyCAN, python-can) into the recording folder. The log holds the frames seen in the captures (about one per capture, 30–40/s): a sample of the bus, not every frame.").clicked() {
                    self.export_can_log();
                }
                if help(ui.add_enabled(n > 0, egui::Button::new("Clear").small()), "Empty the CAN log.").clicked() {
                    self.decoder.clear_log();
                }
            });
        }
    }

    /// Side panel "Automotive tests": the guided tests, the signal type per shown channel (phases highlighted, readouts,
    /// verdict), test runs with a report, typical waveforms.
    pub(super) fn analysis_panel(&mut self, ui: &mut egui::Ui) {
        self.presets_button(ui, false);
        // waveform analysis: signal type per shown channel
        // the test-run controls stay while a run is active, so it can always be stopped
        if self.test_running() && !self.kinds.iter().any(|k| *k != crate::analyze::Kind::None) {
            self.test_controls(ui);
        }
        if self.kinds.iter().any(|k| *k != crate::analyze::Kind::None) {
            self.test_controls(ui);
            help(
                ui.checkbox(&mut self.highlight_on, "highlight the waveform's parts"),
                "Coloured bands over the trace and a labelled strip for each part the analysis found (e.g. injector open, spike, pintle close; ignition dwell, spike, burn). Off: a clean trace — the readouts and the verdict stay.",
            );
            help(
                ui.checkbox(&mut self.typical_on, "show a typical healthy waveform (dashed)"),
                "Built-in reference: how a healthy injector / ignition primary / Hall or PWM pulse / crank or wheel-speed sensor typically looks (fitted to the measured speed and amplitude, drawn from the trigger point), and limit lines for lambda and battery voltage. Your own references: Menu › References (R / Shift+R).",
            );
        }
        for ch in 0..CHANNELS {
            if !self.on[ch] {
                continue;
            }
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("Analyse CH{} as", ch + 1)).color(COLORS[ch]));
                let before = self.kinds[ch];
                let k = &mut self.kinds[ch];
                let cb = egui::ComboBox::from_id_salt(("kind_panel", ch))
                    .width(ui.available_width())
                    .truncate()
                    .selected_text(crate::analyze::KINDS.iter().find(|x| x.0 == *k).map_or("—", |x| x.1))
                    .show_ui(ui, |ui| {
                        for (kind, name) in crate::analyze::KINDS {
                            ui.selectable_value(k, kind, name);
                        }
                    });
                help(cb.response, "How this channel's trace is analysed (only that; 'Guided tests…' sets up channels, ranges, timebase, trigger and this in one go, with instructions). The phases of its waveform (e.g. injector open / spike / pintle close, ignition dwell / spike / burn) are found, highlighted on the trace and summed up with a verdict. Changing it ends an open guided test for this channel (its connect / expect text no longer fits).");
                if self.kinds[ch] != before {
                    self.guide_kind_changed(ch);
                }
            });
        }
    }
}

/// "Sat Nov 22 01:02:03.456 pm 2026" (ASC header date) from unix seconds, UTC.
fn asc_date(unix: f64) -> String {
    let secs = unix.max(0.0).floor() as i64;
    let ms = ((unix - secs as f64) * 1000.0) as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    // civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    let wd = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let mon = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(m - 1) as usize];
    let (h, mi, s) = (sod / 3600, sod / 60 % 60, sod % 60);
    let (h12, ampm) = (
        if h % 12 == 0 { 12 } else { h % 12 },
        if h < 12 { "am" } else { "pm" },
    );
    format!("{wd} {mon} {d:02} {h12:02}:{mi:02}:{s:02}.{ms:03} {ampm} {y}")
}

/// Vector ASC text (CANalyzer / CANoe, SavvyCAN, python-can, asammdf) for logged frames, channel 1, times relative
/// to the first frame.
pub(super) fn asc_text(log: &[(f64, CanFrame)]) -> String {
    let t0 = log.first().map_or(0.0, |f| f.0);
    let date = asc_date(t0);
    let mut s = format!("date {date}\nbase hex  timestamps absolute\ninternal events logged\n// version 8.0.0\nBegin Triggerblock {date}\n   0.000000 Start of measurement\n");
    for (t, f) in log {
        let id = if f.ext {
            format!("{:X}x", f.id)
        } else {
            format!("{:X}", f.id)
        };
        let body = if f.rtr {
            format!("r {:X}", f.data.len())
        } else {
            format!(
                "d {} {}",
                f.data.len(),
                f.data
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        };
        s += &format!("{:>11.6} 1  {id:<15} Rx   {}\n", t - t0, body.trim_end());
    }
    s += "End TriggerBlock\n";
    s
}

impl App {
    /// Info-line advice for the decoder (too few samples per bit, CAN log full).
    pub(super) fn decode_notice(&self, snap: &Snapshot, v: &mut Vec<(Level, String)>) {
        let d = &self.decoder;
        if !d.on || !self.on[d.ch] {
            return;
        }
        if let Some(cfg) = snap.cfg.as_ref() {
            let n = snap.frame.data[d.ch].len().max(1);
            let dt = if snap.roll {
                1.0 / self.shown_rate.max(1e-9)
            } else {
                capture_seconds(cfg.timebase_id) / n as f64
            };
            let spb = 1.0 / (d.baud * dt);
            let can = matches!(d.proto, Proto::CanH | Proto::CanL);
            if spb < if can { 1.8 } else { 3.0 } {
                v.push((
                    Level::Tip,
                    if can {
                        format!("Decode: CAN at {:.0} kbit/s needs ≥ 2 samples per bit, this setting gives {spb:.1} — use 100 µs/div; for 500 kbit/s high-speed CAN try 'fast 1-channel mode (experimental)' in Protocol decoding.", d.baud / 1e3)
                    } else {
                        format!("Decode: {spb:.1} samples per bit — use a faster timebase (LIN / K-line ≤ 500 µs/div).")
                    },
                ));
            }
        }
        if d.log.len() >= CAN_LOG_MAX {
            v.push((
                Level::Warn,
                format!(
                    "CAN log full ({CAN_LOG_MAX} frames) — export and clear it to keep logging."
                ),
            ));
        }
    }

    /// (file-name part, description) of the active decoder, for screenshots.
    pub(super) fn decoder_summary(&self) -> Option<(String, String)> {
        let d = &self.decoder;
        d.on.then(|| {
            let p = match d.proto {
                Proto::Lin => "LIN",
                Proto::KLine => "K-line",
                Proto::CanH => "CAN-H",
                Proto::CanL => "CAN-L",
            };
            (
                format!("{p}-CH{}", d.ch + 1),
                format!(
                    "{p} on CH{} at {:.0} bit/s{}",
                    d.ch + 1,
                    d.baud,
                    if d.fast { " (fast 1-channel mode)" } else { "" }
                ),
            )
        })
    }

    pub(super) fn can_log_len(&self) -> usize {
        self.decoder.log.len()
    }

    /// Every new capture while CAN decoding runs: append its CRC-correct frames to the log (time from the capture).
    pub(super) fn can_log_tick(&mut self, snap: &Snapshot) {
        let d = &self.decoder;
        if !d.on
            || !matches!(d.proto, Proto::CanH | Proto::CanL)
            || snap.roll
            || self.run != Run::Running
            || snap.frame.seq == d.log_seq
        {
            return;
        }
        let Some(x) = self.units_of(snap, d.ch) else {
            return;
        };
        let dt = self.sample_dt(snap, x.len());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |t| t.as_secs_f64());
        let frames: Vec<(f64, CanFrame)> = can_decode(&x, dt, d.baud, d.proto == Proto::CanH)
            .into_iter()
            .filter_map(|(s0, _, f)| {
                f.filter(|f| f.crc_ok)
                    .map(|f| (now - (x.len() - s0) as f64 * dt, f))
            })
            .collect();
        let d = &mut self.decoder;
        d.log_seq = snap.frame.seq;
        if d.log.len() + frames.len() <= CAN_LOG_MAX {
            d.log.extend(frames);
        }
    }

    /// Menu: write the CAN log as Vector ASC into the recording folder.
    pub(super) fn export_can_log(&mut self) {
        let dir = self.files.dir();
        let name = format!("can_{}.asc", crate::rec::stamp(SystemTime::now()));
        let r = std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(dir.join(&name), asc_text(&self.decoder.log)));
        let n = self.decoder.log.len();
        match r {
            Ok(()) => self.files_msg(
                Level::Info,
                format!("CAN log: {n} frames saved to {}", dir.join(&name).display()),
            ),
            Err(e) => self.files_msg(Level::Warn, format!("CAN log export failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asc_export_format() {
        assert_eq!(
            asc_date(1_795_348_659.25),
            "Sun Nov 22 11:57:39.250 am 2026"
        ); // checked with Python datetime
        assert_eq!(asc_date(0.0), "Thu Jan 01 12:00:00.000 am 1970");
        let f = |id, ext, data: &[u8]| CanFrame {
            id,
            ext,
            rtr: false,
            data: data.to_vec(),
            crc_ok: true,
        };
        let log = vec![
            (100.0, f(0x2C0, false, &[1, 2, 3, 4])),
            (100.0125, f(0x1ABCDEF0, true, &[0xFF])),
            (
                100.5,
                CanFrame {
                    id: 0x7DF,
                    ext: false,
                    rtr: true,
                    data: vec![0; 8],
                    crc_ok: true,
                },
            ),
        ];
        let t = asc_text(&log);
        let lines: Vec<&str> = t.lines().collect();
        assert!(lines[0].starts_with("date ") && lines[1] == "base hex  timestamps absolute");
        assert_eq!(
            lines[6],
            "   0.000000 1  2C0             Rx   d 4 01 02 03 04"
        );
        assert_eq!(lines[7], "   0.012500 1  1ABCDEF0x       Rx   d 1 FF");
        assert_eq!(lines[8], "   0.500000 1  7DF             Rx   r 8");
        assert_eq!(*lines.last().unwrap(), "End TriggerBlock");
    }

    #[test]
    fn fast_can_mode_only_for_can() {
        let mut d = Decode {
            on: true,
            ch: 6,
            proto: Proto::CanH,
            baud: 500_000.0,
            log: Vec::new(),
            log_seq: 0,
            fast: true,
        };
        assert_eq!(d.fast_channel(), Some(6));
        d.proto = Proto::Lin; // fast mode is a CAN-only thing
        assert_eq!(d.fast_channel(), None);
        d.proto = Proto::CanL;
        d.off();
        assert_eq!(d.fast_channel(), None);
        assert!(!d.fast, "switching decoding off also ends the fast mode");
        // never saved: a decoded settings line has it off
        assert!(!Decode::decode("1:6:canh:500000").unwrap().fast);
    }

    #[test]
    fn decode_settings_roundtrip() {
        for proto in [Proto::Lin, Proto::KLine, Proto::CanH, Proto::CanL] {
            let d = Decode {
                on: true,
                ch: 4,
                proto,
                baud: 100_000.0,
                log: Vec::new(),
                log_seq: 0,
                fast: false,
            };
            let e = Decode::decode(&d.encode()).unwrap();
            assert_eq!((e.on, e.ch, e.proto, e.baud), (true, 4, proto, 100_000.0));
        }
        assert!(Decode::decode("1:0:lin:0").is_none());
    }
}
