//! `hantek1008c probe` — trigger experiments (reverse engineering on the device itself).
//! CH1 on the internal 1 kHz 2 Vpp generator. Each variant: fresh init, CH1 only, 500 µs/div,
//! range X1, then the variant's raw trigger commands, then 8 captures with capture mode `a4 <arm>`.
//! Reports: captures that completed, `a5` ready-reply histogram, first-edge spread (locked < 40 of 800).

use crate::hantek::*;
use std::time::Duration;

struct Variant {
    name: String,
    arm: u8,
    cmds: Vec<Vec<u8>>,
    ready_ping: bool,
}

fn v(name: &str, arm: u8, cmds: Vec<Vec<u8>>) -> Variant {
    Variant {
        name: name.into(),
        arm,
        cmds,
        ready_ping: false,
    }
}

pub fn run(args: &[String]) -> std::result::Result<(), String> {
    let e = |e: Error| e.to_string();
    let mut dev = match Hantek::open(Timing::fast()) {
        Err(Error::Usb(rusb::Error::Busy)) => {
            return Err("scope is in use by another program (close the GUI)".into())
        }
        x => x.map_err(e)?,
    };
    let mut cfg = Config::default();
    cfg.active = [false; CHANNELS];
    cfg.active[0] = true;
    cfg.timebase_id = burst_id_for_ns(500_000).unwrap();
    dev.init(&cfg).map_err(e)?;

    // CH1 signal is 0 V .. 2 V; on range X1 (10 mV/count) 1.0 V = zero + 100 counts
    let z = dev.zero[VScale::X1.index()][0].round() as u16;
    let mid = z + 100;
    let [mh, ml] = mid.to_be_bytes();
    println!("zero CH1 (X1) = {z} counts, mid-signal level = {mid} (0x{mid:04x})\n");

    if args.iter().any(|a| a == "--rates") {
        return rates(&mut dev, mid).map_err(e);
    }
    if let Some(i) = args.iter().position(|a| a == "--dump") {
        let n: usize = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(1);
        let us: u64 = args.get(i + 2).and_then(|v| v.parse().ok()).unwrap_or(1000);
        let pos: u16 = args.get(i + 3).and_then(|v| v.parse().ok()).unwrap_or(500);
        let mode = match args.get(i + 4).map(String::as_str) {
            Some("free") => TrigMode::Free,
            Some("normal") => TrigMode::Normal,
            _ => TrigMode::Auto,
        };
        return dump(&mut dev, mid, n, us, pos, mode).map_err(e);
    }
    if let Some(i) = args.iter().position(|a| a == "--window") {
        let n: usize = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(1);
        let us: u64 = args.get(i + 2).and_then(|v| v.parse().ok()).unwrap_or(1000);
        return window_scan(&mut dev, mid, n, us).map_err(e);
    }
    if let Some(i) = args.iter().position(|a| a == "--sanity") {
        let n: usize = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(300);
        return sanity(&mut dev, mid, n).map_err(e);
    }
    if args.iter().any(|a| a == "--jitter") {
        return jitter(&mut dev, mid).map_err(e);
    }
    if args.iter().any(|a| a == "--trigpos") {
        return trigpos(&mut dev, mid).map_err(e);
    }

    let mut vs = vec![
        // ── mode 00 (never got ready so far): is it a "wait for trigger" mode with wrong trigger settings? ──
        v(
            "a4 00 | c1 00 00, ab BE mid",
            0,
            vec![vec![0xc1, 0, 0], vec![0xab, mh, ml]],
        ),
        v(
            "a4 00 | c1 00 01 (falling), ab BE mid",
            0,
            vec![vec![0xc1, 0, 1], vec![0xab, mh, ml]],
        ),
        v(
            "a4 00 | ab LE mid",
            0,
            vec![vec![0xc1, 0, 0], vec![0xab, ml, mh]],
        ),
        v(
            "a4 00 | ab 8-bit (mid>>4) in byte 2",
            0,
            vec![vec![0xc1, 0, 0], vec![0xab, 0, (mid >> 4) as u8]],
        ),
        v(
            "a4 00 | ab 8-bit (mid>>4) in byte 1",
            0,
            vec![vec![0xc1, 0, 0], vec![0xab, (mid >> 4) as u8, 0]],
        ),
        v(
            "a4 00 | c1 01 00 (1-based src)",
            0,
            vec![vec![0xc1, 1, 0], vec![0xab, mh, ml]],
        ),
        v(
            "a4 00 | ab 0x0000",
            0,
            vec![vec![0xc1, 0, 0], vec![0xab, 0x00, 0x00]],
        ),
        v(
            "a4 00 | ab 0x0800",
            0,
            vec![vec![0xc1, 0, 0], vec![0xab, 0x08, 0x00]],
        ),
        v(
            "a4 00 | ab 0x0fff",
            0,
            vec![vec![0xc1, 0, 0], vec![0xab, 0x0f, 0xff]],
        ),
        Variant {
            name: "a4 00 | BE mid + f3 pings while waiting".into(),
            arm: 0,
            cmds: vec![vec![0xc1, 0, 0], vec![0xab, mh, ml]],
            ready_ping: true,
        },
        // ── mode 01 (always completes, edge free-running): does any encoding make it hold the edge? ──
        v(
            "a4 01 | BE mid (as now)",
            1,
            vec![vec![0xc1, 0, 0], vec![0xab, mh, ml]],
        ),
        v(
            "a4 01 | ab LE mid",
            1,
            vec![vec![0xc1, 0, 0], vec![0xab, ml, mh]],
        ),
        v(
            "a4 01 | c1 01 00 (1-based src)",
            1,
            vec![vec![0xc1, 1, 0], vec![0xab, mh, ml]],
        ),
        v(
            "a4 01 | ab 8-bit in byte 2",
            1,
            vec![vec![0xc1, 0, 0], vec![0xab, 0, (mid >> 4) as u8]],
        ),
    ];
    if args.iter().any(|a| a == "--a7") {
        // a7 xx yy: sent once at init as `a7 00 00` (reply 00), meaning unknown — trigger mode/enable?
        let base = vec![vec![0xc1, 0, 0], vec![0xab, mh, ml]];
        vs = [
            [0x00u8, 0x01],
            [0x01, 0x00],
            [0x01, 0x01],
            [0x02, 0x00],
            [0x00, 0x02],
        ]
        .iter()
        .map(|p| {
            let mut c = vec![vec![0xa7, p[0], p[1]]];
            c.extend(base.clone());
            v(
                &format!("a4 01 | a7 {:02x} {:02x}, BE mid", p[0], p[1]),
                1,
                c,
            )
        })
        .collect();
    }
    if args.iter().any(|a| a == "--quick") {
        vs.truncate(3);
    }
    if args.iter().any(|a| a == "--mode1") {
        vs.retain(|v| v.arm == 1);
    }

    println!(
        "{:<44} {:>5} {:>22} {:>18}",
        "variant", "done", "a5 replies 0/1/2/3/x", "edge spread"
    );
    let mut f = Frame::default();
    for var in vs {
        if dev.init(&cfg).is_err() {
            // wedged: USB reset + reopen, then init again
            dev.close();
            std::thread::sleep(Duration::from_millis(1500));
            dev = match Hantek::open(Timing::fast()) {
                Ok(d) => d,
                Err(x) => {
                    println!("{:<44} reopen failed after reset: {x}, stopping", var.name);
                    return Ok(());
                }
            };
            if dev.init(&cfg).is_err() {
                println!(
                    "{:<44} re-init failed even after USB reset, stopping",
                    var.name
                );
                break;
            }
        }
        let mut setup_err = None;
        for c in &var.cmds {
            if let Err(err) = dev.raw(c, 0, true) {
                setup_err = Some(err.to_string());
                break;
            }
        }
        if let Some(err) = setup_err {
            println!("{:<44} setup error: {err}", var.name);
            continue;
        }
        dev.arm = var.arm;
        dev.timing.ready_ping = var.ready_ping;
        dev.ready_deadline = Duration::from_millis(400);
        dev.ready_hist = [0; 5];
        let (mut done, mut pos, mut err) = (0, Vec::new(), None);
        for _ in 0..8 {
            match dev.capture(&mut f) {
                Ok(_) => {
                    done += 1;
                    pos.extend(first_edge(&f.data[0], true).map(|i| i as f64));
                }
                Err(x) => {
                    err = Some(x.to_string());
                    break;
                }
            }
        }
        dev.arm = 1;
        dev.timing.ready_ping = false;
        dev.ready_deadline = Duration::from_secs(2);
        let h = dev.ready_hist;
        let spread = if pos.len() >= 2 {
            let (lo, hi) = pos
                .iter()
                .fold((f64::MAX, f64::MIN), |(a, b), &p| (a.min(p), b.max(p)));
            format!(
                "{:.0}{}",
                hi - lo,
                if hi - lo < 40.0 { " LOCKED" } else { "" }
            )
        } else {
            "-".into()
        };
        println!(
            "{:<44} {:>3}/8 {:>22} {:>18}{}",
            var.name,
            done,
            format!("{}/{}/{}/{}/{}", h[0], h[1], h[2], h[3], h[4]),
            spread,
            err.map_or(String::new(), |x| format!("   ({x})"))
        );
    }
    dev.close();
    Ok(())
}

fn cfg_n(n: usize, tb: u8, level: u16) -> Config {
    let mut c = Config::default();
    c.active = [false; CHANNELS];
    c.active[..n].iter_mut().for_each(|a| *a = true);
    c.timebase_id = tb;
    c.trig_level = level;
    c
}

/// Real sample rate per (active channel count, timebase), triggered (Auto), 8 captures each.
/// ratio = measured / nominal (nominal = samples per channel over 10 div). Stable+exact = usable.
fn rates(dev: &mut Hantek, level: u16) -> Result<()> {
    let mut f = Frame::default();
    println!("rate ratio measured/nominal (CH1 = 1 kHz): median [min..max] over 8 captures; ok = within 1 %");
    print!("{:>12}", "time/div");
    let ns = [1usize, 2, 4, 8];
    for n in ns {
        print!("{:>26}", format!("{n} ch"));
    }
    println!();
    for tb in 0x0fu8..=0x17 {
        print!("{:>12}", format!("{} µs", burst_ns_per_div(tb) / 1000));
        for n in ns {
            let cfg = cfg_n(n, tb, level);
            dev.configure(&cfg)?;
            let mut r = Vec::new();
            for _ in 0..8 {
                if dev.capture(&mut f)? {
                    let nominal = f.data[0].len() as f64 / (burst_ns_per_div(tb) as f64 * 1e-8);
                    let z = dev.zero[cfg.vscale[0].index()][0];
                    if let Some(cps) =
                        measure(&f.data[0], z, volts_per_count(cfg.vscale[0]), Some(1.0)).freq
                    {
                        r.push(1000.0 / cps / nominal);
                    }
                }
            }
            if r.is_empty() {
                print!("{:>26}", "--");
                continue;
            }
            r.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let (med, lo, hi) = (r[r.len() / 2], r[0], r[r.len() - 1]);
            let ok = (lo - 1.0).abs() < 0.01 && (hi - 1.0).abs() < 0.01;
            print!(
                "{:>26}",
                format!("{med:.3} [{lo:.3}..{hi:.3}]{}", if ok { " ok" } else { "" })
            );
        }
        println!();
    }
    Ok(())
}

/// Where does the trigger point really land for a given trigger position? (8 ch and 1 ch, 500 µs/div)
fn trigpos(dev: &mut Hantek, level: u16) -> Result<()> {
    let mut f = Frame::default();
    println!("trigger position: expected sample = pos/1000 × samples; found = nearest rising edge (10 captures, median)");
    for n in [8usize, 4, 2, 1] {
        for pos in [100u16, 300, 500, 700, 900] {
            let mut cfg = cfg_n(n, burst_id_for_ns(500_000).unwrap(), level);
            cfg.trig_pos = pos;
            dev.configure(&cfg)?;
            let mut offs = Vec::new();
            let mut len = 0;
            for _ in 0..10 {
                if !dev.capture(&mut f)? {
                    continue;
                }
                let d = &f.data[0];
                len = d.len();
                let expect = pos as f64 / 1000.0 * len as f64;
                let mut best = f64::MAX;
                let mut start = 0;
                while start < d.len() {
                    match first_edge(&d[start..], true) {
                        Some(i) => {
                            let at = (start + i) as f64;
                            if (at - expect).abs() < best.abs() {
                                best = at - expect;
                            }
                            start += i + 1;
                        }
                        None => break,
                    }
                }
                if best != f64::MAX {
                    offs.push(best);
                }
            }
            offs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            if offs.is_empty() {
                println!("  {n} ch pos {pos:>4}: no edges");
                continue;
            }
            let med = offs[offs.len() / 2];
            println!(
                "  {n} ch pos {pos:>4}: edge offset {med:+6.0} samples ({:+.2} % of screen, {:+.1} µs), spread {:.0}",
                med / len as f64 * 100.0,
                med / len as f64 * 5000.0,
                offs[offs.len() - 1] - offs[0]
            );
        }
    }
    Ok(())
}

/// `probe --dump <n_ch> <µs/div>`: one triggered capture, CH1 shown as runs of high/low
/// (threshold = mid level) so garbled or interleaved data is visible at a glance.
fn dump(
    dev: &mut Hantek,
    level: u16,
    n: usize,
    us_div: u64,
    pos: u16,
    mode: TrigMode,
) -> Result<()> {
    let tb = burst_id_for_ns(us_div * 1000)
        .ok_or_else(|| Error::Protocol(format!("no timebase {us_div} µs/div")))?;
    let mut cfg = cfg_n(n, tb, level);
    cfg.trig_pos = pos;
    cfg.trig_mode = mode;
    println!("{n} ch, {us_div} µs/div, trigger pos {pos}, mode {mode:?}");
    if let Ok(us) = std::env::var("ARM_DELAY_US") {
        dev.timing.burst_a4 = Duration::from_micros(us.parse().unwrap_or(0));
        println!("  a4 -> c0 delay {us} µs");
    }
    if let Ok(s) = std::env::var("PRE_SCALE") {
        // experiment: pre-trigger time × s, post-trigger unchanged
        let s: u64 = s.parse().unwrap_or(1);
        let w = window_us(tb) as u64;
        let pre = ((w - 2) * pos as u64 / 1000 + 1) * s;
        let post = w - ((w - 2) * pos as u64 / 1000 + 1);
        let (a, b, c) = (
            pos.to_be_bytes(),
            (pre as u32).to_be_bytes(),
            (post as u32).to_be_bytes(),
        );
        dev.raw(
            &[0xac, a[0], a[1], b[1], b[2], b[3], c[1], c[2], c[3]],
            0,
            true,
        )?;
        println!("  ac pre {pre} µs (×{s}), post {post} µs");
    }
    dev.configure(&cfg)?;
    let mut f = Frame::default();
    for i in 0..3 {
        if !dev.capture(&mut f)? {
            println!("capture {i}: no trigger");
            continue;
        }
        let d = &f.data[0];
        let runs: Vec<String> = {
            let mut v = Vec::new();
            let mut cur = d[0] > level;
            let mut len = 0;
            for &s in d {
                let h = s > level;
                if h == cur {
                    len += 1;
                } else {
                    v.push(format!("{}{len}", if cur { 'H' } else { 'L' }));
                    cur = h;
                    len = 1;
                }
            }
            v.push(format!("{}{len}", if cur { 'H' } else { 'L' }));
            v
        };
        let (lo, hi) = d
            .iter()
            .fold((u16::MAX, 0), |(a, b), &s| (a.min(s), b.max(s)));
        println!(
            "capture {i}: {} samples, min {lo} max {hi}, {} runs:",
            d.len(),
            runs.len()
        );
        println!(
            "  {}",
            runs.iter().take(60).cloned().collect::<Vec<_>>().join(" ")
        );
        println!("  first 24 raw: {:?}", &d[..24.min(d.len())]);
        // outliers: far from both signal levels (0 V ≈ level-100, 2 V ≈ level+100 on range X1)
        let out: Vec<(usize, u16)> = d
            .iter()
            .copied()
            .enumerate()
            .filter(|&(_, s)| s < level - 160 || s > level + 160)
            .collect();
        println!(
            "  outliers (< {} or > {}): {} → first {:?}",
            level - 160,
            level + 160,
            out.len(),
            &out[..out.len().min(12)]
        );
    }
    Ok(())
}

/// Index just past the last high/low transition (= how far the capture holds real signal).
fn valid_len(d: &[u16], level: u16) -> usize {
    let mut last = 0;
    for i in 1..d.len() {
        if (d[i] > level) != (d[i - 1] > level) {
            last = i;
        }
    }
    last
}

/// `probe --window <n_ch> <µs/div>`: does scaling the `ac` window fill the whole capture?
/// For each scale, `ac` = (pos 500, pre and post × scale); reports valid samples of 4000/n.
fn window_scan(dev: &mut Hantek, level: u16, n: usize, us_div: u64) -> Result<()> {
    let tb = burst_id_for_ns(us_div * 1000)
        .ok_or_else(|| Error::Protocol(format!("no timebase {us_div} µs/div")))?;
    let cfg = cfg_n(n, tb, level);
    dev.configure(&cfg)?;
    let mut f = Frame::default();
    let w = window_us(tb);
    println!("{n} ch, {us_div} µs/div, nominal window {w} µs; valid = samples up to the last edge (+ ≤ half a period)");
    for scale in [1u32, 2, 4, 8] {
        let msg = ac_payload(500, w * scale);
        dev.raw(&msg, 0, true)?;
        let mut v = Vec::new();
        let mut hp = Vec::new();
        for _ in 0..5 {
            if dev.capture(&mut f)? {
                let d = &f.data[0];
                v.push(valid_len(d, level));
                // half period in samples from the first full run
                let mut runs = Vec::new();
                let mut len = 0;
                for i in 1..d.len() {
                    len += 1;
                    if (d[i] > level) != (d[i - 1] > level) {
                        runs.push(len);
                        len = 0;
                    }
                }
                if runs.len() > 2 {
                    hp.push(runs[1]);
                }
            }
        }
        println!(
            "  ac window ×{scale}: valid {v:?} of {} samples, half-period {hp:?} samples",
            f.data[0].len()
        );
    }
    dev.raw(&ac_payload(500, w), 0, true)?;
    Ok(())
}

/// `probe --sanity [N]`: N 8-channel captures per timebase; counts impossible samples (> 4095) and spike
/// samples (a sample differing > 400 counts from both neighbours, i.e. a one-sample glitch) per capture.
fn sanity(dev: &mut Hantek, level: u16, n: usize) -> Result<()> {
    let mut f = Frame::default();
    println!("data sanity, 8 channels, {n} captures per timebase (CH1 = 1 kHz generator)");
    for us in [100u64, 200, 500, 1000, 2000, 5000] {
        let tb = burst_id_for_ns(us * 1000).unwrap();
        let mut cfg = cfg_n(8, tb, level);
        cfg.trig_mode = TrigMode::Auto;
        dev.configure(&cfg)?;
        let (mut bad_frames, mut over, mut spikes, mut short, mut errors) =
            (0, 0usize, 0usize, 0, 0);
        let mut first_bad: Option<(usize, String)> = None;
        for k in 0..n {
            match dev.capture(&mut f) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => {
                    errors += 1;
                    continue;
                }
            }
            let mut o = 0;
            let mut s = 0;
            for ch in 0..CHANNELS {
                let d = &f.data[ch];
                if d.len() != 500 {
                    short += 1;
                }
                o += d.iter().filter(|&&v| v > 4095).count();
                s += d
                    .windows(3)
                    .filter(|w| {
                        (w[1] as i32 - w[0] as i32).abs() > 400
                            && (w[1] as i32 - w[2] as i32).abs() > 400
                    })
                    .count();
            }
            for ch in 0..CHANNELS {
                for (i, w) in f.data[ch].windows(3).enumerate() {
                    if (w[1] as i32 - w[0] as i32).abs() > 400
                        && (w[1] as i32 - w[2] as i32).abs() > 400
                    {
                        println!("    spike: {us} µs/div capture #{k} CH{} sample {} value {} (neighbours {} {})", ch + 1, i + 1, w[1], w[0], w[2]);
                    }
                }
            }
            if o > 0 || s > 0 {
                bad_frames += 1;
                if first_bad.is_none() {
                    let d = &f.data[0];
                    let worst = d.iter().copied().max().unwrap_or(0);
                    first_bad = Some((k, format!("CH1 max {worst}, over-range {o}, spikes {s}")));
                }
            }
            over += o;
            spikes += s;
        }
        println!(
            "  {:>5} µs/div: bad captures {bad_frames}/{n}, samples > 4095: {over}, one-sample spikes: {spikes}, wrong length: {short}, errors: {errors}{}",
            us,
            first_bad.map_or(String::new(), |(k, t)| format!("  (first bad #{k}: {t})"))
        );
    }
    Ok(())
}

/// `probe --jitter`: does the waveform sit at the same place in every capture? 8 channels (as the GUI),
/// CH1 on the 1 kHz generator, trigger CH1 rising at mid level, position 50 %, Normal mode, 200 captures per
/// timebase. For each capture the rising crossing of the trigger level nearest to where the screen puts the
/// trigger point (pos × samples), interpolated between samples. Reports the spread in samples / µs / screen
/// pixels (1000 px plot), the whole-sample histogram and the capture-to-capture pattern.
fn jitter(dev: &mut Hantek, level: u16) -> Result<()> {
    let mut f = Frame::default();
    println!(
        "offset = trigger-level crossing − pos×samples (samples). px = at a 1000 px wide plot.\n"
    );
    for us in [10u64, 20, 50, 100, 500, 1000, 5000] {
        let Some(tb) = burst_id_for_ns(us * 1000) else {
            continue;
        };
        let mut cfg = cfg_n(8, tb, level);
        cfg.trig_pos = 500;
        cfg.trig_mode = TrigMode::Normal;
        dev.configure(&cfg)?;
        let (mut offs, mut n, mut misses) = (Vec::new(), 0usize, 0);
        // independent check: the next falling edge after the trigger, uncorrected vs with the GUI's correction
        let (mut raw, mut fixed, mut applied) = (Vec::new(), Vec::new(), 0);
        for _ in 0..200 {
            let forced0 = dev.stats.forced;
            if !dev.capture(&mut f)? || dev.stats.forced != forced0 {
                misses += 1;
                continue;
            }
            let d = &f.data[0];
            n = d.len();
            let expect = 0.5 * n as f64;
            let lv = level as f64;
            let mut best: Option<f64> = None;
            for i in 1..d.len() {
                let (a, b) = (d[i - 1] as f64, d[i] as f64);
                if a < lv && b >= lv {
                    let x = (i - 1) as f64 + (lv - a) / (b - a);
                    if best.is_none_or(|bb| (x - expect).abs() < (bb - expect).abs()) {
                        best = Some(x);
                    }
                }
            }
            match best {
                Some(x) => offs.push(x - expect),
                None => misses += 1,
            }
            let corr = trigger_offset(d, expect, level, true, 12);
            applied += corr.is_some() as usize;
            let c = corr.unwrap_or(0.0) as f64;
            let start = ((expect + c).ceil() as usize + 1).min(d.len());
            if let Some(i) =
                (start.max(1)..d.len()).find(|&i| (d[i - 1] as f64) > lv && (d[i] as f64) <= lv)
            {
                let (a, b) = (d[i - 1] as f64, d[i] as f64);
                let fx = (i - 1) as f64 + (a - lv) / (a - b);
                raw.push(fx - expect);
                fixed.push(fx - expect - c);
            }
        }
        if offs.len() < 10 {
            println!(
                "{us:>5} µs/div: only {} captures with an edge ({misses} missed)",
                offs.len()
            );
            continue;
        }
        let dt_us = capture_seconds(tb) / n as f64 * 1e6;
        let px = 1000.0 * dt_us / (10.0 * us as f64); // px per sample on a 1000 px plot showing 10 div
        let m = offs.iter().sum::<f64>() / offs.len() as f64;
        let sd = (offs.iter().map(|o| (o - m).powi(2)).sum::<f64>() / offs.len() as f64).sqrt();
        let (lo, hi) = offs
            .iter()
            .fold((f64::MAX, f64::MIN), |(l, h), &o| (l.min(o), h.max(o)));
        let mut hist = std::collections::BTreeMap::new();
        for o in &offs {
            *hist.entry(o.round() as i64).or_insert(0) += 1;
        }
        let lag1 = {
            let v: f64 = offs
                .windows(2)
                .map(|w| (w[0] - m) * (w[1] - m))
                .sum::<f64>()
                / (offs.len() - 1) as f64;
            v / (sd * sd).max(1e-12)
        };
        println!(
            "{us:>5} µs/div: {n} samples/ch, {dt_us:.2} µs/sample, {} captures ({misses} missed/forced)\n        offset mean {m:+.2}  sd {sd:.2}  range {lo:+.2} … {hi:+.2} samples = {:.1} µs p-p = {:.1} px p-p; lag-1 corr {lag1:+.2}",
            offs.len(),
            (hi - lo) * dt_us,
            (hi - lo) * px
        );
        println!(
            "        whole samples: {}",
            hist.iter()
                .map(|(k, c)| format!("{k:+}:{c}"))
                .collect::<Vec<_>>()
                .join("  ")
        );
        println!(
            "        first 24: {}",
            offs.iter()
                .take(24)
                .map(|o| format!("{o:+.1}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let pp = |v: &[f64]| {
            v.iter()
                .fold((f64::MAX, f64::MIN), |(l, h), &o| (l.min(o), h.max(o)))
        };
        let sd = |v: &[f64]| {
            let m = v.iter().sum::<f64>() / v.len().max(1) as f64;
            (v.iter().map(|o| (o - m).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt()
        };
        let ((rl, rh), (fl, fh)) = (pp(&raw), pp(&fixed));
        println!(
            "        next falling edge ({} captures, correction found in {applied}): uncorrected p-p {:.2} samples = {:.1} px (sd {:.2}) → corrected p-p {:.2} samples = {:.1} px (sd {:.2})",
            raw.len(),
            rh - rl,
            (rh - rl) * px,
            sd(&raw),
            fh - fl,
            (fh - fl) * px,
            sd(&fixed)
        );
    }
    Ok(())
}
