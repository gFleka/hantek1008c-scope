//! Acquisition thread: the only code that touches USB. UI → here via `Cmd` (mpsc),
//! here → UI via a lock-free triple buffer holding the newest `Snapshot`.

use crate::hantek::*;
use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use triple_buffer::{triple_buffer, Input, Output};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Run {
    Running,
    Stopped,
    Single,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Burst,
    /// roll mode with an `a3` rate id from ROLL_RATES
    Roll(u8),
}

pub enum Cmd {
    Config(Config),
    Run(Run),
    Mode(Mode),
    /// rows to keep in roll/stream mode (the UI sends one screen's worth)
    Keep(usize),
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub frame: Frame,
    /// settings the frame was captured with
    pub cfg: Option<Config>,
    pub zero: [[f32; CHANNELS]; 3],
    pub roll: bool,
    pub acq_fps: f32,
    pub usb_ms: f32,
    pub status: String,
    /// the last capture was forced by the Auto timeout (no trigger found)
    pub forced: bool,
    /// acquisition is stopped (e.g. after the Single capture)
    pub stopped: bool,
    /// real trigger crossing − nominal trigger sample, in samples (hardware-triggered captures only; the
    /// UI shifts the capture by it — trigger jitter correction)
    pub trig_off: Option<f32>,
}

pub struct Acq {
    pub tx: Sender<Cmd>,
    pub out: Output<Snapshot>,
    pub history: Arc<Mutex<History>>,
    /// recording to disk: settings/meta go straight to the writer thread; status for the UI
    pub rec: crate::rec::RecSink,
}

/// One stored capture.
pub struct HEntry {
    pub snap: Snapshot,
    /// wall-clock time of the capture
    pub at: std::time::SystemTime,
}

/// Every triggered capture, newest last, plus the stream recording — together within one memory limit.
/// Whatever is being recorded now wins: the other kind is dropped first, then its own oldest data.
/// Captures are numbered: `first_no` is the number of `items[0]`, so numbers stay stable while old ones drop.
pub struct History {
    pub items: VecDeque<Arc<HEntry>>,
    pub first_no: u64,
    bytes: usize,
    pub budget: usize,
    /// the current/last stream recording (roll mode), browsable after Stop
    pub stream: StreamRec,
}

/// A stream recording: rows appended as they arrive; oldest rows dropped above the budget.
#[derive(Default)]
pub struct StreamRec {
    pub data: [Vec<u16>; CHANNELS],
    pub active: [bool; CHANNELS],
    /// rows per second per channel
    pub rate: f64,
    pub cfg: Option<Config>,
    pub zero: [[f32; CHANNELS]; 3],
    pub started: Option<std::time::SystemTime>,
    /// rows dropped from the front (row numbers stay absolute: row = dropped + index)
    pub dropped: u64,
}

impl StreamRec {
    pub fn rows(&self) -> usize {
        self.data
            .iter()
            .zip(self.active)
            .filter(|(_, a)| *a)
            .map(|(d, _)| d.len())
            .max()
            .unwrap_or(0)
    }
    pub fn bytes(&self) -> usize {
        self.data.iter().map(|d| d.len() * 2).sum()
    }
    /// Append the last `rows` rows of `f` (the memory limit is applied by `History::trim`).
    pub fn append(&mut self, f: &Frame, rows: usize) {
        for ch in 0..CHANNELS {
            if self.active[ch] {
                let d = &f.data[ch];
                let k = rows.min(d.len());
                self.data[ch].extend_from_slice(&d[d.len() - k..]);
            }
        }
    }
    /// Drop the oldest 10 % of rows in one go (draining moves the buffer; don't do it per row).
    fn drop_oldest(&mut self) {
        let drop = (self.rows() / 10).max(1);
        for d in self.data.iter_mut() {
            let k = drop.min(d.len());
            d.drain(..k);
        }
        self.dropped += drop as u64;
    }
}

impl History {
    pub fn new(budget: usize) -> Self {
        History {
            items: VecDeque::new(),
            first_no: 0,
            bytes: 0,
            budget,
            stream: StreamRec::default(),
        }
    }
    fn cost(e: &HEntry) -> usize {
        e.snap
            .frame
            .data
            .iter()
            .map(|d| d.capacity() * 2)
            .sum::<usize>()
            + std::mem::size_of::<HEntry>()
            + 64
    }
    pub fn push(&mut self, e: HEntry) {
        self.bytes += Self::cost(&e);
        self.items.push_back(Arc::new(e));
        self.trim(false);
    }
    /// Append stream rows and apply the limit (captures are dropped before stream rows).
    pub fn append_stream(&mut self, f: &Frame, rows: usize) {
        self.stream.append(f, rows);
        self.trim(true);
    }
    /// Change the memory limit and apply it now.
    pub fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
        self.trim(false);
    }
    fn drop_capture(&mut self) {
        if let Some(old) = self.items.pop_front() {
            self.bytes -= Self::cost(&old);
            self.first_no += 1;
        }
    }
    /// Keep captures + stream under the limit; `streaming` = the stream is the data being recorded now.
    /// An opened recording file is what the user wants to see: make room for it (captures go first).
    pub fn fit_stream(&mut self) {
        self.trim(true);
    }
    fn trim(&mut self, streaming: bool) {
        while self.bytes + self.stream.bytes() > self.budget {
            let (caps, rows) = (self.items.len(), self.stream.rows());
            if streaming && caps > 0 {
                self.drop_capture();
            } else if !streaming && rows > 0 {
                self.stream = StreamRec {
                    dropped: self.stream.dropped + rows as u64,
                    ..StreamRec::default()
                };
            } else if streaming && rows > 1 {
                self.stream.drop_oldest();
            } else if !streaming && caps > 1 {
                self.drop_capture();
            } else {
                break; // keep at least the newest capture / rows
            }
        }
    }
    /// Captures + stream recording in bytes.
    pub fn used_bytes(&self) -> usize {
        self.bytes + self.stream.bytes()
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    /// Capture by its number (None if dropped or not yet taken).
    pub fn get(&self, no: u64) -> Option<Arc<HEntry>> {
        no.checked_sub(self.first_no)
            .and_then(|i| self.items.get(i as usize))
            .cloned()
    }
    pub fn last_no(&self) -> Option<u64> {
        (!self.items.is_empty()).then(|| self.first_no + self.items.len() as u64 - 1)
    }
    pub fn megabytes(&self) -> f32 {
        self.bytes as f32 / 1e6
    }
}

/// Default memory limit for captures + stream recording: 512 MB ≈ 64 000 8-channel captures ≈ 30 min at full
/// rate (review #5). Changeable in Settings (64 MB … 16 GB).
pub const HISTORY_BUDGET: usize = 512 * 1024 * 1024;

pub fn spawn(sim: bool, cfg: Config, repaint: impl Fn() + Send + 'static) -> Acq {
    let (tx, rx) = channel();
    let (input, out) = triple_buffer(&Snapshot::default());
    let history = Arc::new(Mutex::new(History::new(HISTORY_BUDGET)));
    let h2 = history.clone();
    let rec = crate::rec::spawn_writer();
    let rec2 = rec.clone();
    thread::Builder::new()
        .name("acq".into())
        .spawn(move || {
            Worker {
                rx,
                input,
                repaint: Box::new(repaint),
                cfg,
                run: Run::Running,
                mode: Mode::Burst,
                status: String::new(),
                keep: 2200,
                history: h2,
                rec: rec2,
                stream_new: None,
                recording: false,
            }
            .main(sim)
        })
        .expect("spawn acq thread");
    Acq {
        tx,
        out,
        history,
        rec,
    }
}

enum Src {
    Dev(Hantek),
    Sim(Sim),
}

struct Worker {
    rx: Receiver<Cmd>,
    input: Input<Snapshot>,
    repaint: Box<dyn Fn() + Send>,
    cfg: Config,
    run: Run,
    mode: Mode,
    status: String,
    /// rows kept in roll/stream mode (= one screen at the current rate)
    keep: usize,
    history: Arc<Mutex<History>>,
    rec: crate::rec::RecSink,
    /// a stream (re)started: the recording is replaced when its first rows arrive (so a recording opened
    /// from a file stays browsable until Run); value = roll rate id
    stream_new: Option<u8>,
    /// the writer has an open stream (Start sent, Stop not yet)
    recording: bool,
}

impl Worker {
    /// First rows of a new stream: new in-memory recording + new file (if auto-store is on).
    fn start_stream(&mut self, id: u8, zero: &[[f32; CHANNELS]; 3]) {
        let n = self.cfg.active.iter().filter(|&&a| a).count();
        let rate = ROLL_RATES.iter().find(|r| r.1 == id).map_or(0.0, |r| r.0) * roll_rate_factor(n);
        let now = std::time::SystemTime::now();
        if let Ok(mut h) = self.history.lock() {
            h.stream = StreamRec {
                active: self.cfg.active,
                rate,
                cfg: Some(self.cfg.clone()),
                zero: *zero,
                started: Some(now),
                ..Default::default()
            };
        }
        self.rec
            .send(crate::rec::RecMsg::Start(crate::rec::RecHeader {
                started_unix: now
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0.0, |d| d.as_secs_f64()),
                rate,
                roll_id: id,
                active: self.cfg.active,
                vscale: std::array::from_fn(|c| self.cfg.vscale[c].id()),
                zero: *zero,
                ..Default::default() // calibration + probes: added by the writer from the UI's Meta
            }));
        self.recording = true;
    }

    fn status(&mut self, s: String) {
        self.status = s;
        self.input
            .input_buffer_mut()
            .status
            .clone_from(&self.status);
        self.input.publish();
        (self.repaint)();
    }

    /// Apply pending UI commands. Returns (config changed, mode changed); Err = UI gone.
    fn drain(&mut self, block: Duration) -> std::result::Result<(bool, bool), ()> {
        let (mut cfg_changed, mut mode_changed) = (false, false);
        let mut first = if block.is_zero() {
            match self.rx.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return Err(()),
            }
        } else {
            match self.rx.recv_timeout(block) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return Err(()),
            }
        };
        while let Some(c) = first.take().or_else(|| self.rx.try_recv().ok()) {
            match c {
                Cmd::Config(c) => {
                    cfg_changed |= c != self.cfg;
                    self.cfg = c;
                }
                Cmd::Run(r) => self.run = r,
                Cmd::Keep(k) => self.keep = k.clamp(16, 500_000),
                Cmd::Mode(m) => {
                    mode_changed |= m != self.mode;
                    self.mode = m;
                }
            }
        }
        Ok((cfg_changed, mode_changed))
    }

    fn main(mut self, sim: bool) {
        loop {
            // connect (retry until the scope shows up)
            let mut src = if sim {
                Src::Sim(Sim::default())
            } else {
                match Hantek::open(Timing::fast()).and_then(|mut h| h.init(&self.cfg).map(|_| h)) {
                    Ok(h) => Src::Dev(h),
                    Err(e) => {
                        self.status(format!("waiting for scope: {e}"));
                        if self.drain(Duration::from_millis(500)).is_err() {
                            return;
                        }
                        continue;
                    }
                }
            };
            self.status(if sim {
                "simulator".into()
            } else {
                "connected".into()
            });
            match self.session(&mut src) {
                Ok(()) => {
                    if let Src::Dev(h) = src {
                        h.close();
                    }
                    return; // UI closed
                }
                Err(e) => self.status(format!("error: {e} — reconnecting")),
            }
        }
    }

    /// Capture loop on a connected source. Ok = UI closed, Err = device problem.
    fn session(&mut self, src: &mut Src) -> Result<()> {
        let zero = src.zero();
        let mut f = Frame::default();
        let mut mode_started = false;
        let (mut n, mut t0) = (0u32, Instant::now());
        let mut cap_usb = Duration::ZERO; // USB time spent in captures only (not keep-alive pings)
        let (mut fps, mut usb_ms) = (0f32, 0f32);
        let mut last_pub = Instant::now();
        src.configure(&self.cfg)?;
        loop {
            let block = if self.run == Run::Stopped {
                Duration::from_millis(10)
            } else {
                Duration::ZERO
            };
            let Ok((cfg_changed, mode_changed)) = self.drain(block) else {
                return Ok(());
            };
            if cfg_changed || mode_changed {
                src.configure(&self.cfg)?;
            }
            // configure() re-sends the burst timebase, so roll mode must be restarted after it
            let roll_restart = cfg_changed && matches!(self.mode, Mode::Roll(_));
            if mode_changed || !mode_started || roll_restart {
                f.clear();
                if self.recording {
                    self.rec.send(crate::rec::RecMsg::Stop);
                    self.recording = false;
                }
                self.stream_new = None;
                if let Mode::Roll(id) = self.mode {
                    src.roll_start(id)?;
                    self.stream_new = Some(id);
                }
                mode_started = true;
            }
            if self.run == Run::Stopped {
                src.ping()?; // keep-alive
                (n, t0, cap_usb) = (0, Instant::now(), Duration::ZERO); // stats restart on resume
                continue;
            }
            let usb_before = src.usb();
            let forced_before = src.forced();
            let fresh = match self.mode {
                Mode::Burst => {
                    let t_cap = Instant::now();
                    let polls0 = src.polls();
                    let got = src.capture(&mut f)?;
                    let ms = t_cap.elapsed().as_secs_f32() * 1e3;
                    if ms > 150.0 {
                        log::debug!(
                            "acq: slow capture {ms:.0} ms ({} ready polls, forced {})",
                            src.polls() - polls0,
                            src.forced() - forced_before
                        );
                    }
                    if got && self.run == Run::Single {
                        self.run = Run::Stopped;
                    }
                    got
                }
                Mode::Roll(_) => {
                    let rows = src.roll_poll(&mut f)?;
                    if rows > 0 {
                        if let Some(id) = self.stream_new.take() {
                            self.start_stream(id, &zero);
                        }
                        if let Ok(mut h) = self.history.lock() {
                            h.append_stream(&f, rows);
                        }
                        // to the disk writer (outside the lock; never blocks)
                        let mut r: [Vec<u16>; CHANNELS] = Default::default();
                        for ch in 0..CHANNELS {
                            if self.cfg.active[ch] {
                                let d = &f.data[ch];
                                r[ch] = d[d.len() - rows.min(d.len())..].to_vec();
                            }
                        }
                        self.rec.send(crate::rec::RecMsg::Rows(r));
                    }
                    // trim only when 25 % over: draining the front moves the whole buffer
                    for d in f.data.iter_mut() {
                        if d.len() > self.keep + self.keep / 4 {
                            d.drain(..d.len() - self.keep);
                        }
                    }
                    rows > 0
                }
            };
            cap_usb += src.usb() - usb_before;
            if !fresh {
                continue;
            }
            // stream mode polls ~220×/s: publish (= copy the buffer) at most ~60×/s, the UI can't show more
            if matches!(self.mode, Mode::Roll(_)) && last_pub.elapsed() < Duration::from_millis(16)
            {
                continue;
            }
            last_pub = Instant::now();
            n += 1;
            let dt = t0.elapsed();
            if dt >= Duration::from_millis(500) || self.run == Run::Stopped {
                fps = if self.run == Run::Stopped {
                    0.0
                } else {
                    n as f32 / dt.as_secs_f32()
                };
                usb_ms = cap_usb.as_secs_f32() * 1e3 / n as f32;
                (n, t0, cap_usb) = (0, Instant::now(), Duration::ZERO);
            }
            let s = self.input.input_buffer_mut();
            s.frame.clone_from(&f);
            s.cfg = Some(self.cfg.clone());
            s.zero = zero;
            s.roll = matches!(self.mode, Mode::Roll(_));
            s.acq_fps = fps;
            s.usb_ms = usb_ms;
            s.status.clone_from(&self.status); // every one of the 3 buffers needs it
            s.forced = src.forced() > forced_before;
            s.stopped = self.run == Run::Stopped;
            // trigger jitter: find the real crossing near the nominal point (37 ns; only real triggers)
            s.trig_off = (!s.roll && !s.forced && self.cfg.trig_mode != TrigMode::Free)
                .then(|| {
                    let d = &f.data[self.cfg.trig_src as usize];
                    let expect = self.cfg.trig_pos as f64 / 1000.0 * d.len() as f64;
                    trigger_offset(
                        d,
                        expect,
                        self.cfg.trig_level,
                        self.cfg.trig_slope == Slope::Rising,
                        12,
                    )
                })
                .flatten();
            // every triggered capture also goes into the history (the triple buffer may drop frames)
            let entry = (!s.roll).then(|| HEntry {
                snap: s.clone(),
                at: std::time::SystemTime::now(),
            });
            self.input.publish();
            (self.repaint)();
            if let Some(e) = entry {
                if let Ok(mut h) = self.history.lock() {
                    h.push(e);
                }
            }
        }
    }
}

impl Src {
    fn configure(&mut self, c: &Config) -> Result<()> {
        match self {
            Src::Dev(h) => h.configure(c),
            Src::Sim(s) => {
                s.cfg = c.clone();
                Ok(())
            }
        }
    }
    fn capture(&mut self, f: &mut Frame) -> Result<bool> {
        match self {
            Src::Dev(h) => h.capture(f),
            Src::Sim(s) => {
                s.capture(f);
                Ok(true)
            }
        }
    }
    fn roll_start(&mut self, id: u8) -> Result<()> {
        match self {
            Src::Dev(h) => h.roll_start(id),
            Src::Sim(s) => {
                let n = s.cfg.active.iter().filter(|&&a| a).count();
                s.rate = ROLL_RATES.iter().find(|r| r.1 == id).map_or(440.0, |r| r.0)
                    * roll_rate_factor(n);
                s.last = None;
                Ok(())
            }
        }
    }
    fn roll_poll(&mut self, f: &mut Frame) -> Result<usize> {
        match self {
            Src::Dev(h) => h.roll_poll(f),
            Src::Sim(s) => Ok(s.roll(f)),
        }
    }
    fn ping(&mut self) -> Result<()> {
        match self {
            Src::Dev(h) => h.ping(),
            Src::Sim(_) => Ok(()),
        }
    }
    fn zero(&self) -> [[f32; CHANNELS]; 3] {
        match self {
            Src::Dev(h) => h.zero,
            Src::Sim(_) => [[2048.0; CHANNELS]; 3],
        }
    }
    fn polls(&self) -> u64 {
        match self {
            Src::Dev(h) => h.stats.ready_polls,
            Src::Sim(_) => 0,
        }
    }
    fn forced(&self) -> u64 {
        match self {
            Src::Dev(h) => h.stats.forced,
            Src::Sim(_) => 0,
        }
    }
    fn usb(&self) -> Duration {
        match self {
            Src::Dev(h) => h.stats.usb,
            Src::Sim(_) => Duration::ZERO,
        }
    }
}

/// Fake 1008C: 4000 samples per frame shared by the active channels, ~26 ms per frame like the real one.
#[derive(Default)]
struct Sim {
    cfg: Config,
    t: f64,
    /// stream: rows per second (as the real scope: nominal × channel factor) and last poll time
    rate: f64,
    last: Option<Instant>,
}

/// Simulated low-side injector on its switched wire: 13.8 V, open 3 ms every 20 ms, 70 V flyback, pintle bump.
fn injector_v(t: f64) -> f64 {
    let t = t % 0.02;
    match t {
        t if (0.002..0.005).contains(&t) => 0.4,
        t if (0.005..0.0052).contains(&t) => 70.0,
        t if (0.0052..0.008).contains(&t) => {
            let u = t - 0.0052;
            13.8 + 4.0 * (-u / 0.0003).exp()
                + if (0.0006..0.0008).contains(&u) {
                    1.2
                } else {
                    0.0
                }
        }
        _ => 13.8,
    }
}

/// Simulated ignition coil primary: dwell 3 ms, 350 V firing spike, 1.5 ms burn at 40 V, ringing; every 20 ms.
fn ignition_primary(t: f64) -> f64 {
    let t = t % 0.02;
    match t {
        t if (0.005..0.008).contains(&t) => 0.8,
        t if (0.008..0.0081).contains(&t) => 350.0,
        t if (0.0081..0.0096).contains(&t) => 40.0,
        t if (0.0096..0.0111).contains(&t) => {
            13.8 + 12.0
                * ((t - 0.0096) * 2.0 * std::f64::consts::PI * 3000.0).sin()
                * (-(t - 0.0096) / 0.0006).exp()
        }
        _ => 13.8,
    }
}

/// Simulated CAN-H at 100 kbit/s: one frame (ID 0x2C0, 01 02 03 04) every 5 ms (a real bus is mostly idle). True = dominant.
fn can_dominant(t: f64) -> bool {
    static BITS: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let bits = BITS.get_or_init(|| crate::dsp::can_frame_bits(0x2C0, &[0x01, 0x02, 0x03, 0x04]));
    let k = ((t % 0.005) * 100_000.0) as usize;
    bits.get(k).is_some_and(|&b| b == 0)
}

/// Simulated LIN bus level at time t (true = recessive/high).
fn lin_level(t: f64) -> bool {
    let bit = ((t % 0.01) * 19_200.0) as usize; // bit index within the 10 ms frame period
                                                // 1 idle, 14 break (low), 1 delimiter, then bytes of 10 bits (start, 8 data LSB first, stop) + 1 idle each
    let bytes = [0x55u8, 0x3C, 0x12, 0x34, !(0x3Cu16 + 0x12 + 0x34) as u8];
    match bit {
        0 => true,
        1..=14 => false,
        15 => true,
        n => {
            let k = n - 16;
            let (byte, pos) = (k / 11, k % 11);
            match (bytes.get(byte), pos) {
                (None, _) => true,
                (_, 0) => false,
                (Some(b), 1..=8) => (b >> (pos - 1)) & 1 == 1,
                _ => true,
            }
        }
    }
}

impl Sim {
    /// Slow automotive signals for the stream (roll) mode at real time `t`: CH2 lambda, CH4 battery through an engine
    /// start, CH5 starter current (clamp 10 mV/A) with one weak cylinder. None = the burst signal, slowed 1000×.
    fn slow(ch: usize, t: f64) -> Option<f64> {
        let pi2 = std::f64::consts::TAU;
        match ch {
            // narrowband lambda: rich 0.85 V / lean 0.12 V, ~1.2 switches per second, finite slope
            1 => {
                let ph = (t * 0.6).fract();
                let x = ((ph * pi2).sin() * 6.0).tanh();
                Some(0.485 + 0.365 * x)
            }
            // battery: 4 s rest 12.6 V, 2 s cranking ≈ 10.4 V with compression ripple, then 14.2 V charging; 14 s cycle
            3 => {
                let c = t % 14.0;
                Some(if c < 4.0 {
                    12.6
                } else if c < 6.0 {
                    10.4 + 0.4 * ((c - 4.0) * 8.0 * pi2).sin()
                } else {
                    14.2 - 0.6 * (-(c - 6.0) / 0.5).exp()
                })
            }
            // starter current: 4 compression humps per second, the 3rd cylinder clearly weaker; 0 A between 6 s crank bursts
            4 => {
                let c = t % 8.0;
                if c >= 6.0 {
                    return Some(0.0);
                }
                let hump = (c * 4.0) as usize % 4;
                let k = if hump == 2 { 0.45 } else { 1.0 };
                let amps =
                    120.0 + 80.0 * k * (((c * 4.0).fract() * pi2 - pi2 / 4.0).sin() * 0.5 + 0.5);
                Some(amps * 0.01)
            }
            _ => None,
        }
    }

    fn sample(&self, ch: usize, t: f64) -> u16 {
        let vs = [0.02, 0.125, 1.0][self.cfg.vscale[ch].index()];
        // debug aid: HANTEK_SIM_OPEN=1,3 makes CH1 and CH3 an open input (nothing connected): a few counts of noise
        static OPEN: std::sync::OnceLock<[bool; CHANNELS]> = std::sync::OnceLock::new();
        let open = OPEN.get_or_init(|| {
            let mut m = [false; CHANNELS];
            for c in std::env::var("HANTEK_SIM_OPEN")
                .unwrap_or_default()
                .split(',')
                .filter_map(|c| c.trim().parse::<usize>().ok())
            {
                if (1..=CHANNELS).contains(&c) {
                    m[c - 1] = true;
                }
            }
            m
        });
        if open[ch] {
            let n = ((t * 1e7) as u64).wrapping_mul(2654435761) % 6;
            return (2048.0 - 0.033 / (0.01 * vs) + n as f64 - 3.0).clamp(0.0, 4095.0) as u16;
        }
        // CH8: a LIN bus (12 V idle, 19.2 kbit/s), one frame every 10 ms: break, sync, ID 0x3C, 0x12 0x34, checksum
        let volts = if ch == 7 {
            if lin_level(t) {
                12.0
            } else {
                0.5
            }
        } else if ch == 6 {
            if can_dominant(t) {
                3.5
            } else {
                2.5
            } // CAN-H
        } else if ch == 4 {
            ignition_primary(t) / 20.0 // seen through a ×20 attenuator
        } else if ch == 5 {
            injector_v(t) / 20.0 // seen through a ×20 attenuator
        } else {
            match ch % 4 {
                0 => 5.0 * (t * 2.0 * std::f64::consts::PI * 1000.0).sin(), // 1 kHz sine, 5 V
                // ignition trigger (ECU → igniter / coil driver): 5 V during the dwell of the CH5 coil
                1 => {
                    if (0.005..0.008).contains(&(t % 0.02)) {
                        5.0
                    } else {
                        0.0
                    }
                }
                2 => {
                    // crank sensor 60-2 at 1000 rpm (1 kHz teeth, one revolution per 60 ms; realistic for idle / fast idle)
                    let tooth = (t * 1000.0).fract();
                    if (t * 1000.0 / 60.0).fract() < 2.0 / 60.0 {
                        0.0
                    } else {
                        2.0 * (tooth * 2.0 * std::f64::consts::PI).sin()
                    }
                }
                // cam Hall: one window per 2 crank revolutions (120 ms), high for 180° cam, starting 10 teeth after the gap
                _ => {
                    if ((t - 0.010) / 0.120).rem_euclid(1.0) < 0.25 {
                        5.0
                    } else {
                        0.1
                    }
                }
            }
        };
        (2048.0 + volts / (0.01 * vs)).clamp(0.0, 4095.0) as u16
    }

    fn capture(&mut self, f: &mut Frame) {
        thread::sleep(Duration::from_millis(26));
        let n = self.cfg.active.iter().filter(|&&a| a).count().max(1);
        let per = 4000 / n;
        // the real scope captures capture_seconds() (≥ 1668 µs window), not just the 10 visible divisions
        let dt = capture_seconds(self.cfg.timebase_id) / per as f64;
        f.clear();
        f.active = self.cfg.active;
        f.seq += 1;
        let start = match self.find_trigger(dt) {
            Some(tt) => tt - self.cfg.trig_pos as f64 / 1000.0 * per as f64 * dt,
            None => self.t, // free run / Auto timeout
        };
        for ch in 0..CHANNELS {
            if self.cfg.active[ch] {
                for i in 0..per {
                    let v = self.sample(ch, start + i as f64 * dt);
                    f.data[ch].push(v);
                }
            }
        }
        self.t = start + per as f64 * dt + 0.0137; // re-arm a little later (USB readout gap)
    }

    /// Like the hardware trigger: the first crossing of the trigger level on the source channel within 100 ms of
    /// signal time (Normal mode waits up to 1 s). None in free-run mode or without a crossing.
    fn find_trigger(&self, dt: f64) -> Option<f64> {
        let c = &self.cfg;
        if c.trig_mode == TrigMode::Free {
            return None;
        }
        let (src, lvl, rising) = (
            c.trig_src as usize,
            c.trig_level,
            c.trig_slope == Slope::Rising,
        );
        let limit = if c.trig_mode == TrigMode::Normal {
            1.0
        } else {
            0.1
        };
        let n = ((limit / dt) as usize).min(2_000_000);
        let mut prev = self.sample(src, self.t);
        for k in 1..n {
            let t = self.t + k as f64 * dt;
            let v = self.sample(src, t);
            if (rising && prev < lvl && v >= lvl) || (!rising && prev > lvl && v <= lvl) {
                return Some(t);
            }
            prev = v;
        }
        None
    }

    /// Stream like the real scope: `rate` rows per second (real time); the signal is slowed 1000× so
    /// a "1 kHz" sine is 1 Hz on the stream.
    fn roll(&mut self, f: &mut Frame) -> usize {
        thread::sleep(Duration::from_micros(4500));
        f.active = self.cfg.active;
        let now = Instant::now();
        let last = *self.last.get_or_insert(now);
        let rate = if self.rate > 0.0 { self.rate } else { 440.0 };
        let rows = (now.duration_since(last).as_secs_f64() * rate) as usize;
        if rows == 0 {
            return 0;
        }
        self.last = Some(last + Duration::from_secs_f64(rows as f64 / rate));
        for _ in 0..rows {
            self.t += 1.0 / rate;
            for ch in 0..CHANNELS {
                if self.cfg.active[ch] {
                    let v = match Self::slow(ch, self.t) {
                        Some(volts) => (2048.0
                            + volts / (0.01 * [0.02, 0.125, 1.0][self.cfg.vscale[ch].index()]))
                        .clamp(0.0, 4095.0) as u16,
                        None => self.sample(ch, self.t * 0.001),
                    };
                    f.data[ch].push(v);
                }
            }
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(samples: usize) -> HEntry {
        let mut s = Snapshot::default();
        s.frame.data[0] = vec![2048; samples];
        HEntry {
            snap: s,
            at: std::time::SystemTime::now(),
        }
    }

    #[test]
    fn stream_rec_appends_new_rows_and_respects_budget() {
        let mut rec = StreamRec {
            active: [true, false, false, false, false, false, false, false],
            ..Default::default()
        };
        let mut f = Frame::default();
        f.data[0] = (0..100u16).collect();
        f.data[1] = vec![7; 100]; // inactive in the recording → ignored
        rec.append(&f, 30); // the last 30 rows are new
        assert_eq!(rec.data[0], (70..100u16).collect::<Vec<_>>());
        assert!(rec.data[1].is_empty());
        f.data[0].extend(100..110u16);
        rec.append(&f, 10);
        assert_eq!(rec.rows(), 40);
        assert_eq!(*rec.data[0].last().unwrap(), 109);
        rec.drop_oldest(); // oldest 10 %
        assert_eq!(rec.rows(), 36);
        assert_eq!(rec.dropped, 4);
        assert_eq!(rec.data[0][0], 74); // row numbers stay absolute: dropped + index
    }

    #[test]
    fn history_numbers_and_budget() {
        let one = History::cost(&entry(1000));
        let mut h = History::new(one * 3 + one / 2); // room for 3
        assert!(h.last_no().is_none());
        for _ in 0..5 {
            h.push(entry(1000));
        }
        assert_eq!(h.len(), 3);
        assert_eq!(h.first_no, 2); // #0 and #1 dropped
        assert_eq!(h.last_no(), Some(4));
        assert!(h.get(1).is_none());
        assert!(h.get(2).is_some());
        assert!(h.get(5).is_none());
    }

    #[test]
    fn shared_limit_newest_kind_wins() {
        let one = History::cost(&entry(1000));
        let mut h = History::new(one * 4);
        for _ in 0..3 {
            h.push(entry(1000));
        }
        h.stream.active[0] = true;
        let mut f = Frame::default();
        f.data[0] = vec![1; 3000]; // ≈ 6 kB, more than the free room
        h.append_stream(&f, 3000);
        assert!(h.used_bytes() <= h.budget);
        assert!(h.len() < 3, "captures make room for the stream first");
        assert_eq!(h.stream.rows(), 3000);
        // a new capture now drops the (older) stream recording first
        h.push(entry(1000));
        assert_eq!(h.stream.rows(), 0);
        assert!(h.used_bytes() <= h.budget);
        h.set_budget(one * 2 + one / 2);
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn sim_stream_is_recorded_to_disk() {
        let dir = std::env::temp_dir().join(format!("h8r-acq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = Config::default();
        cfg.active = [true, true, false, false, false, false, false, false];
        let acq = spawn(true, cfg.clone(), || {});
        acq.rec
            .send(crate::rec::RecMsg::Limits(crate::rec::RecLimits {
                dir: dir.clone(),
                auto: true,
                max_bytes: 1 << 30,
                keep_free: 0,
            }));
        acq.tx.send(Cmd::Config(cfg)).unwrap();
        acq.tx.send(Cmd::Mode(Mode::Roll(ROLL_RATES[0].1))).unwrap();
        std::thread::sleep(Duration::from_millis(3500));
        acq.tx.send(Cmd::Mode(Mode::Burst)).unwrap(); // ends the stream → file closed
        std::thread::sleep(Duration::from_millis(500));
        let mem_rows = acq.history.lock().unwrap().stream.rows();
        let files = crate::rec::list(&dir);
        assert_eq!(files.len(), 1, "{files:?}");
        let r = crate::rec::read(&files[0].0, None).unwrap();
        assert!(!r.truncated);
        assert!(r.data[0].len() > 500, "rows on disk {}", r.data[0].len());
        assert_eq!(r.data[0].len(), mem_rows, "disk = memory");
        assert_eq!(r.data[0], acq.history.lock().unwrap().stream.data[0]);
        assert!(r.data[2].is_empty());
        assert_eq!(
            acq.rec
                .shared
                .lost
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn sim_lin_bus_decodes() {
        let dt = 1.0 / 300_000.0;
        let x: Vec<f32> = (0..3000)
            .map(|i| if lin_level(i as f64 * dt) { 12.0 } else { 0.5 })
            .collect();
        let items: Vec<crate::dsp::SerialItem> = crate::dsp::uart_decode(&x, dt, 19_200.0)
            .into_iter()
            .map(|i| i.2)
            .collect();
        assert_eq!(items[0], crate::dsp::SerialItem::Break);
        assert_eq!(items[1], crate::dsp::SerialItem::Byte(0x55), "{items:?}");
        assert_eq!(items[2], crate::dsp::SerialItem::Byte(0x3C));
    }

    #[test]
    fn sim_can_bus_decodes_in_a_capture_window() {
        let dt = capture_seconds(burst_id_for_ns(100_000).unwrap()) / 500.0;
        let mut found = 0;
        for k in 0..15 {
            let t0 = 0.0042 + k as f64 * 0.00005; // windows starting in the idle before the frame at 5 ms
            let x: Vec<f32> = (0..500)
                .map(|i| {
                    if can_dominant(t0 + i as f64 * dt) {
                        3.5
                    } else {
                        2.5
                    }
                })
                .collect();
            let f = crate::dsp::can_decode(&x, dt, 100_000.0, true);
            found += f
                .iter()
                .filter(|x| x.2.as_ref().is_some_and(|fr| fr.id == 0x2C0 && fr.crc_ok))
                .count();
            if k < 3 {
                eprintln!(
                    "t0 {t0}: {:?}",
                    f.iter()
                        .map(|x| (x.0, x.2.as_ref().map(|f| (f.id, f.crc_ok))))
                        .collect::<Vec<_>>()
                );
            }
        }
        assert!(found >= 5, "found {found}");
    }
}
