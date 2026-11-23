//! acq::History (shared memory limit of captures + stream recording) and StreamRec.
//!
//! `History::trim` decision, evaluated while over the limit (first true branch wins):
//!   A streaming && caps > 0  → drop the oldest capture
//!   B !streaming && rows > 0 → drop the whole stream recording
//!   C streaming && rows > 1  → drop the oldest 10 % of stream rows
//!   D !streaming && caps > 1 → drop the oldest capture
//!   else                     → stop (keep the newest capture / rows)
//! `streaming` = true from append_stream / fit_stream, false from push / set_budget.

use crate::acq::*;
use crate::hantek::*;

fn entry(samples: usize) -> HEntry {
    let mut s = Snapshot::default();
    s.frame.data[0] = vec![2048; samples];
    HEntry {
        snap: s,
        at: std::time::SystemTime::now(),
    }
}

/// Bytes one `entry(samples)` costs in the history.
fn cost(samples: usize) -> usize {
    let mut h = History::new(usize::MAX);
    h.push(entry(samples));
    h.used_bytes()
}

fn frame(rows: usize) -> Frame {
    let mut f = Frame::default();
    f.data[0] = (0..rows as u16).collect();
    f
}

/// History with `caps` captures of 1000 samples and `rows` stream rows on CH1, under no limit.
fn state(caps: usize, rows: usize) -> History {
    let mut h = History::new(usize::MAX);
    for _ in 0..caps {
        h.push(entry(1000));
    }
    h.stream.active[0] = true;
    if rows > 0 {
        h.append_stream(&frame(rows), rows);
    }
    h
}

#[derive(Clone, Copy, Debug)]
enum Op {
    /// a new capture arrives (trim false)
    Push,
    /// stream rows arrive (trim true)
    Stream(usize),
    /// the limit changes (trim false)
    SetBudget,
    /// an opened file is made to fit (trim true)
    FitStream,
}

#[test]
fn trim_mcdc() {
    let c = cost(1000);
    #[rustfmt::skip]
    let cases: &[(&str, usize, usize, Op, Box<dyn Fn(usize) -> usize>, usize, usize)] = &[
        // name                                  caps rows  op               budget(c)             → caps rows
        ("A: stream arrives, captures go first",  3, 0,    Op::Stream(1000),  Box::new(|c| c + 2000),  1, 1000),
        ("A→C: no captures left, oldest rows go", 1, 0,    Op::Stream(1000),  Box::new(|_| 1500),      0, 729),
        ("C: stream only, 10 % steps",            0, 1000, Op::FitStream,     Box::new(|_| 1500),      0, 729),
        ("C stop: 1 row is kept",                 0, 1,    Op::FitStream,     Box::new(|_| 0),         0, 1),
        ("B: new capture drops the stream",       2, 1000, Op::Push,          Box::new(|c| 3 * c),     3, 0),
        ("B then D: stream gone, then captures",  3, 1000, Op::Push,          Box::new(|c| 2 * c),     2, 0),
        ("D: limit lowered, oldest captures go",  5, 0,    Op::SetBudget,     Box::new(|c| 2 * c + 1), 2, 0),
        ("D stop: newest capture is kept",        3, 0,    Op::SetBudget,     Box::new(|_| 0),         1, 0),
        ("B via set_budget: stream before caps",  2, 1000, Op::SetBudget,     Box::new(|c| 2 * c),     2, 0),
        ("under the limit: nothing dropped",      2, 1000, Op::SetBudget,     Box::new(|c| 2 * c + 2000), 2, 1000),
        ("A via fit_stream (opened file)",        3, 1000, Op::FitStream,     Box::new(|c| c + 2000),  1, 1000),
    ];
    for (name, caps, rows, op, budget, want_caps, want_rows) in cases {
        let mut h = state(*caps, *rows);
        let b = budget(c);
        let first0 = h.first_no;
        match op {
            Op::Push => {
                h.budget = b;
                h.push(entry(1000));
            }
            Op::Stream(n) => {
                h.budget = b;
                h.stream.active[0] = true;
                h.append_stream(&frame(*n), *n);
            }
            Op::SetBudget => h.set_budget(b),
            Op::FitStream => {
                h.budget = b;
                h.fit_stream();
            }
        }
        assert_eq!(
            (h.len(), h.stream.rows()),
            (*want_caps, *want_rows),
            "{name}"
        );
        // invariants: within the limit unless only the newest data is left; numbers stay stable
        let minimal = h.len() <= 1 && h.stream.rows() <= 1;
        assert!(
            h.used_bytes() <= h.budget || minimal,
            "{name}: {} > {}",
            h.used_bytes(),
            h.budget
        );
        let pushed = *caps as u64 + matches!(op, Op::Push) as u64;
        assert_eq!(
            h.first_no - first0 + h.len() as u64,
            pushed,
            "{name}: numbering"
        );
        if *rows > 0 && *want_rows == 0 {
            assert_eq!(
                h.stream.dropped, *rows as u64,
                "{name}: dropped rows counted"
            );
        }
    }
}

#[test]
fn stream_rows_stay_absolute_when_dropped() {
    let mut h = state(0, 0);
    h.budget = 2000; // 1000 rows of 1 channel
    for k in 0..50u16 {
        let mut f = Frame::default();
        f.data[0] = (k * 100..k * 100 + 100).collect();
        h.append_stream(&f, 100);
        // row number of data[0] = dropped; values encode their absolute row
        assert_eq!(
            h.stream.data[0][0] as u64, h.stream.dropped,
            "after block {k}"
        );
        assert!(h.used_bytes() <= 2000);
    }
    assert_eq!(h.stream.dropped + h.stream.rows() as u64, 5000);
}

#[test]
fn history_numbers_lookups() {
    let c = cost(1000);
    let mut h = History::new(3 * c);
    assert_eq!((h.last_no(), h.len()), (None, 0));
    assert!(h.get(0).is_none());
    for k in 0..10u64 {
        h.push(entry(1000));
        assert_eq!(h.last_no(), Some(k));
    }
    #[rustfmt::skip]
    let lookups: &[(u64, bool)] = &[(0, false), (6, false), (7, true), (8, true), (9, true), (10, false), (u64::MAX, false)];
    for &(no, present) in lookups {
        assert_eq!(h.get(no).is_some(), present, "capture #{no}");
    }
    assert_eq!(h.first_no, 7);
    assert!(h.megabytes() > 0.0);
}

#[test]
fn stream_rec_append_and_drop() {
    let mut r = StreamRec {
        active: [true, false, true, false, false, false, false, false],
        ..Default::default()
    };
    let mut f = Frame::default();
    f.data[0] = (0..10).collect();
    f.data[1] = vec![9; 10]; // not recorded
    f.data[2] = (100..110).collect();
    #[rustfmt::skip]
    let steps: &[(usize, usize)] = &[
        // rows asked → total rows after
        (3, 3),
        (0, 3),
        (50, 13), // more than the frame has: the whole frame
    ];
    for &(rows, total) in steps {
        r.append(&f, rows);
        assert_eq!(r.rows(), total, "append {rows}");
        assert!(r.data[1].is_empty());
    }
    assert_eq!(r.data[0][..3], [7, 8, 9]);
    assert_eq!(r.bytes(), 2 * 2 * 13);
    // an inactive channel's stale data doesn't count as rows
    r.data[5] = vec![0; 1000];
    assert_eq!(r.rows(), 13);
    // empty recording: 0 rows
    assert_eq!(StreamRec::default().rows(), 0);
}
