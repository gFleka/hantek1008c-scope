//! `hantek1008c check` — hardware self-test without the GUI.
//! Expects CH1 on the 1008C's internal 1 kHz / 2 Vpp generator. Prints PASS / FAIL / INFO lines;
//! exit code 1 if anything failed.

use crate::hantek::*;
use std::time::{Duration, Instant};

const GEN_HZ: f64 = 1000.0;
const GEN_VPP: f32 = 2.0;

#[derive(Default)]
struct Report {
    fails: u32,
}

impl Report {
    fn check(&mut self, ok: bool, name: &str, detail: String) {
        if !ok {
            self.fails += 1;
        }
        println!("{} {name:<26} {detail}", if ok { "PASS " } else { "FAIL " });
    }
    fn info(&self, name: &str, detail: String) {
        println!("INFO  {name:<26} {detail}");
    }
}

fn cfg_with(n_ch: usize, ns_per_div: u64) -> Config {
    let mut c = Config::default();
    c.active = [false; CHANNELS];
    c.active[..n_ch].iter_mut().for_each(|a| *a = true);
    c.timebase_id = burst_id_for_ns(ns_per_div).expect("valid timebase");
    c
}

/// Seconds per sample on CH1 for the last capture (samples span 10 div).
fn dt(cfg: &Config, f: &Frame) -> f64 {
    // real capture length (2.4 MS/s cap at fast timebases, measured corrections at 5–20 ms/div)
    capture_seconds(cfg.timebase_id) / f.data[0].len().max(1) as f64
}

fn grab(dev: &mut Hantek, cfg: &Config, f: &mut Frame) -> Result<bool> {
    dev.configure(cfg)?;
    dev.capture(f)
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

pub fn run(_args: &[String]) -> std::result::Result<(), String> {
    println!("Hantek 1008C hardware check — CH1 must be on the internal 1 kHz 2 Vpp generator; close the GUI first.\n");
    let e = |e: Error| e.to_string();
    let mut r = Report::default();
    let mut f = Frame::default();

    // ── start-up ──
    let t = Instant::now();
    let mut dev = match Hantek::open(Timing::fast()) {
        Err(Error::Usb(rusb::Error::Busy)) => {
            return Err("scope is in use by another program (close the GUI)".into())
        }
        x => x.map_err(e)?,
    };
    dev.init(&Config::default()).map_err(e)?;
    r.check(
        true,
        "open + init",
        format!("{:.0} ms", t.elapsed().as_secs_f64() * 1e3),
    );
    let zs: Vec<f32> = dev.zero.iter().flatten().copied().collect();
    let (zmin, zmax) = zs
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &z| (a.min(z), b.max(z)));
    r.check(
        zmin > 1700.0 && zmax < 2300.0,
        "zero offsets",
        format!("{zmin:.0} .. {zmax:.0} counts (3 ranges × 8 ch)"),
    );

    // ── samples per channel ──
    for n in [1usize, 2, 4, 8] {
        let cfg = cfg_with(n, 500_000);
        grab(&mut dev, &cfg, &mut f).map_err(e)?;
        let lens: Vec<usize> = f.data[..n].iter().map(Vec::len).collect();
        let total: usize = lens.iter().sum();
        let equal = lens.iter().all(|&l| l == lens[0]);
        r.check(
            equal && lens[0] > 0,
            &format!("samples, {n} ch"),
            format!("{} per channel, {total} total", lens[0]),
        );
    }

    // ── time axis: frequency of the 1 kHz generator at several timebases ──
    // the app captures all 8 channels (review #3): check the time axis there at every verified timebase;
    // 1-channel pairs are the known firmware fault path → reported as INFO, not pass/fail
    // (≤ 100 µs/div holds < 2 periods of 1 kHz → not measurable here; its 2.4 MS/s rate is verified by the rate table)
    for (n, ns) in [
        (8, 500_000u64),
        (8, 1_000_000),
        (8, 2_000_000),
        (8, 5_000_000),
        (8, 10_000_000),
        (8, 20_000_000),
        (1, 500_000),
        (1, 1_000_000),
        (1, 2_000_000),
    ] {
        let cfg = cfg_with(n, ns);
        let mut fs = Vec::new();
        for _ in 0..3 {
            grab(&mut dev, &cfg, &mut f).map_err(e)?;
            let z = dev.zero[cfg.vscale[0].index()][0];
            if let Some(hz) = measure(
                &f.data[0],
                z,
                volts_per_count(cfg.vscale[0]),
                Some(dt(&cfg, &f)),
            )
            .freq
            {
                fs.push(hz);
            }
        }
        let name = format!("time axis {n} ch {}", fmt_ns(ns));
        let result = if fs.is_empty() {
            (false, "no frequency found".to_string())
        } else {
            let hz = median(&mut fs);
            (
                (hz / GEN_HZ - 1.0).abs() < 0.01,
                format!("{hz:.2} Hz (expect {GEN_HZ:.0} ±1 %)"),
            )
        };
        if n == CHANNELS {
            r.check(result.0, &name, result.1);
        } else {
            r.info(
                &name,
                format!(
                    "{} — <8-channel firmware path, not used by the app",
                    result.1
                ),
            );
        }
    }

    // ── real sample rate per timebase (measured from the 1 kHz period) ──
    println!();
    println!("      sample rate per channel: measured / nominal (nominal = samples / 10 div), 1 kHz generator; -- = < 2 periods");
    for n in [1usize, 8] {
        for id in 9..=21u8 {
            let ns = burst_ns_per_div(id);
            let cfg = cfg_with(n, ns);
            dev.configure(&cfg).map_err(e)?;
            let z = dev.zero[cfg.vscale[0].index()][0];
            // 5 captures right after the timebase change: shows whether early frames are stale
            let mut cols = Vec::new();
            let mut nominal = 0.0;
            for _ in 0..5 {
                dev.capture(&mut f).map_err(e)?;
                nominal = f.data[0].len() as f64 / (ns as f64 * 1e-9 * 10.0);
                // dt = 1 sample → frequency in cycles per sample → samples/s
                let real = measure(&f.data[0], z, volts_per_count(cfg.vscale[0]), Some(1.0))
                    .freq
                    .map(|cps| GEN_HZ / cps);
                cols.push(real.map_or("      --".to_string(), |sr| {
                    format!("{:>8.3}", sr / nominal)
                }));
            }
            r.info(
                &format!("rate {n} ch {}", fmt_ns(ns)),
                format!(
                    "nominal {nominal:>10.0} S/s  measured/nominal per capture: {}",
                    cols.join(" ")
                ),
            );
        }
    }
    println!();

    // ── amplitude per hardware range ──
    for vs in [VScale::X1, VScale::X0_125] {
        let mut cfg = cfg_with(1, 500_000);
        cfg.vscale[0] = vs;
        let mut amps = Vec::new();
        for _ in 0..5 {
            grab(&mut dev, &cfg, &mut f).map_err(e)?;
            let m = measure(
                &f.data[0],
                dev.zero[vs.index()][0],
                Cal::load().vpc(0, vs),
                None,
            );
            amps.push((m.top - m.base) as f64);
        }
        let amp = median(&mut amps) as f32;
        r.check(
            (amp / GEN_VPP - 1.0).abs() < 0.05,
            &format!("amplitude range {vs:?}"),
            format!(
                "{amp:.4} V (expect {GEN_VPP} ±5 %{}) → remaining gain error ×{:.4}",
                if Cal::load() == Cal::default() {
                    ", uncalibrated"
                } else {
                    ", calibrated"
                },
                GEN_VPP / amp
            ),
        );
    }
    r.info(
        "amplitude range X0_02",
        "skipped: ±0.4 V range, the 2 Vpp generator clips".into(),
    );

    // ── trigger: does the first edge stay put? ──
    // 1 ch @ 500 µs/div → ~800 samples per period; random positions spread over the whole period.
    println!();
    println!(
        "      trigger: first-edge sample index over 20 captures (1 ch, 500 µs/div, range X1)"
    );
    let mut any_locked = false;
    for slope in [Slope::Rising, Slope::Falling] {
        for lvl_v in [0.5f32, 1.0, 1.5] {
            let mut cfg = cfg_with(1, 500_000);
            cfg.trig_slope = slope;
            let z = dev.zero[cfg.vscale[0].index()][0];
            cfg.trig_level = (z + lvl_v / volts_per_count(cfg.vscale[0])).round() as u16;
            let mut pos = Vec::new();
            let mut per = 0.0;
            for _ in 0..20 {
                grab(&mut dev, &cfg, &mut f).map_err(e)?;
                per = 1.0 / (GEN_HZ * dt(&cfg, &f));
                if let Some(i) = first_edge(&f.data[0], slope == Slope::Rising) {
                    pos.push(i as f64);
                }
            }
            if pos.is_empty() {
                r.info("trigger", format!("{slope:?} {lvl_v} V: no edges found"));
                continue;
            }
            let (lo, hi) = pos
                .iter()
                .fold((f64::MAX, f64::MIN), |(a, b), &p| (a.min(p), b.max(p)));
            let spread = hi - lo;
            let locked = spread < 0.05 * per;
            any_locked |= locked;
            r.info(
                &format!("trigger {slope:?} {lvl_v:.1} V"),
                format!(
                    "edge at {lo:.0}..{hi:.0} (spread {spread:.0} of {per:.0} samples/period) raw level {} → {}",
                    cfg.trig_level,
                    if locked { "LOCKED" } else { "free-running" }
                ),
            );
        }
    }
    r.check(
        any_locked,
        "trigger holds edge position",
        if any_locked {
            "at least one setting locks".into()
        } else {
            "no setting locks the edge — trigger not working".into()
        },
    );
    println!();

    // ── hardware range switch time ──
    {
        let mut cfg = cfg_with(1, 500_000);
        dev.configure(&cfg).map_err(e)?;
        let mut ts = Vec::new();
        for i in 0..4 {
            cfg.vscale[0] = if i % 2 == 0 {
                VScale::X0_125
            } else {
                VScale::X1
            };
            let t = Instant::now();
            dev.configure(&cfg).map_err(e)?;
            ts.push(t.elapsed().as_secs_f64() * 1e3);
        }
        r.info(
            "range switch time",
            format!("{:.0} ms median", median(&mut ts)),
        );
    }

    // ── robustness: 200 captures, 8 ch ──
    {
        let cfg = cfg_with(8, 500_000);
        dev.configure(&cfg).map_err(e)?;
        let t = Instant::now();
        let mut errs = 0;
        for _ in 0..200 {
            if dev.capture(&mut f).is_err() {
                errs += 1;
            }
        }
        r.check(
            errs == 0,
            "200 captures, 8 ch",
            format!(
                "{errs} errors, {:.1} captures/s",
                200.0 / t.elapsed().as_secs_f64()
            ),
        );
    }

    // ── roll mode 440 S/s ──
    {
        dev.configure(&cfg_with(1, 500_000)).map_err(e)?;
        dev.roll_start(ROLL_RATES[0].1).map_err(e)?;
        let t = Instant::now();
        let mut rows = 0;
        while t.elapsed() < Duration::from_secs(2) {
            f.clear();
            rows += dev.roll_poll(&mut f).map_err(e)?;
        }
        let rate = rows as f64 / t.elapsed().as_secs_f64();
        // hantek1008py: the real roll rate rises when fewer channels are active (×4.56 at 1 ch)
        let expect = 440.0 * roll_rate_factor(1);
        r.check(
            (rate / expect - 1.0).abs() < 0.1,
            "roll mode 440 S/s, 1 ch",
            format!("{rate:.0} rows/s (expect {expect:.0})"),
        );
    }

    if _args.iter().any(|a| a == "--trigger-explore") {
        trigger_explore(&mut dev, &mut f, &r);
    }

    dev.close();
    println!("\n{} failed", r.fails);
    if r.fails > 0 {
        Err(format!("{} check(s) failed", r.fails))
    } else {
        Ok(())
    }
}

/// Opt-in experiment: start burst captures with other `a4` parameters and see whether any of
/// them makes the edge position hold (= a triggered mode). Unknown values may be rejected;
/// on error the scope is re-initialised.
fn trigger_explore(dev: &mut Hantek, f: &mut Frame, r: &Report) {
    println!();
    println!("      trigger explore: a4 <arm>, rising 1.0 V, 10 captures each (1 ch, 500 µs/div)");
    for arm in [0u8, 3] {
        // fresh init so each variant starts from a known state
        let mut cfg = cfg_with(1, 500_000);
        if dev.init(&cfg).is_err() {
            r.info("trigger explore", "re-init failed, stopping".into());
            return;
        }
        let z = dev.zero[cfg.vscale[0].index()][0];
        cfg.trig_level = (z + 1.0 / volts_per_count(cfg.vscale[0])).round() as u16;
        if dev.configure(&cfg).is_err() {
            continue;
        }
        dev.arm = arm;
        let mut pos = Vec::new();
        let mut err = None;
        let t = Instant::now();
        for _ in 0..10 {
            match dev.capture(f) {
                Ok(_) => pos.extend(first_edge(&f.data[0], true).map(|i| i as f64)),
                Err(e) => {
                    err = Some(e.to_string());
                    break;
                }
            }
        }
        dev.arm = 1;
        let ms = t.elapsed().as_secs_f64() * 1e3 / 10.0;
        let detail = match (err, pos.is_empty()) {
            (Some(e), _) => format!("error: {e}"),
            (None, true) => "captures OK but no edges".into(),
            (None, false) => {
                let (lo, hi) = pos
                    .iter()
                    .fold((f64::MAX, f64::MIN), |(a, b), &p| (a.min(p), b.max(p)));
                format!("edge at {lo:.0}..{hi:.0} (spread {:.0} of 800), {} samples, {ms:.0} ms/capture", hi - lo, f.data[0].len())
            }
        };
        r.info(&format!("a4 {arm:02x}"), detail);
    }
}

fn fmt_ns(ns: u64) -> String {
    match ns {
        n if n >= 1_000_000 => format!("{} ms/div", n / 1_000_000),
        n if n >= 1_000 => format!("{} µs/div", n / 1_000),
        n => format!("{n} ns/div"),
    }
}
