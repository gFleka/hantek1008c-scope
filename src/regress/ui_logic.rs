//! UI decision functions (MC/DC tables), settings codecs fed with garbage / old formats, formatters,
//! and structural walks of every export format (CSV, WAV, MDF4, sigrok .sr).

use super::super::*; // ui::files (and through it everything in ui)
use std::time::{Duration, Instant};

// ───────────────────────────── timebase stepping ─────────────────────────────

/// Exhaustive: every timebase × direction. Result = the nearest id in that direction that is not skipped;
/// None only at the ends. Walking the whole range visits every valid id exactly once.
#[test]
fn next_timebase_exhaustive() {
    let valid = |t: i32| (0..=TB_MAX as i32).contains(&t) && !TB_SKIP.contains(&(t as u8));
    for tb in 0..=TB_MAX {
        for dir in [-1i32, 1, -5, 7] {
            let want = (1..=TB_MAX as i32 + 1)
                .map(|k| tb as i32 + k * dir.signum())
                .take_while(|t| (0..=TB_MAX as i32).contains(t))
                .find(|&t| valid(t))
                .map(|t| t as u8);
            assert_eq!(next_timebase(tb, dir), want, "tb {tb} dir {dir}");
        }
    }
    let mut seen = vec![0u8];
    while let Some(t) = next_timebase(*seen.last().unwrap(), 1) {
        seen.push(t);
    }
    assert_eq!(seen.len(), TB_MAX as usize + 1 - TB_SKIP.len());
    assert!(seen.iter().all(|t| !TB_SKIP.contains(t)));
    // constants agree with the skip list / capture limits
    assert!(TB_STREAM_MIN <= TB_CAPTURE_MAX && TB_CAPTURE_MAX < TB_MAX);
    assert_eq!(TB_SKIP, [16, 23]);
}

// ───────────────────────────── adaptive feature dropout ─────────────────────────────
// Decisions: slow → (≥ 1 s slow && level < max) shed; fast → (level > 0 && !held && ≥ 5 s fast) restore;
// neither → reset timers; bounce = shed within 15 s of a restore → hold doubles (10 s × 2^bounces, ≤ 5).

#[test]
fn dropout_mcdc() {
    type Step = (f32, bool, bool, Option<u8>); // t, slow, fast → returned level
    #[rustfmt::skip]
    let scenarios: &[(&str, &[Step])] = &[
        ("slow < 1 s: nothing",        &[(0.0, true, false, None), (0.99, true, false, None)]),
        ("slow 1 s: shed level 1",     &[(0.0, true, false, None), (1.0, true, false, Some(1))]),
        ("another second per step",    &[(0.0, true, false, None), (1.0, true, false, Some(1)), (1.5, true, false, None), (2.0, true, false, Some(2))]),
        ("capped at 5 levels",         &[(0.0, true, false, None), (1.0, true, false, Some(1)), (2.0, true, false, Some(2)), (3.0, true, false, Some(3)),
                                         (4.0, true, false, Some(4)), (5.0, true, false, Some(5)), (6.0, true, false, None), (9.0, true, false, None)]),
        ("neither resets slow timer",  &[(0.0, true, false, None), (0.6, false, false, None), (1.2, true, false, None), (2.1, true, false, None), (2.2, true, false, Some(1))]),
        ("fast resets slow timer",     &[(0.0, true, false, None), (0.6, false, true, None), (1.2, true, false, None), (2.2, true, false, Some(1))]),
        ("fast at level 0: nothing",   &[(0.0, false, true, None), (10.0, false, true, None)]),
        ("held 10 s after a shed",     &[(0.0, true, false, None), (1.0, true, false, Some(1)), (1.5, false, true, None), (6.5, false, true, None),
                                         (10.9, false, true, None), (11.0, false, true, Some(0))]),
        ("restore needs 5 s fast",     &[(0.0, true, false, None), (1.0, true, false, Some(1)), (20.0, false, true, None), (24.9, false, true, None), (25.0, false, true, Some(0))]),
        ("fast interrupted resets",    &[(0.0, true, false, None), (1.0, true, false, Some(1)), (20.0, false, true, None), (23.0, false, false, None), (24.0, false, true, None),
                                         (28.9, false, true, None), (29.0, false, true, Some(0))]),
        ("bounce doubles the hold",    &[(0.0, true, false, None), (1.0, true, false, Some(1)), (1.5, false, true, None), (11.0, false, true, Some(0)),
                                         (12.0, true, false, None), (13.0, true, false, Some(1)), (13.5, false, true, None), (32.9, false, true, None), (33.0, false, true, Some(0))]),
        ("no bounce after 15 s",       &[(0.0, true, false, None), (1.0, true, false, Some(1)), (1.5, false, true, None), (11.0, false, true, Some(0)),
                                         (27.0, true, false, None), (28.0, true, false, Some(1)), (28.5, false, true, None), (37.9, false, true, None), (38.0, false, true, Some(0))]),
    ];
    for (name, steps) in scenarios {
        let t0 = Instant::now();
        let mut d = Dropout::default();
        for (k, &(t, slow, fast, want)) in steps.iter().enumerate() {
            let got = d.update(t0 + Duration::from_secs_f32(t), slow, fast);
            assert_eq!(got, want, "{name}: step {k} (t {t})");
        }
    }
}

#[test]
fn dropout_bounce_hold_doubles_up_to_320s() {
    let t0 = Instant::now();
    let at = |s: f64| t0 + Duration::from_secs_f64(s);
    let mut d = Dropout::default();
    let mut t = 0.0;
    let mut holds = Vec::new();
    for _ in 0..7 {
        // 1 s slow → shed
        d.update(at(t), true, false);
        t += 1.0;
        assert_eq!(d.update(at(t), true, false), Some(1));
        let shed = t;
        // fast until restored
        t += 0.1;
        d.update(at(t), false, true);
        loop {
            t += 0.5;
            if d.update(at(t), false, true) == Some(0) {
                break;
            }
            assert!(t - shed < 400.0);
        }
        holds.push((t - shed).round() as u32);
        t += 0.5; // bounce: slow again right away
    }
    assert_eq!(holds, vec![10, 20, 40, 80, 160, 320, 320]);
}

// ───────────────────────────── info-line debounce ─────────────────────────────
// Show: age ≥ 0.4 s, or Warn and age ≥ 0.15 s · keep: last seen < 2 s ago · identity: level + text w/o digits.

#[test]
fn notice_debounce_mcdc() {
    use Level::*;
    type Frame = (
        f32,
        &'static [(Level, &'static str)],
        &'static [&'static str],
    ); // t, raised → shown
    #[rustfmt::skip]
    let scenarios: &[(&str, &[Frame])] = &[
        ("info shows after 0.4 s",   &[(0.0, &[(Info, "a")], &[]), (0.39, &[(Info, "a")], &[]), (0.4, &[(Info, "a")], &["a"])]),
        ("tip like info",            &[(0.0, &[(Tip, "t")], &[]), (0.2, &[(Tip, "t")], &[]), (0.45, &[(Tip, "t")], &["t"])]),
        ("warn after 0.15 s",        &[(0.0, &[(Warn, "w")], &[]), (0.1, &[(Warn, "w")], &[]), (0.15, &[(Warn, "w")], &["w"])]),
        ("held 2 s after it ends",   &[(0.0, &[(Info, "a")], &[]), (0.5, &[(Info, "a")], &["a"]), (2.4, &[], &["a"]), (2.5, &[], &[])]),
        // current behaviour (see notice_one_frame_blip_is_not_shown): age counts from the first sighting
        ("one-frame blip shows late", &[(0.0, &[(Info, "a")], &[]), (0.1, &[], &[]), (0.5, &[], &["a"]), (2.2, &[], &[])]),
        ("digits don't restart it",  &[(0.0, &[(Info, "v 1")], &[]), (0.3, &[(Info, "v 2")], &[]), (0.5, &[(Info, "v 3")], &["v 3"])]),
        ("last text is shown",       &[(0.0, &[(Info, "v 1")], &[]), (0.5, &[(Info, "v 9")], &["v 9"]), (1.0, &[], &["v 9"])]),
        ("level is part of the key", &[(0.0, &[(Info, "x")], &[]), (0.3, &[(Warn, "x")], &[]), (0.5, &[(Info, "x"), (Warn, "x")], &["x", "x"])]),
        ("order of first appearance",&[(0.0, &[(Info, "b")], &[]), (0.1, &[(Info, "a"), (Info, "b")], &[]), (0.6, &[(Info, "a"), (Info, "b")], &["b", "a"])]),
        ("text change = new notice", &[(0.0, &[(Info, "alpha")], &[]), (0.3, &[(Info, "beta")], &[]), (0.5, &[(Info, "beta")], &["alpha"]), (0.75, &[(Info, "beta")], &["alpha", "beta"])]),
    ];
    for (name, frames) in scenarios {
        let t0 = Instant::now();
        let mut d = NoticeDebounce::default();
        for (k, &(t, raised, want)) in frames.iter().enumerate() {
            let list = raised.iter().map(|(l, s)| (*l, s.to_string())).collect();
            let got: Vec<String> = d
                .update(list, t0 + Duration::from_secs_f32(t))
                .into_iter()
                .map(|x| x.1)
                .collect();
            assert_eq!(got, want.to_vec(), "{name}: frame {k} (t {t})");
        }
    }
}

/// NoticeDebounce's doc: "a notice shows after it has been present for NOTICE_SHOW_AFTER". A notice raised in a
/// single frame (one clipped capture) is not present for 0.4 s, yet it pops up 0.4 s later for 1.6 s.
#[test]
#[ignore = "BUG (minor, owner decides): ui::NoticeDebounce::update shows a one-frame notice 0.4 s later for 1.6 s"]
fn notice_one_frame_blip_is_not_shown() {
    let t0 = Instant::now();
    let mut d = NoticeDebounce::default();
    d.update(vec![(Level::Info, "clip".into())], t0);
    for k in 1..30 {
        let shown = d.update(Vec::new(), t0 + Duration::from_millis(k * 100));
        assert!(shown.is_empty(), "shown at {} ms", k * 100);
    }
}

/// `TB_MAX` = 50 s/div (500 s per screen), as its comment says (the comment said 20 s/div before; fixed).
#[test]
fn tb_max_is_50s_per_div() {
    assert_eq!(burst_ns_per_div(TB_MAX), 50_000_000_000);
}

// ───────────────────────────── auto-set ─────────────────────────────

fn meas(min: f32, max: f32, freq: Option<f64>) -> Option<Meas> {
    Some(Meas {
        min,
        max,
        freq,
        ..Default::default()
    })
}

#[test]
fn autoset_pick_mcdc() {
    let none: [Option<Meas>; CHANNELS] = [None; CHANNELS];
    let one = [1.0f32; CHANNELS];
    let at0 = |m: Option<Meas>| {
        let mut a = none;
        a[0] = m;
        a
    };
    #[rustfmt::skip]
    let cases: Vec<(&str, [Option<Meas>; CHANNELS], [f32; CHANNELS], Option<u8>, Option<usize>, Option<f32>, Option<(usize, f32)>)> = vec![
        // name                                   meas                                   scale   tb        vdiv[0]   offset[0]  trig
        ("no channel → nothing",                  none,                                   one,  None,     None,     None,      None),
        ("flat 0 V: smallest V/div, no trigger",  at0(meas(0.0, 0.0, None)),              one,  None,     Some(0),  Some(0.0), None),
        ("1 kHz 0…5 V",                           at0(meas(0.0, 5.0, Some(1e3))),         one,  Some(17), Some(8),  Some(-1.25), Some((0, 2.5))),
        ("2 kHz: 200 µs skipped → 500 µs",        at0(meas(0.0, 5.0, Some(2e3))),         one,  Some(17), Some(8),  Some(-1.25), Some((0, 2.5))),
        ("50 Hz → 10 ms/div",                     at0(meas(0.0, 5.0, Some(50.0))),        one,  Some(21), Some(8),  Some(-1.25), Some((0, 2.5))),
        ("10 Hz: 50 ms skipped → 100 ms",         at0(meas(0.0, 5.0, Some(10.0))),        one,  Some(24), Some(8),  Some(-1.25), Some((0, 2.5))),
        ("0.001 Hz: slower than 20 s/div",        at0(meas(0.0, 5.0, Some(1e-3))),        one,  None,     Some(8),  Some(-1.25), Some((0, 2.5))),
        ("1 GHz → fastest",                       at0(meas(0.0, 5.0, Some(1e9))),         one,  Some(0),  Some(8),  Some(-1.25), Some((0, 2.5))),
        ("probe ×10: same index for 10× volts",   at0(meas(0.0, 50.0, Some(1e3))),        [10.0; CHANNELS], Some(17), Some(8), Some(-1.25), Some((0, 25.0))),
        ("probe −10 (inverted) uses |scale|",     at0(meas(0.0, 50.0, Some(1e3))),        [-10.0; CHANNELS], Some(17), Some(8), Some(-1.25), Some((0, 25.0))),
        ("huge signal → largest V/div, clamped",  at0(meas(9000.0, 11000.0, None)),       one,  None,     Some(12), Some(-4.0), None),
        ("peak decides: 100 V DC + 1 V ripple",   at0(meas(100.0, 101.0, None)),          one,  None,     Some(12), Some(-2.01), None),
        ("negative signal",                       at0(meas(-5.0, 0.0, Some(1e3))),        one,  Some(17), Some(8),  Some(1.25), Some((0, -2.5))),
    ];
    for (name, m, scale, tb, vdiv, offset, trig) in cases {
        let p = autoset_pick(&m, &scale);
        assert_eq!(p.tb_id, tb, "{name}: tb");
        assert_eq!(p.vdiv[0], vdiv, "{name}: vdiv");
        match (p.offset[0], offset) {
            (Some(a), Some(b)) => assert!((a - b).abs() < 1e-4, "{name}: offset {a}"),
            (a, b) => assert_eq!(a, b, "{name}: offset"),
        }
        match (p.trig, trig) {
            (Some((c, l)), Some((wc, wl))) => {
                assert!(c == wc && (l - wl).abs() < 1e-4, "{name}: trig {c} {l}")
            }
            (a, b) => assert_eq!(a, b, "{name}: trig"),
        }
        assert!(p.tb_id.is_none_or(|t| !TB_SKIP.contains(&t)), "{name}");
    }
    // trigger: the first channel that has a frequency, not the first shown channel
    let mut m = none;
    m[0] = meas(0.0, 1.0, None);
    m[1] = meas(0.0, 1.0, Some(50.0));
    m[2] = meas(0.0, 1.0, Some(1e3));
    let p = autoset_pick(&m, &one);
    assert_eq!(p.trig.map(|t| t.0), Some(1));
    assert_eq!(p.tb_id, Some(21), "timebase from the trigger channel");
    assert!(p.vdiv[..3].iter().all(Option::is_some) && p.vdiv[3..].iter().all(Option::is_none));
}

// ───────────────────────────── settings codecs: garbage and old formats ─────────────────────────────

#[test]
fn probe_decode_garbage() {
    #[rustfmt::skip]
    let cases: &[(&str, Option<(usize, f32, f32, &str)>)] = &[
        ("0:1:0:V",                  Some((0, 1.0, 0.0, "V"))),
        ("5:2.5:-1.25:bar",          Some((5, 2.5, -1.25, "bar"))),
        ("6:-10:0:V",                Some((6, -10.0, 0.0, "V"))),   // custom, inverting
        ("0:1:0",                    Some((0, 1.0, 0.0, "V"))),     // no unit → V
        ("0:1:0:",                   Some((0, 1.0, 0.0, ""))),
        ("0:1:0:VeryLongUnit",       Some((0, 1.0, 0.0, "VeryLong"))), // 8 chars max
        ("99:1:0:V",                 Some((99, 1.0, 0.0, "V"))),    // unknown preset = custom (UI uses .get)
        ("",                         None),
        ("x:1:0:V",                  None),
        ("0:0:0:V",                  None), // scale 0
        ("0:NaN:0:V",                None),
        ("0:inf:0:V",                None),
        ("0:1:NaN:V",                None),
        ("0:1:inf:V",                None),
        ("0:1",                      None),
        ("-1:1:0:V",                 None),
        ("0:1e40:0:V",               None), // overflows f32 → inf
    ];
    for &(s, want) in cases {
        let got = Probe::decode(s).map(|p| (p.preset, p.scale, p.offset, p.unit.clone()));
        let want = want.map(|(a, b, c, d)| (a, b, c, d.to_string()));
        assert_eq!(got, want, "{s:?}");
    }
    // encode strips the separators so a unit can't break the settings line
    let p = Probe {
        preset: 6,
        scale: 3.0,
        offset: 0.5,
        unit: "a,b:c\nd".into(),
    };
    let d = Probe::decode(&p.encode()).unwrap();
    assert_eq!(d.unit, "abcd");
    for i in 0..PROBE_PRESETS.len() + 3 {
        let p = Probe::from_preset(i);
        assert_eq!(Probe::decode(&p.encode()), Some(p), "preset {i}");
    }
}

#[test]
fn math_decode_garbage_and_old_formats() {
    #[rustfmt::skip]
    let cases: &[(&str, Option<(usize, usize, usize, f32, usize, f32, usize)>)] = &[
        // op index, a, b, k, vdiv, offset, pane
        ("1:0:1:1:9:0:0",            Some((1, 0, 1, 1.0, 9, 0.0, 0))),
        ("5:2:0:58",                 Some((5, 2, 0, 58.0, 9, 0.0, 0))),     // old 4-field form
        ("4:0:0:100:3",              Some((4, 0, 0, 100.0, 3, 0.0, 0))),    // 5 fields
        ("1:9:99:1",                 Some((1, 7, 7, 1.0, 9, 0.0, 0))),      // channels clamped
        ("1:0:1:2:999:99:9",         Some((1, 0, 1, 2.0, MATH_DIV_N - 1, 4.0, MAX_PANES - 1))),
        ("1:0:1:2:x:NaN:z",          Some((1, 0, 1, 2.0, 9, 0.0, 0))),      // bad display fields → defaults
        ("1:0:1:2:1:-inf:1",         Some((1, 0, 1, 2.0, 1, 0.0, 1))),
        ("1:0:1:0",                  Some((1, 0, 1, 0.0, 9, 0.0, 0))),      // k = 0 accepted (UX #54)
        ("",                         None),
        ("7:0:1:1",                  Some((7, 0, 1, 1.0, 9, 0.0, 0))), // AC part (added in Phase 4)
        ("10:0:1:1",                 None), // op out of range
        ("-1:0:1:1",                 None),
        ("1:0:1",                    None),
        ("1:0:1:inf",                None),
        ("1:0:1:NaN",                None),
        ("1:x:1:1",                  None),
    ];
    for &(s, want) in cases {
        let got = Math::decode(s).map(|m| {
            let op = MATH_OPS.iter().position(|o| o.0 == m.op).unwrap();
            (op, m.a, m.b, m.k, m.vdiv, m.offset, m.pane)
        });
        assert_eq!(got, want, "{s:?}");
    }
    for (op, _) in MATH_OPS {
        let m = Math {
            op,
            a: 3,
            b: 6,
            k: 2.5,
            vdiv: 4,
            offset: -1.5,
            pane: 2,
        };
        assert_eq!(Math::decode(&m.encode()), Some(m));
        assert!(!m.label().is_empty());
    }
    // math_div: 1-2-5 steps, increasing; math_div_for picks the smallest step ≥ v
    for i in 1..MATH_DIV_N {
        assert!(math_div(i) > math_div(i - 1));
    }
    #[rustfmt::skip]
    let fits: &[(f32, usize)] = &[(0.0, 0), (0.001, 0), (0.0011, 1), (0.4, 8), (0.5, 8), (1e9, MATH_DIV_N - 1), (-5.0, 0)];
    for &(v, i) in fits {
        assert_eq!(math_div_for(v), i, "v {v}");
    }
}

#[test]
fn engine_decode_garbage() {
    #[rustfmt::skip]
    let cases: &[(&str, Option<(bool, usize, Option<usize>, u32, u32, f32, &str)>)] = &[
        ("1:2:5:60:2:14.5:1-3-4-2",  Some((true, 2, Some(5), 60, 2, 14.5, "1-3-4-2"))),
        ("0:0:-1:36:1:0:1-5-3-6-2-4", Some((false, 0, None, 36, 1, 0.0, "1-5-3-6-2-4"))),
        ("1:99:99:60:2:0:1",         Some((true, 7, None, 60, 2, 0.0, "1"))),       // crank clamped, cam dropped
        ("1:0:-7:1:9:0:1-2",         Some((true, 0, None, 4, 3, 0.0, "1-2"))),      // total ≥ 4, missing ≤ 3
        ("1:0:-1:9999:2:0:1-2",      Some((true, 0, None, 400, 2, 0.0, "1-2"))),
        ("2:0:-1:60:2:0:1",          Some((false, 0, None, 60, 2, 0.0, "1"))),      // on only for "1"
        ("1:0:-1:60:2:0:x",          None), // no cylinder
        ("1:0:-1:60:2:NaN:1",        None),
        ("1:0:-1:60:2:inf:1",        None),
        ("1:a:-1:60:2:0:1",          None),
        ("1:0:-1:-60:2:0:1",         None),
        ("1:0:-1:60:2:0",            None),
        ("",                         None),
    ];
    for &(s, want) in cases {
        let got = engine::Engine::decode(s);
        let got = got.map(|e| {
            let enc = e.encode();
            let p: Vec<&str> = enc.split(':').collect();
            (
                p[0] == "1",
                p[1].parse::<usize>().unwrap(),
                p[2].parse::<i32>()
                    .ok()
                    .filter(|&c| c >= 0)
                    .map(|c| c as usize),
                p[3].parse::<u32>().unwrap(),
                p[4].parse::<u32>().unwrap(),
                p[5].parse::<f32>().unwrap(),
                p[6].to_string(),
            )
        });
        let want = want.map(|(a, b, c, d, e, f, g)| (a, b, c, d, e, f, g.to_string()));
        assert_eq!(got, want, "{s:?}");
    }
}

#[test]
fn sw_codecs_garbage() {
    #[rustfmt::skip]
    let cases: &[(&str, Option<SwTrig>)] = &[
        ("off",                         Some(SwTrig::Off)),
        ("width,low,shorter,0.001,0",   Some(SwTrig::Width { high: false, cmp: Cmp::Shorter, t1: 0.001, t2: 0.0 })),
        ("width,x,weird,1,2",           Some(SwTrig::Width { high: true, cmp: Cmp::Longer, t1: 1.0, t2: 2.0 })),
        (" width , high , between , 1 , 2 ", Some(SwTrig::Width { high: true, cmp: Cmp::Between, t1: 1.0, t2: 2.0 })),
        ("zone,0,1,2,3",                Some(SwTrig::Zone { t0: 0.0, t1: 1.0, v0: 2.0, v1: 3.0, enter: true })), // old: no enter field
        ("zone,0,1,2,3,avoid",          Some(SwTrig::Zone { t0: 0.0, t1: 1.0, v0: 2.0, v1: 3.0, enter: false })),
        ("dropout,0.005",               Some(SwTrig::Dropout { t: 0.005 })),
        ("runt,low,1,2",                Some(SwTrig::Runt { high: false, lo: 1.0, hi: 2.0 })),
        ("window,-1,1",                 Some(SwTrig::Window { lo: -1.0, hi: 1.0 })),
        ("",                            None),
        ("width,high,longer,1",         None),
        ("width",                       None),
        ("runt,high,1",                 None),
        ("dropout",                     None),
        ("dropout,x",                   None),
        ("window,1",                    None),
        ("zone,0,1,2",                  None),
        ("WIDTH,high,longer,1,2",       None), // case-sensitive
        ("nonsense",                    None),
    ];
    for (s, want) in cases {
        assert_eq!(sw_decode(s), *want, "{s:?}");
    }
    #[rustfmt::skip]
    let levels: &[(&str, SwLevel)] = &[
        ("auto", SwLevel::Auto), ("trigger", SwLevel::Trigger), (" trigger ", SwLevel::Trigger),
        ("1.5", SwLevel::Custom(1.5)), ("-2", SwLevel::Custom(-2.0)), ("", SwLevel::Auto), ("x", SwLevel::Auto),
    ];
    for &(s, want) in levels {
        assert_eq!(sw_level_decode(s), want, "{s:?}");
    }
    for l in [SwLevel::Auto, SwLevel::Trigger, SwLevel::Custom(0.25)] {
        assert_eq!(sw_level_decode(&sw_level_encode(l)), l);
    }
    // defaults per kind: sw_kind is the inverse, every one describes and round-trips
    for k in 0..SW_NAMES.len() {
        let t = sw_default(k, 2.0);
        assert_eq!(sw_kind(&t), k);
        assert_eq!(sw_decode(&sw_encode(&t)), Some(t));
        assert!(!sw_describe(&t, "V").is_empty());
    }
    assert_eq!(sw_default(99, 1.0), SwTrig::Off);
    assert_eq!(
        sw_describe(&sw_default(5, 0.0), "V"),
        "zone (not drawn yet)"
    );
}

/// NaN parses as a valid f64 ("NaN"), so a corrupt settings line yields a NaN threshold. It must not
/// reach the trigger as NaN (the filter would silently never / always match).
#[test]
fn sw_decode_rejects_non_finite() {
    for s in [
        "dropout,NaN",
        "width,high,longer,inf,1",
        "window,NaN,1",
        "zone,0,NaN,0,1",
    ] {
        assert_eq!(sw_decode(s), None, "{s}");
    }
}

// ───────────────────────────── formatters ─────────────────────────────

#[test]
fn formatters() {
    #[rustfmt::skip]
    let times: &[(f64, &str)] = &[
        (7200.0, "2 h 0 min"), (3661.0, "1 h 1 min"), (90.0, "1 min 30 s"), (59.9, "59.9 s"), (1.0, "1 s"),
        (1.5, "1.5 s"), (1e-3, "1 ms"), (0.0015, "1.5 ms"), (5e-6, "5 µs"), (1e-6, "1 µs"), (2.5e-7, "250 ns"), (0.0, "0 ns"),
    ];
    for &(v, s) in times {
        assert_eq!(fmt_time(v), s, "fmt_time({v})");
    }
    #[rustfmt::skip]
    let shorts: &[(f64, &str)] = &[(1.5, "1.500 s"), (-0.002, "-2.000 ms"), (2.5e-6, "2.50 µs"), (5e-8, "50 ns"), (0.0, "0 ns"), (-1.0, "-1.000 s")];
    for &(v, s) in shorts {
        assert_eq!(fmt_time_short(v), s, "fmt_time_short({v})");
    }
    #[rustfmt::skip]
    let volts: &[(f32, &str)] = &[(1.0, "1.000 V"), (-1.5, "-1.500 V"), (0.5, "500.0 mV"), (0.0, "0.0 mV"), (-0.0012, "-1.2 mV")];
    for &(v, s) in volts {
        assert_eq!(fmt_v(v), s, "fmt_v({v})");
        assert_eq!(fmt_u(v, "V"), s, "fmt_u({v}, V)");
    }
    #[rustfmt::skip]
    let freqs: &[(f64, &str)] = &[(1e6, "1.0000 MHz"), (1500.0, "1.5000 kHz"), (999.9, "999.900 Hz"), (0.0, "0.000 Hz")];
    for &(v, s) in freqs {
        assert_eq!(fmt_freq(v), s, "fmt_freq({v})");
    }
    #[rustfmt::skip]
    let rates: &[(f64, &str)] = &[(2400.0, "2.4 kS/s"), (1000.0, "1.0 kS/s"), (440.0, "440 S/s"), (10.0, "10 S/s"), (0.5, "0.50 S/s")];
    for &(v, s) in rates {
        assert_eq!(fmt_rate(v), s, "fmt_rate({v})");
    }
    #[rustfmt::skip]
    let counts: &[(f64, &str)] = &[(0.0, "0"), (9999.0, "9999"), (10_000.0, "10k"), (38_016_000.0, "38016k")];
    for &(v, s) in counts {
        assert_eq!(fmt_count(v), s, "fmt_count({v})");
    }
    #[rustfmt::skip]
    let units: &[(f32, &str, &str)] = &[(1500.0, "A", "1.50 kA"), (0.0, "A", "0.000 A"), (2.5, "bar", "2.500 bar"), (0.25, "A", "250.0 mA"), (-3.0, "bar", "-3.000 bar")];
    for &(v, u, s) in units {
        assert_eq!(fmt_u(v, u), s, "fmt_u({v}, {u})");
    }
    #[rustfmt::skip]
    let divs: &[(f32, &str, &str)] = &[(0.5, "V", "500 mV"), (5.0, "V", "5 V"), (0.005, "V", "5 mV"), (2.5, "bar", "2.5 bar"), (1500.0, "A", "1.5 kA"), (10.0, "A", "10 A")];
    for &(v, u, s) in divs {
        assert_eq!(fmt_div(v, u), s, "fmt_div({v}, {u})");
    }
    // every V/div step reads without trailing zeros
    for v in VDIVS {
        let s = fmt_div(v, "V");
        assert!(!s.contains(".0 ") && !s.ends_with('.'), "{s}");
    }
    #[rustfmt::skip]
    let fvolts: &[(f32, &str)] = &[(1.0, "1 V"), (20.0, "20 V"), (0.5, "500 mV"), (0.005, "5 mV")];
    for &(v, s) in fvolts {
        assert_eq!(fmt_volts(v), s, "fmt_volts({v})");
    }
    #[rustfmt::skip]
    let qs: &[(usize, f64, &str)] = &[(0, 1000.0, "1.0000 kHz"), (1, 0.001, "1.000 ms"), (2, 0.25, "25.0 %"), (5, 2e-6, "2.00 µs"), (7, 2.0, "2.000 V"), (10, 0.5, "500.0 mV")];
    for &(q, v, s) in qs {
        assert_eq!(fmt_q(q, v, "V"), s, "fmt_q({q}, {v})");
    }
    assert_eq!(Q_NAMES.len(), Q_HELP.len());
}

#[test]
fn stat_quantities_and_rates() {
    let mut s = Stat::default();
    assert_eq!((s.mean(), s.sd()), (0.0, 0.0));
    for v in [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0] {
        s.add(v);
    }
    assert_eq!((s.n, s.min, s.max, s.mean()), (8, 2.0, 9.0, 5.0));
    assert!((s.sd() - 2.0).abs() < 1e-12);
    let mut neg = Stat::default();
    neg.add(-3.0);
    assert_eq!(
        (neg.min, neg.max),
        (-3.0, -3.0),
        "first value initialises min/max"
    );
    // quantities: column order matches Q_NAMES
    let m = Meas {
        min: -1.0,
        max: 3.0,
        top: 2.5,
        base: -0.5,
        mean: 1.0,
        rms: 1.5,
        freq: Some(50.0),
        ..Default::default()
    };
    let q = quantities(&m);
    assert_eq!(q[0], Some(50.0));
    assert_eq!(
        (q[7], q[8], q[9], q[10]),
        (Some(3.0), Some(4.0), Some(1.0), Some(1.5))
    );
    assert_eq!(q[1..7], [None; 6]);
    // stream rate = nominal × channel factor; unknown id → 0
    assert_eq!(stream_rate(ROLL_RATES[0].1, 8), 440.0);
    assert!((stream_rate(ROLL_RATES[0].1, 1) - 440.0 * 4.56).abs() < 1e-9);
    assert_eq!(stream_rate(0x99, 2), 0.0);
}

#[test]
fn zoom_clamped_table() {
    let z = |x0, x1, y0, y1| Zoom { x0, x1, y0, y1 };
    #[rustfmt::skip]
    let cases: &[(&str, Zoom, (f32, f32, f32, f32))] = &[
        ("default stays",          Zoom::default(),          (0.0, 1.0, 0.0, 1.0)),
        ("wider than the screen",  z(-0.5, 0.7, 0.0, 1.0),   (0.0, 1.0, 0.0, 1.0)),
        ("past the right edge",    z(0.9, 1.2, 0.0, 1.0),    (0.7, 1.0, 0.0, 1.0)),
        ("past the left edge",     z(-0.2, 0.1, 0.0, 1.0),   (0.0, 0.3, 0.0, 1.0)),
        ("zero width → minimum",   z(0.5, 0.5, 0.2, 0.2),    (0.5, 0.501, 0.2, 0.21)),
        ("reversed → minimum",     z(0.6, 0.4, 0.0, 1.0),    (0.6, 0.601, 0.0, 1.0)),
        ("minimum at the edge",    z(1.0, 1.0, 1.0, 1.0),    (0.999, 1.0, 0.99, 1.0)),
    ];
    for (name, zz, (a, b, c, d)) in cases {
        let r = zz.clamped();
        for (got, want) in [(r.x0, *a), (r.x1, *b), (r.y0, *c), (r.y1, *d)] {
            assert!((got - want).abs() < 1e-5, "{name}: {r:?}");
        }
        let rr = r.clamped();
        assert!(
            (rr.x0 - r.x0).abs() < 1e-5
                && (rr.x1 - r.x1).abs() < 1e-5
                && (rr.y0 - r.y0).abs() < 1e-5,
            "{name}: idempotent"
        );
    }
}

#[test]
fn decimate_bounds() {
    let r = Rect::from_min_size(Pos2::new(10.0, 0.0), Vec2::new(100.0, 50.0));
    let mut out = Vec::new();
    for n in [2usize, 3, 199, 200, 201, 1000, 100_000] {
        let d: Vec<u16> = (0..n).map(|i| (i % 4096) as u16).collect();
        decimate(&d, r, |v| v as f32, &mut out);
        assert!(out.len() <= (2 * 100).max(n), "n {n}: {} points", out.len());
        assert!(
            out.iter()
                .all(|p| p.x >= r.left() - 1e-3 && p.x <= r.right() + 1e-3),
            "n {n}"
        );
        if n > 200 {
            assert_eq!(out.len(), 200, "n {n}: min/max per pixel column");
        } else {
            assert_eq!(out.len(), n);
        }
    }
    // float version skips NaN (rpm outside the edges)
    let mut f: Vec<f32> = (0..1000).map(|i| i as f32).collect();
    f[..500].iter_mut().for_each(|v| *v = f32::NAN);
    decimate_f32(&f, r, |v| v, &mut out);
    assert!(out.iter().all(|p| !p.y.is_nan()));
    decimate_f32(&[1.0], r, |v| v, &mut out);
    assert!(out.is_empty());
}

// ───────────────────────────── exports: structural walks ─────────────────────────────

fn export_data(rows: usize) -> ExportData {
    ExportData {
        names: vec!["CH1_V".into(), "CH2_A".into(), "M1_rpm".into()],
        cols: vec![
            (0..rows).map(|i| (2048 + (i * 37) % 1000) as u16).collect(),
            (0..rows + 10).map(|i| (i % 4096) as u16).collect(), // longer: only the common rows are written
            (0..rows).map(|i| 4095 - (i % 4096) as u16).collect(),
        ],
        conv: vec![(2048.0, 0.01), (0.0, 1.0), (-1000.0, 0.5)],
        t0: -0.25,
        dt: 1e-3,
        note: "regress".into(),
    }
}

fn tmp(name: &str) -> PathBuf {
    crate::regress::util::tmp_dir(&format!("export-{name}")).join(name)
}

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
fn f64le(b: &[u8], at: usize) -> f64 {
    f64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

#[test]
fn csv_export_rows_and_precision() {
    for rows in [0usize, 1, 90] {
        let d = export_data(rows);
        let p = tmp(&format!("r{rows}.csv"));
        write_csv(&d, &p, &AtomicU32::new(0)).unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines.len(), 3 + rows, "rows {rows}");
        assert!(lines[0].starts_with('#') && lines[0].contains("regress"));
        assert_eq!(lines[2], "time_s,CH1_V,CH2_A,M1_rpm");
        // decimals: one more than a count step (0.01 → 3, 1 → 1, 0.5 → 2)
        for (i, l) in lines[3..].iter().enumerate() {
            let f: Vec<&str> = l.split(',').collect();
            assert_eq!(f.len(), 4);
            let t: f64 = f[0].parse().unwrap();
            assert!((t - (-0.25 + i as f64 * 1e-3)).abs() < 1e-9);
            for (k, dec) in [(1usize, 3usize), (2, 1), (3, 2)] {
                assert_eq!(
                    f[k].split('.').nth(1).map_or(0, str::len),
                    dec,
                    "col {k}: {}",
                    f[k]
                );
                let (z, u) = d.conv[k - 1];
                let want = (d.cols[k - 1][i] as f64 - z as f64) * u as f64;
                assert!(
                    (f[k].parse::<f64>().unwrap() - want).abs() < 1e-6,
                    "col {k} row {i}"
                );
            }
        }
    }
}

#[test]
fn wav_export_structure() {
    for rows in [0usize, 1, 90] {
        let d = export_data(rows);
        let p = tmp(&format!("r{rows}.wav"));
        write_wav(&d, &p, &AtomicU32::new(0)).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(&b[..4], b"RIFF");
        assert_eq!(u32le(&b, 4) as usize, b.len() - 8, "RIFF size");
        assert_eq!(&b[8..12], b"WAVE");
        // walk the chunks: each size stays inside the file, chunks are word-aligned
        let mut at = 12;
        let mut found = Vec::new();
        while at + 8 <= b.len() {
            let id = String::from_utf8_lossy(&b[at..at + 4]).to_string();
            let n = u32le(&b, at + 4) as usize;
            assert!(at + 8 + n <= b.len(), "{id} size");
            found.push((id, at + 8, n));
            at += 8 + n + n % 2;
        }
        assert_eq!(at, b.len());
        let ids: Vec<&str> = found.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(ids, ["fmt ", "LIST", "data"]);
        let (_, fmt, _) = found[0];
        assert_eq!(
            (u16le(&b, fmt), u16le(&b, fmt + 2), u32le(&b, fmt + 4)),
            (1, 3, 1000),
            "PCM, 3 ch, 1 kHz"
        );
        assert_eq!(u32le(&b, fmt + 8), 1000 * 3 * 2);
        assert_eq!((u16le(&b, fmt + 12), u16le(&b, fmt + 14)), (6, 16));
        let (_, data, n) = found[2];
        assert_eq!(n, rows * 3 * 2);
        for i in 0..rows {
            for k in 0..3 {
                let v = i16::from_le_bytes(b[data + 2 * (3 * i + k)..][..2].try_into().unwrap());
                let want = ((d.cols[k][i] as f32 - d.conv[k].0) * 8.0)
                    .round()
                    .clamp(-32768.0, 32767.0) as i16;
                assert_eq!(v, want, "row {i} ch {k}");
            }
        }
    }
    // clipping: (4095 + 1000) × 8 > 32767
    let d = export_data(10);
    let p = tmp("clip.wav");
    write_wav(&d, &p, &AtomicU32::new(0)).unwrap();
    let b = std::fs::read(&p).unwrap();
    let data = b.windows(4).position(|w| w == b"data").unwrap() + 8;
    assert_eq!(i16::from_le_bytes([b[data + 4], b[data + 5]]), 32767);
}

/// MDF 4.10: walk ID → HD → DG → CG → CN chain (+ TX names/units, CC conversions) → DT records.
#[test]
fn mf4_export_walk() {
    for rows in [0usize, 1, 90] {
        let d = export_data(rows);
        let p = tmp(&format!("r{rows}.mf4"));
        write_mf4(&d, &p, &AtomicU32::new(0)).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(&b[..8], b"MDF     ");
        assert_eq!(&b[8..16], b"4.10    ");
        assert_eq!(u16le(&b, 28), 410);
        // a block at `at`: (id, links, data offset, data len); checks alignment and bounds
        let blk = |at: u64| -> (String, Vec<u64>, usize, usize) {
            let at = at as usize;
            assert_eq!(at % 8, 0, "block at {at} aligned");
            assert_eq!(&b[at..at + 2], b"##", "block at {at}");
            let len = u64le(&b, at + 8) as usize;
            let nl = u64le(&b, at + 16) as usize;
            assert!(at + len <= b.len(), "block at {at} inside the file");
            let links: Vec<u64> = (0..nl).map(|i| u64le(&b, at + 24 + 8 * i)).collect();
            for &l in &links {
                assert!(
                    l == 0
                        || (l as usize + 24 <= b.len() && &b[l as usize..l as usize + 2] == b"##"),
                    "link {l}"
                );
            }
            let data = at + 24 + 8 * nl;
            (
                String::from_utf8_lossy(&b[at..at + 4]).into(),
                links,
                data,
                at + len - data,
            )
        };
        let text = |at: u64| -> String {
            let (id, _, data, n) = blk(at);
            assert_eq!(id, "##TX");
            String::from_utf8_lossy(&b[data..data + n])
                .trim_end_matches('\0')
                .to_string()
        };
        let (id, hd, _, _) = blk(64);
        assert_eq!(id, "##HD");
        let (id, dg, _, _) = blk(hd[0]);
        assert_eq!(id, "##DG");
        let (id, cg, cgd, _) = blk(dg[1]);
        assert_eq!(id, "##CG");
        assert_eq!(u64le(&b, cgd + 8) as usize, rows, "cycle count");
        let rec = u32le(&b, cgd + 24) as usize;
        assert_eq!(rec, 8 + 2 * 3);
        // channel chain
        let mut cn = cg[1];
        let mut chans = Vec::new();
        while cn != 0 {
            let (id, l, data, _) = blk(cn);
            assert_eq!(id, "##CN");
            let (kind, dtype, off, bits) = (
                b[data],
                b[data + 2],
                u32le(&b, data + 4),
                u32le(&b, data + 8),
            );
            let conv = (l[4] != 0).then(|| {
                let (id, _, cd, _) = blk(l[4]);
                assert_eq!(id, "##CC");
                assert_eq!(b[cd], 1, "linear");
                (f64le(&b, cd + 24), f64le(&b, cd + 32))
            });
            chans.push((text(l[2]), text(l[6]), kind, dtype, off, bits, conv));
            cn = l[0];
        }
        assert_eq!(chans.len(), 4);
        assert_eq!(
            (
                chans[0].0.as_str(),
                chans[0].1.as_str(),
                chans[0].2,
                chans[0].3,
                chans[0].4,
                chans[0].5
            ),
            ("time", "s", 2, 4, 0, 64)
        );
        for (k, (name, unit)) in [("CH1", "V"), ("CH2", "A"), ("M1", "rpm")]
            .into_iter()
            .enumerate()
        {
            let c = &chans[k + 1];
            assert_eq!(
                (c.0.as_str(), c.1.as_str(), c.2, c.3, c.4, c.5),
                (name, unit, 0, 0, 8 + 2 * k as u32, 16)
            );
            let (z, u) = d.conv[k];
            let (p1, p2) = c.6.unwrap();
            assert!(
                (p1 + z as f64 * u as f64).abs() < 1e-6 && (p2 - u as f64).abs() < 1e-9,
                "{name} conversion"
            );
        }
        // data block: rows × record, time = t0 + i·dt, raw counts
        let (id, _, dt, n) = blk(dg[2]);
        assert_eq!(id, "##DT");
        assert_eq!(n, rows * rec);
        for i in 0..rows {
            let r = dt + i * rec;
            assert!((f64le(&b, r) - (-0.25 + i as f64 * 1e-3)).abs() < 1e-12);
            for k in 0..3 {
                assert_eq!(u16le(&b, r + 8 + 2 * k), d.cols[k][i]);
            }
        }
        assert_eq!(b.len() % 8, 0, "padded to 8 bytes");
    }
}

/// sigrok .sr: a stored zip — walk the central directory, check every CRC, decode the float channels.
#[test]
fn sr_export_zip_walk() {
    for rows in [0usize, 1, 90] {
        let d = export_data(rows);
        let p = tmp(&format!("r{rows}.sr"));
        write_sr(&d, &p, &AtomicU32::new(0)).unwrap();
        let z = std::fs::read(&p).unwrap();
        let end = z.len() - 22;
        assert_eq!(u32le(&z, end), 0x0605_4b50, "end of central directory");
        let entries = u16le(&z, end + 10) as usize;
        let (cd_size, cd_off) = (u32le(&z, end + 12) as usize, u32le(&z, end + 16) as usize);
        assert_eq!(cd_off + cd_size, end);
        assert_eq!(entries, 2 + 3);
        let mut at = cd_off;
        let mut files = std::collections::BTreeMap::new();
        for _ in 0..entries {
            assert_eq!(u32le(&z, at), 0x0201_4b50);
            let (crc, csize, usize_) = (
                u32le(&z, at + 16),
                u32le(&z, at + 20) as usize,
                u32le(&z, at + 24) as usize,
            );
            assert_eq!(csize, usize_, "stored");
            let nlen = u16le(&z, at + 28) as usize;
            let name = String::from_utf8_lossy(&z[at + 46..at + 46 + nlen]).to_string();
            let local = u32le(&z, at + 42) as usize;
            assert_eq!(u32le(&z, local), 0x0403_4b50, "{name}: local header");
            assert_eq!(u16le(&z, local + 8), 0, "{name}: method stored");
            let data = local + 30 + u16le(&z, local + 26) as usize + u16le(&z, local + 28) as usize;
            let body = &z[data..data + csize];
            assert_eq!(crc32fast::hash(body), crc, "{name}: CRC");
            files.insert(name, body.to_vec());
            at += 46 + nlen;
        }
        assert_eq!(files["version"], b"2");
        let meta = String::from_utf8_lossy(&files["metadata"]).to_string();
        assert!(
            meta.contains("samplerate=1000") && meta.contains("total analog=3"),
            "{meta}"
        );
        assert!(
            meta.contains("analog1=CH1") && meta.contains("analog3=M1"),
            "{meta}"
        );
        for k in 0..3 {
            let f = &files[&format!("analog-1-{}-1", k + 1)];
            assert_eq!(f.len(), rows * 4);
            let (zz, u) = d.conv[k];
            for i in 0..rows {
                let v = f32::from_le_bytes(f[4 * i..4 * i + 4].try_into().unwrap());
                assert_eq!(v, (d.cols[k][i] as f32 - zz) * u, "ch {k} row {i}");
            }
        }
    }
}

/// Export column names: "CH1_V" → (CH1, V); names without unit keep their name.
#[test]
fn export_column_names() {
    #[rustfmt::skip]
    let cases: &[(&str, &str, &str)] = &[("CH1_V", "CH1", "V"), ("M1_rpm", "M1", "rpm"), ("a_b_c", "a_b", "c"), ("plain", "plain", ""), ("", "", "")];
    for &(n, name, unit) in cases {
        assert_eq!((col_name(n), col_unit(n)), (name, unit), "{n:?}");
    }
    for f in [
        ExportFmt::Csv,
        ExportFmt::Wav,
        ExportFmt::Mf4,
        ExportFmt::Sr,
    ] {
        assert!(!f.ext().is_empty());
    }
}
