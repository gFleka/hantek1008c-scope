//! Hantek 1008C: types, sample decoder and USB driver (port of hantek1008py, Apache-2.0).

pub const CHANNELS: usize = 8;

/// Hardware input range. Ids sent in `a2` are 1..=3.
/// Factors from hantek1008py (0.02 / 0.125 / 1.0); real volts/div still to be calibrated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VScale {
    X0_02 = 1,
    X0_125 = 2,
    X1 = 3,
}

impl VScale {
    pub const ALL: [VScale; 3] = [VScale::X0_02, VScale::X0_125, VScale::X1];
    pub fn id(self) -> u8 {
        self as u8
    }
    pub fn index(self) -> usize {
        self as usize - 1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slope {
    Rising = 0,
    Falling = 1,
}

/// Burst-mode timebase id (`a3`): ns/div = {1,2,5}[id % 3] * 10^(id / 3), id 0..=25.
pub fn burst_ns_per_div(id: u8) -> u64 {
    [1u64, 2, 5][(id % 3) as usize] * 10u64.pow((id / 3) as u32)
}

pub fn burst_id_for_ns(ns: u64) -> Option<u8> {
    (0..26u8).find(|&id| burst_ns_per_div(id) == ns)
}

/// Roll-mode rates in samples/s/channel → `a3` id.
pub const ROLL_RATES: [(f64, u8); 13] = [
    (440.0, 0x18),
    (220.0, 0x19),
    (88.0, 0x1a),
    (44.0, 0x1b),
    (22.0, 0x1c),
    (11.0, 0x1d),
    (5.0, 0x1e),
    (2.0, 0x1f),
    (1.0, 0x20),
    (0.5, 0x21),
    (0.25, 0x22),
    (0.125, 0x23),
    (1.0 / 16.0, 0x24),
];

/// Acquisition settings. Compared field-by-field so only changed settings hit the wire.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub active: [bool; CHANNELS],
    pub vscale: [VScale; CHANNELS],
    pub timebase_id: u8,
    pub trig_src: u8,
    pub trig_slope: Slope,
    /// 12-bit raw trigger level
    pub trig_level: u16,
    pub trig_mode: TrigMode,
    /// trigger position on screen in 0.1 % (0 = left edge, 500 = centre, 1000 = right edge)
    pub trig_pos: u16,
}

/// How a capture ends (vendor capture 2026-11-20: `c2` forces a capture; `a5` 0 = armed/waiting,
/// 1 = triggered/filling, 2 = done).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrigMode {
    /// force immediately (`c2` right after arming) — free-running, the old behaviour
    Free,
    /// wait for the trigger, force after a timeout (vendor "Auto")
    Auto,
    /// wait for the trigger only (vendor "Normal"); no frame if nothing triggers
    Normal,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            active: [true; CHANNELS],
            vscale: [VScale::X1; CHANNELS],
            timebase_id: burst_id_for_ns(500_000).unwrap(), // vendor SW default 500 µs/div
            trig_src: 0,
            trig_slope: Slope::Rising,
            trig_level: 2048,
            trig_mode: TrigMode::Auto,
            trig_pos: 500,
        }
    }
}

/// Capture window in µs for a burst timebase, as the vendor software sends it in `ac`:
/// 10 div + 2 µs, but never below 1668 µs (4000 samples at the 2.4 MS/s maximum).
pub fn window_us(timebase_id: u8) -> u32 {
    let us = (burst_ns_per_div(timebase_id) * 10 / 1000).min(u32::MAX as u64 - 2) as u32;
    (us + 2).max(1668)
}

/// Real duration of an 8-channel burst capture in seconds. ≤ 100 µs/div the scope is capped at
/// 2.4 MS/s, so a capture always spans the 1668 µs minimum window; at 5/10/20 ms/div it samples
/// measurably slower than nominal (`probe --rates`, 8 ch: 0.9375 / 0.926 / 0.893).
pub fn capture_seconds(timebase_id: u8) -> f64 {
    let ratio = match burst_ns_per_div(timebase_id) {
        5_000_000 => 0.9375,
        10_000_000 => 0.926,
        20_000_000 => 0.893,
        _ => 1.0,
    };
    window_us(timebase_id) as f64 * 1e-6 / ratio
}

/// Channel skew: the active channels are converted one after another by one ADC at 2.4 MS/s, back to
/// back, at every timebase (measured CH2−CH1 ≈ 0.42 µs at 10 µs…2 ms/div, 2026-11-20). The n-th active
/// channel in scan order is sampled n × this later than the first.
pub const CHANNEL_SKEW_S: f64 = 1.0 / 2.4e6;

/// Timebases whose 8-channel time axis has been verified with the 1 kHz generator (`check`/`probe --rates`).
pub fn timing_verified(timebase_id: u8) -> bool {
    let ns = burst_ns_per_div(timebase_id);
    (1_000..=100_000).contains(&ns)
        || matches!(
            ns,
            500_000 | 1_000_000 | 2_000_000 | 5_000_000 | 10_000_000 | 20_000_000
        )
}

/// Burst timebases where capturing fewer than 8 channels is proven fully correct on this unit.
/// Currently none: 500 µs/div has exact rate and no hole/seam at 1/2/4 ch, but the trigger point is
/// offset (−352…+192 samples at 1 ch) and with the periodic 1 kHz test signal its true placement can't
/// be measured (only its phase). Re-enable after measuring with a one-off edge (generator pattern).
/// Everywhere else the firmware's <8-channel path is faulty (hole, seam). → capture all 8, show enabled.
pub fn partial_capture_ok(_timebase_id: u8) -> bool {
    false
}

/// `ac` payload: trigger position (‰ of screen), pre-trigger µs, post-trigger µs.
pub fn ac_payload(pos: u16, window: u32) -> [u8; 9] {
    let pos = pos.min(1000);
    let pre = (window as u64 - 2) * pos as u64 / 1000 + 1;
    let post = window as u64 - pre;
    let (a, b, c) = (
        pos.to_be_bytes(),
        (pre as u32).to_be_bytes(),
        (post as u32).to_be_bytes(),
    );
    [0xac, a[0], a[1], b[1], b[2], b[3], c[1], c[2], c[3]]
}

/// One capture. Buffers are reused between frames (no per-frame allocation once warm).
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub data: [Vec<u16>; CHANNELS],
    pub active: [bool; CHANNELS],
    pub seq: u64,
}

impl Frame {
    pub fn clear(&mut self) {
        for d in &mut self.data {
            d.clear();
        }
    }
}

/// De-interleave little-endian u16 samples into per-channel buffers (appending).
/// `stride` is the number of interleaved columns (active count, +1 in roll mode for the
/// extra 9th column, which is dropped). Trailing partial rows are ignored.
/// Returns the number of rows decoded.
pub fn decode_append(
    bytes: &[u8],
    active: &[bool; CHANNELS],
    stride: usize,
    out: &mut [Vec<u16>; CHANNELS],
) -> usize {
    let mut cols = [usize::MAX; CHANNELS]; // column index -> channel
    let mut n = 0;
    for (ch, &a) in active.iter().enumerate() {
        if a {
            cols[n] = ch;
            n += 1;
        }
    }
    debug_assert!(stride >= n && stride > 0);
    let row_bytes = stride * 2;
    let rows = bytes.len() / row_bytes;
    for &ch in &cols[..n] {
        out[ch].reserve(rows);
    }
    for row in bytes.chunks_exact(row_bytes) {
        for (i, &ch) in cols[..n].iter().enumerate() {
            out[ch].push(u16::from_le_bytes([row[2 * i], row[2 * i + 1]]));
        }
    }
    rows
}

// ───────────────────────────── measurements ─────────────────────────────

/// Per-app config folder: Linux $XDG_CONFIG_HOME or ~/.config, Windows %APPDATA% → …/hantek1008c.
pub fn config_dir() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("hantek1008c"))
}

/// Gain correction per channel and hardware range (1.0 = uncalibrated), from `hantek1008c calibrate`.
/// Calibrated volts per count = volts_per_count(range) × gain[ch][range]. Stored in calibration.txt as
/// `ch=1 range=X1 gain=1.02920` lines. (Zero offsets are measured live at every start-up.)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cal {
    pub gain: [[f32; 3]; CHANNELS],
}

impl Default for Cal {
    fn default() -> Self {
        Cal {
            gain: [[1.0; 3]; CHANNELS],
        }
    }
}

impl Cal {
    pub fn path() -> Option<std::path::PathBuf> {
        config_dir().map(|d| d.join("calibration.txt"))
    }
    pub fn load() -> Cal {
        let mut c = Cal::default();
        let Some(txt) = Cal::path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return c;
        };
        for line in txt.lines() {
            let (mut ch, mut rg, mut g) = (None, None, None);
            for kv in line.split_whitespace() {
                match kv.split_once('=') {
                    Some(("ch", v)) => {
                        ch = v
                            .parse::<usize>()
                            .ok()
                            .filter(|&x| (1..=CHANNELS).contains(&x))
                    }
                    Some(("range", v)) => {
                        rg = VScale::ALL.into_iter().find(|s| format!("{s:?}") == v)
                    }
                    Some(("gain", v)) => {
                        g = v.parse::<f32>().ok().filter(|x| (0.5..2.0).contains(x))
                    }
                    _ => {}
                }
            }
            if let (Some(ch), Some(rg), Some(g)) = (ch, rg, g) {
                c.gain[ch - 1][rg.index()] = g;
            }
        }
        c
    }
    pub fn save(&self) -> std::io::Result<std::path::PathBuf> {
        let path = Cal::path().ok_or_else(|| std::io::Error::other("no config folder"))?;
        let mut txt =
            String::from("# hantek1008c gain calibration: calibrated V/count = nominal × gain\n");
        for ch in 0..CHANNELS {
            for vs in VScale::ALL {
                let g = self.gain[ch][vs.index()];
                if g != 1.0 {
                    txt += &format!("ch={} range={vs:?} gain={g:.5}\n", ch + 1);
                }
            }
        }
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&path, txt)?;
        Ok(path)
    }
    /// Calibrated volts per raw count.
    pub fn vpc(&self, ch: usize, vs: VScale) -> f32 {
        volts_per_count(vs) * self.gain[ch][vs.index()]
    }
}

/// Roll mode: real rate = nominal × this factor for `n` active channels (hantek1008py; confirmed
/// on hardware for 1 ch: 1992 rows/s at nominal 440).
pub fn roll_rate_factor(n: usize) -> f64 {
    [4.56, 3.03, 2.27, 1.82, 1.51, 1.3, 1.14, 1.00][n.clamp(1, CHANNELS) - 1]
}

/// Volts per raw count for range factor 0.02 / 0.125 / 1.0 (from hantek1008py, uncalibrated).
pub fn volts_per_count(vs: VScale) -> f32 {
    0.01 * [0.02, 0.125, 1.0][vs.index()]
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Meas {
    pub min: f32,
    pub max: f32,
    /// mean of the samples above / below mid-level (flat top / base of a pulse)
    pub top: f32,
    pub base: f32,
    /// mean and RMS of the whole capture (volts, DC included)
    pub mean: f32,
    pub rms: f32,
    /// time-based values (None without timing or without enough edges)
    pub freq: Option<f64>,
    pub period: Option<f64>,
    /// fraction of the period above mid-level (0…1)
    pub duty: Option<f64>,
    /// mean high / low pulse width (s)
    pub width_pos: Option<f64>,
    pub width_neg: Option<f64>,
    /// mean 10–90 % rise / 90–10 % fall time (s)
    pub rise: Option<f64>,
    pub fall: Option<f64>,
}

/// All measurements of one channel's capture, three passes over the raw samples.
/// Edges: mid-level crossings with 10 % p-p hysteresis, positions interpolated between samples.
pub fn measure(d: &[u16], zero: f32, vpc: f32, dt: Option<f64>) -> Meas {
    let mut m = Meas::default();
    if d.is_empty() {
        return m;
    }
    let to_v = |c: f32| (c - zero) * vpc;
    // pass 1: extremes, mean, RMS
    let (mut lo, mut hi, mut sum, mut sum2) = (u16::MAX, 0u16, 0f64, 0f64);
    for &v in d {
        lo = lo.min(v);
        hi = hi.max(v);
        let x = to_v(v as f32) as f64;
        sum += x;
        sum2 += x * x;
    }
    let n = d.len() as f64;
    m.min = to_v(lo as f32);
    m.max = to_v(hi as f32);
    m.mean = (sum / n) as f32;
    m.rms = (sum2 / n).sqrt() as f32;
    // pass 2: plateaus (top/base)
    let mid = (hi as f32 + lo as f32) / 2.0;
    let (mut st, mut nt, mut sb, mut nb) = (0f64, 0u32, 0f64, 0u32);
    for &v in d {
        if v as f32 > mid {
            st += v as f64;
            nt += 1;
        } else {
            sb += v as f64;
            nb += 1;
        }
    }
    let top_c = if nt > 0 {
        (st / nt as f64) as f32
    } else {
        hi as f32
    };
    let base_c = if nb > 0 {
        (sb / nb as f64) as f32
    } else {
        lo as f32
    };
    m.top = to_v(top_c);
    m.base = to_v(base_c);
    let span = (hi - lo) as f32;
    let Some(dt) = dt else { return m };
    if span < 8.0 {
        return m;
    }
    // pass 3: edges (interpolated mid crossings) + 10/90 % transition times
    let (on, off) = (mid + 0.1 * span, mid - 0.1 * span);
    let (l10, l90) = (
        base_c + 0.1 * (top_c - base_c),
        base_c + 0.9 * (top_c - base_c),
    );
    let cross = |i: usize, level: f32| -> f64 {
        // fractional index where the signal crosses `level` between i-1 and i
        let (a, b) = (d[i - 1] as f32, d[i] as f32);
        if (b - a).abs() < f32::EPSILON {
            i as f64
        } else {
            (i - 1) as f64 + ((level - a) / (b - a)).clamp(0.0, 1.0) as f64
        }
    };
    let mut rises: Vec<f64> = Vec::new();
    let mut falls: Vec<f64> = Vec::new();
    let (mut rise_t, mut nr, mut fall_t, mut nf) = (0f64, 0u32, 0f64, 0u32);
    let mut state: i8 = 0; // 1 = high, -1 = low, 0 = unknown
    for i in 1..d.len() {
        let v = d[i] as f32;
        if state != 1 && v > on {
            if state == -1 {
                // locate the mid crossing walking back from here
                let mut j = i;
                while j > 1 && d[j - 1] as f32 > mid {
                    j -= 1;
                }
                rises.push(cross(j, mid));
                // 10 → 90 % time around this edge
                let (mut a, mut b) = (j, j);
                while a > 1 && d[a - 1] as f32 > l10 {
                    a -= 1;
                }
                while b < d.len() - 1 && (d[b] as f32) < l90 {
                    b += 1;
                }
                if a > 1 && b < d.len() - 1 && b > a {
                    rise_t += (cross(b, l90) - cross(a, l10)).max(0.0);
                    nr += 1;
                }
            }
            state = 1;
        } else if state != -1 && v < off {
            if state == 1 {
                let mut j = i;
                while j > 1 && (d[j - 1] as f32) < mid {
                    j -= 1;
                }
                falls.push(cross(j, mid));
                let (mut a, mut b) = (j, j);
                while a > 1 && (d[a - 1] as f32) < l90 {
                    a -= 1;
                }
                while b < d.len() - 1 && d[b] as f32 > l10 {
                    b += 1;
                }
                if a > 1 && b < d.len() - 1 && b > a {
                    fall_t += (cross(b, l10) - cross(a, l90)).max(0.0);
                    nf += 1;
                }
            }
            state = -1;
        }
    }
    if rises.len() >= 2 {
        let per = (rises[rises.len() - 1] - rises[0]) / (rises.len() - 1) as f64 * dt;
        m.period = Some(per);
        m.freq = Some(1.0 / per);
    }
    // pulse widths: rise → next fall (high), fall → next rise (low)
    let (mut wp, mut np, mut wn, mut nn) = (0f64, 0u32, 0f64, 0u32);
    // both lists are in time order: the first later edge by a pointer that only moves forward (was a search per
    // edge, O(edges²): 16.5 ms for one 401k-sample stream channel; speed review 2026-11-23)
    let mut j = 0;
    for &r in &rises {
        while j < falls.len() && falls[j] <= r {
            j += 1;
        }
        if let Some(&f) = falls.get(j) {
            wp += f - r;
            np += 1;
        }
    }
    let mut j = 0;
    for &f in &falls {
        while j < rises.len() && rises[j] <= f {
            j += 1;
        }
        if let Some(&r) = rises.get(j) {
            wn += r - f;
            nn += 1;
        }
    }
    if np > 0 {
        m.width_pos = Some(wp / np as f64 * dt);
    }
    if nn > 0 {
        m.width_neg = Some(wn / nn as f64 * dt);
    }
    if let (Some(p), Some(q)) = (m.width_pos, m.width_neg) {
        m.duty = Some(p / (p + q));
    }
    if nr > 0 {
        m.rise = Some(rise_t / nr as f64 * dt);
    }
    if nf > 0 {
        m.fall = Some(fall_t / nf as f64 * dt);
    }
    m
}

/// Sample index of the first mid-level crossing (10 % p-p hysteresis), rising or falling.
/// Used to check whether the trigger holds the edge at a fixed position.
/// Where the trigger crossing really is in a capture, relative to the nominal trigger sample `expect`
/// (= position × samples): the `level` crossing on the trigger edge nearest to `expect` within ±`search`
/// samples, interpolated between samples. The 1008C only knows its trigger point to a few samples (measured:
/// −1…−7 samples, random per capture — `re/PROTOCOL.md`), so the display shifts each capture by this.
/// ~25 comparisons: negligible next to a capture.
pub fn trigger_offset(
    d: &[u16],
    expect: f64,
    level: u16,
    rising: bool,
    search: usize,
) -> Option<f32> {
    if d.len() < 2 || !expect.is_finite() {
        return None;
    }
    let c = expect.round().clamp(1.0, (d.len() - 1) as f64) as usize;
    let (i0, i1) = (
        c.saturating_sub(search).max(1),
        (c + search).min(d.len() - 1),
    );
    let lv = level as f32;
    let mut best: Option<f32> = None;
    for i in i0..=i1 {
        let (a, b) = (d[i - 1] as f32, d[i] as f32);
        let hit = if rising {
            a < lv && b >= lv
        } else {
            a > lv && b <= lv
        };
        if hit {
            let x = (i - 1) as f32 + (lv - a) / (b - a);
            let off = x - expect as f32;
            if best.is_none_or(|bb| off.abs() < bb.abs()) {
                best = Some(off);
            }
        }
    }
    best
}

pub fn first_edge(d: &[u16], rising: bool) -> Option<usize> {
    let (lo, hi) = d
        .iter()
        .fold((u16::MAX, 0u16), |(l, h), &v| (l.min(v), h.max(v)));
    let span = hi.saturating_sub(lo) as f32;
    if span < 8.0 {
        return None;
    }
    let mid = (hi as f32 + lo as f32) / 2.0;
    let (on, off) = (mid + 0.1 * span, mid - 0.1 * span);
    let mut armed = false;
    for (i, &v) in d.iter().enumerate() {
        let v = v as f32;
        let (pre, post) = if rising {
            (v < off, v > on)
        } else {
            (v > on, v < off)
        };
        if pre {
            armed = true;
        } else if armed && post {
            return Some(i);
        }
    }
    None
}

// ───────────────────────────── software triggers ─────────────────────────────

/// Comparison for pulse-width triggers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cmp {
    Longer,
    Shorter,
    Between,
}

/// Software trigger evaluated on a captured trace (on top of the hardware edge trigger).
/// Times in seconds, voltages in volts. Zone times are relative to the trigger point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SwTrig {
    Off,
    /// a complete high (`high`) or low pulse at the trigger level whose width matches
    Width {
        high: bool,
        cmp: Cmp,
        t1: f64,
        t2: f64,
    },
    /// a pulse that crosses `lo` but turns back before reaching `hi` (high runt), or mirrored (low runt)
    Runt {
        high: bool,
        lo: f32,
        hi: f32,
    },
    /// no crossing of the trigger level for longer than `t`
    Dropout {
        t: f64,
    },
    /// any sample outside the band lo…hi
    Window {
        lo: f32,
        hi: f32,
    },
    /// the trace enters (or, with `enter = false`, never enters) the box t0…t1 × v0…v1
    Zone {
        t0: f64,
        t1: f64,
        v0: f32,
        v1: f32,
        enter: bool,
    },
}

/// A software-trigger match: the event starts at sample `start` and the condition becomes true at `at`
/// (e.g. pulse width > T: start = the pulse's first edge, at = start + T). `at >= start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwHit {
    pub start: usize,
    pub at: usize,
}

/// Evaluate a software trigger on one channel's samples. `zero`/`vpc` convert counts to volts, `dt` is the
/// sample spacing, `trig_idx` the (fractional) sample index of the hardware trigger point, `level` the
/// trigger level in volts. Returns the first matching event.
pub fn sw_trigger(
    t: &SwTrig,
    d: &[u16],
    zero: f32,
    vpc: f32,
    dt: f64,
    trig_idx: f64,
    level: f32,
) -> Option<SwHit> {
    if d.len() < 2 || dt <= 0.0 {
        return None;
    }
    let counts = |v: f32| zero + v / vpc;
    let samples = |t: f64| (t / dt - 1e-9).ceil().max(1.0) as usize; // tolerate float error (100e-6/1e-6 > 100)
    let hit = |start: usize, at: usize| {
        Some(SwHit {
            start,
            at: at.max(start),
        })
    };
    match *t {
        SwTrig::Off => None,
        SwTrig::Width { high, cmp, t1, t2 } => {
            let lv = counts(level);
            let above = |i: usize| d[i] as f32 > lv;
            let mut start: Option<usize> = None;
            for i in 1..d.len() {
                // "longer than": matches the moment the pulse has lasted t1, without waiting for its end
                if let (Some(s), Cmp::Longer) = (start, cmp) {
                    if i - s > samples(t1) {
                        return hit(s, s + samples(t1));
                    }
                }
                let (p, c) = (above(i - 1), above(i));
                if p == c {
                    continue;
                }
                if c == high {
                    start = Some(i); // entering the pulse polarity
                } else if let Some(s) = start.take() {
                    let w = (i - s) as f64 * dt;
                    let ok = match cmp {
                        Cmp::Longer => w > t1,
                        Cmp::Shorter => w < t1,
                        Cmp::Between => w >= t1.min(t2) && w <= t1.max(t2),
                    };
                    if ok {
                        return hit(s, i); // known only when the pulse ends
                    }
                }
            }
            None
        }
        SwTrig::Runt { high, lo, hi } => {
            let (lc, hc) = (counts(lo.min(hi)), counts(lo.max(hi)));
            // high runt: rises above lo, falls back below lo without reaching hi (low runt mirrored)
            let mut start: Option<usize> = None;
            let mut reached = false;
            for (i, &s) in d.iter().enumerate() {
                let v = s as f32;
                let (enter, full, back) = if high {
                    (v > lc, v >= hc, v < lc)
                } else {
                    (v < hc, v <= lc, v > hc)
                };
                if start.is_none() && enter {
                    start = Some(i);
                    reached = false;
                }
                if start.is_some() && full {
                    reached = true;
                }
                if back {
                    if let (Some(s), false) = (start, reached) {
                        return hit(s, i);
                    }
                    start = None;
                }
            }
            None
        }
        SwTrig::Dropout { t } => {
            let lv = counts(level);
            let n = samples(t);
            let mut last = 0usize;
            for i in 1..d.len() {
                if (d[i - 1] as f32 > lv) != (d[i] as f32 > lv) {
                    if i - last > n {
                        return hit(last, last + n);
                    }
                    last = i;
                }
            }
            if d.len() - 1 - last > n {
                hit(last, last + n)
            } else {
                None
            }
        }
        SwTrig::Window { lo, hi } => {
            let (lc, hc) = (counts(lo.min(hi)), counts(lo.max(hi)));
            let i = d.iter().position(|&s| (s as f32) < lc || (s as f32) > hc)?;
            hit(i, i)
        }
        SwTrig::Zone {
            t0,
            t1,
            v0,
            v1,
            enter,
        } => {
            let (a, b) = (t0.min(t1), t0.max(t1));
            let (lc, hc) = (counts(v0.min(v1)), counts(v0.max(v1)));
            let i0 = (trig_idx + a / dt).floor().max(0.0) as usize;
            let i1 = ((trig_idx + b / dt).ceil().max(0.0) as usize).min(d.len());
            if i0 >= i1 {
                return None; // zone outside this capture
            }
            let inside = d[i0..i1]
                .iter()
                .position(|&s| (s as f32) >= lc && (s as f32) <= hc);
            match (enter, inside) {
                (true, Some(k)) => hit(i0 + k, i0 + k),
                (false, None) => hit(i0, i1 - 1),
                _ => None,
            }
        }
    }
}

/// All matches of a software trigger in one capture (first `max`), in order. After each event the search
/// resumes past its match moment; for Window past the excursion (back inside the band), for Dropout at the
/// next level crossing, so one long event isn't reported many times. Zone yields at most one.
#[allow(clippy::too_many_arguments)]
pub fn sw_matches(
    t: &SwTrig,
    d: &[u16],
    zero: f32,
    vpc: f32,
    dt: f64,
    trig_idx: f64,
    level: f32,
    max: usize,
) -> Vec<SwHit> {
    let mut out = Vec::new();
    let mut off = 0usize;
    let counts = |v: f32| zero + v / vpc;
    while off + 1 < d.len() && out.len() < max {
        let Some(h) = sw_trigger(t, &d[off..], zero, vpc, dt, trig_idx - off as f64, level) else {
            break;
        };
        let h = SwHit {
            start: h.start + off,
            at: h.at + off,
        };
        out.push(h);
        if matches!(t, SwTrig::Zone { .. }) {
            break;
        }
        let mut next = h.at + 1;
        match *t {
            SwTrig::Window { lo, hi } => {
                let (lc, hc) = (counts(lo.min(hi)), counts(lo.max(hi)));
                while next < d.len() && ((d[next] as f32) < lc || (d[next] as f32) > hc) {
                    next += 1;
                }
            }
            SwTrig::Dropout { .. } => {
                let lv = counts(level);
                while next < d.len() && (d[next - 1] as f32 > lv) == (d[next] as f32 > lv) {
                    next += 1;
                }
            }
            _ => {}
        }
        off = next;
    }
    out
}

// ───────────────────────────── USB driver ─────────────────────────────

use rusb::{DeviceHandle, Direction, GlobalContext, TransferType};
use std::thread::sleep;
use std::time::{Duration, Instant};

pub const VID: u16 = 0x0783;
pub const PID: u16 = 0x5725;
const PKT: usize = 64;
const TIMEOUT: Duration = Duration::from_millis(1000);

/// Every delay the Python reference inserts. `reference()` reproduces it exactly;
/// the optimisation loop removes them one by one and benches each step.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// sleep before every command write (py: sec_till_start = 2 ms)
    pub pre_cmd: Duration,
    /// sleep between write and read after a2 / f5 / f6 (py: 213 ms)
    pub slow_cfg: Duration,
    /// sleep between write and read after `a4 01` in a burst capture (py: 15 ms)
    pub burst_a4: Duration,
    /// sleep before c2 in the zero-offset init (py: 12.4 ms)
    pub init_c2: Duration,
    /// sleep between the two b0 commands at init (py: 0.7 s)
    pub b0_settle: Duration,
    /// sleep between a5 5a retries (py: 20 ms)
    pub a55a_retry: Duration,
    /// sleep before the ping that starts roll mode (py: 10 ms)
    pub roll_start: Duration,
    /// e4/e6 pair before and after each burst capture (py: yes, "not necessarily required")
    pub burst_e4e6: bool,
    /// f3 ping at the start of each burst capture (py: yes)
    pub burst_ping: bool,
    /// f3 ping between a5 5a ready-poll retries (py: yes)
    pub ready_ping: bool,
    /// a6 data requests written before reading their replies (1 = strict request/response)
    pub pipeline: u8,
    /// sleep ~ the acquisition time (10 div × t/div, minus 1 ms) before the first ready poll
    pub ready_pace: bool,
}

impl Timing {
    pub fn reference() -> Self {
        Timing {
            pre_cmd: Duration::from_micros(2000),
            slow_cfg: Duration::from_micros(213_200),
            burst_a4: Duration::from_millis(15),
            init_c2: Duration::from_micros(12_400),
            b0_settle: Duration::from_millis(700),
            a55a_retry: Duration::from_millis(20),
            roll_start: Duration::from_millis(10),
            burst_e4e6: true,
            burst_ping: true,
            ready_ping: true,
            pipeline: 1,
            ready_pace: false,
        }
    }
    /// Python's targeted waits, but no blanket 2 ms pre-command sleep.
    pub fn lean() -> Self {
        Timing {
            pre_cmd: Duration::ZERO,
            ..Self::reference()
        }
    }
    /// No sleeps, no optional commands: blocking reads are the only pacing.
    pub fn none() -> Self {
        Timing {
            pre_cmd: Duration::ZERO,
            slow_cfg: Duration::ZERO,
            burst_a4: Duration::ZERO,
            init_c2: Duration::ZERO,
            b0_settle: Duration::ZERO,
            a55a_retry: Duration::ZERO,
            roll_start: Duration::ZERO,
            burst_e4e6: false,
            burst_ping: false,
            ready_ping: false,
            pipeline: 1,
            ready_pace: false,
        }
    }
    /// Measured best (2026-11-20 sweep): no sleeps, no optional commands; the device's own
    /// blocking replies pace everything. Only the one-off roll-mode start wait is untested.
    pub fn fast() -> Self {
        Timing {
            roll_start: Duration::from_millis(10),
            ready_pace: true,
            ..Self::none()
        }
    }
    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "fast" => Some(Self::fast()),
            "reference" => Some(Self::reference()),
            "lean" => Some(Self::lean()),
            "none" => Some(Self::none()),
            _ => None,
        }
    }
    /// Override one field: `name=µs` for delays, `name=0|1` for flags (bench `--set`).
    pub fn set(&mut self, kv: &str) -> std::result::Result<(), String> {
        let (k, v) = kv.split_once('=').ok_or("expected name=value")?;
        let n: u64 = v.parse().map_err(|_| format!("bad value {v}"))?;
        let d = Duration::from_micros(n);
        match k {
            "pre_cmd" => self.pre_cmd = d,
            "slow_cfg" => self.slow_cfg = d,
            "burst_a4" => self.burst_a4 = d,
            "init_c2" => self.init_c2 = d,
            "b0_settle" => self.b0_settle = d,
            "a55a_retry" => self.a55a_retry = d,
            "roll_start" => self.roll_start = d,
            "burst_e4e6" => self.burst_e4e6 = n != 0,
            "burst_ping" => self.burst_ping = n != 0,
            "ready_ping" => self.ready_ping = n != 0,
            "pipeline" => self.pipeline = n.clamp(1, 64) as u8,
            "ready_pace" => self.ready_pace = n != 0,
            _ => return Err(format!("unknown timing field {k}")),
        }
        Ok(())
    }
}

/// Where the time goes — the speed reviews read these.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub cmds: u64,
    pub bytes_in: u64,
    pub usb: Duration,
    pub slept: Duration,
    pub ready_polls: u64,
    /// captures forced by the Auto timeout (no trigger seen)
    pub forced: u64,
    /// corrupted captures dropped / end-of-buffer glitch samples repaired (see `sanitize`)
    pub rejected: u64,
    pub repaired: u64,
}

#[derive(Debug)]
pub enum Error {
    Usb(rusb::Error),
    NotFound,
    Protocol(String),
}

impl From<rusb::Error> for Error {
    fn from(e: rusb::Error) -> Self {
        Error::Usb(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Usb(e) => write!(f, "USB: {e}"),
            Error::NotFound => write!(f, "Hantek 1008C ({VID:04x}:{PID:04x}) not found"),
            Error::Protocol(s) => write!(f, "protocol: {s}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Raw blocks read during init; meaning unknown yet (RE target: calibration data).
#[derive(Clone, Debug, Default)]
pub struct InitBlobs {
    pub b5: Vec<u8>,
    pub b6: Vec<u8>,
    pub e5: Vec<u8>,
    pub f7: Vec<u8>,
    pub f8: Vec<u8>,
    pub fa: Vec<u8>,
    pub e9: Vec<u8>,
}

pub struct Hantek {
    h: DeviceHandle<GlobalContext>,
    ep_out: u8,
    ep_in: u8,
    pub timing: Timing,
    pub stats: Stats,
    /// what the device currently has (None until init) — only diffs are sent
    sent: Option<Config>,
    /// zero offset [vscale index][channel], measured at init
    pub zero: [[f32; CHANNELS]; 3],
    pub blobs: InitBlobs,
    /// `a4` parameter used to start a burst capture (01 in the reference; 02 = roll). Experiment knob.
    pub arm: u8,
    /// give up waiting for a capture after this long
    pub ready_deadline: Duration,
    /// histogram of `a5 5a` replies 0..=3 (index 4 = anything else) — what the scope says while we wait
    pub ready_hist: [u64; 5],
    /// consecutive captures forced by the Auto timeout
    auto_streak: u32,
    buf: Vec<u8>,
    rx: [u8; PKT + 1],
}

impl Hantek {
    pub fn open(timing: Timing) -> Result<Self> {
        let h = rusb::open_device_with_vid_pid(VID, PID).ok_or(Error::NotFound)?;
        let _ = h.set_auto_detach_kernel_driver(true); // unsupported on Windows: fine
        let cfg = h.device().active_config_descriptor()?;
        let (mut ep_out, mut ep_in, mut iface_no) = (None, None, 0);
        for iface in cfg.interfaces() {
            for d in iface.descriptors() {
                for ep in d.endpoint_descriptors() {
                    if ep.transfer_type() == TransferType::Bulk {
                        iface_no = d.interface_number();
                        match ep.direction() {
                            Direction::Out => ep_out = ep_out.or(Some(ep.address())),
                            Direction::In => ep_in = ep_in.or(Some(ep.address())),
                        }
                    }
                }
            }
        }
        let (Some(ep_out), Some(ep_in)) = (ep_out, ep_in) else {
            return Err(Error::Protocol("no bulk endpoints".into()));
        };
        h.claim_interface(iface_no)?;
        log::info!("opened {VID:04x}:{PID:04x} out={ep_out:#04x} in={ep_in:#04x}");
        Ok(Hantek {
            h,
            ep_out,
            ep_in,
            timing,
            stats: Stats::default(),
            sent: None,
            zero: [[0.0; CHANNELS]; 3],
            blobs: InitBlobs::default(),
            arm: 1,
            ready_deadline: Duration::from_secs(2),
            ready_hist: [0; 5],
            auto_streak: 0,
            buf: Vec::with_capacity(64 * 1024),
            rx: [0; PKT + 1],
        })
    }

    fn nap(&mut self, d: Duration) {
        if !d.is_zero() {
            sleep(d);
            self.stats.slept += d;
        }
    }

    /// Write `msg`, optionally wait, read at least `len` bytes into `self.rx`.
    fn xfer(&mut self, msg: &[u8], len: usize, wait: Duration) -> Result<()> {
        let pre = self.timing.pre_cmd;
        self.nap(pre);
        let t = Instant::now();
        self.h.write_bulk(self.ep_out, msg, TIMEOUT)?;
        let mut usb = t.elapsed();
        self.nap(wait);
        let mut n = 0;
        if len > 0 {
            let t = Instant::now();
            n = self
                .h
                .read_bulk(self.ep_in, &mut self.rx[..len.max(PKT)], TIMEOUT)?;
            usb += t.elapsed();
        }
        self.stats.cmds += 1;
        self.stats.bytes_in += n as u64;
        self.stats.usb += usb;
        if n < len {
            return Err(Error::Protocol(format!(
                "cmd {:02x}: got {n} of {len} bytes",
                msg[0]
            )));
        }
        Ok(())
    }

    /// Command that echoes its id, plus `len` reply bytes (in `self.rx[1..=len]`).
    fn cmd(&mut self, msg: &[u8], len: usize, wait: Duration) -> Result<()> {
        self.xfer(msg, len + 1, wait)?;
        if self.rx[0] != msg[0] {
            return Err(Error::Protocol(format!(
                "cmd {:02x}: echo {:02x}",
                msg[0], self.rx[0]
            )));
        }
        Ok(())
    }

    /// Command without echo, raw reply of `len` bytes.
    fn query(&mut self, msg: &[u8], len: usize) -> Result<&[u8]> {
        self.xfer(msg, len, Duration::ZERO)?;
        Ok(&self.rx[..len])
    }

    /// f3 keep-alive (the reference pings every 10 ms while paused)
    pub fn ping(&mut self) -> Result<()> {
        self.cmd(&[0xf3], 0, Duration::ZERO)
    }

    fn set_active(&mut self, active: &[bool; CHANNELS]) -> Result<()> {
        let n = active.iter().filter(|&&a| a).count() as u8;
        self.cmd(&[0xa0, n], 0, Duration::ZERO)?;
        let mut m = [0xaa, 0, 0, 0, 0, 0, 0, 0, 0];
        for (i, &a) in active.iter().enumerate() {
            m[1 + i] = a as u8;
        }
        self.cmd(&m, 0, Duration::ZERO)
    }

    fn set_vscale(&mut self, vs: &[VScale; CHANNELS]) -> Result<()> {
        let mut m = [0xa2, 0, 0, 0, 0, 0, 0, 0, 0];
        for (i, v) in vs.iter().enumerate() {
            m[1 + i] = v.id();
        }
        let w = self.timing.slow_cfg;
        self.cmd(&m, 0, w)
    }

    fn set_timebase(&mut self, id: u8) -> Result<()> {
        self.cmd(&[0xa3, id], 0, Duration::ZERO)
    }

    fn set_trigger(&mut self, src: u8, slope: Slope) -> Result<()> {
        self.cmd(&[0xc1, src, slope as u8], 0, Duration::ZERO)
    }

    fn set_trigger_level(&mut self, lvl: u16) -> Result<()> {
        let [a, b] = lvl.min(4095).to_be_bytes();
        self.cmd(&[0xab, a, b], 0, Duration::ZERO)
    }

    fn e4e6(&mut self) -> Result<()> {
        self.cmd(&[0xe4, 0x01], 0, Duration::ZERO)?;
        self.query(&[0xe6, 0x01], 10)?;
        Ok(())
    }

    /// a5 5a: poll until the capture is ready (reply 2 or 3).
    fn wait_ready(&mut self) -> Result<()> {
        // deadline instead of the reference's 20 attempts, so retries can run back-to-back
        let deadline = Instant::now() + self.ready_deadline;
        loop {
            self.cmd(&[0xa5, 0x5a], 1, Duration::ZERO)?;
            self.stats.ready_polls += 1;
            self.ready_hist[(self.rx[1] as usize).min(4)] += 1;
            match self.rx[1] {
                2 | 3 => return Ok(()),
                0 | 1 => {}
                x => return Err(Error::Protocol(format!("a5 reply {x}"))),
            }
            if Instant::now() > deadline {
                return Err(Error::Protocol(format!(
                    "a5 5a: not ready after {:?}",
                    self.ready_deadline
                )));
            }
            let r = self.timing.a55a_retry;
            self.nap(r);
            if self.timing.ready_ping {
                self.ping()?;
            }
        }
    }

    /// c6 → BE length, then ceil(len/64) × a6 reads, appended to `self.buf`.
    fn read_bank(&mut self, bank: u8) -> Result<()> {
        let r = self.query(&[0xc6, bank], 2)?;
        let mut left = u16::from_be_bytes([r[0], r[1]]) as usize;
        let k = self.timing.pipeline.max(1) as usize;
        if k == 1 {
            while left > 0 {
                self.query(&[0xa6, bank], PKT)?;
                let take = left.min(PKT);
                self.buf.extend_from_slice(&self.rx[..take]);
                left -= take;
            }
            return Ok(());
        }
        // pipelined: write up to k requests, then read their k replies
        let mut pkts = left.div_ceil(PKT);
        while pkts > 0 {
            let batch = pkts.min(k);
            let t = Instant::now();
            for _ in 0..batch {
                self.h.write_bulk(self.ep_out, &[0xa6, bank], TIMEOUT)?;
            }
            for _ in 0..batch {
                let n = self.h.read_bulk(self.ep_in, &mut self.rx[..PKT], TIMEOUT)?;
                if n < PKT {
                    return Err(Error::Protocol(format!(
                        "pipelined a6: got {n} of {PKT} bytes"
                    )));
                }
                let take = left.min(PKT);
                self.buf.extend_from_slice(&self.rx[..take]);
                left -= take;
                self.stats.bytes_in += n as u64;
            }
            self.stats.cmds += batch as u64;
            self.stats.usb += t.elapsed();
            pkts -= batch;
        }
        Ok(())
    }

    fn generator_off(&mut self) -> Result<()> {
        // speed 300 000 rpm → pulse length 1200 (as in the reference init)
        let pl = 1200u32.to_le_bytes();
        self.cmd(&[0xb9, 0x01, pl[0], pl[1], pl[2], pl[3]], 0, Duration::ZERO)?;
        self.cmd(&[0xb7, 0x00], 0, Duration::ZERO)?;
        self.cmd(&[0xbb, 0x08, 0x00], 0, Duration::ZERO)
    }

    fn read_blobs(&mut self) -> Result<()> {
        self.blobs.e5 = self.query(&[0xe5], 2)?.to_vec();
        self.blobs.f7 = self.query(&[0xf7], 64)?.to_vec();
        self.blobs.f8 = self.query(&[0xf8], 64)?.to_vec();
        self.blobs.fa = self.query(&[0xfa], 56)?.to_vec();
        Ok(())
    }

    /// Full init — same sequence as the vendor software / hantek1008py — then applies `cfg`.
    pub fn init(&mut self, cfg: &Config) -> Result<()> {
        let t = self.timing;
        // init1
        self.cmd(&[0xb0], 0, Duration::ZERO)?;
        self.nap(t.b0_settle);
        self.cmd(&[0xb0], 0, Duration::ZERO)?;
        self.ping()?;
        self.generator_off()?;
        self.blobs.b5 = self.query(&[0xb5], 64)?.to_vec();
        self.blobs.b6 = self.query(&[0xb6], 64)?.to_vec();
        self.read_blobs()?;
        self.cmd(&[0xf5], 0, t.slow_cfg)?;
        self.set_active(&[true; CHANNELS])?;
        self.set_timebase(burst_id_for_ns(500_000).unwrap())?;
        self.set_trigger(0, Slope::Rising)?;
        self.cmd(&[0xa7, 0x00, 0x00], 1, Duration::ZERO)?;
        self.cmd(
            &[0xac, 0x01, 0xf4, 0x00, 0x09, 0xc5, 0x00, 0x09, 0xc5],
            0,
            Duration::ZERO,
        )?;

        // init2: zero offset per range, all channels
        let all = [true; CHANNELS];
        let mut f: [Vec<u16>; CHANNELS] = Default::default();
        for vs in VScale::ALL {
            self.ping()?;
            self.set_vscale(&[vs; CHANNELS])?;
            self.cmd(&[0xa4, 0x01], 0, Duration::ZERO)?;
            self.cmd(&[0xc0], 0, Duration::ZERO)?;
            self.nap(t.init_c2);
            self.cmd(&[0xc2], 0, Duration::ZERO)?;
            self.wait_ready()?;
            self.buf.clear();
            self.read_bank(2)?;
            self.read_bank(3)?;
            f.iter_mut().for_each(Vec::clear);
            decode_append(&self.buf, &all, CHANNELS, &mut f);
            for (ch, d) in f.iter().enumerate() {
                self.zero[vs.index()][ch] = if d.is_empty() {
                    2048.0
                } else {
                    d.iter().map(|&v| v as f32).sum::<f32>() / d.len() as f32
                };
            }
        }

        // init3
        self.cmd(&[0xf6], 0, t.slow_cfg)?;
        self.read_blobs()?;
        self.set_timebase(cfg.timebase_id)?;
        self.cmd(
            &ac_payload(cfg.trig_pos, window_us(cfg.timebase_id)),
            0,
            Duration::ZERO,
        )?; // vendor-style window (reference had 00c8·0002bd·0002bd)
        self.e4e6()?;
        self.ping()?;
        self.set_active(&cfg.active)?;
        self.set_vscale(&cfg.vscale)?;
        self.set_timebase(cfg.timebase_id)?;
        self.set_trigger(cfg.trig_src, cfg.trig_slope)?;
        self.cmd(&[0xa7, 0x00, 0x00], 1, Duration::ZERO)?;
        self.cmd(
            &ac_payload(cfg.trig_pos, window_us(cfg.timebase_id)),
            0,
            Duration::ZERO,
        )?; // vendor: window for the timebase (reference had 0000·000001·000579)
        self.set_trigger_level(cfg.trig_level)?;
        self.blobs.e9 = self.query(&[0xe9], 2)?.to_vec();
        self.sent = Some(cfg.clone());
        Ok(())
    }

    /// Send only what changed since the last init/configure.
    pub fn configure(&mut self, cfg: &Config) -> Result<()> {
        let old = self.sent.take();
        let diff = |f: &dyn Fn(&Config) -> bool| old.as_ref().is_none_or(f);
        if diff(&|o| o.active != cfg.active) {
            self.set_active(&cfg.active)?;
        }
        if diff(&|o| o.vscale != cfg.vscale) {
            self.set_vscale(&cfg.vscale)?;
        }
        if diff(&|o| o.timebase_id != cfg.timebase_id) {
            self.set_timebase(cfg.timebase_id)?;
        }
        // the vendor re-sends the window after every timebase change and while dragging the trigger position.
        // It must also follow a channel-count change: the scope sizes the capture from the window at
        // the time `ac` arrives (stale window → recording stops early, flat tail; probe --window).
        if diff(&|o| {
            o.timebase_id != cfg.timebase_id || o.trig_pos != cfg.trig_pos || o.active != cfg.active
        }) {
            self.cmd(
                &ac_payload(cfg.trig_pos, window_us(cfg.timebase_id)),
                0,
                Duration::ZERO,
            )?;
        }
        if diff(&|o| o.trig_src != cfg.trig_src || o.trig_slope != cfg.trig_slope) {
            self.set_trigger(cfg.trig_src, cfg.trig_slope)?;
        }
        if diff(&|o| o.trig_level != cfg.trig_level) {
            self.set_trigger_level(cfg.trig_level)?;
        }
        self.sent = Some(cfg.clone());
        Ok(())
    }

    /// Send an arbitrary command (experiments / RE). `echo`: reply starts with the command id.
    /// Returns the reply bytes after the echo.
    pub fn raw(&mut self, msg: &[u8], len: usize, echo: bool) -> Result<Vec<u8>> {
        if echo {
            self.cmd(msg, len, Duration::ZERO)?;
            Ok(self.rx[1..=len].to_vec())
        } else {
            Ok(self.query(msg, len)?.to_vec())
        }
    }

    pub fn config(&self) -> Option<&Config> {
        self.sent.as_ref()
    }

    fn active(&self) -> [bool; CHANNELS] {
        self.sent.as_ref().map_or([true; CHANNELS], |c| c.active)
    }

    /// One triggered (burst) capture into `out` (buffers reused).
    pub fn capture(&mut self, out: &mut Frame) -> Result<bool> {
        let t = self.timing;
        if t.burst_ping {
            self.ping()?;
        }
        if t.burst_e4e6 {
            self.e4e6()?;
        }
        let arm = self.arm;
        self.cmd(&[0xa4, arm], 0, t.burst_a4)?;
        self.cmd(&[0xc0], 0, Duration::ZERO)?;
        let mode = self.sent.as_ref().map_or(TrigMode::Free, |c| c.trig_mode);
        if mode == TrigMode::Free {
            self.cmd(&[0xc2], 0, Duration::ZERO)?; // force now
        }
        if t.ready_pace {
            let tb = self
                .sent
                .as_ref()
                .map_or(0, |c| burst_ns_per_div(c.timebase_id));
            let acq = Duration::from_nanos(10 * tb);
            // only worth it for long acquisitions: at 500 µs/div pacing measured -4..6 % fps
            if acq >= Duration::from_millis(20) {
                self.nap(acq - Duration::from_millis(1));
            }
        }
        if !self.wait_trigger(mode)? {
            return Ok(false); // Normal mode, nothing triggered
        }
        self.buf.clear();
        self.read_bank(2)?;
        self.read_bank(3)?;
        if t.burst_e4e6 {
            self.e4e6()?;
        }
        let active = self.active();
        let n = active.iter().filter(|&&a| a).count();
        out.clear();
        out.active = active;
        out.seq += 1;
        decode_append(&self.buf, &active, n, &mut out.data);
        Ok(self.sanitize(out))
    }

    /// Data sanity (`probe --sanity`, 2026-11-21): the scope occasionally returns a corrupted capture
    /// (values far above 12 bits; seen at 100 µs/div, ~0.6 %) → rejected (no frame, the caller captures
    /// again). At 1–2 ms/div the second-to-last sample sometimes jumps (end-of-buffer seam, ~1–3 % of
    /// captures) → repaired at exactly that position only, so real one-sample events elsewhere survive.
    fn sanitize(&mut self, out: &mut Frame) -> bool {
        if out.data.iter().any(|d| d.iter().any(|&v| v > 4095)) {
            self.stats.rejected += 1;
            return false;
        }
        for d in out.data.iter_mut() {
            let n = d.len();
            if n >= 4 {
                let (a, b, c) = (d[n - 3] as i32, d[n - 2] as i32, d[n - 1] as i32);
                if (b - a).abs() > 400 && (b - c).abs() > 400 {
                    d[n - 2] = ((a + c) / 2) as u16;
                    self.stats.repaired += 1;
                }
            }
        }
        true
    }

    /// Wait for the armed capture. Auto: force with `c2` once `auto_timeout` passes without a
    /// trigger. Normal: give up (Ok(false)) at `ready_deadline`. Free: already forced.
    fn wait_trigger(&mut self, mode: TrigMode) -> Result<bool> {
        if mode == TrigMode::Free {
            self.wait_ready()?;
            return Ok(true);
        }
        let t0 = Instant::now();
        let window = Duration::from_micros(window_us(
            self.sent.as_ref().map_or(17, |c| c.timebase_id),
        ) as u64);
        // auto: wait at least 50 ms (vendor ≈ 50 ms) and 2 windows before forcing
        // auto: wait at least 50 ms (vendor ≈ 50 ms) and 2 windows before forcing; after 3 forced captures in a
        // row (no trigger in sight) only 2 windows, so the screen keeps refreshing at near full speed
        let floor = if self.auto_streak >= 3 {
            Duration::from_millis(5)
        } else {
            Duration::from_millis(50)
        };
        let auto_after = (2 * window).max(floor);
        let mut forced = false;
        loop {
            self.cmd(&[0xa5, 0x5a], 1, Duration::ZERO)?;
            self.stats.ready_polls += 1;
            self.ready_hist[(self.rx[1] as usize).min(4)] += 1;
            match self.rx[1] {
                2 | 3 => {
                    if !forced {
                        self.auto_streak = 0; // a real trigger: back to the normal Auto wait
                    }
                    return Ok(true);
                }
                0 | 1 => {}
                x => return Err(Error::Protocol(format!("a5 reply {x}"))),
            }
            let el = t0.elapsed();
            if mode == TrigMode::Auto && !forced && el >= auto_after {
                self.cmd(&[0xc2], 0, Duration::ZERO)?;
                forced = true;
                self.stats.forced += 1;
                self.auto_streak += 1;
            }
            // Normal: give up after a short slice so the caller can apply settings and re-arm
            if mode == TrigMode::Normal && el >= (2 * window).max(Duration::from_millis(100)) {
                return Ok(false);
            }
            // slow timebases: a capture itself takes up to 10 div × t/div → scale the deadline with it
            let deadline = self.ready_deadline.max(3 * window + Duration::from_secs(1));
            if el >= deadline {
                return Err(Error::Protocol(format!(
                    "a5 5a: not ready after {:?}",
                    deadline
                )));
            }
            if self.timing.ready_ping {
                self.ping()?;
            }
        }
    }

    /// Enter roll mode at `rate_id` (see ROLL_RATES).
    pub fn roll_start(&mut self, rate_id: u8) -> Result<()> {
        self.set_timebase(rate_id)?;
        if let Some(c) = self.sent.as_mut() {
            c.timebase_id = rate_id;
        }
        let w = self.timing.roll_start;
        self.nap(w);
        self.ping()?;
        self.cmd(&[0xa4, 0x02], 0, Duration::ZERO)?;
        self.cmd(&[0xc0], 0, Duration::ZERO)?;
        self.cmd(&[0xc2], 0, Duration::ZERO)
    }

    /// Poll roll mode once; appends new rows to `out.data`, returns rows added (0 = none yet).
    pub fn roll_poll(&mut self, out: &mut Frame) -> Result<usize> {
        self.ping()?;
        let r = self.query(&[0xc7], 2)?;
        let mut left = u16::from_be_bytes([r[0], r[1]]) as usize;
        if left == 0 {
            return Ok(0);
        }
        self.buf.clear();
        while left > 0 {
            self.query(&[0xc8], PKT)?;
            let take = left.min(PKT);
            self.buf.extend_from_slice(&self.rx[..take]);
            left -= take;
        }
        let active = self.active();
        out.active = active;
        let stride = active.iter().filter(|&&a| a).count() + 1; // + mystery 9th column
        Ok(decode_append(&self.buf, &active, stride, &mut out.data))
    }

    pub fn close(mut self) {
        let _ = self
            .h
            .read_bulk(self.ep_in, &mut self.rx, Duration::from_millis(100)); // leftovers
        let _ = self.ping();
        let _ = self.cmd(&[0xf4], 0, Duration::ZERO);
        let _ = self.h.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le(vals: &[u16]) -> Vec<u8> {
        vals.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn timebase_ids() {
        assert_eq!(burst_ns_per_div(0), 1);
        assert_eq!(burst_ns_per_div(1), 2);
        assert_eq!(burst_ns_per_div(2), 5);
        assert_eq!(burst_ns_per_div(9), 1_000);
        assert_eq!(burst_ns_per_div(25), 200_000_000);
        assert_eq!(burst_id_for_ns(500_000), Some(17));
        assert_eq!(burst_id_for_ns(3), None);
    }

    #[test]
    fn decode_all_channels() {
        let active = [true; 8];
        let bytes = le(&(0..16).collect::<Vec<u16>>());
        let mut out: [Vec<u16>; 8] = Default::default();
        assert_eq!(decode_append(&bytes, &active, 8, &mut out), 2);
        assert_eq!(out[0], vec![0, 8]);
        assert_eq!(out[7], vec![7, 15]);
    }

    #[test]
    fn decode_sparse_channels_and_ninth_column() {
        let mut active = [false; 8];
        active[1] = true;
        active[6] = true;
        // roll mode: 2 active + mystery column (~1742)
        let bytes = le(&[100, 600, 1742, 101, 601, 1742, 102]); // trailing partial row
        let mut out: [Vec<u16>; 8] = Default::default();
        assert_eq!(decode_append(&bytes, &active, 3, &mut out), 2);
        assert_eq!(out[1], vec![100, 101]);
        assert_eq!(out[6], vec![600, 601]);
        assert!(out[0].is_empty());
    }

    #[test]
    fn decode_appends() {
        let active = [true, false, false, false, false, false, false, false];
        let mut out: [Vec<u16>; 8] = Default::default();
        decode_append(&le(&[1, 0x0fff]), &active, 1, &mut out);
        decode_append(&le(&[3]), &active, 1, &mut out);
        assert_eq!(out[0], vec![1, 0x0fff, 3]);
    }

    #[test]
    fn measure_square_1khz_2vpp() {
        // 1 channel @ 500 µs/div: 4000 samples over 5 ms -> dt = 1.25 µs; 2 V at 1.25 mV/count = 1600 counts
        let dt = 5e-3 / 4000.0;
        let d: Vec<u16> = (0..4000)
            .map(|i| {
                if ((i as f64 * dt) * 1000.0).fract() < 0.5 {
                    3600
                } else {
                    2000
                }
            })
            .collect();
        let m = measure(&d, 2000.0, 0.00125, Some(dt));
        assert!((m.max - m.min - 2.0).abs() < 1e-3, "vpp {}", m.max - m.min);
        assert!(m.min.abs() < 1e-3);
        assert!(
            (m.top - m.base - 2.0).abs() < 1e-3,
            "amp {}",
            m.top - m.base
        );
        let f = m.freq.unwrap();
        assert!((f - 1000.0).abs() < 1.0, "freq {f}");
    }

    #[test]
    fn measure_flat_has_no_freq() {
        let d = vec![2048u16; 1000];
        assert!(measure(&d, 2048.0, 0.01, Some(1e-6)).freq.is_none());
    }

    #[test]
    fn measure_pwm_duty_widths_edges() {
        // 1 kHz, 25 % duty, 1 µs per sample → 1000 samples/period; edges are 8-sample linear ramps
        let dt = 1e-6;
        let (lo, hi) = (2000.0f32, 3600.0f32);
        let d: Vec<u16> = (0..5000)
            .map(|i| {
                let p = (i % 1000) as f32;
                let v = if p < 8.0 {
                    lo + (hi - lo) * p / 8.0 // rising ramp
                } else if p < 250.0 {
                    hi
                } else if p < 258.0 {
                    hi - (hi - lo) * (p - 250.0) / 8.0 // falling ramp
                } else {
                    lo
                };
                v as u16
            })
            .collect();
        let m = measure(&d, 2000.0, 0.00125, Some(dt));
        let near = |a: Option<f64>, b: f64, tol: f64| a.is_some_and(|x| (x - b).abs() <= tol);
        assert!(near(m.period, 1e-3, 1e-6), "period {:?}", m.period);
        assert!(near(m.duty, 0.25, 0.005), "duty {:?}", m.duty);
        assert!(near(m.width_pos, 250e-6, 2e-6), "+width {:?}", m.width_pos);
        assert!(near(m.width_neg, 750e-6, 2e-6), "-width {:?}", m.width_neg);
        // 10→90 % of an 8-sample linear ramp = 6.4 samples
        assert!(near(m.rise, 6.4e-6, 1.1e-6), "rise {:?}", m.rise);
        assert!(near(m.fall, 6.4e-6, 1.1e-6), "fall {:?}", m.fall);
        assert!((m.mean - (0.25 * 2.0)).abs() < 0.03, "mean {}", m.mean);
    }

    /// samples from a list of (count, volts) segments; zero 2000 counts, 10 mV/count
    fn seg(parts: &[(usize, f32)]) -> Vec<u16> {
        parts
            .iter()
            .flat_map(|&(n, v)| std::iter::repeat((2000.0 + v / 0.01) as u16).take(n))
            .collect()
    }

    #[test]
    fn sw_width_longer_shorter_between() {
        // pulses of 100 and 300 samples (1 µs each) at 5 V
        let d = seg(&[(50, 0.0), (100, 5.0), (100, 0.0), (300, 5.0), (100, 0.0)]);
        let tr = |cmp, t1, t2| {
            sw_trigger(
                &SwTrig::Width {
                    high: true,
                    cmp,
                    t1,
                    t2,
                },
                &d,
                2000.0,
                0.01,
                1e-6,
                0.0,
                2.5,
            )
        };
        assert_eq!(
            tr(Cmp::Longer, 200e-6, 0.0),
            Some(SwHit {
                start: 250,
                at: 450
            })
        ); // matches once 200 µs have passed
        assert_eq!(
            tr(Cmp::Shorter, 200e-6, 0.0),
            Some(SwHit { start: 50, at: 150 })
        );
        assert_eq!(
            tr(Cmp::Between, 250e-6, 350e-6),
            Some(SwHit {
                start: 250,
                at: 550
            })
        );
        assert_eq!(tr(Cmp::Longer, 400e-6, 0.0), None);
        // low pulse of 100 samples between them
        let low = sw_trigger(
            &SwTrig::Width {
                high: false,
                cmp: Cmp::Between,
                t1: 90e-6,
                t2: 110e-6,
            },
            &d,
            2000.0,
            0.01,
            1e-6,
            0.0,
            2.5,
        );
        assert_eq!(
            low,
            Some(SwHit {
                start: 150,
                at: 250
            })
        );
    }

    #[test]
    fn sw_matches_finds_every_event_once() {
        // three high pulses: 300, 100, 300 samples
        let d = seg(&[
            (50, 0.0),
            (300, 5.0),
            (100, 0.0),
            (100, 5.0),
            (100, 0.0),
            (300, 5.0),
            (100, 0.0),
        ]);
        let longer = SwTrig::Width {
            high: true,
            cmp: Cmp::Longer,
            t1: 200e-6,
            t2: 0.0,
        };
        let m = sw_matches(&longer, &d, 2000.0, 0.01, 1e-6, 0.0, 2.5, 100);
        assert_eq!(
            m,
            vec![
                SwHit { start: 50, at: 250 },
                SwHit {
                    start: 650,
                    at: 850
                }
            ]
        );
        // window 0…3 V: three excursions (the three pulses), each reported once
        let w = sw_matches(
            &SwTrig::Window { lo: -0.5, hi: 3.0 },
            &d,
            2000.0,
            0.01,
            1e-6,
            0.0,
            2.5,
            100,
        );
        assert_eq!(
            w.iter().map(|h| h.start).collect::<Vec<_>>(),
            vec![50, 450, 650]
        );
        // dropout 50 µs: each flat stretch longer than 50 samples (the first is exactly 50 → not), once per stretch
        let dr = sw_matches(
            &SwTrig::Dropout { t: 50e-6 },
            &d,
            2000.0,
            0.01,
            1e-6,
            0.0,
            2.5,
            100,
        );
        assert_eq!(dr.len(), 6);
        assert_eq!(
            sw_matches(&longer, &d, 2000.0, 0.01, 1e-6, 0.0, 2.5, 1).len(),
            1
        ); // max respected
    }

    #[test]
    fn sw_runt_dropout_window_zone() {
        // full pulse to 5 V, then a runt to 2 V, then flat
        let d = seg(&[(20, 0.0), (20, 5.0), (20, 0.0), (20, 2.0), (200, 0.0)]);
        let runt = SwTrig::Runt {
            high: true,
            lo: 1.0,
            hi: 4.0,
        };
        assert_eq!(
            sw_trigger(&runt, &d, 2000.0, 0.01, 1e-6, 0.0, 2.5),
            Some(SwHit { start: 60, at: 80 })
        );
        // dropout: last edge = the fall at 40 (the runt doesn't cross 2.5 V) → 240 samples without an edge
        assert_eq!(
            sw_trigger(
                &SwTrig::Dropout { t: 100e-6 },
                &d,
                2000.0,
                0.01,
                1e-6,
                0.0,
                2.5
            ),
            Some(SwHit { start: 40, at: 140 })
        );
        assert_eq!(
            sw_trigger(
                &SwTrig::Dropout { t: 300e-6 },
                &d,
                2000.0,
                0.01,
                1e-6,
                0.0,
                2.5
            ),
            None
        );
        // window 0…3 V: the 5 V pulse leaves it
        assert_eq!(
            sw_trigger(
                &SwTrig::Window { lo: -0.5, hi: 3.0 },
                &d,
                2000.0,
                0.01,
                1e-6,
                0.0,
                2.5
            ),
            Some(SwHit { start: 20, at: 20 })
        );
        // zone 55…85 µs after the trigger (at sample 0), 1…3 V: the runt enters it
        let z = SwTrig::Zone {
            t0: 55e-6,
            t1: 85e-6,
            v0: 1.0,
            v1: 3.0,
            enter: true,
        };
        assert_eq!(
            sw_trigger(&z, &d, 2000.0, 0.01, 1e-6, 0.0, 2.5),
            Some(SwHit { start: 60, at: 60 })
        );
        let not = SwTrig::Zone {
            t0: 55e-6,
            t1: 85e-6,
            v0: 1.0,
            v1: 3.0,
            enter: false,
        };
        assert_eq!(sw_trigger(&not, &d, 2000.0, 0.01, 1e-6, 0.0, 2.5), None);
    }

    #[test]
    fn first_edge_positions() {
        let d: Vec<u16> = (0..100)
            .map(|i| if (i / 25) % 2 == 1 { 3000 } else { 1000 })
            .collect();
        assert_eq!(first_edge(&d, true), Some(25));
        assert_eq!(first_edge(&d, false), Some(50));
        assert_eq!(first_edge(&[2048; 50], true), None);
    }
}

#[cfg(test)]
mod trigger_offset_tests {
    use super::*;

    #[test]
    fn finds_the_interpolated_crossing_nearest_the_nominal_point() {
        // rising edge from 1000 to 3000 between samples 47 and 48; level 2000 → crossing at 47.5
        let mut d = vec![1000u16; 100];
        d[48..].iter_mut().for_each(|v| *v = 3000);
        assert_eq!(trigger_offset(&d, 50.0, 2000, true, 12), Some(-2.5));
        // falling edge is not a rising crossing
        assert_eq!(trigger_offset(&d, 50.0, 2000, false, 12), None);
        // outside the search window → no correction
        assert_eq!(trigger_offset(&d, 70.0, 2000, true, 12), None);
        // two crossings: the nearer one wins
        let mut e = vec![1000u16; 100];
        e[40..44].iter_mut().for_each(|v| *v = 3000);
        e[52..].iter_mut().for_each(|v| *v = 3000);
        assert_eq!(trigger_offset(&e, 50.0, 2000, true, 12), Some(1.5));
        // edges of the buffer don't panic
        assert_eq!(trigger_offset(&d, 0.0, 2000, true, 12), None);
        assert_eq!(trigger_offset(&d, 1e9, 2000, true, 12), None);
    }

    /// cargo test --release trigger_offset_speed -- --ignored --nocapture
    #[test]
    #[ignore]
    fn trigger_offset_speed() {
        let d: Vec<u16> = (0..4096)
            .map(|i| if (i / 50) % 2 == 0 { 1000 } else { 3000 })
            .collect();
        let t = std::time::Instant::now();
        let mut acc = 0.0f32;
        for k in 0..1_000_000 {
            acc += trigger_offset(&d, 2048.0 + (k % 7) as f64, 2000, true, 12).unwrap_or(0.0);
        }
        println!(
            "trigger_offset: {:.0} ns per capture ({acc})",
            t.elapsed().as_secs_f64() * 1e9 / 1e6
        );
    }
}
