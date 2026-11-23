//! Recording files (`.h8r`): append-only, crash-tolerant storage of stream recordings, and the writer thread
//! that fills them (acq → RAM spool → writer → disk; acq never waits for the disk). A watchdog in the writer
//! compares the disk with the incoming data every second; a slow or stalled disk only grows the spool (RAM),
//! up to its limit, and the UI warns long before anything is lost. `bench_dir` measures a folder in the background.
//!
//! Layout (little endian):
//! ```text
//! "H8REC001"  u32 header_len  header (UTF-8 key=value lines)
//! chunk*:  "CHNK" u64 first_row u32 rows u8 nch u8[3] 0
//!          u16 min[nch] u16 max[nch]  u16 data[nch][rows] (channel-planar)  u32 fnv1a(first_row .. data)
//! ```
//! Every chunk is written whole and flushed, so after a crash all complete chunks are readable; the reader
//! stops at a truncated or corrupt last chunk. The chunk headers (row number, size, min/max) are the index.

use crate::hantek::CHANNELS;
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const MAGIC: &[u8; 8] = b"H8REC001";
const CHUNK_MAGIC: &[u8; 4] = b"CHNK";
/// rows per chunk (440 S/s → ~9 s; a chunk is also closed after `CHUNK_MAX_AGE`)
pub const CHUNK_ROWS: usize = 4096;
const CHUNK_MAX_AGE: Duration = Duration::from_secs(2);
/// start a new file above this size
const ROTATE_BYTES: u64 = 2 << 30;
pub const EXT: &str = "h8r";

/// Everything needed to turn the raw samples back into units.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RecHeader {
    pub started_unix: f64,
    /// rows per second per channel
    pub rate: f64,
    /// `a3` roll rate id the stream ran at
    pub roll_id: u8,
    pub active: [bool; CHANNELS],
    /// hardware range id per channel (VScale::id)
    pub vscale: [u8; CHANNELS],
    /// zero offsets in counts per range index × channel
    pub zero: [[f32; CHANNELS]; 3],
    /// calibration gain per channel × range index
    pub gain: [[f32; 3]; CHANNELS],
    /// UI probe settings (opaque, `Probe::encode` joined by ',')
    pub probes: String,
    /// triggered captures (references, events): timebase id, trigger position (0.1 %), trigger jitter offset
    pub capture: Option<(u8, u16, f32)>,
}

fn join<T: ToString>(it: impl IntoIterator<Item = T>) -> String {
    it.into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn floats(s: &str) -> Vec<f32> {
    s.split(',').filter_map(|x| x.trim().parse().ok()).collect()
}

impl RecHeader {
    pub fn encode(&self) -> String {
        let mut s = format!(
            "format=1\nkind=stream\napp=hantek1008c {}\nstarted_unix={:.3}\nrate={}\nroll_id={}\nactive={}\nvscale={}\n",
            env!("CARGO_PKG_VERSION"),
            self.started_unix,
            self.rate,
            self.roll_id,
            join(self.active.iter().map(|&a| a as u8)),
            join(self.vscale),
        );
        for (i, z) in self.zero.iter().enumerate() {
            s += &format!("zero{i}={}\n", join(z.iter()));
        }
        s += &format!("gain={}\n", join(self.gain.iter().flatten()));
        s += &format!("probes={}\n", self.probes.replace('\n', " "));
        if let Some((tb, pos, off)) = self.capture {
            s += &format!("capture={tb},{pos},{off}\n");
        }
        s
    }

    pub fn decode(s: &str) -> RecHeader {
        let mut h = RecHeader {
            gain: [[1.0; 3]; CHANNELS],
            ..Default::default()
        };
        for line in s.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k {
                "started_unix" => h.started_unix = v.parse().unwrap_or(0.0),
                "rate" => h.rate = v.parse().unwrap_or(0.0),
                "roll_id" => h.roll_id = v.parse().unwrap_or(0),
                "active" => v
                    .split(',')
                    .take(CHANNELS)
                    .enumerate()
                    .for_each(|(i, x)| h.active[i] = x.trim() == "1"),
                "vscale" => v
                    .split(',')
                    .take(CHANNELS)
                    .enumerate()
                    .for_each(|(i, x)| h.vscale[i] = x.trim().parse().unwrap_or(3)),
                "zero0" | "zero1" | "zero2" => {
                    let r = (k.as_bytes()[4] - b'0') as usize;
                    floats(v)
                        .into_iter()
                        .take(CHANNELS)
                        .enumerate()
                        .for_each(|(i, z)| h.zero[r][i] = z);
                }
                "gain" => floats(v)
                    .into_iter()
                    .take(CHANNELS * 3)
                    .enumerate()
                    .for_each(|(i, g)| h.gain[i / 3][i % 3] = g),
                "probes" => h.probes = v.to_string(),
                "capture" => {
                    let p: Vec<&str> = v.split(',').collect();
                    h.capture = match p[..] {
                        [tb, pos, off] => Some((
                            tb.trim().parse().unwrap_or(0),
                            pos.trim().parse().unwrap_or(500),
                            off.trim().parse().unwrap_or(0.0),
                        )),
                        _ => None,
                    };
                }
                _ => {}
            }
        }
        h
    }

    fn nch(&self) -> usize {
        self.active.iter().filter(|&&a| a).count()
    }
}

fn fnv1a(h: u32, bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(h, |h, &b| (h ^ b as u32).wrapping_mul(0x0100_0193))
}
const FNV0: u32 = 0x811c_9dc5;

pub fn write_header(w: &mut impl Write, h: &RecHeader) -> io::Result<u64> {
    let text = h.encode();
    w.write_all(MAGIC)?;
    w.write_all(&(text.len() as u32).to_le_bytes())?;
    w.write_all(text.as_bytes())?;
    Ok(12 + text.len() as u64)
}

/// Write one chunk: `data` = one Vec per *active* channel, all the same length. Returns bytes written.
pub fn write_chunk(w: &mut impl Write, first_row: u64, data: &[&[u16]]) -> io::Result<u64> {
    let rows = data.first().map_or(0, |d| d.len());
    let nch = data.len();
    let mut body = Vec::with_capacity(16 + nch * 4 + nch * rows * 2);
    body.extend_from_slice(&first_row.to_le_bytes());
    body.extend_from_slice(&(rows as u32).to_le_bytes());
    body.extend_from_slice(&[nch as u8, 0, 0, 0]);
    for d in data {
        body.extend_from_slice(&d.iter().copied().min().unwrap_or(0).to_le_bytes());
    }
    for d in data {
        body.extend_from_slice(&d.iter().copied().max().unwrap_or(0).to_le_bytes());
    }
    for d in data {
        debug_assert_eq!(d.len(), rows);
        for v in d.iter() {
            body.extend_from_slice(&v.to_le_bytes());
        }
    }
    w.write_all(CHUNK_MAGIC)?;
    w.write_all(&body)?;
    w.write_all(&fnv1a(FNV0, &body).to_le_bytes())?;
    Ok(4 + body.len() as u64 + 4)
}

/// One chunk as found in a file (the index).
#[derive(Clone, Debug, PartialEq)]
pub struct ChunkInfo {
    pub offset: u64,
    pub first_row: u64,
    pub rows: u32,
    pub min: Vec<u16>,
    pub max: Vec<u16>,
}

pub struct Recording {
    pub header: RecHeader,
    /// the index of the whole file (min/max per chunk: overview of long recordings)
    pub chunks: Vec<ChunkInfo>,
    /// samples per channel (only active channels filled), from `first_row` on
    pub data: [Vec<u16>; CHANNELS],
    /// row number of data[..][0] (rows before it were left out to fit `max_rows`)
    pub first_row: u64,
    pub total_rows: u64,
    /// the file ended in a partial or corrupt chunk (crash / power loss) — everything before it was read
    pub truncated: bool,
}

/// Read a recording; with `max_rows`, only the newest whole chunks that fit are loaded (the index covers all).
#[cfg_attr(not(test), allow(dead_code))]
pub fn read(path: &Path, max_rows: Option<u64>) -> io::Result<Recording> {
    read_window(path, max_rows, None)
}

/// Like `read`, but with `around` the loaded part is the run of chunks around that row (as many as fit
/// `max_rows`, growing both ways) instead of the newest ones — browsing a long file part by part.
pub fn read_window(
    path: &Path,
    max_rows: Option<u64>,
    around: Option<u64>,
) -> io::Result<Recording> {
    let mut f = io::BufReader::new(File::open(path)?);
    let len = f.get_ref().metadata()?.len();
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a hantek1008c recording (.h8r)",
        ));
    }
    let mut b4 = [0u8; 4];
    f.read_exact(&mut b4)?;
    let hl = u32::from_le_bytes(b4) as u64;
    if hl > 1 << 20 || 12 + hl > len {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad header"));
    }
    let mut text = vec![0u8; hl as usize];
    f.read_exact(&mut text)?;
    let header = RecHeader::decode(&String::from_utf8_lossy(&text));
    let nch = header.nch();
    // scan the chunk headers (seek over the data)
    let mut chunks = Vec::new();
    let mut pos = 12 + hl;
    let mut truncated = false;
    let fixed = 4 + 8 + 4 + 4;
    while pos < len {
        if pos + fixed as u64 > len {
            truncated = true;
            break;
        }
        f.seek(SeekFrom::Start(pos))?;
        let mut hd = [0u8; 20];
        f.read_exact(&mut hd)?;
        let rows = u32::from_le_bytes(hd[12..16].try_into().unwrap());
        let n = hd[16] as usize;
        if &hd[..4] != CHUNK_MAGIC || n != nch {
            truncated = true;
            break;
        }
        let size = fixed as u64 + (n * 4) as u64 + (n as u64 * rows as u64 * 2) + 4;
        if pos + size > len {
            truncated = true;
            break;
        }
        let mut mm = vec![0u8; n * 4];
        f.read_exact(&mut mm)?;
        let w = |i: usize| u16::from_le_bytes([mm[2 * i], mm[2 * i + 1]]);
        chunks.push(ChunkInfo {
            offset: pos,
            first_row: u64::from_le_bytes(hd[4..12].try_into().unwrap()),
            rows,
            min: (0..n).map(w).collect(),
            max: (n..2 * n).map(w).collect(),
        });
        pos += size;
    }
    // pick the chunks to load: the newest that fit, or the ones around `around`
    let fits = |sum: u64, add: u32| max_rows.is_none_or(|m| sum + add as u64 <= m);
    let (mut start, mut end, mut sum) = (chunks.len(), chunks.len(), 0u64);
    match around {
        Some(r) if !chunks.is_empty() => {
            let k = chunks.iter().rposition(|c| c.first_row <= r).unwrap_or(0);
            (start, end, sum) = (k, k + 1, chunks[k].rows as u64);
            loop {
                let mut grew = false;
                if start > 0 && fits(sum, chunks[start - 1].rows) {
                    start -= 1;
                    sum += chunks[start].rows as u64;
                    grew = true;
                }
                if end < chunks.len() && fits(sum, chunks[end].rows) {
                    sum += chunks[end].rows as u64;
                    end += 1;
                    grew = true;
                }
                if !grew {
                    break;
                }
            }
        }
        _ => {
            while start > 0 && fits(sum, chunks[start - 1].rows) {
                start -= 1;
                sum += chunks[start].rows as u64;
            }
        }
    }
    let mut data: [Vec<u16>; CHANNELS] = Default::default();
    let act: Vec<usize> = (0..CHANNELS).filter(|&c| header.active[c]).collect();
    for c in &act {
        data[*c].reserve(sum as usize);
    }
    let mut good = chunks.len();
    for (k, c) in chunks.iter().enumerate().take(end).skip(start) {
        f.seek(SeekFrom::Start(c.offset + 4))?;
        let body_len = 16 + nch * 4 + nch * c.rows as usize * 2;
        let mut body = vec![0u8; body_len + 4];
        f.read_exact(&mut body)?;
        let sum_ok = fnv1a(FNV0, &body[..body_len])
            == u32::from_le_bytes(body[body_len..].try_into().unwrap());
        if !sum_ok {
            // a torn write can only be the last chunk; anything else is real corruption — stop there either way
            truncated = true;
            good = k;
            break;
        }
        let samples = &body[16 + nch * 4..body_len];
        for (i, ch) in act.iter().enumerate() {
            let s = &samples[i * c.rows as usize * 2..(i + 1) * c.rows as usize * 2];
            data[*ch].extend(
                s.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| u16::from_le_bytes([b[0], b[1]])),
            );
        }
    }
    chunks.truncate(good);
    let first_row = chunks.get(start).map_or(0, |c| c.first_row);
    let total_rows = chunks.last().map_or(0, |c| c.first_row + c.rows as u64);
    Ok(Recording {
        header,
        chunks,
        data,
        first_row,
        total_rows,
        truncated,
    })
}

// ───────────────────────────── writer thread ─────────────────────────────

/// Where and how much to record (from Settings).
#[derive(Clone, Debug, PartialEq)]
pub struct RecLimits {
    pub dir: PathBuf,
    /// write stream recordings to disk automatically
    pub auto: bool,
    /// all recordings in `dir` together; oldest other recordings are deleted above it
    pub max_bytes: u64,
    /// stop recording when the disk has less free space than this
    pub keep_free: u64,
}

pub enum RecMsg {
    /// new stream started (acq); header without calibration/probes — the writer adds the latest from `Meta`
    Start(RecHeader),
    /// new rows, one Vec per channel (inactive empty)
    Rows([Vec<u16>; CHANNELS]),
    /// stream ended
    Stop,
    Limits(RecLimits),
    /// calibration gains + probes from the UI, written into the next file header
    Meta([[f32; 3]; CHANNELS], String),
    /// app quits: write what is buffered, close + sync the file, then acknowledge
    Shutdown(std::sync::mpsc::Sender<()>),
}

#[derive(Clone, Debug, Default)]
pub struct RecStatus {
    pub file: Option<PathBuf>,
    pub bytes: u64,
    pub rows: u64,
    /// why recording stopped / failed (shown in the info line)
    pub problem: Option<String>,
}

#[derive(Default)]
pub struct RecShared {
    pub status: Mutex<RecStatus>,
    /// row blocks the acq thread had to drop because the writer fell behind (channel full)
    pub lost: AtomicU64,
    /// `now_ms()` when the current disk write/sync began, 0 = not writing (a stalled disk shows up here)
    pub write_since_ms: AtomicU64,
    pub health: Mutex<Health>,
}

/// Watchdog result, updated every second by the writer.
#[derive(Clone, Debug, Default)]
pub struct Health {
    /// incoming stream data, bytes/s
    pub in_bps: f64,
    /// disk throughput while writing (bytes / time spent in write+flush), bytes/s
    pub disk_bps: f64,
    /// data waiting in RAM for the disk, bytes, and the RAM limit
    pub backlog: u64,
    pub max_backlog: u64,
    /// backlog in seconds of incoming data
    pub behind_s: f64,
    /// seconds the backlog has been growing in a row
    pub growing_s: u32,
    /// at the current growth: seconds until the RAM limit is reached
    pub full_in_s: Option<f64>,
    /// slowest single write in the last second, ms
    pub worst_write_ms: f64,
}

/// Milliseconds since the first call (≥ 1, so 0 can mean "none").
pub fn now_ms() -> u64 {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// RAM spool between the acquisition thread and the disk writer. Pushing takes a short lock and never waits
/// for the disk; row data is limited to `max_bytes` (Settings), control messages always pass.
pub struct Spool {
    q: Mutex<VecDeque<RecMsg>>,
    cv: Condvar,
    /// row data waiting, bytes
    pub bytes: AtomicU64,
    pub max_bytes: AtomicU64,
    /// row data ever pushed, bytes (incoming rate)
    pub in_total: AtomicU64,
}

/// Default RAM buffer for a slow disk: 256 MB ≈ 10 h of 8-channel streaming, ≈ 50 s at full capture rate.
pub const SPOOL_DEFAULT: u64 = 256 << 20;

fn msg_bytes(m: &RecMsg) -> u64 {
    match m {
        RecMsg::Rows(r) => r.iter().map(|d| d.len() as u64 * 2).sum(),
        _ => 0,
    }
}

impl Spool {
    fn pop(&self, timeout: Duration) -> Result<RecMsg, RecvTimeoutError> {
        let mut q = self.q.lock().unwrap_or_else(|p| p.into_inner());
        if q.is_empty() {
            q = self
                .cv
                .wait_timeout(q, timeout)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        let m = q.pop_front().ok_or(RecvTimeoutError::Timeout)?;
        self.bytes.fetch_sub(msg_bytes(&m), Ordering::Relaxed);
        Ok(m)
    }
}

#[derive(Clone)]
pub struct RecSink {
    pub spool: Arc<Spool>,
    pub shared: Arc<RecShared>,
}

impl RecSink {
    /// On quit: let the writer flush and close the open recording (waits at most `timeout`).
    pub fn shutdown(&self, timeout: std::time::Duration) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(RecMsg::Shutdown(tx));
        let _ = rx.recv_timeout(timeout);
    }
    /// Never waits for the disk: rows go into the RAM spool; only when the spool is at its limit is the
    /// block dropped (and counted — the watchdog warns long before that).
    pub fn send(&self, m: RecMsg) {
        let sp = &self.spool;
        let n = msg_bytes(&m);
        if n > 0 {
            if sp.bytes.load(Ordering::Relaxed) + n > sp.max_bytes.load(Ordering::Relaxed) {
                self.shared.lost.fetch_add(1, Ordering::Relaxed);
                return;
            }
            sp.in_total.fetch_add(n, Ordering::Relaxed);
            sp.bytes.fetch_add(n, Ordering::Relaxed);
        }
        sp.q.lock().unwrap_or_else(|p| p.into_inner()).push_back(m);
        sp.cv.notify_one();
    }
}

pub fn default_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    match home {
        Some(h) if h.join("Documents").is_dir() => {
            h.join("Documents").join("hantek1008c").join("recordings")
        }
        _ => crate::hantek::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("recordings"),
    }
}

/// Free bytes on the file system holding `p` (Unix; None elsewhere = guard off).
#[cfg(unix)]
pub fn free_bytes(p: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid C string and out-pointer
    (unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0)
        .then(|| s.f_bavail as u64 * s.f_frsize as u64)
}
#[cfg(not(unix))]
pub fn free_bytes(_: &Path) -> Option<u64> {
    None
}

/// Recordings in `dir`, oldest first, with sizes.
pub fn list(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut v: Vec<(PathBuf, u64, SystemTime)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == EXT))
        // stream recordings only: event logs (events_*) are never counted or pruned by the folder limit
        .filter(|e| e.file_name().to_string_lossy().starts_with("stream_"))
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((e.path(), m.len(), m.modified().unwrap_or(UNIX_EPOCH)))
        })
        .collect();
    v.sort_by_key(|x| x.2);
    v.into_iter().map(|(p, l, _)| (p, l)).collect()
}

pub fn spawn_writer() -> RecSink {
    let spool = Arc::new(Spool {
        q: Mutex::new(VecDeque::new()),
        cv: Condvar::new(),
        bytes: AtomicU64::new(0),
        max_bytes: AtomicU64::new(SPOOL_DEFAULT),
        in_total: AtomicU64::new(0),
    });
    let shared = Arc::new(RecShared::default());
    let (s2, sp2) = (shared.clone(), spool.clone());
    std::thread::Builder::new()
        .name("rec-writer".into())
        .spawn(move || Writer::new(sp2, s2).run())
        .expect("spawn writer thread");
    RecSink { spool, shared }
}

struct Open {
    w: BufWriter<File>,
    path: PathBuf,
    bytes: u64,
    rows_written: u64,
    synced: Instant,
}

struct Writer {
    spool: Arc<Spool>,
    /// watchdog state
    tick: Instant,
    last_in: u64,
    last_backlog: u64,
    in_ewma: f64,
    disk_ewma: f64,
    growing_s: u32,
    worst_write_ms: f64,
    shared: Arc<RecShared>,
    limits: Option<RecLimits>,
    meta: ([[f32; 3]; CHANNELS], String),
    header: Option<RecHeader>,
    file: Option<Open>,
    buf: [Vec<u16>; CHANNELS],
    buf_since: Option<Instant>,
    chunks_since_check: u32,
}

impl Writer {
    fn new(spool: Arc<Spool>, shared: Arc<RecShared>) -> Self {
        Writer {
            spool,
            tick: Instant::now(),
            last_in: 0,
            last_backlog: 0,
            in_ewma: 0.0,
            disk_ewma: 0.0,
            growing_s: 0,
            worst_write_ms: 0.0,
            shared,
            limits: None,
            meta: ([[1.0; 3]; CHANNELS], String::new()),
            header: None,
            file: None,
            buf: Default::default(),
            buf_since: None,
            chunks_since_check: 0,
        }
    }

    fn problem(&self, p: Option<String>) {
        if let Some(e) = &p {
            log::warn!("recording: {e}");
        }
        if let Ok(mut s) = self.shared.status.lock() {
            s.problem = p;
        }
    }

    fn run(mut self) {
        loop {
            if self.tick.elapsed() >= Duration::from_secs(1) {
                self.watchdog();
            }
            match self.spool.pop(Duration::from_millis(250)) {
                Ok(RecMsg::Start(mut h)) => {
                    self.close();
                    (h.gain, h.probes) = (self.meta.0, self.meta.1.clone());
                    self.header = Some(h);
                    self.problem(None);
                }
                Ok(RecMsg::Rows(r)) => {
                    if self.header.is_some() && self.limits.as_ref().is_some_and(|l| l.auto) {
                        for (b, d) in self.buf.iter_mut().zip(r) {
                            b.extend_from_slice(&d);
                        }
                        self.buf_since.get_or_insert_with(Instant::now);
                        if self.buffered() >= CHUNK_ROWS {
                            self.flush_chunk(true);
                        }
                    }
                }
                Ok(RecMsg::Stop) => {
                    self.close();
                    self.header = None;
                }
                Ok(RecMsg::Limits(l)) => {
                    if !l.auto {
                        self.close();
                    }
                    self.limits = Some(l);
                }
                Ok(RecMsg::Meta(g, p)) => self.meta = (g, p),
                Ok(RecMsg::Shutdown(ack)) => {
                    self.close();
                    let _ = ack.send(());
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.close();
                    return;
                }
            }
            if self.buf_since.is_some_and(|t| t.elapsed() >= CHUNK_MAX_AGE) {
                self.flush_chunk(true);
            }
        }
    }

    fn buffered(&self) -> usize {
        let a = self.header.as_ref().map_or([false; CHANNELS], |h| h.active);
        (0..CHANNELS)
            .filter(|&c| a[c])
            .map(|c| self.buf[c].len())
            .min()
            .unwrap_or(0)
    }

    fn open(&mut self) -> Option<()> {
        let l = self.limits.clone()?;
        let h = self.header.as_mut()?;
        if let Err(e) = fs::create_dir_all(&l.dir) {
            self.problem(Some(format!("cannot create {}: {e}", l.dir.display())));
            self.header = None;
            return None;
        }
        let start = UNIX_EPOCH + Duration::from_secs_f64(h.started_unix.max(0.0));
        let name = file_name(start);
        let mut path = l.dir.join(&name);
        for k in 2.. {
            if !path.exists() {
                break;
            }
            path = l
                .dir
                .join(name.replace(&format!(".{EXT}"), &format!("_{k}.{EXT}")));
        }
        let res = File::create(&path).and_then(|f| {
            let mut w = BufWriter::with_capacity(1 << 20, f);
            let n = write_header(&mut w, h)?;
            Ok((w, n))
        });
        match res {
            Ok((w, n)) => {
                self.file = Some(Open {
                    w,
                    path: path.clone(),
                    bytes: n,
                    rows_written: 0,
                    synced: Instant::now(),
                });
                if let Ok(mut s) = self.shared.status.lock() {
                    *s = RecStatus {
                        file: Some(path),
                        bytes: n,
                        rows: 0,
                        problem: None,
                    };
                }
                Some(())
            }
            Err(e) => {
                self.problem(Some(format!("cannot write {}: {e}", path.display())));
                self.header = None;
                None
            }
        }
    }

    /// Write up to CHUNK_ROWS buffered rows as one chunk; `guard` = run the disk guard every few chunks.
    fn flush_chunk(&mut self, guard: bool) {
        self.buf_since = None;
        let Some(h) = self.header.clone() else { return };
        let rows = self.buffered().min(CHUNK_ROWS);
        if rows == 0 {
            return;
        }
        self.shared
            .write_since_ms
            .store(now_ms(), Ordering::Relaxed); // open/write/sync: watched for stalls
        if self.file.is_none() && self.open().is_none() {
            self.shared.write_since_ms.store(0, Ordering::Relaxed);
            self.buf.iter_mut().for_each(Vec::clear);
            return;
        }
        let act: Vec<usize> = (0..CHANNELS).filter(|&c| h.active[c]).collect();
        let parts: Vec<&[u16]> = act.iter().map(|&c| &self.buf[c][..rows]).collect();
        let o = self.file.as_mut().unwrap();
        let t0 = Instant::now();
        let res =
            write_chunk(&mut o.w, o.rows_written, &parts).and_then(|n| o.w.flush().map(|_| n));
        let took = t0.elapsed().as_secs_f64();
        match res {
            Ok(n) => {
                o.bytes += n;
                o.rows_written += rows as u64;
                if o.synced.elapsed() > Duration::from_secs(30) {
                    let _ = o.w.get_ref().sync_data(); // power loss: at most ~30 s lost
                    o.synced = Instant::now();
                }
                let took = t0.elapsed().as_secs_f64().max(took);
                self.worst_write_ms = self.worst_write_ms.max(took * 1e3);
                let bps = n as f64 / took.max(1e-6);
                self.disk_ewma = if self.disk_ewma == 0.0 {
                    bps
                } else {
                    0.8 * self.disk_ewma + 0.2 * bps
                };
                if let Ok(mut s) = self.shared.status.lock() {
                    s.bytes = o.bytes;
                    s.rows = o.rows_written;
                }
            }
            Err(e) => {
                let p = o.path.display().to_string();
                self.file = None;
                self.header = None;
                self.problem(Some(format!(
                    "write to {p} failed: {e} — recording stopped"
                )));
            }
        }
        self.shared.write_since_ms.store(0, Ordering::Relaxed);
        for c in &act {
            self.buf[*c].drain(..rows);
        }
        if self.buffered() >= CHUNK_ROWS {
            self.flush_chunk(guard);
        }
        self.chunks_since_check += 1;
        if guard && self.chunks_since_check >= 8 {
            self.chunks_since_check = 0;
            self.shared
                .write_since_ms
                .store(now_ms(), Ordering::Relaxed);
            self.guard();
            self.shared.write_since_ms.store(0, Ordering::Relaxed);
        }
    }

    /// Disk guard (every few chunks): rotate big files, delete the oldest other recordings above the folder
    /// limit, stop when the disk is nearly full.
    fn guard(&mut self) {
        let (Some(l), Some(o)) = (self.limits.clone(), self.file.as_ref()) else {
            return;
        };
        if o.bytes > ROTATE_BYTES {
            // continue in a new file whose start time is where this one ended
            let mut h = self.header.clone().unwrap();
            h.started_unix += o.rows_written as f64 / h.rate.max(1e-9);
            self.close();
            self.header = Some(h);
            return;
        }
        let cur = o.path.clone();
        let mut files = list(&l.dir);
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        while total > l.max_bytes {
            let Some(i) = files.iter().position(|f| f.0 != cur) else {
                break;
            };
            let (p, n) = files.remove(i);
            if fs::remove_file(&p).is_ok() {
                log::info!("recording: deleted old {} (folder limit)", p.display());
                total -= n;
            } else {
                break;
            }
        }
        let over = total > l.max_bytes;
        let low = free_bytes(&l.dir).is_some_and(|f| f < l.keep_free);
        if over || low {
            self.close();
            self.header = None;
            self.problem(Some(if low {
                "disk almost full (keep-free limit in Settings) — recording to disk stopped".into()
            } else {
                "this recording reached the folder limit in Settings — recording to disk stopped"
                    .into()
            }));
        }
    }

    /// Every second: incoming rate vs disk, RAM backlog, growth, time until the RAM limit.
    fn watchdog(&mut self) {
        let dt = self.tick.elapsed().as_secs_f64().max(1e-3);
        self.tick = Instant::now();
        let total = self.spool.in_total.load(Ordering::Relaxed);
        let in_bps = (total - self.last_in) as f64 / dt;
        self.last_in = total;
        self.in_ewma = 0.7 * self.in_ewma + 0.3 * in_bps;
        let backlog = self.spool.bytes.load(Ordering::Relaxed);
        let max = self.spool.max_bytes.load(Ordering::Relaxed);
        let growth = (backlog as f64 - self.last_backlog as f64) / dt;
        self.growing_s = if growth > 0.0 && backlog > 64 << 10 {
            self.growing_s + 1
        } else {
            0
        };
        self.last_backlog = backlog;
        let h = Health {
            in_bps: self.in_ewma,
            disk_bps: self.disk_ewma,
            backlog,
            max_backlog: max,
            behind_s: if self.in_ewma > 0.0 {
                backlog as f64 / self.in_ewma
            } else {
                0.0
            },
            growing_s: self.growing_s,
            full_in_s: (growth > 0.0).then(|| max.saturating_sub(backlog) as f64 / growth),
            worst_write_ms: std::mem::take(&mut self.worst_write_ms),
        };
        if let Ok(mut s) = self.shared.health.lock() {
            *s = h;
        }
    }

    fn close(&mut self) {
        // a stream shorter than one chunk / 2 s has no file yet: open it so its rows are not dropped
        if self.file.is_none()
            && self.header.is_some()
            && self.buffered() > 0
            && self.limits.as_ref().is_some_and(|l| l.auto)
        {
            let _ = self.open();
        }
        while self.buffered() > 0 && self.file.is_some() && self.header.is_some() {
            self.flush_chunk(false);
        }
        self.buf.iter_mut().for_each(Vec::clear);
        self.buf_since = None;
        if let Some(mut o) = self.file.take() {
            let _ = o.w.flush();
            let _ = o.w.get_ref().sync_all();
            log::info!(
                "recording: closed {} ({} rows, {:.1} MB)",
                o.path.display(),
                o.rows_written,
                o.bytes as f64 / 1e6
            );
        }
    }
}

/// `stream_2026-11-21_143005.h8r` (UTC)
fn file_name(t: SystemTime) -> String {
    format!("stream_{}.{EXT}", stamp(t))
}

/// `2026-11-21_143005` (UTC) for file names
pub fn stamp(t: SystemTime) -> String {
    let s = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, sod) = ((s / 86_400) as i64, s % 86_400);
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02}_{:02}{:02}{:02}",
        sod / 3600,
        sod / 60 % 60,
        sod % 60
    )
}

/// `2026-11-21 14:30:05 UTC` for file headers
pub fn utc(t: SystemTime) -> String {
    let st = stamp(t);
    format!(
        "{} {}:{}:{} UTC",
        &st[..10],
        &st[11..13],
        &st[13..15],
        &st[15..17]
    )
}

/// Days since 1970-01-01 → (year, month, day) (H. Hinnant's algorithm).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> RecHeader {
        let mut h = RecHeader {
            started_unix: 1_763_700_000.5,
            rate: 440.0,
            roll_id: 0x18,
            probes: "1:10:0:V,0:1:0:V".into(),
            ..Default::default()
        };
        h.active[0] = true;
        h.active[3] = true;
        h.vscale = [3, 2, 3, 1, 3, 3, 3, 3];
        h.zero[2][3] = 2047.5;
        h.gain[3][1] = 1.07;
        h
    }

    /// A 24 h, 2-channel 440 S/s recording (~150 MB) for trying the overview by hand:
    /// `H8R_LONG=/path/day.h8r cargo test --release long_file -- --ignored`, then `HANTEK_OPEN=/path/day.h8r`.
    #[test]
    #[ignore]
    fn long_file() {
        let path = PathBuf::from(
            std::env::var("H8R_LONG").unwrap_or_else(|_| tmp("day.h8r").display().to_string()),
        );
        let mut w = BufWriter::new(File::create(&path).unwrap());
        let mut h = header();
        h.probes = "0:1:0:V,0:1:0:V".into();
        write_header(&mut w, &h).unwrap();
        let rows = 24 * 3600 * 440u64;
        let mut row = 0u64;
        while row < rows {
            let n = CHUNK_ROWS.min((rows - row) as usize);
            // CH1: slow 'battery' drift over the day with two short dips; CH4: 2 Hz square, amplitude drifts
            let a: Vec<u16> = (0..n as u64)
                .map(|i| {
                    let t = (row + i) as f64 / 440.0;
                    let dip =
                        if (30_000.0..30_002.0).contains(&t) || (61_000.0..61_001.0).contains(&t) {
                            -600.0
                        } else {
                            0.0
                        };
                    (2500.0 + 300.0 * (t / 86_400.0 * std::f64::consts::TAU).sin() + dip) as u16
                })
                .collect();
            let b: Vec<u16> = (0..n as u64)
                .map(|i| {
                    let t = (row + i) as f64 / 440.0;
                    let amp = 800.0 + 400.0 * (t / 20_000.0).sin();
                    (2048.0 + if (t * 2.0).fract() < 0.5 { amp } else { -amp }) as u16
                })
                .collect();
            write_chunk(&mut w, row, &[&a, &b]).unwrap();
            row += n as u64;
        }
        w.flush().unwrap();
        println!(
            "wrote {} ({:.0} MB)",
            path.display(),
            fs::metadata(&path).unwrap().len() as f64 / 1e6
        );
    }

    #[test]
    fn capture_header_roundtrip() {
        let mut h = header();
        h.capture = Some((17, 260, -1.75));
        assert_eq!(RecHeader::decode(&h.encode()), h);
        assert_eq!(RecHeader::decode(&header().encode()).capture, None); // stream files: no capture line
    }

    /// A reference file for trying references by hand: H8R_REF=/cfg/hantek1008c/ref1.h8r cargo test --release write_ref -- --ignored
    #[test]
    #[ignore]
    fn write_ref() {
        let p = PathBuf::from(std::env::var("H8R_REF").unwrap());
        let mut h = RecHeader {
            started_unix: 0.0,
            rate: 100_000.0,
            active: [true; CHANNELS],
            vscale: [1; CHANNELS],
            gain: [[1.0; 3]; CHANNELS],
            ..Default::default()
        };
        h.zero = [[2048.0; CHANNELS]; 3];
        h.capture = Some((17, 500, 0.0));
        let _ = fs::create_dir_all(p.parent().unwrap());
        let mut w = BufWriter::new(File::create(&p).unwrap());
        write_header(&mut w, &h).unwrap();
        // 8 channels × 500 samples over 5 ms: 1 kHz square, edges at the trigger point (sample 250)
        let sq: Vec<u16> = (0..500)
            .map(|i| if ((i + 50) / 50) % 2 == 0 { 2048 } else { 2148 })
            .collect();
        let parts: Vec<&[u16]> = (0..CHANNELS).map(|_| sq.as_slice()).collect();
        write_chunk(&mut w, 0, &parts).unwrap();
        w.flush().unwrap();
    }

    fn write_file(path: &Path, chunks: usize) -> Vec<u16> {
        let mut w = BufWriter::new(File::create(path).unwrap());
        write_header(&mut w, &header()).unwrap();
        let mut all = Vec::new();
        for k in 0..chunks {
            let a: Vec<u16> = (0..100).map(|i| (k * 100 + i) as u16).collect();
            let b: Vec<u16> = a.iter().map(|v| 4000 - v).collect();
            write_chunk(&mut w, (k * 100) as u64, &[&a, &b]).unwrap();
            all.extend(a);
        }
        w.flush().unwrap();
        all
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("h8r-test-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d.join(name)
    }

    #[test]
    fn header_roundtrip() {
        let h = header();
        let mut d = RecHeader::decode(&h.encode());
        d.started_unix = (d.started_unix * 1000.0).round() / 1000.0;
        assert_eq!(d, h);
    }

    #[test]
    fn write_read_and_index() {
        let p = tmp("full.h8r");
        let all = write_file(&p, 5);
        let r = read(&p, None).unwrap();
        assert!(!r.truncated);
        assert_eq!(r.header, RecHeader::decode(&header().encode()));
        assert_eq!(r.chunks.len(), 5);
        assert_eq!(r.chunks[2].first_row, 200);
        assert_eq!((r.chunks[2].min[0], r.chunks[2].max[0]), (200, 299));
        assert_eq!(r.data[0], all);
        assert_eq!(r.data[3][0], 4000);
        assert!(r.data[1].is_empty());
        assert_eq!(r.total_rows, 500);
        // only the newest chunks that fit
        let r = read(&p, Some(250)).unwrap();
        assert_eq!((r.first_row, r.data[0].len()), (300, 200));
        assert_eq!(r.chunks.len(), 5); // a window around row 120: chunk 1 (100..200) plus one neighbour each way while it fits 300 rows
        let r = read_window(&p, Some(300), Some(120)).unwrap();
        assert_eq!((r.first_row, r.data[0].len()), (0, 300));
        assert_eq!(r.data[0], all[..300]);
        let r = read_window(&p, Some(250), Some(420)).unwrap();
        assert_eq!((r.first_row, r.data[0].len()), (300, 200));
        assert_eq!(r.total_rows, 500);
    }

    #[test]
    fn truncated_file_recovers_complete_chunks() {
        let p = tmp("cut.h8r");
        let all = write_file(&p, 4);
        let len = fs::metadata(&p).unwrap().len();
        // cut inside the last chunk (torn write at a crash)
        let f = fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_len(len - 37).unwrap();
        let r = read(&p, None).unwrap();
        assert!(r.truncated);
        assert_eq!(r.chunks.len(), 3);
        assert_eq!(r.data[0], all[..300]);
        // a corrupt (bit-flipped) last chunk is dropped too
        let p2 = tmp("flip.h8r");
        write_file(&p2, 2);
        let mut bytes = fs::read(&p2).unwrap();
        let n = bytes.len();
        bytes[n - 10] ^= 0xff;
        fs::write(&p2, bytes).unwrap();
        let r = read(&p2, None).unwrap();
        assert!(r.truncated);
        assert_eq!(r.data[0].len(), 100);
    }

    #[test]
    fn writer_thread_writes_chunks_and_stops() {
        let dir = std::env::temp_dir().join(format!("h8r-writer-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let sink = spawn_writer();
        sink.send(RecMsg::Limits(RecLimits {
            dir: dir.clone(),
            auto: true,
            max_bytes: 1 << 30,
            keep_free: 0,
        }));
        sink.send(RecMsg::Meta([[1.0; 3]; CHANNELS], "probe".into()));
        sink.send(RecMsg::Start(header()));
        for k in 0..10u16 {
            let mut r: [Vec<u16>; CHANNELS] = Default::default();
            r[0] = (0..1000).map(|i| k * 1000 + i).collect();
            r[3] = vec![7; 1000];
            sink.send(RecMsg::Rows(r));
        }
        sink.send(RecMsg::Stop);
        let t = Instant::now();
        let file = loop {
            let s = sink.shared.status.lock().unwrap().clone();
            if s.rows == 10_000 {
                break s.file.unwrap();
            }
            assert!(
                t.elapsed() < Duration::from_secs(5),
                "writer did not finish: {s:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        std::thread::sleep(Duration::from_millis(50));
        let r = read(&file, None).unwrap();
        assert_eq!(r.data[0].len(), 10_000);
        assert_eq!(r.data[0][9_999], 9_999);
        assert_eq!(r.header.probes, "probe");
        assert!(file
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("stream_2025-11-21_"));
        assert_eq!(sink.shared.lost.load(Ordering::Relaxed), 0);
    }
}

// ───────────────────────────── disk speed test ─────────────────────────────

/// Result of a quick write test of a folder.
#[derive(Clone, Debug)]
pub struct DiskBench {
    pub dir: PathBuf,
    /// (sequential write incl. sync, MB/s; median latency of a small write + sync, ms) or why it failed
    pub res: std::result::Result<(f64, f64), String>,
    pub took: Duration,
}

/// Bytes/s the streams need at most (8 channels × 440 S/s × 2 B) and full-rate captures (2.4 MS/s × 2 B).
pub const NEED_STREAM_BPS: f64 = 8.0 * 440.0 * 2.0;
pub const NEED_FULL_BPS: f64 = 2.4e6 * 2.0;

/// Lowest CPU and I/O priority for the calling thread (Linux), so a test never competes with the scope or
/// the recording.
fn background_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 19);
        libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13); // IOPRIO_WHO_PROCESS, this thread, idle class
    }
}

/// Write 4 MB (+ sync, so it measures the disk, not the page cache) and 3 small synced writes into `dir`,
/// then delete the test file. Short: ~50 ms on an SSD, ~2 s on a slow USB stick.
pub fn bench_dir(dir: &Path) -> DiskBench {
    let t0 = Instant::now();
    let p = dir.join(".h8r-speedtest.tmp");
    let run = || -> std::result::Result<(f64, f64), String> {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create the folder: {e}"))?;
        let mut f = File::create(&p).map_err(|e| format!("not writable: {e}"))?;
        let block = vec![0xA5u8; 256 << 10];
        let t = Instant::now();
        for _ in 0..16 {
            f.write_all(&block)
                .map_err(|e| format!("write failed: {e}"))?;
        }
        f.sync_all().map_err(|e| format!("sync failed: {e}"))?;
        let mb_s = (16 * block.len()) as f64 / 1e6 / t.elapsed().as_secs_f64().max(1e-6);
        let mut lat = Vec::new();
        for _ in 0..3 {
            let t = Instant::now();
            f.write_all(&block[..4096])
                .map_err(|e| format!("write failed: {e}"))?;
            f.sync_data().map_err(|e| format!("sync failed: {e}"))?;
            lat.push(t.elapsed().as_secs_f64() * 1e3);
        }
        lat.sort_by(|a, b| a.total_cmp(b));
        Ok((mb_s, lat[1]))
    };
    let res = run();
    let _ = fs::remove_file(&p);
    DiskBench {
        dir: dir.to_path_buf(),
        res,
        took: t0.elapsed(),
    }
}

/// Run `bench_dir` on a background thread with the lowest priority; the result lands in `out`.
pub fn spawn_bench(dir: PathBuf, out: Arc<Mutex<Option<DiskBench>>>) {
    let _ = std::thread::Builder::new()
        .name("disk-bench".into())
        .spawn(move || {
            background_priority();
            let b = bench_dir(&dir);
            log::info!(
                "disk test {}: {:?} in {:.0} ms",
                dir.display(),
                b.res,
                b.took.as_secs_f64() * 1e3
            );
            if let Ok(mut o) = out.lock() {
                *o = Some(b);
            }
        });
}

#[cfg(test)]
mod spool_tests {
    use super::*;

    #[test]
    fn spool_defers_to_ram_and_drops_only_at_its_limit() {
        let sink = RecSink {
            spool: Arc::new(Spool {
                q: Mutex::new(VecDeque::new()),
                cv: Condvar::new(),
                bytes: AtomicU64::new(0),
                max_bytes: AtomicU64::new(1000),
                in_total: AtomicU64::new(0),
            }),
            shared: Arc::new(RecShared::default()),
        };
        let rows = || {
            let mut r: [Vec<u16>; CHANNELS] = Default::default();
            r[0] = vec![1; 100]; // 200 bytes
            RecMsg::Rows(r)
        };
        for _ in 0..5 {
            sink.send(rows()); // nobody drains: a stalled disk
        }
        assert_eq!(sink.spool.bytes.load(Ordering::Relaxed), 1000);
        sink.send(rows()); // over the limit → dropped and counted
        sink.send(RecMsg::Stop); // control messages always pass
        assert_eq!(sink.shared.lost.load(Ordering::Relaxed), 1);
        assert!(matches!(
            sink.spool.pop(Duration::ZERO),
            Ok(RecMsg::Rows(_))
        ));
        assert_eq!(sink.spool.bytes.load(Ordering::Relaxed), 800);
    }

    #[test]
    fn bench_measures_a_folder_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("h8r-bench-{}", std::process::id()));
        let b = bench_dir(&dir);
        let (mb_s, sync_ms) = b.res.expect("temp dir writable");
        assert!(mb_s > 0.0 && sync_ms >= 0.0);
        assert!(!dir.join(".h8r-speedtest.tmp").exists());
        let _ = fs::remove_dir_all(&dir);
        // a path below a file is not a folder → reported, not panicking
        let f = std::env::temp_dir().join(format!("h8r-notdir-{}", std::process::id()));
        fs::write(&f, b"x").unwrap();
        assert!(bench_dir(&f.join("sub")).res.is_err());
        let _ = fs::remove_file(&f);
    }
}
