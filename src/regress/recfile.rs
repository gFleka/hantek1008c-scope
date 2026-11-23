//! rec.rs: the `.h8r` format (header codec, chunks, reader windows, damage at every byte) and the writer
//! thread (auto on/off, Start/Stop, Meta, folder limit, keep-free stop, spool limit, shutdown).

use super::util::{tmp_dir, wait_until};
use crate::hantek::CHANNELS;
use crate::rec::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn header(active: [bool; CHANNELS]) -> RecHeader {
    RecHeader {
        started_unix: 1_763_700_000.25,
        rate: 1333.2,
        roll_id: 0x18,
        active,
        vscale: [3, 2, 1, 3, 3, 3, 3, 3],
        zero: [[2048.5; CHANNELS], [2040.0; CHANNELS], [2100.25; CHANNELS]],
        gain: [[1.01, 0.99, 1.0]; CHANNELS],
        probes: "0:1:0:V,3:100:0:A".into(),
        capture: None,
    }
}

const TWO: [bool; CHANNELS] = [true, false, true, false, false, false, false, false];

/// Value of channel slot `c` at absolute row `r` (makes every sample checkable).
fn val(c: usize, r: u64) -> u16 {
    ((r * 7 + c as u64 * 1000) % 4096) as u16
}

/// File with `chunks` chunks of `rows` rows for the active channels of `h`. Returns (bytes, chunk offsets).
fn build(h: &RecHeader, chunks: usize, rows: usize) -> (Vec<u8>, Vec<u64>) {
    let nch = h.active.iter().filter(|&&a| a).count();
    let mut b = Vec::new();
    let mut offs = Vec::new();
    let mut pos = write_header(&mut b, h).unwrap();
    for k in 0..chunks {
        let first = (k * rows) as u64;
        let cols: Vec<Vec<u16>> = (0..nch)
            .map(|c| (0..rows as u64).map(|i| val(c, first + i)).collect())
            .collect();
        let parts: Vec<&[u16]> = cols.iter().map(|c| c.as_slice()).collect();
        offs.push(pos);
        pos += write_chunk(&mut b, first, &parts).unwrap();
    }
    assert_eq!(pos as usize, b.len());
    (b, offs)
}

fn save(dir: &Path, name: &str, b: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, b).unwrap();
    p
}

/// Rows of the loaded data are exactly the expected absolute rows of every active channel.
fn check_data(r: &Recording, from: u64, n: u64) {
    let act: Vec<usize> = (0..CHANNELS).filter(|&c| r.header.active[c]).collect();
    for (slot, &ch) in act.iter().enumerate() {
        assert_eq!(r.data[ch].len() as u64, n, "ch {ch} rows");
        for (i, &v) in r.data[ch].iter().enumerate() {
            assert_eq!(
                v,
                val(slot, from + i as u64),
                "ch {ch} row {}",
                from + i as u64
            );
        }
    }
    for c in 0..CHANNELS {
        if !r.header.active[c] {
            assert!(r.data[c].is_empty());
        }
    }
}

// ───────────────────────────── header codec ─────────────────────────────

#[test]
fn header_roundtrip_variants() {
    let mut caps = header(TWO);
    caps.capture = Some((17, 250, -3.5));
    let mut nl = header([true; CHANNELS]);
    nl.probes = "a\nb".into(); // newline would break the line format → replaced by a space
    let mut none = header([false; CHANNELS]);
    none.probes.clear();
    for (name, h) in [
        ("2 ch", header(TWO)),
        ("capture", caps),
        ("probes newline", nl),
        ("no channel", none),
    ] {
        let d = RecHeader::decode(&h.encode());
        let mut want = h.clone();
        want.probes = want.probes.replace('\n', " ");
        // started_unix is written with ms precision
        assert!((d.started_unix - want.started_unix).abs() < 1e-3, "{name}");
        want.started_unix = d.started_unix;
        assert_eq!(d, want, "{name}");
    }
}

#[test]
fn header_decode_garbage() {
    #[rustfmt::skip]
    let cases: &[(&str, &str, Box<dyn Fn(&RecHeader) -> bool>)] = &[
        ("empty → defaults, gain 1",    "",                         Box::new(|h| h.gain == [[1.0; 3]; CHANNELS] && h.rate == 0.0 && h.capture.is_none())),
        ("unknown keys ignored",        "foo=1\nbar\n=\n",          Box::new(|h| h.active == [false; CHANNELS])),
        ("bad numbers → 0",             "rate=x\nroll_id=999\nstarted_unix=y", Box::new(|h| h.rate == 0.0 && h.roll_id == 0 && h.started_unix == 0.0)),
        ("bad vscale → 3 (X1)",         "vscale=a,2",               Box::new(|h| h.vscale[0] == 3 && h.vscale[1] == 2 && h.vscale[2] == 0)),
        ("too many channels ignored",   "active=1,1,1,1,1,1,1,1,1,1,1", Box::new(|h| h.active == [true; CHANNELS])),
        ("short active list",           "active=0,1",               Box::new(|h| h.active[1] && !h.active[0] && !h.active[2])),
        ("zero row with junk",          "zero1=1,x,3",              Box::new(|h| h.zero[1][0] == 1.0 && h.zero[1][1] == 3.0)),
        ("gain partial",                "gain=2,3",                 Box::new(|h| h.gain[0][0] == 2.0 && h.gain[0][1] == 3.0 && h.gain[0][2] == 1.0)),
        ("capture 2 fields → none",     "capture=1,2",              Box::new(|h| h.capture.is_none())),
        ("capture 4 fields → none",     "capture=1,2,3,4",          Box::new(|h| h.capture.is_none())),
        ("capture junk → defaults",     "capture=a,b,c",            Box::new(|h| h.capture == Some((0, 500, 0.0)))),
        ("value with '='",              "probes=a=b",               Box::new(|h| h.probes == "a=b")),
        ("CRLF lines",                  "rate=5\r\nactive=1\r\n",   Box::new(|h| h.active[0])),
    ];
    for (name, txt, ok) in cases {
        let h = RecHeader::decode(txt);
        assert!(ok(&h), "{name}: {h:?}");
    }
}

/// A header written on Windows (or edited) with CRLF line ends: `rate=5\r` must still parse as 5.
#[test]
fn header_decode_crlf_numbers() {
    let h = RecHeader::decode("rate=5\r\nroll_id=24\r\n");
    assert_eq!((h.rate, h.roll_id), (5.0, 24));
}

// ───────────────────────────── reader ─────────────────────────────

#[test]
fn read_full_and_index() {
    let dir = tmp_dir("rec-full");
    #[rustfmt::skip]
    let cases: &[(&str, [bool; CHANNELS], usize, usize)] = &[
        ("2 ch, 5 × 100",      TWO, 5, 100),
        ("8 ch, 3 × 4096",     [true; CHANNELS], 3, 4096),
        ("1 ch, 1 × 1",        [false, false, false, false, false, false, false, true], 1, 1),
        ("header only",        TWO, 0, 10),
        ("2 ch, empty chunks", TWO, 3, 0),
    ];
    for (name, act, chunks, rows) in cases {
        let h = header(*act);
        let (b, offs) = build(&h, *chunks, *rows);
        let p = save(&dir, "f.h8r", &b);
        let r = read(&p, None).unwrap();
        assert!(!r.truncated, "{name}");
        assert_eq!(r.header, RecHeader::decode(&h.encode()), "{name}");
        assert_eq!(r.chunks.len(), *chunks, "{name}");
        assert_eq!(r.total_rows, (*chunks * *rows) as u64, "{name}");
        assert_eq!(r.first_row, 0, "{name}");
        check_data(&r, 0, r.total_rows);
        for (k, c) in r.chunks.iter().enumerate() {
            assert_eq!(
                (c.offset, c.first_row, c.rows),
                (offs[k], (k * rows) as u64, *rows as u32),
                "{name}"
            );
            let nch = act.iter().filter(|&&a| a).count();
            for s in 0..nch {
                let vals: Vec<u16> = (0..*rows as u64).map(|i| val(s, c.first_row + i)).collect();
                assert_eq!(
                    c.min[s],
                    vals.iter().copied().min().unwrap_or(0),
                    "{name} min"
                );
                assert_eq!(
                    c.max[s],
                    vals.iter().copied().max().unwrap_or(0),
                    "{name} max"
                );
            }
        }
    }
}

/// Cut the file at every byte: never a panic; a cut inside the header is an error, otherwise all complete
/// chunks are read and `truncated` says whether the cut fell inside a chunk.
#[test]
fn read_cut_at_every_byte() {
    let dir = tmp_dir("rec-cut");
    let h = header(TWO);
    let (b, offs) = build(&h, 3, 13);
    let hdr_end = offs[0] as usize;
    let mut ends: Vec<usize> = offs.iter().skip(1).map(|&o| o as usize).collect();
    ends.push(b.len());
    for cut in 0..=b.len() {
        let p = save(&dir, "cut.h8r", &b[..cut]);
        let r = read(&p, None);
        if cut < hdr_end {
            assert!(r.is_err(), "cut {cut} in the header must fail");
            continue;
        }
        let r = r.unwrap_or_else(|e| panic!("cut {cut}: {e}"));
        let complete = ends.iter().filter(|&&e| e <= cut).count();
        let on_boundary = cut == hdr_end || ends.contains(&cut);
        assert_eq!(r.chunks.len(), complete, "cut {cut}");
        assert_eq!(r.truncated, !on_boundary, "cut {cut}");
        check_data(&r, 0, complete as u64 * 13);
    }
}

/// Flip one byte anywhere after the header: never a panic, never wrong data — the reader stops before the
/// damaged chunk (checksum, chunk magic or channel count) and flags it.
#[test]
fn read_flip_every_byte() {
    let dir = tmp_dir("rec-flip");
    let h = header(TWO);
    let (b, offs) = build(&h, 3, 7);
    for at in offs[0] as usize..b.len() {
        let mut d = b.clone();
        d[at] ^= 0x5a;
        let p = save(&dir, "flip.h8r", &d);
        let r = read(&p, None).unwrap_or_else(|e| panic!("flip {at}: {e}"));
        let k = offs.iter().rposition(|&o| o as usize <= at).unwrap(); // damaged chunk
        assert!(r.truncated, "flip {at}");
        assert_eq!(
            r.chunks.len(),
            k,
            "flip {at}: index ends before the damaged chunk"
        );
        // whatever was loaded is the undamaged data
        let rows = r.data[0].len() as u64;
        assert!(
            rows <= k as u64 * 7,
            "flip {at}: loaded {rows} rows past the damage"
        );
        check_data(&r, r.first_row, rows);
    }
}

#[test]
fn read_rejects_bad_files() {
    let dir = tmp_dir("rec-bad");
    let (good, _) = build(&header(TWO), 1, 10);
    let mut magic = good.clone();
    magic[0] = b'X';
    let mut huge = good.clone();
    huge[8..12].copy_from_slice(&(2u32 << 20).to_le_bytes());
    let mut past = good[..40].to_vec();
    past[8..12].copy_from_slice(&1000u32.to_le_bytes());
    #[rustfmt::skip]
    let cases: &[(&str, Vec<u8>)] = &[
        ("empty file",          vec![]),
        ("magic only",          b"H8REC001".to_vec()),
        ("wrong magic",         magic),
        ("header > 1 MB",       huge),
        ("header past the end", past),
    ];
    for (name, b) in cases {
        let p = save(&dir, "bad.h8r", b);
        assert!(read(&p, None).is_err(), "{name}");
    }
    assert!(read(&dir.join("missing.h8r"), None).is_err());
    // a chunk with another channel count than the header ends the scan (not an error)
    let mut h3 = header(TWO);
    h3.active[5] = true;
    let (mut b, _) = build(&header(TWO), 2, 10);
    let (b3, offs3) = build(&h3, 1, 10);
    b.extend_from_slice(&b3[offs3[0] as usize..]);
    let p = save(&dir, "nch.h8r", &b);
    let r = read(&p, None).unwrap();
    assert_eq!((r.chunks.len(), r.truncated), (2, true));
}

/// Windows over a 10 × 100-row file: newest that fit, or grown around a row both ways.
#[test]
fn read_window_mcdc() {
    let dir = tmp_dir("rec-win");
    let (b, _) = build(&header(TWO), 10, 100);
    let p = save(&dir, "w.h8r", &b);
    #[rustfmt::skip]
    let cases: &[(&str, Option<u64>, Option<u64>, u64, u64)] = &[
        // name                              max_rows    around     → first_row rows
        ("no limit: everything",             None,       None,       0,   1000),
        ("limit = file",                     Some(1000), None,       0,   1000),
        ("newest 2 fit",                     Some(250),  None,       800, 200),
        ("newest: limit = 1 chunk",          Some(100),  None,       900, 100),
        ("around row 0",                     Some(250),  Some(0),    0,   200),
        ("around row 550 grows both ways",   Some(300),  Some(550),  400, 300),
        ("around row 599 (chunk end)",       Some(100),  Some(599),  500, 100),
        ("around row 600 (chunk start)",     Some(100),  Some(600),  600, 100),
        ("around past the end",              Some(300),  Some(5000), 700, 300),
        ("around, no limit",                 None,       Some(300),  0,   1000),
        ("around, limit < 1 chunk: 1 chunk", Some(50),   Some(300),  300, 100),
    ];
    for &(name, max, around, first, rows) in cases {
        let r = read_window(&p, max, around).unwrap();
        assert_eq!(
            (r.first_row, r.data[0].len() as u64),
            (first, rows),
            "{name}"
        );
        assert_eq!(
            r.chunks.len(),
            10,
            "{name}: the index always covers the whole file"
        );
        assert_eq!(r.total_rows, 1000, "{name}");
        check_data(&r, first, rows);
    }
    // limit smaller than one chunk without `around`: nothing loaded (index only)
    let r = read_window(&p, Some(50), None).unwrap();
    assert_eq!(r.data[0].len(), 0);
}

// ───────────────────────────── time stamps ─────────────────────────────

#[test]
fn stamp_and_utc_dates() {
    #[rustfmt::skip]
    let cases: &[(u64, &str)] = &[
        (0,              "1970-01-01_000000"),
        (59,             "1970-01-01_000059"),
        (86_399,         "1970-01-01_235959"),
        (951_782_400,    "2000-02-29_000000"), // leap day of a /400 year
        (1_709_164_800,  "2024-02-29_000000"),
        (4_107_542_400,  "2100-03-01_000000"), // 2100 is no leap year
        (1_795_305_600,  "2026-11-22_000000"),
        (1_795_350_245,  "2026-11-22_122405"),
    ];
    for &(s, want) in cases {
        let t = UNIX_EPOCH + Duration::from_secs(s);
        assert_eq!(stamp(t), want, "{s}");
    }
    let t = UNIX_EPOCH + Duration::from_secs(1_795_350_245);
    assert_eq!(utc(t), "2026-11-22 12:24:05 UTC");
    // before 1970 → clamped to the epoch, no panic
    assert_eq!(
        stamp(UNIX_EPOCH - Duration::from_secs(10)),
        "1970-01-01_000000"
    );
}

// ───────────────────────────── writer thread ─────────────────────────────

fn limits(dir: &Path, auto: bool) -> RecMsg {
    RecMsg::Limits(RecLimits {
        dir: dir.to_path_buf(),
        auto,
        max_bytes: 1 << 40,
        keep_free: 0,
    })
}

fn rows(n: usize, from: u64) -> RecMsg {
    let mut r: [Vec<u16>; CHANNELS] = Default::default();
    r[0] = (0..n as u64).map(|i| val(0, from + i)).collect();
    r[2] = (0..n as u64).map(|i| val(1, from + i)).collect();
    RecMsg::Rows(r)
}

fn start(t: u64) -> RecMsg {
    let mut h = header(TWO);
    h.started_unix = t as f64;
    RecMsg::Start(h)
}

fn shutdown(s: &RecSink) {
    s.shutdown(Duration::from_secs(5));
}

/// Writer scenarios: message sequence → files and rows on disk.
#[test]
fn writer_sequences() {
    struct Case {
        name: &'static str,
        msgs: Vec<RecMsg>,
        files: Vec<u64>, // rows per file, oldest first
    }
    let t0 = 1_700_000_000u64;
    let d = |n: &str| tmp_dir(&format!("wr-{n}"));
    // every stream starts with one full chunk, so its file is open (see writer_short_stream_is_written)
    const C: usize = CHUNK_ROWS;
    const C64: u64 = CHUNK_ROWS as u64;
    #[rustfmt::skip]
    let cases = vec![
        ("start rows stop", vec![start(t0), rows(C, 0), rows(50, C64), RecMsg::Stop], vec![C64 + 50]),
        ("auto off: nothing", vec![RecMsg::Stop, start(t0), rows(C, 0), RecMsg::Stop], vec![]),
        ("rows before start dropped", vec![rows(77, 0), start(t0), rows(C, 0), RecMsg::Stop], vec![C64]),
        ("rows after stop dropped", vec![start(t0), rows(C, 0), RecMsg::Stop, rows(5, C64)], vec![C64]),
        ("start twice: two files", vec![start(t0), rows(C, 0), start(t0 + 5), rows(C, 0), rows(20, C64), RecMsg::Stop], vec![C64, C64 + 20]),
        ("same second: _2 name", vec![start(t0), rows(C, 0), start(t0), rows(C, 0), rows(20, C64), RecMsg::Stop], vec![C64, C64 + 20]),
        ("more than a chunk", vec![start(t0), rows(5000, 0), rows(5000, 5000), RecMsg::Stop], vec![10_000]),
        ("start without rows: no file", vec![start(t0), RecMsg::Stop], vec![]),
        ("shutdown flushes", vec![start(t0), rows(C, 0), rows(33, C64)], vec![C64 + 33]),
    ];
    for (k, (name, msgs, files)) in cases.into_iter().enumerate() {
        let c = Case { name, msgs, files };
        let dir = d(&k.to_string());
        let s = spawn_writer();
        s.send(limits(&dir, k != 1));
        for m in c.msgs {
            s.send(m);
        }
        shutdown(&s);
        let list = list(&dir);
        let got: Vec<u64> = list
            .iter()
            .map(|(p, _)| read(p, None).unwrap().total_rows)
            .collect();
        // files of the same second are ordered by mtime; compare as sorted multisets
        let (mut g, mut w) = (got.clone(), c.files.clone());
        g.sort();
        w.sort();
        assert_eq!(g, w, "{}: {list:?}", c.name);
        for (p, _) in &list {
            let r = read(p, None).unwrap();
            assert!(!r.truncated, "{}", c.name);
            check_data(&r, 0, r.total_rows);
        }
        assert_eq!(s.shared.lost.load(Ordering::Relaxed), 0, "{}", c.name);
    }
}

#[test]
fn writer_meta_and_auto_off_mid_stream() {
    let dir = tmp_dir("wr-meta");
    let s = spawn_writer();
    s.send(limits(&dir, true));
    let mut g = [[1.0f32; 3]; CHANNELS];
    g[2][1] = 1.25;
    s.send(RecMsg::Meta(g, "probe-text".into()));
    s.send(start(1_700_000_100));
    s.send(rows(CHUNK_ROWS, 0));
    s.send(rows(40, CHUNK_ROWS as u64));
    s.send(limits(&dir, false)); // switched off: the file is closed complete
    s.send(rows(40, 40));
    shutdown(&s);
    let l = list(&dir);
    assert_eq!(l.len(), 1);
    let r = read(&l[0].0, None).unwrap();
    assert_eq!(r.total_rows, CHUNK_ROWS as u64 + 40);
    assert_eq!(r.header.gain[2][1], 1.25);
    assert_eq!(r.header.probes, "probe-text");
    let st = s.shared.status.lock().unwrap().clone();
    assert_eq!(st.rows, CHUNK_ROWS as u64 + 40);
    assert!(st.problem.is_none());
}

/// Folder limit: after 8 chunks the guard deletes the oldest *other* stream recordings (never the current one,
/// never event logs) until the folder fits.
#[test]
fn writer_folder_limit_deletes_oldest_others() {
    let dir = tmp_dir("wr-folder");
    let old: Vec<PathBuf> = (0..3)
        .map(|i| {
            let p = dir.join(format!("stream_2020-01-0{}_000000.h8r", i + 1));
            std::fs::write(&p, vec![0u8; 100_000]).unwrap();
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_modified(SystemTime::now() - Duration::from_secs(1000 - i * 100))
                .unwrap();
            p
        })
        .collect();
    let ev = dir.join("events_2020-01-01_000000.h8r");
    std::fs::write(&ev, vec![0u8; 500_000]).unwrap();
    let s = spawn_writer();
    s.send(RecMsg::Limits(RecLimits {
        dir: dir.clone(),
        auto: true,
        max_bytes: 250_000,
        keep_free: 0,
    }));
    s.send(start(1_700_000_200));
    for k in 0..8u64 {
        s.send(rows(CHUNK_ROWS, k * CHUNK_ROWS as u64));
    }
    assert!(wait_until(10.0, || !old[1].exists()), "two oldest deleted");
    shutdown(&s);
    assert!(!old[0].exists() && !old[1].exists());
    assert!(
        old[2].exists(),
        "the folder fits again: newest old file kept"
    );
    assert!(ev.exists(), "event logs are not counted or deleted");
    assert!(s.shared.status.lock().unwrap().problem.is_none());
    let cur = list(&dir)
        .into_iter()
        .find(|(p, _)| !old.contains(p))
        .unwrap();
    assert_eq!(
        read(&cur.0, None).unwrap().total_rows,
        8 * CHUNK_ROWS as u64
    );
}

#[test]
fn writer_stops_at_the_limits() {
    #[rustfmt::skip]
    let cases: &[(&str, u64, u64, &str)] = &[
        ("keep-free: disk 'full'",       1 << 40, u64::MAX, "disk almost full"),
        ("this recording > folder limit", 10_000,  0,        "folder limit"),
    ];
    for (k, &(name, max_bytes, keep_free, problem)) in cases.iter().enumerate() {
        let dir = tmp_dir(&format!("wr-stop{k}"));
        let s = spawn_writer();
        s.send(RecMsg::Limits(RecLimits {
            dir: dir.clone(),
            auto: true,
            max_bytes,
            keep_free,
        }));
        s.send(start(1_700_000_300));
        for i in 0..10u64 {
            s.send(rows(CHUNK_ROWS, i * CHUNK_ROWS as u64));
        }
        shutdown(&s);
        let p = s
            .shared
            .status
            .lock()
            .unwrap()
            .problem
            .clone()
            .unwrap_or_default();
        assert!(p.contains(problem), "{name}: {p:?}");
        let l = list(&dir);
        assert_eq!(l.len(), 1, "{name}");
        let r = read(&l[0].0, None).unwrap();
        assert_eq!(
            r.total_rows,
            8 * CHUNK_ROWS as u64,
            "{name}: stopped after the guard's 8th chunk"
        );
        assert!(!r.truncated, "{name}: closed cleanly");
    }
}

#[test]
fn spool_limit_counts_lost_blocks_never_blocks() {
    let dir = tmp_dir("wr-spool");
    let s = spawn_writer();
    s.send(limits(&dir, true));
    s.spool.max_bytes.store(0, Ordering::Relaxed);
    let t = std::time::Instant::now();
    for _ in 0..100 {
        s.send(rows(10, 0));
    }
    assert!(t.elapsed() < Duration::from_millis(200), "send never waits");
    assert_eq!(s.shared.lost.load(Ordering::Relaxed), 100);
    // control messages always pass
    s.send(RecMsg::Stop);
    shutdown(&s);
    assert!(list(&dir).is_empty());
}

#[test]
fn unwritable_folder_is_a_problem_not_a_panic() {
    let dir = tmp_dir("wr-ro");
    let blocker = dir.join("file-not-dir");
    std::fs::write(&blocker, b"x").unwrap();
    let s = spawn_writer();
    s.send(limits(&blocker.join("sub"), true));
    s.send(start(1_700_000_400));
    s.send(rows(CHUNK_ROWS, 0));
    s.send(rows(10, 0));
    shutdown(&s);
    let p = s.shared.status.lock().unwrap().problem.clone();
    assert!(p.is_some_and(|p| p.contains("cannot create")));
}

/// A stream that ends before its first chunk is due (< 4096 rows and < 2 s, e.g. a quick look in stream
/// mode, or quitting right after starting) never reaches the disk: `Writer::close` only flushes when a file
/// is already open, and the file is opened lazily by the first chunk. Auto-store promises every stream.
#[test]
fn writer_short_stream_is_written() {
    for is_stop in [true, false] {
        let dir = tmp_dir(&format!("wr-short-{is_stop}"));
        let s = spawn_writer();
        s.send(limits(&dir, true));
        s.send(start(1_700_000_500));
        s.send(rows(100, 0));
        s.send(if is_stop {
            RecMsg::Stop
        } else {
            limits(&dir, false)
        }); // stream ends / auto-store off
        shutdown(&s);
        let l = list(&dir);
        assert_eq!(l.len(), 1, "stop: {is_stop}");
        assert_eq!(read(&l[0].0, None).unwrap().total_rows, 100);
    }
    // quitting the app right after a stream started
    let dir = tmp_dir("wr-short-quit");
    let s = spawn_writer();
    s.send(limits(&dir, true));
    s.send(start(1_700_000_600));
    s.send(rows(100, 0));
    shutdown(&s);
    assert_eq!(list(&dir).len(), 1, "shutdown");
}
