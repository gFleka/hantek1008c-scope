//! dsp.rs: lowpass, rpm_series, crank_teeth, spectrum_db — guards (MC/DC) and behaviour on clean, noisy,
//! partial and broken signals.

use super::util::Lcg;
use crate::dsp::*;

// lowpass guard: `x.is_empty() || fc <= 0` → input unchanged.
#[test]
fn lowpass_guard_and_response() {
    let x: Vec<f32> = (0..100).map(|i| if i < 50 { 0.0 } else { 1.0 }).collect();
    #[rustfmt::skip]
    let passthrough: &[(&str, &[f32], f64)] = &[
        ("empty",     &[],  10.0),
        ("fc 0",      &x,   0.0),
        ("fc < 0",    &x,  -5.0),
    ];
    for &(name, d, fc) in passthrough {
        assert_eq!(lowpass(d, 1e-3, fc), d.to_vec(), "{name}");
    }
    // constant stays constant (starts at x[0], no start-up transient)
    assert!(lowpass(&[3.0; 50], 1e-3, 10.0)
        .iter()
        .all(|&v| (v - 3.0).abs() < 1e-6));
    // monotonic step response between 0 and 1; higher cutoff follows faster
    let slow = lowpass(&x, 1e-3, 5.0);
    let fast = lowpass(&x, 1e-3, 50.0);
    assert!(slow.windows(2).all(|w| w[1] >= w[0] - 1e-7));
    assert!(slow.iter().chain(&fast).all(|&v| (0.0..=1.0).contains(&v)));
    assert!(fast[60] > slow[60]);
    // a huge cutoff passes the signal nearly unchanged
    let raw = lowpass(&x, 1e-3, 1e9);
    assert!((raw[50] - 1.0).abs() < 1e-3);
}

// rpm_series guards: `len < 3 || ppr <= 0` · `span <= 0` · edges ≥ 2 for any value.
#[test]
fn rpm_series_mcdc() {
    let pulses = |period: usize, n: usize| -> Vec<f32> {
        (0..n)
            .map(|i| if i % period < period / 2 { 0.0 } else { 5.0 })
            .collect()
    };
    let p = pulses(10, 100);
    #[rustfmt::skip]
    let all_nan: &[(&str, Vec<f32>, f64)] = &[
        ("len 2",         vec![0.0, 5.0], 1.0),
        ("ppr 0",         p.clone(), 0.0),
        ("ppr < 0",       p.clone(), -1.0),
        ("flat",          vec![1.0; 100], 1.0),
        ("one edge",      [vec![0.0; 50], vec![5.0; 50]].concat(), 1.0),
    ];
    for (name, x, ppr) in all_nan {
        let r = rpm_series(x, 1e-3, *ppr);
        assert_eq!(r.len(), x.len(), "{name}");
        assert!(r.iter().all(|v| v.is_nan()), "{name}");
    }
    // 10 ms period, 1 pulse/rev → 6000 rpm; 2 pulses/rev → 3000 rpm; NaN before the first / after the last edge
    for (ppr, rpm) in [(1.0, 6000.0f32), (2.0, 3000.0)] {
        let r = rpm_series(&p, 1e-3, ppr);
        let first = r.iter().position(|v| !v.is_nan()).unwrap();
        let last = r.iter().rposition(|v| !v.is_nan()).unwrap();
        assert_eq!(first, 5, "first rising edge");
        assert!(last < 95);
        assert!(
            r[first..=last].iter().all(|&v| (v - rpm).abs() < 1e-2),
            "ppr {ppr}"
        );
    }
    // noisy pulses (noise < hysteresis) → same rpm
    let mut g = Lcg(5);
    let noisy: Vec<f32> = p.iter().map(|v| v + 0.3 * g.next()).collect();
    let r = rpm_series(&noisy, 1e-3, 1.0);
    assert!(r
        .iter()
        .filter(|v| !v.is_nan())
        .all(|&v| (v - 6000.0).abs() < 1.0));
}

/// Missing-tooth wheel signal: `total` positions, the last `missing` absent, `spt` samples per tooth,
/// starting `start` positions into the wheel, `n` positions long, optional noise.
fn wheel(total: u32, missing: u32, spt: usize, start: u32, n: u32, noise: f32) -> Vec<f32> {
    let mut g = Lcg(9);
    let mut x = Vec::new();
    for p in start..start + n {
        let pos = p % total;
        let tooth = pos < total - missing;
        for s in 0..spt {
            let v = if tooth && s >= spt / 2 { 1.0 } else { 0.0 };
            x.push(v + noise * g.next());
        }
    }
    x
}

/// Check a crank_teeth result: tooth numbers count up by one per edge (mod present), and every tooth 0
/// follows a long interval (the gap).
fn check_numbering(t: &[(usize, u32)], present: u32, spt: usize) {
    for w in t.windows(2) {
        let (a, b) = (w[0], w[1]);
        assert_eq!(b.1, (a.1 + 1) % present, "numbering at sample {}", b.0);
        let gap = b.0 - a.0;
        if b.1 == 0 {
            assert!(gap * 2 > spt * 3, "tooth 0 after the gap");
        } else {
            assert!(gap.abs_diff(spt) <= 1, "regular tooth spacing");
        }
    }
}

// crank_teeth: guards `len < 8 || teeth < 4` · `span <= 0` · gap `d > 1.5 × avg(prev 3)` (needs k ≥ 4).
#[test]
fn crank_teeth_cases() {
    // guards → empty
    #[rustfmt::skip]
    let empty: &[(&str, Vec<f32>, u32)] = &[
        ("len 7",            vec![0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0], 58),
        ("teeth 3",          wheel(60, 2, 10, 0, 120, 0.0), 3),
        ("flat",             vec![0.5; 1000], 58),
        ("no gap (60-0)",    wheel(60, 0, 10, 0, 180, 0.0), 60),
        ("before the gap only", wheel(60, 2, 10, 0, 57, 0.0), 58),
    ];
    for (name, x, teeth) in empty {
        assert!(crank_teeth(x, *teeth).is_empty(), "{name}");
    }
    #[rustfmt::skip]
    let cases: &[(&str, u32, u32, u32, u32, f32, usize)] = &[
        // name                          total missing start positions noise  revolutions (tooth 0 count)
        ("60-2, 2 revolutions",           60, 2,  0,  125, 0.0,  2),
        ("60-2, starts mid-wheel",        60, 2,  25, 125, 0.0,  2),
        ("60-2, noisy (±5 %)",            60, 2,  10, 125, 0.05, 2),
        ("36-1",                          36, 1,  5,  80,  0.0,  2),
        ("60-2, partial: one gap",        60, 2,  40, 60,  0.0,  1),
    ];
    for &(name, total, missing, start, n, noise, revs) in cases {
        let x = wheel(total, missing, 10, start, n, noise);
        let present = total - missing;
        let t = crank_teeth(&x, present);
        assert!(!t.is_empty(), "{name}");
        assert_eq!(
            t[0].1, 0,
            "{name}: output starts at tooth 0 (after the first gap)"
        );
        check_numbering(&t, present, 10);
        assert_eq!(t.iter().filter(|x| x.1 == 0).count(), revs, "{name}");
    }
}

/// A gap within the first 3 tooth intervals of the capture can't be told from a slow tooth: it is skipped,
/// numbering starts at the next gap (documented limit, not a bug).
#[test]
fn crank_teeth_gap_right_at_the_start_is_skipped() {
    let x = wheel(60, 2, 10, 56, 55, 0.0); // the gap is the 2nd interval
    let t = crank_teeth(&x, 58);
    assert!(t.is_empty(), "{t:?}");
}

// spectrum_db: guard `len < 8` · largest power-of-two tail · peak bin · DC removed.
#[test]
fn spectrum_cases() {
    assert_eq!(spectrum_db(&[0.0; 7], 1e-3), (0.0, Vec::new()));
    #[rustfmt::skip]
    let cases: &[(usize, usize)] = &[(8, 8), (9, 8), (1000, 512), (1024, 1024), (4000, 2048)];
    for &(len, n) in cases {
        let (df, m) = spectrum_db(&vec![0.0; len], 1e-5);
        assert_eq!(m.len(), n / 2 + 1, "len {len}");
        assert!((df - 1.0 / (n as f64 * 1e-5)).abs() < 1e-9, "len {len}");
        assert!(m.iter().all(|&v| v <= -200.0), "silence");
    }
    // 1 kHz sine, amplitude 1, sampled at 100 kHz, 1024 points + DC offset 3: peak near bin 10.24, ~0 dB
    let x: Vec<f32> = (0..1024)
        .map(|i| 3.0 + (2.0 * std::f64::consts::PI * 1000.0 * i as f64 * 1e-5).sin() as f32)
        .collect();
    let (df, m) = spectrum_db(&x, 1e-5);
    let (k, peak) = m
        .iter()
        .enumerate()
        .fold((0, f32::MIN), |b, (i, &v)| if v > b.1 { (i, v) } else { b });
    assert!(
        (10..=11).contains(&k),
        "peak bin {k} ({} Hz)",
        k as f64 * df
    );
    assert!((-2.0..=0.5).contains(&peak), "peak {peak} dB");
    // DC removed: the spectrum doesn't change when a DC offset is added
    let no_dc: Vec<f32> = x.iter().map(|v| v - 3.0).collect();
    let (_, m0) = spectrum_db(&no_dc, 1e-5);
    assert!(
        (m[0] - m0[0]).abs() < 0.5 && (m[k] - m0[k]).abs() < 0.01,
        "DC {} vs {}",
        m[0],
        m0[0]
    );
    // NaN inside the data does not panic
    let mut bad = x.clone();
    bad[100] = f32::NAN;
    let _ = spectrum_db(&bad, 1e-5);
}

/// Speed review (2026-11-23): per-call cost of the per-frame signal paths at capture size (4000 / 500 samples) and
/// stream sizes (88 k = 8 ch × 200 s at 440 S/s; 401 k = 1 ch × 200 s at 2006 S/s on the real scope), plus the FFT
/// with a twiddle table (proposed fix, local copy here — the app's `fft` is unchanged) and a Snapshot copy.
/// Run: cargo test --release speed_micro -- --ignored --nocapture
#[test]
#[ignore]
fn speed_micro() {
    use std::hint::black_box;
    use std::time::Instant;
    fn time<T>(reps: usize, mut f: impl FnMut() -> T) -> f64 {
        let t = Instant::now();
        for _ in 0..reps {
            black_box(f());
        }
        t.elapsed().as_secs_f64() * 1e3 / reps as f64
    }
    let mut rng = Lcg(7);
    let raw = |n: usize, rng: &mut Lcg| -> Vec<u16> {
        (0..n)
            .map(|i| {
                (2048.0 + if (i / 37) % 2 == 0 { 600.0 } else { 0.0 } + 6.0 * rng.next()) as u16
            })
            .collect()
    };
    for &n in &[500usize, 4000, 88_000, 401_000] {
        let d = raw(n, &mut rng);
        let reps = (2_000_000 / n).clamp(3, 2000);
        let m = time(reps, || {
            crate::hantek::measure(&d, 2048.0, 0.0125, Some(1e-5))
        });
        let units = time(reps, || {
            d.iter()
                .map(|&c| (c as f32 - 2048.0) * 0.0125)
                .collect::<Vec<f32>>()
        });
        let x: Vec<f32> = d.iter().map(|&c| (c as f32 - 2048.0) * 0.0125).collect();
        let fft = time(reps.min(200), || spectrum_db(&x, 1e-5));
        let fft2 = time(reps.min(200), || spectrum_db_table(&x, 1e-5));
        let minmax = time(reps, || {
            d.iter()
                .fold((u16::MAX, 0u16), |(a, b), &s| (a.min(s), b.max(s)))
        });
        let ana = time(reps.min(200), || {
            crate::analyze::analyze(crate::analyze::Kind::PulseHigh, &x, 1e-5)
        });
        let can = time(reps.min(200), || can_decode(&x, 1e-5, 10_000.0, true));
        let uart = time(reps.min(200), || uart_decode(&x, 1e-5, 9600.0));
        println!(
            "n={n:>7}: measure {m:8.3} ms ({:.1} ns/sample) | units_of {units:7.3} | min/max {minmax:7.3} | FFT now {fft:8.3} ms, with twiddle table {fft2:8.3} ms ({:.1}×) | analyze(pulse) {ana:7.3} | can_decode {can:7.3} | uart_decode {uart:7.3} ms",
            m * 1e6 / n as f64,
            fft / fft2
        );
        let (a, b) = (spectrum_db(&x, 1e-5).1, spectrum_db_table(&x, 1e-5).1);
        let err = a
            .iter()
            .zip(&b)
            .map(|(p, q)| (p - q).abs())
            .fold(0.0f32, f32::max);
        assert!(err < 0.05, "table FFT differs by {err} dB");
    }
    // Snapshot copy (UI: `output_buffer().clone()` every frame; stream window cache clone)
    for &(ch, n) in &[(8usize, 500usize), (8, 88_000), (8, 220_000), (1, 401_000)] {
        let mut s = crate::acq::Snapshot::default();
        for c in 0..ch {
            s.frame.active[c] = true;
            s.frame.data[c] = raw(n, &mut rng);
        }
        let clone = time(50, || s.clone());
        let mut dst = crate::acq::Snapshot::default();
        let clone_from = time(50, || dst.clone_from(&s));
        println!(
            "Snapshot {ch} ch × {n:>7}: clone {clone:7.3} ms (fresh allocation), clone_from into a warm buffer {clone_from:7.3} ms"
        );
    }
}

/// `spectrum_db` with the twiddle factors computed once per call (n/2 sin/cos instead of n/2·log2 n).
fn spectrum_db_table(x: &[f32], dt: f64) -> (f64, Vec<f32>) {
    if x.len() < 8 {
        return (0.0, Vec::new());
    }
    let n = 1usize << (usize::BITS - 1 - x.len().leading_zeros());
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
    let tw: Vec<(f64, f64)> = (0..n / 2)
        .map(|k| {
            let a = -2.0 * std::f64::consts::PI * k as f64 / n as f64;
            (a.cos(), a.sin())
        })
        .collect();
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
    let mags = (0..=n / 2).map(|k| {
        let a = (re[k] * re[k] + im[k] * im[k]).sqrt() * 4.0 / n as f64;
        (20.0 * a.max(1e-12).log10()) as f32
    });
    (1.0 / (n as f64 * dt), mags.collect())
}
