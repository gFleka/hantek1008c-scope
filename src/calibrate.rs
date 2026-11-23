//! `hantek1008c calibrate --ch <1-8> --volts <multimeter reading>`
//! Connect a steady DC source (battery) to the channel, measure it with a multimeter, pass the reading.
//! Every hardware range that can hold the voltage is measured (mean of 20 captures, 8 channels each)
//! and its gain correction stored in calibration.txt. Run it once per reference voltage:
//! ~12–14 V (car battery) → range X1; ~1.5 V (AA cell) → X0_125 too; ≤ 0.35 V (divider) → X0_02 too.

use crate::hantek::*;

pub fn run(args: &[String]) -> std::result::Result<(), String> {
    let mut ch = None;
    let mut volts = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--ch" => {
                ch = it
                    .next()
                    .and_then(|v| v.parse::<usize>().ok())
                    .filter(|c| (1..=CHANNELS).contains(c))
            }
            "--volts" => {
                volts = it
                    .next()
                    .and_then(|v| v.replace(',', ".").parse::<f32>().ok())
            }
            x => return Err(format!("unknown arg {x}")),
        }
    }
    let (Some(ch), Some(volts)) = (ch, volts) else {
        return Err(
            "usage: hantek1008c calibrate --ch <1-8> --volts <multimeter reading, e.g. 12.62>"
                .into(),
        );
    };
    if volts.abs() < 0.05 {
        return Err("use a reference of at least 0.05 V".into());
    }
    let c = ch - 1;
    let e = |e: Error| e.to_string();
    let mut dev = match Hantek::open(Timing::fast()) {
        Err(Error::Usb(rusb::Error::Busy)) => {
            return Err("scope is in use by another program (close the GUI)".into())
        }
        x => x.map_err(e)?,
    };
    let mut cfg = Config::default();
    cfg.trig_mode = TrigMode::Free; // DC: nothing to trigger on
    dev.init(&cfg).map_err(e)?;

    let mut cal = Cal::load();
    let mut f = Frame::default();
    let mut done = 0;
    println!("CH{ch}: reference {volts} V (multimeter)");
    for vs in VScale::ALL {
        if range_half_span(vs) < volts.abs() * 1.05 {
            println!(
                "  range {vs:?} (±{:.2} V): too small for {volts} V, skipped",
                range_half_span(vs)
            );
            continue;
        }
        cfg.vscale = [vs; CHANNELS];
        dev.configure(&cfg).map_err(e)?;
        let (mut sum, mut n) = (0f64, 0usize);
        for _ in 0..20 {
            dev.capture(&mut f).map_err(e)?;
            sum += f.data[c].iter().map(|&v| v as f64).sum::<f64>();
            n += f.data[c].len();
        }
        let mean = sum / n as f64;
        let counts = mean - dev.zero[vs.index()][c] as f64;
        if counts.abs() < 20.0 {
            println!("  range {vs:?}: only {counts:.1} counts above zero — is the reference connected to CH{ch}?");
            continue;
        }
        let measured = counts as f32 * volts_per_count(vs);
        let gain = volts / measured;
        if !(0.8..1.25).contains(&gain) {
            println!("  range {vs:?}: reads {measured:.4} V → gain {gain:.4} is implausible, not stored (check wiring / reading)");
            continue;
        }
        cal.gain[c][vs.index()] = gain;
        done += 1;
        println!("  range {vs:?}: uncalibrated {measured:.4} V ({counts:.1} counts) → gain {gain:.5} ({:+.2} %)", (gain - 1.0) * 100.0);
    }
    dev.close();
    if done == 0 {
        return Err("nothing calibrated".into());
    }
    let path = cal.save().map_err(|e| e.to_string())?;
    println!("saved {done} range(s) to {}", path.display());
    Ok(())
}

/// Usable half-span in volts of a hardware range (±~1900 counts around zero).
fn range_half_span(vs: VScale) -> f32 {
    1900.0 * volts_per_count(vs)
}
