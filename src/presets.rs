//! Guided tests (Phase 4): one click sets channels, ranges, probe, timebase, trigger and helpers for a common
//! automotive measurement, and a small guide window says what to connect and what a good signal looks like.
//! A child module of `ui` (`#[path]`), like files.rs; data only + one apply function.

use super::*;

/// One channel of a preset: channel, units per division (in the probe's unit), probe preset index, offset (div).
type Chan = (usize, f32, usize, f32);

pub(super) struct Preset {
    group: &'static str,
    pub(super) name: &'static str,
    chans: &'static [Chan],
    /// screen time per division (ns); `stream` = continuous stream at 440 S/s instead of triggered captures
    ns_div: u64,
    stream: bool,
    /// trigger: source channel, rising, level (in the source's unit), position (0.1 %), Normal mode
    trig: Option<(usize, bool, f32, u16, bool)>,
    /// RPM math channel from channel A with this many pulses per revolution
    rpm: Option<(usize, f32)>,
    /// bus decoding on the first channel: "lin:19200", "canh:100000"
    bus: Option<&'static str>,
    /// match filter (e.g. CAN: a bus-idle gap so the shown capture holds a whole frame start)
    sw: Option<SwTrig>,
    /// waveform phase analysis per channel
    an: &'static [(usize, crate::analyze::Kind)],
    connect: &'static str,
    expect: &'static str,
    /// what this test also needs in the Settings window (not in the side panel); "" = nothing
    settings: &'static str,
}

const X1: usize = 0;
const X20: usize = 2;
const CLAMP_HI: usize = 3; // 10 mV/A (high-current clamp)
const CLAMP_LO: usize = 4; // 100 mV/A

pub(super) const PRESETS: &[Preset] = &[
    Preset {
        group: "Bus",
        name: "CAN low-speed / body bus (100 kbit/s)",
        chans: &[(0, 0.5, X1, -4.0)],
        ns_div: 100_000,
        stream: false,
        trig: Some((0, true, 3.0, 100, false)),
        rpm: None,
        bus: Some("canh:100000"),
        sw: None,
        an: &[],
        connect: "CH1 to CAN-H (back-probe a body / low-speed CAN at a door, seat or body module — the OBD port pins 6/14 usually carry 500 kbit/s diagnostic CAN), ground clip to chassis ground. Ignition on.",
        expect: "CAN-H idles at ≈ 2.5 V and goes to ≈ 3.5 V for dominant bits. Frames are shown with ID, data bytes and a CRC check. On an idle bus the trigger catches the start of a frame (10 % pre-trigger leaves room for the idle the decoder needs); a capture that starts mid-frame shows 'CAN ?'. This scope samples 300 kS/s per channel: 100–125 kbit/s low-speed / body CAN works, 500 kbit/s high-speed CAN only in the experimental fast 1-channel mode (Protocol decoding).",
        settings: "",
    },
    Preset {
        group: "Sensors",
        name: "Wheel speed sensor (ABS) / speed sensor",
        chans: &[(0, 1.0, X1, 0.0)],
        ns_div: 2_000_000,
        stream: false,
        trig: Some((0, true, 0.2, 100, false)),
        rpm: None,
        bus: None,
        sw: None,
        an: &[(0, crate::analyze::Kind::WheelSpeed)],
        connect: "CH1 to the sensor signal at its connector (2-wire passive / VR: across the two wires; active Hall / magnetoresistive: they switch a current ~7 / 14 mA — measure the voltage over the control unit's sense resistor or a small shunt). Lift the wheel and turn it by hand or drive slowly.",
        expect: "Passive (VR): a sine whose amplitude and frequency grow with speed. Active: a square wave between two levels. Every tooth looks the same: a missing / damaged tooth (gap), uneven spacing or an amplitude that wobbles once per turn (runout, air gap) is marked. Adjust the timebase so a few dozen teeth are on screen.",
        settings: "",
    },
    Preset {
        group: "Bus",
        name: "LIN bus (19.2 kbit/s)",
        chans: &[(0, 5.0, X1, -2.0)],
        ns_div: 500_000,
        stream: false,
        trig: Some((0, false, 6.0, 50, false)),
        rpm: None,
        bus: Some("lin:19200"),
        sw: None,
        an: &[],
        connect: "CH1 to the LIN wire (e.g. alternator regulator, rain/light sensor, window or mirror modules), ground clip to chassis ground. Ignition on.",
        expect: "Idle ≈ battery voltage, pulled to ≈ 0–1 V for dominant bits. Each frame: BRK, SYNC (0x55), ID (parity checked), data and checksum. Missing answers after the header = the slave doesn't respond.",
        settings: "",
    },
    Preset {
        group: "General",
        name: "Battery & cranking voltage",
        chans: &[(0, 2.0, X1, -4.0)], // 0 V at the bottom: 0-16 V on screen (charging 14.x V stays visible)
        ns_div: 500_000_000,
        stream: true,
        trig: None,
        rpm: None,
        bus: None,
        sw: None,
        an: &[(0, crate::analyze::Kind::Battery)],
        connect: "CH1 + to the battery + terminal, CH1 − (ground clip) to the battery − terminal. Then crank the engine.",
        expect: "Engine off ≈ 12.4–12.7 V. While cranking it must stay above ≈ 9.6 V (lower: weak battery or bad cables). Running ≈ 13.8–14.7 V (charging).",
        settings: "",
    },
    Preset {
        group: "General",
        name: "Relative compression (starter current)",
        chans: &[(0, 50.0, CLAMP_HI, -3.0)],
        ns_div: 200_000_000,
        stream: true,
        trig: None,
        rpm: None,
        bus: None,
        sw: None,
        an: &[(0, crate::analyze::Kind::Compression)],
        connect: "High-current clamp (10 mV/A) around the starter cable, arrow towards the starter. Disable fuel/ignition so the engine cranks without starting, then crank for ~5 s.",
        expect: "One current hump per compression stroke (4 per 2 revolutions). Equal humps = even compression; a clearly lower hump = that cylinder has less compression.",
        settings: "Settings › Probes: this test sets CH1 to a high-current clamp at 10 mV/A. If your clamp has another sensitivity (e.g. 1 mV/A), set it there — the shape is still right, the amps are not.",
    },
    Preset {
        group: "General",
        name: "Lambda (narrowband) switching",
        chans: &[(0, 0.2, X1, -2.0)],
        ns_div: 500_000_000,
        stream: true,
        trig: None,
        rpm: None,
        bus: None,
        sw: None,
        an: &[(0, crate::analyze::Kind::Lambda)],
        connect: "CH1 to the lambda sensor's signal wire (back-probe at the connector), ground clip to engine ground. Engine warm, hold ~2500 rpm.",
        expect: "Switching between ≈ 0.1 V (lean) and ≈ 0.8–0.9 V (rich), roughly once per second or faster. Stuck low/high or slow (> 1 s per switch) = sensor or mixture problem.",
        settings: "",
    },
    Preset {
        group: "Engine",
        name: "Crank sensor, inductive 60-2 + rpm",
        chans: &[(0, 2.0, X1, 0.0)],
        ns_div: 10_000_000,
        stream: false,
        trig: Some((0, true, 0.5, 100, false)),
        rpm: Some((0, 58.0)),
        bus: None,
        sw: None,
        an: &[(0, crate::analyze::Kind::CrankVr)],
        connect: "CH1 to the crankshaft (engine speed) sensor signal — back-probe its connector, pins from the wiring diagram — ground clip to engine ground. Crank or idle.",
        expect: "AC sine-like wave, one cycle per tooth, with one wider gap per revolution (the 2 missing teeth). Amplitude rises with rpm (≈ 1–5 V cranking, more at idle). M1 shows rpm (58 pulses/rev). Missing/irregular teeth or no gap = sensor, wheel or wiring.",
        settings: "Settings › Engine: trigger wheel (tooth positions and missing teeth, e.g. 60 − 2 or 36 − 1), cylinders, firing order and the tooth of TDC 1 — the crank analysis and the TDC markers use them. Other wheel: also set M1 'pulses/rev' (teeth present) in the Math section.",
    },
    Preset {
        group: "Engine",
        name: "Crank + cam correlation",
        chans: &[(0, 2.0, X1, 1.5), (1, 2.0, X1, -3.0)],
        // 10 ms/div: ~5-6 samples per crank tooth at idle with all 8 channels captured (20 ms/div gave 2.5 = useless)
        ns_div: 10_000_000,
        stream: false,
        trig: Some((1, true, 2.5, 100, false)),
        rpm: None,
        bus: None,
        sw: None,
        an: &[(0, crate::analyze::Kind::CrankVr), (1, crate::analyze::Kind::PulseHigh)],
        connect: "CH1 to the crankshaft sensor signal, CH2 to the camshaft (Hall) sensor signal, ground clips to engine ground. Idle.",
        expect: "Cam sensor: square wave 0 … 5 V, its edge must fall at the same place relative to the crank gap every time. A shifted edge = timing belt jumped / cam timing off; no square wave = sensor or wiring.",
        settings: "Settings › Engine: trigger wheel, cylinders, firing order, TDC 1 tooth and the cam channel — the TDC markers and the cam phase (TDC 1 vs its twin) depend on them.",
    },
    Preset {
        group: "Engine",
        name: "Injector (voltage)",
        chans: &[(0, 20.0, X20, -2.5)],
        ns_div: 1_000_000,
        stream: false,
        trig: Some((0, false, 6.0, 200, false)),
        rpm: None,
        bus: None,
        sw: None,
        an: &[(0, crate::analyze::Kind::Injector)],
        connect: "Use the ×20 attenuator. CH1 to the injector's ECU-side (switched) wire, ground clip to engine ground. Idle.",
        expect: "≈ 12–14 V, pulled to ≈ 0 V while the injector is open (Low time ≈ 2.5–4 ms at warm idle), then a flyback spike (tens of volts) when it closes. No spike = injector coil open; no pull-down = no ECU signal.",
        settings: "Settings › Probes: this test sets CH1 to the ×20 attenuator. Measuring without one? Set the probe there to ×1, or the readings are 20× too high.",
    },
    Preset {
        group: "Engine",
        name: "Ignition trigger to the output stage / coil",
        chans: &[(0, 2.0, X1, -2.0)],
        ns_div: 2_000_000,
        stream: false,
        trig: Some((0, true, 2.5, 200, false)),
        rpm: None,
        bus: None,
        sw: None,
        an: &[(0, crate::analyze::Kind::PulseHigh)],
        connect: "CH1 to the ECU's ignition trigger wire (to an igniter module or a coil with built-in driver; one coil, or one coil pair on wasted spark) (low-voltage side only — never the coil secondary), ground clip to engine ground. Idle.",
        expect: "Short 0 … 5 V pulses (dwell, a few ms) once per firing of that coil. Missing pulses = ECU / wiring; pulses present but no spark = output stage or coil.",
        settings: "",
    },
    Preset {
        group: "Engine",
        name: "Ignition primary (coil −) voltage",
        // 100 V/div, 0 V one division above the bottom: −100 … +700 V, the firing spike (200–400 V) stays on screen
        chans: &[(0, 100.0, X20, -3.0)],
        ns_div: 1_000_000,
        stream: false,
        // falling edge at the start of the dwell (coil − pulled to ground): dwell, spike, burn and ringing follow
        trig: Some((0, false, 6.0, 200, false)),
        rpm: None,
        bus: None,
        sw: None,
        an: &[(0, crate::analyze::Kind::IgnitionPrimary)],
        connect: "Use the ×20 attenuator. CH1 to the coil's primary terminal 1 / coil − (the side switched by the igniter or ECU; on coils with a built-in driver there is no coil − to reach — use 'Ignition trigger' instead), ground clip to engine ground. Idle.",
        expect: "≈ battery voltage, pulled to ≈ 0–2 V during the dwell (a few ms), then a firing spike of ≈ 200–400 V, a burn line of ≈ 30–60 V for ≈ 1–2 ms and a few ringing oscillations. Low / no spike = coil or output stage; short burn = lean mixture, worn plug or HT lead; no ringing = shorted coil windings.",
        settings: "Settings › Probes: this test sets CH1 to the ×20 attenuator. Without it the spike exceeds the input range — never measure coil − directly.",
    },
    Preset {
        group: "Engine",
        name: "Ignition coil primary current (clamp)",
        chans: &[(0, 2.0, CLAMP_LO, -3.0)],
        ns_div: 1_000_000,
        stream: false,
        trig: Some((0, true, 1.0, 200, false)),
        rpm: None,
        bus: None,
        sw: None,
        an: &[],
        connect: "Low-current clamp (100 mV/A) around the coil's primary feed wire. Idle.",
        expect: "Current ramps up during dwell to a flat limit (several A), then drops at the spark. Same shape on every coil; a slow ramp or low peak = weak coil or supply.",
        settings: "Settings › Probes: this test sets CH1 to a low-current clamp at 100 mV/A. If your clamp has another sensitivity, set it there or the amps are wrong.",
    },
];

impl App {
    /// Guided tests list: the bus tests (`bus`) or the automotive (sensor / engine / general) tests.
    pub(super) fn presets_menu(&mut self, ui: &mut egui::Ui, bus: bool) {
        let mut group = "";
        for (i, p) in PRESETS.iter().enumerate() {
            if (p.group == "Bus") != bus {
                continue;
            }
            if p.group != group {
                if !group.is_empty() {
                    ui.separator();
                }
                group = p.group;
                ui.label(egui::RichText::new(group).weak());
            }
            if ui.button(p.name).on_hover_text(p.connect).clicked() {
                self.apply_preset(i);
                ui.close();
            }
        }
    }

    /// 'Guided test ▾' drop-down at the top of a side-panel section.
    pub(super) fn presets_button(&mut self, ui: &mut egui::Ui, bus: bool) {
        let r = ui
            .menu_button("Guided tests…", |ui| self.presets_menu(ui, bus))
            .response;
        help(
            r,
            if bus {
                "One click sets channel, range, timebase, trigger and decoder for a CAN or LIN check, with a window on what to connect and what to expect."
            } else {
                "One click sets channels, ranges, timebase, trigger and the analysis for a sensor / engine check (crank, cam, injector, ignition, lambda, battery, compression, wheel speed), with a window on what to connect and what to expect."
            },
        );
    }

    pub(super) fn apply_preset(&mut self, i: usize) {
        let base = if self.sim {
            sim_channel(&PRESETS[i]).unwrap_or(0)
        } else {
            0
        };
        self.apply_preset_at(i, base);
    }

    /// Apply guided test `i` with its CH1 moved to channel `base` (the others follow).
    fn apply_preset_at(&mut self, i: usize, base: usize) {
        let p = &PRESETS[i];
        let m = |c: usize| (c + base) % CHANNELS;
        self.guide_base = base;
        // a running test run belongs to the previous setup: end it (its report opens) instead of mixing statistics
        if self.test_running() {
            self.test_finish();
        }
        // keys (↑/↓ V/div, R save reference, cursors) act on the test's channel
        self.sel_ch = m(p.chans[0].0);
        for ch in 0..CHANNELS {
            self.on[ch] = false;
        }
        for &(ch, per_div, probe, offset) in p.chans {
            let ch = m(ch);
            self.on[ch] = true;
            self.probes[ch] = Probe::from_preset(probe);
            let scale = self.probes[ch].scale.abs();
            self.vdiv[ch] = VDIVS
                .iter()
                .position(|&v| v * scale >= per_div * 0.999)
                .unwrap_or(VDIVS.len() - 1);
            self.offset[ch] = offset;
        }
        if let Some(id) =
            (0..=TB_MAX).find(|&id| !TB_SKIP.contains(&id) && burst_ns_per_div(id) == p.ns_div)
        {
            self.tb_id = id;
        }
        self.roll = p.stream.then_some(ROLL_RATES[0].1);
        if let Some((src, rising, level, pos, normal)) = p.trig {
            self.trig_src = m(src) as u8;
            self.trig_slope = if rising {
                Slope::Rising
            } else {
                Slope::Falling
            };
            self.trig_v = level;
            self.trig_pos = pos;
            self.trig_mode = if normal {
                TrigMode::Normal
            } else {
                TrigMode::Auto
            };
        }
        self.math = [Math::default(); 2];
        if let Some((a, ppr)) = p.rpm {
            let a = m(a) as _;
            self.math[0] = Math {
                op: MathOp::Rpm,
                a,
                b: a,
                k: ppr,
                vdiv: math_div_for(500.0),
                offset: -3.0,
                pane: 0,
            };
        }
        // a guided test starts clean: no decoder / TDC markers / meter mode / extra plots from the previous one
        self.decoder.off();
        self.engine.on = false;
        self.meter.on = false;
        self.set_panes(1);
        // crank presets also switch on the TDC markers (CH1 = crank; the correlation preset has the cam on CH2)
        if p.name.starts_with("Crank") {
            self.engine
                .set_channels(m(0), (p.chans.len() > 1).then_some(m(1)));
        }
        self.sw = p.sw.unwrap_or(SwTrig::Off);
        self.kinds = [crate::analyze::Kind::None; CHANNELS];
        for &(ch, k) in p.an {
            self.kinds[m(ch)] = k;
        }
        if let Some(bus) = p.bus {
            self.decoder.set(m(p.chans[0].0), bus);
        }
        self.zoom = Zoom::default();
        if self.run != Run::Running {
            self.set_run(Run::Running);
        }
        self.guide = Some(i);
    }

    /// Small window with what to connect and what to expect for the applied guided test.
    pub(super) fn guide_window(&mut self, ctx: &egui::Context) {
        // debug aid like HANTEK_SHOT: HANTEK_PRESET=n applies guided test n (0-based) at start
        static ENV: std::sync::Once = std::sync::Once::new();
        let mut env = None;
        ENV.call_once(|| {
            env = std::env::var("HANTEK_PRESET")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
        });
        if let Some(i) = env.filter(|&i| i < PRESETS.len()) {
            self.apply_preset(i);
        }
        let Some(i) = self.guide else { return };
        let p = &PRESETS[i];
        let mut open = true;
        egui::Window::new(format!("Guided test: {}", p.name)).open(&mut open).default_width(380.0).show(ctx, |ui| {
            ui.label(egui::RichText::new("Connect").strong());
            ui.label(shift_ch(p.connect, self.guide_base));
            ui.add_space(4.0);
            ui.label(egui::RichText::new("What you should see").strong());
            ui.label(shift_ch(p.expect, self.guide_base));
            if !p.settings.is_empty() {
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Also set in Settings (not in the side panel)").strong().color(egui::Color32::from_rgb(255, 200, 90)));
                ui.label(shift_ch(p.settings, self.guide_base));
                if help(ui.button("Open Settings"), "Opens the Settings window (also F2).").clicked() {
                    self.settings_open = true;
                }
            }
            ui.add_space(4.0);
            // step status of the test's first analysed channel: waiting → signal → result
            let mut fit = None;
            if let Some(&(c0, _)) = p.an.first() {
                let ch = (c0 + self.guide_base) % CHANNELS;
                let st = &self.ana[ch];
                ui.add_space(2.0);
                let (txt, col) = if !self.on[ch] {
                    (format!("CH{} is off — switch it on (Shift+{})", ch + 1, ch + 1), egui::Color32::from_rgb(255, 200, 90))
                } else if !st.present {
                    (format!("1. Waiting for a signal on CH{} — {}", ch + 1, crate::analyze::waiting_hint(self.kinds[ch])), egui::Color32::from_rgb(255, 200, 90))
                } else {
                    match &st.verdict {
                        // some signals (pulses: Hall, trigger, PWM) are measured but have no pass / fail rule
                        None if !st.readouts().is_empty() => (
                            format!("3. Signal on CH{} — readings in the box (no pass / fail rule for this signal)", ch + 1),
                            egui::Color32::from_rgb(170, 240, 255),
                        ),
                        None => (format!("2. Signal on CH{} — analysing…", ch + 1), egui::Color32::from_rgb(170, 240, 255)),
                        Some((true, t)) => (format!("3. Result: OK — {t}"), egui::Color32::from_rgb(120, 220, 140)),
                        Some((false, t)) => (format!("3. Result: CHECK — {t}"), egui::Color32::from_rgb(255, 120, 100)),
                    }
                };
                ui.label(egui::RichText::new(txt).color(col).strong());
                // offer to fit V/div, position and trigger when the signal is tiny or doesn't fit the screen
                if st.present {
                    let (lo, hi) = st.lohi;
                    let ud = self.udiv(ch);
                    let off = self.offset[ch];
                    // a battery is a DC level: a small swing is right, fitting it would zoom into noise
                    let small = hi - lo < 1.5 * ud && self.kinds[ch] != crate::analyze::Kind::Battery;
                    let outside = hi / ud + off > DIV_Y / 2.0 || lo / ud + off < -DIV_Y / 2.0;
                    if (small || outside) && help(ui.button("Fit to signal"), "Sets this channel's V/div so the signal fills about 5 divisions, centres it and puts the trigger level in its middle.").clicked() {
                        fit = Some((ch, lo, hi));
                    }
                }
            }
            if let Some((ch, lo, hi)) = fit {
                let scale = self.probes[ch].scale.abs().max(1e-9);
                let per_div = ((hi - lo) / 5.0).max(1e-6);
                self.vdiv[ch] = VDIVS.iter().position(|&v| v * scale >= per_div * 0.999).unwrap_or(VDIVS.len() - 1);
                let mid = (lo + hi) / 2.0;
                self.offset[ch] = (-mid / self.udiv(ch)).clamp(-DIV_Y / 2.0, DIV_Y / 2.0);
                if self.trig_src as usize == ch {
                    self.trig_v = mid;
                }
            }
            let mut base = self.guide_base;
            ui.horizontal(|ui| {
                ui.label("Use channel");
                egui::ComboBox::from_id_salt("guide_base").width(56.0).selected_text(format!("CH{}", base + 1)).show_ui(ui, |ui| {
                    for c in 0..CHANNELS {
                        ui.selectable_value(&mut base, c, format!("CH{}", c + 1));
                    }
                });
                ui.label(egui::RichText::new(match (self.sim, sim_channel(p)) {
                    (true, Some(_)) => "(simulator: preset to the channel with this signal)",
                    (true, None) => "(the simulator has no signal for this test — connect the scope)",
                    _ => "for CH1 of the test (the others follow)",
                }).weak().small());
            });
            if base != self.guide_base {
                self.apply_preset_at(i, base);
            }
            ui.label(egui::RichText::new("Values are typical guidance — check pins and specs in the wiring / repair manual. Settings stay as set until you change them.").weak().small());
        });
        if !open {
            self.guide = None;
        }
    }
}

/// Simulator channel that carries a guided test's signal (see acq.rs `Sim::sample`), so the tests show something in --sim.
impl App {
    /// Name of the open guided test (screenshots).
    pub(super) fn guide_name(&self) -> Option<&'static str> {
        self.guide.map(|i| PRESETS[i].name)
    }

    /// The user changed a channel's signal type: if the open guided test is about that channel, its text no longer
    /// fits → close the guide window (the settings stay as they are).
    pub(super) fn guide_kind_changed(&mut self, ch: usize) {
        if let Some(i) = self.guide {
            if PRESETS[i]
                .an
                .iter()
                .any(|&(c, _)| (c + self.guide_base) % CHANNELS == ch)
            {
                self.guide = None;
            }
        }
    }
}

/// The guide texts name the test's own channels (CH1, CH2); shift them to where the test was moved.
fn shift_ch(text: &str, base: usize) -> String {
    if base == 0 {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("CH") {
        out.push_str(&rest[..i]);
        let d = rest[i + 2..]
            .chars()
            .next()
            .and_then(|c| c.to_digit(10))
            .map(|d| d as usize);
        match d {
            Some(d) if (1..=CHANNELS).contains(&d) => {
                out.push_str(&format!("CH{}", (d - 1 + base) % CHANNELS + 1));
                rest = &rest[i + 3..];
            }
            _ => {
                out.push_str("CH");
                rest = &rest[i + 2..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// None = the simulator has no such signal.
fn sim_channel(p: &Preset) -> Option<usize> {
    use crate::analyze::Kind;
    match (p.bus, p.an.first().map(|a| a.1)) {
        (Some(b), _) if b.starts_with("lin") => Some(7),
        (Some(_), _) => Some(6),
        (_, Some(Kind::CrankVr)) => Some(2), // crank CH3, cam CH4
        (_, Some(Kind::Injector)) => Some(5),
        (_, Some(Kind::IgnitionPrimary)) => Some(4),
        (_, Some(Kind::PulseHigh)) => Some(1), // ignition trigger
        (_, Some(Kind::Lambda)) => Some(1),
        (_, Some(Kind::Battery)) => Some(3),
        (_, Some(Kind::Compression)) => Some(4),
        (_, Some(Kind::WheelSpeed)) => Some(0), // a 1 kHz sine stands in for a VR sensor
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guide_texts_follow_the_channel() {
        assert_eq!(
            shift_ch("CH1 crank, CH2 cam; M1 rpm", 2),
            "CH3 crank, CH4 cam; M1 rpm"
        );
        assert_eq!(shift_ch("CH8 and CH1", 1), "CH1 and CH2");
        assert_eq!(shift_ch("CHECK CH", 3), "CHECK CH");
        assert_eq!(shift_ch("CH1", 0), "CH1");
    }

    #[test]
    fn presets_are_consistent() {
        for p in PRESETS {
            assert!(!p.chans.is_empty(), "{}", p.name);
            for &(ch, per_div, probe, off) in p.chans {
                assert!(
                    ch < CHANNELS
                        && probe < PROBE_PRESETS.len()
                        && per_div > 0.0
                        && off.abs() <= DIV_Y / 2.0,
                    "{}",
                    p.name
                );
                // the scale exists for that probe
                let scale = PROBE_PRESETS[probe].1;
                assert!(
                    VDIVS
                        .iter()
                        .any(|&v| (v * scale - per_div).abs() < per_div * 0.01),
                    "{}: {per_div}/div not a step",
                    p.name
                );
            }
            assert!(
                (0..=TB_MAX).any(|id| !TB_SKIP.contains(&id) && burst_ns_per_div(id) == p.ns_div),
                "{}: timebase",
                p.name
            );
            if let Some((src, ..)) = p.trig {
                assert!(
                    p.chans.iter().any(|c| c.0 == src),
                    "{}: trigger source not on",
                    p.name
                );
            }
        }
    }
}
