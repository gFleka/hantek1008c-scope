//! Signal processing for math channels: pure functions on samples in display units.

/// Single-pole low-pass (RC) with cutoff `fc` Hz; `dt` = sample spacing in seconds.
pub fn lowpass(x: &[f32], dt: f64, fc: f64) -> Vec<f32> {
    if x.is_empty() || fc <= 0.0 {
        return x.to_vec();
    }
    let rc = 1.0 / (2.0 * std::f64::consts::PI * fc);
    let a = (dt / (rc + dt)) as f32;
    let mut y = Vec::with_capacity(x.len());
    let mut s = x[0];
    for &v in x {
        s += a * (v - s);
        y.push(s);
    }
    y
}

/// Speed in rpm over time from a pulse signal (`ppr` pulses per revolution): at each sample the rpm of the
/// interval between the surrounding rising edges (mid-level, 10 % hysteresis); NaN outside the first/last edge.
pub fn rpm_series(x: &[f32], dt: f64, ppr: f64) -> Vec<f32> {
    let mut out = vec![f32::NAN; x.len()];
    if x.len() < 3 || ppr <= 0.0 {
        return out;
    }
    let (lo, hi) = x
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let span = hi - lo;
    if span <= 0.0 {
        return out;
    }
    let mid = (lo + hi) / 2.0;
    let (on, off) = (mid + 0.1 * span, mid - 0.1 * span);
    let mut edges = Vec::new();
    let mut armed = false;
    for (i, &v) in x.iter().enumerate() {
        if v < off {
            armed = true;
        } else if armed && v > on {
            armed = false;
            edges.push(i);
        }
    }
    for w in edges.windows(2) {
        let period = (w[1] - w[0]) as f64 * dt;
        let rpm = (60.0 / (period * ppr)) as f32;
        out[w[0]..w[1]].iter_mut().for_each(|o| *o = rpm);
    }
    out
}

/// Teeth of a missing-tooth crank wheel (e.g. 60-2) in a sampled sensor signal: for each rising edge after the
/// first detected gap, (sample index, tooth number 0 … teeth−1), tooth 0 = the first tooth after the gap.
/// A gap = an edge interval > 1.5 × the average of the previous 3 intervals (missing teeth). Mid-level, 10 %
/// hysteresis (inductive and Hall sensors alike). Empty if no gap is in the data.
pub fn crank_teeth(x: &[f32], teeth: u32) -> Vec<(usize, u32)> {
    let mut out = Vec::new();
    if x.len() < 8 || teeth < 4 {
        return out;
    }
    let (lo, hi) = x
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let span = hi - lo;
    if span <= 0.0 {
        return out;
    }
    let mid = (lo + hi) / 2.0;
    let (on, off) = (mid + 0.1 * span, mid - 0.1 * span);
    let mut edges = Vec::new();
    let mut armed = false;
    for (i, &v) in x.iter().enumerate() {
        if v < off {
            armed = true;
        } else if armed && v > on {
            armed = false;
            edges.push(i);
        }
    }
    let mut tooth: Option<u32> = None;
    for k in 1..edges.len() {
        let d = (edges[k] - edges[k - 1]) as f32;
        let avg = if k >= 4 {
            (edges[k - 1] - edges[k - 4]) as f32 / 3.0
        } else {
            f32::MAX
        };
        if d > 1.5 * avg {
            tooth = Some(0); // first tooth after the gap
        } else if let Some(t) = tooth.as_mut() {
            *t = (*t + 1) % teeth;
        }
        if let Some(t) = tooth {
            out.push((edges[k], t));
        }
    }
    out
}

/// One decoded item of a UART-style bus (LIN, K-line): sample range and what it is.
#[derive(Clone, Debug, PartialEq)]
pub enum SerialItem {
    /// LIN break: low for at least 13 bit times
    Break,
    Byte(u8),
    /// start bit found, but the stop bit was low (wrong baud rate / noise)
    FrameError,
}

/// Decode an idle-high UART bus (LIN, K-line): start bit low, 8 data bits LSB first, stop bit high, `baud` bit/s.
/// Threshold = middle of the signal's span. Returns (first sample, last sample, item).
pub fn uart_decode(x: &[f32], dt: f64, baud: f64) -> Vec<(usize, usize, SerialItem)> {
    let mut out = Vec::new();
    if x.len() < 16 || baud <= 0.0 || dt <= 0.0 {
        return out;
    }
    let (lo, hi) = x
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    if hi - lo <= 0.0 {
        return out;
    }
    let thr = (lo + hi) / 2.0;
    let bit = |i: usize| x.get(i).is_some_and(|&v| v > thr);
    let spb = 1.0 / (baud * dt); // samples per bit
    if spb < 3.0 {
        return out; // too few samples per bit to decode
    }
    let mut i = 1;
    while i < x.len() {
        // falling edge = start of a start bit (or a break)
        if !(bit(i - 1) && !bit(i)) {
            i += 1;
            continue;
        }
        let start = i;
        let at = |k: f64| start + (k * spb) as usize;
        // low run length: a break is ≥ 13 bit times low
        let mut j = start;
        while j < x.len() && !bit(j) {
            j += 1;
        }
        if (j - start) as f64 >= 13.0 * spb * 0.95 {
            out.push((start, j, SerialItem::Break));
            i = j;
            continue;
        }
        let stop = at(9.5);
        if stop >= x.len() {
            break; // byte cut off at the end of the capture
        }
        let byte = (0..8).fold(0u8, |b, k| b | ((bit(at(1.5 + k as f64)) as u8) << k));
        let item = if bit(stop) {
            SerialItem::Byte(byte)
        } else {
            SerialItem::FrameError
        };
        out.push((start, at(10.0).min(x.len() - 1), item));
        i = at(9.5) + 1; // resync on the next falling edge after the stop bit's middle
    }
    out
}

/// LIN protected identifier → (frame id 0…63, parity ok).
pub fn lin_pid(pid: u8) -> (u8, bool) {
    let id = pid & 0x3F;
    let b = |n: u8| (id >> n) & 1;
    let p0 = b(0) ^ b(1) ^ b(2) ^ b(4);
    let p1 = 1 ^ (b(1) ^ b(3) ^ b(4) ^ b(5));
    (id, pid >> 6 == p0 | (p1 << 1))
}

/// A decoded CAN frame.
#[derive(Clone, Debug, PartialEq)]
pub struct CanFrame {
    pub id: u32,
    pub ext: bool,
    pub rtr: bool,
    pub data: Vec<u8>,
    pub crc_ok: bool,
}

/// CAN-15 CRC over destuffed bits (SOF … end of data).
pub fn can_crc15(bits: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in bits {
        let nxt = (b as u16) ^ ((crc >> 14) & 1);
        crc = (crc << 1) & 0x7FFF;
        if nxt == 1 {
            crc ^= 0x4599;
        }
    }
    crc
}

/// A standard CAN data frame as bus bits (dominant = 0), stuffed, with 12 idle bits before and after
/// (simulator and tests).
pub fn can_frame_bits(id: u32, data: &[u8]) -> Vec<u8> {
    let mut b: Vec<u8> = vec![0]; // SOF
    for k in (0..11).rev() {
        b.push(((id >> k) & 1) as u8);
    }
    b.extend([0, 0, 0]); // RTR, IDE, r0
    for k in (0..4).rev() {
        b.push(((data.len() >> k) & 1) as u8);
    }
    for d in data {
        for k in (0..8).rev() {
            b.push((d >> k) & 1);
        }
    }
    let crc = can_crc15(&b);
    for k in (0..15).rev() {
        b.push(((crc >> k) & 1) as u8);
    }
    let mut st = Vec::new();
    let (mut same, mut last) = (0, 2u8);
    for &x in &b {
        st.push(x);
        if x == last {
            same += 1
        } else {
            same = 1;
            last = x
        }
        if same == 5 {
            let s = 1 - x;
            st.push(s);
            same = 1;
            last = s;
        }
    }
    let mut all = vec![1u8; 12];
    all.extend(st);
    all.extend([1, 0, 1]); // CRC delimiter, ACK, ACK delimiter
    all.extend(vec![1u8; 12]);
    all
}

/// Destuffing bit reader over a raw CAN bit stream (bit, sample position).
struct Destuff<'a> {
    raw: &'a [(u8, usize)],
    pos: usize,
    same: u32,
    last: u8,
    stuffing: bool,
    /// destuffed bits read so far (for the CRC)
    bits: Vec<u8>,
}

impl Destuff<'_> {
    fn next(&mut self) -> Option<u8> {
        loop {
            let (b, _) = *self.raw.get(self.pos)?;
            self.pos += 1;
            if self.stuffing && self.same == 5 {
                if b == self.last {
                    return None; // stuff error
                }
                (self.same, self.last) = (1, b);
                continue;
            }
            if b == self.last {
                self.same += 1;
            } else {
                (self.same, self.last) = (1, b);
            }
            self.bits.push(b);
            return Some(b);
        }
    }
    fn read(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.next()? as u32;
        }
        Some(v)
    }
}

/// Decode CAN frames from one wire (CAN-H: dominant = high; CAN-L: dominant = low) or CAN-H − CAN-L.
/// With only ~3 samples per bit (this scope: 300 kS/s per channel) bits are recovered from the run lengths between
/// edges (rounded to whole bits), then destuffed; standard and extended frames, CRC-15 checked.
/// Returns (first sample, last sample, frame or None = frame start found but not decodable).
pub fn can_decode(
    x: &[f32],
    dt: f64,
    bitrate: f64,
    dominant_high: bool,
) -> Vec<(usize, usize, Option<CanFrame>)> {
    let mut out = Vec::new();
    if x.len() < 16 || bitrate <= 0.0 || dt <= 0.0 {
        return out;
    }
    let spb = 1.0 / (bitrate * dt);
    if spb < 1.8 {
        return out; // under ~2 samples per bit a CAN bit stream can't be recovered
    }
    let (lo, hi) = x
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    if hi - lo <= 0.0 {
        return out;
    }
    let thr = (lo + hi) / 2.0;
    // bit value per sample: dominant = 0, recessive = 1
    let level = |v: f32| -> u8 {
        if (v > thr) == dominant_high {
            0
        } else {
            1
        }
    };
    // runs: (start sample, length, bit)
    let mut runs: Vec<(usize, usize, u8)> = Vec::new();
    for (i, &v) in x.iter().enumerate() {
        let b = level(v);
        match runs.last_mut() {
            Some(r) if r.2 == b => r.1 += 1,
            _ => runs.push((i, 1, b)),
        }
    }
    let mut r = 1;
    while r < runs.len() {
        // start of frame: dominant after ≥ 7 recessive bits (end of frame / bus idle)
        if !(runs[r].2 == 0 && runs[r - 1].2 == 1 && runs[r - 1].1 as f64 >= 6.5 * spb) {
            r += 1;
            continue;
        }
        // expand the runs from here into a raw bit stream with sample positions
        let mut raw: Vec<(u8, usize)> = Vec::new();
        let mut k = r;
        while k < runs.len() && raw.len() < 200 {
            let (st, len, b) = runs[k];
            let n = ((len as f64 / spb).round() as usize).max(1);
            if b == 1 && n >= 7 && k > r {
                // a long recessive run ends the frame (EOF / idle) - keep enough bits for the tail
                for j in 0..n.min(10) {
                    raw.push((1, st + (j as f64 * spb) as usize));
                }
                break;
            }
            for j in 0..n {
                raw.push((b, st + (j as f64 * spb) as usize));
            }
            k += 1;
        }
        let frame_start = runs[r].0;
        // destuff and read fields
        let mut rd = Destuff {
            raw: &raw,
            pos: 0,
            same: 0,
            last: 2,
            stuffing: true,
            bits: Vec::new(),
        };
        let parsed = (|| {
            rd.read(1)?; // SOF
            let mut id = rd.read(11)?;
            let rtr_srr = rd.read(1)?;
            let ide = rd.read(1)? == 1;
            let rtr = if ide {
                id = (id << 18) | rd.read(18)?;
                let rtr = rd.read(1)? == 1;
                rd.read(2)?; // r1, r0
                rtr
            } else {
                rd.read(1)?; // r0
                rtr_srr == 1
            };
            let dlc = (rd.read(4)? as usize).min(8);
            let mut data = Vec::new();
            if !rtr {
                for _ in 0..dlc {
                    data.push(rd.read(8)? as u8);
                }
            }
            Some((id, ide, rtr, data))
        })();
        let Some((id, ext, rtr, data)) = parsed else {
            out.push((frame_start, raw.last().map_or(frame_start, |b| b.1), None));
            r = k.max(r + 1);
            continue;
        };
        let crc_calc = can_crc15(&rd.bits);
        let crc_rx = rd.read(15).map(|v| v as u16);
        let end = raw
            .get(rd.pos + 3)
            .or(raw.last())
            .map_or(frame_start, |b| b.1);
        out.push((
            frame_start,
            end,
            Some(CanFrame {
                id,
                ext,
                rtr,
                data,
                crc_ok: crc_rx == Some(crc_calc),
            }),
        ));
        r = k.max(r + 1);
    }
    out
}

/// Magnitude spectrum in dB (relative to full-scale 1 unit) of the largest power-of-two tail of `x`,
/// Hann window. Returns (bin spacing in Hz, magnitudes for bins 0 … N/2).
pub fn spectrum_db(x: &[f32], dt: f64) -> (f64, Vec<f32>) {
    let n = if x.len() < 8 {
        return (0.0, Vec::new());
    } else {
        1usize << (usize::BITS - 1 - x.len().leading_zeros())
    };
    let tail = &x[x.len() - n..];
    let mean = tail.iter().sum::<f32>() / n as f32;
    let mut re: Vec<f64> = tail
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos();
            (v - mean) as f64 * w
        })
        .collect();
    let mut im = vec![0.0f64; n];
    fft(&mut re, &mut im);
    // Hann coherent gain 0.5; single-sided ×2 → amplitude = |X| · 4 / n
    let mags = (0..=n / 2).map(|k| {
        let a = (re[k] * re[k] + im[k] * im[k]).sqrt() * 4.0 / n as f64;
        (20.0 * a.max(1e-12).log10()) as f32
    });
    (1.0 / (n as f64 * dt), mags.collect())
}

/// In-place radix-2 FFT (length must be a power of two).
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    // twiddle factors once per transform (n/2 sin/cos) instead of per butterfly (n/2·log2 n): ~2× faster
    // (speed review 2026-11-23); stage `len` uses every (n/len)-th entry
    let tw: Vec<(f64, f64)> = (0..n / 2)
        .map(|k| {
            let a = -2.0 * std::f64::consts::PI * k as f64 / n as f64;
            (a.cos(), a.sin())
        })
        .collect();
    let mut len = 2;
    while len <= n {
        let step = n / len;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (wr, wi) = tw[k * step];
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * wr - im[b] * wi, re[b] * wi + im[b] * wr);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowpass_step_reaches_63_percent_after_rc() {
        // fc = 1 kHz → RC = 159 µs; 1 µs samples → after ~159 samples the step is at ~63 %
        let x = vec![1.0f32; 2000];
        let y = lowpass(&[&[0.0f32][..], &x[..]].concat(), 1e-6, 1000.0);
        let at_rc = y[160];
        assert!((at_rc - 0.632).abs() < 0.02, "{at_rc}");
        assert!(y[1999] > 0.99);
    }

    #[test]
    fn rpm_from_60_minus_2_like_pulses() {
        // 10 pulses per revolution, pulse period 1 ms → 6000 rpm
        let dt = 1e-5;
        let x: Vec<f32> = (0..5000)
            .map(|i| if (i % 100) < 50 { 5.0 } else { 0.0 })
            .collect();
        let r = rpm_series(&x, dt, 10.0);
        let v = r
            .iter()
            .copied()
            .filter(|v| !v.is_nan())
            .collect::<Vec<_>>();
        assert!(!v.is_empty());
        assert!(
            v.iter().all(|&rpm| (rpm - 6000.0).abs() < 1.0),
            "{:?}",
            &v[..3]
        );
        assert!(r[0].is_nan()); // before the first edge
    }

    #[test]
    fn spectrum_peak_at_signal_frequency() {
        // 1 kHz sine, amplitude 2, 100 kS/s, 4096 samples
        let dt = 1e-5;
        let x: Vec<f32> = (0..4096)
            .map(|i| 2.0 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 * dt).sin() as f32)
            .collect();
        let (df, db) = spectrum_db(&x, dt);
        let (k, &peak) = db
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        assert!(
            (k as f64 * df - 1000.0).abs() < 2.0 * df,
            "peak at {} Hz",
            k as f64 * df
        );
        assert!((peak - 20.0 * 2.0f32.log10()).abs() < 1.5, "peak {peak} dB"); // ≈ 6 dB for amplitude 2
    }

    #[test]
    fn crank_60_2_teeth_numbered_from_the_gap() {
        // 60-2 wheel: 58 teeth then a gap worth 2 teeth; 20 samples per tooth; 2.5 revolutions
        let mut x = Vec::new();
        for rev in 0..3 {
            for t in 0..60 {
                let present = t < 58;
                for i in 0..20 {
                    x.push(if present && i < 10 { 1.0 } else { 0.0 });
                }
            }
            let _ = rev;
        }
        let teeth = crank_teeth(&x, 58);
        assert!(!teeth.is_empty());
        // tooth 0 right after each gap, every 58 teeth
        let zeros: Vec<usize> = teeth.iter().filter(|t| t.1 == 0).map(|t| t.0).collect();
        assert!(zeros.len() >= 2);
        assert_eq!(zeros[1] - zeros[0], 1200); // one revolution = 60 tooth positions × 20 samples
        assert_eq!(zeros[0] % 1200, 0); // first tooth of a revolution
        assert!(crank_teeth(&vec![0.5; 1000], 58).is_empty());
    }

    /// A UART frame as samples: idle high, bits LSB first, `spb` samples per bit.
    fn uart(bytes: &[u8], spb: usize, brk: bool) -> Vec<f32> {
        let mut x = vec![12.0f32; spb * 3];
        if brk {
            x.extend(vec![0.0; spb * 14]);
            x.extend(vec![12.0; spb]);
        }
        for &b in bytes {
            x.extend(vec![0.0; spb]); // start
            for k in 0..8 {
                x.extend(vec![if (b >> k) & 1 == 1 { 12.0 } else { 0.0 }; spb]);
            }
            x.extend(vec![12.0; spb * 2]); // stop + idle
        }
        x
    }

    #[test]
    fn lin_frame_decodes_break_sync_pid_data() {
        // LIN at 19.2 kbit/s sampled at 300 kS/s ≈ 15.6 samples/bit
        let dt = 1.0 / 300_000.0;
        let baud = 300_000.0 / 15.0;
        let x = uart(&[0x55, 0x3C, 0x12, 0xFF, 0x00], 15, true);
        let items: Vec<SerialItem> = uart_decode(&x, dt, baud).into_iter().map(|i| i.2).collect();
        assert_eq!(
            items,
            vec![
                SerialItem::Break,
                SerialItem::Byte(0x55),
                SerialItem::Byte(0x3C),
                SerialItem::Byte(0x12),
                SerialItem::Byte(0xFF),
                SerialItem::Byte(0x00)
            ]
        );
        // wrong baud rate → errors / wrong bytes, never a panic
        let _ = uart_decode(&x, dt, baud * 2.0);
        assert!(uart_decode(&vec![5.0; 500], dt, baud).is_empty());
        // PID: id 0x3C with correct parity bits = 0x3C (P1=0, P0=0)
        assert_eq!(lin_pid(0x3C), (0x3C, true));
        assert_eq!(lin_pid(0x7C).1, false);
    }

    fn render(bits: &[u8], spb: f64, high_dom: bool) -> Vec<f32> {
        let n = (bits.len() as f64 * spb) as usize;
        (0..n)
            .map(|i| {
                let dom = bits[(i as f64 / spb) as usize] == 0;
                if dom == high_dom {
                    3.5
                } else {
                    2.5
                }
            })
            .collect()
    }

    #[test]
    fn can_frames_decode_at_3_samples_per_bit() {
        let bits = can_frame_bits(0x123, &[0x11, 0x22, 0x33, 0xFF, 0x00]);
        // 3-4 samples/bit: 100-125 kbit/s at 300 kS/s; 4.8: 500 kbit/s in the fast 1-channel mode (2.4 MS/s)
        for (spb, high) in [(3.0, true), (4.0, false), (3.3, true), (4.8, true)] {
            let x = render(&bits, spb, high);
            let dt = 1.0 / 300_000.0;
            let f = can_decode(&x, dt, 1.0 / (spb * dt), high);
            assert_eq!(f.len(), 1, "spb {spb}");
            let fr = f[0].2.clone().expect("decoded");
            assert_eq!(
                (fr.id, fr.ext, fr.rtr, fr.data.as_slice(), fr.crc_ok),
                (
                    0x123,
                    false,
                    false,
                    &[0x11, 0x22, 0x33, 0xFF, 0x00][..],
                    true
                ),
                "spb {spb}"
            );
        }
        // a flipped data bit → CRC mismatch is reported, not hidden
        let mut bad = bits.clone();
        let flip = 12 + 30;
        bad[flip] ^= 1;
        let f = can_decode(&render(&bad, 4.0, true), 1.0 / 300_000.0, 75_000.0, true);
        assert!(f.iter().all(|x| x
            .2
            .as_ref()
            .is_none_or(|fr| !fr.crc_ok || fr.data != [0x11, 0x22, 0x33, 0xFF, 0x00])));
        // too fast for the sample rate → nothing, no panic
        assert!(can_decode(&render(&bits, 4.0, true), 1.0 / 300_000.0, 500_000.0, true).is_empty());
    }
}
