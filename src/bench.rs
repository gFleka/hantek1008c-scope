//! `hantek1008c bench [--timing fast|reference|lean|none] [--frames N] [--dump]`
//! Fixed scenarios on the real scope; prints a table and appends to perf/history.csv.

use crate::hantek::*;
use std::io::Write;
use std::time::{Duration, Instant};

struct Row {
    scenario: String,
    fps: f64,
    p50_ms: f64,
    p99_ms: f64,
    cmds_per_frame: f64,
    usb_ms_per_frame: f64,
    sleep_ms_per_frame: f64,
    samples_per_frame: f64,
    polls_per_frame: f64,
    zerr: f64,
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn measure(
    dev: &mut Hantek,
    name: String,
    frames: usize,
    mut one: impl FnMut(&mut Hantek, &mut Frame) -> Result<usize>,
) -> Result<Row> {
    let mut f = Frame::default();
    one(dev, &mut f)?; // warm-up (buffers, device state)
    let s0 = dev.stats;
    let mut lat = Vec::with_capacity(frames);
    let mut samples = 0usize;
    let mut zerr = 0f64;
    let t0 = Instant::now();
    for _ in 0..frames {
        let t = Instant::now();
        samples += one(dev, &mut f)?;
        lat.push(t.elapsed().as_secs_f64() * 1e3);
        zerr = zerr.max(zero_err(dev, &f));
    }
    let total = t0.elapsed().as_secs_f64();
    let s = dev.stats;
    let n = frames as f64;
    Ok(Row {
        scenario: name,
        fps: n / total,
        p50_ms: pct(&mut lat, 0.5),
        p99_ms: pct(&mut lat, 0.99),
        cmds_per_frame: (s.cmds - s0.cmds) as f64 / n,
        usb_ms_per_frame: (s.usb - s0.usb).as_secs_f64() * 1e3 / n,
        sleep_ms_per_frame: (s.slept - s0.slept).as_secs_f64() * 1e3 / n,
        samples_per_frame: samples as f64 / n,
        polls_per_frame: (s.ready_polls - s0.ready_polls) as f64 / n,
        zerr,
    })
}

/// Data integrity probe: max over channels of |mean − zero offset| (raw counts).
/// Only meaningful with nothing connected to the inputs; a shifted/stale packet shows as tens of counts.
fn zero_err(dev: &Hantek, f: &Frame) -> f64 {
    let Some(cfg) = dev.config() else { return 0.0 };
    let mut worst = 0f64;
    for ch in 0..CHANNELS {
        let d = &f.data[ch];
        if !f.active[ch] || d.len() < 16 {
            continue;
        }
        let mean = d.iter().map(|&v| v as f64).sum::<f64>() / d.len() as f64;
        worst = worst.max((mean - dev.zero[cfg.vscale[ch].index()][ch] as f64).abs());
    }
    worst
}

fn burst(dev: &mut Hantek, f: &mut Frame) -> Result<usize> {
    dev.capture(f)?;
    Ok(f.data.iter().map(Vec::len).sum())
}

pub fn run(args: &[String]) -> std::result::Result<(), String> {
    let mut timing_name = "fast".to_string();
    let mut frames = 50usize;
    let mut dump = false;
    let mut sets: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--timing" => timing_name = it.next().ok_or("--timing needs a value")?.clone(),
            "--frames" => {
                frames = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--frames needs a number")?
            }
            "--set" => sets.push(it.next().ok_or("--set needs name=value")?.clone()),
            "--dump" => dump = true,
            x => return Err(format!("unknown arg {x}")),
        }
    }
    let mut timing =
        Timing::by_name(&timing_name).ok_or("timing: fast | reference | lean | none")?;
    for kv in &sets {
        timing.set(kv)?;
        timing_name.push('+');
        timing_name.push_str(kv);
    }
    let e = |e: Error| e.to_string();

    let mut dev = Hantek::open(timing).map_err(e)?;
    let mut rows = Vec::new();

    // init
    let cfg0 = Config::default();
    let s0 = dev.stats;
    let t = Instant::now();
    dev.init(&cfg0).map_err(e)?;
    let init_ms = t.elapsed().as_secs_f64() * 1e3;
    rows.push(Row {
        scenario: "init".into(),
        fps: 1e3 / init_ms,
        p50_ms: init_ms,
        p99_ms: init_ms,
        cmds_per_frame: (dev.stats.cmds - s0.cmds) as f64,
        usb_ms_per_frame: (dev.stats.usb - s0.usb).as_secs_f64() * 1e3,
        sleep_ms_per_frame: (dev.stats.slept - s0.slept).as_secs_f64() * 1e3,
        samples_per_frame: 0.0,
        polls_per_frame: (dev.stats.ready_polls - s0.ready_polls) as f64,
        zerr: 0.0,
    });
    if dump {
        let b = &dev.blobs;
        for (n, v) in [
            ("b5", &b.b5),
            ("b6", &b.b6),
            ("e5", &b.e5),
            ("f7", &b.f7),
            ("f8", &b.f8),
            ("fa", &b.fa),
            ("e9", &b.e9),
        ] {
            println!(
                "{n}: {}",
                v.iter().map(|x| format!("{x:02x}")).collect::<String>()
            );
        }
        for (i, vs) in VScale::ALL.iter().enumerate() {
            println!("zero {vs:?}: {:?}", dev.zero[i]);
        }
    }

    // burst: channel count × timebase (fast / vendor default / slow)
    for chans in [8usize, 4, 1] {
        for ns in [1_000u64, 500_000, 10_000_000] {
            let mut cfg = Config::default();
            cfg.active = [false; CHANNELS];
            cfg.active[..chans].iter_mut().for_each(|a| *a = true);
            cfg.timebase_id = burst_id_for_ns(ns).unwrap();
            dev.configure(&cfg).map_err(e)?;
            let n = if ns >= 10_000_000 {
                frames.min(10)
            } else {
                frames
            };
            rows.push(measure(&mut dev, format!("burst_{chans}ch_{ns}ns"), n, burst).map_err(e)?);
        }
    }

    // config-change latency: toggle ch1 range, time until the next frame is in
    {
        let mut cfg = Config::default();
        let mut flip = false;
        rows.push(
            measure(&mut dev, "cfg_change".into(), frames.min(20), |d, f| {
                flip = !flip;
                cfg.vscale[0] = if flip { VScale::X0_125 } else { VScale::X1 };
                d.configure(&cfg)?;
                burst(d, f)
            })
            .map_err(e)?,
        );
    }

    // roll 440 S/s, 8 ch: one "frame" = one poll that returned data
    {
        dev.configure(&Config::default()).map_err(e)?;
        dev.roll_start(ROLL_RATES[0].1).map_err(e)?;
        rows.push(
            measure(&mut dev, "roll_440".into(), frames, |d, f| loop {
                f.clear();
                let n = d.roll_poll(f)?;
                if n > 0 {
                    return Ok(n);
                }
            })
            .map_err(e)?,
        );
    }
    dev.close();

    // report
    println!(
        "[{timing_name}]\n{:<24} {:>9} {:>9} {:>9} {:>8} {:>9} {:>9} {:>7} {:>7} {:>6}",
        "scenario",
        "frames/s",
        "p50 ms",
        "p99 ms",
        "cmds/f",
        "usb ms/f",
        "sleep/f",
        "polls/f",
        "samp/f",
        "zerr"
    );
    for r in &rows {
        println!(
            "{:<24} {:>9.2} {:>9.2} {:>9.2} {:>8.1} {:>9.2} {:>9.2} {:>7.1} {:>7.0} {:>6.1}",
            r.scenario,
            r.fps,
            r.p50_ms,
            r.p99_ms,
            r.cmds_per_frame,
            r.usb_ms_per_frame,
            r.sleep_ms_per_frame,
            r.polls_per_frame,
            r.samples_per_frame,
            r.zerr
        );
    }

    let git = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let path = "perf/history.csv";
    let new = !std::path::Path::new(path).exists();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    if new {
        writeln!(f, "unix_ts,git,os,timing,scenario,fps,p50_ms,p99_ms,cmds_per_frame,usb_ms_per_frame,sleep_ms_per_frame,samples_per_frame")
            .map_err(|e| e.to_string())?;
    }
    for r in &rows {
        writeln!(
            f,
            "{ts},{git},{},{timing_name},{},{:.3},{:.3},{:.3},{:.1},{:.3},{:.3},{:.0}",
            std::env::consts::OS,
            r.scenario,
            r.fps,
            r.p50_ms,
            r.p99_ms,
            r.cmds_per_frame,
            r.usb_ms_per_frame,
            r.sleep_ms_per_frame,
            r.samples_per_frame
        )
        .map_err(|e| e.to_string())?;
    }
    println!("appended {} rows to {path}", rows.len());
    Ok(())
}
