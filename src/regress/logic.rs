//! MC/DC tables for the pure decision functions in hantek.rs: trigger_offset, sw_trigger / sw_matches,
//! measure, first_edge, timebase / payload helpers. Counts = volts here (zero 0, 1 V per count, dt 1 s),
//! so every threshold in a row reads directly as samples / counts.

use super::util::{seg, Lcg};
use crate::hantek::*;

// ───────────────────────────── trigger_offset ─────────────────────────────
// Decisions: guard `len < 2 || !expect.finite` · hit `a < lv && b >= lv` (rising) / `a > lv && b <= lv`
// (falling) · inside window `i0..=i1` · nearest `|off| < |best|` (strict: first wins a tie).

#[test]
fn trigger_offset_mcdc() {
    let ramp = vec![0u16, 0, 100, 100];
    let fall = vec![100u16, 100, 0, 0];
    let two = vec![0u16, 100, 100, 0, 0, 100]; // rising crossings at x = 0.5 and 4.5
    #[rustfmt::skip]
    let cases: &[(&str, &[u16], f64, u16, bool, usize, Option<f32>)] = &[
        // name                              data   expect  level rising search  result
        ("guard: len 1",                     &[5],   0.0,    50, true,  12, None),
        ("guard: len 0",                     &[],    0.0,    50, true,  12, None),
        ("guard: expect NaN",                &ramp,  f64::NAN, 50, true, 12, None),
        ("guard: expect inf",                &ramp,  f64::INFINITY, 50, true, 12, None),
        ("rising: a<lv, b>=lv → hit",        &ramp,  1.5,    50, true,  12, Some(0.0)),
        ("rising: slope flipped → no hit",   &ramp,  1.5,    50, false, 12, None),
        ("rising: b == lv counts",           &[0, 50], 1.0,  50, true,  12, Some(0.0)),
        ("rising: a == lv does not",         &[50, 100], 1.0, 50, true, 12, None),
        ("rising: never reaches lv",         &[0, 40, 40], 1.0, 50, true, 12, None),
        ("falling: a>lv, b<=lv → hit",       &fall,  1.5,    50, false, 12, Some(0.0)),
        ("falling: slope flipped → no hit",  &fall,  1.5,    50, true,  12, None),
        ("falling: b == lv counts",          &[100, 50], 1.0, 50, false, 12, Some(0.0)),
        ("nearest of two (right)",           &two,   3.0,    50, true,  12, Some(1.5)),
        ("nearest of two (left)",            &two,   2.0,    50, true,  12, Some(-1.5)),
        ("tie → first (left) wins",          &two,   2.5,    50, true,  12, Some(-2.0)),
        ("both outside ±1 window",           &two,   3.0,    50, true,  1,  None),
        ("search 0: only sample c",          &two,   1.0,    50, true,  0,  Some(-0.5)),
        ("expect past the end → clamped",    &two,   100.0,  50, true,  200, Some(-95.5)),
        ("expect negative → clamped",        &two,   -5.0,   50, true,  12, Some(5.5)),
        ("interpolation 1/4",                &[0, 200], 1.0, 50, true,  12, Some(-0.75)),
    ];
    for &(name, d, expect, level, rising, search, want) in cases {
        let got = trigger_offset(d, expect, level, rising, search);
        assert_eq!(got, want, "{name}");
    }
}

#[test]
fn trigger_offset_is_bounded_by_the_window_on_random_data() {
    // property: any result is a real crossing within ±(search+1) samples of the clamped nominal point
    let mut r = Lcg(7);
    for k in 0..500 {
        let n = 2 + (k % 50);
        let d: Vec<u16> = (0..n)
            .map(|_| (2048.0 + 2000.0 * r.next()) as u16)
            .collect();
        let expect = (r.next() as f64 + 1.0) * n as f64 / 2.0;
        let search = k % 13;
        if let Some(off) = trigger_offset(&d, expect, 2048, k % 2 == 0, search) {
            assert!(off.is_finite());
            let c = expect.round().clamp(1.0, (n - 1) as f64);
            assert!(
                (expect as f32 + off - c as f32).abs() <= search as f32 + 1.0,
                "case {k}"
            );
        }
    }
}

// ───────────────────────────── software triggers ─────────────────────────────

/// Pulse: `a` low (0), `w` high (10), `b` low.
fn pulse(a: usize, w: usize, b: usize) -> Vec<u16> {
    seg(&[(a, 0), (w, 10), (b, 0)])
}

fn width(high: bool, cmp: Cmp, t1: f64, t2: f64) -> SwTrig {
    SwTrig::Width { high, cmp, t1, t2 }
}

fn sw(t: &SwTrig, d: &[u16], trig_idx: f64, level: f32) -> Option<(usize, usize)> {
    sw_trigger(t, d, 0.0, 1.0, 1.0, trig_idx, level).map(|h| (h.start, h.at))
}

// Width decisions: entering `c == high` · Longer early match `i - s >= n(t1)` · at the pulse end
// Longer `w > t1` / Shorter `w < t1` / Between `t_lo <= w <= t_hi`.
#[test]
fn sw_width_mcdc() {
    use Cmp::*;
    let low_pulse = seg(&[(5, 10), (5, 0), (5, 10)]);
    let unfinished = seg(&[(5, 0), (5, 10)]);
    let at_start = seg(&[(3, 10), (5, 0)]);
    #[rustfmt::skip]
    let cases: Vec<(&str, SwTrig, Vec<u16>, Option<(usize, usize)>)> = vec![
        ("longer 3: matches after 3 samples",   width(true, Longer, 3.0, 0.0),  pulse(5, 5, 5), Some((5, 8))),
        ("longer 6: 5-sample pulse too short",  width(true, Longer, 6.0, 0.0),  pulse(5, 5, 5), None),
        ("longer 4.5: 5 > 4.5",                 width(true, Longer, 4.5, 0.0),  pulse(5, 5, 5), Some((5, 10))),
        ("longer: unfinished pulse still hits", width(true, Longer, 3.0, 0.0),  unfinished.clone(), Some((5, 8))),
        ("shorter 6: 5 < 6",                    width(true, Shorter, 6.0, 0.0), pulse(5, 5, 5), Some((5, 10))),
        ("shorter 5: 5 < 5 false",              width(true, Shorter, 5.0, 0.0), pulse(5, 5, 5), None),
        ("shorter: unfinished pulse → none",    width(true, Shorter, 6.0, 0.0), unfinished.clone(), None),
        ("shorter: pulse running at start",     width(true, Shorter, 9.0, 0.0), at_start.clone(), None),
        ("between 4…6",                         width(true, Between, 4.0, 6.0), pulse(5, 5, 5), Some((5, 10))),
        ("between swapped 6…4",                 width(true, Between, 6.0, 4.0), pulse(5, 5, 5), Some((5, 10))),
        ("between 5…5 inclusive",               width(true, Between, 5.0, 5.0), pulse(5, 5, 5), Some((5, 10))),
        ("between 6…8 → none",                  width(true, Between, 6.0, 8.0), pulse(5, 5, 5), None),
        ("between 2…4 → none",                  width(true, Between, 2.0, 4.0), pulse(5, 5, 5), None),
        ("low pulse: shorter 6",                width(false, Shorter, 6.0, 0.0), low_pulse.clone(), Some((5, 10))),
        ("low trigger on a high pulse → none",  width(false, Shorter, 6.0, 0.0), pulse(5, 5, 5), None),
        ("high trigger on a low pulse → none",  width(true, Shorter, 6.0, 0.0), low_pulse, None),
        ("second pulse matches, first not",     width(true, Shorter, 3.0, 0.0), seg(&[(2, 0), (4, 10), (2, 0), (2, 10), (2, 0)]), Some((8, 10))),
    ];
    for (name, t, d, want) in cases {
        assert_eq!(sw(&t, &d, 0.0, 5.0), want, "{name}");
    }
}

/// "Longer than T" must not match a pulse of exactly T: the end-of-pulse branch uses `w > t1`, but the early
/// match fires at `i - s >= samples(t1)`, i.e. already when the pulse has lasted exactly t1 (checked before
/// the falling edge at that sample is seen).
#[test]
fn sw_width_longer_is_strict() {
    let t = width(true, Cmp::Longer, 5.0, 0.0);
    assert_eq!(sw(&t, &pulse(5, 5, 5), 0.0, 5.0), None);
}

// Runt decisions (high; low mirrored): enter `v > lo` · full `v >= hi` · back `v < lo` with `!reached`.
#[test]
fn sw_runt_mcdc() {
    let runt = |high, lo, hi| SwTrig::Runt { high, lo, hi };
    #[rustfmt::skip]
    let cases: Vec<(&str, SwTrig, Vec<u16>, Option<(usize, usize)>)> = vec![
        ("high runt: crosses lo, not hi",       runt(true, 3.0, 8.0), seg(&[(3, 0), (3, 5), (3, 0)]), Some((3, 6))),
        ("full pulse reaches hi → none",        runt(true, 3.0, 8.0), seg(&[(3, 0), (3, 10), (3, 0)]), None),
        ("exactly hi counts as full",           runt(true, 3.0, 8.0), seg(&[(3, 0), (3, 8), (3, 0)]), None),
        ("never above lo → none",               runt(true, 3.0, 8.0), seg(&[(3, 0), (3, 2), (3, 0)]), None),
        ("exactly lo is not above",             runt(true, 3.0, 8.0), seg(&[(3, 0), (3, 3), (3, 0)]), None),
        ("runt not back yet → none",            runt(true, 3.0, 8.0), seg(&[(3, 0), (3, 5)]), None),
        ("full, then runt → the runt",          runt(true, 3.0, 8.0), seg(&[(2, 0), (2, 10), (2, 0), (2, 5), (2, 0)]), Some((6, 8))),
        ("lo/hi swapped",                       runt(true, 8.0, 3.0), seg(&[(3, 0), (3, 5), (3, 0)]), Some((3, 6))),
        ("starts inside the band",              runt(true, 3.0, 8.0), seg(&[(3, 5), (3, 0)]), Some((0, 3))),
        ("low runt: dips below hi, not lo",     runt(false, 3.0, 8.0), seg(&[(3, 10), (3, 5), (3, 10)]), Some((3, 6))),
        ("low full dip → none",                 runt(false, 3.0, 8.0), seg(&[(3, 10), (3, 0), (3, 10)]), None),
        ("low trigger on a high runt → none",   runt(false, 3.0, 8.0), seg(&[(3, 0), (3, 5), (3, 0)]), None),
    ];
    for (name, t, d, want) in cases {
        assert_eq!(sw(&t, &d, 0.0, 5.0), want, "{name}");
    }
}

// Dropout decisions: between crossings `i - last > n` · at the end `len-1-last > n`.
#[test]
fn sw_dropout_mcdc() {
    let t = SwTrig::Dropout { t: 4.0 };
    let square: Vec<u16> = (0..20).map(|i| if i % 4 < 2 { 0 } else { 10 }).collect();
    #[rustfmt::skip]
    let cases: Vec<(&str, Vec<u16>, Option<(usize, usize)>)> = vec![
        ("regular edges every 2 → none",       square, None),
        ("gap of 8 between edges",             seg(&[(2, 0), (2, 10), (8, 0), (2, 10), (2, 0)]), Some((4, 8))),
        ("gap exactly n → none (strict)",      seg(&[(2, 0), (2, 10), (4, 0), (2, 10)]), None),
        ("gap n+1 → hit",                      seg(&[(2, 0), (2, 10), (5, 0), (2, 10)]), Some((4, 8))),
        ("flat capture longer than t",         vec![0; 10], Some((0, 4))),
        ("flat capture of n+1 samples → none", vec![0; 5], None),
        ("trailing silence",                   seg(&[(2, 0), (2, 10), (10, 0)]), Some((4, 8))),
        ("leading silence counts from 0",      seg(&[(6, 0), (2, 10), (2, 0)]), Some((0, 4))),
    ];
    for (name, d, want) in cases {
        assert_eq!(sw(&t, &d, 0.0, 5.0), want, "{name}");
    }
}

// Window decision: `v < lo || v > hi` (each condition alone).
#[test]
fn sw_window_mcdc() {
    let w = |lo, hi| SwTrig::Window { lo, hi };
    #[rustfmt::skip]
    let cases: Vec<(&str, SwTrig, Vec<u16>, Option<(usize, usize)>)> = vec![
        ("inside → none",             w(3.0, 7.0), vec![5; 10], None),
        ("below lo",                  w(3.0, 7.0), seg(&[(4, 5), (1, 2), (5, 5)]), Some((4, 4))),
        ("above hi",                  w(3.0, 7.0), seg(&[(4, 5), (1, 8), (5, 5)]), Some((4, 4))),
        ("on the bounds → none",      w(3.0, 7.0), seg(&[(2, 3), (2, 7)]), None),
        ("lo/hi swapped",             w(7.0, 3.0), seg(&[(4, 5), (1, 8), (5, 5)]), Some((4, 4))),
        ("first sample outside",      w(3.0, 7.0), seg(&[(1, 0), (5, 5)]), Some((0, 0))),
    ];
    for (name, t, d, want) in cases {
        assert_eq!(sw(&t, &d, 0.0, 5.0), want, "{name}");
    }
}

// Zone decisions: zone inside the capture `i0 < i1` · (enter, trace inside) → 4 combinations.
#[test]
fn sw_zone_mcdc() {
    let z = |t0, t1, v0, v1, enter| SwTrig::Zone {
        t0,
        t1,
        v0,
        v1,
        enter,
    };
    let mut touch = vec![0u16; 20];
    touch[9] = 5;
    #[rustfmt::skip]
    let cases: Vec<(&str, SwTrig, Vec<u16>, f64, Option<(usize, usize)>)> = vec![
        ("enter + inside → hit",            z(-2.0, 2.0, 4.0, 6.0, true),  touch.clone(), 10.0, Some((9, 9))),
        ("enter + never inside → none",     z(-2.0, 2.0, 4.0, 6.0, true),  vec![0; 20],   10.0, None),
        ("avoid + never inside → hit",      z(-2.0, 2.0, 4.0, 6.0, false), vec![0; 20],   10.0, Some((8, 11))),
        ("avoid + inside → none",           z(-2.0, 2.0, 4.0, 6.0, false), touch.clone(), 10.0, None),
        ("zone swapped t and v",            z(2.0, -2.0, 6.0, 4.0, true),  touch.clone(), 10.0, Some((9, 9))),
        ("zone after the capture → none",   z(-2.0, 2.0, 4.0, 6.0, false), vec![0; 20],   100.0, None),
        ("zone before the capture → none",  z(-5.0, -1.0, 4.0, 6.0, false), vec![0; 20],  0.0, None),
        ("zone cut at the end",             z(-2.0, 2.0, 4.0, 6.0, false), vec![0; 20],   19.0, Some((17, 19))),
        ("inside outside the zone ignored", z(-2.0, 2.0, 4.0, 6.0, true),  touch, 15.0, None),
    ];
    for (name, t, d, trig, want) in cases {
        assert_eq!(sw(&t, &d, trig, 5.0), want, "{name}");
    }
}

// Common guard: `len < 2 || dt <= 0` and Off, for every kind.
#[test]
fn sw_guards_for_every_kind() {
    let kinds = [
        SwTrig::Off,
        width(true, Cmp::Longer, 1.0, 0.0),
        SwTrig::Runt {
            high: true,
            lo: 1.0,
            hi: 2.0,
        },
        SwTrig::Dropout { t: 1.0 },
        SwTrig::Window { lo: 1.0, hi: 2.0 },
        SwTrig::Zone {
            t0: -1.0,
            t1: 1.0,
            v0: -100.0,
            v1: 100.0,
            enter: true,
        },
    ];
    let busy = pulse(3, 3, 3);
    for t in kinds {
        assert_eq!(
            sw_trigger(&t, &[7], 0.0, 1.0, 1.0, 0.0, 5.0),
            None,
            "{t:?} len 1"
        );
        assert_eq!(
            sw_trigger(&t, &busy, 0.0, 1.0, 0.0, 4.0, 5.0),
            None,
            "{t:?} dt 0"
        );
        assert_eq!(
            sw_trigger(&t, &busy, 0.0, 1.0, -1.0, 4.0, 5.0),
            None,
            "{t:?} dt < 0"
        );
        // garbage thresholds (from a corrupt settings file) never panic
        for bad in [f64::NAN, f64::INFINITY, -1.0, 0.0, 1e300] {
            let g = match t {
                SwTrig::Width { high, cmp, .. } => width(high, cmp, bad, bad),
                SwTrig::Dropout { .. } => SwTrig::Dropout { t: bad },
                SwTrig::Zone { v0, v1, enter, .. } => SwTrig::Zone {
                    t0: bad,
                    t1: -bad,
                    v0,
                    v1,
                    enter,
                },
                o => o,
            };
            let _ = sw_trigger(&g, &busy, 0.0, 1.0, 1.0, 4.0, 5.0);
            let _ = sw_matches(&g, &busy, 0.0, 1.0, 1.0, 4.0, 5.0, 100);
        }
    }
    assert_eq!(sw(&SwTrig::Off, &busy, 4.0, 5.0), None);
}

#[test]
fn sw_hit_volts_conversion() {
    // zero 2048 counts, 10 mV per count: a 1 V level = 2148 counts
    let d = seg(&[(5, 2048), (5, 2248), (5, 2048)]);
    let t = width(true, Cmp::Shorter, 6e-3, 0.0);
    let h = sw_trigger(&t, &d, 2048.0, 0.01, 1e-3, 0.0, 1.0).unwrap();
    assert_eq!((h.start, h.at), (5, 10));
    // level above the pulse: nothing
    assert_eq!(sw_trigger(&t, &d, 2048.0, 0.01, 1e-3, 0.0, 2.5), None);
}

// sw_matches: every event once, `max` respected, long events not repeated.
#[test]
fn sw_matches_counts() {
    let three = seg(&[(2, 0), (3, 10), (2, 0), (3, 10), (2, 0), (3, 10), (2, 0)]);
    let two_exc = seg(&[(3, 5), (3, 9), (3, 5), (2, 0), (3, 5)]);
    let runts = seg(&[(2, 0), (2, 5), (2, 0), (2, 10), (2, 0), (2, 5), (2, 0)]);
    #[rustfmt::skip]
    let cases: Vec<(&str, SwTrig, Vec<u16>, usize, Vec<usize>)> = vec![
        ("3 short pulses",               width(true, Cmp::Shorter, 4.0, 0.0), three.clone(), 100, vec![2, 7, 12]),
        ("max 2",                        width(true, Cmp::Shorter, 4.0, 0.0), three.clone(), 2, vec![2, 7]),
        ("max 0",                        width(true, Cmp::Shorter, 4.0, 0.0), three, 0, vec![]),
        ("window: 2 excursions, once each", SwTrig::Window { lo: 3.0, hi: 7.0 }, two_exc, 100, vec![3, 9]),
        ("dropout: one long silence once", SwTrig::Dropout { t: 4.0 }, seg(&[(2, 0), (2, 10), (30, 0)]), 100, vec![4]),
        ("zone: at most one",            SwTrig::Zone { t0: -100.0, t1: 100.0, v0: -1.0, v1: 20.0, enter: true }, vec![5; 30], 100, vec![0]),
        ("2 runts, full pulse skipped",  SwTrig::Runt { high: true, lo: 3.0, hi: 8.0 }, runts, 100, vec![2, 10]),
    ];
    for (name, t, d, max, starts) in cases {
        let hits = sw_matches(&t, &d, 0.0, 1.0, 1.0, 15.0, 5.0, max);
        let got: Vec<usize> = hits.iter().map(|h| h.start).collect();
        assert_eq!(got, starts, "{name}");
        assert!(
            hits.iter()
                .all(|h| h.at >= h.start && h.at < d.len().max(1) + 8),
            "{name}"
        );
        assert!(
            hits.windows(2).all(|w| w[1].start > w[0].start),
            "{name}: in order"
        );
    }
}

// ───────────────────────────── measure ─────────────────────────────
// Decisions: empty · `dt` given · `span >= 8` · rises >= 2 (freq) · rise→fall pairs (widths) · both widths
// (duty) · 10/90 % edges inside the capture (`a > 1 && b < len-1`).

fn square(lo: u16, hi: u16, low: usize, high: usize, periods: usize) -> Vec<u16> {
    (0..periods)
        .flat_map(|_| seg(&[(low, lo), (high, hi)]))
        .collect()
}

#[test]
fn measure_mcdc() {
    struct Row {
        name: &'static str,
        d: Vec<u16>,
        dt: Option<f64>,
        freq: Option<f64>,
        duty: Option<f64>,
        width_pos: Option<f64>,
    }
    let mut noisy = square(1000, 3000, 10, 10, 6);
    let mut r = Lcg(3);
    for v in noisy.iter_mut() {
        *v = (*v as f32 + 150.0 * r.next()) as u16;
    }
    #[rustfmt::skip]
    let rows = vec![
        Row { name: "flat → no timing",          d: vec![2048; 100], dt: Some(1e-3), freq: None, duty: None, width_pos: None },
        Row { name: "span 7 → too small",        d: square(1000, 1007, 10, 10, 5), dt: Some(1e-3), freq: None, duty: None, width_pos: None },
        Row { name: "span 8 → measured",         d: square(1000, 1008, 10, 10, 5), dt: Some(1e-3), freq: Some(50.0), duty: Some(0.5), width_pos: Some(10e-3) },
        Row { name: "no dt → no timing",         d: square(1000, 3000, 10, 10, 5), dt: None, freq: None, duty: None, width_pos: None },
        Row { name: "single rising edge",        d: seg(&[(50, 0), (50, 1000)]), dt: Some(1e-3), freq: None, duty: None, width_pos: None },
        Row { name: "one pulse: width, no freq", d: seg(&[(20, 0), (10, 1000), (20, 0)]), dt: Some(1e-3), freq: None, duty: None, width_pos: Some(10e-3) },
        Row { name: "square 50 Hz 50 %",         d: square(1000, 3000, 10, 10, 5), dt: Some(1e-3), freq: Some(50.0), duty: Some(0.5), width_pos: Some(10e-3) },
        Row { name: "PWM 25 %",                  d: square(1000, 3000, 15, 5, 5), dt: Some(1e-3), freq: Some(50.0), duty: Some(0.25), width_pos: Some(5e-3) },
        Row { name: "clipped 0 / 4095",          d: square(0, 4095, 10, 10, 5), dt: Some(1e-3), freq: Some(50.0), duty: Some(0.5), width_pos: Some(10e-3) },
        Row { name: "noise < hysteresis",        d: noisy, dt: Some(1e-3), freq: Some(50.0), duty: Some(0.5), width_pos: Some(10e-3) },
    ];
    let close = |a: Option<f64>, b: Option<f64>, tol: f64| match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => (a - b).abs() <= tol * b.abs().max(1e-12),
        _ => false,
    };
    for row in rows {
        let m = measure(&row.d, 0.0, 1.0, row.dt);
        assert!(
            close(m.freq, row.freq, 0.02),
            "{}: freq {:?}",
            row.name,
            m.freq
        );
        assert!(
            close(m.duty, row.duty, 0.06),
            "{}: duty {:?}",
            row.name,
            m.duty
        );
        assert!(
            close(m.width_pos, row.width_pos, 0.12),
            "{}: width {:?}",
            row.name,
            m.width_pos
        );
        let lo = *row.d.iter().min().unwrap() as f32;
        let hi = *row.d.iter().max().unwrap() as f32;
        assert_eq!((m.min, m.max), (lo, hi), "{}", row.name);
        assert!(
            m.base <= m.top && m.min <= m.base && m.top <= m.max,
            "{}",
            row.name
        );
        assert!(m.rms >= m.mean.abs() - 1e-3, "{}", row.name);
    }
}

#[test]
fn measure_levels_and_edges() {
    // empty → all default
    let m = measure(&[], 0.0, 1.0, Some(1.0));
    assert_eq!((m.min, m.max, m.mean, m.freq), (0.0, 0.0, 0.0, None));
    // volts conversion: (counts − zero) × vpc
    let m = measure(&[2148; 10], 2048.0, 0.01, None);
    assert!(
        (m.mean - 1.0).abs() < 1e-5 && (m.rms - 1.0).abs() < 1e-5 && (m.max - 1.0).abs() < 1e-5
    );
    // symmetric square around zero: mean 0, rms = amplitude, amplitude top − base = 2
    let m = measure(&square(2047, 2049, 10, 10, 4), 2048.0, 1.0, None);
    assert!(
        m.mean.abs() < 1e-6 && (m.rms - 1.0).abs() < 1e-6 && (m.top - m.base - 2.0).abs() < 1e-6
    );
    // rise / fall time of a trapezoid: 10 samples per 0→1000 ramp → 10–90 % ≈ 8 samples
    let ramp_up: Vec<u16> = (0..=10).map(|i| i * 100).collect();
    let ramp_dn: Vec<u16> = ramp_up.iter().rev().copied().collect();
    let mut d = Vec::new();
    for _ in 0..3 {
        d.extend(seg(&[(20, 0)]));
        d.extend(&ramp_up);
        d.extend(seg(&[(20, 1000)]));
        d.extend(&ramp_dn);
    }
    d.extend(seg(&[(20, 0)]));
    let m = measure(&d, 0.0, 1.0, Some(1.0));
    let (rise, fall) = (m.rise.unwrap(), m.fall.unwrap());
    assert!((6.5..=9.5).contains(&rise), "rise {rise}");
    assert!((6.5..=9.5).contains(&fall), "fall {fall}");
    // an edge at the very start can't have its 10 % point inside the capture → no rise time
    let early = seg(&[(1, 0), (30, 1000), (30, 0)]);
    let m = measure(&early, 0.0, 1.0, Some(1.0));
    assert_eq!(m.rise, None);
}

#[test]
fn measure_never_panics_on_random_or_extreme_input() {
    let mut r = Lcg(11);
    for k in 0..300 {
        let n = k % 64;
        let d: Vec<u16> = (0..n)
            .map(|_| match k % 3 {
                0 => (2048.0 + 2047.0 * r.next()) as u16,
                1 => {
                    if r.next() > 0.0 {
                        4095
                    } else {
                        0
                    }
                }
                _ => u16::MAX,
            })
            .collect();
        let m = measure(&d, 2048.0, 0.01, Some(1e-6));
        for v in [
            m.freq,
            m.period,
            m.duty,
            m.width_pos,
            m.width_neg,
            m.rise,
            m.fall,
        ]
        .into_iter()
        .flatten()
        {
            assert!(v.is_finite() && v >= 0.0, "case {k}: {m:?}");
        }
        if let Some(d) = m.duty {
            assert!((0.0..=1.0).contains(&d), "case {k}");
        }
    }
}

// first_edge decisions: `span >= 8` · armed by the opposite level · then crossing the other threshold.
#[test]
fn first_edge_mcdc() {
    #[rustfmt::skip]
    let cases: &[(&str, Vec<u16>, bool, Option<usize>)] = &[
        ("flat → none",               vec![100; 20], true, None),
        ("span 7 → none",             seg(&[(5, 0), (5, 7)]), true, None),
        ("rising after a low",        seg(&[(5, 100), (5, 0), (5, 100)]), true, Some(10)),
        ("falling after a high",      seg(&[(5, 100), (5, 0), (5, 100)]), false, Some(5)),
        ("rising: never low first",   seg(&[(5, 100), (5, 100)]), true, None),
        ("falling: high, never falls", seg(&[(5, 0), (5, 100)]), false, None),
    ];
    for (name, d, rising, want) in cases {
        assert_eq!(first_edge(d, *rising), *want, "{name}");
    }
}

// ───────────────────────────── timebase / payload helpers ─────────────────────────────

#[test]
fn timebase_tables() {
    // every id maps to a unique 1-2-5 step and back
    for id in 0..26u8 {
        assert_eq!(burst_id_for_ns(burst_ns_per_div(id)), Some(id), "id {id}");
    }
    #[rustfmt::skip]
    let ns: &[(u8, u64)] = &[(0, 1), (1, 2), (2, 5), (3, 10), (15, 100_000), (16, 200_000), (17, 500_000), (18, 1_000_000), (23, 50_000_000), (25, 200_000_000)];
    for &(id, n) in ns {
        assert_eq!(burst_ns_per_div(id), n, "id {id}");
    }
    assert_eq!(burst_id_for_ns(3), None);
    assert_eq!(burst_id_for_ns(0), None);
    // window: 10 div + 2 µs, never below 1668 µs
    #[rustfmt::skip]
    let win: &[(u8, u32)] = &[(0, 1668), (15, 1668), (17, 5002), (18, 10_002), (25, 2_000_002)];
    for &(id, us) in win {
        assert_eq!(window_us(id), us, "id {id}");
    }
    // capture duration: slow-sampling correction only at 5/10/20 ms/div
    for id in 0..26u8 {
        let ratio = capture_seconds(id) / (window_us(id) as f64 * 1e-6);
        let want = match burst_ns_per_div(id) {
            5_000_000 => 1.0 / 0.9375,
            10_000_000 => 1.0 / 0.926,
            20_000_000 => 1.0 / 0.893,
            _ => 1.0,
        };
        assert!((ratio - want).abs() < 1e-9, "id {id}");
    }
    // verified timebases: 1 µs … 100 µs plus the listed ones; never the skipped 200 µs / 50 ms
    for id in 0..26u8 {
        let n = burst_ns_per_div(id);
        let want = (1_000..=100_000).contains(&n)
            || [
                500_000, 1_000_000, 2_000_000, 5_000_000, 10_000_000, 20_000_000,
            ]
            .contains(&n);
        assert_eq!(timing_verified(id), want, "id {id}");
    }
    assert!(!timing_verified(16) && !timing_verified(23));
    // roll factor: clamped to 1…8 channels, decreasing
    assert_eq!(roll_rate_factor(0), roll_rate_factor(1));
    assert_eq!(roll_rate_factor(99), 1.0);
    for n in 1..8 {
        assert!(roll_rate_factor(n) > roll_rate_factor(n + 1));
    }
    // roll ids are unique and continuous 0x18…0x24, rates descending
    for w in ROLL_RATES.windows(2) {
        assert_eq!(w[1].1, w[0].1 + 1);
        assert!(w[1].0 < w[0].0);
    }
}

#[test]
fn ac_payload_splits_the_window() {
    for &window in &[1668u32, 5002, 10_002, 2_000_002] {
        for &pos in &[0u16, 1, 250, 500, 999, 1000, 1001, u16::MAX] {
            let p = ac_payload(pos, window);
            assert_eq!(p[0], 0xac);
            let pp = u16::from_be_bytes([p[1], p[2]]);
            let pre = u32::from_be_bytes([0, p[3], p[4], p[5]]);
            let post = u32::from_be_bytes([0, p[6], p[7], p[8]]);
            assert_eq!(pp, pos.min(1000), "pos clamped");
            assert_eq!(pre + post, window, "pos {pos} window {window}");
            assert!(pre >= 1);
        }
    }
}

#[test]
fn volts_per_count_and_ranges() {
    let v: Vec<f32> = VScale::ALL.iter().map(|&s| volts_per_count(s)).collect();
    assert!(v[0] < v[1] && v[1] < v[2]);
    for (i, s) in VScale::ALL.iter().enumerate() {
        assert_eq!(s.index(), i);
        assert_eq!(s.id() as usize, i + 1);
        assert_eq!(Cal::default().vpc(3, *s), volts_per_count(*s));
    }
}

#[test]
fn decode_append_every_channel_mask() {
    // all 255 channel sets, burst (stride = n) and roll (stride = n + 1, 9th column dropped), with a partial row
    for mask in 1u16..256 {
        let active: [bool; CHANNELS] = std::array::from_fn(|c| mask & (1 << c) != 0);
        let n = active.iter().filter(|&&a| a).count();
        for extra in [0usize, 1] {
            let stride = n + extra;
            let rows = 5;
            let vals: Vec<u16> = (0..(rows * stride + stride - 1) as u16).collect();
            let bytes: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
            let mut out: [Vec<u16>; CHANNELS] = Default::default();
            assert_eq!(
                decode_append(&bytes, &active, stride, &mut out),
                rows,
                "mask {mask:#x}"
            );
            let mut col = 0;
            for ch in 0..CHANNELS {
                if active[ch] {
                    let want: Vec<u16> = (0..rows).map(|r| (r * stride + col) as u16).collect();
                    assert_eq!(out[ch], want, "mask {mask:#x} ch {ch} stride {stride}");
                    col += 1;
                } else {
                    assert!(out[ch].is_empty());
                }
            }
        }
    }
}
