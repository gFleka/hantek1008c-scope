//! Layer 2: the real acquisition thread (`acq::spawn(true, …)`, simulator) driven through mode changes,
//! timebases, channel counts and Run/Stop/Single; asserts snapshots, history, stream recording and the
//! `.h8r` files it writes. Each scenario takes ≈ 1–3 s; they run in parallel. Long runs are `#[ignore]`.

use super::util::{tmp_dir, wait_until};
use crate::acq::*;
use crate::hantek::*;
use crate::rec::{RecLimits, RecMsg};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn cfg_n(n: usize, tb: u8) -> Config {
    Config {
        active: std::array::from_fn(|c| c < n),
        timebase_id: tb,
        ..Config::default()
    }
}

/// Acquisition on the simulator; recordings (if any) go to `dir`, never to the default folder.
fn start(cfg: &Config, dir: &Path, auto: bool) -> Acq {
    let acq = spawn(true, cfg.clone(), || {});
    acq.rec.send(RecMsg::Limits(RecLimits {
        dir: dir.to_path_buf(),
        auto,
        max_bytes: 1 << 34,
        keep_free: 0,
    }));
    acq.tx.send(Cmd::Config(cfg.clone())).unwrap();
    acq
}

/// Newest snapshot (waits for one with `ok`).
fn snap_where(acq: &mut Acq, secs: f32, ok: impl Fn(&Snapshot) -> bool) -> Snapshot {
    let t0 = Instant::now();
    loop {
        acq.out.update();
        let s = acq.out.output_buffer().clone();
        if ok(&s) {
            return s;
        }
        assert!(
            t0.elapsed().as_secs_f32() < secs,
            "no matching snapshot in {secs} s (last: seq {} roll {} cfg {:?})",
            s.frame.seq,
            s.roll,
            s.cfg.as_ref().map(|c| c.timebase_id)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn caps(acq: &Acq) -> usize {
    acq.history.lock().unwrap().len()
}

/// Burst mode, every channel count 1…8 and a spread of timebases (incl. the skipped 200 µs and 50 ms ones,
/// which the acq thread must still handle if asked): samples per channel, active set, config echo, history.
#[test]
fn burst_channel_counts_and_timebases() {
    let dir = tmp_dir("sim-burst");
    #[rustfmt::skip]
    let cases: &[(usize, u8)] = &[(1, 17), (2, 0), (3, 9), (4, 15), (5, 16), (6, 18), (7, 23), (8, 25)];
    let mut acq = start(&cfg_n(8, 17), &dir, false);
    for &(n, tb) in cases {
        let c = cfg_n(n, tb);
        acq.tx.send(Cmd::Config(c.clone())).unwrap();
        let s = snap_where(&mut acq, 3.0, |s| s.cfg.as_ref() == Some(&c));
        // the frame after the config echo is captured with it
        let s2 = snap_where(&mut acq, 3.0, |x| x.frame.seq > s.frame.seq);
        assert!(!s2.roll && !s2.stopped, "{n} ch tb {tb}");
        assert_eq!(s2.frame.active, c.active, "{n} ch tb {tb}");
        for ch in 0..CHANNELS {
            let want = if ch < n { 4000 / n } else { 0 };
            assert_eq!(s2.frame.data[ch].len(), want, "{n} ch tb {tb} ch {ch}");
            assert!(s2.frame.data[ch].iter().all(|&v| v <= 4095));
        }
        assert_eq!(s2.zero, [[2048.0; CHANNELS]; 3]);
        assert_eq!(s2.status, "simulator");
    }
    // every capture went into the history, numbered without gaps
    let h = acq.history.lock().unwrap();
    assert!(h.len() >= cases.len() * 2);
    let seqs: Vec<u64> = h.items.iter().map(|e| e.snap.frame.seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "no capture lost: {seqs:?}"
    );
}

/// Capture rate ≈ the simulated 26 ms per frame (≈ 38/s), reported in the snapshot.
#[test]
fn burst_rate_and_stats() {
    let dir = tmp_dir("sim-rate");
    let mut acq = start(&cfg_n(8, 17), &dir, false);
    std::thread::sleep(Duration::from_millis(300));
    let n0 = caps(&acq);
    std::thread::sleep(Duration::from_millis(1500));
    let rate = (caps(&acq) - n0) as f32 / 1.5;
    assert!((20.0..=40.0).contains(&rate), "captures/s {rate}");
    let s = snap_where(&mut acq, 2.0, |s| s.acq_fps > 0.0);
    assert!((15.0..=45.0).contains(&s.acq_fps), "acq_fps {}", s.acq_fps);
    // simulator: free-running, no trigger jitter offset found by the default Auto mode? It is computed:
    // the sim is untriggered, so the offset may or may not exist — but it is never NaN
    assert!(s.trig_off.is_none_or(|o| o.is_finite()));
}

/// Run / Stop / Single state machine as seen in the history and snapshots.
#[test]
fn run_stop_single() {
    let dir = tmp_dir("sim-run");
    let mut acq = start(&cfg_n(2, 17), &dir, false);
    assert!(wait_until(3.0, || caps(&acq) >= 3));
    // Stop: no new captures, snapshot says stopped (the last published one before the stop may not)
    acq.tx.send(Cmd::Run(Run::Stopped)).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let n = caps(&acq);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(caps(&acq), n, "stopped: no captures");
    // Single: exactly one more, then stopped by itself
    acq.tx.send(Cmd::Run(Run::Single)).unwrap();
    let s = snap_where(&mut acq, 2.0, |s| s.stopped && caps_seq(s) > 0);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(caps(&acq), n + 1, "single = one capture");
    assert!(s.stopped);
    // Single again while stopped: one more
    acq.tx.send(Cmd::Run(Run::Single)).unwrap();
    assert!(wait_until(2.0, || caps(&acq) == n + 2));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(caps(&acq), n + 2);
    // Run resumes
    acq.tx.send(Cmd::Run(Run::Running)).unwrap();
    assert!(wait_until(2.0, || caps(&acq) >= n + 6));
    let s = snap_where(&mut acq, 2.0, |s| !s.stopped);
    assert!(!s.stopped);
}

fn caps_seq(s: &Snapshot) -> u64 {
    s.frame.seq
}

/// Stream (roll) mode for several channel counts and rates: rows/s match the real scope's
/// nominal × channel factor, memory = disk, no lost blocks, inactive channels empty.
#[test]
fn stream_rates_channels_and_recording() {
    #[rustfmt::skip]
    let cases: &[(usize, usize)] = &[
        // channels, ROLL_RATES index
        (1, 0), (2, 0), (3, 1), (8, 0), (4, 2),
    ];
    std::thread::scope(|sc| {
        for &(n, ri) in cases {
            sc.spawn(move || stream_case(n, ri));
        }
    });
}

fn stream_case(n: usize, ri: usize) {
    let dir = tmp_dir(&format!("sim-stream-{n}-{ri}"));
    let c = cfg_n(n, 26);
    let mut acq = start(&c, &dir, true);
    let id = ROLL_RATES[ri].1;
    acq.tx.send(Cmd::Mode(Mode::Roll(id))).unwrap();
    acq.tx.send(Cmd::Keep(1000)).unwrap();
    let s = snap_where(&mut acq, 3.0, |s| s.roll);
    let t0 = Instant::now();
    let r0 = acq.history.lock().unwrap().stream.rows();
    std::thread::sleep(Duration::from_millis(2500)); // > 2 s: the first chunk is written (see writer_short_stream_is_written)
    let dt = t0.elapsed().as_secs_f64();
    let rows = acq.history.lock().unwrap().stream.rows() - r0;
    let want = ROLL_RATES[ri].0 * roll_rate_factor(n);
    let got = rows as f64 / dt;
    let tag = format!("{n} ch, {} S/s", ROLL_RATES[ri].0);
    assert!(
        (got - want).abs() <= want * 0.12 + 3.0,
        "{tag}: {got:.0} rows/s, want {want:.0}"
    );
    {
        let h = acq.history.lock().unwrap();
        assert!((h.stream.rate - want).abs() < 1e-6, "{tag}: stream rate");
        assert_eq!(h.stream.active, c.active, "{tag}");
        assert_eq!(h.len(), 0, "{tag}: streams don't add captures");
        for ch in n..CHANNELS {
            assert!(h.stream.data[ch].is_empty(), "{tag}");
        }
    }
    // the published frame keeps about one screen (Keep), trimmed when 25 % over
    let s2 = snap_where(&mut acq, 2.0, |x| x.roll);
    assert!(
        s.roll && s2.frame.data[0].len() <= 1250 + 64,
        "{tag}: kept {}",
        s2.frame.data[0].len()
    );
    // end the stream: the file holds exactly the in-memory recording
    acq.tx.send(Cmd::Mode(Mode::Burst)).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let mem = acq.history.lock().unwrap().stream.data.clone();
    let files = wait_files(&dir, 1);
    assert!(
        wait_until(5.0, || crate::rec::read(&files[0], None)
            .is_ok_and(|r| r.data[0].len() == mem[0].len())),
        "{tag}: file complete"
    );
    let r = crate::rec::read(&files[0], None).unwrap();
    assert!(!r.truncated, "{tag}");
    for ch in 0..CHANNELS {
        assert_eq!(r.data[ch], mem[ch], "{tag}: ch {ch} disk = memory");
    }
    assert_eq!(r.header.active, c.active);
    assert!((r.header.rate - want).abs() < 1e-6);
    assert_eq!(r.header.roll_id, id);
    assert_eq!(acq.rec.shared.lost.load(Ordering::Relaxed), 0, "{tag}");
}

/// Wait until `n` closed recordings exist (status shows no open file).
fn wait_files(dir: &Path, n: usize) -> Vec<std::path::PathBuf> {
    assert!(
        wait_until(5.0, || crate::rec::list(dir).len() >= n
            && crate::rec::list(dir)
                .iter()
                .all(
                    |(p, _)| crate::rec::read(p, None).is_ok_and(|r| !r.truncated)
                )),
        "{n} recordings in {}",
        dir.display()
    );
    std::thread::sleep(Duration::from_millis(300)); // writer closes after the last rows
    crate::rec::list(dir).into_iter().map(|x| x.0).collect()
}

/// Mode / config changes in stream mode each start a new stream (new file, new in-memory recording);
/// Burst in between adds captures; Stop in stream mode keeps the recording browsable.
#[test]
fn stream_restarts_on_mode_and_config_changes() {
    let dir = tmp_dir("sim-restart");
    let c2 = cfg_n(2, 26);
    let mut acq = start(&c2, &dir, true);
    let roll = Mode::Roll(ROLL_RATES[0].1);
    // each stream lasts > 2 s: shorter ones never reach the disk (writer_short_stream_is_written, BUG)
    let disk = Duration::from_millis(2300);
    acq.tx.send(Cmd::Mode(roll)).unwrap();
    assert!(wait_until(3.0, || acq
        .history
        .lock()
        .unwrap()
        .stream
        .rows()
        > 300));
    std::thread::sleep(disk);
    // config change (3 channels) → new stream with 3 channels
    let c3 = cfg_n(3, 26);
    acq.tx.send(Cmd::Config(c3.clone())).unwrap();
    assert!(wait_until(3.0, || {
        let h = acq.history.lock().unwrap();
        h.stream.active == c3.active && h.stream.rows() > 300
    }));
    std::thread::sleep(disk);
    // stop: rows stop growing, recording stays
    acq.tx.send(Cmd::Run(Run::Stopped)).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let rows = acq.history.lock().unwrap().stream.rows();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        acq.history.lock().unwrap().stream.rows(),
        rows,
        "stopped stream doesn't grow"
    );
    // run on → burst → captures, the stream recording is kept until the next stream starts
    acq.tx.send(Cmd::Run(Run::Running)).unwrap();
    acq.tx.send(Cmd::Mode(Mode::Burst)).unwrap();
    assert!(wait_until(3.0, || caps(&acq) >= 3));
    let s = snap_where(&mut acq, 2.0, |s| !s.roll);
    assert!(!s.roll);
    // back to stream: replaced at its first rows
    acq.tx.send(Cmd::Mode(roll)).unwrap();
    assert!(wait_until(3.0, || {
        let h = acq.history.lock().unwrap();
        h.stream.rows() > 50 && h.stream.rows() < rows
    }));
    std::thread::sleep(disk);
    acq.tx.send(Cmd::Mode(Mode::Burst)).unwrap();
    let files = wait_files(&dir, 3);
    assert_eq!(files.len(), 3, "2 ch, 3 ch, again 3 ch: {files:?}");
    let chans: Vec<usize> = files
        .iter()
        .map(|p| {
            crate::rec::read(p, None)
                .unwrap()
                .header
                .active
                .iter()
                .filter(|&&a| a)
                .count()
        })
        .collect();
    assert_eq!(chans, vec![2, 3, 3]);
    assert_eq!(acq.rec.shared.lost.load(Ordering::Relaxed), 0);
}

/// The history memory limit holds while captures stream in.
#[test]
fn history_limit_holds_under_load() {
    let dir = tmp_dir("sim-limit");
    let acq = start(&cfg_n(8, 17), &dir, false);
    assert!(wait_until(3.0, || caps(&acq) >= 2));
    let one = {
        let h = acq.history.lock().unwrap();
        h.used_bytes() / h.len().max(1)
    };
    acq.history.lock().unwrap().set_budget(one * 5 + one / 2);
    std::thread::sleep(Duration::from_millis(800));
    let h = acq.history.lock().unwrap();
    assert!(h.len() <= 5 && h.len() >= 4, "{} captures kept", h.len());
    assert!(h.used_bytes() <= h.budget);
    assert!(h.first_no > 0, "old captures dropped, numbers stay stable");
    assert!(h.get(h.last_no().unwrap()).is_some());
}

/// UI gone (all senders dropped) → the acq thread ends instead of spinning.
#[test]
fn acq_thread_ends_when_the_ui_is_gone() {
    let dir = tmp_dir("sim-end");
    let acq = start(&cfg_n(1, 17), &dir, false);
    let h = acq.history.clone();
    assert!(wait_until(3.0, || h.lock().unwrap().len() >= 1));
    drop(acq);
    std::thread::sleep(Duration::from_millis(200));
    let n = h.lock().unwrap().len();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        h.lock().unwrap().len(),
        n,
        "no captures after the UI went away"
    );
}

/// Long stream run: 60 s at 8 channels (≈ 26 600 rows/ch) — rate stable, file = memory, no loss.
#[test]
#[ignore = "long: run with scripts/regress.sh --long"]
fn long_stream_60s() {
    let dir = tmp_dir("sim-long");
    let c = cfg_n(8, 26);
    let acq = start(&c, &dir, true);
    acq.tx.send(Cmd::Mode(Mode::Roll(ROLL_RATES[0].1))).unwrap();
    std::thread::sleep(Duration::from_secs(60));
    acq.tx.send(Cmd::Mode(Mode::Burst)).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let mem = acq.history.lock().unwrap().stream.data.clone();
    let files = wait_files(&dir, 1);
    assert!(wait_until(10.0, || crate::rec::read(&files[0], None)
        .is_ok_and(|r| r.data[0].len() == mem[0].len())));
    let r = crate::rec::read(&files[0], None).unwrap();
    assert_eq!(r.data, mem);
    let rate = r.data[0].len() as f64 / 60.0;
    assert!((rate - 440.0).abs() < 440.0 * 0.05, "rows/s {rate}");
    assert_eq!(acq.rec.shared.lost.load(Ordering::Relaxed), 0);
}

/// Long capture run: 30 s of mode/timebase/channel churn every 300 ms; never stalls, history numbering intact.
#[test]
#[ignore = "long: run with scripts/regress.sh --long"]
fn long_churn_30s() {
    let dir = tmp_dir("sim-churn");
    let mut acq = start(&cfg_n(8, 17), &dir, true);
    let t0 = Instant::now();
    let mut k = 0usize;
    while t0.elapsed() < Duration::from_secs(30) {
        k += 1;
        let c = cfg_n(1 + k % 8, [0u8, 9, 15, 17, 18, 22, 24, 25][k % 8]);
        acq.tx.send(Cmd::Config(c)).unwrap();
        let m = if k % 5 == 0 {
            Mode::Roll(ROLL_RATES[k % 3].1)
        } else {
            Mode::Burst
        };
        acq.tx.send(Cmd::Mode(m)).unwrap();
        if k % 7 == 0 {
            acq.tx.send(Cmd::Run(Run::Single)).unwrap();
        } else {
            acq.tx.send(Cmd::Run(Run::Running)).unwrap();
        }
        let seq = {
            acq.out.update();
            acq.out.output_buffer().frame.seq
        };
        snap_where(&mut acq, 3.0, |s| s.frame.seq != seq || s.stopped || s.roll);
        std::thread::sleep(Duration::from_millis(300));
    }
    acq.tx.send(Cmd::Mode(Mode::Burst)).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    for (p, _) in crate::rec::list(&dir) {
        let r = crate::rec::read(&p, None).unwrap();
        assert!(!r.truncated, "{}", p.display());
    }
    let h = acq.history.lock().unwrap();
    let seqs: Vec<u64> = h.items.iter().map(|e| e.snap.frame.seq).collect();
    assert!(seqs.windows(2).all(|w| w[1] > w[0]));
    assert_eq!(acq.rec.shared.lost.load(Ordering::Relaxed), 0);
}
