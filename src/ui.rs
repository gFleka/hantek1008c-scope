//! egui app: controls (right), scope view (centre), status bar (bottom).
//! V/div is a software zoom; the hardware range follows it (smallest range covering the screen).

use crate::acq::{self, Acq, Cmd, HEntry, Mode, Run, Snapshot};
use crate::hantek::*;
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, Vec2};
use std::time::Instant;

#[path = "decode.rs"]
mod decode;
#[path = "engine.rs"]
mod engine;
#[path = "files.rs"]
mod files;
#[path = "meter.rs"]
mod meter;
#[path = "presets.rs"]
mod presets;
#[path = "testrun.rs"]
mod testrun;
#[path = "waves.rs"]
mod waves;

/// Speed-review instrumentation: CPU time per section of `App::ui` (time since the previous mark goes to the
/// named section). Off unless a test enables it on its thread: then a mark is one thread-local bool check.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod prof {
    use std::cell::{Cell, RefCell};
    use std::time::Instant;
    thread_local! {
        static ON: Cell<bool> = const { Cell::new(false) };
        static ACC: RefCell<(Option<Instant>, Vec<(&'static str, f64, u32)>)> = const { RefCell::new((None, Vec::new())) };
    }
    pub fn enable(on: bool) {
        ON.with(|o| o.set(on));
    }
    /// Start of a measured stretch (nothing is attributed).
    #[inline]
    pub fn start() {
        if ON.with(|o| o.get()) {
            ACC.with(|a| a.borrow_mut().0 = Some(Instant::now()));
        }
    }
    /// Time since the previous mark / start → `name`.
    #[inline]
    pub fn mark(name: &'static str) {
        if !ON.with(|o| o.get()) {
            return;
        }
        let now = Instant::now();
        ACC.with(|a| {
            let mut a = a.borrow_mut();
            let dt =
                a.0.map_or(0.0, |t| now.duration_since(t).as_secs_f64() * 1e3);
            a.0 = Some(now);
            match a.1.iter_mut().find(|e| e.0 == name) {
                Some(e) => {
                    e.1 += dt;
                    e.2 += 1;
                }
                None => a.1.push((name, dt, 1)),
            }
        });
    }
    /// (section, total ms, calls) since the last take, in first-seen order.
    pub fn take() -> Vec<(&'static str, f64, u32)> {
        ACC.with(|a| std::mem::take(&mut a.borrow_mut().1))
    }
}

const DIV_X: f32 = 10.0;
/// Statistics over a custom range, built up incrementally (see `current_stats`).
#[derive(Clone, Copy)]
struct RangeAcc {
    /// the user's range setting (from, to) it belongs to
    set: (Option<u64>, Option<u64>),
    /// next capture number to add
    next: u64,
    stats: Stats,
}

/// Statistics work per frame (outside the history lock).
const STATS_BUDGET: std::time::Duration = std::time::Duration::from_millis(2);

/// Width of the collapsed side panel (F4).
const PANEL_MIN_W: f32 = 60.0;

/// Base width of the side panel (it grows when its content needs more).
const PANEL_W: f32 = 290.0;
const DIV_Y: f32 = 8.0;
/// Display V/div steps.
const VDIVS: [f32; 13] = [
    0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0,
];
const COLORS: [Color32; CHANNELS] = [
    Color32::from_rgb(255, 214, 10),  // yellow
    Color32::from_rgb(0, 200, 255),   // cyan
    Color32::from_rgb(255, 80, 200),  // magenta
    Color32::from_rgb(60, 220, 90),   // green
    Color32::from_rgb(255, 140, 40),  // orange
    Color32::from_rgb(150, 120, 255), // violet
    Color32::from_rgb(255, 90, 90),   // red
    Color32::from_rgb(220, 220, 220), // white
];
/// Usable half-span in volts of a hardware range (±~2000 counts around zero).
fn range_half_span(vs: VScale) -> f32 {
    1900.0 * volts_per_count(vs)
}

pub struct App {
    acq: Acq,
    // user settings
    on: [bool; CHANNELS],
    vdiv: [usize; CHANNELS], // index into VDIVS
    offset: [f32; CHANNELS], // divisions
    tb_id: u8,
    roll: Option<u8>,
    run: Run,
    trig_src: u8,
    trig_slope: Slope,
    trig_v: f32,
    trig_mode: TrigMode,
    trig_pos: u16,
    sent: Option<Config>,
    // perf readouts
    last: Instant,
    ui_fps: f32,
    paint_ms: f32,
    perf_log: Instant,
    pts: Vec<Pos2>,
    drag: Option<Drag>,
    last_seq: u64,
    last_frame_at: Instant,
    range_changed_at: Option<Instant>,
    /// last acquisition mode / stream length sent to the acq thread
    mode_sent: Option<Option<u8>>,
    keep_sent: usize,
    /// gain calibration (calibration.txt), applied to every volts conversion
    cal: Cal,
    /// capture number shown from the history (None = live / latest)
    view: Option<u64>,
    /// statistics range (None = whole history)
    stats_from: Option<u64>,
    stats_to: Option<u64>,
    /// statistics over a custom history range (key = from, to, first_no)
    range_stats: Option<RangeAcc>,
    /// measurement table per channel: (data key, when, result) — recomputed only for new data, ≤ 10×/s in live streams
    meas_cache: [(u64, Option<Instant>, Meas); CHANNELS],
    /// FFT windows: (data key, when, bin width, dB spectrum) — recomputed only for new data, ≤ 10×/s in live streams
    fft_cache: [(u64, Option<Instant>, f64, Vec<f32>); 2],
    /// running statistics over all captures since start / Reset, fed incrementally
    acc_all: Stats,
    acc_next: u64,
    show_stats: bool,
    /// measurement table shown (F3); off = one line, nothing measured (8 channels fill half the screen)
    meas_on: bool,
    /// visible part of the screen (zoom/pan), undo stack, box-zoom drag origin
    zoom: Zoom,
    zoom_undo: Vec<Zoom>,
    /// where the plot menu was opened (right-click)
    menu_at: Option<Pos2>,
    box_from: Option<Pos2>,
    /// software trigger, its matches in the history (ascending capture numbers), scan position
    sw: SwTrig,
    hits: Vec<u64>,
    hits_next: u64,
    hits_for: Option<(SwTrig, u8, f32, SwLevel)>,
    /// threshold the match filter measures pulse widths / dropouts at
    sw_level: SwLevel,
    /// stop capturing at the first match after arming (Run / ticking the box)
    sw_stop: bool,
    sw_arm_from: u64,
    /// next box drag defines the zone instead of zooming
    zone_draw: bool,
    /// debug screenshot (HANTEK_SHOT=file.ppm, HANTEK_SHOT_AFTER=seconds)
    started: Instant,
    shot_done: bool,
    /// stream recording: right-edge row when browsing, cached window, match search state
    stream_end: Option<u64>,
    stream_cache: Option<((u64, u64, u64), Snapshot)>,
    stream_hits: Vec<u64>,
    stream_scan: u64,
    stream_hits_for: Option<(SwTrig, u8, f32, SwLevel)>,
    stream_arm_from: u64,
    /// channel the ↑/↓ keys act on; shortcut overlay open
    sel_ch: usize,
    cursors: Cursors,
    /// persistence view: draw the previous `persist_n` captures faded under the live trace
    persist_on: bool,
    persist_n: usize,
    /// per pane: (view+captures key, cached persistence image)
    persist_tex: Vec<Option<PersistTex>>,
    /// math channels M1, M2
    math: [Math; 2],
    /// auto-set in progress, and its last message (shown in the info line for a few seconds)
    autoset: Option<AutoSet>,
    autoset_msg: Option<(String, Instant)>,
    /// settings before the last auto-set (Backspace restores them)
    autoset_undo: Option<ViewUndo>,
    /// status bar with UI fps / paint / USB timings (Settings)
    show_perf: bool,
    /// samples/s per channel of the stream on screen (see `ui`)
    shown_rate: f64,
    /// adaptive feature dropout (Settings) and its state; CPU work per frame (ms, smoothed)
    adaptive: bool,
    /// info-line notices shown only after 0.4 s and kept 2 s (no flicker at thresholds)
    notice_debounce: NoticeDebounce,
    /// guided test whose guide window is open (Menu or the side panel sections)
    guide: Option<usize>,
    /// meter mode (M): big numbers instead of plots
    meter: meter::Meter,
    /// engine-cycle TDC markers (Settings > Engine)
    engine: engine::Engine,
    /// LIN / K-line decoder (side panel: Protocol decoding)
    decoder: decode::Decode,
    /// signal type per channel for the waveform phase analysis
    kinds: [crate::analyze::Kind; CHANNELS],
    /// built-in typical waveform drawn for analysed channels (Automotive tests)
    typical_on: bool,
    /// side panel collapsed to a narrow column of short buttons (F4): more room for the waveform
    panel_min: bool,
    /// analysis bands + labelled strip over the trace (the readout box stays while a signal type is set)
    highlight_on: bool,
    /// side panel width: grows to its content (cap 45 % of the window), back to the base when its layout changes
    panel_w: f32,
    panel_key: u64,
    /// last frame: how much wider the panel content was than the panel (> 0 = something was cut off)
    panel_over: f32,
    /// screenshots get an info strip appended below (file name + PNG metadata always)
    shot_strip: bool,
    /// per-channel analysis with memory (calm display), updated once per new capture
    ana: [waves::ChanAna; CHANNELS],
    /// simulator source (guided tests default to the channel that carries the matching simulated signal)
    sim: bool,
    /// guided test: preset CH1 is moved to this channel
    guide_base: usize,
    /// test run (injector / coil / sensor test with a report); report window shown / auto-opened once
    test: testrun::TestRun,
    test_show: bool,
    test_shown_once: bool,
    dropout: Dropout,
    work_ms: f32,
    /// reference waveforms R1/R2 (known-good captures) and whether they are drawn
    refs: [Option<files::RefWave>; 2],
    refs_on: bool,
    /// reference slot edited (show / channel / offset): saved 0.5 s after the last change, not every drag frame
    ref_dirty: [Option<Instant>; 2],
    /// save the shown capture as reference R1/R2 (menu, keys)
    ref_req: Option<usize>,
    /// shift each capture so the real trigger crossing sits on the marker (Settings)
    trig_corr: bool,
    /// recording to disk, export, open (src/files.rs)
    files: files::Files,
    /// what is connected to each channel (scale, offset, unit)
    probes: [Probe; CHANNELS],
    show_keys: bool,
    /// Tab / Shift+digit taken out of the raw input (see raw_input_hook), handled once per frame
    tab_pressed: bool,
    /// units/channels the running statistics were computed with
    acc_units: u64,
    /// newest frame when Single was pressed (the single capture is the first one after it)
    single_seq: u64,
    shift_digit: Option<usize>,
    /// Settings window open; memory limit (MB) for captures + stream recording
    settings_open: bool,
    mem_mb: usize,
    /// number of plot panes, pane per channel, relative pane heights
    panes: usize,
    pane_of: [usize; CHANNELS],
    pane_h: [f32; MAX_PANES],
    /// history capture number on screen (None = live)
    shown_no: Option<u64>,
    saved_hash: u64,
    saved_at: Instant,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, sim: bool) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        // a press held for the help tooltip (0.5 s, see help()) must not also count as a click on release
        cc.egui_ctx
            .options_mut(|o| o.input_options.max_click_duration = 0.45);
        let ctx = cc.egui_ctx.clone();
        let mut app = App {
            acq: acq::spawn(sim, Config::default(), move || ctx.request_repaint()),
            on: [true, true, false, false, false, false, false, false],
            vdiv: [9; CHANNELS], // 5 V/div
            offset: [0.0; CHANNELS],
            tb_id: burst_id_for_ns(500_000).unwrap(),
            roll: None,
            run: Run::Running,
            trig_src: 0,
            trig_slope: Slope::Rising,
            trig_v: 1.0,
            trig_mode: TrigMode::Auto,
            trig_pos: 500,
            sent: None,
            last: Instant::now(),
            ui_fps: 0.0,
            paint_ms: 0.0,
            perf_log: Instant::now(),
            pts: Vec::with_capacity(4096),
            drag: None,
            last_seq: 0,
            last_frame_at: Instant::now(),
            range_changed_at: None,
            mode_sent: None,
            keep_sent: 0,
            cal: Cal::load(),
            view: None,
            stats_from: None,
            stats_to: None,
            range_stats: None,
            meas_cache: [(0, None, Meas::default()); CHANNELS],
            fft_cache: [(0, None, 0.0, Vec::new()), (0, None, 0.0, Vec::new())],
            acc_all: Default::default(),
            acc_next: 0,
            show_stats: false,
            meas_on: true,
            zoom: Zoom::default(),
            zoom_undo: Vec::new(),
            menu_at: None,
            box_from: None,
            sw: SwTrig::Off,
            hits: Vec::new(),
            hits_next: 0,
            hits_for: None,
            zone_draw: false,
            started: Instant::now(),
            shot_done: false,
            stream_end: None,
            stream_cache: None,
            stream_hits: Vec::new(),
            stream_scan: 0,
            stream_hits_for: None,
            stream_arm_from: 0,
            sel_ch: 0,
            cursors: Cursors::default(),
            persist_on: false,
            persist_n: 30,
            persist_tex: Vec::new(),
            math: [Math::default(); 2],
            autoset: None,
            autoset_msg: None,
            autoset_undo: None,
            show_perf: false,
            shown_rate: 1.0,
            adaptive: true,
            notice_debounce: NoticeDebounce::default(),
            guide: None,
            meter: meter::Meter::default(),
            engine: engine::Engine::default(),
            decoder: decode::Decode::default(),
            kinds: [crate::analyze::Kind::None; CHANNELS],
            typical_on: true,
            panel_min: false,
            highlight_on: true,
            panel_w: PANEL_W,
            panel_key: 0,
            panel_over: 0.0,
            shot_strip: true,
            ana: Default::default(),
            sim,
            guide_base: 0,
            test: testrun::TestRun::default(),
            test_show: false,
            test_shown_once: false,
            dropout: Dropout::default(),
            work_ms: 0.0,
            refs: [None, None],
            refs_on: true,
            ref_dirty: [None, None],
            ref_req: None,
            trig_corr: true,
            files: files::Files::load(),
            probes: std::array::from_fn(|_| Probe::default()),
            show_keys: false,
            tab_pressed: false,
            acc_units: 0,
            single_seq: 0,
            shift_digit: None,
            settings_open: false,
            mem_mb: crate::acq::HISTORY_BUDGET / (1024 * 1024),
            panes: 1,
            pane_of: [0; CHANNELS],
            pane_h: [1.0; MAX_PANES],
            sw_level: SwLevel::Auto,
            sw_stop: false,
            sw_arm_from: 0,
            shown_no: None,
            saved_hash: 0,
            saved_at: Instant::now(),
        };
        app.load_settings();
        app.load_refs();
        if let Ok(mut h) = app.acq.history.lock() {
            h.set_budget(app.mem_mb * 1024 * 1024);
        }
        app.saved_hash = app.settings_hash();
        app.push_config(&Snapshot::default());
        app
    }

    // ── settings persistence: small key=value text file, saved ≤ 1×/s after a change ──

    fn settings_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            self.on,
            self.vdiv,
            self.tb_id,
            self.roll,
            self.trig_src,
            self.trig_slope as u8,
            self.trig_mode as u8,
            self.trig_pos,
        )
            .hash(&mut h);
        sw_encode(&self.sw).hash(&mut h); // tiny string per frame; keeps the hash simple
        (self.sw_stop, sw_level_encode(self.sw_level)).hash(&mut h);
        (self.panes, self.pane_of).hash(&mut h);
        (self.persist_on, self.persist_n).hash(&mut h);
        self.math
            .iter()
            .map(Math::encode)
            .collect::<Vec<_>>()
            .hash(&mut h);
        (
            self.mem_mb,
            self.show_perf,
            self.trig_corr,
            self.adaptive,
            self.shot_strip,
            self.highlight_on,
            self.typical_on,
            (self.panel_min, self.meas_on),
            self.meter.on,
            self.engine.encode(),
            self.decoder.encode(),
            self.kinds.map(waves::kind_index),
        )
            .hash(&mut h);
        for p in &self.probes {
            (p.preset, p.scale.to_bits(), p.offset.to_bits(), &p.unit).hash(&mut h);
        }
        for v in self.pane_h {
            v.to_bits().hash(&mut h);
        }
        for v in self.offset.iter().chain(std::iter::once(&self.trig_v)) {
            v.to_bits().hash(&mut h);
        }
        h.finish()
    }

    fn save_settings_if_changed(&mut self) {
        let h = self.settings_hash();
        if h == self.saved_hash || self.saved_at.elapsed().as_secs_f32() < 1.0 {
            return;
        }
        let Some(path) = settings_path() else { return };
        let join = |v: &mut dyn Iterator<Item = String>| v.collect::<Vec<_>>().join(",");
        let txt = format!(
            "on={}\nvdiv={}\noffset={}\ntb={}\nroll={}\ntrig_src={}\ntrig_slope={}\ntrig_v={}\ntrig_mode={}\ntrig_pos={}\nsw={}\nsw_level={}\nsw_stop={}\npanes={}\npane_of={}\npane_h={}\nprobes={}\npersist={}\nmath={}\nmem_mb={}\nshow_perf={}\ntrig_corr={}\nadaptive={}\nshot_strip={}\nhighlight={}\ntypical={}\npanel_min={}\nmeas={}\nmeter={}\nengine={}\ndecode={}\nanalysis={}\n",
            join(&mut self.on.iter().map(|&b| (b as u8).to_string())),
            join(&mut self.vdiv.iter().map(|v| v.to_string())),
            join(&mut self.offset.iter().map(|v| v.to_string())),
            self.tb_id,
            self.roll.map_or("-".to_string(), |r| r.to_string()),
            self.trig_src,
            if self.trig_slope == Slope::Rising { "rising" } else { "falling" },
            self.trig_v,
            match self.trig_mode {
                TrigMode::Auto => "auto",
                TrigMode::Normal => "normal",
                TrigMode::Free => "free",
            },
            self.trig_pos,
            sw_encode(&self.sw),
            sw_level_encode(self.sw_level),
            self.sw_stop as u8,
            self.panes,
            join(&mut self.pane_of.iter().map(|v| v.to_string())),
            join(&mut self.pane_h.iter().map(|v| format!("{v:.3}"))),
            self.probes.iter().map(Probe::encode).collect::<Vec<_>>().join(","),
            format!("{},{}", self.persist_on as u8, self.persist_n),
            self.math.iter().map(Math::encode).collect::<Vec<_>>().join(","),
            self.mem_mb,
            self.show_perf as u8,
            self.trig_corr as u8,
            self.adaptive as u8,
            self.shot_strip as u8,
            self.highlight_on as u8,
            self.typical_on as u8,
            self.panel_min as u8,
            self.meas_on as u8,
            self.meter.on as u8,
            self.engine.encode(),
            self.decoder.encode(),
            self.kinds.iter().map(|&k| waves::kind_index(k).to_string()).collect::<Vec<_>>().join(","),
        );
        let ok = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(&path, txt));
        if let Err(e) = ok {
            log::warn!("saving settings to {}: {e}", path.display());
        }
        self.saved_hash = h;
        self.saved_at = Instant::now();
    }

    fn load_settings(&mut self) {
        let Some(txt) = settings_path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return;
        };
        for line in txt.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let list = |v: &str| {
                v.split(',')
                    .map(str::trim)
                    .map(String::from)
                    .collect::<Vec<_>>()
            };
            match k.trim() {
                "on" => list(v)
                    .iter()
                    .take(CHANNELS)
                    .enumerate()
                    .for_each(|(i, s)| self.on[i] = s == "1"),
                "vdiv" => list(v)
                    .iter()
                    .take(CHANNELS)
                    .enumerate()
                    .for_each(|(i, s)| {
                        if let Ok(x) = s.parse::<usize>() {
                            self.vdiv[i] = x.min(VDIVS.len() - 1);
                        }
                    }),
                "offset" => list(v)
                    .iter()
                    .take(CHANNELS)
                    .enumerate()
                    .for_each(|(i, s)| {
                        if let Some(x) = s.parse::<f32>().ok().filter(|x| !x.is_nan()) {
                            self.offset[i] = x.clamp(-4.0, 4.0);
                        }
                    }),
                "tb" => {
                    if let Ok(x) = v.trim().parse::<u8>() {
                        self.tb_id = x.min(TB_MAX);
                        if TB_SKIP.contains(&self.tb_id) {
                            self.tb_id += 1; // next valid (slower) timebase
                        }
                    }
                }
                "roll" => {
                    self.roll = v
                        .trim()
                        .parse::<u8>()
                        .ok()
                        .filter(|id| ROLL_RATES.iter().any(|r| r.1 == *id))
                }
                "trig_src" => {
                    if let Ok(x) = v.trim().parse::<u8>() {
                        self.trig_src = x.min(CHANNELS as u8 - 1);
                    }
                }
                "trig_slope" => {
                    self.trig_slope = if v.trim() == "falling" {
                        Slope::Falling
                    } else {
                        Slope::Rising
                    }
                }
                "trig_v" => {
                    self.trig_v = v
                        .trim()
                        .parse::<f32>()
                        .ok()
                        .filter(|x| x.is_finite())
                        .unwrap_or(self.trig_v)
                }
                "trig_mode" => {
                    self.trig_mode = match v.trim() {
                        "normal" => TrigMode::Normal,
                        "free" => TrigMode::Free,
                        _ => TrigMode::Auto,
                    }
                }
                "sw" => self.sw = sw_decode(v.trim()).unwrap_or(SwTrig::Off),
                "sw_level" => self.sw_level = sw_level_decode(v),
                "sw_stop" => self.sw_stop = v.trim() == "1",
                "show_perf" => self.show_perf = v.trim() != "0",
                "adaptive" => self.adaptive = v.trim() != "0",
                "shot_strip" => self.shot_strip = v.trim() != "0",
                "highlight" => self.highlight_on = v.trim() != "0",
                "typical" => self.typical_on = v.trim() != "0",
                "panel_min" => self.panel_min = v.trim() == "1",
                "meas" => self.meas_on = v.trim() != "0",
                "meter" => self.meter.on = v.trim() == "1",
                "analysis" => v.split(',').take(CHANNELS).enumerate().for_each(|(i, x)| {
                    if let Some(k) = x
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| crate::analyze::KINDS.get(n))
                    {
                        self.kinds[i] = k.0;
                    }
                }),
                "decode" => {
                    if let Some(d) = decode::Decode::decode(v.trim()) {
                        self.decoder = d;
                    }
                }
                "engine" => {
                    if let Some(e) = engine::Engine::decode(v.trim()) {
                        self.engine = e;
                    }
                }
                "trig_corr" => self.trig_corr = v.trim() != "0",
                "mem_mb" => {
                    self.mem_mb = v
                        .trim()
                        .parse::<usize>()
                        .unwrap_or(512)
                        .clamp(64, 16 * 1024)
                }
                "math" => v.split(',').take(2).enumerate().for_each(|(i, s)| {
                    if let Some(m) = Math::decode(s.trim()) {
                        self.math[i] = m;
                    }
                }),
                "persist" => {
                    let mut it = v.split(',');
                    self.persist_on = it.next().is_some_and(|x| x.trim() == "1");
                    self.persist_n = it
                        .next()
                        .and_then(|x| x.trim().parse().ok())
                        .unwrap_or(30)
                        .clamp(2, 100);
                }
                "probes" => v.split(',').take(CHANNELS).enumerate().for_each(|(i, s)| {
                    if let Some(p) = Probe::decode(s.trim()) {
                        self.probes[i] = p;
                    }
                }),
                "panes" => self.panes = v.trim().parse::<usize>().unwrap_or(1).clamp(1, MAX_PANES),
                "pane_of" => list(v)
                    .iter()
                    .take(CHANNELS)
                    .enumerate()
                    .for_each(|(i, s)| {
                        self.pane_of[i] = s.parse::<usize>().unwrap_or(0).min(MAX_PANES - 1)
                    }),
                "pane_h" => list(v)
                    .iter()
                    .take(MAX_PANES)
                    .enumerate()
                    .for_each(|(i, s)| {
                        if let Some(x) = s.parse::<f32>().ok().filter(|x| !x.is_nan()) {
                            self.pane_h[i] = x.clamp(0.01, 100.0);
                        }
                    }),
                "trig_pos" => {
                    if let Ok(x) = v.trim().parse::<u16>() {
                        self.trig_pos = x.min(1000);
                    }
                }
                _ => {}
            }
        }
        if !self.on.iter().any(|&b| b) {
            self.on[0] = true;
        }
    }

    /// Smallest hardware range whose span covers the visible screen for this channel.
    fn hw_range(&self, ch: usize) -> VScale {
        let need = (DIV_Y / 2.0 + self.offset[ch].abs()) * VDIVS[self.vdiv[ch]];
        VScale::ALL
            .into_iter()
            .find(|&vs| range_half_span(vs) >= need)
            .unwrap_or(VScale::X1)
    }

    /// The scope configuration the current settings ask for (pure; `push_config` sends it when it changed).
    pub(super) fn wanted_config(&self, snap: &Snapshot) -> Config {
        // capture only the shown channels where the firmware does that correctly (more samples per channel),
        // otherwise all 8 (review #3). Roll mode: fewer channels = faster, proven clean.
        let partial = self.roll.is_some() || partial_capture_ok(self.tb_id);
        let mut c = Config {
            active: match self.decoder.fast_channel() {
                // experimental fast CAN: only the bus channel (full 2.4 MS/s); the firmware's <8-channel glitch is accepted
                Some(ch) => std::array::from_fn(|c| c == ch),
                None if partial => self.on,
                None => [true; CHANNELS],
            },
            timebase_id: self.tb_id,
            ..Config::default()
        };
        for ch in 0..CHANNELS {
            c.vscale[ch] = self.hw_range(ch);
        }
        let s = self.trig_src as usize;
        c.trig_src = self.trig_src;
        c.trig_slope = self.trig_slope;
        c.trig_mode = self.trig_mode;
        c.trig_pos = self.trig_pos;
        let zero = if snap.zero[0][0] > 0.0 {
            snap.zero[c.vscale[s].index()][s]
        } else {
            2048.0
        };
        // trigger level is set in the trigger channel's unit → counts
        let (zu, vu) = self.uconv(s, c.vscale[s], zero);
        c.trig_level = (zu + self.trig_v / vu).clamp(0.0, 4095.0) as u16;
        c
    }

    fn push_config(&mut self, snap: &Snapshot) {
        let c = self.wanted_config(snap);
        if self.sent.as_ref() != Some(&c) {
            if self.sent.as_ref().is_some_and(|o| o.vscale != c.vscale) {
                self.range_changed_at = Some(Instant::now());
            }
            let _ = self.acq.tx.send(Cmd::Config(c.clone()));
            self.sent = Some(c);
        }
    }

    fn set_run(&mut self, r: Run) {
        if r == Run::Running && self.run != Run::Running {
            self.arm_stop_on_match(); // pressing Run re-arms "stop on match"
        }
        if r == Run::Single {
            self.single_seq = self.acq.out.output_buffer().frame.seq;
        }
        self.run = r;
        let _ = self.acq.tx.send(Cmd::Run(r));
    }

    /// Collapsed side panel (F4): the most used controls as short buttons, each with its help; the rest is one
    /// click away (expand) or in Menu / the keyboard.
    fn compact_controls(&mut self, ui: &mut egui::Ui) {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend); // 'Stop', 'Menu' on one line
        let w = ui.available_width();
        let btn = |ui: &mut egui::Ui, text: egui::RichText, selected: bool| {
            ui.add(
                egui::Button::new(text)
                    .selected(selected)
                    .min_size(Vec2::new(w, 26.0)),
            )
        };
        if help(
            btn(ui, egui::RichText::new("<<"), false),
            "Expand the side panel (all controls). Key: F4",
        )
        .clicked()
        {
            self.panel_min = false;
        }
        ui.separator();
        let running = self.run == Run::Running;
        if help(
            btn(
                ui,
                egui::RichText::new(if running { "Stop" } else { "Run" }).strong(),
                running,
            ),
            "Run / Stop. Key: Space",
        )
        .clicked()
        {
            self.set_run(if running { Run::Stopped } else { Run::Running });
        }
        if help(
            btn(ui, egui::RichText::new("1x"), self.run == Run::Single),
            "Single capture. Key: S",
        )
        .clicked()
        {
            self.set_run(Run::Single);
        }
        if help(
            btn(ui, egui::RichText::new("Auto"), false),
            "Auto-set: timebase, V/div, offsets and trigger for the shown channels. Key: A",
        )
        .clicked()
        {
            self.autoset_start();
        }
        let (m, _) = egui::containers::menu::MenuButton::from_button(
            egui::Button::new("Menu").min_size(Vec2::new(w, 26.0)),
        )
        .ui(ui, |ui| self.main_menu(ui));
        help(
            m,
            "Menu: guided tests, recordings, export, references, screenshot, settings.",
        );
        ui.separator();
        // timebase: value and < >
        let tb = fmt_time(burst_ns_per_div(self.tb_id) as f64 * 1e-9).replace(' ', "");
        help(
            ui.add_sized(
                [w, 16.0],
                egui::Label::new(egui::RichText::new(tb).monospace().small()),
            ),
            "Time per division. Keys , and .",
        );
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let half = Vec2::new((w - 2.0) / 2.0, 24.0);
            if help(
                ui.add(egui::Button::new("<").min_size(half)),
                "Faster timebase. Key: ,",
            )
            .clicked()
            {
                self.step_timebase(-1);
            }
            if help(
                ui.add(egui::Button::new(">").min_size(half)),
                "Slower timebase. Key: .",
            )
            .clicked()
            {
                self.step_timebase(1);
            }
        });
        ui.separator();
        // channels on / off in their colours
        for ch in 0..CHANNELS {
            let on = self.on[ch];
            let t = egui::RichText::new(format!("CH{}", ch + 1))
                .color(if on {
                    COLORS[ch]
                } else {
                    Color32::from_gray(110)
                })
                .strong();
            if help(
                btn(ui, t, on),
                "Show / hide this channel (at least one stays on). Key: Shift + 1…8",
            )
            .clicked()
                && (!on || self.on.iter().filter(|&&a| a).count() > 1)
            {
                self.on[ch] = !on;
            }
        }
        ui.separator();
        if help(
            btn(ui, egui::RichText::new("Cur"), self.cursors.on),
            "Cursors on / off. Key: C",
        )
        .clicked()
        {
            self.cursors.on = !self.cursors.on;
            if self.cursors.on && self.cursors.t == [0.0, 0.0] {
                self.cursors_init();
            }
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        /// one size for the main action buttons (both rows)
        const BTN: Vec2 = Vec2::new(84.0, 28.0);
        ui.horizontal(|ui| {
            let running = self.run == Run::Running;
            let b = ui.add(
                egui::Button::new(if running { "Stop" } else { "Run" })
                    .min_size(BTN),
            );
            if help(
                b,
                "Start / stop continuous capturing. Stopped: the last capture stays on screen.",
            )
            .clicked()
            {
                self.set_run(if running { Run::Stopped } else { Run::Running });
            }
            let b = ui.add(egui::Button::new("Single").min_size(BTN));
            if help(
                b,
                "Capture once (waits for the trigger in Normal mode), then stop.",
            )
            .clicked()
            {
                self.set_run(Run::Single);
            }
            let cb = ui.add(egui::Button::new("Cursors").selected(self.cursors.on).min_size(BTN));
            if help(cb, "Time cursors T1/T2 (all plots) and voltage cursors V1/V2 on the selected channel, with Δt, 1/Δt, rpm and ΔV. Drag them, or Tab + Alt+arrows. Key: C").clicked() {
                self.cursors.on = !self.cursors.on;
                if self.cursors.on && self.cursors.t == [0.0, 0.0] {
                    self.cursors_init();
                }
            }
        });
        ui.horizontal(|ui| {
            let a = ui.add(egui::Button::new("Auto-set").min_size(BTN));
            if help(a, "Auto-set: picks timebase, V/div, offsets and the trigger for the shown channels (two quick passes). Key: A, undo: Backspace").clicked() {
                self.autoset_start();
            }
            let p = ui.add(egui::Button::new("Persistence").selected(self.persist_on).min_size(BTN));
            if help(p, "Persistence: the previous captures are drawn faded under the live trace (oldest faintest), so rare glitches stay visible. How many: Settings. Key: P").clicked() {
                self.persist_on = !self.persist_on;
            }
            let (m, _) = egui::containers::menu::MenuButton::from_button(egui::Button::new("Menu").min_size(BTN)).ui(ui, |ui| self.main_menu(ui));
            help(m, "Open a recording, export, screenshot (F12), reference waveforms (R / Shift+R), event log, Settings (F2), keyboard shortcuts (F1).");
        });
        ui.separator();

        ui.label(egui::RichText::new("Timebase").strong());
        ui.horizontal(|ui| {
            if help(ui.button("<"), "Faster timebase (less time per division). Key: ,").clicked() {
                self.step_timebase(-1);
            }
            let cb = egui::ComboBox::from_id_salt("timebase")
                .width(92.0)
                .selected_text(egui::RichText::new(fmt_time(burst_ns_per_div(self.tb_id) as f64 * 1e-9) + "/div").monospace())
                .height(400.0)
                .show_ui(ui, |ui| {
                    for id in (0..=TB_MAX).filter(|id| !TB_SKIP.contains(id)) {
                        ui.selectable_value(&mut self.tb_id, id, fmt_time(burst_ns_per_div(id) as f64 * 1e-9) + "/div");
                    }
                });
            help(
                cb.response,
                "Time per horizontal division (the screen is 10 divisions wide). Pick directly, or step with < > (keys , and .). 200 µs/div and 50 ms/div are left out on purpose: the scope delivers wrong data there.",
            );
            if help(ui.button(">"), "Slower timebase (more time per division). Key: .").clicked() {
                self.step_timebase(1);
            }
        });
        // long screens: one triggered capture per screen, or continuous streaming at a chosen rate
        if self.tb_id >= TB_STREAM_MIN {
            let span = self.span_s();
            let shown = self.on.iter().filter(|&&a| a).count();
            let cap_rate = 500.0 / span; // per channel: 4000 samples over 8 channels per screen
            let current = match self.roll {
                None => format!("1 capture/screen · {}/ch", fmt_rate(cap_rate)),
                Some(id) => format!("stream · {}/ch", fmt_rate(stream_rate(id, shown))),
            };
            let cb = egui::ComboBox::from_id_salt("rate")
                .width(230.0)
                .selected_text(current)
                .show_ui(ui, |ui| {
                    if self.tb_id <= TB_CAPTURE_MAX
                        && ui
                            .selectable_label(
                                self.roll.is_none(),
                                format!("1 capture/screen · {}/ch · triggered", fmt_rate(cap_rate)),
                            )
                            .clicked()
                    {
                        self.roll = None;
                    }
                    for (_, rid) in ROLL_RATES {
                        let r = stream_rate(rid, shown);
                        if r * span < 8.0 {
                            continue; // fewer than 8 samples per screen: useless
                        }
                        let label = format!(
                            "stream · {}/ch · {} per screen",
                            fmt_rate(r),
                            fmt_count(r * span)
                        );
                        if ui.selectable_label(self.roll == Some(rid), label).clicked() {
                            self.roll = Some(rid);
                        }
                    }
                });
            help(
                cb.response,
                "Sample rate per channel on long screens (refresh ↔ resolution). '1 capture/screen' is triggered but holds only 500 samples per channel and refreshes once per screen. 'stream' is continuous and gap-free, up to 440 S/s per channel with 8 channels shown and ×4.56 with one, but has no hardware trigger.",
            );
        }
        ui.separator();

        ui.label(egui::RichText::new("Capture trigger (scope)").strong());
        ui.horizontal(|ui| {
            let cb = egui::ComboBox::from_id_salt("trig_src")
                .width(60.0)
                .selected_text(format!("CH{}", self.trig_src + 1))
                .show_ui(ui, |ui| {
                    for ch in 0..CHANNELS as u8 {
                        ui.selectable_value(&mut self.trig_src, ch, format!("CH{}", ch + 1));
                    }
                });
            help(cb.response, "Which channel the trigger watches.");
            let r = ui.selectable_value(&mut self.trig_slope, Slope::Rising, "Rising");
            help(r, "Trigger when the signal crosses the level going up.");
            let f = ui.selectable_value(&mut self.trig_slope, Slope::Falling, "Falling");
            help(f, "Trigger when the signal crosses the level going down.");
        });
        let trig_speed = self.udiv(self.trig_src as usize) / 50.0; // one division per 50 px
        ui.horizontal(|ui| {
            let l = ui.add(egui::DragValue::new(&mut self.trig_v).speed(trig_speed).suffix(format!(" {}", self.probes[self.trig_src as usize].unit)).prefix("level "));
            help(l, "Trigger level in the trigger channel's unit (volts unless a probe is set). You can also drag the handle on the right edge of the graph.");
            let p = ui.add(egui::DragValue::new(&mut self.trig_pos).speed(2.0).range(0..=1000).custom_formatter(|v, _| format!("pos {:.0} %", v / 10.0)));
            help(p, "Where the trigger point sits on screen (0 % = left edge). Left of it = before the trigger. You can also drag the handle on the top edge of the graph.");
        });
        ui.horizontal(|ui| {
            let a = ui.selectable_value(&mut self.trig_mode, TrigMode::Auto, "Auto");
            help(a, "Wait for the trigger; if nothing triggers within a short time, capture anyway (the trace never freezes).");
            let n = ui.selectable_value(&mut self.trig_mode, TrigMode::Normal, "Normal");
            help(n, "Only show captures that triggered. The screen keeps the last one while waiting.");
            let fr = ui.selectable_value(&mut self.trig_mode, TrigMode::Free, "Free run");
            help(fr, "No trigger: capture continuously (the trace won't stand still on repeating signals).");
        });
        ui.separator();

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Channels").strong());
            ui.add_space(12.0);
            help(ui.label("Plots"), "Number of plot panes stacked on screen. Assign each channel to a plot in the P column; drag the divider between plots to resize them.");
            for n in 1..=MAX_PANES {
                if ui.selectable_label(self.panes == n, format!("{n}")).clicked() && self.panes != n {
                    self.set_panes(n);
                }
            }
        });
        egui::Grid::new("chans").num_columns(4).spacing([6.0, 4.0]).show(ui, |ui| {
            for ch in 0..CHANNELS {
                let mut on = self.on[ch];
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let c = ui.checkbox(&mut on, "");
                    if help(c, "Show / hide this channel (Shift + 1…8). At least one channel stays on.").changed()
                        && (on || self.on.iter().filter(|&&a| a).count() > 1)
                    {
                        self.on[ch] = on;
                    }
                    let name = egui::RichText::new(format!("CH{}", ch + 1)).color(COLORS[ch]).strong();
                    let sel = ui.selectable_label(ch == self.sel_ch, name);
                    if help(sel, "Select this channel (or press 1…8): ↑/↓ V/div, Shift+↑/↓ offset, voltage cursors and the cursor readout act on it.").clicked() {
                        self.sel_ch = ch;
                    }
                });
                if !self.on[ch] {
                    ui.end_row(); // off: just the checkbox and the name, no greyed controls
                    continue;
                }
                ui.add_enabled_ui(self.on[ch], |ui| {
                    let cb = egui::ComboBox::from_id_salt(("vdiv", ch))
                        .width(70.0)
                        .selected_text(fmt_div(self.udiv(ch), &self.probes[ch].unit))
                        .show_ui(ui, |ui| {
                            for (i, v) in VDIVS.iter().enumerate() {
                                ui.selectable_value(&mut self.vdiv[ch], i, fmt_div(*v * self.probes[ch].scale, &self.probes[ch].unit));
                            }
                        });
                    help(
                        cb.response,
                        "Volts per vertical division (8 divisions high). Instant zoom; the scope's hardware range follows automatically (a range switch takes ~0.2 s). Smaller V/div → finer range → more resolution.",
                    );
                });
                let d = ui.add_enabled(self.on[ch], egui::DragValue::new(&mut self.offset[ch]).speed(0.05).range(-4.0..=4.0).suffix(" div").max_decimals(2));
                help(d, "Vertical position of this channel's 0 V line, in divisions. Drag or type.");
                if self.panes > 1 {
                    let cur = self.pane_for(ch);
                    let cb = egui::ComboBox::from_id_salt(("pane", ch)).width(40.0).selected_text(format!("P{}", cur + 1)).show_ui(ui, |ui| {
                        for p in 0..self.panes {
                            ui.selectable_value(&mut self.pane_of[ch], p, format!("Plot {}", p + 1));
                        }
                    });
                    help(cb.response, "Which plot (pane) this channel is drawn in.");
                }
                ui.end_row();
            }
            // math channels with a time-domain result: own scale, position and plot, like a channel
            for i in 0..self.math.len() {
                let m = self.math[i];
                if matches!(m.op, MathOp::Off | MathOp::Fft) {
                    continue;
                }
                let unit = if m.op == MathOp::Rpm { "rpm".to_string() } else { self.unit(m.a).to_string() };
                help(
                    ui.add(egui::Label::new(egui::RichText::new(format!("  M{}", i + 1)).color(MATH_COLORS[i]).strong()).sense(Sense::click())),
                    &format!("Math channel M{}: {}. Set it up in the Math section; drag its M{} handle on the left edge to move it.", i + 1, m.label(), i + 1),
                );
                let mm = &mut self.math[i];
                let cb = egui::ComboBox::from_id_salt(("mdiv", i)).width(70.0).selected_text(fmt_div(math_div(mm.vdiv), &unit)).show_ui(ui, |ui| {
                    for k in 0..MATH_DIV_N {
                        ui.selectable_value(&mut mm.vdiv, k, fmt_div(math_div(k), &unit));
                    }
                });
                help(cb.response, "Units per division of this math channel (8 divisions high).");
                let d = ui.add(egui::DragValue::new(&mut mm.offset).speed(0.05).range(-4.0..=4.0).suffix(" div").max_decimals(2));
                help(d, "Vertical position of this math channel's zero line, in divisions. Or drag its handle.");
                if self.panes > 1 {
                    let cur = mm.pane.min(self.panes - 1);
                    let cb = egui::ComboBox::from_id_salt(("mpane", i)).width(40.0).selected_text(format!("P{}", cur + 1)).show_ui(ui, |ui| {
                        for p in 0..self.panes {
                            ui.selectable_value(&mut mm.pane, p, format!("Plot {}", p + 1));
                        }
                    });
                    help(cb.response, "Which plot (pane) this math channel is drawn in.");
                }
                ui.end_row();
            }
            // references: show / hide, which channel's scale and plot, own offset, clear
            for k in 0..2 {
                let Some(before) = self.refs[k].as_ref().map(|r| (r.on, r.under, r.offset)) else { continue };
                let (shown, refs_on, stream) = (self.on, self.refs_on, self.roll.is_some());
                let mut clear = false;
                let rf = self.refs[k].as_mut().expect("checked above");
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let c = ui.add_enabled(refs_on, egui::Checkbox::without_text(&mut rf.on));
                    help(c, "Show / hide this reference (Ctrl+R: all references).").on_disabled_hover_text("All references are hidden (Ctrl+R shows them).");
                    let saved = crate::rec::utc(std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(rf.saved_unix.max(0.0)));
                    help(
                        ui.add(egui::Label::new(egui::RichText::new(format!("R{}", k + 1)).color(REF_COLORS[k]).strong()).sense(Sense::click())),
                        &format!("Reference R{}: {}\nfrom CH{}, unit {}, saved {saved}\nDrawn on captures (not in stream mode){}.\nMenu › References: save the selected channel (R / Shift+R) or load one from the library.", k + 1, rf.name, rf.src + 1, rf.unit, if stream { " — now streaming, so not shown" } else { "" }),
                    );
                });
                // same columns as a channel row: [name] [scale source] [offset …] — nothing wider than a V/div box
                let cb = egui::ComboBox::from_id_salt(("ref_under", k)).width(70.0).selected_text(if shown[rf.under] { format!("on CH{}", rf.under + 1) } else { format!("on CH{} (off)", rf.under + 1) }).show_ui(ui, |ui| {
                    for c in 0..CHANNELS {
                        ui.selectable_value(&mut rf.under, c, format!("CH{}", c + 1));
                    }
                });
                help(cb.response, "Drawn with this channel's V/div, position and plot — e.g. compare injector 1 on CH1 with a reference taken from CH2.");
                let mut off = rf.offset;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let d = ui.add(egui::DragValue::new(&mut off).speed(0.05).range(-8.0..=8.0).suffix(" div").max_decimals(2));
                    help(d, "Extra vertical shift of the reference against its channel, in divisions (e.g. a different battery voltage).");
                    let l = ui.menu_button("load", |ui| self.ref_load_menu(ui, k)).response;
                    help(l, "Replace this reference: pick one from the library (newest first) or browse for a file anywhere.");
                    if help(ui.small_button("clear"), "Remove this reference (its library file stays).").clicked() {
                        clear = true;
                    }
                });
                if let Some(r) = self.refs[k].as_mut() {
                    r.offset = off;
                }
                ui.end_row();
                if clear {
                    self.refs[k] = None;
                    self.save_ref(k);
                } else if self.refs[k].as_ref().is_some_and(|r| (r.on, r.under, r.offset) != before) {
                    self.ref_dirty[k] = Some(Instant::now());
                }
            }
            if self.refs.iter().any(Option::is_none) {
                help(ui.label(egui::RichText::new("  Ref").weak()), "Reference waveforms R1 / R2: a known-good signal drawn under a channel. Save the selected channel with R / Shift+R, or load one here.");
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for k in 0..2 {
                        if self.refs[k].is_none() {
                            let l = ui.menu_button(format!("Load R{}", k + 1), |ui| self.ref_load_menu(ui, k)).response;
                            help(l, "Load a reference: from the library (newest first) or browse for a file anywhere.");
                        }
                    }
                });
                ui.end_row();
            }
        });
    }

    /// All plot panes stacked vertically, with drag-resizable dividers between them.
    fn plot(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let t = Instant::now();
        let n = self.panes.clamp(1, MAX_PANES);
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        const GAP: f32 = 6.0;
        let avail = ui.available_size();
        let usable = (avail.y - GAP * (n - 1) as f32).max(40.0);
        let total: f32 = self.pane_h[..n].iter().sum::<f32>().max(1e-3);
        for p in 0..n {
            let h = (usable * self.pane_h[p] / total).max(20.0);
            self.plot_pane(ui, snap, p, Vec2::new(avail.x, h));
            if p + 1 < n {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(avail.x, GAP), Sense::drag());
                let hot = resp.hovered() || resp.dragged();
                ui.painter().rect_filled(
                    rect,
                    0.0,
                    if hot {
                        Color32::from_gray(90)
                    } else {
                        Color32::from_gray(38)
                    },
                );
                if hot {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                }
                if resp.dragged() {
                    // move weight between the two neighbouring panes
                    let d = resp.drag_delta().y / usable * total;
                    let (a, b) = (self.pane_h[p], self.pane_h[p + 1]);
                    let min = total * 0.06;
                    let na = (a + d).clamp(min, a + b - min);
                    self.pane_h[p] = na;
                    self.pane_h[p + 1] = a + b - na;
                }
            }
        }
        self.paint_ms = 0.9 * self.paint_ms + 0.1 * t.elapsed().as_secs_f32() * 1e3;
    }

    /// One plot pane: its assigned channels; the trigger source's pane also carries the trigger markers,
    /// match-filter events and status.
    fn plot_pane(&mut self, ui: &mut egui::Ui, snap: &Snapshot, pane: usize, size: Vec2) {
        let is_trig_pane = self.pane_for(self.trig_src as usize) == pane;
        // persistence: the N captures before the one on screen (captures only, not streams)
        let persist: Vec<std::sync::Arc<HEntry>> =
            if self.persist_on && !snap.roll && !self.dropped(DROP_PERSIST) {
                self.acq.history.lock().ok().map_or(Vec::new(), |h| {
                    let Some(last) = h.last_no() else {
                        return Vec::new();
                    };
                    let base = self.shown_no.unwrap_or(last);
                    (base.saturating_sub(self.persist_n as u64)..base)
                        .rev()
                        .filter_map(|no| h.get(no))
                        .collect()
                })
            } else {
                Vec::new()
            };
        prof::mark("plot:rest");
        let (resp, painter) = ui.allocate_painter(size, Sense::click_and_drag());
        prof::mark("plot:persist_collect");
        if resp.drag_stopped() {
            self.drag = None;
        }
        let r = resp.rect;
        let painter = painter.with_clip_rect(r);
        self.zoom_input(ui, &resp, r, pane == 0);
        // virtual rect: the full 10 × 8 div screen, scaled/shifted by the zoom; drawing maps through
        // `vr`, cropping and handles use the real rect `r`
        let z = self.zoom;
        let vr = Rect::from_min_size(
            Pos2::new(
                r.left() - z.x0 * r.width() / (z.x1 - z.x0),
                r.top() - z.y0 * r.height() / (z.y1 - z.y0),
            ),
            Vec2::new(r.width() / (z.x1 - z.x0), r.height() / (z.y1 - z.y0)),
        );
        painter.rect_filled(r, 0.0, Color32::from_rgb(12, 14, 18));
        let (dx, dy) = (r.width() / DIV_X, r.height() / DIV_Y);
        let dyv = vr.height() / DIV_Y; // px per division of the (zoomed) signal
        self.plot_menu(&resp, vr, dyv, pane, snap);
        let grid = Stroke::new(1.0, Color32::from_gray(45));
        for i in 1..DIV_X as i32 {
            let x = r.left() + i as f32 * dx;
            painter.line_segment([Pos2::new(x, r.top()), Pos2::new(x, r.bottom())], grid);
        }
        for i in 1..DIV_Y as i32 {
            let y = r.top() + i as f32 * dy;
            painter.line_segment([Pos2::new(r.left(), y), Pos2::new(r.right(), y)], grid);
        }
        let axis = Stroke::new(1.0, Color32::from_gray(80));
        painter.line_segment(
            [
                Pos2::new(r.center().x, r.top()),
                Pos2::new(r.center().x, r.bottom()),
            ],
            axis,
        );
        painter.line_segment(
            [
                Pos2::new(r.left(), r.center().y),
                Pos2::new(r.right(), r.center().y),
            ],
            axis,
        );

        if !snap.roll {
            if is_trig_pane {
                self.trigger_markers(ui, &resp, &painter, r, vr);
            } else {
                // other panes: faint line at the trigger position (shared time axis)
                let tx = vr.left() + self.trig_pos as f32 / 1000.0 * vr.width();
                if (r.left()..=r.right()).contains(&tx) {
                    let c = COLORS[self.trig_src as usize].gamma_multiply(0.3);
                    painter.extend(Shape::dashed_line(
                        &[Pos2::new(tx, r.top()), Pos2::new(tx, r.bottom())],
                        Stroke::new(1.0, c),
                        6.0,
                        6.0,
                    ));
                }
            }
        }

        let mut trig_ab: Option<(f32, f32)> = None; // sample → x mapping of the trigger channel
        let mut chan_ab: [Option<(f32, f32)>; CHANNELS] = [None; CHANNELS]; // per drawn channel (cursor values)
        let mut taken: Vec<f32> = Vec::new(); // y of channel handles drawn so far
        let press = ui
            .input(|i| i.pointer.press_origin())
            .or(resp.interact_pointer_pos()); // where a drag began
                                              // persistence: earlier captures (one cached image per pane), under the live traces
        prof::mark("plot:grid+markers");
        self.persist_image(&painter, r, vr, dyv, pane, &persist);
        prof::mark("plot:persist_image");
        if !snap.roll {
            self.draw_refs(&painter, r, vr, dyv, pane);
        }
        prof::mark("plot:refs");
        if snap.cfg.is_some() {
            for ch in 0..CHANNELS {
                if !snap.frame.active[ch] || !self.on[ch] || self.pane_for(ch) != pane {
                    continue;
                }
                let d = &snap.frame.data[ch];
                if d.len() < 2 {
                    continue;
                }
                let y0 = vr.center().y - self.offset[ch] * dyv;
                let mut pts = std::mem::take(&mut self.pts);
                let Some((a, b)) = self.trace(snap, ch, r, vr, dyv, &mut pts) else {
                    continue;
                };
                if ch == self.trig_src as usize {
                    trig_ab = Some((a, b));
                }
                chan_ab[ch] = Some((a, b));
                if pts.len() > 1 {
                    painter.add(Shape::line(pts, Stroke::new(1.2, COLORS[ch])));
                }
                // channel handle on the left edge at its 0 V line: drag vertically to move the waveform
                let hy = y0.clamp(r.top() + 7.0, r.bottom() - 7.0);
                // handles closer than 14 px stack to the right, so each stays visible and grabbable
                let hx = r.left()
                    + 20.0 * taken.iter().filter(|&&t| (t - hy).abs() < 14.0).count() as f32;
                taken.push(hy);
                let on_handle = |p: Pos2| p.x >= hx && p.x < hx + 20.0 && (p.y - hy).abs() < 9.0;
                if resp.drag_started() && self.drag.is_none() && press.is_some_and(on_handle) {
                    self.drag = Some(Drag::Channel(ch));
                    self.sel_ch = ch; // grabbing a channel selects it
                                      // egui starts a drag only after a few px of movement: apply that movement too
                    if let (Some(o), Some(p)) = (press, resp.interact_pointer_pos()) {
                        self.offset[ch] =
                            (self.offset[ch] - (p.y - o.y) / dyv).clamp(-DIV_Y / 2.0, DIV_Y / 2.0);
                    }
                }
                let active = self.drag == Some(Drag::Channel(ch));
                if active && resp.dragged() && !resp.drag_started() {
                    self.offset[ch] = (self.offset[ch] - resp.drag_delta().y / dyv)
                        .clamp(-DIV_Y / 2.0, DIV_Y / 2.0);
                }
                if active || resp.hover_pos().is_some_and(on_handle) {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                }
                painter.add(Shape::convex_polygon(
                    vec![
                        Pos2::new(hx, hy - 7.0),
                        Pos2::new(hx + 12.0, hy - 7.0),
                        Pos2::new(hx + 18.0, hy),
                        Pos2::new(hx + 12.0, hy + 7.0),
                        Pos2::new(hx, hy + 7.0),
                    ],
                    if active {
                        COLORS[ch]
                    } else {
                        COLORS[ch].gamma_multiply(0.75)
                    },
                    Stroke::NONE,
                ));
                painter.text(
                    Pos2::new(hx + 6.0, hy),
                    Align2::CENTER_CENTER,
                    format!("{}", ch + 1),
                    FontId::monospace(11.0),
                    Color32::BLACK,
                );
            }
        }

        // software trigger: zone box and the matched event in this capture
        let orange = Color32::from_rgb(255, 160, 60);
        let s = self.trig_src as usize;
        let (pos_f, span) = (self.trig_pos as f32 / 1000.0, self.span_s() as f32);
        let y0s = vr.center().y - self.offset[s] * dyv;
        let x_of_t = |t: f64| vr.left() + (pos_f + t as f32 / span) * vr.width();
        let y_of_v = |v: f32| y0s - v / self.udiv(s) * dyv;
        if let (
            true,
            SwTrig::Zone {
                t0,
                t1,
                v0,
                v1,
                enter,
            },
        ) = (is_trig_pane, self.sw)
        {
            if t0 != t1 {
                let zr = Rect::from_two_pos(
                    Pos2::new(x_of_t(t0), y_of_v(v0)),
                    Pos2::new(x_of_t(t1), y_of_v(v1)),
                );
                let fill = if enter {
                    Color32::from_rgba_unmultiplied(255, 160, 60, 30)
                } else {
                    Color32::from_rgba_unmultiplied(255, 80, 80, 30)
                };
                painter.rect_filled(zr, 0.0, fill);
                painter.rect_stroke(zr, 0.0, Stroke::new(1.0, orange), egui::StrokeKind::Inside);
            }
        }
        // the matched event: a line at the trigger level from where the event starts to where the
        // condition became true (e.g. pulse width > 8 ms: from the pulse's edge to edge + 8 ms), a tick at
        // the match moment and the event's duration
        prof::mark("plot:traces");
        let events = self.sw_all(snap);
        if let Some((a, b)) = trig_ab {
            for hit in events.iter().copied() {
                let (xs, xa) = (a + b * hit.start as f32, a + b * hit.at as f32);
                let yl = y_of_v(self.sw_threshold(snap).unwrap_or(self.trig_v))
                    .clamp(r.top() + 4.0, r.bottom() - 4.0);
                let (l, rr) = (xs.max(r.left()), xa.min(r.right()));
                if rr >= l {
                    painter.line_segment(
                        [Pos2::new(l, yl), Pos2::new(rr.max(l + 1.0), yl)],
                        Stroke::new(3.0, orange),
                    );
                }
                if (r.left()..=r.right()).contains(&xs) {
                    painter.line_segment(
                        [Pos2::new(xs, yl - 6.0), Pos2::new(xs, yl + 6.0)],
                        Stroke::new(2.0, orange),
                    );
                }
                if (r.left()..=r.right()).contains(&xa) {
                    painter.line_segment(
                        [Pos2::new(xa, yl - 12.0), Pos2::new(xa, yl + 12.0)],
                        Stroke::new(2.0, orange),
                    );
                    painter.add(Shape::convex_polygon(
                        vec![
                            Pos2::new(xa - 7.0, r.bottom() - 16.0),
                            Pos2::new(xa + 7.0, r.bottom() - 16.0),
                            Pos2::new(xa, r.bottom() - 4.0),
                        ],
                        orange,
                        Stroke::NONE,
                    ));
                }
                if hit.at > hit.start {
                    let dur =
                        (hit.at - hit.start) as f64 * b as f64 / vr.width() as f64 * self.span_s();
                    let label = match self.sw {
                        SwTrig::Width {
                            cmp: Cmp::Longer, ..
                        }
                        | SwTrig::Dropout { .. } => format!("> {}", fmt_time_short(dur)),
                        _ => fmt_time_short(dur),
                    };
                    painter.text(
                        Pos2::new((l + rr) / 2.0, yl - 8.0),
                        Align2::CENTER_BOTTOM,
                        label,
                        FontId::monospace(12.0),
                        orange,
                    );
                }
            }
        }

        // match-filter status in the graph corner
        if is_trig_pane && self.sw != SwTrig::Off && !snap.roll {
            let pos = self
                .shown_no
                .and_then(|no| self.hits.iter().position(|&h| h == no));
            let n_events = events.len();
            let text = match (pos, self.shown_no) {
                (Some(i), _) => format!(
                    "match {} of {} · {} event{} in this capture",
                    i + 1,
                    self.hits.len(),
                    n_events,
                    if n_events == 1 { "" } else { "s" }
                ),
                (None, Some(_)) => "this capture doesn't match".to_string(),
                (None, None) => "no match yet — showing live captures".to_string(),
            };
            painter.text(
                Pos2::new(r.right() - 26.0, r.top() + 12.0),
                Align2::RIGHT_TOP,
                text,
                FontId::proportional(14.0),
                orange,
            );
        }

        // cursors (grab after markers and handles, before box zoom) and their readout in the selected channel's pane
        prof::mark("plot:sw_events");
        self.math_pane(ui, &resp, &painter, r, vr, pane, snap, press, &mut taken);
        prof::mark("plot:math_pane");
        self.scale_legend(&painter, r, pane);
        self.engine_markers(&painter, r, vr, pane, snap);
        prof::mark("plot:legend+engine");
        self.decode_overlay(&painter, r, vr, pane, snap);
        prof::mark("plot:decode_overlay");
        self.analysis_overlay(&painter, r, vr, pane, snap);
        prof::mark("plot:analysis_overlay");
        self.cursors_pane(ui, &resp, &painter, r, vr, pane, snap);
        if self.pane_for(self.sel_ch) == pane {
            self.cursor_readout(&painter, r, snap, chan_ab[self.sel_ch], vr);
        }
        prof::mark("plot:cursors");

        // box zoom: drag on empty graph area (nothing else grabbed) → rubber band → zoom on release
        if resp.drag_started() && self.drag.is_none() {
            self.box_from = ui.input(|i| i.pointer.press_origin());
        }
        if let (Some(a), Some(b)) = (self.box_from, resp.interact_pointer_pos()) {
            let sel = Rect::from_two_pos(a, b).intersect(r);
            if resp.dragged() {
                painter.rect_filled(sel, 0.0, Color32::from_rgba_unmultiplied(120, 170, 255, 28));
                painter.rect_stroke(
                    sel,
                    0.0,
                    Stroke::new(1.0, Color32::from_rgb(120, 170, 255)),
                    egui::StrokeKind::Inside,
                );
            }
            if resp.drag_stopped() && self.zone_draw {
                // the box defines the software-trigger zone (time from the trigger point, volts of the source)
                self.box_from = None;
                self.zone_draw = false;
                let t_of_x = |x: f32| (((x - vr.left()) / vr.width() - pos_f) * span) as f64;
                let v_of_y = |y: f32| (y0s - y) / dyv * self.udiv(s);
                if let SwTrig::Zone { enter, .. } = self.sw {
                    self.sw = SwTrig::Zone {
                        t0: t_of_x(sel.left()),
                        t1: t_of_x(sel.right()),
                        v0: v_of_y(sel.bottom()),
                        v1: v_of_y(sel.top()),
                        enter,
                    };
                }
            } else if resp.drag_stopped() {
                self.box_from = None;
                if sel.width() >= 8.0 {
                    let z = self.zoom;
                    let (w, h) = (z.x1 - z.x0, z.y1 - z.y0);
                    let mut nz = z;
                    nz.x0 = z.x0 + (sel.left() - r.left()) / r.width() * w;
                    nz.x1 = z.x0 + (sel.right() - r.left()) / r.width() * w;
                    if sel.height() >= 20.0 {
                        nz.y0 = z.y0 + (sel.top() - r.top()) / r.height() * h;
                        nz.y1 = z.y0 + (sel.bottom() - r.top()) / r.height() * h;
                    }
                    self.set_zoom(nz);
                }
            }
        } else if resp.drag_stopped() {
            self.box_from = None;
        }

        // overview strip while zoomed: where the visible window sits in the full screen
        let z = self.zoom;
        if pane == 0 && z.x1 - z.x0 < 0.999 {
            let bar = Rect::from_min_size(
                Pos2::new(r.left() + 30.0, r.top() + 2.0),
                Vec2::new(r.width() - 60.0, 5.0),
            );
            painter.rect_filled(bar, 2.0, Color32::from_gray(55));
            let win = Rect::from_x_y_ranges(
                bar.left() + z.x0 * bar.width()..=bar.left() + z.x1 * bar.width().max(1.0),
                bar.y_range(),
            );
            painter.rect_filled(win, 2.0, Color32::from_rgb(120, 170, 255));
        }
    }

    /// Wheel / touchpad / keyboard zoom & pan on the graph.
    /// Scroll = zoom time around the pointer, Shift+scroll = zoom vertically, pinch / Ctrl+wheel = zoom,
    /// horizontal two-finger scroll = pan time, double-click = full view, keys + − 0 Backspace, Shift+←/→ pan.
    fn zoom_input(&mut self, ui: &egui::Ui, resp: &egui::Response, r: Rect, keys: bool) {
        let z = self.zoom;
        let (w, h) = (z.x1 - z.x0, z.y1 - z.y0);
        let ptr = resp.hover_pos();
        let fx = ptr.map_or(0.5, |p| ((p.x - r.left()) / r.width()).clamp(0.0, 1.0));
        let fy = ptr.map_or(0.5, |p| ((p.y - r.top()) / r.height()).clamp(0.0, 1.0));
        let (scroll, pinch, shift) =
            ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), i.modifiers.shift));
        let mut nz = z;
        let mut changed = false;
        if resp.hovered() {
            // pinch / Ctrl+wheel arrive as zoom_delta, plain wheel / two-finger scroll as scroll delta
            let mut factor = 1.0 / pinch;
            let mut vertical = shift;
            if pinch == 1.0 {
                let s = if shift { scroll.x + scroll.y } else { scroll.y };
                factor = (-s * 0.0025).exp();
                if !shift && scroll.x != 0.0 {
                    // horizontal two-finger scroll pans time
                    let shift_frac = -scroll.x / r.width() * w;
                    nz.x0 += shift_frac;
                    nz.x1 += shift_frac;
                    changed = true;
                }
                vertical = shift;
            }
            if (factor - 1.0).abs() > 1e-4 {
                if vertical {
                    let c = z.y0 + fy * h;
                    let nh = (h * factor).clamp(0.01, 1.0);
                    nz.y0 = c - fy * nh;
                    nz.y1 = nz.y0 + nh;
                } else {
                    let c = z.x0 + fx * w;
                    let nw = (w * factor).clamp(0.001, 1.0);
                    nz.x0 = c - fx * nw;
                    nz.x1 = nz.x0 + nw;
                }
                changed = true;
            }
            if resp.double_clicked() {
                self.set_zoom(Zoom::default());
                return;
            }
        }
        if keys && !ui.ctx().egui_wants_keyboard_input() {
            ui.input(|i| {
                let (cx, cy) = (z.x0 + w / 2.0, z.y0 + h / 2.0);
                if i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals) {
                    let nw = (w / 2.0).max(0.001);
                    (nz.x0, nz.x1) = (cx - nw / 2.0, cx + nw / 2.0);
                    changed = true;
                }
                if i.key_pressed(egui::Key::Minus) {
                    // out on both axes (a drag zoom zooms both)
                    let (nw, nh) = ((w * 2.0).min(1.0), (h * 2.0).min(1.0));
                    (nz.x0, nz.x1) = (cx - nw / 2.0, cx + nw / 2.0);
                    (nz.y0, nz.y1) = (cy - nh / 2.0, cy + nh / 2.0);
                    changed = true;
                }
                if i.key_pressed(egui::Key::Num0) {
                    nz = Zoom::default();
                    changed = true;
                }
                if i.modifiers.shift && i.key_pressed(egui::Key::ArrowLeft) {
                    (nz.x0, nz.x1) = (z.x0 - w * 0.1, z.x1 - w * 0.1);
                    changed = true;
                }
                if i.modifiers.shift && i.key_pressed(egui::Key::ArrowRight) {
                    (nz.x0, nz.x1) = (z.x0 + w * 0.1, z.x1 + w * 0.1);
                    changed = true;
                }
            });
            if ui.input(|i| i.key_pressed(egui::Key::Backspace)) {
                if let Some(prev) = self.zoom_undo.pop() {
                    self.zoom = prev;
                }
                return;
            }
        }
        if changed {
            // continuous gestures (wheel/pinch/pan) don't flood the undo stack: only discrete ones push
            let discrete = !resp.hovered() || (scroll == Vec2::ZERO && pinch == 1.0);
            if discrete {
                self.set_zoom(nz);
            } else {
                self.zoom = nz.clamped();
            }
        }
    }

    /// Right-click (touchpad: two-finger tap) menu on a plot.
    fn plot_menu(
        &mut self,
        resp: &egui::Response,
        vr: Rect,
        dyv: f32,
        pane: usize,
        snap: &Snapshot,
    ) {
        if resp.secondary_clicked() {
            self.menu_at = resp.interact_pointer_pos();
        }
        resp.context_menu(|ui| {
            let at = self.menu_at;
            if ui.button("Zoom out ×2  (−)").clicked() {
                let z = self.zoom;
                let (cx, cy, nw, nh) = ((z.x0 + z.x1) / 2.0, (z.y0 + z.y1) / 2.0, ((z.x1 - z.x0) * 2.0).min(1.0), ((z.y1 - z.y0) * 2.0).min(1.0));
                self.set_zoom(Zoom { x0: cx - nw / 2.0, x1: cx + nw / 2.0, y0: cy - nh / 2.0, y1: cy + nh / 2.0 });
                ui.close();
            }
            if ui.add_enabled(!self.zoom_undo.is_empty(), egui::Button::new("Undo zoom  (Backspace)")).clicked() {
                if let Some(prev) = self.zoom_undo.pop() {
                    self.zoom = prev;
                }
                ui.close();
            }
            if ui.add_enabled(self.zoom != Zoom::default(), egui::Button::new("Full view  (0, double-click)")).clicked() {
                self.set_zoom(Zoom::default());
                ui.close();
            }
            ui.separator();
            let tch = self.trig_src as usize;
            let captures = self.roll.is_none();
            if ui.add_enabled(captures && at.is_some() && self.pane_for(tch) == pane, egui::Button::new(format!("Trigger level here (CH{})", tch + 1))).on_disabled_hover_text(if captures { "The trigger channel is drawn in another plot." } else { "Streaming has no hardware trigger." }).clicked() {
                if let Some(p) = at {
                    let y0 = vr.center().y - self.offset[tch] * dyv;
                    self.trig_v = (y0 - p.y) / dyv * self.udiv(tch);
                }
                ui.close();
            }
            if ui.add_enabled(captures && at.is_some(), egui::Button::new("Trigger position here")).on_disabled_hover_text("Streaming has no hardware trigger.").clicked() {
                if let Some(p) = at {
                    self.trig_pos = (((p.x - vr.left()) / vr.width()).clamp(0.0, 1.0) * 1000.0).round() as u16;
                }
                ui.close();
            }
            ui.separator();
            for i in 0..2 {
                let b = egui::Button::new(format!("Save CH{} as reference R{}", self.sel_ch + 1, i + 1));
                if ui.add_enabled(captures && snap.cfg.is_some() && self.on[self.sel_ch], b).on_hover_text("Keep the selected channel of the capture on screen as a known-good waveform (also a file in the reference library, Menu > References).").clicked() {
                    self.ref_take(i, snap, self.sel_ch);
                    ui.close();
                }
            }
            for i in 0..2 {
                ui.menu_button(format!("Load reference into R{}", i + 1), |ui| self.ref_load_menu(ui, i));
            }
            if self.refs.iter().any(Option::is_some) {
                if ui.button(if self.refs_on { "Hide references" } else { "Show references" }).clicked() {
                    self.refs_on = !self.refs_on;
                    ui.close();
                }
                if ui.button("Clear references").clicked() {
                    self.refs = [None, None];
                    self.save_ref(0);
                    self.save_ref(1);
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("Save screenshot (PNG)  F12").clicked() {
                self.files.shot_req = true;
                ui.close();
            }
            if ui.button("Auto-set  (A)").clicked() {
                self.autoset_start();
                ui.close();
            }
        });
    }

    fn set_zoom(&mut self, z: Zoom) {
        let z = z.clamped();
        if z != self.zoom {
            self.zoom_undo.push(self.zoom);
            if self.zoom_undo.len() > 50 {
                self.zoom_undo.remove(0);
            }
            self.zoom = z;
        }
    }

    /// Trigger level (horizontal line, handle on the right edge) and trigger position (vertical
    /// line, handle on the top edge). Both draggable; drawn from the UI settings so they move
    /// instantly. Positions map through the zoomed virtual rect `vr`; handles sit on the real edges.
    fn trigger_markers(
        &mut self,
        ui: &egui::Ui,
        resp: &egui::Response,
        painter: &egui::Painter,
        r: Rect,
        vr: Rect,
    ) {
        const GRAB: f32 = 10.0; // px either side of a line that still grabs it
        const HANDLE: f32 = 9.0;
        let dyv = vr.height() / DIV_Y;
        let s = self.trig_src as usize;
        let c = COLORS[s];
        let y0 = vr.center().y - self.offset[s] * dyv; // 0 V of the trigger channel
        let ty = (y0 - self.trig_v / self.udiv(s) * dyv).clamp(r.top(), r.bottom());
        let tx_raw = vr.left() + self.trig_pos as f32 / 1000.0 * vr.width();
        let tx_visible = (r.left()..=r.right()).contains(&tx_raw);
        let tx = tx_raw.clamp(r.left(), r.right());

        // drag handling
        let near = |p: Pos2| -> Option<Drag> {
            if (p.y - ty).abs() < GRAB
                && (p.x > r.right() - 4.0 * HANDLE || !(p.x - tx).abs().lt(&GRAB))
            {
                Some(Drag::Level)
            } else if tx_visible && (p.x - tx).abs() < GRAB {
                Some(Drag::Pos)
            } else {
                None
            }
        };
        if resp.drag_started() {
            // hit-test where the press began: by the time egui reports the drag the pointer has moved away
            self.drag = ui
                .input(|i| i.pointer.press_origin())
                .or(resp.interact_pointer_pos())
                .and_then(near);
        }
        if resp.dragged() {
            if let (Some(d), Some(p)) = (self.drag, resp.interact_pointer_pos()) {
                match d {
                    Drag::Level => {
                        let y = p.y.clamp(r.top(), r.bottom());
                        self.trig_v = (y0 - y) / dyv * self.udiv(s);
                    }
                    Drag::Pos => {
                        self.trig_pos = (((p.x - vr.left()) / vr.width()).clamp(0.0, 1.0) * 1000.0)
                            .round() as u16;
                    }
                    Drag::Channel(_) | Drag::Math(_) | Drag::TCur(_) | Drag::VCur(_) => {} // handled elsewhere
                }
            }
        }
        let hover = self.drag.or_else(|| resp.hover_pos().and_then(near));
        match hover {
            Some(Drag::Level) | Some(Drag::Channel(_)) | Some(Drag::Math(_)) => {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical)
            }
            Some(Drag::Pos) | Some(Drag::TCur(_)) => {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal)
            }
            Some(Drag::VCur(_)) => ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical),
            None => {}
        }

        // drawing (brighter while hovered/dragged)
        let line = |hot: bool| {
            Stroke::new(
                if hot { 1.5 } else { 1.0 },
                c.gamma_multiply(if hot { 0.9 } else { 0.55 }),
            )
        };
        let lvl_hot = hover == Some(Drag::Level);
        let pos_hot = hover == Some(Drag::Pos);
        painter.extend(Shape::dashed_line(
            &[Pos2::new(r.left(), ty), Pos2::new(r.right(), ty)],
            line(lvl_hot),
            6.0,
            6.0,
        ));
        if tx_visible {
            painter.extend(Shape::dashed_line(
                &[Pos2::new(tx, r.top()), Pos2::new(tx, r.bottom())],
                line(pos_hot),
                6.0,
                6.0,
            ));
        }
        // level handle: triangle on the right edge pointing left
        painter.add(Shape::convex_polygon(
            vec![
                Pos2::new(r.right(), ty - HANDLE),
                Pos2::new(r.right(), ty + HANDLE),
                Pos2::new(r.right() - 1.6 * HANDLE, ty),
            ],
            c,
            Stroke::NONE,
        ));
        // position handle: triangle on the top edge pointing down (dimmed at the edge when zoomed away from it)
        painter.add(Shape::convex_polygon(
            vec![
                Pos2::new(tx - HANDLE, r.top()),
                Pos2::new(tx + HANDLE, r.top()),
                Pos2::new(tx, r.top() + 1.6 * HANDLE),
            ],
            if tx_visible {
                c
            } else {
                c.gamma_multiply(0.35)
            },
            Stroke::NONE,
        ));
        painter.text(
            Pos2::new(r.right() - 2.0 * HANDLE, ty - 2.0),
            Align2::RIGHT_BOTTOM,
            format!("T {}", fmt_u(self.trig_v, self.unit(s))),
            FontId::monospace(12.0),
            c,
        );
    }
}

/// Visible part of the full screen, as fractions (x: left→right, y: top→bottom). Default = everything.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Zoom {
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
}

impl Default for Zoom {
    fn default() -> Self {
        Zoom {
            x0: 0.0,
            x1: 1.0,
            y0: 0.0,
            y1: 1.0,
        }
    }
}

impl Zoom {
    /// Keep the window inside the screen and at least a minimum size.
    fn clamped(self) -> Zoom {
        let fit = |a: f32, b: f32, min: f32| {
            let w = (b - a).clamp(min, 1.0);
            let a = a.clamp(0.0, 1.0 - w);
            (a, a + w)
        };
        let (x0, x1) = fit(self.x0, self.x1, 0.001);
        let (y0, y1) = fit(self.y0, self.y1, 0.01);
        Zoom { x0, x1, y0, y1 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    Level,
    Pos,
    /// vertical position (offset) of one channel's waveform
    Channel(usize),
    /// vertical position of math channel M1/M2
    Math(usize),
    /// time cursor T1/T2, voltage cursor V1/V2
    TCur(usize),
    VCur(usize),
}

/// Map samples to screen points: all points if few, else min/max per pixel column.
fn decimate(d: &[u16], r: Rect, y: impl Fn(u16) -> f32, out: &mut Vec<Pos2>) {
    out.clear();
    let n = d.len();
    let w = r.width().max(1.0) as usize;
    if n <= 2 * w {
        let sx = r.width() / (n - 1) as f32;
        out.extend(
            d.iter()
                .enumerate()
                .map(|(i, &v)| Pos2::new(r.left() + i as f32 * sx, y(v))),
        );
        return;
    }
    for px in 0..w {
        let a = px * n / w;
        let b = ((px + 1) * n / w).max(a + 1);
        let (mut lo, mut hi) = (u16::MAX, 0u16);
        for &v in &d[a..b] {
            lo = lo.min(v);
            hi = hi.max(v);
        }
        let x = r.left() + px as f32;
        out.push(Pos2::new(x, y(lo)));
        out.push(Pos2::new(x, y(hi)));
    }
}

fn fmt_time(s: f64) -> String {
    match s {
        s if s >= 3600.0 => format!(
            "{} h {} min",
            (s / 3600.0).floor(),
            ((s % 3600.0) / 60.0).floor()
        ),
        s if s >= 60.0 => format!("{} min {} s", (s / 60.0).floor(), (s % 60.0).round()),
        s if s >= 1.0 => format!("{} s", (s * 1000.0).round() / 1000.0),
        s if s >= 1e-3 => format!("{} ms", (s * 1e3 * 1000.0).round() / 1000.0),
        s if s >= 1e-6 => format!("{} µs", (s * 1e6 * 1000.0).round() / 1000.0),
        s => format!("{} ns", (s * 1e9).round()),
    }
}

fn fmt_v(v: f32) -> String {
    if v.abs() >= 1.0 {
        format!("{v:.3} V")
    } else {
        format!("{:.1} mV", v * 1000.0)
    }
}

fn fmt_freq(f: f64) -> String {
    match f {
        f if f >= 1e6 => format!("{:.4} MHz", f / 1e6),
        f if f >= 1e3 => format!("{:.4} kHz", f / 1e3),
        f => format!("{f:.3} Hz"),
    }
}

/// Slowest timebase offered: 50 s/div (500 s per screen).
const TB_MAX: u8 = 32;
/// Timebases skipped because the scope's data is wrong there (8 channels, measured 2026-11-20/21):
/// 200 µs/div (rate ~0.33× nominal, unstable, seam at sample ~450) and 50 ms/div (rate 125× off).
const TB_SKIP: [u8; 2] = [16, 23];
/// Most plot panes on screen.
const MAX_PANES: usize = 4;
/// From 100 ms/div (1 s per screen) the user can choose streaming instead of one capture per screen.
const TB_STREAM_MIN: u8 = 24;
/// Slowest timebase where "one capture per screen" is offered (200 ms/div); slower = streaming only
/// until the progressive scan mode is implemented.
const TB_CAPTURE_MAX: u8 = 25;

/// Real streaming rate per channel for a roll-rate id with `shown` channels streamed.
fn stream_rate(rate_id: u8, shown: usize) -> f64 {
    ROLL_RATES
        .iter()
        .find(|r| r.1 == rate_id)
        .map_or(0.0, |r| r.0)
        * roll_rate_factor(shown)
}

fn fmt_rate(r: f64) -> String {
    if r >= 1000.0 {
        format!("{:.1} kS/s", r / 1000.0)
    } else if r >= 10.0 {
        format!("{r:.0} S/s")
    } else {
        format!("{r:.2} S/s")
    }
}

fn fmt_count(n: f64) -> String {
    if n >= 10_000.0 {
        format!("{:.0}k", n / 1000.0)
    } else {
        format!("{n:.0}")
    }
}

impl App {
    /// Screen width in seconds (10 divisions).
    fn span_s(&self) -> f64 {
        burst_ns_per_div(self.tb_id) as f64 * 1e-9 * DIV_X as f64
    }

    /// Keep acquisition mode and stream buffer length in line with the timebase / rate choice.
    fn sync_mode(&mut self) {
        if self.meter.on && self.roll.is_none() && self.tb_id >= TB_STREAM_MIN {
            self.roll = Some(ROLL_RATES[0].1); // a live meter needs continuous data, not one capture per 10 s
            self.meter.forced_stream = true;
        } else if !self.meter.on && self.meter.forced_stream {
            self.meter.forced_stream = false;
            if self.tb_id <= TB_CAPTURE_MAX {
                self.roll = None;
            }
        }
        if self.tb_id < TB_STREAM_MIN {
            self.roll = None; // streaming only makes sense on long screens
        } else if self.tb_id > TB_CAPTURE_MAX && self.roll.is_none() {
            self.roll = Some(ROLL_RATES[0].1); // capture mode not available this slow yet
        }
        if self.mode_sent != Some(self.roll) {
            let _ = self
                .acq
                .tx
                .send(Cmd::Mode(self.roll.map_or(Mode::Burst, Mode::Roll)));
            self.mode_sent = Some(self.roll);
        }
        if let Some(id) = self.roll {
            let shown = self.on.iter().filter(|&&a| a).count();
            let keep = (stream_rate(id, shown) * self.span_s()).ceil() as usize + 2;
            if keep != self.keep_sent {
                let _ = self.acq.tx.send(Cmd::Keep(keep));
                self.keep_sent = keep;
            }
        }
    }
}

/// Linux: $XDG_CONFIG_HOME or ~/.config; Windows: %APPDATA%.
fn settings_path() -> Option<std::path::PathBuf> {
    config_dir().map(|d| d.join("settings.txt"))
}

fn fmt_volts(v: f32) -> String {
    if v >= 1.0 {
        format!("{v} V")
    } else {
        format!("{} mV", (v * 1000.0).round())
    }
}

impl eframe::App for App {
    /// Quit: the open recording gets its last rows written and is closed cleanly (≤ 1 s).
    fn on_exit(&mut self) {
        self.acq.rec.shutdown(std::time::Duration::from_secs(1));
    }

    /// Before egui sees the input: Tab is ours (next cursor) — egui would move focus into a field, which then
    /// swallows every shortcut. Shift + 1…8 arrive as "!", "\"" … on most layouts: match the physical key.
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        if ctx.egui_wants_keyboard_input() {
            return; // typing in a field: Tab / Shift+digits behave normally there
        }
        let digits = [
            egui::Key::Num1,
            egui::Key::Num2,
            egui::Key::Num3,
            egui::Key::Num4,
            egui::Key::Num5,
            egui::Key::Num6,
            egui::Key::Num7,
            egui::Key::Num8,
        ];
        let tab_is_ours = self.cursors.on && !self.settings_open && !self.show_keys;
        raw.events.retain(|e| match e {
            egui::Event::Key {
                key: egui::Key::Tab,
                pressed,
                ..
            } if tab_is_ours => {
                if *pressed {
                    self.tab_pressed = true;
                }
                false
            }
            egui::Event::Key {
                physical_key: Some(k),
                pressed: true,
                modifiers,
                ..
            } if modifiers.shift => {
                if let Some(ch) = digits.iter().position(|d| d == k) {
                    self.shift_digit = Some(ch);
                    return false;
                }
                true
            }
            _ => true,
        });
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        prof::start();
        let dt = now.duration_since(self.last).as_secs_f32();
        self.last = now;
        if dt > 0.0 {
            self.ui_fps = 0.9 * self.ui_fps + 0.1 / dt;
        }
        self.acq.out.update();
        let snap = self.acq.out.output_buffer().clone(); // ~8 KB copy; keeps borrows simple
        prof::mark("snap_clone");
        // sample rate of what streams on screen: live = the chosen rate for the shown channels; stopped = the
        // rate stored with the recording (an opened file, or a stream recorded with other channels on)
        let live = self.roll.map_or(1.0, |id| {
            stream_rate(id, self.on.iter().filter(|&&a| a).count())
        });
        self.shown_rate = match (self.roll, self.run) {
            (Some(_), Run::Stopped) => self
                .acq
                .history
                .lock()
                .ok()
                .map(|h| h.stream.rate)
                .filter(|r| *r > 0.0)
                .unwrap_or(live),
            _ => live,
        };
        // Single done: acq stopped itself after the capture → the UI follows (Run button, notices)
        if self.run == Run::Single && snap.stopped && snap.frame.seq > self.single_seq {
            self.run = Run::Stopped;
        }
        prof::mark("shown_rate");
        self.update_hits();
        prof::mark("update_hits");
        self.update_stream_hits();
        prof::mark("update_stream_hits");
        self.autoset_step(&snap);
        self.can_log_tick(&snap);
        prof::mark("autoset+can_log_tick");
        self.test_tick(&snap);
        prof::mark("test_tick");
        self.files_tick(&snap);
        self.event_tick();
        prof::mark("files+event_tick");
        // browsing the history: show the selected capture instead of the live one; while running with a
        // software trigger: show the latest matching capture
        let shown = self.view.or_else(|| {
            (self.sw != SwTrig::Off && self.run == Run::Running)
                .then(|| self.hits.last().copied())
                .flatten()
        });
        self.shown_no = shown;
        let snap = match shown.and_then(|no| self.acq.history.lock().ok()?.get(no)) {
            Some(e) => e.snap.clone(),
            None => snap,
        };
        // stopped in stream mode: show the chosen window of the stream recording
        let snap = if self.roll.is_some() && self.run == Run::Stopped {
            self.stream_window(&snap).unwrap_or(snap)
        } else {
            snap
        };
        prof::mark("history_get+stream_window");
        self.analysis_tick(&snap);
        prof::mark("analysis_tick");
        if snap.frame.seq != self.last_seq {
            let gap = now.duration_since(self.last_frame_at).as_secs_f32();
            if gap > 0.3 && self.run == Run::Running && self.view.is_none() {
                log::debug!("ui: no new frame for {:.0} ms", gap * 1e3);
            }
            self.last_seq = snap.frame.seq;
            self.last_frame_at = now;
        }
        if self.trig_mode == TrigMode::Normal {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(500)); // "waiting for trigger" notice
        }
        self.global_keys(ui.ctx());
        self.cursor_keys(ui.ctx());
        prof::mark("keys");
        let raw_notices = self.notices(&snap);
        let notices = self.notice_debounce.update(raw_notices, Instant::now());
        prof::mark("notices");

        // side panel first: it gets the full window height, the bottom panels sit under the plot only.
        // Its width follows the content: a fixed width let wide rows (plot column, reference row, long names) spill
        // under the plot. Content wider than the panel → the panel grows next frame (≤ 45 % of the window); a layout
        // change (plots, references, math, analysis, decoder) starts again from the base width so it also shrinks.
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (
                self.panes,
                self.refs.iter().filter(|r| r.is_some()).count(),
                self.kinds.map(waves::kind_index),
                self.decoder.encode(),
            )
                .hash(&mut h);
            self.math
                .iter()
                .map(|m| m.op != MathOp::Off)
                .collect::<Vec<_>>()
                .hash(&mut h);
            h.finish()
        };
        if key != self.panel_key {
            (self.panel_key, self.panel_w) = (key, PANEL_W);
        }
        let cap = (ui.ctx().content_rect().width() * 0.45).max(PANEL_W);
        let panel_w = self.panel_w.clamp(PANEL_W, cap);
        let mut over = 0.0f32;
        if self.panel_min {
            egui::Panel::right("controls_min")
                .resizable(false)
                .exact_size(PANEL_MIN_W)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| self.compact_controls(ui));
                });
        } else {
            egui::Panel::right("controls")
            .resizable(false)
            .exact_size(panel_w)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let avail = ui.available_width();
                ui.horizontal(|ui| {
                    if help(ui.small_button(">>"), "Collapse the side panel to a narrow column of short buttons: more room for the waveform on small screens. Key: F4").clicked() {
                        self.panel_min = true;
                    }
                    ui.label(egui::RichText::new("collapse panel").weak().small());
                });
                self.controls(ui);
                ui.separator();
                egui::CollapsingHeader::new(egui::RichText::new("Automotive tests").strong())
                    .default_open(true)
                    .show(ui, |ui| self.analysis_panel(ui))
                    .header_response
                    .on_hover_text("Guided sensor / engine tests and the waveform analysis per channel (injector, ignition, crank, …): phases highlighted on the trace with readouts and a verdict; test runs with a report.");
                ui.separator();
                egui::CollapsingHeader::new(egui::RichText::new("Protocol decoding").strong())
                    .default_open(true)
                    .show(ui, |ui| self.protocol_panel(ui))
                    .header_response
                    .on_hover_text("Bus decoding on one channel: LIN, K-line, CAN (with log and .asc export); frames drawn above the trace.");
                ui.separator();
                egui::CollapsingHeader::new(egui::RichText::new("Match filter (software)").strong())
                    .default_open(self.sw != SwTrig::Off)
                    .show(ui, |ui| self.sw_controls(ui))
                    .header_response
                    .on_hover_text("Checks every capture on the trigger source for a pulse width, runt, dropout, window or zone; the screen holds the latest match, [ ] jump between matches.");
                ui.separator();
                egui::CollapsingHeader::new(egui::RichText::new("Math").strong())
                    .default_open(self.math.iter().any(|m| m.op != MathOp::Off))
                    .show(ui, |ui| self.math_controls(ui))
                    .header_response
                    .on_hover_text("Math channels: A−B, A+B, A×k, low-pass, RPM, FFT.");
                over = ui.min_rect().width() - avail;
                });
            });
        }
        self.panel_over = over;
        prof::mark("side_panel");
        if over > 0.5 && panel_w < cap {
            self.panel_w = (panel_w + over + 2.0).min(cap);
            ui.ctx().request_repaint(); // redraw at once with the wider panel (one frame)
        }
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                if self.show_perf {
                    ui.monospace(format!(
                        "UI {:5.0} fps | paint {:4.2} ms | acq {:5.1} frames/s | USB {:5.1} ms/frame | {}",
                        self.ui_fps, self.paint_ms, snap.acq_fps, snap.usb_ms, snap.status
                    ));
                } else {
                    ui.monospace(format!("{} | {:.1} {}/s", snap.status, snap.acq_fps, if snap.roll { "stream updates" } else { "captures" }));
                }
            });
        });
        // info line: above the status bar, outside the graph
        prof::mark("status_bar");
        egui::Panel::bottom("info").show(ui, |ui| self.info_line(ui, &notices));
        prof::mark("info_line");
        egui::Panel::bottom("history").show(ui, |ui| {
            if self.roll.is_some() {
                self.stream_panel(ui)
            } else {
                self.history_panel(ui)
            }
        });
        prof::mark("history/stream_panel");
        self.update_acc();
        prof::mark("update_acc");
        egui::Panel::bottom("meas").show(ui, |ui| {
            if self.meas_on {
                egui::ScrollArea::horizontal()
                    .id_salt("meas_scroll")
                    .show(ui, |ui| self.measurement_panel(ui, &snap));
            } else {
                // hidden: one line to bring it back, nothing measured (statistics keep adding up)
                let b = ui.add(egui::Button::new("▸ Measurements").frame(false));
                if help(
                    b,
                    "Show the measurement table (one row per shown channel). Key: F3",
                )
                .clicked()
                {
                    self.meas_on = true;
                }
            }
        });
        // reference requested from the menu / keys (R, Shift+R): the capture being shown
        prof::mark("measurement_panel");
        if let Some(i) = self.ref_req.take() {
            self.ref_take(i, &snap, self.sel_ch);
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                if self.meter.on {
                    self.meter_view(ui, &snap)
                } else {
                    self.plot(ui, &snap)
                }
            });
        prof::mark("plot:rest");
        if self.perf_log.elapsed().as_secs() >= 2 {
            self.perf_log = Instant::now();
            log::debug!(
                "perf ui_fps={:.0} paint_ms={:.3} acq_fps={:.1} usb_ms={:.1} status={}",
                self.ui_fps,
                self.paint_ms,
                snap.acq_fps,
                snap.usb_ms,
                snap.status
            );
            for (l, t) in &notices {
                log::debug!("notice {l:?}: {t}");
            }
        }
        self.sync_mode();
        self.push_config(&snap);
        for k in 0..2 {
            if self.ref_dirty[k].is_some_and(|t| t.elapsed().as_secs_f32() > 0.5) {
                self.ref_dirty[k] = None;
                self.save_ref(k);
            } else if self.ref_dirty[k].is_some() {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(600));
                // save even when idle
            }
        }
        prof::mark("sync+push_config+refs");
        self.save_settings_if_changed();
        prof::mark("save_settings_if_changed");
        self.debug_screenshot(ui.ctx());
        self.screenshot_tick(ui.ctx());
        self.browse_tick();
        self.keys_overlay(ui.ctx());
        self.settings_window(ui.ctx());
        prof::mark("shot+browse+keys+settings_win");
        self.fft_windows(ui.ctx(), &snap);
        prof::mark("fft_windows");
        self.guide_window(ui.ctx());
        self.test_window(ui.ctx());
        prof::mark("guide+test_window");
        // debug aid like HANTEK_SHOT: HANTEK_SLOW_MS=60 makes every frame that much slower (tests the dropout)
        static SLOW: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let slow_ms = *SLOW.get_or_init(|| {
            std::env::var("HANTEK_SLOW_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
        });
        if slow_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(slow_ms));
        }
        // adaptive feature dropout: this frame's CPU work (not the frame rate — slow timebases repaint rarely)
        let work = now.elapsed().as_secs_f32() * 1e3;
        self.work_ms = if self.work_ms == 0.0 {
            work
        } else {
            0.9 * self.work_ms + 0.1 * work
        };
        if self.adaptive {
            let running = self.run == Run::Running;
            let (slow, fast) = (
                running && self.work_ms > 1000.0 / DROP_FPS,
                self.work_ms < 1000.0 / DROP_FPS * 0.4,
            );
            if let Some(l) = self.dropout.update(Instant::now(), slow, fast) {
                log::info!(
                    "adaptive quality: level {l} (frame work {:.1} ms)",
                    self.work_ms
                );
            }
        } else {
            self.dropout = Dropout::default();
        }
    }
}

/// Hover tooltip, plus the same text after a press-and-hold of 0.5 s (touchpad / touch: no hover needed).
/// A label in a slot that never gets narrower (widest width so far, per `id`, kept in egui's memory): a readout
/// whose digits change (recording time, sample count, capture number) doesn't push the controls after it left and
/// right. Left-aligned in its slot.
fn stable_label(ui: &mut egui::Ui, id: &str, text: impl Into<egui::WidgetText>) -> egui::Response {
    let text: egui::WidgetText = text.into();
    let w = text
        .clone()
        .into_galley(
            ui,
            Some(egui::TextWrapMode::Extend),
            f32::INFINITY,
            egui::TextStyle::Body,
        )
        .size()
        .x;
    let key = egui::Id::NULL.with(("stable_label", id));
    let slot = ui.ctx().data_mut(|d| {
        let m = d.get_temp_mut_or(key, 0.0f32);
        *m = m.max(w);
        *m
    });
    let h = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(
        Vec2::new(slot, h),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(slot); // the slot's width, also when this text is shorter
            ui.label(text)
        },
    )
    .inner
}

#[cfg(test)]
mod stable_label_tests {
    use super::*;

    /// Owner: the recording time made the rest of the history row flicker left and right. The control after a
    /// stable label keeps its place when the text gets shorter again.
    #[test]
    fn controls_after_a_changing_readout_stay_put() {
        let ctx = egui::Context::default();
        let mut xs = Vec::new();
        for text in [
            "Recording 1.0 s · 9",
            "Recording 10.25 s · 9.99k",
            "Recording 1.1 s · 11",
            "Recording 10.30 s · 10.0k",
        ] {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.horizontal(|ui| {
                    stable_label(ui, "test_head", egui::RichText::new(text).strong());
                    xs.push(ui.button("|<").rect.min.x);
                });
            });
        }
        assert!(xs[1] > xs[0], "a wider text widens the slot: {xs:?}");
        assert_eq!(
            xs[2], xs[1],
            "a shorter text doesn't pull the controls back: {xs:?}"
        );
        assert!(xs[3] >= xs[2], "{xs:?}");
    }
}

fn help(resp: egui::Response, text: &str) -> egui::Response {
    let held = resp.is_pointer_button_down_on()
        && resp.ctx.input(|i| {
            i.pointer
                .press_start_time()
                .is_some_and(|t| i.time - t > 0.5)
        });
    if held || resp.long_touched() {
        resp.show_tooltip_text(text);
    } else if resp.is_pointer_button_down_on() {
        resp.ctx
            .request_repaint_after(std::time::Duration::from_millis(100)); // re-check for the hold
    }
    resp.on_hover_text(text)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Level {
    Info,
    Tip,
    Warn,
}

impl App {
    /// Plain-language notices for the info line: what the scope is doing, limits hit, and suggestions.
    fn notices(&self, snap: &Snapshot) -> Vec<(Level, String)> {
        let mut trig_flat = false; // trigger channel is an open / flat input (one message instead of two)
        use Level::*;
        let mut v = Vec::new();
        if self.roll.is_some() && self.refs_on && self.refs.iter().flatten().any(|r| r.on) {
            v.push((
                Tip,
                "References are drawn on captures only — not in stream mode.".to_string(),
            ));
        }
        if !matches!(snap.status.as_str(), "connected" | "simulator" | "") {
            v.push((Warn, snap.status.clone()));
        }
        let tb = self.tb_id;
        let tb_txt = fmt_time(burst_ns_per_div(tb) as f64 * 1e-9);
        if let (Some(_), false) = (self.roll, self.run == Run::Stopped) {
            // streaming live: the STREAM badge shows the rate
        } else {
            let span = burst_ns_per_div(tb) as f64 * 1e-9 * DIV_X as f64;
            let cap = capture_seconds(tb);
            if cap > span * 1.05 {
                let vis = (500.0 * span / cap).round();
                v.push((
                    Info,
                    format!(
                        "Sample-rate limit: the scope samples at most 2.4 MS/s in total (300 kS/s per channel), so at {tb_txt}/div only ~{vis} samples per channel are on screen; each capture holds {:.2} ms.",
                        cap * 1e3
                    ),
                ));
            }
            if !timing_verified(tb) {
                v.push((Warn, format!("The time axis at {tb_txt}/div is not verified on this scope yet — time and frequency readings may be off.")));
            }
            if span >= 1.0 {
                v.push((
                    Info,
                    format!("Slow timebase: one capture takes {span:.0} s and the screen updates once per capture. For a continuous, faster-sampled trace pick a stream rate under the timebase."),
                ));
            }
        }

        if let Some(cfg) = &snap.cfg {
            for ch in 0..CHANNELS {
                let d = &snap.frame.data[ch];
                if !self.on[ch] || !snap.frame.active[ch] || d.is_empty() {
                    continue;
                }
                let (lo, hi) = d
                    .iter()
                    .fold((u16::MAX, 0u16), |(a, b), &s| (a.min(s), b.max(s)));
                let vs = cfg.vscale[ch];
                let (zero, vpc) = (snap.zero[vs.index()][ch], self.cal.vpc(ch, vs));
                let (vlo, vhi) = ((lo as f32 - zero) * vpc, (hi as f32 - zero) * vpc);
                let vdiv = VDIVS[self.vdiv[ch]];
                if lo <= 3 || hi >= 4092 {
                    if vs == VScale::X1 {
                        v.push((Warn, format!("CH{} exceeds the scope's input range (about ±19 V) and is clipped — use a ×10 or ×20 attenuator.", ch + 1)));
                    } else {
                        v.push((
                            Warn,
                            format!("CH{} is clipped by its hardware range (±{}) — choose a larger V/div.", ch + 1, fmt_v(range_half_span(vs))),
                        ));
                    }
                    continue;
                }
                if vhi / vdiv + self.offset[ch] > DIV_Y / 2.0
                    || vlo / vdiv + self.offset[ch] < -DIV_Y / 2.0
                {
                    v.push((
                        Info,
                        format!(
                            "CH{} goes off-screen — increase V/div or move its offset.",
                            ch + 1
                        ),
                    ));
                }
                // a finer hardware range would hold the whole signal → suggest the V/div that selects it
                let vmax = vlo.abs().max(vhi.abs());
                let has_signal = hi - lo >= 20; // an open input shows ~5 counts of noise; don't advise on that
                if !has_signal
                    && ch == self.trig_src as usize
                    && self.roll.is_none()
                    && self.trig_mode != TrigMode::Free
                {
                    // one plain message instead of 'level outside the signal' + 'no trigger found' for a flat input
                    trig_flat = true;
                    let p = &self.probes[ch];
                    v.push((
                        Warn,
                        format!(
                            "CH{}: no signal — flat at {} (only noise). Check the probe tip and ground clip; the trigger waits for a signal.",
                            ch + 1,
                            fmt_u((vlo + vhi) / 2.0 * p.scale + p.offset, &p.unit)
                        ),
                    ));
                    continue;
                }
                if !has_signal {
                    // fall through to the trigger check only
                } else if let Some(fine) = VScale::ALL
                    .into_iter()
                    .find(|f| f.index() < vs.index() && range_half_span(*f) >= vmax * 1.1)
                {
                    let room = DIV_Y / 2.0 + self.offset[ch].abs();
                    if let Some(&sug) = VDIVS
                        .iter()
                        .rev()
                        .find(|&&x| room * x <= range_half_span(fine))
                    {
                        if sug * (DIV_Y / 2.0 - self.offset[ch].abs()) >= vmax {
                            let gain = volts_per_count(vs) / volts_per_count(fine);
                            v.push((
                                Tip,
                                format!("CH{}: set V/div to {} or less for {:.0}× finer resolution (the signal fits a finer hardware range).", ch + 1, fmt_volts(sug), gain),
                            ));
                        }
                    }
                }
                if !self.roll.is_some()
                    && self.trig_mode != TrigMode::Free
                    && ch == self.trig_src as usize
                    && {
                        // compare in the channel's unit (vlo/vhi are scope volts)
                        let p = &self.probes[ch];
                        let (ulo, uhi) = (vlo * p.scale + p.offset, vhi * p.scale + p.offset);
                        self.trig_v > ulo.max(uhi) || self.trig_v < ulo.min(uhi)
                    }
                {
                    v.push((
                        Warn,
                        format!(
                            "Trigger level {} is outside CH{}'s signal ({} … {}) — it can never trigger.",
                            fmt_u(self.trig_v, &self.probes[ch].unit),
                            ch + 1,
                            fmt_u(vlo * self.probes[ch].scale + self.probes[ch].offset, &self.probes[ch].unit),
                            fmt_u(vhi * self.probes[ch].scale + self.probes[ch].offset, &self.probes[ch].unit)
                        ),
                    ));
                }
            }
        }

        if self.roll.is_none() && self.run == Run::Running {
            if self.trig_mode == TrigMode::Auto && snap.forced && !trig_flat {
                v.push((Warn, format!("No trigger found — Auto shows untriggered captures. Check the trigger source (CH{}) and level.", self.trig_src + 1)));
            }
            if self.trig_mode == TrigMode::Normal
                && self.last_frame_at.elapsed().as_secs_f32() > 1.0
            {
                v.push((Info, "Normal mode: waiting for a trigger…".into()));
            }
        }
        if self.sw != SwTrig::Off {
            if self.roll.is_some() {
                v.push((
                    Info,
                    if matches!(self.sw, SwTrig::Zone { .. }) { "Zone match filter needs captures (not available while streaming).".to_string() } else { format!("Match filter on the stream recording: {} match(es) so far; [ and ] jump between them when stopped.", self.stream_hits.len()) },
                ));
            } else {
                let total = self.acq.history.lock().map_or(0, |h| h.len());
                let what = format!(
                    "Match filter on CH{}: {}",
                    self.trig_src + 1,
                    sw_describe(&self.sw, self.unit(self.trig_src as usize))
                );
                if self.hits.is_empty() {
                    v.push((Info, format!("{what} — no match yet in {total} captures; the screen shows live captures until one matches.")));
                } else {
                    v.push((
                        Info,
                        format!(
                            "{what} — {} matching of {total} captures; [ and ] jump between them.",
                            self.hits.len()
                        ),
                    ));
                }
                if self.sw_stop && self.run == Run::Running {
                    v.push((
                        Info,
                        "Stop on match: armed — capturing until the next match.".into(),
                    ));
                }
            }
        }
        self.files_notice(&mut v);
        self.decode_notice(snap, &mut v);
        if let Some((msg, at)) = &self.autoset_msg {
            if at.elapsed().as_secs_f32() < 6.0 || self.autoset.is_some() {
                v.push((Info, msg.clone()));
            }
        }
        // zoom: shown as the ZOOM badge in the info line
        if self
            .range_changed_at
            .is_some_and(|t| t.elapsed().as_secs_f32() < 0.6)
        {
            v.push((
                Info,
                "Switching hardware range (the scope needs ~0.2 s)…".into(),
            ));
        }
        // most important first
        v.sort_by_key(|(l, _)| match l {
            Warn => 0,
            Info => 1,
            Tip => 2,
        });
        v
    }

    /// Info line: always two rows (the plot never jumps). Row 1 starts with the run-state badge; notices that
    /// don't fit become "+N more" (hover for all). Long notices are cut, full text on hover.
    fn info_line(&self, ui: &mut egui::Ui, notices: &[(Level, String)]) {
        let row = ui.text_style_height(&egui::TextStyle::Body) + ui.spacing().item_spacing.y;
        ui.set_min_height(2.0 * row);
        let style = |lvl: &Level| match lvl {
            Level::Warn => ("!", Color32::from_rgb(255, 180, 60)),
            Level::Info => ("i", Color32::from_rgb(120, 180, 255)),
            Level::Tip => ("Tip", Color32::from_rgb(120, 220, 140)),
        };
        let badge = match (self.run, self.view) {
            (_, Some(no)) => Some((
                format!("VIEWING HISTORY #{}", no + 1),
                Color32::from_rgb(255, 200, 80),
            )),
            (Run::Stopped, None) => Some(("STOPPED".to_string(), Color32::from_rgb(255, 120, 100))),
            (Run::Single, None) => Some((
                "SINGLE: waiting".to_string(),
                Color32::from_rgb(120, 200, 255),
            )),
            (Run::Running, None) => None,
        };
        // state badges after the run state: what mode the scope is in, at a glance (hover for details)
        let mut states: Vec<(String, Color32, String)> = Vec::new();
        let on = self.on.iter().filter(|&&a| a).count();
        if let (Some(id), true) = (self.roll, self.run != Run::Stopped) {
            let r = stream_rate(id, on);
            states.push((format!("STREAM {}/ch", fmt_rate(r)), Color32::from_rgb(140, 200, 255), format!("Streaming continuously at {} per channel with {on} channel(s) on — fewer channels stream faster.", fmt_rate(r))));
        }
        let z = self.zoom;
        if z != Zoom::default() {
            let span = self.span_s() * (z.x1 - z.x0) as f64;
            states.push((
                format!("ZOOM ×{:.1}", 1.0 / (z.x1 - z.x0)),
                Color32::from_rgb(200, 180, 255),
                format!("Zoom ×{:.1} time, ×{:.1} vertical: {} on screen ({}/div). Double-click or 0 = full view, − = zoom out, Backspace = undo.", 1.0 / (z.x1 - z.x0), 1.0 / (z.y1 - z.y0), fmt_time(span), fmt_time(span / DIV_X as f64)),
            ));
        }
        if self.roll.is_some() && self.run != Run::Stopped && self.files_auto() {
            states.push(("REC".into(), Color32::from_rgb(255, 120, 120), "This stream is also being written to a file in the recording folder (auto-store, Settings).".into()));
        }
        if let Some(t) = self.files_event_state() {
            states.push(("EVENT LOG".into(), Color32::from_rgb(255, 170, 90), t));
        }
        if let Some(t) = self.test_badge() {
            states.push((t, Color32::from_rgb(120, 220, 140), "A test run is analysing every capture; the report opens when it ends (Stop in Automotive tests).".into()));
        }
        if let Some(ch) = self.decoder.fast_channel() {
            states.push((
                "FAST 1-CH (exp.)".into(),
                Color32::from_rgb(255, 190, 60),
                format!("Experimental fast CAN mode: only CH{} is captured (2.4 MS/s), the other channels are paused. The firmware's fewer-channel path has a known glitch — some frames will fail the CRC or be missing.", ch + 1),
            ));
        }
        if self.dropout.level > 0 {
            let paused: Vec<&str> = DROP_NAMES[..self.dropout.level as usize].to_vec();
            states.push((
                "REDUCED".into(),
                Color32::from_rgb(255, 210, 90),
                format!("Adaptive quality (Settings): paused {} to keep ≥ {} fps — this computer needed {:.0} ms per frame. They come back by themselves when there is room.", paused.join(", "), DROP_FPS, self.work_ms),
            ));
        }
        if self.persist_on && self.roll.is_some() {
            states.push((
                "persistence paused".into(),
                Color32::GRAY,
                "Persistence only works with captures, not while streaming.".into(),
            ));
        }
        let shown = notices.len().min(2);
        for i in 0..2 {
            ui.horizontal(|ui| {
                if i == 0 {
                    if let Some((t, c)) = &badge {
                        help(
                            ui.add(egui::Label::new(egui::RichText::new(format!(" {t} ")).monospace().strong().color(Color32::BLACK).background_color(*c)).sense(Sense::click())),
                            "Run state. Space = Run/Stop, S = Single; End or Run returns from history browsing to live.",
                        );
                    }
                    for (t, c, tip) in &states {
                        help(ui.add(egui::Label::new(egui::RichText::new(format!(" {t} ")).monospace().small().color(Color32::BLACK).background_color(*c)).sense(Sense::click())), tip);
                    }
                }
                let Some((lvl, text)) = notices.get(i) else {
                    if i == 0 && notices.is_empty() && badge.is_none() && states.is_empty() {
                        ui.label(egui::RichText::new("Ready.").weak());
                    }
                    return;
                };
                let (tag, color) = style(lvl);
                ui.label(egui::RichText::new(format!("{tag:>3}")).monospace().strong().color(color));
                let more = notices.len() - shown;
                if i == 1 && more > 0 {
                    let all: Vec<String> = notices[shown..].iter().map(|(l, t)| format!("{} {t}", style(l).0)).collect();
                    let m = ui.add(egui::Label::new(egui::RichText::new(format!("+{more} more")).strong().color(Color32::GRAY)).sense(Sense::click()));
                    help(m, &all.join("\n"));
                }
                ui.add(egui::Label::new(egui::RichText::new(text).color(color)).truncate());
            });
        }
    }
}

/// min / mean / max / σ of one measured quantity over a range of captures.
#[derive(Clone, Copy, Default)]
struct Stat {
    n: u32,
    min: f64,
    max: f64,
    sum: f64,
    sum2: f64,
}

impl Stat {
    fn add(&mut self, v: f64) {
        if self.n == 0 {
            (self.min, self.max) = (v, v);
        }
        self.n += 1;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.sum += v;
        self.sum2 += v * v;
    }
    fn mean(&self) -> f64 {
        self.sum / self.n.max(1) as f64
    }
    fn sd(&self) -> f64 {
        let m = self.mean();
        (self.sum2 / self.n.max(1) as f64 - m * m).max(0.0).sqrt()
    }
}

/// Measured quantities shown in the measurement panel, in column order.
const NQ: usize = 11;
const Q_NAMES: [&str; NQ] = [
    "Frequency",
    "Period",
    "Duty",
    "High time",
    "Low time",
    "Rise",
    "Fall",
    "Amplitude",
    "Vpp",
    "Mean",
    "RMS",
];
const Q_HELP: [&str; NQ] = [
    "Signal frequency from the rising edges (needs at least two in the capture).",
    "Time between rising edges.",
    "Share of the period the signal is high (above mid-level).",
    "Mean time the signal stays high per pulse (rising → falling edge).",
    "Mean time the signal stays low per pulse (falling → rising edge). Injector on-time with a low-side driver (the usual case): Low time.",
    "Mean 10 % → 90 % rise time. Limited by the sample spacing: at most 2.4 MS/s in total.",
    "Mean 90 % → 10 % fall time. Limited by the sample spacing.",
    "Top minus base: the flat high level minus the flat low level (ignores spikes).",
    "Peak to peak: highest minus lowest sample (includes spikes and noise).",
    "Average voltage of the whole capture (DC level).",
    "Root mean square of the whole capture (DC included).",
];

type Stats = [[Stat; NQ]; CHANNELS];

fn quantities(m: &Meas) -> [Option<f64>; NQ] {
    [
        m.freq,
        m.period,
        m.duty,
        m.width_pos,
        m.width_neg,
        m.rise,
        m.fall,
        Some((m.top - m.base) as f64),
        Some((m.max - m.min) as f64),
        Some(m.mean as f64),
        Some(m.rms as f64),
    ]
}

fn fmt_q(q: usize, v: f64, unit: &str) -> String {
    match q {
        0 => fmt_freq(v),
        1 | 3 | 4 | 5 | 6 => fmt_time_short(v),
        2 => format!("{:.1} %", v * 100.0),
        _ => fmt_u(v as f32, unit),
    }
}

fn fmt_time_short(s: f64) -> String {
    let a = s.abs();
    if a >= 1.0 {
        format!("{s:.3} s")
    } else if a >= 1e-3 {
        format!("{:.3} ms", s * 1e3)
    } else if a >= 1e-6 {
        format!("{:.2} µs", s * 1e6)
    } else {
        format!("{:.0} ns", s * 1e9)
    }
}

/// Measure every shown channel of one stored capture into `acc`.
fn add_capture(
    acc: &mut Stats,
    e: &HEntry,
    on: &[bool; CHANNELS],
    cal: &Cal,
    probes: &[Probe; CHANNELS],
) {
    let Some(cfg) = &e.snap.cfg else { return };
    for ch in 0..CHANNELS {
        let d = &e.snap.frame.data[ch];
        if !on[ch] || d.len() < 16 {
            continue;
        }
        let vs = cfg.vscale[ch];
        let dt = capture_seconds(cfg.timebase_id) / d.len() as f64;
        let vpc = cal.vpc(ch, vs) * probes[ch].scale;
        let zu = e.snap.zero[vs.index()][ch] - probes[ch].offset / vpc;
        let m = measure(d, zu, vpc, Some(dt));
        for (q, v) in quantities(&m).into_iter().enumerate() {
            if let Some(v) = v {
                acc[ch][q].add(v);
            }
        }
    }
}

impl App {
    /// History navigator + statistics range, shown above the info line.
    fn history_panel(&mut self, ui: &mut egui::Ui) {
        let (first, last, count, mb) = {
            let h = self.acq.history.lock().unwrap_or_else(|p| p.into_inner());
            (h.first_no, h.last_no(), h.len(), h.megabytes())
        };
        let Some(last) = last else {
            ui.label(egui::RichText::new("History: no captures yet.").weak());
            return;
        };
        if self.run != Run::Stopped {
            self.view = None;
        }
        let cur = self.view.unwrap_or(last).clamp(first, last);
        let sw_on = self.sw != SwTrig::Off;
        let prev_hit = self.hits.iter().rev().find(|&&n| n < cur).copied();
        let next_hit = self.hits.iter().find(|&&n| n > cur).copied();
        let n_hits = self.hits.len();
        let mut go: Option<i64> = None;
        let mut jump: Option<u64> = None;

        // keyboard (not while typing in a field)
        if !ui.ctx().egui_wants_keyboard_input() {
            ui.input(|i| {
                if i.key_pressed(egui::Key::ArrowLeft)
                    && !i.modifiers.shift
                    && !i.modifiers.alt
                    && !i.modifiers.command
                {
                    go = Some(-1);
                }
                if i.key_pressed(egui::Key::ArrowRight)
                    && !i.modifiers.shift
                    && !i.modifiers.alt
                    && !i.modifiers.command
                {
                    go = Some(1);
                }
                if i.key_pressed(egui::Key::PageUp) {
                    go = Some(-10);
                }
                if i.key_pressed(egui::Key::PageDown) {
                    go = Some(10);
                }
                if i.key_pressed(egui::Key::Home) {
                    jump = Some(first);
                }
                if i.key_pressed(egui::Key::End) {
                    jump = Some(last);
                }
                if i.key_pressed(egui::Key::OpenBracket) {
                    jump = prev_hit.or(jump);
                }
                if i.key_pressed(egui::Key::CloseBracket) {
                    jump = next_hit.or(jump);
                }
            });
            if (go.is_some() || jump.is_some()) && self.run != Run::Stopped {
                // first key while capturing: stop and keep the current capture; the next keys browse
                self.set_run(Run::Stopped);
                (go, jump) = (None, None);
            }
        }

        ui.horizontal(|ui| {
            let label = egui::RichText::new(format!("History {count} captures · {mb:.0} MB")).strong();
            help(stable_label(ui, "history_head", label), "Every triggered capture is kept (oldest dropped above the memory limit in Settings; 512 MB ≈ 30 min). Stop, then step through them: ← → one, PgUp/PgDn ten, Home/End first/last.");
            for (txt, step, tip) in [("|<", i64::MIN, "First capture (Home)"), ("-10", -10, "Back 10 (PgUp)"), ("<", -1, "Back one (←)"), (">", 1, "Forward one (→)"), ("+10", 10, "Forward 10 (PgDn)"), (">|", i64::MAX, "Last capture (End)")] {
                if help(ui.button(txt), tip).clicked() {
                    match step {
                        i64::MIN => jump = Some(first),
                        i64::MAX => jump = Some(last),
                        s => go = Some(s),
                    }
                }
            }
            let mut pos = cur;
            let s = ui.add_sized([(ui.available_width() - 210.0).max(60.0), 18.0], egui::Slider::new(&mut pos, first..=last).show_value(false));
            if help(s, "Scrub through the history (stops capturing).").changed() {
                jump = Some(pos);
            }
            let at = self.acq.history.lock().ok().and_then(|h| h.get(cur)).map(|e| e.at);
            let when = at.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(String::new(), |d| {
                let s = d.as_secs() % 86_400;
                format!("{:02}:{:02}:{:02}.{:03} UTC", s / 3600, s / 60 % 60, s % 60, d.subsec_millis())
            });
            stable_label(ui, "history_at", egui::RichText::new(format!("#{} {when}", cur + 1)).monospace());
            if sw_on {
                ui.separator();
                if let Some(p) = prev_hit {
                    if help(ui.button("< match"), "Previous capture that passes the match filter ([).").clicked() {
                        jump = Some(p);
                    }
                }
                if let Some(nx) = next_hit {
                    if help(ui.button("match >"), "Next capture that passes the match filter (]).").clicked() {
                        jump = Some(nx);
                    }
                }
                ui.label(egui::RichText::new(format!("{n_hits} matches")).color(Color32::from_rgb(255, 160, 60)));
            }
        });
        if go.is_some() || jump.is_some() {
            if self.run != Run::Stopped {
                self.set_run(Run::Stopped);
            }
            let target = jump.unwrap_or_else(|| {
                (cur as i64 + go.unwrap_or(0)).clamp(first as i64, last as i64) as u64
            });
            self.view = Some(target);
        }

        // statistics range (default: everything since start / reset)
        ui.horizontal(|ui| {
            let custom = self.stats_from.is_some() || self.stats_to.is_some();
            let (mut from, mut to) = (self.stats_from.unwrap_or(first).clamp(first, last), self.stats_to.unwrap_or(last).clamp(first, last));
            help(
                ui.label(if custom { "Statistics over captures" } else { "Statistics: all captures" }),
                "Measurement statistics (min / mean / max / σ). Default: all captures since start or Reset (keeps growing). Pick a range to analyse part of the history.",
            );
            let rf = ui.add(egui::DragValue::new(&mut from).range(first..=to).custom_formatter(|v, _| format!("#{}", v as u64 + 1)).custom_parser(|t| t.trim().trim_start_matches('#').parse::<f64>().ok().map(|v| v - 1.0)));
            ui.label("–");
            let rt = ui.add(egui::DragValue::new(&mut to).range(from..=last).custom_formatter(|v, _| format!("#{}", v as u64 + 1)).custom_parser(|t| t.trim().trim_start_matches('#').parse::<f64>().ok().map(|v| v - 1.0)));
            if rf.changed() {
                self.stats_from = Some(from);
            }
            if rt.changed() {
                self.stats_to = Some(to);
            }
            if help(ui.button("from here"), "Start the statistics range at the capture shown.").clicked() {
                self.stats_from = Some(cur);
            }
            if help(ui.button("to here"), "End the statistics range at the capture shown.").clicked() {
                self.stats_to = Some(cur);
            }
            if help(ui.button("All"), "Statistics over all captures since start / Reset (default).").clicked() {
                self.stats_from = None;
                self.stats_to = None;
            }
            if help(ui.button("Reset"), "Restart the 'all captures' statistics from now.").clicked() {
                self.acc_all = Default::default();
                self.acc_next = last + 1;
            }
        });
    }

    /// Feed new captures into the running "all captures" statistics (bounded work per frame).
    fn update_acc(&mut self) {
        if self.dropped(DROP_STATS) {
            return; // catches up (bounded per frame) when restored
        }
        let units = {
            use std::hash::{Hash, Hasher};
            let mut hs = std::collections::hash_map::DefaultHasher::new();
            self.on.hash(&mut hs);
            for p in &self.probes {
                (p.scale.to_bits(), p.offset.to_bits(), &p.unit).hash(&mut hs);
            }
            hs.finish()
        };
        let (first, last) = {
            let h = self.acq.history.lock().unwrap_or_else(|p| p.into_inner());
            (h.first_no, h.last_no())
        };
        if units != self.acc_units {
            // probe / unit / channel change: old values are in other units → recompute from the kept history
            self.acc_units = units;
            self.acc_all = Default::default();
            self.acc_next = first;
            self.range_stats = None;
        }
        let Some(last) = last else { return };
        let start = self.acc_next.max(first);
        if start > last {
            return;
        }
        // the history lock only for taking the capture references (µs); the maths runs outside it, ≤ 2 ms per
        // frame (review 2026-11-23: 300 captures under the lock held the acquisition up to 6 ms per push)
        let items = self.history_items(start, last.min(start + 299));
        let t = Instant::now();
        let mut next = last.min(start + 299) + 1;
        for (no, e) in &items {
            add_capture(&mut self.acc_all, e, &self.on, &self.cal, &self.probes);
            if t.elapsed() > STATS_BUDGET {
                next = no + 1;
                break;
            }
        }
        self.acc_next = next;
    }

    /// `current_stats` for tests (the range statistics and their label).
    #[cfg(test)]
    fn clone_stats_label(&mut self) -> (Stats, String) {
        self.current_stats()
    }

    /// References to the kept captures `from..=to` (the history lock is held only for this, a few µs).
    fn history_items(&self, from: u64, to: u64) -> Vec<(u64, std::sync::Arc<crate::acq::HEntry>)> {
        let h = self.acq.history.lock().unwrap_or_else(|p| p.into_inner());
        (from..=to)
            .filter_map(|no| h.get(no).map(|e| (no, e)))
            .collect()
    }

    /// Statistics to show: the custom range (computed once per range change) or the running ones.
    fn current_stats(&mut self) -> (Stats, String) {
        if self.stats_from.is_none() && self.stats_to.is_none() {
            return (self.acc_all, "all captures".into());
        }
        let (first, last) = {
            let h = self.acq.history.lock().unwrap_or_else(|p| p.into_inner());
            (h.first_no, h.last_no())
        };
        let Some(last) = last else {
            return (Default::default(), String::new());
        };
        let from = self.stats_from.unwrap_or(first).clamp(first, last);
        let to = self.stats_to.unwrap_or(last).clamp(from, last);
        // built up incrementally: new captures are added, a full recompute only when the range itself changes
        // (review 2026-11-23: recomputed in full under the history lock on every capture — 306 ms with 20k
        // captures, the acquisition blocked as long)
        let set = (self.stats_from, self.stats_to);
        if self
            .range_stats
            .as_ref()
            .is_none_or(|r| r.set != set || r.next > to + 1)
        {
            self.range_stats = Some(RangeAcc {
                set,
                next: from,
                stats: Default::default(),
            });
        }
        let next = self.range_stats.as_ref().map_or(from, |r| r.next);
        let mut computing = false;
        if next <= to {
            let start = next.max(first);
            let end = to.min(start + 2999);
            let items = self.history_items(start, end);
            let t = Instant::now();
            let mut done = end + 1;
            if let Some(r) = self.range_stats.as_mut() {
                for (no, e) in &items {
                    add_capture(&mut r.stats, e, &self.on, &self.cal, &self.probes);
                    if t.elapsed() > STATS_BUDGET {
                        done = no + 1;
                        break;
                    }
                }
                r.next = done;
            }
            computing = done <= to;
        }
        let label = format!(
            "captures #{}–#{}{}",
            from + 1,
            to + 1,
            if computing { " (adding up…)" } else { "" }
        );
        (
            self.range_stats
                .as_ref()
                .map(|r| r.stats)
                .unwrap_or_default(),
            label,
        )
    }

    /// Measurement table: one row per shown channel, current value per quantity; hover / hold a cell
    /// for its statistics; optional mean / σ rows.
    fn measurement_panel(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let (stats, range_label) = self.current_stats();
        if range_label.ends_with("(adding up…)") {
            ui.ctx().request_repaint(); // keep adding up, also when stopped and idle
        }
        ui.horizontal(|ui| {
            let b = ui.add(egui::Button::new(egui::RichText::new("▾ Measurements").strong()).frame(false));
            if help(b, "Hide the measurement table (with 8 channels it fills half the screen; statistics keep adding up). Key: F3").clicked() {
                self.meas_on = false;
            }
            let c = ui.checkbox(&mut self.show_stats, "mean / σ rows");
            help(c, "Show the mean and standard deviation of every measurement below its current value.");
            ui.label(egui::RichText::new(format!("statistics: {range_label}")).weak());
        });
        let Some(cfg) = snap.cfg.as_ref() else { return };
        egui::Grid::new("meas").striped(true).spacing([10.0, 2.0]).min_col_width(40.0).show(ui, |ui| {
            ui.label("");
            for (q, name) in Q_NAMES.iter().enumerate() {
                help(ui.add(egui::Label::new(egui::RichText::new(format!("{name:>11}")).monospace().strong()).sense(Sense::click())), Q_HELP[q]);
            }
            ui.end_row();
            for ch in 0..CHANNELS {
                let d = &snap.frame.data[ch];
                if !self.on[ch] || !snap.frame.active[ch] || d.len() < 2 {
                    continue;
                }
                let vs = cfg.vscale[ch];
                let dt = if snap.roll {
                    self.roll.map(|_| 1.0 / self.shown_rate.max(1e-3))
                } else {
                    Some(capture_seconds(cfg.timebase_id) / d.len() as f64)
                };
                let (zu, vu) = self.uconv(ch, vs, snap.zero[vs.index()][ch]);
                // the same data with the same conversion is not measured again (≈ 30 % of frames have no new
                // capture; a full stream screen cost 8–16 ms per frame on the scope — speed review 2026-11-23)
                let key = {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    (snap.frame.seq, snap.roll, self.view, self.stream_end, d.len(), zu.to_bits(), vu.to_bits(), dt.map(f64::to_bits)).hash(&mut h);
                    h.finish()
                };
                let (k0, at, m0) = self.meas_cache[ch];
                let live_stream = snap.roll && self.run == Run::Running;
                let m = if k0 == key || (live_stream && at.is_some_and(|t| t.elapsed().as_millis() < 100)) {
                    m0
                } else {
                    let m = measure(d, zu, vu, dt);
                    self.meas_cache[ch] = (key, Some(Instant::now()), m);
                    m
                };
                let unit = self.probes[ch].unit.clone();
                ui.label(egui::RichText::new(format!("CH{}", ch + 1)).strong().color(COLORS[ch]));
                for (q, v) in quantities(&m).into_iter().enumerate() {
                    let s = stats[ch][q];
                    // rise/fall below ~1.5 samples is just the sample spacing: show the resolution limit, not noise
                    let limit = dt.map(|dt| 1.5 * dt).filter(|_| q == 5 || q == 6);
                    let txt = match (v, limit) {
                        (Some(v), Some(l)) if v < l => format!("< {}", fmt_time_short(l)),
                        (Some(v), _) => fmt_q(q, v, &unit),
                        (None, _) => "--".to_string(),
                    };
                    let tip = if s.n > 0 {
                        format!(
                            "{}: now {txt}\nmin {}   mean {}   max {}\nσ {}   over {} captures ({range_label})",
                            Q_NAMES[q],
                            fmt_q(q, s.min, &unit),
                            fmt_q(q, s.mean(), &unit),
                            fmt_q(q, s.max, &unit),
                            fmt_q(q, s.sd(), &unit),
                            s.n
                        )
                    } else {
                        format!("{}: now {txt}\n(no statistics yet)", Q_NAMES[q])
                    };
                    help(ui.label(egui::RichText::new(format!("{txt:>11}")).monospace().color(COLORS[ch])), &tip);
                }
                ui.end_row();
                if self.show_stats {
                    for (tag, f) in [("mean", Stat::mean as fn(&Stat) -> f64), ("σ", Stat::sd as fn(&Stat) -> f64)] {
                        ui.label(egui::RichText::new(tag).weak());
                        for q in 0..NQ {
                            let s = stats[ch][q];
                            ui.label(egui::RichText::new(format!("{:>11}", if s.n > 0 { fmt_q(q, f(&s), &unit) } else { "--".into() })).monospace().weak());
                        }
                        ui.end_row();
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_min_mean_max_sd() {
        let mut s = Stat::default();
        for v in [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0] {
            s.add(v);
        }
        assert_eq!((s.n, s.min, s.max), (8, 2.0, 9.0));
        assert!((s.mean() - 5.0).abs() < 1e-12);
        assert!((s.sd() - 2.0).abs() < 1e-12);
    }
}

#[cfg(test)]
mod zoom_tests {
    use super::*;

    #[test]
    fn zoom_clamped_stays_inside_and_min_size() {
        let z = Zoom {
            x0: -0.2,
            x1: 0.3,
            y0: 0.9,
            y1: 1.4,
        }
        .clamped();
        assert!((z.x0, z.x1) == (0.0, 0.5));
        assert!((z.y0 - 0.5).abs() < 1e-6 && (z.y1 - 1.0).abs() < 1e-6);
        let tiny = Zoom {
            x0: 0.5,
            x1: 0.5,
            y0: 0.0,
            y1: 1.0,
        }
        .clamped();
        assert!((tiny.x1 - tiny.x0 - 0.001).abs() < 1e-6);
        assert_eq!(Zoom::default().clamped(), Zoom::default());
    }
}

// ───────────────────────────── software triggers (UI) ─────────────────────────────

const SW_NAMES: [&str; 6] = ["Off", "Pulse width", "Runt", "Dropout", "Window", "Zone"];

fn sw_kind(t: &SwTrig) -> usize {
    match t {
        SwTrig::Off => 0,
        SwTrig::Width { .. } => 1,
        SwTrig::Runt { .. } => 2,
        SwTrig::Dropout { .. } => 3,
        SwTrig::Window { .. } => 4,
        SwTrig::Zone { .. } => 5,
    }
}

fn sw_default(kind: usize, level: f32) -> SwTrig {
    match kind {
        1 => SwTrig::Width {
            high: true,
            cmp: Cmp::Longer,
            t1: 1e-3,
            t2: 2e-3,
        },
        2 => SwTrig::Runt {
            high: true,
            lo: level * 0.5,
            hi: level * 1.5 + 0.5,
        },
        3 => SwTrig::Dropout { t: 5e-3 },
        4 => SwTrig::Window {
            lo: level - 1.0,
            hi: level + 1.0,
        },
        5 => SwTrig::Zone {
            t0: 0.0,
            t1: 0.0,
            v0: 0.0,
            v1: 0.0,
            enter: true,
        },
        _ => SwTrig::Off,
    }
}

fn sw_describe(t: &SwTrig, unit: &str) -> String {
    match *t {
        SwTrig::Off => "off".into(),
        SwTrig::Width { high, cmp, t1, t2 } => {
            let p = if high { "high" } else { "low" };
            match cmp {
                Cmp::Longer => format!("{p} pulse longer than {}", fmt_time_short(t1)),
                Cmp::Shorter => format!("{p} pulse shorter than {}", fmt_time_short(t1)),
                Cmp::Between => format!(
                    "{p} pulse {} … {}",
                    fmt_time_short(t1.min(t2)),
                    fmt_time_short(t1.max(t2))
                ),
            }
        }
        SwTrig::Runt { high, lo, hi } => format!(
            "{} runt between {} and {}",
            if high { "high" } else { "low" },
            fmt_u(lo, unit),
            fmt_u(hi, unit)
        ),
        SwTrig::Dropout { t } => format!("no edge for {}", fmt_time_short(t)),
        SwTrig::Window { lo, hi } => {
            format!("signal leaves {} … {}", fmt_u(lo, unit), fmt_u(hi, unit))
        }
        SwTrig::Zone {
            t0,
            t1,
            v0,
            v1,
            enter,
        } => {
            if t0 == t1 {
                "zone (not drawn yet)".into()
            } else {
                format!(
                    "trace {} zone {} … {}, {} … {}",
                    if enter { "enters" } else { "avoids" },
                    fmt_time_short(t0.min(t1)),
                    fmt_time_short(t0.max(t1)),
                    fmt_u(v0.min(v1), unit),
                    fmt_u(v0.max(v1), unit)
                )
            }
        }
    }
}

/// Edit a time value in milliseconds.
fn ms_value(ui: &mut egui::Ui, t: &mut f64, tip: &str) {
    let mut ms = *t * 1e3;
    let speed = (ms.abs() * 0.01).max(0.001);
    let r = ui.add(
        egui::DragValue::new(&mut ms)
            .speed(speed) // 1 % per pixel: 50 µs and 50 ms both easy to set
            .range(0.001..=100_000.0)
            .suffix(" ms")
            .max_decimals(3),
    );
    if help(r, tip).changed() {
        *t = ms * 1e-3;
    }
}

fn volt_value(ui: &mut egui::Ui, v: &mut f32, unit: &str, udiv: f32, tip: &str) {
    help(
        ui.add(
            egui::DragValue::new(v)
                .speed(udiv / 50.0) // one division per 50 px at the trigger channel's scale
                .suffix(format!(" {unit}"))
                .max_decimals(3),
        ),
        tip,
    );
}

impl App {
    /// "Software trigger" section of the controls.
    fn sw_controls(&mut self, ui: &mut egui::Ui) {
        if self.roll.is_some() {
            ui.label(
                egui::RichText::new(
                    "Streaming: matches are searched in the whole recording (zone needs captures).",
                )
                .small()
                .weak(),
            );
        }
        let mut kind = sw_kind(&self.sw);
        let cb = egui::ComboBox::from_id_salt("sw_kind")
            .width(120.0)
            .selected_text(SW_NAMES[kind])
            .show_ui(ui, |ui| {
                for (i, n) in SW_NAMES.iter().enumerate() {
                    ui.selectable_value(&mut kind, i, *n);
                }
            });
        help(
            cb.response,
            "Match filter: an extra condition checked on every capture of the trigger source channel (on top of the edge trigger). While running, the screen shows the latest matching capture; the whole history is searched too — jump between matches with [ and ].",
        );
        if kind != sw_kind(&self.sw) {
            self.sw = sw_default(kind, self.trig_v);
        }
        let unit = self.probes[self.trig_src as usize].unit.clone(); // thresholds are in the trigger channel's unit
        let ud = self.udiv(self.trig_src as usize);
        match &mut self.sw {
            SwTrig::Off => {}
            SwTrig::Width { high, cmp, t1, t2 } => {
                ui.horizontal(|ui| {
                    help(
                        ui.selectable_value(high, true, "High"),
                        "Pulse above the trigger level.",
                    );
                    help(
                        ui.selectable_value(high, false, "Low"),
                        "Pulse below the trigger level.",
                    );
                    help(
                        ui.selectable_value(cmp, Cmp::Longer, ">"),
                        "Longer than the time.",
                    );
                    help(
                        ui.selectable_value(cmp, Cmp::Shorter, "<"),
                        "Shorter than the time.",
                    );
                    help(
                        ui.selectable_value(cmp, Cmp::Between, "between"),
                        "Between two times.",
                    );
                });
                ui.horizontal(|ui| {
                    ms_value(
                        ui,
                        t1,
                        "Pulse width limit (measured at the threshold below).",
                    );
                    if *cmp == Cmp::Between {
                        ui.label("…");
                        ms_value(ui, t2, "Second pulse width limit.");
                    }
                });
            }
            SwTrig::Runt { high, lo, hi } => {
                ui.horizontal(|ui| {
                    help(
                        ui.selectable_value(high, true, "High"),
                        "Rises above the lower threshold but turns back before the upper one.",
                    );
                    help(
                        ui.selectable_value(high, false, "Low"),
                        "Falls below the upper threshold but turns back before the lower one.",
                    );
                });
                ui.horizontal(|ui| {
                    volt_value(ui, lo, &unit, ud, "Lower threshold.");
                    ui.label("…");
                    volt_value(ui, hi, &unit, ud, "Upper threshold.");
                });
            }
            SwTrig::Dropout { t } => {
                ui.horizontal(|ui| {
                    ui.label("no edge for");
                    ms_value(ui, t, "Matches when the signal doesn't cross the threshold below for longer than this (signal dropouts, missing teeth).");
                });
            }
            SwTrig::Window { lo, hi } => {
                ui.horizontal(|ui| {
                    ui.label("leaves");
                    volt_value(ui, lo, &unit, ud, "Lower edge of the allowed band.");
                    ui.label("…");
                    volt_value(ui, hi, &unit, ud, "Upper edge of the allowed band.");
                });
            }
            SwTrig::Zone { enter, .. } => {
                ui.horizontal(|ui| {
                    let b = ui.selectable_label(self.zone_draw, "Draw zone");
                    if help(
                        b,
                        "Then drag a box on the graph to define the zone (instead of zooming).",
                    )
                    .clicked()
                    {
                        self.zone_draw = !self.zone_draw;
                    }
                    help(
                        ui.selectable_value(enter, true, "enters"),
                        "Match when the trace passes through the zone.",
                    );
                    help(
                        ui.selectable_value(enter, false, "avoids"),
                        "Match when the trace never enters the zone.",
                    );
                });
            }
        }
        if matches!(self.sw, SwTrig::Width { .. } | SwTrig::Dropout { .. }) {
            self.sw_level_controls(ui);
        }
        if self.sw != SwTrig::Off {
            let c = ui.checkbox(&mut self.sw_stop, "Stop on match");
            if help(c, "Like a Normal trigger in Single mode: keep capturing until the filter matches, then stop and keep that capture on screen. Press Run to arm it again. Older matches in the history don't count.").changed()
                && self.sw_stop
            {
                self.arm_stop_on_match();
            }
        }
        if self.sw != SwTrig::Off {
            ui.label(
                egui::RichText::new(sw_describe(&self.sw, self.unit(self.trig_src as usize)))
                    .weak(),
            );
        }
    }

    /// Evaluate the software trigger on a stored capture (trigger source channel).
    /// All match events in a capture (for drawing), at most 200.
    fn sw_all(&self, s: &Snapshot) -> Vec<SwHit> {
        if self.sw == SwTrig::Off || (s.roll && matches!(self.sw, SwTrig::Zone { .. })) {
            return Vec::new();
        }
        let Some(cfg) = s.cfg.as_ref() else {
            return Vec::new();
        };
        let ch = self.trig_src as usize;
        let d = &s.frame.data[ch];
        let Some(level) = self.sw_threshold(s) else {
            return Vec::new();
        };
        if d.len() < 2 {
            return Vec::new();
        }
        let vs = cfg.vscale[ch];
        let (dt, trig_idx) = if s.roll {
            (1.0 / self.shown_rate.max(1e-9), 0.0)
        } else {
            (
                capture_seconds(cfg.timebase_id) / d.len() as f64,
                cfg.trig_pos as f64 / 1000.0 * d.len() as f64,
            )
        };

        sw_matches(
            &self.sw,
            d,
            self.uconv(ch, vs, s.zero[vs.index()][ch]).0,
            self.uconv(ch, vs, s.zero[vs.index()][ch]).1,
            dt,
            trig_idx,
            level,
            200,
        )
    }

    fn sw_eval(&self, s: &Snapshot) -> Option<SwHit> {
        if self.sw == SwTrig::Off || s.roll {
            return None;
        }
        let cfg = s.cfg.as_ref()?;
        let ch = self.trig_src as usize;
        let d = &s.frame.data[ch];
        if d.len() < 2 {
            return None;
        }
        let vs = cfg.vscale[ch];
        let dt = capture_seconds(cfg.timebase_id) / d.len() as f64;
        let trig_idx = cfg.trig_pos as f64 / 1000.0 * d.len() as f64;
        let level = self.sw_threshold(s)?;
        sw_trigger(
            &self.sw,
            d,
            self.uconv(ch, vs, s.zero[vs.index()][ch]).0,
            self.uconv(ch, vs, s.zero[vs.index()][ch]).1,
            dt,
            trig_idx,
            level,
        )
    }

    /// Threshold (volts) the match filter uses for pulse widths / dropouts on this capture:
    /// auto = 50 % between the trace's min and max, the capture-trigger level, or a custom level.
    fn sw_threshold(&self, s: &Snapshot) -> Option<f32> {
        match self.sw_level {
            SwLevel::Trigger => Some(self.trig_v),
            SwLevel::Custom(v) => Some(v),
            SwLevel::Auto => {
                let cfg = s.cfg.as_ref()?;
                let ch = self.trig_src as usize;
                let d = &s.frame.data[ch];
                let (lo, hi) = d
                    .iter()
                    .fold((u16::MAX, 0u16), |(a, b), &v| (a.min(v), b.max(v)));
                if d.is_empty() {
                    return None;
                }
                let vs = cfg.vscale[ch];
                Some({
                    let (zu, vu) = self.uconv(ch, vs, s.zero[vs.index()][ch]);
                    ((lo as f32 + hi as f32) / 2.0 - zu) * vu
                })
            }
        }
    }

    /// Search new (or, after a settings change, all) history captures for software-trigger matches.
    fn update_hits(&mut self) {
        let key = (self.sw, self.trig_src, self.trig_v, self.sw_level);
        if self.hits_for != Some(key) {
            self.hits.clear();
            self.hits_next = 0;
            self.hits_for = Some(key);
        }
        if self.sw == SwTrig::Off {
            return;
        }
        let (first, last) = {
            let h = self.acq.history.lock().unwrap_or_else(|p| p.into_inner());
            (h.first_no, h.last_no())
        };
        let Some(last) = last else { return };
        self.hits.retain(|&n| n >= first);
        let start = self.hits_next.max(first);
        if start > last {
            return;
        }
        let end = last.min(start + 2000); // bounded work per frame; a full history rescan takes a few frames
                                          // references under the lock, the filter outside it (it held the lock up to 2.2 ms per frame)
        let items = self.history_items(start, end);
        let mut stop_at = None;
        for (no, e) in items {
            {
                if self.sw_eval(&e.snap).is_some() {
                    self.hits.push(no);
                    // stop on match: the first match among captures taken after arming
                    if self.sw_stop
                        && self.run == Run::Running
                        && no > self.sw_arm_from
                        && stop_at.is_none()
                    {
                        stop_at = Some(no);
                    }
                }
            }
        }
        self.hits_next = end + 1;
        if let Some(no) = stop_at {
            log::info!(
                "stop on match: capture #{no} matched ({}), capturing stopped",
                sw_describe(&self.sw, self.unit(self.trig_src as usize))
            );
            self.set_run(Run::Stopped);
            self.view = Some(no);
            let what = sw_describe(&self.sw, self.unit(self.trig_src as usize));
            self.files_msg(
                Level::Warn,
                format!(
                    "Stopped: capture #{} passed the match filter ({what}). Run (Space) continues.",
                    no + 1
                ),
            );
        }
    }

    /// Arm "stop on match": only captures newer than the current last one can stop the run.
    fn arm_stop_on_match(&mut self) {
        self.stream_arm_from = self
            .acq
            .history
            .lock()
            .ok()
            .map_or(0, |h| h.stream.dropped + h.stream.rows() as u64);
        self.sw_arm_from = self
            .acq
            .history
            .lock()
            .ok()
            .and_then(|h| h.last_no())
            .unwrap_or(0);
    }
}

/// Settings-file form of a software trigger, e.g. `width,high,longer,0.001,0.002`.
fn sw_encode(t: &SwTrig) -> String {
    let pol = |h: bool| if h { "high" } else { "low" };
    match *t {
        SwTrig::Off => "off".into(),
        SwTrig::Width { high, cmp, t1, t2 } => {
            let c = match cmp {
                Cmp::Longer => "longer",
                Cmp::Shorter => "shorter",
                Cmp::Between => "between",
            };
            format!("width,{},{c},{t1},{t2}", pol(high))
        }
        SwTrig::Runt { high, lo, hi } => format!("runt,{},{lo},{hi}", pol(high)),
        SwTrig::Dropout { t } => format!("dropout,{t}"),
        SwTrig::Window { lo, hi } => format!("window,{lo},{hi}"),
        SwTrig::Zone {
            t0,
            t1,
            v0,
            v1,
            enter,
        } => format!(
            "zone,{t0},{t1},{v0},{v1},{}",
            if enter { "enter" } else { "avoid" }
        ),
    }
}

fn sw_decode(s: &str) -> Option<SwTrig> {
    let p: Vec<&str> = s.split(',').map(str::trim).collect();
    let f = |i: usize| {
        p.get(i)
            .and_then(|x| x.parse::<f64>().ok())
            .filter(|x| x.is_finite())
    };
    let v = |i: usize| {
        p.get(i)
            .and_then(|x| x.parse::<f32>().ok())
            .filter(|x| x.is_finite())
    };
    let high = |i: usize| p.get(i).map(|x| *x != "low");
    Some(match *p.first()? {
        "off" => SwTrig::Off,
        "width" => SwTrig::Width {
            high: high(1)?,
            cmp: match *p.get(2)? {
                "shorter" => Cmp::Shorter,
                "between" => Cmp::Between,
                _ => Cmp::Longer,
            },
            t1: f(3)?,
            t2: f(4)?,
        },
        "runt" => SwTrig::Runt {
            high: high(1)?,
            lo: v(2)?,
            hi: v(3)?,
        },
        "dropout" => SwTrig::Dropout { t: f(1)? },
        "window" => SwTrig::Window {
            lo: v(1)?,
            hi: v(2)?,
        },
        "zone" => SwTrig::Zone {
            t0: f(1)?,
            t1: f(2)?,
            v0: v(3)?,
            v1: v(4)?,
            enter: p.get(5).is_none_or(|x| *x != "avoid"),
        },
        _ => return None,
    })
}

#[cfg(test)]
mod sw_settings_tests {
    use super::*;

    #[test]
    fn sw_encode_decode_roundtrip() {
        for t in [
            SwTrig::Off,
            SwTrig::Width {
                high: false,
                cmp: Cmp::Between,
                t1: 0.001,
                t2: 0.0025,
            },
            SwTrig::Runt {
                high: true,
                lo: 0.5,
                hi: 1.5,
            },
            SwTrig::Dropout { t: 0.005 },
            SwTrig::Window { lo: -1.0, hi: 2.0 },
            SwTrig::Zone {
                t0: -0.0002,
                t1: 0.0003,
                v0: 0.5,
                v1: 1.5,
                enter: false,
            },
        ] {
            assert_eq!(sw_decode(&sw_encode(&t)), Some(t));
        }
        assert_eq!(sw_decode("nonsense"), None);
    }
}

/// Where the match filter measures pulse widths / dropouts.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SwLevel {
    /// 50 % between the trace's min and max (default; independent of the capture trigger)
    Auto,
    /// the capture trigger's level
    Trigger,
    Custom(f32),
}

impl App {
    /// Threshold selector for the match filter (pulse width / dropout).
    fn sw_level_controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("at");
            let a = ui.selectable_label(self.sw_level == SwLevel::Auto, "50 %");
            if help(a, "Measure at 50 % between the signal's lowest and highest value (works whatever the capture trigger level is).").clicked() {
                self.sw_level = SwLevel::Auto;
            }
            let t = ui.selectable_label(self.sw_level == SwLevel::Trigger, "trigger level");
            if help(t, "Measure at the capture trigger's level.").clicked() {
                self.sw_level = SwLevel::Trigger;
            }
            let mut v = match self.sw_level {
                SwLevel::Custom(v) => v,
                _ => self.trig_v,
            };
            let c = ui.add(egui::DragValue::new(&mut v).speed(0.02).suffix(format!(" {}", self.probes[self.trig_src as usize].unit)).max_decimals(3));
            if help(c, "Measure at this voltage (drag or type).").changed() {
                self.sw_level = SwLevel::Custom(v);
            }
        });
    }
}

fn sw_level_encode(l: SwLevel) -> String {
    match l {
        SwLevel::Auto => "auto".into(),
        SwLevel::Trigger => "trigger".into(),
        SwLevel::Custom(v) => format!("{v}"),
    }
}

fn sw_level_decode(s: &str) -> SwLevel {
    match s.trim() {
        "trigger" => SwLevel::Trigger,
        x => x
            .parse::<f32>()
            .map(SwLevel::Custom)
            .unwrap_or(SwLevel::Auto),
    }
}

impl App {
    /// Pane a channel is drawn in (assignments beyond the current pane count fall into the last pane).
    fn pane_for(&self, ch: usize) -> usize {
        self.pane_of[ch].min(self.panes.clamp(1, MAX_PANES) - 1)
    }

    /// Change the number of panes: spread the shown channels round-robin over them, equal heights.
    fn set_panes(&mut self, n: usize) {
        let n = n.clamp(1, MAX_PANES);
        self.panes = n;
        let mut k = 0;
        for ch in 0..CHANNELS {
            if self.on[ch] {
                self.pane_of[ch] = k % n;
                k += 1;
            } else {
                self.pane_of[ch] = 0;
            }
        }
        self.pane_h = [1.0; MAX_PANES];
    }
}

impl App {
    /// Debug aid for headless checks: with HANTEK_SHOT=<file.ppm> the app saves one screenshot of its window
    /// after HANTEK_SHOT_AFTER seconds (default 3) as a binary PPM. No effect otherwise.
    fn debug_screenshot(&mut self, ctx: &egui::Context) {
        if self.shot_done {
            return;
        }
        let Ok(path) = std::env::var("HANTEK_SHOT") else {
            self.shot_done = true;
            return;
        };
        let after: f32 = std::env::var("HANTEK_SHOT_AFTER")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3.0);
        if self.started.elapsed().as_secs_f32() < after {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }
        self.shot_done = true;
        ctx.request_screenshot(move |img| {
            let [w, h] = img.size;
            let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
            out.reserve(w * h * 3);
            for p in &img.pixels {
                out.extend_from_slice(&[p.r(), p.g(), p.b()]);
            }
            match std::fs::write(&path, out) {
                Ok(()) => log::info!("screenshot saved to {path}"),
                Err(e) => log::warn!("screenshot {path}: {e}"),
            }
        });
        ctx.request_repaint();
    }
}

// ───────────────────────────── stream recording (browse / search) ─────────────────────────────

impl App {
    /// Rows on one screen at the current stream rate.
    fn stream_rows_per_screen(&self) -> usize {
        self.roll.map_or(1, |_| {
            (self.shown_rate * self.span_s()).ceil().max(2.0) as usize
        })
    }

    /// One screen of the stream recording ending at absolute row `end` (None = latest), as a snapshot.
    fn stream_window(&mut self, live: &Snapshot) -> Option<Snapshot> {
        let h = self.acq.history.lock().ok()?;
        let rec = &h.stream;
        let len = rec.rows() as u64;
        if len < 2 {
            return None;
        }
        let total = rec.dropped + len;
        let end = self
            .stream_end
            .unwrap_or(total)
            .clamp(rec.dropped + 2, total);
        let key = (end, rec.dropped, len.min(end));
        if let Some((k, s)) = &self.stream_cache {
            if *k == key {
                return Some(s.clone());
            }
        }
        let n = self.stream_rows_per_screen() as u64;
        let (a, b) = (
            (end.saturating_sub(n)).max(rec.dropped) - rec.dropped,
            end - rec.dropped,
        );
        let mut s = Snapshot {
            cfg: rec.cfg.clone(),
            zero: rec.zero,
            roll: true,
            status: live.status.clone(),
            ..Default::default()
        };
        s.frame.active = rec.active;
        for ch in 0..CHANNELS {
            if rec.active[ch] {
                s.frame.data[ch] = rec.data[ch][a as usize..b as usize].to_vec();
            }
        }
        drop(h);
        log::debug!(
            "stream_window end={end} n={n} rows {a}..{b} ranges {:?}",
            (0..CHANNELS)
                .filter(|&c| !s.frame.data[c].is_empty())
                .map(|c| (
                    c,
                    s.frame.data[c].iter().min(),
                    s.frame.data[c].iter().max()
                ))
                .collect::<Vec<_>>()
        );
        self.stream_cache = Some((key, s.clone()));
        Some(s)
    }

    /// Navigator for the stream recording (shown instead of the capture navigator in stream mode).
    fn stream_panel(&mut self, ui: &mut egui::Ui) {
        self.file_overview(ui); // whole opened recording file (only when one is open)
        let (dropped, len, rate, started) = {
            let h = self.acq.history.lock().unwrap_or_else(|p| p.into_inner());
            (
                h.stream.dropped,
                h.stream.rows() as u64,
                h.stream.rate.max(1e-9),
                h.stream.started,
            )
        };
        if len < 2 {
            ui.label(egui::RichText::new("Recording: waiting for stream data.").weak());
            return;
        }
        let total = dropped + len;
        let screen = self.stream_rows_per_screen() as u64;
        let cur = self.stream_end.unwrap_or(total).clamp(dropped + 2, total);
        if self.run != Run::Stopped {
            self.stream_end = None;
        }
        let mut target: Option<u64> = None;
        let step = |d: i64| Some((cur as i64 + d).clamp((dropped + 2) as i64, total as i64) as u64);
        let prev_hit = self
            .stream_hits
            .iter()
            .rev()
            .find(|&&h| h + screen / 2 < cur)
            .copied();
        let next_hit = self
            .stream_hits
            .iter()
            .find(|&&h| h + screen / 2 > cur)
            .copied();
        if !ui.ctx().egui_wants_keyboard_input() {
            ui.input(|i| {
                let tenth = (screen / 10).max(1) as i64;
                if i.key_pressed(egui::Key::ArrowLeft)
                    && !i.modifiers.shift
                    && !i.modifiers.alt
                    && !i.modifiers.command
                {
                    target = step(-tenth);
                }
                if i.key_pressed(egui::Key::ArrowRight)
                    && !i.modifiers.shift
                    && !i.modifiers.alt
                    && !i.modifiers.command
                {
                    target = step(tenth);
                }
                if i.key_pressed(egui::Key::PageUp) {
                    target = step(-(screen as i64));
                }
                if i.key_pressed(egui::Key::PageDown) {
                    target = step(screen as i64);
                }
                if i.key_pressed(egui::Key::Home) {
                    target = Some(dropped + screen.min(len));
                }
                if i.key_pressed(egui::Key::End) {
                    target = Some(total);
                }
                if i.key_pressed(egui::Key::OpenBracket) {
                    target = prev_hit.map(|h| h + screen / 2).or(target);
                }
                if i.key_pressed(egui::Key::CloseBracket) {
                    target = next_hit.map(|h| h + screen / 2).or(target);
                }
            });
            if target.is_some() && self.run != Run::Stopped {
                self.set_run(Run::Stopped); // first key while streaming: stop; the next keys browse
                target = None;
            }
        }
        ui.horizontal(|ui| {
            let secs = len as f64 / rate;
            help(
                stable_label(ui, "stream_head", egui::RichText::new(format!("Recording {} · {}", fmt_time(secs), fmt_count(len as f64))).strong()),
                "Everything streamed since the stream started is kept (oldest dropped above the memory limit in Settings). Stop, then scroll through it: ← → a tenth of a screen, PgUp/PgDn a screen, Home/End start/end.",
            );
            for (txt, d, tip) in [
                ("|<", i64::MIN, "Start of the recording (Home)"),
                ("-1", -(screen as i64), "Back one screen (PgUp)"),
                ("<", -((screen / 10).max(1) as i64), "Back a tenth of a screen (←)"),
                (">", (screen / 10).max(1) as i64, "Forward a tenth of a screen (→)"),
                ("+1", screen as i64, "Forward one screen (PgDn)"),
                (">|", i64::MAX, "Latest data (End)"),
            ] {
                if help(ui.button(txt), tip).clicked() {
                    target = match d {
                        i64::MIN => Some(dropped + screen.min(len)),
                        i64::MAX => Some(total),
                        d => step(d),
                    };
                }
            }
            let mut pos = cur;
            let s = ui.add_sized([(ui.available_width() - 230.0).max(60.0), 18.0], egui::Slider::new(&mut pos, (dropped + 2)..=total).show_value(false));
            if help(s, "Scroll through the recording (stops streaming).").changed() {
                target = Some(pos);
            }
            let t_end = cur as f64 / rate; // since the stream / file start (rows are absolute)
            let wall = started.map(|t| t + std::time::Duration::from_secs_f64((cur as f64) / rate));
            let when = wall.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(String::new(), |d| {
                let s = d.as_secs() % 86_400;
                format!(" · {:02}:{:02}:{:02} UTC", s / 3600, s / 60 % 60, s % 60)
            });
            stable_label(ui, "stream_at", egui::RichText::new(format!("t {}{when}", fmt_time(t_end))).monospace());
            if self.sw != SwTrig::Off {
                ui.separator();
                if let Some(p) = prev_hit {
                    if help(ui.button("< match"), "Previous match in the recording ([).").clicked() {
                        target = Some(p + screen / 2);
                    }
                }
                if let Some(nx) = next_hit {
                    if help(ui.button("match >"), "Next match in the recording (]).").clicked() {
                        target = Some(nx + screen / 2);
                    }
                }
                ui.label(egui::RichText::new(format!("{} matches", self.stream_hits.len())).color(Color32::from_rgb(255, 160, 60)));
            }
        });
        if let Some(t) = target {
            if self.run != Run::Stopped {
                self.set_run(Run::Stopped);
            }
            self.stream_end = Some(t.clamp(dropped + 2, total));
        }
    }

    /// Search new stream rows for match-filter events (with overlap so events across chunk edges are found).
    fn update_stream_hits(&mut self) {
        let key = (self.sw, self.trig_src, self.trig_v, self.sw_level);
        if self.stream_hits_for != Some(key) {
            self.stream_hits.clear();
            self.stream_scan = 0;
            self.stream_hits_for = Some(key);
        }
        if self.sw == SwTrig::Off || matches!(self.sw, SwTrig::Zone { .. }) || self.roll.is_none() {
            return;
        }
        let dt = 1.0 / self.shown_rate.max(1e-9);
        let h = self.acq.history.lock().unwrap_or_else(|p| p.into_inner());
        let rec = &h.stream;
        let ch = self.trig_src as usize;
        let Some(cfg) = rec.cfg.as_ref() else { return };
        if !rec.active[ch] {
            return;
        }
        let d = &rec.data[ch];
        let total = rec.dropped + d.len() as u64;
        if self.stream_scan > total {
            self.stream_scan = 0; // new recording
            self.stream_hits.clear();
        }
        // longest event we care about, as overlap between scans
        let longest = match self.sw {
            SwTrig::Width { t1, t2, .. } => t1.max(t2),
            SwTrig::Dropout { t } => t,
            _ => 0.0,
        };
        let overlap = ((longest * 2.0 / dt) as u64).max(64);
        let from = self.stream_scan.saturating_sub(overlap).max(rec.dropped);
        if total <= from + 2 {
            return;
        }
        let a = (from - rec.dropped) as usize;
        let vs = cfg.vscale[ch];
        let (zero, vpc) = self.uconv(ch, vs, rec.zero[vs.index()][ch]);
        let level = match self.sw_level {
            SwLevel::Trigger => self.trig_v,
            SwLevel::Custom(v) => v,
            SwLevel::Auto => {
                let (lo, hi) = d[a..]
                    .iter()
                    .fold((u16::MAX, 0u16), |(x, y), &v| (x.min(v), y.max(v)));
                ((lo as f32 + hi as f32) / 2.0 - zero) * vpc
            }
        };
        let last_start = self.stream_hits.last().copied();
        let mut new_hit = None;
        for hit in sw_matches(&self.sw, &d[a..], zero, vpc, dt, 0.0, level, 10_000) {
            let abs = from + hit.start as u64;
            if last_start.is_none_or(|l| abs > l)
                && self.stream_hits.last().is_none_or(|&l| abs > l)
            {
                self.stream_hits.push(abs);
                if abs >= self.stream_arm_from {
                    new_hit.get_or_insert(from + hit.at as u64);
                }
            }
        }
        self.stream_scan = total;
        let screen = self.stream_rows_per_screen() as u64;
        drop(h);
        if let (Some(at), true, Run::Running) = (new_hit, self.sw_stop, self.run) {
            log::info!(
                "stop on match (stream): row {at} matched ({})",
                sw_describe(&self.sw, self.unit(self.trig_src as usize))
            );
            let what = sw_describe(&self.sw, self.unit(self.trig_src as usize));
            self.files_msg(
                Level::Warn,
                format!(
                    "Stopped: the stream passed the match filter ({what}). Run (Space) continues.",
                ),
            );
            self.set_run(Run::Stopped);
            self.stream_end = Some(at + screen / 4); // event visible, three quarters of the screen before it
        }
    }
}

// ───────────────────────────── keyboard ─────────────────────────────

/// Every shortcut, for the help overlay (key, what it does).
const SHORTCUTS: [(&str, &str); 40] = [
    ("Space", "Run / Stop"),
    ("S", "Single capture"),
    (", / .", "Timebase faster / slower"),
    ("1 … 8", "Select channel"),
    ("Shift + 1 … 8", "Show / hide channel"),
    ("↑ / ↓", "V/div of the selected channel bigger / smaller"),
    (
        "← / →",
        "History: previous / next capture (stream: a tenth of a screen)",
    ),
    (
        "PgUp / PgDn",
        "History: 10 captures back / forward (stream: one screen)",
    ),
    ("Home / End", "History: first / last"),
    ("[ / ]", "Previous / next match (match filter)"),
    (
        "Shift + ↑ / ↓",
        "Move the selected channel up / down (¼ division)",
    ),
    ("Ctrl + ↑ / ↓", "Trigger level up / down (1/10 division)"),
    ("Ctrl + ← / →", "Trigger position left / right (5 %)"),
    ("+ / −", "Zoom time in / zoom out (both axes)"),
    (
        "Right-click / two-finger tap",
        "Plot menu: zoom, trigger level / position here, references R1/R2, screenshot, auto-set",
    ),
    ("0", "Full view (reset zoom)"),
    ("Shift + ← / →", "Pan the zoomed view"),
    ("Scroll", "Zoom time around the pointer"),
    ("Shift + scroll", "Zoom vertically"),
    ("Pinch / Ctrl + wheel", "Zoom"),
    ("Two-finger sideways", "Pan time"),
    (
        "Drag on the graph",
        "Box zoom (or draw the match-filter zone)",
    ),
    ("Double-click", "Full view"),
    ("Hold a control", "Explain it (also on hover)"),
    ("F1 / H", "Show / hide this list (Esc closes)"),
    ("C", "Cursors on / off"),
    ("Tab", "Next cursor (T1, T2, V1, V2) while cursors are on; otherwise moves between fields"),
    ("Alt + arrows", "Move the active cursor"),
    (
        "P",
        "Persistence on / off (earlier captures faded under the live trace)",
    ),
    (
        "F2",
        "Settings window (probes, display, trigger, engine, memory, recording, folders)",
    ),
    (
        "A",
        "Auto-set: timebase, V/div, offsets and trigger for the shown channels",
    ),
    ("F3", "Show / hide the measurement table (hidden: one line, more room for the waveform)"),
    ("F4", "Collapse / expand the side panel (a narrow column of short buttons: more room for the waveform)"),
    ("F12", "Save a screenshot (PNG) into the recording folder"),
    ("M", "Meter mode: big numbers per channel (average, min/max, RMS, frequency, trend) / back to the scope"),
    ("R / Shift+R", "Save the selected channel of the capture on screen as reference R1 / R2 (also a library file)"),
    ("Ctrl+R", "Show / hide the references"),
    ("Ctrl+E", "Export what is shown (format: Menu › Export)"),
    ("Ctrl+O", "Settings: open a recording (file / folder)"),
    (
        "Backspace",
        "Undo the last auto-set (right after one), otherwise undo the last zoom",
    ),
];

impl App {
    /// Global shortcuts (not while typing in a field).
    fn global_keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let digits = [
            egui::Key::Num1,
            egui::Key::Num2,
            egui::Key::Num3,
            egui::Key::Num4,
            egui::Key::Num5,
            egui::Key::Num6,
            egui::Key::Num7,
            egui::Key::Num8,
        ];
        let (space, single, faster, slower, up, down, help_key, esc, shift, num) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::S),
                i.key_pressed(egui::Key::Comma),
                i.key_pressed(egui::Key::Period),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::F1) || i.key_pressed(egui::Key::H),
                i.key_pressed(egui::Key::Escape),
                i.modifiers.shift,
                digits.iter().position(|k| i.key_pressed(*k)),
            )
        });
        if space {
            self.set_run(if self.run == Run::Running {
                Run::Stopped
            } else {
                Run::Running
            });
        }
        if single {
            self.set_run(Run::Single);
        }
        if faster {
            self.step_timebase(-1);
        }
        if slower {
            self.step_timebase(1);
        }
        if let Some(ch) = self.shift_digit.take() {
            if !self.on[ch] || self.on.iter().filter(|&&a| a).count() > 1 {
                self.on[ch] = !self.on[ch];
            }
        } else if let Some(ch) = num.filter(|_| !shift) {
            self.sel_ch = ch;
        }
        let c = self.sel_ch;
        let (alt, ctrl, left, right) = ctx.input(|i| {
            (
                i.modifiers.alt,
                i.modifiers.command,
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
            )
        });
        let plain = !alt && !shift && !ctrl; // Alt + arrows move a cursor, Shift + ←/→ pan the zoom
        if up && plain && self.vdiv[c] + 1 < VDIVS.len() {
            self.vdiv[c] += 1;
        }
        if down && plain && self.vdiv[c] > 0 {
            self.vdiv[c] -= 1;
        }
        // Shift + ↑/↓: move the selected channel by a quarter division
        if shift && !alt && !ctrl && (up || down) {
            let d = if up { 0.25 } else { -0.25 };
            self.offset[c] = (self.offset[c] + d).clamp(-DIV_Y / 2.0, DIV_Y / 2.0);
        }
        // Ctrl + ↑/↓: trigger level by a tenth of a division; Ctrl + ←/→: trigger position by 5 %
        if ctrl && !alt {
            let step = self.udiv(self.trig_src as usize) / 10.0;
            if up {
                self.trig_v += step;
            }
            if down {
                self.trig_v -= step;
            }
            if left {
                self.trig_pos = self.trig_pos.saturating_sub(50);
            }
            if right {
                self.trig_pos = (self.trig_pos + 50).min(1000);
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F4)) {
            self.panel_min = !self.panel_min;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F3)) {
            self.meas_on = !self.meas_on;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F2)) {
            self.settings_open = !self.settings_open;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::A)) {
            self.autoset_start();
        }
        if self.autoset.is_none()
            && self
                .autoset_msg
                .as_ref()
                .is_some_and(|m| m.1.elapsed().as_secs() > 15)
        {
            self.autoset_undo = None; // undo only right after an auto-set
        }
        if self.autoset_undo.is_some()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Backspace))
        {
            self.autoset_undo(); // Backspace then no longer reaches the zoom undo below
        }
        if ctx.input(|i| i.key_pressed(egui::Key::P)) {
            self.persist_on = !self.persist_on;
        }
        let (r_key, shift_r, ctrl) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::R),
                i.modifiers.shift,
                i.modifiers.command,
            )
        });
        if r_key && ctrl {
            self.refs_on = !self.refs_on;
        } else if r_key {
            self.ref_req = Some(shift_r as usize);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::O) && i.modifiers.command) {
            self.settings_open = true; // Settings > Recording, event log, open, export: open a recording
        }
        if ctx.input(|i| i.key_pressed(egui::Key::E) && i.modifiers.command) {
            self.files.src = files::ExportSrc::Shown; // Ctrl+E is 'Shown' (the whole stream is in Menu › Export)
            self.files.export_req = true;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::M) && !i.modifiers.command) {
            self.meter.on = !self.meter.on;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F12)) {
            self.files.shot_req = true;
        }
        if help_key {
            self.show_keys = !self.show_keys;
        }
        if esc {
            self.show_keys = false;
            self.settings_open = false;
            self.guide = None;
            self.test_show = false;
            for m in self.math.iter_mut().filter(|m| m.op == MathOp::Fft) {
                m.op = MathOp::Off; // same as closing the FFT window
            }
        }
    }

    /// One timebase step faster (-1) or slower (+1), skipping timebases with wrong data.
    fn step_timebase(&mut self, dir: i32) {
        if let Some(tb) = next_timebase(self.tb_id, dir) {
            self.tb_id = tb;
        }
    }

    /// Shortcut overlay (F1 / H).
    fn keys_overlay(&mut self, ctx: &egui::Context) {
        let mut open = self.show_keys;
        egui::Window::new("Keyboard & touchpad")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(ctx.content_rect().height() * 0.75)
                    .show(ui, |ui| {
                        egui::Grid::new("keys")
                            .striped(true)
                            .spacing([18.0, 4.0])
                            .show(ui, |ui| {
                                for (k, what) in SHORTCUTS {
                                    ui.label(egui::RichText::new(k).monospace().strong());
                                    ui.label(what);
                                    ui.end_row();
                                }
                            });
                    });
            });
        self.show_keys = open && self.show_keys;
    }
}

/// The next timebase id in direction `dir` (±1), skipping TB_SKIP; None at either end.
fn next_timebase(tb: u8, dir: i32) -> Option<u8> {
    let mut t = tb as i32;
    loop {
        t += dir.signum();
        if t < 0 || t > TB_MAX as i32 {
            return None;
        }
        if !TB_SKIP.contains(&(t as u8)) {
            return Some(t as u8);
        }
    }
}

#[cfg(test)]
mod timebase_tests {
    use super::*;

    #[test]
    fn next_timebase_skips_broken_and_stops_at_ends() {
        assert_eq!(next_timebase(15, 1), Some(17)); // 100 µs → 500 µs (200 µs skipped)
        assert_eq!(next_timebase(17, -1), Some(15));
        assert_eq!(next_timebase(22, 1), Some(24)); // 20 ms → 100 ms (50 ms skipped)
        assert_eq!(next_timebase(0, -1), None);
        assert_eq!(next_timebase(TB_MAX, 1), None);
    }
}

// ───────────────────────────── cursors ─────────────────────────────

/// Cursor positions: time cursors in seconds relative to the time reference (trigger point; stream: newest
/// sample), voltage cursors in volts on the selected channel.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Cursors {
    on: bool,
    t: [f64; 2],
    v: [f32; 2],
    /// active cursor for keyboard moves: 0 = T1, 1 = T2, 2 = V1, 3 = V2
    active: usize,
}

impl Default for Cursors {
    fn default() -> Self {
        Cursors {
            on: false,
            t: [0.0, 0.0],
            v: [0.0, 0.0],
            active: 0,
        }
    }
}

const CURSOR_COLOR: Color32 = Color32::from_rgb(235, 235, 235);

impl App {
    /// Reference fraction of the screen for time cursors: trigger position (captures) or right edge (stream).
    fn cursor_ref(&self, roll: bool) -> f32 {
        if roll {
            1.0
        } else {
            self.trig_pos as f32 / 1000.0
        }
    }

    /// Put the cursors somewhere sensible when they are first switched on.
    fn cursors_init(&mut self) {
        let span = self.span_s();
        let r = self.cursor_ref(self.roll.is_some()) as f64;
        // T1/T2 at 30 % and 70 % of the visible window, V1/V2 at ±1 div of the selected channel
        let z = self.zoom;
        let f = |x: f32| ((z.x0 + x * (z.x1 - z.x0)) as f64 - r) * span;
        self.cursors.t = [f(0.3), f(0.7)];
        let vd = self.udiv(self.sel_ch);
        self.cursors.v = [vd, -vd];
    }

    /// Draw + drag the cursors in one pane. `chan_ab[ch]` = sample→x mapping per drawn channel (for values).
    #[allow(clippy::too_many_arguments)]
    fn cursors_pane(
        &mut self,
        ui: &egui::Ui,
        resp: &egui::Response,
        painter: &egui::Painter,
        r: Rect,
        vr: Rect,
        pane: usize,
        snap: &Snapshot,
    ) {
        if !self.cursors.on {
            return;
        }
        let span = self.span_s() as f32;
        let rf = self.cursor_ref(snap.roll);
        let x_of = |t: f64| vr.left() + (rf + t as f32 / span) * vr.width();
        let t_of = |x: f32| (((x - vr.left()) / vr.width() - rf) * span) as f64;
        let c = self.sel_ch;
        let dyv = vr.height() / DIV_Y;
        let y0 = vr.center().y - self.offset[c] * dyv;
        let ud = self.udiv(c);
        let y_of = |v: f32| y0 - v / ud * dyv;
        let v_of = |y: f32| (y0 - y) / dyv * ud;
        let v_pane = self.pane_for(c) == pane;

        // grab (after trigger markers and channel handles, before box zoom)
        if resp.drag_started() && self.drag.is_none() {
            if let Some(p) = ui
                .input(|i| i.pointer.press_origin())
                .or(resp.interact_pointer_pos())
            {
                for i in 0..2 {
                    if self.drag.is_some() {
                        break; // first hit wins (a V hit of cursor 2 must not override a T hit of cursor 1)
                    }
                    if (p.x - x_of(self.cursors.t[i])).abs() < 8.0 {
                        self.drag = Some(Drag::TCur(i));
                    } else if v_pane && (p.y - y_of(self.cursors.v[i])).abs() < 8.0 {
                        self.drag = Some(Drag::VCur(i));
                    }
                }
            }
        }
        if resp.dragged() {
            if let Some(p) = resp.interact_pointer_pos() {
                match self.drag {
                    Some(Drag::TCur(i)) => {
                        self.cursors.t[i] = t_of(p.x.clamp(r.left(), r.right()));
                        self.cursors.active = i;
                    }
                    Some(Drag::VCur(i)) if v_pane => {
                        self.cursors.v[i] = v_of(p.y.clamp(r.top(), r.bottom()));
                        self.cursors.active = 2 + i;
                    }
                    _ => {}
                }
            }
        }
        if let Some(p) = resp.hover_pos() {
            if (0..2).any(|i| (p.x - x_of(self.cursors.t[i])).abs() < 8.0) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            } else if v_pane && (0..2).any(|i| (p.y - y_of(self.cursors.v[i])).abs() < 8.0) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
            }
        }

        // draw
        for i in 0..2 {
            let active = self.cursors.active == i;
            let x = x_of(self.cursors.t[i]);
            if (r.left()..=r.right()).contains(&x) {
                let st = Stroke::new(
                    if active { 1.6 } else { 1.0 },
                    CURSOR_COLOR.gamma_multiply(if active { 0.95 } else { 0.6 }),
                );
                painter.extend(Shape::dashed_line(
                    &[Pos2::new(x, r.top()), Pos2::new(x, r.bottom())],
                    st,
                    3.0,
                    3.0,
                ));
                if pane == 0 {
                    painter.text(
                        Pos2::new(x + 3.0, r.bottom() - 4.0),
                        Align2::LEFT_BOTTOM,
                        format!("T{}", i + 1),
                        FontId::monospace(11.0),
                        CURSOR_COLOR,
                    );
                }
            }
            if v_pane {
                let active = self.cursors.active == 2 + i;
                let y = y_of(self.cursors.v[i]);
                if (r.top()..=r.bottom()).contains(&y) {
                    let st = Stroke::new(
                        if active { 1.6 } else { 1.0 },
                        COLORS[c].gamma_multiply(if active { 0.95 } else { 0.6 }),
                    );
                    painter.extend(Shape::dashed_line(
                        &[Pos2::new(r.left(), y), Pos2::new(r.right(), y)],
                        st,
                        3.0,
                        3.0,
                    ));
                    painter.text(
                        Pos2::new(r.left() + 46.0, y - 2.0),
                        Align2::LEFT_BOTTOM,
                        format!("V{}", i + 1),
                        FontId::monospace(11.0),
                        COLORS[c],
                    );
                }
            }
        }
    }

    /// Cursor readout (top-left of the first pane): Δt, 1/Δt, rpm, ΔV, selected channel's value at T1/T2.
    fn cursor_readout(
        &self,
        painter: &egui::Painter,
        r: Rect,
        snap: &Snapshot,
        sel_ab: Option<(f32, f32)>,
        vr: Rect,
    ) {
        if !self.cursors.on {
            return;
        }
        let k = &self.cursors;
        let dt = (k.t[1] - k.t[0]).abs();
        let mut lines = vec![format!(
            "Δt {}   1/Δt {}",
            fmt_time_short(dt),
            if dt > 0.0 {
                fmt_freq(1.0 / dt)
            } else {
                "--".into()
            }
        )];
        if dt > 0.0 {
            lines.push(format!("rpm {:.0}  (one revolution = Δt)", 60.0 / dt));
        }
        let c = self.sel_ch;
        let u = self.unit(c).to_string();
        lines.push(format!(
            "Δ{u} {}   1: {}  2: {}   (CH{})",
            fmt_u((k.v[0] - k.v[1]).abs(), &u),
            fmt_u(k.v[0], &u),
            fmt_u(k.v[1], &u),
            c + 1
        ));
        // value of the selected channel at T1 / T2
        if let (Some((a, b)), Some(cfg)) = (sel_ab, snap.cfg.as_ref()) {
            let d = &snap.frame.data[c];
            let span = self.span_s() as f32;
            let rf = self.cursor_ref(snap.roll);
            let vs = cfg.vscale[c];
            let at = |t: f64| {
                let x = vr.left() + (rf + t as f32 / span) * vr.width();
                let i = ((x - a) / b).round();
                (i >= 0.0 && (i as usize) < d.len()).then(|| {
                    let (zu, vu) = self.uconv(c, vs, snap.zero[vs.index()][c]);
                    (d[i as usize] as f32 - zu) * vu
                })
            };
            let f = |v: Option<f32>| v.map_or("--".into(), |v| fmt_u(v, self.unit(c)));
            lines.push(format!(
                "CH{} at T1 {}   at T2 {}",
                c + 1,
                f(at(k.t[0])),
                f(at(k.t[1]))
            ));
        }
        let text = lines.join("\n");
        let pos = Pos2::new(r.left() + 26.0, r.top() + 12.0);
        let galley = painter.layout_no_wrap(text, FontId::monospace(12.0), CURSOR_COLOR);
        painter.rect_filled(
            Rect::from_min_size(pos - Vec2::splat(4.0), galley.size() + Vec2::splat(8.0)),
            3.0,
            Color32::from_black_alpha(170),
        );
        painter.galley(pos, galley, CURSOR_COLOR);
    }

    /// Keyboard: C toggles cursors, Tab cycles the active one, Alt + arrows move it.
    fn cursor_keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (toggle, tab, alt, l, rgt, up, dn) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::C),
                std::mem::take(&mut self.tab_pressed),
                i.modifiers.alt,
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
            )
        });
        if toggle {
            self.cursors.on = !self.cursors.on;
            if self.cursors.on && self.cursors.t == [0.0, 0.0] {
                self.cursors_init();
            }
        }
        if !self.cursors.on {
            return;
        }
        if tab {
            self.cursors.active = (self.cursors.active + 1) % 4;
        }
        if alt {
            let z = self.zoom;
            let step_t = self.span_s() * (z.x1 - z.x0) as f64 / 50.0;
            let step_v = self.udiv(self.sel_ch) / 10.0;
            let a = self.cursors.active;
            if a < 2 {
                if l {
                    self.cursors.t[a] -= step_t;
                }
                if rgt {
                    self.cursors.t[a] += step_t;
                }
            } else {
                if up {
                    self.cursors.v[a - 2] += step_v;
                }
                if dn {
                    self.cursors.v[a - 2] -= step_v;
                }
            }
        }
    }
}

// ───────────────────────────── probes & units ─────────────────────────────

/// What's connected to a channel: display value = scope volts × scale + offset, in `unit`.
#[derive(Clone, Debug, PartialEq)]
struct Probe {
    /// index into PROBE_PRESETS, or PROBE_PRESETS.len() = custom
    preset: usize,
    scale: f32,
    offset: f32,
    unit: String,
}

impl Default for Probe {
    fn default() -> Self {
        Probe {
            preset: 0,
            scale: 1.0,
            offset: 0.0,
            unit: "V".into(),
        }
    }
}

/// (name, scale, offset, unit)
const PROBE_PRESETS: [(&str, f32, f32, &str); 6] = [
    ("×1", 1.0, 0.0, "V"),
    ("×10", 10.0, 0.0, "V"),
    ("×20 (1008C attenuator)", 20.0, 0.0, "V"),
    ("Current clamp 10 mV/A", 100.0, 0.0, "A"),
    ("Current clamp 100 mV/A", 10.0, 0.0, "A"),
    ("Pressure 0.5–4.5 V = 0–10 bar", 2.5, -1.25, "bar"),
];

impl Probe {
    fn from_preset(i: usize) -> Probe {
        let (_, scale, offset, unit) = PROBE_PRESETS[i.min(PROBE_PRESETS.len() - 1)];
        Probe {
            preset: i,
            scale,
            offset,
            unit: unit.into(),
        }
    }
    fn encode(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.preset,
            self.scale,
            self.offset,
            self.unit.replace([',', ':', '\n'], "")
        )
    }
    fn decode(s: &str) -> Option<Probe> {
        let p: Vec<&str> = s.split(':').collect();
        let scale = p
            .get(1)?
            .parse::<f32>()
            .ok()
            .filter(|x| x.is_finite() && *x != 0.0)?;
        Some(Probe {
            preset: p.first()?.parse().ok()?,
            scale,
            offset: p.get(2)?.parse().ok().filter(|x: &f32| x.is_finite())?,
            unit: p.get(3).map_or("V".into(), |u| u.chars().take(8).collect()),
        })
    }
}

/// Format a value in a channel's unit with an SI prefix (V keeps the familiar mV / V style).
/// A scale step (V/div etc.) without trailing zeros: "500 mV", "5 V", "2.5 bar".
fn fmt_div(v: f32, unit: &str) -> String {
    let t = fmt_u(v, unit);
    match t.split_once(' ') {
        Some((n, u)) if n.contains('.') => {
            format!("{} {u}", n.trim_end_matches('0').trim_end_matches('.'))
        }
        _ => t,
    }
}

fn fmt_u(v: f32, unit: &str) -> String {
    if unit == "V" {
        return fmt_v(v);
    }
    let a = v.abs();
    if a >= 1000.0 {
        format!("{:.2} k{unit}", v / 1000.0)
    } else if a >= 1.0 || a == 0.0 {
        format!("{v:.3} {unit}")
    } else {
        format!("{:.1} m{unit}", v * 1000.0)
    }
}

impl App {
    /// Units per division of a channel (V/div setting × probe scale).
    fn udiv(&self, ch: usize) -> f32 {
        VDIVS[self.vdiv[ch]] * self.probes[ch].scale
    }

    /// Counts → display units for a channel on range `vs`: units = (counts − zero_u) × vpc_u.
    fn uconv(&self, ch: usize, vs: VScale, zero: f32) -> (f32, f32) {
        let p = &self.probes[ch];
        let vpc = self.cal.vpc(ch, vs) * p.scale;
        (zero - p.offset / vpc, vpc)
    }

    fn unit(&self, ch: usize) -> &str {
        &self.probes[ch].unit
    }

    /// Probe editor row for channel `c` (Settings window); custom probes show scale, offset and unit inline.
    fn probe_controls(&mut self, ui: &mut egui::Ui, c: usize) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("CH{}", c + 1)).color(COLORS[c]).strong());
            let mut preset = self.probes[c].preset;
            let name = PROBE_PRESETS.get(preset).map_or("Custom", |p| p.0);
            let cb = egui::ComboBox::from_id_salt(("probe", c)).width(170.0).selected_text(name).show_ui(ui, |ui| {
                for (i, p) in PROBE_PRESETS.iter().enumerate() {
                    ui.selectable_value(&mut preset, i, p.0);
                }
                ui.selectable_value(&mut preset, PROBE_PRESETS.len(), "Custom");
            });
            help(cb.response, "What is connected to this channel. Readouts, V/div, measurements, cursors, math and the trigger level then use its unit.");
            if preset != self.probes[c].preset {
                self.probes[c] = if preset < PROBE_PRESETS.len() { Probe::from_preset(preset) } else { Probe { preset, ..self.probes[c].clone() } };
            }
            if self.probes[c].preset >= PROBE_PRESETS.len() {
                let p = &mut self.probes[c];
                let s = ui.add(egui::DragValue::new(&mut p.scale).speed(0.01).prefix("× ").max_decimals(4));
                help(s, "Units per volt at the scope input (e.g. 100 for a 10 mV/A clamp).");
                let o = ui.add(egui::DragValue::new(&mut p.offset).speed(0.01).prefix("+ ").max_decimals(4));
                help(o, "Offset in units (value when the scope input is at 0 V).");
                let u = ui.add(egui::TextEdit::singleline(&mut p.unit).desired_width(40.0));
                help(u, "Unit name, e.g. A, bar, kPa, psi, °C.");
                if p.scale == 0.0 {
                    p.scale = 1.0;
                }
            }
        });
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    #[test]
    fn probe_roundtrip_and_format() {
        let p = Probe {
            preset: 6,
            scale: 2.5,
            offset: -1.25,
            unit: "bar".into(),
        };
        assert_eq!(Probe::decode(&p.encode()), Some(p));
        assert_eq!(Probe::decode("0:0:0:V"), None); // zero scale refused
        assert_eq!(fmt_u(12.5, "A"), "12.500 A");
        assert_eq!(fmt_u(0.25, "bar"), "250.0 mbar");
    }
}

impl App {
    /// Screen points of one channel of a snapshot (decimated, cropped to `r`), and its sample → x mapping
    /// (x = a + b·i in the zoomed virtual rect `vr`). Burst: real sample spacing, trigger point at the
    /// trigger-position fraction, deskewed. Stream: newest sample at the right edge. Values in display units.
    #[allow(clippy::too_many_arguments)]
    fn trace(
        &self,
        s: &Snapshot,
        ch: usize,
        r: Rect,
        vr: Rect,
        dyv: f32,
        out: &mut Vec<Pos2>,
    ) -> Option<(f32, f32)> {
        out.clear();
        let (a, b) = self.x_map(s, ch, vr)?;
        self.trace_at(s, ch, r, vr, dyv, out, (a, b))
    }

    /// Like `trace`, with a given sample → x mapping (references use the current screen's time scale).
    #[allow(clippy::too_many_arguments)]
    fn trace_at(
        &self,
        s: &Snapshot,
        ch: usize,
        r: Rect,
        vr: Rect,
        dyv: f32,
        out: &mut Vec<Pos2>,
        (a, b): (f32, f32),
    ) -> Option<(f32, f32)> {
        out.clear();
        let cfg = s.cfg.as_ref()?;
        let d = &s.frame.data[ch];
        if d.len() < 2 || !s.frame.active[ch] {
            return None;
        }
        let vs = cfg.vscale[ch];
        let (zero, vpc_u) = self.uconv(ch, vs, s.zero[vs.index()][ch]);
        let k = vpc_u / self.udiv(ch) * dyv;
        let y0 = vr.center().y - self.offset[ch] * dyv;
        let y = |v: u16| (y0 - (v as f32 - zero) * k).clamp(r.top(), r.bottom());
        let n = d.len();
        // only the samples inside the real rect (+1 either side so lines reach the edges)
        let i0 = (((r.left() - a) / b).floor().max(0.0) as usize).min(n - 1);
        let i1 = ((((r.right() - a) / b).ceil() + 1.0).max(0.0) as usize).min(n);
        if i1 > i0 + 1 {
            let sub =
                Rect::from_x_y_ranges(a + b * i0 as f32..=a + b * (i1 - 1) as f32, r.y_range());
            decimate(&d[i0..i1], sub, y, out);
        }
        Some((a, b))
    }

    /// x of sample i of channel `ch` = a + b·i in the zoomed virtual rect `vr`. Burst: real sample spacing,
    /// trigger point at the trigger-position fraction (jitter-corrected), deskewed. Stream: newest at the right.
    fn x_map(&self, s: &Snapshot, ch: usize, vr: Rect) -> Option<(f32, f32)> {
        let cfg = s.cfg.as_ref()?;
        let n = s.frame.data[ch].len();
        if n < 2 || !s.frame.active[ch] {
            return None;
        }
        Some(if s.roll {
            let dt = 1.0 / self.shown_rate.max(1e-3);
            let b = (vr.width() as f64 * dt / self.span_s()) as f32;
            (vr.right() - b * (n - 1) as f32, b)
        } else {
            let dt = capture_seconds(cfg.timebase_id) / n as f64;
            let span = burst_ns_per_div(cfg.timebase_id) as f64 * 1e-9 * DIV_X as f64;
            let pos = cfg.trig_pos as f64 / 1000.0;
            let b = (vr.width() as f64 * dt / span) as f32;
            let scan_idx = s.frame.active[..ch].iter().filter(|&&a| a).count() as f64;
            let skew_px = (vr.width() as f64 * scan_idx * CHANNEL_SKEW_S / span) as f32;
            // trigger jitter correction: the real crossing lands on the trigger marker
            let corr = if self.trig_corr && !self.dropped(DROP_JITTER) {
                s.trig_off.unwrap_or(0.0)
            } else {
                0.0
            };
            (
                vr.left() + vr.width() * pos as f32 - b * ((pos * n as f64) as f32 + corr)
                    + skew_px,
                b,
            )
        })
    }
}

// ───────────────────────────── math channels ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
enum MathOp {
    Off,
    Sub,
    Add,
    Scale,
    LowPass,
    Rpm,
    Fft,
    /// A minus its average: ripple / AC part (the 1008C has no AC coupling)
    Ac,
    /// A × B: power when one is a voltage and the other a current
    Mul,
    /// |A|
    Abs,
}

// new operations go at the end: settings store the index
const MATH_OPS: [(MathOp, &str); 10] = [
    (MathOp::Off, "Off"),
    (MathOp::Sub, "A − B"),
    (MathOp::Add, "A + B"),
    (MathOp::Scale, "A × k"),
    (MathOp::LowPass, "Low-pass A"),
    (MathOp::Rpm, "RPM from A"),
    (MathOp::Fft, "FFT of A"),
    (MathOp::Ac, "AC part of A (ripple)"),
    (MathOp::Mul, "A × B (power)"),
    (MathOp::Abs, "|A|"),
];

const MATH_COLORS: [Color32; 2] = [
    Color32::from_rgb(190, 200, 255),
    Color32::from_rgb(255, 175, 225),
];

/// A math channel: `k` = factor (A × k), cutoff in Hz (low-pass) or pulses per revolution (RPM).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Math {
    op: MathOp,
    a: usize,
    b: usize,
    k: f32,
    /// own display, like a channel: units/div (index into `math_div`), offset in div, plot (pane)
    vdiv: usize,
    offset: f32,
    pane: usize,
}

/// Units per division of a math channel: 1-2-5 steps from 1 m… to 50 k… (index 0 … MATH_DIV_N-1).
const MATH_DIV_N: usize = 24;
fn math_div(i: usize) -> f32 {
    [1.0, 2.0, 5.0][i % 3] * 10f32.powi(i as i32 / 3 - 3)
}
/// Smallest step ≥ `v` (for a default scale).
fn math_div_for(v: f32) -> usize {
    (0..MATH_DIV_N)
        .find(|&i| math_div(i) >= v * 0.999)
        .unwrap_or(MATH_DIV_N - 1)
}

impl Default for Math {
    fn default() -> Self {
        Math {
            op: MathOp::Off,
            a: 0,
            b: 1,
            k: 1.0,
            vdiv: 9,
            offset: 0.0,
            pane: 0,
        }
    }
}

impl Math {
    fn encode(&self) -> String {
        let i = MATH_OPS
            .iter()
            .position(|(o, _)| *o == self.op)
            .unwrap_or(0);
        format!(
            "{i}:{}:{}:{}:{}:{}:{}",
            self.a, self.b, self.k, self.vdiv, self.offset, self.pane
        )
    }
    fn decode(s: &str) -> Option<Math> {
        let p: Vec<&str> = s.split(':').collect();
        Some(Math {
            op: MATH_OPS.get(p.first()?.parse::<usize>().ok()?)?.0,
            a: p.get(1)?.parse::<usize>().ok()?.min(CHANNELS - 1),
            b: p.get(2)?.parse::<usize>().ok()?.min(CHANNELS - 1),
            k: p.get(3)?.parse::<f32>().ok().filter(|k| k.is_finite())?,
            // display fields came later: older settings files don't have them
            vdiv: p
                .get(4)
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(9)
                .min(MATH_DIV_N - 1),
            offset: p
                .get(5)
                .and_then(|v| v.parse::<f32>().ok())
                .filter(|v| v.is_finite())
                .unwrap_or(0.0)
                .clamp(-DIV_Y / 2.0, DIV_Y / 2.0),
            pane: p
                .get(6)
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0)
                .min(MAX_PANES - 1),
        })
    }
    fn label(&self) -> String {
        match self.op {
            MathOp::Off => "off".into(),
            MathOp::Sub => format!("CH{} − CH{}", self.a + 1, self.b + 1),
            MathOp::Add => format!("CH{} + CH{}", self.a + 1, self.b + 1),
            MathOp::Scale => format!("CH{} × {}", self.a + 1, self.k),
            MathOp::Ac => format!("CH{} AC part", self.a + 1),
            MathOp::Mul => format!("CH{} × CH{}", self.a + 1, self.b + 1),
            MathOp::Abs => format!("|CH{}|", self.a + 1),
            MathOp::LowPass => format!("CH{} low-pass {}", self.a + 1, fmt_freq(self.k as f64)),
            MathOp::Rpm => format!("rpm from CH{} ({} pulses/rev)", self.a + 1, self.k),
            MathOp::Fft => format!("FFT of CH{}", self.a + 1),
        }
    }
}

/// Min/max decimation for float series (NaN samples skipped), like `decimate` for raw counts.
fn decimate_f32(d: &[f32], r: Rect, y: impl Fn(f32) -> f32, out: &mut Vec<Pos2>) {
    out.clear();
    let n = d.len();
    if n < 2 {
        return;
    }
    let w = r.width().max(1.0) as usize;
    if n <= 2 * w {
        let sx = r.width() / (n - 1) as f32;
        out.extend(
            d.iter()
                .enumerate()
                .filter(|(_, v)| !v.is_nan())
                .map(|(i, &v)| Pos2::new(r.left() + i as f32 * sx, y(v))),
        );
        return;
    }
    for px in 0..w {
        let (a, b) = (px * n / w, ((px + 1) * n / w).max(px * n / w + 1));
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for &v in &d[a..b] {
            if !v.is_nan() {
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
        if lo <= hi {
            let x = r.left() + px as f32;
            out.push(Pos2::new(x, y(lo)));
            out.push(Pos2::new(x, y(hi)));
        }
    }
}

impl App {
    /// Samples of a channel in its display unit.
    fn units_of(&self, s: &Snapshot, ch: usize) -> Option<Vec<f32>> {
        let cfg = s.cfg.as_ref()?;
        let d = &s.frame.data[ch];
        if d.len() < 2 || !s.frame.active[ch] {
            return None;
        }
        let vs = cfg.vscale[ch];
        let (zu, vu) = self.uconv(ch, vs, s.zero[vs.index()][ch]);
        Some(d.iter().map(|&c| (c as f32 - zu) * vu).collect())
    }

    /// Sample spacing (s) of a snapshot with `n` samples per channel.
    fn sample_dt(&self, s: &Snapshot, n: usize) -> f64 {
        if s.roll {
            1.0 / self.shown_rate.max(1e-9)
        } else {
            s.cfg
                .as_ref()
                .map_or(1.0, |c| capture_seconds(c.timebase_id) / n.max(1) as f64)
        }
    }

    /// Time-domain result of a math channel (same length/time base as channel A), and its unit.
    fn math_series(&self, m: &Math, s: &Snapshot) -> Option<(Vec<f32>, String)> {
        let a = self.units_of(s, m.a)?;
        let dt = self.sample_dt(s, a.len());
        let ua = self.unit(m.a).to_string();
        match m.op {
            MathOp::Off | MathOp::Fft => None,
            MathOp::Sub | MathOp::Add => {
                let b = self.units_of(s, m.b)?;
                let sign = if m.op == MathOp::Sub { -1.0 } else { 1.0 };
                Some((a.iter().zip(&b).map(|(x, y)| x + sign * y).collect(), ua))
            }
            MathOp::Scale => Some((a.iter().map(|x| x * m.k).collect(), ua)),
            MathOp::Ac => {
                let mean = a.iter().sum::<f32>() / a.len().max(1) as f32;
                Some((a.iter().map(|x| x - mean).collect(), ua))
            }
            MathOp::Abs => Some((a.iter().map(|x| x.abs()).collect(), ua)),
            MathOp::Mul => {
                let b = self.units_of(s, m.b)?;
                let ub = self.unit(m.b).to_string();
                // V × A = W; otherwise the product of the units
                let unit = match (ua.as_str(), ub.as_str()) {
                    ("V", "A") | ("A", "V") => "W".to_string(),
                    (x, y) if x == y => format!("{x}²"),
                    (x, y) => format!("{x}·{y}"),
                };
                Some((a.iter().zip(&b).map(|(x, y)| x * y).collect(), unit))
            }
            MathOp::LowPass => Some((crate::dsp::lowpass(&a, dt, m.k as f64), ua)),
            MathOp::Rpm => Some((crate::dsp::rpm_series(&a, dt, m.k as f64), "rpm".into())),
        }
    }

    /// Draw the math channels whose source A is in this pane (A's time mapping; A's scale, RPM auto-scaled).
    #[allow(clippy::too_many_arguments)]
    fn math_pane(
        &mut self,
        ui: &egui::Ui,
        resp: &egui::Response,
        painter: &egui::Painter,
        r: Rect,
        vr: Rect,
        pane: usize,
        snap: &Snapshot,
        press: Option<Pos2>,
        taken: &mut Vec<f32>,
    ) {
        if self.dropped(DROP_MATH) {
            return;
        }
        let dyv = vr.height() / DIV_Y;
        for i in 0..self.math.len() {
            let m = self.math[i];
            if matches!(m.op, MathOp::Off | MathOp::Fft)
                || m.pane.min(self.panes.max(1) - 1) != pane
            {
                continue;
            }
            // handle on the left edge at the math channel's 0 line: drag vertically, like a channel
            let y0 = vr.center().y - m.offset * dyv;
            let hy = y0.clamp(r.top() + 7.0, r.bottom() - 7.0);
            let hx =
                r.left() + 20.0 * taken.iter().filter(|&&t| (t - hy).abs() < 14.0).count() as f32;
            taken.push(hy);
            let on_handle = |p: Pos2| p.x >= hx && p.x < hx + 24.0 && (p.y - hy).abs() < 9.0;
            if resp.drag_started() && self.drag.is_none() && press.is_some_and(on_handle) {
                self.drag = Some(Drag::Math(i));
                if let (Some(o), Some(p)) = (press, resp.interact_pointer_pos()) {
                    self.math[i].offset =
                        (m.offset - (p.y - o.y) / dyv).clamp(-DIV_Y / 2.0, DIV_Y / 2.0);
                }
            }
            let active = self.drag == Some(Drag::Math(i));
            if active && resp.dragged() && !resp.drag_started() {
                self.math[i].offset = (self.math[i].offset - resp.drag_delta().y / dyv)
                    .clamp(-DIV_Y / 2.0, DIV_Y / 2.0);
            }
            if active || resp.hover_pos().is_some_and(on_handle) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
            }
            let col = if active {
                MATH_COLORS[i]
            } else {
                MATH_COLORS[i].gamma_multiply(0.75)
            };
            painter.add(Shape::convex_polygon(
                vec![
                    Pos2::new(hx, hy - 7.0),
                    Pos2::new(hx + 16.0, hy - 7.0),
                    Pos2::new(hx + 22.0, hy),
                    Pos2::new(hx + 16.0, hy + 7.0),
                    Pos2::new(hx, hy + 7.0),
                ],
                col,
                Stroke::NONE,
            ));
            painter.text(
                Pos2::new(hx + 8.0, hy),
                Align2::CENTER_CENTER,
                format!("M{}", i + 1),
                FontId::monospace(10.0),
                Color32::BLACK,
            );
            let m = self.math[i];
            let (Some((a, b)), Some((vals, unit))) =
                (self.x_map(snap, m.a, vr), self.math_series(&m, snap))
            else {
                continue;
            };
            let n = vals.len();
            let finite: Vec<f32> = vals.iter().copied().filter(|v| v.is_finite()).collect();
            if finite.is_empty() || n < 2 {
                continue;
            }
            let (lo, hi) = finite
                .iter()
                .fold((f32::MAX, f32::MIN), |(x, y), &v| (x.min(v), y.max(v)));
            let mean = finite.iter().sum::<f32>() / finite.len() as f32;
            let y0 = vr.center().y - m.offset * dyv;
            let ud = math_div(m.vdiv);
            let y = move |v: f32| (y0 - v / ud * dyv).clamp(r.top(), r.bottom());
            let i0 = (((r.left() - a) / b).floor().max(0.0) as usize).min(n - 1);
            let i1 = ((((r.right() - a) / b).ceil() + 1.0).max(0.0) as usize).min(n);
            if i1 > i0 + 1 {
                let sub =
                    Rect::from_x_y_ranges(a + b * i0 as f32..=a + b * (i1 - 1) as f32, r.y_range());
                let mut pts = std::mem::take(&mut self.pts);
                decimate_f32(&vals[i0..i1], sub, y, &mut pts);
                if pts.len() > 1 {
                    painter.add(Shape::line(pts, Stroke::new(1.4, MATH_COLORS[i])));
                }
            }
            let text = format!(
                "M{} {}: mean {}  [{} … {}]",
                i + 1,
                m.label(),
                fmt_u(mean, &unit),
                fmt_u(lo, &unit),
                fmt_u(hi, &unit)
            );
            painter.text(
                Pos2::new(r.right() - 26.0, r.bottom() - 22.0 - 16.0 * i as f32), // above the scale legend row
                Align2::RIGHT_BOTTOM,
                text,
                FontId::monospace(12.0),
                MATH_COLORS[i],
            );
        }
    }

    /// FFT windows for math channels set to "FFT of A".
    fn fft_windows(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        if self.dropped(DROP_MATH) {
            return;
        }
        for i in 0..self.math.len() {
            let m = self.math[i];
            if m.op != MathOp::Fft {
                continue;
            }
            let mut open = true;
            // the spectrum only for new data (a full stream screen cost 22–98 ms per frame — speed review 2026-11-23)
            let key = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                let p = &self.probes[m.a];
                (
                    snap.frame.seq,
                    snap.roll,
                    self.view,
                    self.stream_end,
                    m.a,
                    snap.frame.data[m.a].len(),
                    p.scale.to_bits(),
                    p.offset.to_bits(),
                )
                    .hash(&mut h);
                h.finish()
            };
            let live_stream = snap.roll && self.run == Run::Running;
            let (k0, at, _, _) = &self.fft_cache[i];
            if *k0 != key && !(live_stream && at.is_some_and(|t| t.elapsed().as_millis() < 100)) {
                let Some(a) = self.units_of(snap, m.a) else {
                    continue;
                };
                let dt = self.sample_dt(snap, a.len());
                let (df, db) = crate::dsp::spectrum_db(&a, dt);
                self.fft_cache[i] = (key, Some(Instant::now()), df, db);
            }
            // borrowed out of the cache for drawing and put back below (no copy of up to 1 MB per frame)
            let (df, db) = (
                self.fft_cache[i].2,
                std::mem::take(&mut self.fft_cache[i].3),
            );
            let unit = self.unit(m.a).to_string();
            egui::Window::new(format!("M{} — FFT of CH{}", i + 1, m.a + 1))
                .open(&mut open)
                .default_size([520.0, 260.0])
                .show(ctx, |ui| {
                    if db.len() < 2 {
                        ui.label("not enough samples");
                        return;
                    }
                    let (resp, painter) = ui.allocate_painter(
                        Vec2::new(ui.available_width().max(300.0), 220.0),
                        Sense::hover(),
                    );
                    let r = resp.rect;
                    painter.rect_filled(r, 0.0, Color32::from_rgb(12, 14, 18));
                    let top = db.iter().skip(1).copied().fold(f32::MIN, f32::max) + 5.0;
                    let floor = top - 90.0;
                    let x_of = |k: usize| r.left() + k as f32 / (db.len() - 1) as f32 * r.width();
                    let y_of = |v: f32| {
                        (r.top() + (top - v) / (top - floor) * r.height())
                            .clamp(r.top(), r.bottom())
                    };
                    for g in 1..9 {
                        let yy = r.top() + g as f32 / 9.0 * r.height();
                        painter.line_segment(
                            [Pos2::new(r.left(), yy), Pos2::new(r.right(), yy)],
                            Stroke::new(1.0, Color32::from_gray(40)),
                        );
                    }
                    // min / max per pixel column: ≤ 2 points per pixel instead of one per bin (up to 500k)
                    let mut pts: Vec<Pos2> = Vec::with_capacity(2 * r.width() as usize + 2);
                    let mut col = i32::MIN;
                    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
                    for (k, &v) in db.iter().enumerate() {
                        let c = x_of(k) as i32;
                        if c != col && col != i32::MIN {
                            pts.push(Pos2::new(col as f32, y_of(hi)));
                            if lo < hi {
                                pts.push(Pos2::new(col as f32, y_of(lo)));
                            }
                            (lo, hi) = (f32::MAX, f32::MIN);
                        }
                        col = c;
                        (lo, hi) = (lo.min(v), hi.max(v));
                    }
                    if col != i32::MIN {
                        pts.push(Pos2::new(col as f32, y_of(hi)));
                    }
                    painter.add(Shape::line(pts, Stroke::new(1.2, MATH_COLORS[i])));
                    let (kp, &vp) = db
                        .iter()
                        .enumerate()
                        .skip(1)
                        .max_by(|x, y| x.1.total_cmp(y.1))
                        .unwrap();
                    painter.text(
                        r.left_top() + Vec2::new(6.0, 4.0),
                        Align2::LEFT_TOP,
                        format!(
                            "peak {} at {:.1} dB({unit})   0 … {}   10 dB/div",
                            fmt_freq(kp as f64 * df),
                            vp,
                            fmt_freq(df * (db.len() - 1) as f64)
                        ),
                        FontId::monospace(12.0),
                        MATH_COLORS[i],
                    );
                });
            self.fft_cache[i].3 = db;
            if !open {
                self.math[i].op = MathOp::Off;
            }
        }
    }

    /// "Math" section of the controls.
    fn math_controls(&mut self, ui: &mut egui::Ui) {
        for i in 0..self.math.len() {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("M{}", i + 1)).color(MATH_COLORS[i]).strong());
                let uda: [f32; CHANNELS] = std::array::from_fn(|c| self.udiv(c));
                let pane_a: [usize; CHANNELS] = std::array::from_fn(|c| self.pane_for(c));
                let m = &mut self.math[i];
                let name = MATH_OPS.iter().find(|(o, _)| *o == m.op).map_or("Off", |x| x.1);
                let cb = egui::ComboBox::from_id_salt(("math_op", i)).width(92.0).selected_text(name).show_ui(ui, |ui| {
                    for (op, n) in MATH_OPS {
                        if ui.selectable_value(&mut m.op, op, n).clicked() {
                            m.k = match op {
                                MathOp::LowPass => 1000.0,
                                MathOp::Rpm => 1.0,
                                _ => 1.0,
                            };
                            // start where A is: its scale (rpm: 500 rpm/div, a bit below centre), its plot
                            (m.vdiv, m.offset) = if op == MathOp::Rpm { (math_div_for(500.0), -3.0) } else { (math_div_for(uda[m.a]), 0.0) };
                            m.pane = pane_a[m.a];
                        }
                    }
                });
                help(cb.response, "Math channel: difference / sum of two channels, a scaled or low-pass-filtered channel, engine speed (rpm) from a crank or cam signal, or a frequency spectrum (FFT, own window).");
                if m.op != MathOp::Off {
                    let ca = egui::ComboBox::from_id_salt(("math_a", i)).width(44.0).selected_text(format!("A CH{}", m.a + 1)).show_ui(ui, |ui| {
                        for ch in 0..CHANNELS {
                            ui.selectable_value(&mut m.a, ch, format!("CH{}", ch + 1));
                        }
                    });
                    help(ca.response, "Source channel A.");
                }
            });
            ui.horizontal(|ui| {
                ui.add_space(26.0);
                let m = &mut self.math[i];
                match m.op {
                    MathOp::Sub | MathOp::Add | MathOp::Mul => {
                        let cb = egui::ComboBox::from_id_salt(("math_b", i)).width(44.0).selected_text(format!("B CH{}", m.b + 1)).show_ui(ui, |ui| {
                            for ch in 0..CHANNELS {
                                ui.selectable_value(&mut m.b, ch, format!("CH{}", ch + 1));
                            }
                        });
                        help(cb.response, "Channel B (shown with A's scale and unit).");
                    }
                    MathOp::Scale => {
                        help(ui.add(egui::DragValue::new(&mut m.k).speed(0.01).prefix("k ")), "Factor.");
                    }
                    MathOp::LowPass => {
                        help(ui.add(egui::DragValue::new(&mut m.k).speed(10.0).range(0.1..=1_000_000.0).suffix(" Hz").prefix("cutoff ")), "Low-pass cutoff (single-pole RC): removes noise above this frequency.");
                    }
                    MathOp::Rpm => {
                        help(
                            ui.add(egui::DragValue::new(&mut m.k).speed(0.1).range(0.1..=200.0).suffix(" pulses/rev")),
                            "Pulses per revolution of the sensor: crank 60-2 wheel ≈ 58 (one per tooth), cam Hall ≈ 0.5 (one pulse per two crank revolutions), ignition primary per coil ≈ 0.5.",
                        );
                    }
                    _ => {}
                }
            });
        }
    }
}

#[cfg(test)]
mod math_tests {
    use super::*;

    #[test]
    fn math_encode_decode() {
        let m = Math {
            op: MathOp::Rpm,
            a: 2,
            b: 5,
            k: 58.0,
            vdiv: 17,
            offset: -2.5,
            pane: 1,
        };
        assert_eq!(Math::decode(&m.encode()), Some(m));
        assert_eq!(Math::decode("99:0:0:1"), None);
        // indices are stable: older entries keep their meaning, new ops at the end
        assert_eq!(Math::decode("5:0:1:58").map(|m| m.op), Some(MathOp::Rpm));
        for (i, op) in [(7, MathOp::Ac), (8, MathOp::Mul), (9, MathOp::Abs)] {
            let m = Math {
                op,
                a: 1,
                b: 3,
                k: 1.0,
                vdiv: 9,
                offset: 0.0,
                pane: 0,
            };
            assert!(m.encode().starts_with(&format!("{i}:")));
            assert_eq!(Math::decode(&m.encode()), Some(m));
        }
        // older 4-field form still loads, with a default display
        assert_eq!(
            Math::decode("5:0:1:58").map(|m| (m.k, m.vdiv, m.pane)),
            Some((58.0, 9, 0))
        );
        assert_eq!(math_div(17), 500.0);
        assert_eq!(math_div(math_div_for(0.4)), 0.5);
    }
}

// ───────────────────────────── auto-set ─────────────────────────────

/// What auto-set picked.
#[derive(Clone, Debug, PartialEq)]
struct AutoPick {
    tb_id: Option<u8>,
    vdiv: [Option<usize>; CHANNELS],
    offset: [Option<f32>; CHANNELS],
    trig: Option<(usize, f32)>,
}

/// Decide settings from one measurement per shown channel (display units; `udiv_scale[ch]` = probe scale).
/// Timebase: ~3 periods on screen (≥ next valid timebase); V/div: smallest fitting the peak-to-peak in
/// 6 div; offset centres the mean; trigger: first channel with a frequency, at mid-level.
fn autoset_pick(meas: &[Option<Meas>; CHANNELS], scale: &[f32; CHANNELS]) -> AutoPick {
    let mut p = AutoPick {
        tb_id: None,
        vdiv: [None; CHANNELS],
        offset: [None; CHANNELS],
        trig: None,
    };
    for ch in 0..CHANNELS {
        let Some(m) = meas[ch] else { continue };
        let vpp = (m.max - m.min).abs();
        // fit the peak-to-peak in 6 div, and keep the peak within ±4 div: the offset is display-only, so the
        // hardware range must still cover the real signal
        let peak = m.max.abs().max(m.min.abs());
        let need = (vpp / 6.0).max(peak / 4.0).max(0.002 * scale[ch].abs());
        let i = VDIVS
            .iter()
            .position(|&v| v * scale[ch].abs() >= need)
            .unwrap_or(VDIVS.len() - 1);
        p.vdiv[ch] = Some(i);
        let ud = VDIVS[i] * scale[ch].abs();
        let mid = (m.max + m.min) / 2.0;
        p.offset[ch] = Some((-mid / ud).clamp(-DIV_Y / 2.0, DIV_Y / 2.0));
        if p.trig.is_none() && m.freq.is_some() {
            p.trig = Some((ch, mid));
            let f = m.freq.unwrap_or(1000.0);
            let want = 3.0 / f / DIV_X as f64; // s/div for ~3 periods
            p.tb_id = (0..=TB_MAX)
                .find(|&id| !TB_SKIP.contains(&id) && burst_ns_per_div(id) as f64 * 1e-9 >= want);
        }
    }
    p
}

struct AutoSet {
    stage: u8,
    seq0: u64,
    started: Instant,
}

impl App {
    /// Start auto-set: broad settings first (largest range, 1 ms/div, Auto trigger, no streaming).
    fn autoset_start(&mut self) {
        self.autoset_undo = Some(self.view_state());
        for ch in 0..CHANNELS {
            if self.on[ch] {
                self.vdiv[ch] = 9; // 5 V/div → largest hardware range
                self.offset[ch] = 0.0;
            }
        }
        self.tb_id = burst_id_for_ns(1_000_000).unwrap_or(18);
        self.roll = None;
        self.trig_mode = TrigMode::Auto;
        self.zoom = Zoom::default();
        if self.run != Run::Running {
            self.set_run(Run::Running);
        }
        self.autoset = Some(AutoSet {
            stage: 1,
            seq0: self.last_seq,
            started: Instant::now(),
        });
        self.autoset_msg = Some(("Auto-set: looking at the signals…".into(), Instant::now()));
    }

    /// Advance auto-set when fresh captures have arrived.
    fn autoset_step(&mut self, snap: &Snapshot) {
        let Some(a) = &self.autoset else { return };
        if a.started.elapsed().as_secs_f32() > 5.0 {
            self.autoset = None;
            self.autoset_msg = Some((
                "Auto-set gave up: no captures arrived (is the scope connected?).".into(),
                Instant::now(),
            ));
            return;
        }
        if snap.frame.seq < a.seq0 + 2 || snap.roll {
            return; // wait for two captures taken with the new settings
        }
        let stage = a.stage;
        let Some(cfg) = snap.cfg.as_ref() else { return };
        let mut meas: [Option<Meas>; CHANNELS] = [None; CHANNELS];
        for ch in 0..CHANNELS {
            let d = &snap.frame.data[ch];
            if !self.on[ch] || d.len() < 8 {
                continue;
            }
            let vs = cfg.vscale[ch];
            let (zu, vu) = self.uconv(ch, vs, snap.zero[vs.index()][ch]);
            meas[ch] = Some(measure(
                d,
                zu,
                vu,
                Some(capture_seconds(cfg.timebase_id) / d.len() as f64),
            ));
        }
        let scale: [f32; CHANNELS] = std::array::from_fn(|ch| self.probes[ch].scale);
        let p = autoset_pick(&meas, &scale);
        for ch in 0..CHANNELS {
            if let (Some(i), Some(o)) = (p.vdiv[ch], p.offset[ch]) {
                self.vdiv[ch] = i;
                self.offset[ch] = o;
            }
        }
        if stage == 1 {
            if let Some(tb) = p.tb_id {
                self.tb_id = tb;
            }
            if let Some((ch, lvl)) = p.trig {
                self.trig_src = ch as u8;
                self.trig_v = lvl;
                self.trig_slope = Slope::Rising;
                self.trig_pos = 500;
            }
            self.autoset = Some(AutoSet {
                stage: 2,
                seq0: snap.frame.seq,
                started: Instant::now(),
            });
        } else {
            self.autoset = None;
            let shown: Vec<String> = (0..CHANNELS)
                .filter(|&ch| self.on[ch])
                .map(|ch| {
                    format!(
                        "CH{} {}/div",
                        ch + 1,
                        fmt_div(self.udiv(ch), &self.probes[ch].unit)
                    )
                })
                .collect();
            let trig = match p.trig {
                Some((ch, _)) => format!(
                    "trigger CH{} at {}",
                    ch + 1,
                    fmt_u(self.trig_v, &self.probes[ch].unit)
                ),
                None => "no periodic signal found — trigger unchanged".into(),
            };
            let tb = fmt_time(burst_ns_per_div(self.tb_id) as f64 * 1e-9);
            self.autoset_msg = Some((
                format!(
                    "Auto-set: {tb}/div, {}, {trig}. Backspace = undo.",
                    shown.join(", ")
                ),
                Instant::now(),
            ));
        }
    }
}

#[cfg(test)]
mod autoset_tests {
    use super::*;

    #[test]
    fn autoset_1khz_2vpp_square() {
        let m = Meas {
            min: 0.0,
            max: 2.0,
            freq: Some(1000.0),
            ..Default::default()
        };
        let mut meas = [None; CHANNELS];
        meas[0] = Some(m);
        meas[1] = Some(Meas {
            min: 12.0,
            max: 12.4,
            ..Default::default()
        }); // battery-like DC, no frequency
        let p = autoset_pick(&meas, &[1.0; CHANNELS]);
        // 3 periods = 3 ms over 10 div → ≥ 300 µs/div → 500 µs (200 µs is skipped)
        assert_eq!(p.tb_id, burst_id_for_ns(500_000));
        // 2 Vpp / 6 div = 0.33 → 0.5 V/div, centred: offset = −1 V / 0.5 = −2 div
        assert_eq!(p.vdiv[0].map(|i| VDIVS[i]), Some(0.5));
        assert!((p.offset[0].unwrap() + 2.0).abs() < 1e-6);
        assert_eq!(p.trig, Some((0, 1.0)));
        // DC channel 12.0…12.4 V: peak 12.4 V within 4 div → 5 V/div, centred at −2.44 div
        assert_eq!(p.vdiv[1].map(|i| VDIVS[i]), Some(5.0));
        assert!((p.offset[1].unwrap() + 2.44).abs() < 1e-3);
    }
}

// ───────────────────────────── settings window ─────────────────────────────

/// Total RAM in MB (Linux; None elsewhere) — caps the memory limit at 75 %.
fn total_ram_mb() -> Option<usize> {
    let t = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: usize = t
        .lines()
        .find(|l| l.starts_with("MemTotal:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some(kb / 1024)
}

impl App {
    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Settings").open(&mut open).default_width(440.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().max_height(ctx.content_rect().height() * 0.8).show(ui, |ui| {
                ui.horizontal(|ui| {
                    if help(ui.button("Keyboard shortcuts"), "List of all keys (also F1 / H).").clicked() {
                        self.show_keys = true;
                    }
                    if let Some(d) = crate::hantek::config_dir() {
                        ui.label(egui::RichText::new(format!("saved in {}", d.display())).weak());
                    }
                });
                // sections remember open/closed (egui memory); only the shown channels' probes are listed
                let head = |t: &str| egui::RichText::new(t).strong();
                egui::CollapsingHeader::new(head("Probes")).default_open(true).show(ui, |ui| {
                    for c in 0..CHANNELS {
                        if !self.on[c] {
                            continue;
                        }
                        self.probe_controls(ui, c);
                    }
                    ui.label(egui::RichText::new("Channels that are off are not listed — switch one on to set its probe.").weak());
                });
                egui::CollapsingHeader::new(head("Display")).default_open(true).show(ui, |ui| {
                    let n = ui.add(egui::DragValue::new(&mut self.persist_n).range(2..=100).suffix(" captures").prefix("persistence depth "));
                    help(n, "How many earlier captures persistence (P) draws faded under the live trace.");
                    help(
                        ui.checkbox(&mut self.adaptive, format!("adaptive quality (keep ≥ {DROP_FPS} fps)")),
                        "When a frame takes too long (less than 20 fps possible), nice-to-have features pause one at a time — persistence, math & FFT, references, statistics, jitter correction — and come back by themselves. A REDUCED badge shows what is paused. Acquisition, traces, trigger and recording are never touched.",
                    );
                    help(ui.checkbox(&mut self.show_perf, "performance numbers in the status bar"), "UI frames/s, paint time, capture rate and USB time per capture. Off: only the scope state and captures/s.");
                });
                egui::CollapsingHeader::new(head("Trigger")).default_open(false).show(ui, |ui| {
                    help(
                        ui.checkbox(&mut self.trig_corr, "trigger jitter correction"),
                        "The scope only knows its trigger point to a few samples (up to ~20 µs at fast timebases), so the waveform jumps left/right. On: each capture is shifted so the real trigger crossing sits exactly on the trigger marker (~40 ns per capture).",
                    );
                });
                egui::CollapsingHeader::new(head("Engine")).default_open(false).show(ui, |ui| self.engine_settings(ui));
                egui::CollapsingHeader::new(head("Memory")).default_open(false).show(ui, |ui| {
                    let (used, budget) = self.acq.history.lock().map_or((0, 0), |h| (h.used_bytes(), h.budget));
                    let ram = total_ram_mb();
                    let max = ram.map_or(16 * 1024, |m| (m * 3 / 4).clamp(64, 16 * 1024));
                    ui.horizontal(|ui| {
                        let d = ui.add(egui::DragValue::new(&mut self.mem_mb).range(64..=max).speed(16.0).suffix(" MB").prefix("history limit "));
                        help(d, "RAM for the capture history and the stream recording on screen together. When full, whatever is recorded now pushes out the other kind first, then its own oldest data. 512 MB ≈ 30 min of 8-channel captures at full rate.");
                        ui.label(format!("used {:.0} MB", used as f64 / 1048576.0));
                    });
                    if let Some(ram) = ram {
                        ui.label(egui::RichText::new(format!("max {max} MB (75 % of {ram} MB RAM)")).weak());
                    }
                    if self.mem_mb * 1024 * 1024 != budget {
                        if let Ok(mut h) = self.acq.history.lock() {
                            h.set_budget(self.mem_mb * 1024 * 1024);
                        }
                    }
                    self.files_ram_setting(ui);
                });
                egui::CollapsingHeader::new(head("Recording, event log, folder, open")).default_open(false).show(ui, |ui| {
                    self.files_settings(ui);
                });
            });
        });
        self.settings_open &= open;
    }
}

// ───────────────────────────── persistence image ─────────────────────────────

/// Rasterise a screen polyline into a COLUMN-MAJOR premultiplied RGBA image (`w`×`h` px, index x·h + y, origin
/// `o`, `ppp` px per point): per column the vertical span the segment covers — contiguous in memory, so the
/// per-byte max vectorises. Colours combine by per-component max (no overflow, brighter = newer wins).
fn raster_polyline(
    img: &mut [[u8; 4]],
    w: usize,
    h: usize,
    pts: &[Pos2],
    o: Pos2,
    ppp: f32,
    col: Color32,
) {
    let (wm, hm) = (w as f32 - 1.0, h as f32 - 1.0);
    let c = col.to_array();
    let mut put = |x: usize, ya: f32, yb: f32| {
        let (a, b) = (
            ya.min(yb).clamp(0.0, hm) as usize,
            ya.max(yb).clamp(0.0, hm) as usize,
        );
        for p in &mut img[x * h + a..=x * h + b] {
            for k in 0..4 {
                p[k] = p[k].max(c[k]);
            }
        }
    };
    for s in pts.windows(2) {
        let (mut xa, mut ya) = ((s[0].x - o.x) * ppp, (s[0].y - o.y) * ppp);
        let (mut xb, mut yb) = ((s[1].x - o.x) * ppp, (s[1].y - o.y) * ppp);
        if xa > xb {
            std::mem::swap(&mut xa, &mut xb);
            std::mem::swap(&mut ya, &mut yb);
        }
        if xb < 0.0 || xa > wm + 1.0 {
            continue;
        }
        let (ca, cb) = (xa.clamp(0.0, wm) as usize, xb.clamp(0.0, wm) as usize);
        if ca == cb || xb - xa < 1e-3 {
            put(ca, ya, yb);
            continue;
        }
        let k = (yb - ya) / (xb - xa);
        for x in ca..=cb {
            let (x0, x1) = ((x as f32).max(xa), (x as f32 + 1.0).min(xb));
            put(x, ya + k * (x0 - xa), ya + k * (x1 - xa));
        }
    }
}

/// Multiply every byte by `q`/256 (phosphor decay; premultiplied colour, so alpha fades with it).
fn decay(img: &mut [[u8; 4]], q: u16) {
    for b in img.as_flattened_mut() {
        *b = ((*b as u16 * q) >> 8) as u8;
    }
}

/// Cached persistence image of one pane.
struct PersistTex {
    /// view key (everything that moves traces on screen, and the depth)
    view: u64,
    /// newest capture drawn into the image
    newest: usize,
    tex: egui::TextureHandle,
    /// column-major RGBA (x·h + y), uploaded as an h×w texture and drawn on a rotated quad
    buf: Vec<[u8; 4]>,
}

impl App {
    /// Persistence for one pane, phosphor style: earlier captures in one image, fading with age.
    /// A new capture costs one decay pass + drawing that capture (independent of the depth); the image is
    /// rebuilt from the last N captures only when the view changes or the capture sequence jumps (history
    /// browsing). Drawn as one textured quad instead of N × channels tessellated lines.
    #[allow(clippy::too_many_arguments)]
    fn persist_image(
        &mut self,
        painter: &egui::Painter,
        r: Rect,
        vr: Rect,
        dyv: f32,
        pane: usize,
        persist: &[std::sync::Arc<HEntry>],
    ) {
        use std::hash::{Hash, Hasher};
        let ppp = painter.ctx().pixels_per_point();
        let (w, h) = (
            (r.width() * ppp).round() as usize,
            (r.height() * ppp).round() as usize,
        );
        if persist.is_empty() || w < 2 || h < 2 {
            return;
        }
        let mut hs = std::collections::hash_map::DefaultHasher::new();
        (w, h, self.persist_n).hash(&mut hs);
        for v in [
            r.min.x, r.min.y, vr.min.x, vr.min.y, vr.max.x, vr.max.y, dyv, ppp,
        ] {
            v.to_bits().hash(&mut hs);
        }
        for ch in 0..CHANNELS {
            let p = &self.probes[ch];
            (
                self.on[ch],
                self.pane_for(ch),
                self.vdiv[ch],
                self.offset[ch].to_bits(),
                p.scale.to_bits(),
                p.offset.to_bits(),
            )
                .hash(&mut hs);
        }
        let view = hs.finish();
        let id = |e: &std::sync::Arc<HEntry>| std::sync::Arc::as_ptr(e) as usize;
        let newest = id(&persist[0]);
        // per-capture decay so the oldest of N is at ~15 % of the newest
        let q = (0.15f32.powf(1.0 / self.persist_n.max(2) as f32) * 256.0) as u16;
        let top = 0.55f32; // brightness of the newest persisted capture (the live trace stays brighter)
        if self.persist_tex.len() <= pane {
            self.persist_tex.resize_with(pane + 1, || None);
        }
        let (tex, mut buf, incremental) = match self.persist_tex[pane].take() {
            Some(t) if t.view == view && t.newest == newest => {
                self.persist_tex[pane] = Some(t); // nothing new
                return self.persist_draw(painter, r, pane);
            }
            // exactly one new capture since the image was drawn → incremental
            Some(t) if t.view == view && persist.get(1).is_some_and(|e| id(e) == t.newest) => {
                (Some(t.tex), t.buf, true)
            }
            Some(t) => (Some(t.tex), t.buf, false),
            None => (None, Vec::new(), false),
        };
        let t0 = Instant::now();
        let chans: Vec<usize> = (0..CHANNELS)
            .filter(|&c| self.on[c] && self.pane_for(c) == pane)
            .collect();
        let mut pts = std::mem::take(&mut self.pts);
        if incremental {
            decay(&mut buf, q);
            for &ch in &chans {
                if self
                    .trace(&persist[0].snap, ch, r, vr, dyv, &mut pts)
                    .is_some()
                {
                    raster_polyline(
                        &mut buf,
                        w,
                        h,
                        &pts,
                        r.min,
                        ppp,
                        COLORS[ch].gamma_multiply(top),
                    );
                }
            }
        } else {
            buf.clear();
            buf.resize(w * h, [0; 4]);
            let qf = q as f32 / 256.0;
            for &ch in &chans {
                for (age, e) in persist.iter().enumerate() {
                    if self.trace(&e.snap, ch, r, vr, dyv, &mut pts).is_some() {
                        raster_polyline(
                            &mut buf,
                            w,
                            h,
                            &pts,
                            r.min,
                            ppp,
                            COLORS[ch].gamma_multiply(top * qf.powi(age as i32)),
                        );
                    }
                }
            }
        }
        self.pts = pts;
        let image = egui::ColorImage::new(
            [h, w],
            buf.iter()
                .map(|p| Color32::from_rgba_premultiplied(p[0], p[1], p[2], p[3]))
                .collect(),
        );
        let tex = match tex {
            Some(mut tex) => {
                tex.set(image, egui::TextureOptions::NEAREST);
                tex
            }
            None => painter.ctx().load_texture(
                format!("persist{pane}"),
                image,
                egui::TextureOptions::NEAREST,
            ),
        };
        log::trace!(
            "persistence {w}×{h} {}: {:.2} ms",
            if incremental { "+1" } else { "rebuild" },
            t0.elapsed().as_secs_f64() * 1e3
        );
        self.persist_tex[pane] = Some(PersistTex {
            view,
            newest,
            tex,
            buf,
        });
        self.persist_draw(painter, r, pane);
    }

    /// The cached image on a quad rotated so that texture rows (= screen columns) run left → right.
    fn persist_draw(&self, painter: &egui::Painter, r: Rect, pane: usize) {
        let Some(Some(t)) = self.persist_tex.get(pane) else {
            return;
        };
        let mut m = egui::Mesh::with_texture(t.tex.id());
        for (pos, uv) in [
            (r.left_top(), (0.0, 0.0)),
            (r.right_top(), (0.0, 1.0)),
            (r.right_bottom(), (1.0, 1.0)),
            (r.left_bottom(), (1.0, 0.0)),
        ] {
            m.vertices.push(egui::epaint::Vertex {
                pos,
                uv: Pos2::new(uv.0, uv.1),
                color: Color32::WHITE,
            });
        }
        m.indices.extend([0, 1, 2, 0, 2, 3]);
        painter.add(Shape::mesh(m));
    }
}

#[cfg(test)]
mod persist_tests {
    use super::*;

    /// 30 captures × 8 channels of a noisy 1500-column trace: old way (tessellate anti-aliased lines) vs raster.
    /// Run: cargo test --release persist_speed -- --ignored --nocapture
    #[test]
    #[ignore]
    fn persist_speed() {
        let (w, h) = (1500usize, 700usize);
        let traces: Vec<Vec<Pos2>> = (0..240)
            .map(|t| {
                (0..w)
                    .flat_map(|x| {
                        let base =
                            350.0 + 200.0 * ((x as f32 / 75.0 + t as f32 * 0.01).sin()).signum();
                        let n = ((x * 7919 + t * 104729) % 13) as f32;
                        [Pos2::new(x as f32, base - n), Pos2::new(x as f32, base + n)]
                    })
                    .collect()
            })
            .collect();
        let mut tess = egui::epaint::Tessellator::new(1.0, Default::default(), [1, 1], vec![]);
        let t0 = Instant::now();
        let mut mesh = egui::epaint::Mesh::default();
        for p in &traces {
            tess.tessellate_shape(
                Shape::line(
                    p.clone(),
                    Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 0, 0, 80)),
                ),
                &mut mesh,
            );
        }
        let old = t0.elapsed().as_secs_f64() * 1e3;
        let t0 = Instant::now();
        let mut img = vec![[0u8; 4]; w * h];
        for p in &traces {
            raster_polyline(
                &mut img,
                w,
                h,
                p,
                Pos2::ZERO,
                1.0,
                Color32::from_rgba_premultiplied(80, 0, 0, 80),
            );
        }
        let new = t0.elapsed().as_secs_f64() * 1e3;
        println!(
            "worst case 240 noisy traces: tessellate {old:.1} ms ({} vertices), raster {new:.1} ms",
            mesh.vertices.len()
        );
        // realistic: 4 channels × 30 captures, 1000 samples across 1500 px, ±1 px noise
        let real: Vec<Vec<Pos2>> = (0..120)
            .map(|t| {
                (0..1000)
                    .map(|i| {
                        Pos2::new(
                            i as f32 * 1.5,
                            350.0
                                + 200.0 * ((i as f32 / 50.0).sin()).signum()
                                + ((i * 31 + t) % 3) as f32,
                        )
                    })
                    .collect()
            })
            .collect();
        let t0 = Instant::now();
        let mut mesh = egui::epaint::Mesh::default();
        for p in &real {
            tess.tessellate_shape(
                Shape::line(
                    p.clone(),
                    Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 0, 0, 80)),
                ),
                &mut mesh,
            );
        }
        let old = t0.elapsed().as_secs_f64() * 1e3;
        let t0 = Instant::now();
        img.iter_mut().for_each(|p| *p = [0; 4]);
        for p in &real {
            raster_polyline(
                &mut img,
                w,
                h,
                p,
                Pos2::ZERO,
                1.0,
                Color32::from_rgba_premultiplied(80, 0, 0, 80),
            );
        }
        let new = t0.elapsed().as_secs_f64() * 1e3;
        println!(
            "realistic 120 traces: tessellate {old:.1} ms ({} vertices), raster {new:.1} ms",
            mesh.vertices.len()
        );
        // incremental phosphor step: decay + 4 new traces + conversion for upload
        let t0 = Instant::now();
        for _ in 0..10 {
            decay(&mut img, 240);
            for p in &real[..4] {
                raster_polyline(
                    &mut img,
                    w,
                    h,
                    p,
                    Pos2::ZERO,
                    1.0,
                    Color32::from_rgba_premultiplied(80, 0, 0, 80),
                );
            }
            let up: Vec<Color32> = img
                .iter()
                .map(|p| Color32::from_rgba_premultiplied(p[0], p[1], p[2], p[3]))
                .collect();
            assert_eq!(up.len(), w * h);
        }
        println!(
            "incremental step (any depth): {:.2} ms",
            t0.elapsed().as_secs_f64() * 1e3 / 10.0
        );
        assert!(img.iter().any(|c| c[0] > 0));
    }

    #[test]
    fn raster_fills_vertical_and_sloped_segments() {
        let (w, h) = (10, 10);
        let mut img = vec![[0u8; 4]; w * h];
        let red = Color32::from_rgba_premultiplied(200, 0, 0, 200);
        raster_polyline(
            &mut img,
            w,
            h,
            &[
                Pos2::new(2.0, 1.0),
                Pos2::new(2.0, 5.0),
                Pos2::new(8.0, 5.0),
            ],
            Pos2::ZERO,
            1.0,
            red,
        );
        assert!((1..=5).all(|y| img[2 * h + y] == red.to_array())); // vertical span (column-major)
        assert!((2..=8).all(|x| img[x * h + 5] == red.to_array())); // horizontal run
        assert_eq!(img[0], [0; 4]);
    }
}

impl App {
    /// Scale of this pane at its bottom-left edge: time/div and each channel's units/div, corrected for zoom.
    fn scale_legend(&self, painter: &egui::Painter, r: Rect, pane: usize) {
        let z = self.zoom;
        let font = FontId::monospace(11.0);
        let mut x = r.left() + 26.0; // right of the channel handles
        let y = r.bottom() - 4.0;
        let mut put = |t: String, c: Color32| {
            let rr = painter.text(Pos2::new(x, y), Align2::LEFT_BOTTOM, t, font.clone(), c);
            x = rr.right() + 10.0;
        };
        put(
            format!(
                "{}/div",
                fmt_time(self.span_s() / DIV_X as f64 * (z.x1 - z.x0) as f64)
            ),
            Color32::from_gray(170),
        );
        for ch in (0..CHANNELS).filter(|&c| self.on[c] && self.pane_for(c) == pane) {
            put(
                format!(
                    "CH{} {}/div",
                    ch + 1,
                    fmt_div(self.udiv(ch) * (z.y1 - z.y0), &self.probes[ch].unit)
                ),
                COLORS[ch].gamma_multiply(0.85),
            );
        }
        for (i, m) in self.math.iter().enumerate() {
            if !matches!(m.op, MathOp::Off | MathOp::Fft)
                && m.pane.min(self.panes.max(1) - 1) == pane
            {
                let unit = if m.op == MathOp::Rpm {
                    "rpm".to_string()
                } else {
                    self.unit(m.a).to_string()
                };
                put(
                    format!(
                        "M{} {}/div",
                        i + 1,
                        fmt_div(math_div(m.vdiv) * (z.y1 - z.y0), &unit)
                    ),
                    MATH_COLORS[i].gamma_multiply(0.85),
                );
            }
        }
    }
}

/// Settings auto-set replaces, so one press of Backspace can undo it.
#[derive(Clone)]
struct ViewUndo {
    tb_id: u8,
    vdiv: [usize; CHANNELS],
    offset: [f32; CHANNELS],
    trig: (u8, f32, Slope, u16, TrigMode),
    roll: Option<u8>,
    zoom: Zoom,
}

impl App {
    fn view_state(&self) -> ViewUndo {
        ViewUndo {
            tb_id: self.tb_id,
            vdiv: self.vdiv,
            offset: self.offset,
            trig: (
                self.trig_src,
                self.trig_v,
                self.trig_slope,
                self.trig_pos,
                self.trig_mode,
            ),
            roll: self.roll,
            zoom: self.zoom,
        }
    }

    /// Undo the last auto-set (Backspace / the info-line hint).
    fn autoset_undo(&mut self) {
        let Some(u) = self.autoset_undo.take() else {
            return;
        };
        self.tb_id = u.tb_id;
        self.vdiv = u.vdiv;
        self.offset = u.offset;
        (
            self.trig_src,
            self.trig_v,
            self.trig_slope,
            self.trig_pos,
            self.trig_mode,
        ) = u.trig;
        self.roll = u.roll;
        self.zoom = u.zoom;
        self.autoset = None;
        self.autoset_msg = Some((
            "Auto-set undone: previous settings restored.".into(),
            Instant::now(),
        ));
    }
}

// ───────────────────────────── reference waveforms ─────────────────────────────

const REF_COLORS: [Color32; 2] = [
    Color32::from_rgb(235, 235, 235),
    Color32::from_rgb(150, 235, 220),
];

impl App {
    /// References R1/R2 under the live traces: in their own unit with the scale of the channel they are shown with
    /// (+ their own offset), their trigger point on the live trigger marker, rescaled to the current timebase; min/max
    /// per pixel column, so the cost doesn't grow with the sample count.
    fn draw_refs(&mut self, painter: &egui::Painter, r: Rect, vr: Rect, dyv: f32, pane: usize) {
        if !self.refs_on || self.dropped(DROP_REFS) {
            return;
        }
        let mut pts = std::mem::take(&mut self.pts);
        for k in 0..2 {
            let Some(rf) = self.refs[k].as_ref() else {
                continue;
            };
            if !rf.on || !self.on[rf.under] || self.pane_for(rf.under) != pane {
                continue;
            }
            let b = (vr.width() as f64 * rf.dt / self.span_s()) as f32; // px per sample
            let x_trig = vr.left() + vr.width() * self.trig_pos as f32 / 1000.0;
            let a = x_trig - b * rf.trig_i as f32;
            let ud = self.udiv(rf.under).max(1e-12);
            let y0 = vr.center().y - (self.offset[rf.under] + rf.offset) * dyv;
            let y = |v: f32| (y0 - v / ud * dyv).clamp(r.top(), r.bottom());
            let n = rf.v.len();
            let i0 = ((r.left() - a) / b).floor().max(0.0) as usize;
            let i1 = (((r.right() - a) / b).ceil().max(0.0) as usize + 1).min(n);
            if i1 <= i0 + 1 {
                continue;
            }
            pts.clear();
            if b >= 1.0 {
                pts.extend((i0..i1).map(|i| Pos2::new(a + b * i as f32, y(rf.v[i]))));
            } else {
                let mut col = (a + b * i0 as f32) as i32;
                let (mut lo, mut hi) = (f32::MAX, f32::MIN);
                for i in i0..i1 {
                    let c = (a + b * i as f32) as i32;
                    if c != col {
                        pts.push(Pos2::new(col as f32, y(hi)));
                        pts.push(Pos2::new(col as f32, y(lo)));
                        (col, lo, hi) = (c, f32::MAX, f32::MIN);
                    }
                    (lo, hi) = (lo.min(rf.v[i]), hi.max(rf.v[i]));
                }
                if lo <= hi {
                    pts.push(Pos2::new(col as f32, y(hi)));
                    pts.push(Pos2::new(col as f32, y(lo)));
                }
            }
            if pts.len() > 1 {
                painter.add(Shape::line(
                    pts.clone(),
                    Stroke::new(1.0, REF_COLORS[k].gamma_multiply(0.55)),
                ));
                let p0 = pts[0];
                painter.text(
                    Pos2::new(p0.x.max(r.left()) + 14.0, p0.y - 2.0),
                    Align2::LEFT_BOTTOM,
                    format!("R{}", k + 1),
                    FontId::monospace(10.0),
                    REF_COLORS[k].gamma_multiply(0.9),
                );
            }
        }
        self.pts = pts;
    }
}

// ───────────────────────────── adaptive feature dropout ─────────────────────────────

/// Below this frame rate nice-to-have features pause (owner: 20 fps).
const DROP_FPS: f32 = 20.0;
/// Drop order (first paused first); index + 1 = the level at which it is paused.
const DROP_NAMES: [&str; 5] = [
    "persistence",
    "math & FFT",
    "references",
    "statistics",
    "jitter correction",
];
const DROP_PERSIST: u8 = 1;
const DROP_MATH: u8 = 2;
const DROP_REFS: u8 = 3;
const DROP_STATS: u8 = 4;
const DROP_JITTER: u8 = 5;

/// Sheds one feature after 1 s of slow frames, restores one after 5 s of fast frames. A restore that is followed by
/// a new shed within 15 s (it didn't fit) doubles the wait before the next try (10 s … 5 min), so nothing flickers.
#[derive(Default)]
struct Dropout {
    level: u8,
    slow_since: Option<Instant>,
    fast_since: Option<Instant>,
    hold_until: Option<Instant>,
    restored_at: Option<Instant>,
    bounces: u32,
}

impl Dropout {
    /// Returns the new level when it changed.
    fn update(&mut self, now: Instant, slow: bool, fast: bool) -> Option<u8> {
        let max = DROP_NAMES.len() as u8;
        if slow {
            self.fast_since = None;
            let since = *self.slow_since.get_or_insert(now);
            if now.duration_since(since).as_secs_f32() >= 1.0 && self.level < max {
                self.level += 1;
                self.slow_since = Some(now); // another second before the next step
                if self
                    .restored_at
                    .is_some_and(|t| now.duration_since(t).as_secs() < 15)
                {
                    self.bounces = (self.bounces + 1).min(5);
                }
                self.hold_until = Some(now + std::time::Duration::from_secs(10 << self.bounces));
                return Some(self.level);
            }
        } else if fast {
            self.slow_since = None;
            let since = *self.fast_since.get_or_insert(now);
            let held = self.hold_until.is_some_and(|h| now < h);
            if self.level > 0 && !held && now.duration_since(since).as_secs_f32() >= 5.0 {
                self.level -= 1;
                self.fast_since = Some(now);
                self.restored_at = Some(now);
                return Some(self.level);
            }
        } else {
            (self.slow_since, self.fast_since) = (None, None);
        }
        None
    }
}

impl App {
    /// A nice-to-have feature is paused by the adaptive feature dropout.
    fn dropped(&self, feature: u8) -> bool {
        self.dropout.level >= feature
    }
}

#[cfg(test)]
mod dropout_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sheds_after_1s_restores_after_5s_and_backs_off_on_bounce() {
        let t0 = Instant::now();
        let at = |s: f32| t0 + Duration::from_secs_f32(s);
        let mut d = Dropout::default();
        assert_eq!(d.update(at(0.0), true, false), None);
        assert_eq!(d.update(at(1.0), true, false), Some(1)); // persistence paused
        assert_eq!(d.update(at(1.5), true, false), None);
        assert_eq!(d.update(at(2.0), true, false), Some(2)); // math paused
                                                             // fast again: held for 10 s after the last shed, then one step back per 5 s
        for k in 0..20 {
            d.update(at(2.5 + k as f32 * 0.5), false, true);
        }
        assert_eq!(d.level, 1);
        assert_eq!(d.update(at(20.0), false, true), Some(0));
        // the restore didn't fit: shed again within 15 s → the next restore waits 20 s
        assert_eq!(d.update(at(21.0), true, false), None);
        assert_eq!(d.update(at(22.0), true, false), Some(1));
        for k in 0..30 {
            d.update(at(22.5 + k as f32 * 0.5), false, true);
        }
        assert_eq!(d.level, 1, "still held (20 s)");
        // fast the whole time: restored as soon as the (doubled) hold ends
        assert_eq!(d.update(at(43.0), false, true), Some(0));
        // in between (neither slow nor fast): nothing changes
        d.level = 3;
        assert_eq!(d.update(at(100.0), false, false), None);
        assert_eq!(d.level, 3);
    }
}

// ───────────────────────────── info-line debounce ─────────────────────────────

/// Notices are recomputed every frame from the signal, so a value near a threshold made a tip appear and vanish many
/// times a second. A notice shows after it has been present for NOTICE_SHOW_AFTER (warnings sooner), and stays
/// NOTICE_HOLD after its condition went away (with its last text). Identity = level + text without digits, so
/// changing numbers don't restart it.
#[derive(Default)]
struct NoticeDebounce {
    /// key, first seen, last seen, last text, level
    seen: Vec<(String, Instant, Instant, String, Level)>,
}

const NOTICE_SHOW_AFTER: f32 = 0.4;
const NOTICE_HOLD: f32 = 2.0;

impl NoticeDebounce {
    fn key(l: Level, t: &str) -> String {
        format!(
            "{l:?}:{}",
            t.chars()
                .filter(|c| !c.is_ascii_digit())
                .collect::<String>()
        )
    }

    fn update(&mut self, now_list: Vec<(Level, String)>, now: Instant) -> Vec<(Level, String)> {
        for (l, t) in &now_list {
            let k = Self::key(*l, t);
            match self.seen.iter_mut().find(|e| e.0 == k) {
                Some(e) => {
                    e.2 = now;
                    e.3.clone_from(t);
                }
                None => self.seen.push((k, now, now, t.clone(), *l)),
            }
        }
        self.seen
            .retain(|e| now.duration_since(e.2).as_secs_f32() < NOTICE_HOLD);
        // in the order they first appeared: stable rows, no jumping
        self.seen
            .iter()
            .filter(|e| {
                let age = now.duration_since(e.1).as_secs_f32();
                age >= NOTICE_SHOW_AFTER || (e.4 == Level::Warn && age >= 0.15)
            })
            .map(|e| (e.4, e.3.clone()))
            .collect()
    }
}

#[cfg(test)]
mod notice_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn flickering_tip_is_shown_steadily() {
        let t0 = Instant::now();
        let at = |s: f32| t0 + Duration::from_secs_f32(s);
        let mut d = NoticeDebounce::default();
        let tip = || vec![(Level::Tip, "CH1: set V/div to 500 mV".to_string())];
        assert!(d.update(tip(), at(0.0)).is_empty()); // not yet (0.4 s)
                                                      // condition toggles every frame for a second: shown steadily after 0.4 s, never hidden
        let mut shown = Vec::new();
        for k in 1..60 {
            let list = if k % 2 == 0 { tip() } else { Vec::new() };
            shown.push(!d.update(list, at(k as f32 / 60.0)).is_empty());
        }
        assert!(shown[30..].iter().all(|&s| s));
        // numbers change: same notice
        assert_eq!(
            d.update(
                vec![(Level::Tip, "CH1: set V/div to 200 mV".into())],
                at(1.1)
            )
            .len(),
            1
        );
        // gone for good: held 2 s, then removed
        assert_eq!(d.update(Vec::new(), at(2.5)).len(), 1);
        assert!(d.update(Vec::new(), at(3.2)).is_empty());
    }
}
