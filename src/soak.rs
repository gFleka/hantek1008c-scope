//! `hantek1008c soak [minutes] [dir]` — headless long run on the simulator (end-of-phase cycle): the real acquisition
//! thread, history, stream recording to disk and its writer, alternating captures and streaming every 5 min, with
//! a small memory limit so trimming runs all the time. Every 30 s: process memory, history, RAM-buffer backlog, lost
//! blocks, capture/stream rate. Ends with a verdict on memory growth and losses.

use crate::acq::{self, Cmd, Mode, Run};
use crate::hantek::{Config, ROLL_RATES};
use crate::rec::{RecLimits, RecMsg};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<f64>().ok())
        .map_or(0.0, |pages| pages * 4096.0 / 1048576.0)
}

pub fn run(args: &[String]) -> Result<(), String> {
    let minutes: f64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(30.0);
    let dir = args.get(1).map_or_else(
        || std::env::temp_dir().join("hantek-soak"),
        std::path::PathBuf::from,
    );
    let mut a = acq::spawn(true, Config::default(), || {});
    let budget = 128usize << 20;
    if let Ok(mut h) = a.history.lock() {
        h.set_budget(budget);
    }
    a.rec.send(RecMsg::Limits(RecLimits {
        dir: dir.clone(),
        auto: true,
        max_bytes: 2 << 30,
        keep_free: 1 << 30,
    }));
    let _ = a.tx.send(Cmd::Run(Run::Running));
    println!(
        "soak {minutes} min, simulator, memory limit 128 MB, recordings in {}",
        dir.display()
    );
    println!(
        "{:>6} {:>7} {:>8} {:>9} {:>8} {:>10} {:>9} {:>6} {:>7}",
        "min", "mode", "RSS MB", "hist MB", "capts", "stream", "spool kB", "lost", "rate/s"
    );
    let t0 = Instant::now();
    let (mut last_print, mut mode_at) = (Instant::now() - Duration::from_secs(30), Instant::now());
    let mut streaming = false;
    let (mut rss_first_half, mut rss_max, mut rows) = (0.0f64, 0.0f64, Vec::new());
    // memory the history (captures + stream recording) accounts for, at the end of the 1st half and at the end
    let (mut hist_half, mut hist_end) = (0.0f64, 0.0f64);
    // switch modes every quarter of the run (<= 5 min), so short runs cover captures and streaming
    let switch_s = (minutes * 60.0 / 4.0).clamp(10.0, 300.0);
    while t0.elapsed().as_secs_f64() < minutes * 60.0 {
        std::thread::sleep(Duration::from_millis(50));
        a.out.update(); // like the UI: read the latest snapshot
        if mode_at.elapsed().as_secs_f64() > switch_s {
            streaming = !streaming;
            mode_at = Instant::now();
            let _ = a.tx.send(Cmd::Mode(if streaming {
                Mode::Roll(ROLL_RATES[0].1)
            } else {
                Mode::Burst
            }));
            if streaming {
                let _ = a.tx.send(Cmd::Keep(4000));
            }
        }
        if last_print.elapsed() >= Duration::from_secs(30) {
            last_print = Instant::now();
            let s = a.out.output_buffer();
            let (hist, caps, srows) = a.history.lock().map_or((0, 0, 0), |h| {
                (
                    h.used_bytes(),
                    h.len(),
                    h.stream.dropped + h.stream.rows() as u64,
                )
            });
            let spool = a.rec.spool.bytes.load(Ordering::Relaxed);
            let lost = a.rec.shared.lost.load(Ordering::Relaxed);
            let rss = rss_mb();
            let m = t0.elapsed().as_secs_f64() / 60.0;
            rss_max = rss_max.max(rss);
            let hist_mb = hist as f64 / 1048576.0;
            if m <= minutes / 2.0 {
                rss_first_half = rss_first_half.max(rss);
                hist_half = hist_half.max(hist_mb);
            }
            hist_end = hist_mb;
            rows.push((m, rss, lost));
            println!(
                "{m:>6.1} {:>7} {rss:>8.1} {:>9.1} {caps:>8} {srows:>10} {:>9.1} {lost:>6} {:>7.1}",
                if streaming { "stream" } else { "capture" },
                hist as f64 / 1048576.0,
                spool as f64 / 1024.0,
                s.acq_fps
            );
        }
    }
    let _ = a.tx.send(Cmd::Run(Run::Stopped));
    a.rec.shutdown(Duration::from_secs(2));
    let lost = a.rec.shared.lost.load(Ordering::Relaxed);
    // a leak = memory growth the history does not account for (the history fills up to its limit by design)
    let growth = (rss_max - rss_first_half) - (hist_end - hist_half).max(0.0);
    println!("\nRSS max {rss_max:.1} MB, unexplained growth in the 2nd half {growth:+.1} MB (history {hist_half:.0} -> {hist_end:.0} MB), lost blocks {lost}");
    let ok = lost == 0 && growth < 20.0 && rss_max < budget as f64 / 1048576.0 + 200.0;
    println!(
        "{}",
        if ok {
            "PASS soak"
        } else {
            "FAIL soak (memory growth or lost data)"
        }
    );
    if ok {
        Ok(())
    } else {
        Err("soak failed".into())
    }
}
