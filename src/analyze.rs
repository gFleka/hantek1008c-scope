//! Waveform phase analyzers (Phase 4): for a known signal type, find the phases of the waveform (e.g. injector open /
//! flyback spike / pintle close; ignition dwell / spike / burn / ringing) and the numbers a mechanic reads from them.
//! Pure functions on samples in display units (V, A) — tested here; the UI only draws the result.

/// What kind of signal a channel carries (side panel / guided tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    None,
    Injector,
    IgnitionPrimary,
    /// digital pulses active high (Hall sensor, ignition trigger)
    PulseHigh,
    /// digital pulses active low (PWM valve / low-side driven actuator)
    PulseLow,
    CrankVr,
    Lambda,
    Battery,
    Compression,
    /// toothed-wheel speed sensor, VR (sine) or Hall / active (square): ABS wheel, vehicle / gearbox speed
    WheelSpeed,
}

// new kinds go at the end: settings store the index
pub const KINDS: [(Kind, &str); 10] = [
    (Kind::None, "—"),
    (Kind::Injector, "Injector (voltage)"),
    (Kind::IgnitionPrimary, "Ignition primary (coil −)"),
    (Kind::PulseHigh, "Pulse, active high (Hall / trigger)"),
    (Kind::PulseLow, "Pulse, active low (PWM valve)"),
    (Kind::CrankVr, "Crank sensor, inductive (missing tooth)"),
    (Kind::Lambda, "Lambda, narrowband zirconia (0–1 V)"),
    (Kind::Battery, "Battery / cranking voltage"),
    (Kind::Compression, "Relative compression (current)"),
    (
        Kind::WheelSpeed,
        "Speed sensor, toothed wheel (ABS, VSS; VR or Hall)",
    ),
];

/// Phase types (drawing colour).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    Active,
    Spike,
    Burn,
    Ringing,
    Event,
    Gap,
    Rich,
    Lean,
    Dip,
    Hump,
    Warn,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Phase {
    pub s0: usize,
    pub s1: usize,
    pub tag: Tag,
    pub label: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Analysis {
    pub phases: Vec<Phase>,
    /// (name, value) lines for the readout box
    pub readouts: Vec<(String, String)>,
    /// overall verdict (ok?, text), where a rule of thumb exists
    pub verdict: Option<(bool, String)>,
    /// sample spacing the analysis ran with (s)
    pub dt: f64,
}

fn ms(s: f64) -> String {
    if s >= 1.0 {
        format!("{s:.2} s")
    } else {
        format!("{:.2} ms", s * 1e3)
    }
}

fn median(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    let mut v = x.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

/// Runs where `pred` holds, at least `min` samples long: (start, end exclusive).
fn runs(x: &[f32], min: usize, pred: impl Fn(f32) -> bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, &v) in x.iter().enumerate() {
        match (pred(v), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                if i - s >= min {
                    out.push((s, i));
                }
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        if x.len() - s >= min {
            out.push((s, x.len()));
        }
    }
    out
}

fn ph(s0: usize, s1: usize, tag: Tag, label: impl Into<String>) -> Phase {
    Phase {
        s0,
        s1,
        tag,
        label: label.into(),
    }
}

/// Analyse `x` (display units) sampled every `dt` seconds as signal `kind` (crank wheel 60-2).
#[cfg_attr(not(test), allow(dead_code))]
pub fn analyze(kind: Kind, x: &[f32], dt: f64) -> Analysis {
    analyze_with(kind, x, dt, (60, 2))
}

/// Like `analyze`, with the crank wheel (tooth positions incl. missing, missing teeth) from Settings > Engine.
pub fn analyze_with(kind: Kind, x: &[f32], dt: f64, wheel: (u32, u32)) -> Analysis {
    if x.len() < 16 || dt <= 0.0 || x.iter().any(|v| !v.is_finite()) {
        return Analysis::default();
    }
    let n_ms = |t: f64| ((t / dt).round() as usize).max(1);
    let mut a = match kind {
        Kind::None => Analysis::default(),
        Kind::Injector => injector(x, dt, n_ms),
        Kind::IgnitionPrimary => ignition(x, dt, n_ms),
        Kind::PulseHigh | Kind::PulseLow => pulses(x, dt, kind == Kind::PulseLow),
        Kind::CrankVr => crank(x, dt, wheel),
        Kind::Lambda => lambda(x, dt),
        Kind::Battery => battery(x, dt),
        Kind::Compression => compression(x),
        Kind::WheelSpeed => wheel_speed(x, dt),
    };
    a.dt = dt;
    a
}

/// Low-side driven injector seen on its switched wire: battery voltage, pulled to ~0 V while open, flyback spike
/// when it closes, and a small bump in the decay when the pintle seats (mechanical close).
fn injector(x: &[f32], dt: f64, n_ms: impl Fn(f64) -> usize) -> Analysis {
    let bat = median(x).max(1.0);
    let mut a = Analysis::default();
    let opens = runs(x, n_ms(0.2e-3), |v| v < 0.3 * bat);
    let mut peaks = Vec::new();
    let mut closes = Vec::new();
    for &(s, e) in &opens {
        a.phases.push(ph(
            s,
            e,
            Tag::Active,
            format!("open {}", ms((e - s) as f64 * dt)),
        ));
        // flyback spike: the maximum within 2 ms after the driver switches off
        let w = (e + n_ms(2e-3)).min(x.len());
        if e >= w {
            continue;
        }
        let (pi, &pv) = x[e..w]
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap();
        if pv > 1.5 * bat {
            let se = x[e + pi..w]
                .iter()
                .position(|&v| v < 1.2 * bat)
                .map_or(w, |k| e + pi + k);
            a.phases
                .push(ph(e, se, Tag::Spike, format!("spike {pv:.0} V")));
            peaks.push(pv);
            // pintle close: after the spike, the decay rises again by > 3 % of battery (a bump) within 3 ms
            let w2 = (se + n_ms(3e-3)).min(x.len());
            let mut low = f32::MAX;
            for k in se..w2 {
                low = low.min(x[k]);
                if x[k] > low + 0.03 * bat && k + 2 < w2 && x[k] >= x[k + 1] && x[k] >= x[k - 1] {
                    a.phases.push(ph(
                        k.saturating_sub(n_ms(0.1e-3)),
                        (k + n_ms(0.1e-3)).min(x.len()),
                        Tag::Event,
                        "pintle closes",
                    ));
                    closes.push((k - e) as f64 * dt);
                    break;
                }
            }
        }
    }
    if opens.is_empty() {
        a.readouts.push((
            "injector".into(),
            "no opening pulse found (driver never pulls low)".into(),
        ));
        a.verdict = Some((false, "no injection pulse".into()));
        return a;
    }
    // timings from complete pulses only (one cut off at the capture edge would pull the average down)
    let whole: Vec<&(usize, usize)> = opens.iter().filter(|r| r.0 > 0 && r.1 < x.len()).collect();
    let src: Vec<&(usize, usize)> = if whole.is_empty() {
        opens.iter().collect()
    } else {
        whole
    };
    let t_open = src.iter().map(|&&(s, e)| (e - s) as f64 * dt).sum::<f64>() / src.len() as f64;
    a.readouts.push(("injection time".into(), ms(t_open)));
    a.readouts.push(("battery".into(), format!("{bat:.1} V")));
    if let Some(p) = peaks.iter().copied().reduce(f32::max) {
        a.readouts
            .push(("flyback peak".into(), format!("{p:.0} V")));
    }
    if !closes.is_empty() {
        a.readouts.push((
            "close delay".into(),
            ms(closes.iter().sum::<f64>() / closes.len() as f64),
        ));
    }
    if opens.len() >= 2 {
        let period = (opens[opens.len() - 1].0 - opens[0].0) as f64 * dt / (opens.len() - 1) as f64;
        a.readouts
            .push(("duty".into(), format!("{:.1} %", t_open / period * 100.0)));
    }
    a.verdict = Some(if peaks.is_empty() {
        (
            false,
            "no flyback spike — injector coil open or clamped driver".into(),
        )
    } else {
        (true, "opens and closes".into())
    });
    a
}

/// Ignition coil primary (coil −): battery, pulled low during dwell (coil charging), a high firing spike, the burn
/// (spark) line, then ringing back to battery.
fn ignition(x: &[f32], dt: f64, n_ms: impl Fn(f64) -> usize) -> Analysis {
    let bat = median(x).max(1.0);
    let mut a = Analysis::default();
    let dwells = runs(x, n_ms(0.5e-3), |v| v < 0.3 * bat);
    let n = x.len();
    let (mut dw, mut pk, mut burn, mut burn_v) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for &(s, e) in &dwells {
        a.phases.push(ph(
            s,
            e,
            Tag::Active,
            format!("dwell {}", ms((e - s) as f64 * dt)),
        ));
        if s > 0 && e < n {
            dw.push((e - s) as f64 * dt); // complete dwell periods only
        }
        let w = (e + n_ms(5e-3)).min(x.len());
        if e >= w {
            continue;
        }
        let (pi, &pv) = x[e..w]
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap();
        // the ~0.1 ms firing spike can fall between samples at slow timebases: the burn is looked for either way
        let spike_end = if pv >= 3.0 * bat {
            let se = x[e + pi..w]
                .iter()
                .position(|&v| v < 0.5 * pv)
                .map_or(w, |k| e + pi + k);
            a.phases
                .push(ph(e, se, Tag::Spike, format!("spike {pv:.0} V")));
            pk.push(pv);
            se
        } else {
            e
        };
        // burn line: stays above 1.6 × battery after the spike (or right after the dwell if the spike wasn't sampled)
        let bs = (spike_end..w).find(|&k| x[k] > 1.6 * bat).unwrap_or(w);
        let spike_end = if spike_end == e { bs } else { spike_end };
        let be = x[spike_end..w]
            .iter()
            .position(|&v| v < 1.6 * bat)
            .map_or(w, |k| spike_end + k);
        if be > spike_end + 2 {
            let mean = x[spike_end..be].iter().sum::<f32>() / (be - spike_end) as f32;
            a.phases.push(ph(
                spike_end,
                be,
                Tag::Burn,
                format!("burn {}", ms((be - spike_end) as f64 * dt)),
            ));
            burn.push((be - spike_end) as f64 * dt);
            burn_v.push(mean);
            // ringing: up to 2 ms after the burn while it still swings > 20 % around battery
            let we = (be + n_ms(2e-3)).min(x.len());
            let re = (be..we)
                .rev()
                .find(|&k| (x[k] - bat).abs() > 0.2 * bat)
                .unwrap_or(be);
            if re > be + 2 {
                a.phases.push(ph(be, re, Tag::Ringing, "coil ringing"));
            }
        }
    }
    if dwells.is_empty() {
        a.readouts
            .push(("ignition".into(), "no dwell found".into()));
        a.verdict = Some((
            false,
            "no dwell — no trigger from the ECU / output stage".into(),
        ));
        return a;
    }
    let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    a.readouts.push(("dwell".into(), ms(avg(&dw))));
    if let Some(p) = pk.iter().copied().reduce(f32::max) {
        a.readouts.push(("firing peak".into(), format!("{p:.0} V")));
    }
    if !burn.is_empty() {
        a.readouts.push(("burn time".into(), ms(avg(&burn))));
        a.readouts.push((
            "burn voltage".into(),
            format!("{:.0} V", burn_v.iter().sum::<f32>() / burn_v.len() as f32),
        ));
    }
    a.verdict = Some(match (pk.is_empty() && burn.is_empty(), burn.is_empty()) {
        (true, _) => (
            false,
            "dwell but no firing spike — coil / output stage".into(),
        ),
        (false, true) => (
            false,
            "spike but no burn line — no spark (open HT, gap too big)".into(),
        ),
        _ => {
            let b = avg(&burn);
            if !(0.6e-3..=3.0e-3).contains(&b) {
                (
                    false,
                    format!(
                        "burn {} — typical 0.8–2 ms (plugs / leads / mixture)",
                        ms(b)
                    ),
                )
            } else {
                (true, "dwell, spike and burn as expected".into())
            }
        }
    });
    a
}

/// Digital pulses (Hall, ignition trigger, PWM): active level runs, frequency, duty, width.
fn pulses(x: &[f32], dt: f64, active_low: bool) -> Analysis {
    let (lo, hi) = x
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let mut a = Analysis::default();
    if hi - lo < 0.5 {
        a.readouts.push((
            "signal".into(),
            format!("flat at {:.2} V — no switching", (lo + hi) / 2.0),
        ));
        a.verdict = Some((false, "no pulses".into()));
        return a;
    }
    let mid = (lo + hi) / 2.0;
    let act = runs(x, 2, |v| (v > mid) != active_low);
    for &(s, e) in &act {
        a.phases
            .push(ph(s, e, Tag::Active, ms((e - s) as f64 * dt)));
    }
    let complete: Vec<&(usize, usize)> = act.iter().filter(|r| r.0 > 0 && r.1 < x.len()).collect();
    if complete.len() >= 2 {
        let period = (complete[complete.len() - 1].0 - complete[0].0) as f64 * dt
            / (complete.len() - 1) as f64;
        let w = complete
            .iter()
            .map(|r| (r.1 - r.0) as f64 * dt)
            .sum::<f64>()
            / complete.len() as f64;
        a.readouts
            .push(("frequency".into(), format!("{:.2} Hz", 1.0 / period)));
        a.readouts.push(("active width".into(), ms(w)));
        a.readouts.push((
            "duty (active)".into(),
            format!("{:.1} %", w / period * 100.0),
        ));
    } else if let Some(r) = complete.first() {
        a.readouts
            .push(("active width".into(), ms((r.1 - r.0) as f64 * dt)));
    }
    a.readouts
        .push(("levels".into(), format!("{lo:.2} … {hi:.2} V")));
    a
}

/// Inductive crank sensor on a missing-tooth wheel: the gap, irregular teeth, rpm and amplitude.
fn crank(x: &[f32], dt: f64, (total, missing): (u32, u32)) -> Analysis {
    let mut a = Analysis::default();
    let present = total.saturating_sub(missing).max(1);
    let teeth = crate::dsp::crank_teeth(x, present);
    let (lo, hi) = x
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    a.readouts
        .push(("amplitude".into(), format!("{:.2} V p-p", hi - lo)));
    if teeth.len() < 4 {
        a.readouts
            .push(("teeth".into(), "no tooth pattern / no gap in view".into()));
        a.verdict = Some((
            false,
            format!("no {total}-{missing} pattern (signal, timebase or wheel setting?)"),
        ));
        return a;
    }
    let iv: Vec<usize> = teeth.windows(2).map(|w| w[1].0 - w[0].0).collect();
    let normal = {
        let mut v: Vec<usize> = teeth
            .windows(2)
            .filter(|w| w[1].1 != 0)
            .map(|w| w[1].0 - w[0].0)
            .collect();
        v.sort();
        v.get(v.len() / 2).copied().unwrap_or(1).max(1)
    };
    // tooth timing is only as exact as the sampling: below ~5 samples per tooth the intervals jitter by ±20 % and more
    if normal < 4 {
        a.readouts
            .push(("samples per tooth".into(), format!("{normal}")));
        a.verdict = Some((false, format!("only {normal} samples per tooth — too few to judge the teeth: faster timebase or fewer channels")));
        return a;
    }
    let mut irregular = 0;
    for (k, w) in teeth.windows(2).enumerate() {
        let ratio = iv[k] as f32 / normal as f32;
        // the real gap spans (missing + 1) tooth spacings: 3 on 60-2, 2 on 36-1
        if w[1].1 == 0 && (ratio - (missing + 1) as f32).abs() <= 0.5 {
            a.phases
                .push(ph(w[0].0, w[1].0, Tag::Gap, "gap (missing teeth)"));
        } else if w[1].1 == 0 {
            // a renumbering interval that doesn't match the wheel's gap: a missing / damaged tooth
            irregular += 1;
            a.phases
                .push(ph(w[0].0, w[1].0, Tag::Warn, "missing tooth"));
        } else {
            let d = iv[k] as f32 / normal as f32;
            // ±1 sample of timing error is normal: the tolerance widens at few samples per tooth
            let tol = (1.5 / normal as f32).max(0.25);
            if !(1.0 - tol..=1.0 + tol).contains(&d) {
                irregular += 1;
                a.phases
                    .push(ph(w[0].0, w[1].0, Tag::Warn, "irregular tooth"));
            }
        }
    }
    a.readouts.push((
        "rpm".into(),
        format!("{:.0}", 60.0 / (normal as f64 * dt * total as f64)),
    ));
    a.verdict = Some(if irregular > 0 {
        (
            false,
            format!("{irregular} irregular tooth interval(s) — wheel damage, runout or noise"),
        )
    } else {
        (true, "regular teeth and gap".into())
    });
    a
}

/// Narrowband lambda: rich (> 0.45 V) / lean, switching rate, range, lean → rich response time (0.3 → 0.6 V).
fn lambda(x: &[f32], dt: f64) -> Analysis {
    let mut a = Analysis::default();
    for (s, e) in runs(x, 2, |v| v > 0.45) {
        a.phases.push(ph(s, e, Tag::Rich, "rich"));
    }
    for (s, e) in runs(x, 2, |v| v <= 0.45) {
        a.phases.push(ph(s, e, Tag::Lean, "lean"));
    }
    let (lo, hi) = x
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    // switching frequency between the first and the last switch (whole cycles only)
    let sw: Vec<usize> = x
        .windows(2)
        .enumerate()
        .filter(|(_, w)| (w[0] > 0.45) != (w[1] > 0.45))
        .map(|(i, _)| i)
        .collect();
    if sw.len() >= 3 {
        let span = (sw[sw.len() - 1] - sw[0]) as f64 * dt;
        a.readouts.push((
            "switching".into(),
            format!("{:.2} Hz", (sw.len() - 1) as f64 / 2.0 / span),
        ));
    } else {
        a.readouts.push(("switching".into(), "none in view".into()));
    }
    a.readouts
        .push(("range".into(), format!("{lo:.2} … {hi:.2} V")));
    // lean → rich response: from crossing 0.3 V upwards to crossing 0.6 V
    let mut resp = Vec::new();
    let mut from = None;
    for (i, w) in x.windows(2).enumerate() {
        if w[0] < 0.3 && w[1] >= 0.3 {
            from = Some(i);
        }
        if let (Some(f), true) = (from, w[0] < 0.6 && w[1] >= 0.6) {
            resp.push((i - f) as f64 * dt);
            from = None;
        }
    }
    if !resp.is_empty() {
        a.readouts.push((
            "lean to rich".into(),
            ms(resp.iter().sum::<f64>() / resp.len() as f64),
        ));
    }
    a.verdict = Some(if lo < -0.3 || hi > 1.3 {
        (false, format!("{lo:.1} … {hi:.1} V is not a narrowband lambda signal (0–1 V): wrong wire, probe or V/div range"))
    } else if hi < 0.6 || lo > 0.3 {
        (
            false,
            "not switching across 0.3–0.6 V (sensor cold/dead or mixture stuck)".into(),
        )
    } else if resp.iter().any(|&r| r > 0.3) {
        (false, "slow response (> 300 ms) — aged sensor".into())
    } else {
        (true, "switching rich / lean".into())
    });
    a
}

/// Battery during a start: rest voltage, the cranking dip, charging voltage once running.
fn battery(x: &[f32], dt: f64) -> Analysis {
    let mut a = Analysis::default();
    let n = x.len();
    let rest = median(&x[..n / 10]);
    let end = median(&x[n - n / 10..]);
    let (mi, &min) = x
        .iter()
        .enumerate()
        .min_by(|a, b| a.1.total_cmp(b.1))
        .unwrap();
    if rest - min > 0.8 {
        // the dip: around the minimum while below rest − 0.5 V
        let s = (0..mi)
            .rev()
            .find(|&k| x[k] > rest - 0.5)
            .map_or(0, |k| k + 1);
        let e = (mi..n).find(|&k| x[k] > rest - 0.5).unwrap_or(n);
        a.phases.push(ph(
            s,
            e,
            Tag::Dip,
            format!("cranking {}", ms((e - s) as f64 * dt)),
        ));
    }
    a.readouts.push(("rest".into(), format!("{rest:.2} V")));
    a.readouts.push(("minimum".into(), format!("{min:.2} V")));
    a.readouts
        .push(("at the end".into(), format!("{end:.2} V")));
    let cranked = rest - min > 0.8;
    a.verdict = Some(if min < 9.6 {
        (
            false,
            format!(
                "drops to {min:.1} V while cranking (< 9.6 V): weak battery / cables / starter"
            ),
        )
    } else if end > 14.8 {
        (false, format!("{end:.1} V: overcharging (regulator)"))
    } else if cranked && end < rest - 0.5 {
        (true, format!("cranking seen (minimum {min:.1} V is fine) — still cranking at the end of the view; let the engine run to judge charging"))
    } else if cranked && end < 13.2 {
        (
            false,
            format!("{end:.1} V after the start: not charging (alternator / regulator / belt)"),
        )
    } else if cranked && end < 13.6 {
        (false, format!("{end:.1} V running: low charging voltage"))
    } else if !cranked {
        (true, format!("no cranking seen yet — battery {rest:.1} V (≥ 12.4 V = charged); start the engine to test cranking and charging"))
    } else {
        (true, "cranking and charging OK".into())
    });
    a
}

/// Starter current while cranking without starting: one hump per compression stroke; equal humps = even compression.
fn compression(x: &[f32]) -> Analysis {
    let mut a = Analysis::default();
    let (lo, hi) = x
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    // humps: runs above a quarter of the swing (half would miss a weak cylinder entirely), peak of each
    let mid = lo + (hi - lo) * 0.25;
    let hs = runs(x, 3, |v| v > mid);
    let peaks: Vec<f32> = hs
        .iter()
        .map(|&(s, e)| x[s..e].iter().copied().fold(f32::MIN, f32::max))
        .collect();
    for (k, &(s, e)) in hs.iter().enumerate() {
        a.phases
            .push(ph(s, e, Tag::Hump, format!("{:.0}", peaks[k])));
    }
    if peaks.len() < 3 {
        a.readouts.push((
            "humps".into(),
            "fewer than 3 compression humps in view".into(),
        ));
        return a;
    }
    let mean = peaks.iter().sum::<f32>() / peaks.len() as f32;
    let (pmin, pmax) = peaks
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let spread = (pmax - pmin) / mean.max(1e-6) * 100.0;
    a.readouts.push(("humps".into(), peaks.len().to_string()));
    a.readouts.push(("peak mean".into(), format!("{mean:.0}")));
    a.readouts.push(("spread".into(), format!("{spread:.0} %")));
    a.verdict = Some(if spread > 15.0 {
        (
            false,
            format!("humps differ by {spread:.0} % — check compression"),
        )
    } else {
        (true, "even compression".into())
    });
    a
}

/// Toothed-wheel speed sensor (ABS wheel, vehicle / gearbox speed), passive VR (sine, amplitude grows with speed) or
/// active Hall (square): frequency, tooth-to-tooth regularity, missing / damaged teeth, amplitude wobble (VR: runout,
/// air gap, bearing play).
fn wheel_speed(x: &[f32], dt: f64) -> Analysis {
    let mut a = Analysis::default();
    let (lo, hi) = x
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    a.readouts
        .push(("amplitude".into(), format!("{:.2} V p-p", hi - lo)));
    if hi - lo < 0.05 {
        a.verdict = Some((
            false,
            "no signal (wheel not turning, open circuit or no supply for an active sensor)".into(),
        ));
        return a;
    }
    let mid = (lo + hi) / 2.0;
    let (on, off) = (mid + 0.1 * (hi - lo), mid - 0.1 * (hi - lo));
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
    if edges.len() < 4 {
        a.readouts.push((
            "teeth".into(),
            "too few teeth in view (slower timebase or turn the wheel faster)".into(),
        ));
        return a;
    }
    let iv: Vec<f64> = edges.windows(2).map(|w| (w[1] - w[0]) as f64).collect();
    let mut sorted = iv.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let med = sorted[sorted.len() / 2].max(1.0);
    let mut faults = 0;
    for (k, w) in edges.windows(2).enumerate() {
        let r = iv[k] / med;
        if r > 1.6 {
            faults += 1;
            a.phases
                .push(ph(w[0], w[1], Tag::Warn, "missing / damaged tooth"));
        } else if r < 0.6 {
            faults += 1;
            a.phases
                .push(ph(w[0], w[1], Tag::Warn, "extra edge (noise)"));
        }
    }
    let normal: Vec<f64> = iv
        .iter()
        .copied()
        .filter(|v| (0.6..=1.6).contains(&(v / med)))
        .collect();
    let mean = normal.iter().sum::<f64>() / normal.len().max(1) as f64;
    let sd = (normal.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / normal.len().max(1) as f64)
        .sqrt();
    let jitter = sd / mean.max(1e-9) * 100.0;
    // amplitude per tooth cycle (VR): peak-to-peak between consecutive edges
    let peaks: Vec<f32> = edges
        .windows(2)
        .map(|w| {
            let (l, h) = x[w[0]..w[1]]
                .iter()
                .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
            h - l
        })
        .collect();
    let (pmin, pmax) = peaks
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let wobble = (pmax - pmin) / pmax.max(1e-6) * 100.0;
    a.readouts
        .push(("frequency".into(), format!("{:.1} Hz", 1.0 / (mean * dt))));
    a.readouts
        .push(("tooth-to-tooth".into(), format!("{jitter:.1} % jitter")));
    a.readouts
        .push(("amplitude wobble".into(), format!("{wobble:.0} %")));
    a.verdict = Some(if faults > 0 {
        (false, format!("{faults} irregular tooth interval(s) — damaged / dirty tone ring, missing tooth or noise"))
    } else if jitter > 8.0 {
        (
            false,
            format!("uneven teeth ({jitter:.0} % jitter) — tone ring damage or bearing play"),
        )
    } else if wobble > 40.0 {
        (
            false,
            format!("amplitude varies {wobble:.0} % — runout, bent ring or varying air gap (VR)"),
        )
    } else {
        (true, "regular teeth, steady signal".into())
    });
    a
}

/// Smallest swing (signal units) that is a real signal of this type; less is noise / nothing connected.
pub fn min_swing(kind: Kind) -> f32 {
    match kind {
        Kind::CrankVr | Kind::WheelSpeed | Kind::Lambda => 0.15,
        Kind::PulseHigh | Kind::PulseLow => 1.0,
        Kind::Injector | Kind::IgnitionPrimary => 4.0,
        Kind::Compression => 15.0,
        Kind::Battery | Kind::None => 0.0,
    }
}

/// What to do while waiting for a signal of this type.
pub fn waiting_hint(kind: Kind) -> &'static str {
    match kind {
        Kind::WheelSpeed => "Turn the wheel (by hand is enough) or drive slowly.",
        Kind::CrankVr | Kind::Compression => "Crank or start the engine.",
        Kind::Injector | Kind::IgnitionPrimary | Kind::PulseHigh | Kind::PulseLow => {
            "Start the engine (or crank it)."
        }
        Kind::Lambda => "Warm engine, hold about 2500 rpm.",
        Kind::Battery => "Clip onto the battery + and − terminals.",
        Kind::None => "",
    }
}

/// Is there a signal of this type at all? None = yes; Some(why not) = still waiting (nothing connected, too small, …).
/// `noise` = the hardware noise floor in signal units (swing an open input can show on this range), `unit` for the text.
pub fn waiting_for(kind: Kind, x: &[f32], noise: f32, unit: &str) -> Option<String> {
    if x.len() < 2 {
        return Some("no data".into());
    }
    let (lo, hi) = x
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let mean = x.iter().sum::<f32>() / x.len() as f32;
    if kind == Kind::Battery {
        return (mean < 6.0).then(|| format!("{mean:.2} {unit} — that is not a 12 V battery"));
    }
    let need = min_swing(kind).max(noise);
    if hi - lo >= need {
        None
    } else if hi - lo < noise {
        Some(format!(
            "flat at {mean:.2} {unit} (only noise) — probe tip or ground clip not connected?"
        ))
    } else {
        Some(format!("{:.2} {unit} p-p is too small for this signal (needs ≥ {need:.2}) — smaller V/div or check the connection", hi - lo))
    }
}

/// What the typical curve is fitted to from the live signal: measured period (pulse / tooth), the signal's low and high
/// level, the time of the first tooth after the crank gap (relative to the trigger) and the crank wheel.
#[derive(Clone, Copy, Default)]
pub struct Hint {
    pub period: Option<f64>,
    pub lo: f32,
    pub hi: f32,
    pub gap_t: Option<f64>,
    pub wheel: (u32, u32),
}

/// Typical healthy levels drawn as dashed lines (value, label) for slow signals that have no trigger-aligned shape.
pub fn bands(kind: Kind) -> &'static [(f32, &'static str)] {
    match kind {
        Kind::Lambda => &[
            (0.8, "rich 0.8–0.9 V"),
            (0.45, "switch point 0.45 V"),
            (0.15, "lean 0.1–0.2 V"),
        ],
        Kind::Battery => &[
            (14.4, "charging 13.8–14.7 V"),
            (12.6, "rested, charged ≥ 12.4 V"),
            (9.6, "cranking minimum 9.6 V"),
        ],
        _ => &[],
    }
}

/// A typical healthy waveform for a signal type, `t` seconds after its trigger event (the falling edge into injector
/// open / ignition dwell, the active edge of a pulse or tooth), in the signal's units, fitted with `h`.
/// None = no model for this type / outside the modelled part. Built-in reference curves.
pub fn model(kind: Kind, t: f64, h: &Hint) -> Option<f32> {
    let bat = 13.8f32;
    let period = h.period;
    match kind {
        // toothed wheel (VR sine): measured tooth period and amplitude; crank: missing teeth where the gap is
        Kind::CrankVr | Kind::WheelSpeed => {
            let p = period?;
            let (mid, amp) = ((h.lo + h.hi) / 2.0, (h.hi - h.lo) / 2.0);
            let t0 = if kind == Kind::CrankVr { h.gap_t? } else { 0.0 };
            let u = (t - t0) / p;
            let (total, missing) = h.wheel;
            if kind == Kind::CrankVr
                && (u.floor() as i64).rem_euclid(total.max(1) as i64)
                    >= (total - missing.min(total)) as i64
            {
                return Some(mid);
            }
            Some(mid + amp * (u.fract() * std::f64::consts::TAU).sin() as f32)
        }
        Kind::Injector => Some(match t {
            t if t < 0.0 => bat,
            t if t < 3.0e-3 => 0.4,
            t if t < 3.15e-3 => 70.0,
            t => {
                let u = (t - 3.15e-3) as f32;
                bat + 4.0 * (-u / 0.0003).exp()
                    + if (0.0006..0.0008).contains(&u) {
                        1.0
                    } else {
                        0.0
                    }
            }
        }),
        Kind::IgnitionPrimary => Some(match t {
            t if t < 0.0 => bat,
            t if t < 3.0e-3 => 0.8,
            t if t < 3.1e-3 => 350.0,
            t if t < 4.6e-3 => 38.0,
            t if t < 6.0e-3 => {
                bat + 12.0
                    * ((t - 4.6e-3) * std::f64::consts::TAU * 3000.0).sin() as f32
                    * (-((t - 4.6e-3) / 0.0006) as f32).exp()
            }
            _ => bat,
        }),
        Kind::PulseHigh | Kind::PulseLow => {
            let p = period?;
            let active = (t.rem_euclid(p)) < p * 0.5;
            let high = if kind == Kind::PulseHigh {
                active
            } else {
                !active
            };
            Some(if high {
                if kind == Kind::PulseHigh {
                    5.0
                } else {
                    bat
                }
            } else {
                0.2
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f64 = 1e-5; // 100 kS/s

    /// Injector: 13.8 V, open 3 ms every 20 ms, flyback 70 V decaying, pintle bump 0.8 ms after close.
    fn injector_sig(spike: bool) -> Vec<f32> {
        (0..6000)
            .map(|i| {
                let t = (i as f64 * DT) % 0.02;
                if (0.002..0.005).contains(&t) {
                    0.4
                } else if spike && (0.005..0.0052).contains(&t) {
                    70.0
                } else if spike && (0.0052..0.008).contains(&t) {
                    let u = t - 0.0052;
                    let bump = if (0.0006..0.0008).contains(&u) {
                        1.2
                    } else {
                        0.0
                    };
                    13.8 + 4.0 * (-u / 0.0003).exp() as f32 + bump
                } else {
                    13.8
                }
            })
            .collect()
    }

    #[test]
    fn injector_phases_and_numbers() {
        let a = analyze(Kind::Injector, &injector_sig(true), DT);
        let tags: Vec<Tag> = a.phases.iter().map(|p| p.tag).collect();
        assert!(
            tags.contains(&Tag::Active) && tags.contains(&Tag::Spike) && tags.contains(&Tag::Event),
            "{tags:?}"
        );
        let get = |k: &str| {
            a.readouts
                .iter()
                .find(|r| r.0 == k)
                .map(|r| r.1.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("injection time"), "3.00 ms");
        assert_eq!(get("flyback peak"), "70 V");
        assert_eq!(get("duty"), "15.0 %");
        assert!(a.verdict.as_ref().unwrap().0);
        // MC/DC: no spike → verdict fails; never pulled low → no pulse
        assert!(
            !analyze(Kind::Injector, &injector_sig(false), DT)
                .verdict
                .unwrap()
                .0
        );
        assert!(
            !analyze(Kind::Injector, &vec![13.8; 3000], DT)
                .verdict
                .unwrap()
                .0
        );
    }

    /// Ignition primary: 13.8 V, dwell 3 ms, spike 350 V, burn 1.5 ms at 40 V, ringing.
    fn ign_sig(burn_ms: f64) -> Vec<f32> {
        (0..4000)
            .map(|i| {
                let t = i as f64 * DT;
                let (d0, d1) = (0.005, 0.008);
                let b1 = d1 + 0.0001 + burn_ms * 1e-3;
                if (d0..d1).contains(&t) {
                    0.8
                } else if (d1..d1 + 0.0001).contains(&t) {
                    350.0
                } else if (d1 + 0.0001..b1).contains(&t) {
                    40.0
                } else if (b1..b1 + 0.0015).contains(&t) {
                    13.8 + 12.0 * ((t - b1) * 2.0 * std::f64::consts::PI * 3000.0).sin() as f32
                } else {
                    13.8
                }
            })
            .collect()
    }

    #[test]
    fn ignition_phases_and_verdicts() {
        let a = analyze(Kind::IgnitionPrimary, &ign_sig(1.5), DT);
        let tags: Vec<Tag> = a.phases.iter().map(|p| p.tag).collect();
        for t in [Tag::Active, Tag::Spike, Tag::Burn, Tag::Ringing] {
            assert!(tags.contains(&t), "{t:?} missing: {tags:?}");
        }
        let get = |k: &str| {
            a.readouts
                .iter()
                .find(|r| r.0 == k)
                .map(|r| r.1.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("dwell"), "3.00 ms");
        assert_eq!(get("firing peak"), "350 V");
        assert_eq!(get("burn voltage"), "40 V");
        assert!(a.verdict.unwrap().0);
        // MC/DC on the verdict: a short burn fails; no spark at all (dwell, no spike, no burn) fails
        assert!(
            !analyze(Kind::IgnitionPrimary, &ign_sig(0.3), DT)
                .verdict
                .unwrap()
                .0
        );
        let no_spark: Vec<f32> = ign_sig(1.5).iter().map(|&v| v.min(14.0)).collect();
        assert!(
            !analyze(Kind::IgnitionPrimary, &no_spark, DT)
                .verdict
                .unwrap()
                .0
        );
        // a spike that falls between samples (clipped here) but a visible burn line is a normal firing
        let spike_missed: Vec<f32> = ign_sig(1.5)
            .iter()
            .map(|&v| if v > 100.0 { 40.0 } else { v })
            .collect();
        let sm = analyze(Kind::IgnitionPrimary, &spike_missed, DT);
        assert!(
            sm.verdict.as_ref().unwrap().0,
            "{:?} {:?}",
            sm.verdict,
            sm.readouts
        );
    }

    #[test]
    fn pulses_lambda_battery_compression() {
        // 50 Hz PWM, 30 % low (active low)
        let pwm: Vec<f32> = (0..5000)
            .map(|i| {
                if ((i as f64 * DT) * 50.0).fract() < 0.3 {
                    0.2
                } else {
                    13.5
                }
            })
            .collect();
        let a = analyze(Kind::PulseLow, &pwm, DT);
        let get = |a: &Analysis, k: &str| {
            a.readouts
                .iter()
                .find(|r| r.0 == k)
                .map(|r| r.1.clone())
                .unwrap_or_default()
        };
        assert_eq!(get(&a, "frequency"), "50.00 Hz");
        assert_eq!(get(&a, "duty (active)"), "30.0 %");
        assert!(
            !analyze(Kind::PulseHigh, &vec![5.0; 500], DT)
                .verdict
                .unwrap()
                .0
        );
        // lambda switching 1 Hz between 0.1 and 0.85 V (sampled at 100 Hz for 4 s)
        let lam: Vec<f32> = (0..400)
            .map(|i| if (i / 50) % 2 == 0 { 0.1 } else { 0.85 })
            .collect();
        let l = analyze(Kind::Lambda, &lam, 0.01);
        assert_eq!(get(&l, "switching"), "1.00 Hz");
        assert!(l.verdict.unwrap().0);
        assert!(
            !analyze(Kind::Lambda, &vec![0.1; 400], 0.01)
                .verdict
                .unwrap()
                .0
        );
        // battery: 12.6 rest, dip to 9.2 while cranking, 14.1 running
        let bat: Vec<f32> = (0..1000)
            .map(|i| {
                if i < 300 {
                    12.6
                } else if i < 400 {
                    9.2
                } else {
                    14.1
                }
            })
            .collect();
        let b = analyze(Kind::Battery, &bat, 0.01);
        assert!(b.phases.iter().any(|p| p.tag == Tag::Dip));
        assert!(!b.verdict.unwrap().0, "9.2 V dip must fail");
        let ok: Vec<f32> = bat
            .iter()
            .map(|&v| if v < 10.0 { 10.4 } else { v })
            .collect();
        assert!(analyze(Kind::Battery, &ok, 0.01).verdict.unwrap().0);
        // MC/DC: cranked but not charging (12.4 V after the start) → fails; no crank at all → informational OK
        let no_charge: Vec<f32> = ok
            .iter()
            .map(|&v| if v > 14.0 { 12.4 } else { v })
            .collect();
        assert!(!analyze(Kind::Battery, &no_charge, 0.01).verdict.unwrap().0);
        assert!(analyze(Kind::Battery, &vec![12.6; 500], 0.01)
            .verdict
            .unwrap()
            .1
            .contains("no cranking"));
        // still cranking at the end of the view: not judged as 'not charging'
        let mid_crank: Vec<f32> = (0..500)
            .map(|i| if i < 300 { 12.6 } else { 10.4 })
            .collect();
        assert!(analyze(Kind::Battery, &mid_crank, 0.01)
            .verdict
            .unwrap()
            .1
            .contains("still cranking"));
        // compression: 8 humps, one 30 % lower
        let comp: Vec<f32> = (0..800)
            .map(|i| {
                let k = i / 100;
                let h = if k == 3 { 140.0 } else { 200.0 };
                80.0 + (h - 80.0) * ((i % 100) as f32 / 100.0 * std::f32::consts::PI).sin()
            })
            .collect();
        let c = analyze(Kind::Compression, &comp, 0.001);
        assert_eq!(get(&c, "humps"), "8");
        assert!(!c.verdict.unwrap().0);
        // garbage never panics
        for k in KINDS {
            let _ = analyze(k.0, &[f32::NAN; 100], DT);
            let _ = analyze(k.0, &[1.0; 3], DT);
            let _ = analyze(k.0, &vec![0.0; 1000], DT);
        }
    }

    #[test]
    fn models_are_analysed_as_healthy() {
        // the built-in typical waveforms pass their own analyzers (they are the 'good' reference)
        let h = Hint::default();
        for kind in [Kind::Injector, Kind::IgnitionPrimary] {
            let x: Vec<f32> = (0..4000)
                .map(|i| model(kind, (i as f64 - 500.0) * DT, &h).unwrap())
                .collect();
            let a = analyze(kind, &x, DT);
            assert!(
                a.verdict.as_ref().is_some_and(|v| v.0),
                "{kind:?}: {:?}",
                a.verdict
            );
        }
        // toothed wheels: crank 60-2 with its gap, wheel speed sensor
        let h = Hint {
            period: Some(20.0 * DT),
            lo: -2.0,
            hi: 2.0,
            gap_t: Some(0.0),
            wheel: (60, 2),
        };
        for kind in [Kind::CrankVr, Kind::WheelSpeed] {
            let x: Vec<f32> = (0..4000)
                .map(|i| model(kind, (i as f64 - 500.0) * DT, &h).unwrap())
                .collect();
            let a = analyze(kind, &x, DT);
            assert!(
                a.verdict.as_ref().is_none_or(|v| v.0),
                "{kind:?}: {:?}",
                a.verdict
            );
            if kind == Kind::CrankVr {
                assert!(
                    a.phases.iter().any(|p| p.tag == Tag::Gap),
                    "crank model has its gap"
                );
            }
        }
        assert_eq!(model(Kind::Lambda, 0.0, &h), None);
        assert!(!bands(Kind::Lambda).is_empty() && !bands(Kind::Battery).is_empty());
        assert!(model(Kind::PulseHigh, 0.0, &Hint::default()).is_none()); // pulses need a measured period
                                                                          // MC/DC lambda range: a ±2.5 V signal is not a narrowband lambda even though it crosses 0.45 V
        let sine: Vec<f32> = (0..4000).map(|i| 2.5 * (i as f32 * 0.003).sin()).collect();
        assert!(analyze(Kind::Lambda, &sine, DT)
            .verdict
            .unwrap()
            .1
            .contains("not a narrowband"));
    }

    #[test]
    fn wheel_speed_regular_damaged_wobble() {
        // VR-like: 48 teeth, 200 Hz, 50 samples per tooth
        let sig = |missing: bool, wobble: f32| -> Vec<f32> {
            (0..10_000)
                .map(|i| {
                    let tooth = i / 50;
                    let amp = 2.0
                        * (1.0
                            - wobble
                                * (0.5
                                    + 0.5 * (tooth as f32 / 48.0 * std::f32::consts::TAU).sin()));
                    if missing && tooth % 48 == 20 {
                        0.0
                    } else {
                        amp * ((i % 50) as f32 / 50.0 * std::f32::consts::TAU).sin()
                    }
                })
                .collect()
        };
        let ok = analyze(Kind::WheelSpeed, &sig(false, 0.0), DT);
        assert!(ok.verdict.as_ref().unwrap().0, "{:?}", ok.verdict);
        assert_eq!(
            ok.readouts
                .iter()
                .find(|r| r.0 == "frequency")
                .map(|r| r.1.as_str()),
            Some("2000.0 Hz")
        );
        // MC/DC: a missing tooth alone fails; wobble alone fails; flat fails
        let miss = analyze(Kind::WheelSpeed, &sig(true, 0.0), DT);
        assert!(!miss.verdict.unwrap().0 && miss.phases.iter().any(|p| p.tag == Tag::Warn));
        assert!(
            !analyze(Kind::WheelSpeed, &sig(false, 0.6), DT)
                .verdict
                .unwrap()
                .0
        );
        assert!(
            !analyze(Kind::WheelSpeed, &vec![1.0; 1000], DT)
                .verdict
                .unwrap()
                .0
        );
    }

    /// cost of one analysis on a full 4000-sample capture (owner rule: measure). `cargo test --release -- --ignored analyze_cost --nocapture`
    #[test]
    #[ignore]
    fn analyze_cost() {
        let x: Vec<f32> = ign_sig(1.5).iter().cycle().take(4000).copied().collect();
        for (k, _) in KINDS.iter().skip(1) {
            let t = std::time::Instant::now();
            for _ in 0..200 {
                std::hint::black_box(analyze(*k, std::hint::black_box(&x), DT));
            }
            println!("{k:?}: {:.1} µs", t.elapsed().as_secs_f64() / 200.0 * 1e6);
        }
    }

    /// the simulator's 1000 rpm crank as captured with 1, 2 and 8 channels: 2.5 samples per tooth must not read as damage
    #[test]
    fn crank_sampling_limit() {
        for (per, win) in [(4000usize, 0.1f64), (2000, 0.2), (500, 0.2), (2000, 0.1)] {
            let dt = win / per as f64;
            let x: Vec<f32> = (0..per)
                .map(|i| {
                    let t = 0.0123 + i as f64 * dt;
                    let tooth = (t * 1000.0f64).fract();
                    if (t * 1000.0 / 60.0).fract() < 2.0 / 60.0 {
                        0.0
                    } else {
                        (2.0 * (tooth * std::f64::consts::TAU).sin()) as f32
                    }
                })
                .collect();
            let a = analyze(Kind::CrankVr, &x, dt);
            let v = a.verdict.unwrap();
            if per == 500 {
                assert!(v.1.contains("samples per tooth"), "{v:?}");
            } else {
                assert!(v.0, "{per}: {v:?}");
            }
        }
    }

    #[test]
    fn waiting_for_a_signal() {
        // MC/DC: open input (noise below the floor) → waiting; a real VR sine → present; a swing above the noise but
        // below the type's minimum → waiting 'too small'; battery: DC level decides
        let noise: Vec<f32> = (0..1000)
            .map(|i| -0.033 + 0.025 * ((i * 7919 % 13) as f32 / 13.0 - 0.5))
            .collect();
        assert!(waiting_for(Kind::WheelSpeed, &noise, 0.2, "V")
            .unwrap()
            .contains("flat"));
        let vr: Vec<f32> = (0..1000).map(|i| 0.5 * (i as f32 * 0.1).sin()).collect();
        assert!(waiting_for(Kind::WheelSpeed, &vr, 0.2, "V").is_none());
        let small: Vec<f32> = vr.iter().map(|v| v * 0.12).collect(); // 0.12 V p-p, floor 0.05, minimum 0.15
        assert!(waiting_for(Kind::WheelSpeed, &small, 0.05, "V")
            .unwrap()
            .contains("too small"));
        assert!(waiting_for(Kind::Battery, &vec![0.0; 100], 0.2, "V").is_some());
        assert!(waiting_for(Kind::Battery, &vec![12.6; 100], 0.2, "V").is_none());
    }

    #[test]
    fn crank_wheel_from_settings() {
        // 36-1 wheel: 35 teeth then one missing
        let mut x = Vec::new();
        for _ in 0..3 {
            for t in 0..36 {
                for i in 0..20 {
                    x.push(if t < 35 && i < 10 { 1.0 } else { 0.0 });
                }
            }
        }
        assert!(analyze_with(Kind::CrankVr, &x, DT, (36, 1))
            .phases
            .iter()
            .any(|p| p.tag == Tag::Gap));
        // rpm: 36 positions × 20 samples × 10 µs = 7.2 ms per revolution = 8333 rpm
        let a = analyze_with(Kind::CrankVr, &x, DT, (36, 1));
        assert_eq!(
            a.readouts
                .iter()
                .find(|r| r.0 == "rpm")
                .map(|r| r.1.as_str()),
            Some("8333")
        );
    }

    #[test]
    fn crank_gap_and_irregular_tooth() {
        let mut x = Vec::new();
        for _ in 0..3 {
            for t in 0..60 {
                for i in 0..20 {
                    x.push(if t < 58 && i < 10 { 1.0 } else { 0.0 });
                }
            }
        }
        let a = analyze(Kind::CrankVr, &x, DT);
        assert!(a.phases.iter().any(|p| p.tag == Tag::Gap));
        assert!(a.verdict.as_ref().unwrap().0, "{:?}", a.verdict);
        // one damaged tooth (missing pulse inside the revolution) → irregular
        for i in 20 * 70..20 * 71 {
            x[i] = 0.0;
        }
        assert!(!analyze(Kind::CrankVr, &x, DT).verdict.unwrap().0);
    }
}
