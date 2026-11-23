//! Files: recording to disk (settings + status), export (CSV / WAV) and opening recordings again.
//! A child module of `ui` (`#[path]` in ui.rs) so it can extend `App`; ui.rs only calls `files_tick`,
//! `files_settings` and `files_notice`.

use super::*;
use crate::rec::{self, ChunkInfo, RecLimits, RecMsg, Recording};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum ExportFmt {
    Csv,
    Wav,
    Mf4,
    Sr,
}

impl ExportFmt {
    fn ext(self) -> &'static str {
        match self {
            ExportFmt::Csv => "csv",
            ExportFmt::Wav => "wav",
            ExportFmt::Mf4 => "mf4",
            ExportFmt::Sr => "sr",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum ExportSrc {
    /// what is on screen (the shown capture, or the shown part of the stream)
    Shown,
    /// the whole stream recording in memory
    Stream,
}

enum JobResult {
    Exported(std::result::Result<PathBuf, String>),
    Loaded(std::result::Result<(PathBuf, Recording), String>),
}

struct Job {
    what: String,
    progress: Arc<AtomicU32>, // ‰
    result: Arc<Mutex<Option<JobResult>>>,
}

pub(super) struct Files {
    dir: String,
    auto: bool,
    max_gb: f32,
    keep_free_gb: f32,
    fmt: ExportFmt,
    pub(super) src: ExportSrc,
    open_path: String,
    sent_limits: Option<RecLimits>,
    sent_meta: Option<String>,
    saved: String,
    pub(super) export_req: bool,
    job: Option<Job>,
    msg: Option<(Level, String, Instant)>,
    /// RAM buffer for a slow disk (MB)
    ram_mb: u32,
    /// disk test: result, running flag, the folder it ran for, when the folder text last changed
    bench: Arc<Mutex<Option<rec::DiskBench>>>,
    bench_for: Option<String>,
    bench_started: Option<Instant>,
    dir_seen: String,
    dir_changed: Instant,
    boot: Instant,
    /// folder listing + free space, refreshed in the background while Settings is open
    folder: Arc<Mutex<FolderStats>>,
    folder_at: Option<Instant>,
    /// the opened recording file: its whole index (overview), and a part being loaded for browsing
    opened: Option<Opened>,
    /// row to show once the part being loaded arrives
    pending_row: Option<u64>,
    pub(super) events: EventLog,
    /// a folder / limit field is being edited this frame (set by the Settings window)
    editing: bool,
    /// screenshot requested (F12 / plot menu) and its result from the encoder thread
    pub(super) shot_req: bool,
    /// frames left before the requested screenshot is taken (the menu closes first)
    pub(super) shot_wait: u8,
    /// native file dialog running in a thread → (what for, picked path / cancelled / no dialog tool)
    browse: Arc<Mutex<Option<(BrowseFor, std::result::Result<Option<PathBuf>, String>)>>>,
    browse_busy: bool,
    /// reference library listing, refreshed at most every 2 s while the References menu is open
    pub(super) ref_lib: (Option<Instant>, Vec<PathBuf>),
    /// the main menu is open (keeps the recent-files list fresh)
    menu_open: bool,
    shot: Arc<Mutex<Option<std::result::Result<PathBuf, String>>>>,
}

/// An opened recording: only a window of it is in memory, the chunk index covers the whole file.
pub(super) struct Opened {
    path: PathBuf,
    started: Option<SystemTime>,
    chunks: Vec<ChunkInfo>,
    /// channel of each min/max column in the chunk index
    chans: Vec<usize>,
    total_rows: u64,
    rate: f64,
    /// per-pixel-column min/max per channel for the overview strip: (width, cols[chan][px])
    cols: Option<(usize, Vec<Vec<(u16, u16)>>)>,
}

#[derive(Default)]
struct FolderStats {
    files: Vec<(PathBuf, u64)>,
    free: Option<u64>,
}

fn files_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("files.txt"))
}

impl Files {
    pub(super) fn load() -> Files {
        let mut f = Files {
            dir: rec::default_dir().display().to_string(),
            auto: true,
            max_gb: 20.0,
            keep_free_gb: 2.0,
            fmt: ExportFmt::Csv,
            src: ExportSrc::Shown,
            open_path: String::new(),
            sent_limits: None,
            sent_meta: None,
            saved: String::new(),
            export_req: false,
            job: None,
            msg: None,
            ram_mb: (rec::SPOOL_DEFAULT >> 20) as u32,
            bench: Arc::new(Mutex::new(None)),
            bench_for: None,
            bench_started: None,
            dir_seen: String::new(),
            dir_changed: Instant::now(),
            boot: Instant::now(),
            folder: Arc::new(Mutex::new(FolderStats::default())),
            folder_at: None,
            opened: None,
            pending_row: None,
            events: EventLog::default(),
            editing: false,
            shot_req: false,
            shot_wait: 0,
            browse: Arc::new(Mutex::new(None)),
            browse_busy: false,
            ref_lib: (None, Vec::new()),
            menu_open: false,
            shot: Arc::default(),
        };
        if let Some(t) = files_path().and_then(|p| std::fs::read_to_string(p).ok()) {
            for l in t.lines() {
                let Some((k, v)) = l.split_once('=') else {
                    continue;
                };
                match k {
                    "rec_dir" if !v.trim().is_empty() => f.dir = v.trim().to_string(),
                    "rec_auto" => f.auto = v.trim() == "1",
                    "rec_max_gb" => {
                        f.max_gb = v.trim().parse().unwrap_or(20.0f32).clamp(0.1, 100_000.0)
                    }
                    "rec_keep_free_gb" => {
                        f.keep_free_gb = v.trim().parse().unwrap_or(2.0f32).clamp(0.0, 10_000.0)
                    }
                    "export_fmt" => {
                        f.fmt = [ExportFmt::Wav, ExportFmt::Mf4, ExportFmt::Sr]
                            .into_iter()
                            .find(|x| x.ext() == v.trim())
                            .unwrap_or(ExportFmt::Csv)
                    }
                    "export_src" => {
                        f.src = if v.trim() == "stream" {
                            ExportSrc::Stream
                        } else {
                            ExportSrc::Shown
                        }
                    }
                    "open_path" => f.open_path = v.trim().to_string(),
                    "rec_ram_mb" => {
                        f.ram_mb = v.trim().parse().unwrap_or(256u32).clamp(16, 16 * 1024)
                    }
                    _ => {}
                }
            }
        }
        f.saved = f.encode();
        // debug aid like HANTEK_SHOT: HANTEK_EVENTLOG=1 starts the event log (the switch itself is not saved:
        // a long run is started on purpose)
        f.events.on = std::env::var_os("HANTEK_EVENTLOG").is_some();
        f
    }

    fn encode(&self) -> String {
        format!(
            "rec_dir={}\nrec_auto={}\nrec_max_gb={}\nrec_keep_free_gb={}\nexport_fmt={}\nexport_src={}\nopen_path={}\nrec_ram_mb={}\n",
            self.dir,
            self.auto as u8,
            self.max_gb,
            self.keep_free_gb,
            self.fmt.ext(),
            if self.src == ExportSrc::Stream { "stream" } else { "shown" },
            self.open_path,
            self.ram_mb
        )
    }

    /// The recording / export folder (Settings).
    pub(super) fn dir(&self) -> PathBuf {
        PathBuf::from(&self.dir)
    }

    fn limits(&self) -> RecLimits {
        RecLimits {
            dir: PathBuf::from(&self.dir),
            auto: self.auto,
            max_bytes: (self.max_gb as f64 * 1e9) as u64,
            keep_free: (self.keep_free_gb as f64 * 1e9) as u64,
        }
    }
}

/// What an export writes: raw columns plus their conversion to units.
struct ExportData {
    names: Vec<String>,
    cols: Vec<Vec<u16>>,
    /// (zero counts, units per count) per column
    conv: Vec<(f32, f32)>,
    t0: f64,
    dt: f64,
    note: String,
}

fn write_csv(d: &ExportData, path: &PathBuf, prog: &AtomicU32) -> std::io::Result<()> {
    let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    writeln!(w, "# hantek1008c export: {}", d.note)?;
    writeln!(w, "# {} samples/s per channel", 1.0 / d.dt)?;
    write!(w, "time_s")?;
    for n in &d.names {
        write!(w, ",{n}")?;
    }
    writeln!(w)?;
    let rows = d.cols.iter().map(Vec::len).min().unwrap_or(0);
    // decimals: one more than the resolution of a count step (no float noise like 0.099999994)
    let prec: Vec<usize> = d
        .conv
        .iter()
        .map(|(_, k)| {
            ((-(k.abs().max(1e-9) as f64).log10() - 1e-4).ceil() + 1.0).clamp(0.0, 9.0) as usize
        })
        .collect();
    for i in 0..rows {
        write!(w, "{:.9}", d.t0 + i as f64 * d.dt)?;
        for ((c, (z, k)), &p) in d.cols.iter().zip(&d.conv).zip(&prec) {
            write!(w, ",{:.p$}", (c[i] as f64 - *z as f64) * *k as f64)?;
        }
        writeln!(w)?;
        if i % 65_536 == 0 {
            prog.store(
                (i as u64 * 1000 / rows.max(1) as u64) as u32,
                Ordering::Relaxed,
            );
        }
    }
    w.flush()
}

/// Multi-channel 16-bit PCM; sample = (counts − zero) × 8, the units per LSB are in the INFO comment.
/// Unit part of an export column name ("CH1_V" → "V").
fn col_unit(name: &str) -> &str {
    name.rsplit_once('_').map_or("", |x| x.1)
}
fn col_name(name: &str) -> &str {
    name.rsplit_once('_').map_or(name, |x| x.0)
}

/// sigrok session file (`.sr`, zip format 2) with analog channels as float32 — opens in PulseView / sigrok-cli.
/// Stored (uncompressed) zip: no compression library needed.
fn write_sr(d: &ExportData, path: &PathBuf, prog: &AtomicU32) -> std::io::Result<()> {
    let rows = d.cols.iter().map(Vec::len).min().unwrap_or(0);
    let rate = (1.0 / d.dt).round().max(1.0) as u64;
    let mut meta = format!(
        "[global]\nsigrok version=0.5.2\n\n[device 1]\nsamplerate={rate}\ntotal analog={}\n",
        d.cols.len()
    );
    for (k, n) in d.names.iter().enumerate() {
        meta += &format!("analog{}={}\n", k + 1, col_name(n));
    }
    let mut files: Vec<(String, Vec<u8>)> = vec![
        ("version".into(), b"2".to_vec()),
        ("metadata".into(), meta.into_bytes()),
    ];
    for (k, col) in d.cols.iter().enumerate() {
        let (z, u) = d.conv[k];
        let mut b = Vec::with_capacity(rows * 4);
        for &v in &col[..rows] {
            b.extend_from_slice(&((v as f32 - z) * u).to_le_bytes());
        }
        files.push((format!("analog-1-{}-1", k + 1), b));
        prog.store(
            ((k + 1) * 900 / d.cols.len().max(1)) as u32,
            Ordering::Relaxed,
        );
    }
    let mut w = BufWriter::new(std::fs::File::create(path)?);
    let mut central = Vec::new();
    let mut off = 0u32;
    for (name, data) in &files {
        let crc = crc32fast::hash(data);
        let local = |sig: u32| {
            let mut h = Vec::new();
            h.extend_from_slice(&sig.to_le_bytes());
            h
        };
        // local file header
        let mut h = local(0x0403_4b50);
        h.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // version, flags, method=stored, time, date
        for v in [crc, data.len() as u32, data.len() as u32] {
            h.extend_from_slice(&v.to_le_bytes());
        }
        h.extend_from_slice(&(name.len() as u16).to_le_bytes());
        h.extend_from_slice(&0u16.to_le_bytes());
        h.extend_from_slice(name.as_bytes());
        w.write_all(&h)?;
        w.write_all(data)?;
        // central directory entry
        let mut c = local(0x0201_4b50);
        c.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // made by, needed, flags, method, time, date
        for v in [crc, data.len() as u32, data.len() as u32] {
            c.extend_from_slice(&v.to_le_bytes());
        }
        c.extend_from_slice(&(name.len() as u16).to_le_bytes());
        c.extend_from_slice(&[0; 8]); // extra len, comment len, disk, internal attrs
        c.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        c.extend_from_slice(&off.to_le_bytes());
        c.extend_from_slice(name.as_bytes());
        central.push(c);
        off = off
            .checked_add((h.len() + data.len()) as u32)
            .ok_or_else(|| {
                std::io::Error::other("too big for a .sr file (4 GB) — export a shorter range")
            })?;
    }
    let cd_size: usize = central.iter().map(Vec::len).sum();
    for c in &central {
        w.write_all(c)?;
    }
    let mut end = 0x0605_4b50u32.to_le_bytes().to_vec();
    end.extend_from_slice(&[0, 0, 0, 0]);
    end.extend_from_slice(&(files.len() as u16).to_le_bytes());
    end.extend_from_slice(&(files.len() as u16).to_le_bytes());
    end.extend_from_slice(&(cd_size as u32).to_le_bytes());
    end.extend_from_slice(&off.to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes());
    w.write_all(&end)?;
    w.flush()
}

/// ASAM MDF 4.10 (`.mf4`), the automotive measurement format (asammdf, CANape, INCA, …): one data group, a float64
/// time master channel and one uint16 channel per scope channel with a linear conversion to its unit.
fn write_mf4(d: &ExportData, path: &PathBuf, prog: &AtomicU32) -> std::io::Result<()> {
    let rows = d.cols.iter().map(Vec::len).min().unwrap_or(0);
    let rec = 8 + 2 * d.cols.len();
    let mut b: Vec<u8> = Vec::new();
    // ID block (64 bytes)
    b.extend_from_slice(b"MDF     4.10    hantek  ");
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&410u16.to_le_bytes());
    b.extend_from_slice(&[0; 34]);
    // generic block: "##XX", reserved, length, link count, links, data (8-byte aligned)
    fn block(b: &mut Vec<u8>, id: &[u8; 4], links: &[u64], data: &[u8]) -> u64 {
        let off = b.len() as u64;
        let pad = (8 - data.len() % 8) % 8;
        b.extend_from_slice(b"##");
        b.extend_from_slice(&id[2..]);
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&((24 + 8 * links.len() + data.len() + pad) as u64).to_le_bytes());
        b.extend_from_slice(&(links.len() as u64).to_le_bytes());
        for l in links {
            b.extend_from_slice(&l.to_le_bytes());
        }
        b.extend_from_slice(data);
        b.extend(std::iter::repeat_n(0, pad));
        off
    }
    fn link(b: &mut [u8], blk: u64, i: usize, to: u64) {
        let at = blk as usize + 24 + 8 * i;
        b[at..at + 8].copy_from_slice(&to.to_le_bytes());
    }
    fn tx(b: &mut Vec<u8>, s: &str) -> u64 {
        let mut t = s.as_bytes().to_vec();
        t.push(0);
        block(b, b"##TX", &[], &t)
    }
    // HD: start time
    let start_ns = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |x| x.as_secs_f64())
        * 1e9) as u64;
    let mut hd = start_ns.to_le_bytes().to_vec();
    hd.extend_from_slice(&[0; 8]); // tz, dst, time flags, class, flags, reserved
    hd.extend_from_slice(&[0; 16]); // start angle, distance
    let hd_at = block(&mut b, b"##HD", &[0; 6], &hd);
    let dg_at = block(&mut b, b"##DG", &[0; 4], &[0; 8]);
    link(&mut b, hd_at, 0, dg_at);
    let mut cg = 0u64.to_le_bytes().to_vec(); // record id
    cg.extend_from_slice(&(rows as u64).to_le_bytes()); // cycle count
    cg.extend_from_slice(&[0; 8]); // flags, path separator, reserved
    cg.extend_from_slice(&(rec as u32).to_le_bytes()); // data bytes
    cg.extend_from_slice(&0u32.to_le_bytes()); // invalidation bytes
    let cg_at = block(&mut b, b"##CG", &[0; 6], &cg);
    link(&mut b, dg_at, 1, cg_at);
    let cn_data = |kind: u8, sync: u8, dtype: u8, byte_off: u32, bits: u32| {
        let mut c = vec![kind, sync, dtype, 0];
        c.extend_from_slice(&byte_off.to_le_bytes());
        c.extend_from_slice(&bits.to_le_bytes());
        c.extend_from_slice(&[0; 12]); // flags, invalidation bit, precision, reserved, attachment count
        c.extend_from_slice(&[0; 48]); // value range, limits, extended limits
        c
    };
    // time master: float64 seconds, t = t0 + i·dt via a linear conversion of the record index? — stored directly
    let t_name = tx(&mut b, "time");
    let t_unit = tx(&mut b, "s");
    let t_cn = block(
        &mut b,
        b"##CN",
        &[0, 0, t_name, 0, 0, 0, t_unit, 0],
        &cn_data(2, 1, 4, 0, 64),
    );
    link(&mut b, cg_at, 1, t_cn);
    let mut prev = t_cn;
    for (k, n) in d.names.iter().enumerate() {
        let (z, u) = d.conv[k];
        let name = tx(&mut b, col_name(n));
        let unit = tx(&mut b, col_unit(n));
        // linear conversion: phys = p1 + p2·raw = (raw − zero)·units_per_count
        let mut cc = vec![1u8, 0, 0, 0, 0, 0, 2, 0]; // type linear, precision, flags, ref count, val count = 2
        cc.extend_from_slice(&[0; 16]); // physical range
        cc.extend_from_slice(&(-(z as f64) * u as f64).to_le_bytes());
        cc.extend_from_slice(&(u as f64).to_le_bytes());
        let cc_at = block(&mut b, b"##CC", &[0, unit, 0, 0], &cc);
        let cn = block(
            &mut b,
            b"##CN",
            &[0, 0, name, 0, cc_at, 0, unit, 0],
            &cn_data(0, 0, 0, 8 + 2 * k as u32, 16),
        );
        link(&mut b, prev, 0, cn);
        prev = cn;
    }
    // DT: header now, records streamed
    let dt_at = b.len() as u64;
    let body = (rows * rec) as u64;
    let pad = (8 - body % 8) % 8;
    link(&mut b, dg_at, 2, dt_at);
    b.extend_from_slice(b"##DT");
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&(24 + body).to_le_bytes());
    b.extend_from_slice(&0u64.to_le_bytes());
    let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    w.write_all(&b)?;
    let mut r = Vec::with_capacity(rec);
    for i in 0..rows {
        r.clear();
        r.extend_from_slice(&(d.t0 + i as f64 * d.dt).to_le_bytes());
        for c in &d.cols {
            r.extend_from_slice(&c[i].to_le_bytes());
        }
        w.write_all(&r)?;
        if i % 65536 == 0 {
            prog.store(
                (i as u64 * 1000 / rows.max(1) as u64) as u32,
                Ordering::Relaxed,
            );
        }
    }
    w.write_all(&vec![0; pad as usize])?;
    w.flush()
}

fn write_wav(d: &ExportData, path: &PathBuf, prog: &AtomicU32) -> std::io::Result<()> {
    let rows = d.cols.iter().map(Vec::len).min().unwrap_or(0);
    let nch = d.cols.len().max(1) as u32;
    let data_len = rows as u64 * nch as u64 * 2;
    if data_len > u32::MAX as u64 - 4096 {
        return Err(std::io::Error::other(
            "too long for WAV (4 GB) — use CSV or a shorter range",
        ));
    }
    let comment = format!(
        "{} | {} | t0 {} s",
        d.note,
        d.names
            .iter()
            .zip(&d.conv)
            .map(|(n, (_, k))| format!("{n}: {:e} per LSB", k / 8.0))
            .collect::<Vec<_>>()
            .join(", "),
        d.t0
    );
    let mut cm = comment.into_bytes();
    cm.push(0);
    if cm.len() % 2 == 1 {
        cm.push(0);
    }
    let list_len = 4 + 8 + cm.len() as u32;
    let rate = (1.0 / d.dt).round().clamp(1.0, u32::MAX as f64) as u32;
    let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    w.write_all(b"RIFF")?;
    w.write_all(&(4 + 24 + 8 + list_len + 8 + data_len as u32).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(&(nch as u16).to_le_bytes())?;
    w.write_all(&rate.to_le_bytes())?;
    w.write_all(&(rate * nch * 2).to_le_bytes())?;
    w.write_all(&((nch * 2) as u16).to_le_bytes())?;
    w.write_all(&16u16.to_le_bytes())?;
    w.write_all(b"LIST")?;
    w.write_all(&list_len.to_le_bytes())?;
    w.write_all(b"INFOICMT")?;
    w.write_all(&(cm.len() as u32).to_le_bytes())?;
    w.write_all(&cm)?;
    w.write_all(b"data")?;
    w.write_all(&(data_len as u32).to_le_bytes())?;
    for i in 0..rows {
        for (c, (z, _)) in d.cols.iter().zip(&d.conv) {
            let v = ((c[i] as f32 - z) * 8.0).round().clamp(-32768.0, 32767.0) as i16;
            w.write_all(&v.to_le_bytes())?;
        }
        if i % 65_536 == 0 {
            prog.store(
                (i as u64 * 1000 / rows.max(1) as u64) as u32,
                Ordering::Relaxed,
            );
        }
    }
    w.flush()
}

fn gb(b: u64) -> String {
    format!("{:.2} GB", b as f64 / 1e9)
}

impl App {
    /// Every frame: send recording settings to the writer when changed, save files.txt, finish jobs,
    /// run a requested export with the shown snapshot.
    pub(super) fn files_tick(&mut self, snap: &Snapshot) {
        let menu_open = std::mem::take(&mut self.files.menu_open);
        // debug aid like HANTEK_SHOT: HANTEK_OPEN=file.h8r opens a recording at start
        static OPEN_ENV: std::sync::Once = std::sync::Once::new();
        let mut env_path = None;
        OPEN_ENV.call_once(|| env_path = std::env::var_os("HANTEK_OPEN"));
        {
            if let Some(p) = env_path {
                self.open_start(PathBuf::from(p));
            }
        }
        let f = &mut self.files;
        self.acq
            .rec
            .spool
            .max_bytes
            .store((f.ram_mb as u64) << 20, Ordering::Relaxed);
        // disk test of the recording folder: 3 s after start (never delays start-up or the first captures),
        // and again 1 s after the folder text stops changing; background thread, lowest priority
        if f.dir != f.dir_seen {
            f.dir_seen = f.dir.clone();
            f.dir_changed = Instant::now();
        }
        let running = f.bench_started.is_some()
            && f.bench
                .lock()
                .map_or(true, |b| b.as_ref().is_none_or(|b| b.dir != f.dir));
        if f.bench_for.as_deref() != Some(f.dir.as_str())
            && f.boot.elapsed() > Duration::from_secs(3)
            && f.dir_changed.elapsed() > Duration::from_secs(1)
            && !f.editing
            && (f.auto || f.events.on || self.settings_open) // only when something would write there
            && !running
        {
            if let Ok(mut b) = f.bench.lock() {
                *b = None;
            }
            f.bench_for = Some(f.dir.clone());
            f.bench_started = Some(Instant::now());
            rec::spawn_bench(PathBuf::from(&f.dir), f.bench.clone());
        }
        if (self.settings_open || menu_open)
            && f.folder_at
                .is_none_or(|t| t.elapsed() > Duration::from_secs(2))
        {
            f.folder_at = Some(Instant::now());
            let (dir, out) = (PathBuf::from(&f.dir), f.folder.clone());
            let _ = std::thread::Builder::new()
                .name("folder-stats".into())
                .spawn(move || {
                    let files = rec::list(&dir);
                    let free =
                        rec::free_bytes(&dir).or_else(|| dir.parent().and_then(rec::free_bytes));
                    if let Ok(mut o) = out.lock() {
                        *o = FolderStats { files, free };
                    }
                });
        }
        // folder / limits apply when editing is done: no half-typed folders, no deletes while dragging "folder max"
        let editing = std::mem::take(&mut self.files.editing);
        let l = if editing {
            self.files
                .sent_limits
                .clone()
                .unwrap_or_else(|| self.files.limits())
        } else {
            self.files.limits()
        };
        if self.files.sent_limits.as_ref() != Some(&l) {
            self.acq.rec.send(RecMsg::Limits(l.clone()));
            self.files.sent_limits = Some(l);
        }
        if self.settings_open || self.files.sent_meta.is_none() {
            let meta = self
                .probes
                .iter()
                .map(Probe::encode)
                .collect::<Vec<_>>()
                .join(",");
            if self.files.sent_meta.as_ref() != Some(&meta) {
                self.acq.rec.send(RecMsg::Meta(self.cal.gain, meta.clone()));
                self.files.sent_meta = Some(meta);
            }
            let enc = self.files.encode();
            if enc != self.files.saved {
                if let Some(p) = files_path() {
                    let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
                    if std::fs::write(&p, &enc).is_ok() {
                        self.files.saved = enc;
                    }
                }
            }
        }
        if std::mem::take(&mut self.files.export_req) {
            self.export_start(snap);
        }
        let done = self
            .files
            .job
            .as_ref()
            .and_then(|j| j.result.lock().ok()?.take().map(|r| (j.what.clone(), r)));
        if let Some((what, r)) = done {
            self.files.job = None;
            match r {
                JobResult::Exported(Ok(p)) => {
                    self.files_msg(Level::Info, format!("{what}: saved {}", p.display()))
                }
                JobResult::Exported(Err(e)) => {
                    self.files_msg(Level::Warn, format!("{what} failed: {e}"))
                }
                JobResult::Loaded(Ok((p, r))) => self.open_apply(p, r),
                JobResult::Loaded(Err(e)) => {
                    self.files_msg(Level::Warn, format!("{what} failed: {e}"))
                }
            }
        }
    }

    pub(super) fn files_msg(&mut self, l: Level, m: String) {
        self.files.msg = Some((l, m, Instant::now()));
    }

    /// Info-line notices: job progress, results, recording problems.
    pub(super) fn files_notice(&self, v: &mut Vec<(Level, String)>) {
        self.event_notice(v);
        if let Some(j) = &self.files.job {
            v.push((
                Level::Info,
                format!(
                    "{}… {:.0} %",
                    j.what,
                    j.progress.load(Ordering::Relaxed) as f32 / 10.0
                ),
            ));
        }
        if let Some((l, m, at)) = &self.files.msg {
            if at.elapsed() < Duration::from_secs(8) {
                v.push((*l, m.clone()));
            }
        }
        if let Ok(s) = self.acq.rec.shared.status.lock() {
            if let Some(p) = &s.problem {
                v.push((Level::Warn, format!("Recording: {p}")));
            }
        }
        let lost = self.acq.rec.shared.lost.load(Ordering::Relaxed);
        if lost > 0 {
            v.push((Level::Warn, format!("Recording: the RAM buffer for the disk was full, {lost} blocks of stream data are missing in the file. Faster folder or bigger RAM buffer: Settings (F2).")));
        }
        // watchdog: a stalled or too slow disk (data is kept in RAM meanwhile)
        let since = self.acq.rec.shared.write_since_ms.load(Ordering::Relaxed);
        let stalled_s = if since > 0 {
            rec::now_ms().saturating_sub(since) as f64 / 1e3
        } else {
            0.0
        };
        if let Ok(h) = self.acq.rec.shared.health.lock() {
            let ram = format!(
                "{:.1} of {} MB RAM",
                h.backlog as f64 / 1048576.0,
                h.max_backlog >> 20
            );
            if stalled_s > 2.0 {
                v.push((Level::Warn, format!("Recording: the disk has not answered for {stalled_s:.0} s — stream data is kept in RAM ({ram}), nothing lost so far.")));
            } else if h.growing_s >= 3 && h.behind_s > 3.0 {
                let full = h.full_in_s.map_or(String::new(), |t| {
                    format!(", RAM buffer full in ~{}", fmt_time(t))
                });
                v.push((Level::Warn, format!("Recording: the disk is slower than the data — {:.0} s buffered in RAM ({ram}{full}). Choose a faster folder in Settings (F2).", h.behind_s)));
            }
        }
        // disk test result for the recording folder
        if self.files.auto {
            if let Ok(b) = self.files.bench.lock() {
                match b.as_ref().map(|b| &b.res) {
                    Some(Err(e)) => v.push((Level::Warn, format!("Recording folder {}: {e} — streams can't be saved; choose another folder in Settings (F2).", self.files.dir))),
                    Some(Ok((mb_s, sync_ms))) if mb_s * 1e6 < 4.0 * rec::NEED_STREAM_BPS || *sync_ms > 2000.0 => v.push((
                        Level::Warn,
                        format!("Recording folder is very slow ({mb_s:.2} MB/s, sync {sync_ms:.0} ms): long streams will lag behind in RAM. Choose another folder in Settings (F2)."),
                    )),
                    _ => {}
                }
            }
            if self
                .files
                .bench_started
                .is_some_and(|t| t.elapsed() > Duration::from_secs(5))
                && self.files.bench.lock().is_ok_and(|b| b.is_none())
            {
                v.push((Level::Warn, format!("Recording folder {} does not respond (disk test running for > 5 s) — a sleeping USB drive or network folder?", self.files.dir)));
            }
        }
    }

    /// Settings > Memory: RAM buffer that bridges a slow / stalled disk (next to the history limit).
    pub(super) fn files_ram_setting(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            help(
                ui.add(egui::DragValue::new(&mut self.files.ram_mb).range(16..=16 * 1024).speed(8.0).prefix("RAM buffer for the disk ").suffix(" MB")),
                "When the disk is slow or stops answering for a while (sleeping USB drive, network folder, a busy laptop), stream data waits here instead of being lost, and the scope never waits for the disk. 256 MB ≈ 10 h of 8-channel streaming. Separate from the history memory limit.",
            );
            if let Ok(h) = self.acq.rec.shared.health.lock() {
                if h.in_bps > 0.0 || h.backlog > 0 {
                    ui.label(
                        egui::RichText::new(format!(
                            "in {:.1} kB/s · disk {} · waiting {:.1} MB · slowest write {:.0} ms",
                            h.in_bps / 1e3,
                            if h.disk_bps > 0.0 { format!("{:.1} MB/s", h.disk_bps / 1e6) } else { "–".into() },
                            h.backlog as f64 / 1048576.0,
                            h.worst_write_ms
                        ))
                        .weak(),
                    );
                }
            }
        });
    }

    /// Settings window: "Recording & files" section.
    pub(super) fn files_settings(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Recording to disk (stream mode)").strong());
        ui.horizontal(|ui| {
            help(ui.checkbox(&mut self.files.auto, "auto-store streams"), "Every stream recording is also written to a file in the folder below while it runs (crash-safe: at most the last 2 s are lost). Off: streams are kept in memory only.");
        });
        ui.horizontal(|ui| {
            ui.label("folder");
            help(
                {
                    let r = ui
                        .add(egui::TextEdit::singleline(&mut self.files.dir).desired_width(300.0));
                    self.files.editing |= r.has_focus();
                    r
                },
                "Where recordings (.h8r) and exports go. Created if missing.",
            );
        });
        ui.horizontal(|ui| {
            help(
                {
                    let r = ui.add(egui::DragValue::new(&mut self.files.max_gb).range(0.1..=100_000.0).speed(0.5).prefix("folder max ").suffix(" GB"));
                    self.files.editing |= r.dragged() || r.has_focus();
                    r
                },
                "All recordings in the folder together. Above it the oldest other recordings are deleted; if the current one alone is bigger, recording to disk stops. A 24 h stream with 8 channels ≈ 0.6 GB.",
            );
            help(
                ui.add(egui::DragValue::new(&mut self.files.keep_free_gb).range(0.0..=10_000.0).speed(0.1).prefix("keep free ").suffix(" GB")),
                "Recording to disk stops when the disk has less free space than this (Linux).",
            );
        });
        if let Ok(s) = self.acq.rec.shared.status.lock() {
            let txt = match &s.file {
                Some(f) => format!(
                    "last file: {} · {} · {} rows",
                    f.file_name()
                        .map_or(String::new(), |n| n.to_string_lossy().into()),
                    gb(s.bytes),
                    fmt_count(s.rows as f64)
                ),
                None => "no recording written yet".into(),
            };
            ui.label(egui::RichText::new(txt).weak());
        }
        let (files, free) = self
            .files
            .folder
            .lock()
            .map_or((Vec::new(), None), |f| (f.files.clone(), f.free));
        let used: u64 = files.iter().map(|f| f.1).sum();
        ui.label(
            egui::RichText::new(format!(
                "{} recordings, {}{}",
                files.len(),
                gb(used),
                free.map_or(String::new(), |f| format!(" · {} free on disk", gb(f)))
            ))
            .weak(),
        );
        // disk speed (background test) and the RAM buffer that bridges a slow / stalled disk
        ui.horizontal(|ui| {
            let (txt, col) = match self.files.bench.lock().ok().and_then(|b| b.clone()) {
                None if self.files.bench_started.is_some() => ("disk test running…".to_string(), Color32::GRAY),
                None => ("disk test in a moment…".to_string(), Color32::GRAY),
                Some(b) => match b.res {
                    Err(e) => (format!("disk: {e}"), Color32::from_rgb(255, 120, 100)),
                    Ok((mb_s, sync_ms)) => {
                        let verdict = if mb_s * 1e6 < 4.0 * rec::NEED_STREAM_BPS || sync_ms > 2000.0 {
                            ("too slow for long streams", Color32::from_rgb(255, 120, 100))
                        } else if mb_s * 1e6 < 2.0 * rec::NEED_FULL_BPS {
                            ("OK for streams; too slow for full-rate capture logging (~5 MB/s)", Color32::from_rgb(255, 200, 80))
                        } else {
                            ("fast enough for streams and full-rate logging", Color32::from_rgb(120, 220, 140))
                        };
                        (format!("disk: {mb_s:.1} MB/s, sync {sync_ms:.0} ms — {}", verdict.0), verdict.1)
                    }
                },
            };
            help(
                ui.add(egui::Label::new(egui::RichText::new(txt).color(col)).sense(egui::Sense::click())),
                "Quick write test of the folder (4 MB + 3 small synced writes, then deleted), in the background at lowest priority: 3 s after start and whenever the folder changes. Streams need ≤ 0.01 MB/s; full-rate capture logging ≈ 5 MB/s.",
            );
            if help(ui.small_button("test again"), "Run the disk test again (e.g. after plugging in a drive).").clicked() {
                self.files.bench_for = None;
                self.files.dir_changed = Instant::now() - Duration::from_secs(2);
            }
        });
        ui.separator();

        self.event_settings(ui);
        ui.separator();
        ui.label(egui::RichText::new("Open a recording").strong());
        ui.horizontal(|ui| {
            help(ui.add(egui::TextEdit::singleline(&mut self.files.open_path).desired_width(300.0).hint_text("path to a .h8r file")), "Full path of a recording. It opens in stream mode, stopped, browsable with the stream navigator (Run starts a new live stream).");
            if help(ui.add_enabled(self.files.job.is_none(), egui::Button::new("Open")), "Open the file above.").clicked() {
                self.open_start(PathBuf::from(self.files.open_path.trim()));
            }
            if help(ui.add_enabled(self.files.job.is_none() && !self.files.browse_busy, egui::Button::new("Browse…")), "Pick a recording (.h8r) anywhere with the system's file dialog.").clicked() {
                self.browse_start(ui.ctx(), BrowseFor::Recording);
            }
        });
        for (p, n) in files.iter().rev().take(6) {
            let name = p
                .file_name()
                .map_or(String::new(), |n| n.to_string_lossy().into());
            if help(
                ui.add_enabled(
                    self.files.job.is_none(),
                    egui::Button::new(format!("{name}  ({})", gb(*n))).small(),
                ),
                "Open this recording.",
            )
            .clicked()
            {
                self.files.open_path = p.display().to_string();
                self.open_start(p.clone());
            }
        }
        ui.label(
            egui::RichText::new(
                "Export: Menu > Export (format, what, CAN log) — files go into the folder above.",
            )
            .weak(),
        );
    }

    fn export_start(&mut self, snap: &Snapshot) {
        let chans: Vec<usize> = (0..CHANNELS).filter(|&c| self.on[c]).collect();
        let (data, cfg, zero, t0, dt, started, what) = if self.files.src == ExportSrc::Stream {
            let h = self.acq.history.lock().unwrap_or_else(|p| p.into_inner());
            let s = &h.stream;
            if s.rows() < 2 {
                drop(h);
                self.files_msg(
                    Level::Warn,
                    "Export: no stream recording in memory (choose a stream rate under Timebase)."
                        .into(),
                );
                return;
            }
            let data: Vec<(usize, Vec<u16>)> = chans
                .iter()
                .filter(|&&c| s.active[c])
                .map(|&c| (c, s.data[c].clone()))
                .collect();
            let rate = s.rate.max(1e-9);
            (
                data,
                s.cfg.clone(),
                s.zero,
                s.dropped as f64 / rate,
                1.0 / rate,
                s.started,
                "whole stream",
            )
        } else {
            let Some(cfg) = snap.cfg.clone() else {
                self.files_msg(Level::Warn, "Export: nothing on screen yet.".into());
                return;
            };
            let n = snap.frame.data.iter().map(Vec::len).max().unwrap_or(0);
            let data: Vec<(usize, Vec<u16>)> = chans
                .iter()
                .filter(|&&c| snap.frame.active[c] && snap.frame.data[c].len() >= 2)
                .map(|&c| (c, snap.frame.data[c].clone()))
                .collect();
            let (t0, dt, started) = if snap.roll {
                let h = self.acq.history.lock().unwrap_or_else(|p| p.into_inner());
                let rate = h.stream.rate.max(1e-9);
                let end = self
                    .stream_end
                    .unwrap_or(h.stream.dropped + h.stream.rows() as u64);
                (
                    end.saturating_sub(n as u64) as f64 / rate,
                    1.0 / rate,
                    h.stream.started,
                )
            } else {
                let dt = capture_seconds(cfg.timebase_id) / n.max(1) as f64;
                (-(cfg.trig_pos as f64 / 1000.0) * n as f64 * dt, dt, None)
            };
            (
                data,
                Some(cfg),
                snap.zero,
                t0,
                dt,
                started,
                if snap.roll {
                    "shown stream"
                } else {
                    "shown capture"
                },
            )
        };
        if data.is_empty() {
            self.files_msg(
                Level::Warn,
                "Export: none of the shown channels has data.".into(),
            );
            return;
        }
        let Some(cfg) = cfg else { return };
        let conv: Vec<(f32, f32)> = data
            .iter()
            .map(|(c, _)| {
                let vs = cfg.vscale[*c];
                self.uconv(*c, vs, zero[vs.index()][*c])
            })
            .collect();
        let names = data
            .iter()
            .map(|(c, _)| format!("CH{}_{}", c + 1, self.unit(*c)))
            .collect();
        let origin = if what == "shown capture" {
            "time 0 = trigger point"
        } else {
            "time 0 = stream start"
        };
        let note = format!(
            "{what}, {}, {origin}, channel deskew not applied",
            started.map_or_else(|| rec::utc(SystemTime::now()), rec::utc)
        );
        let ext = self.files.fmt.ext();
        let dir = PathBuf::from(&self.files.dir);
        let path = dir.join(format!(
            "export_{}_{}.{ext}",
            rec::stamp(SystemTime::now()),
            what.replace(' ', "_")
        ));
        let d = ExportData {
            names,
            cols: data.into_iter().map(|x| x.1).collect(),
            conv,
            t0,
            dt,
            note,
        };
        let job = Job {
            what: format!("Export {ext} ({what})"),
            progress: Arc::default(),
            result: Arc::default(),
        };
        let (prog, res, fmt) = (job.progress.clone(), job.result.clone(), self.files.fmt);
        self.files.job = Some(job);
        std::thread::spawn(move || {
            let r = std::fs::create_dir_all(&dir).and_then(|_| match fmt {
                ExportFmt::Csv => write_csv(&d, &path, &prog),
                ExportFmt::Wav => write_wav(&d, &path, &prog),
                ExportFmt::Mf4 => write_mf4(&d, &path, &prog),
                ExportFmt::Sr => write_sr(&d, &path, &prog),
            });
            *res.lock().unwrap() = Some(JobResult::Exported(
                r.map(|_| path).map_err(|e| e.to_string()),
            ));
        });
    }

    fn open_start(&mut self, path: PathBuf) {
        self.files.pending_row = None;
        self.open_window(path, None);
    }

    /// Load (a part of) a recording in the background: the newest part, or the part around `around`.
    fn open_window(&mut self, path: PathBuf, around: Option<u64>) {
        if path.as_os_str().is_empty() {
            return;
        }
        // load at most what the memory limit allows (newest part of the file)
        let budget = self.acq.history.lock().map_or(512 << 20, |h| h.budget) as u64;
        let job = Job {
            what: format!(
                "Opening {}",
                path.file_name()
                    .map_or(String::new(), |n| n.to_string_lossy().into())
            ),
            progress: Arc::default(),
            result: Arc::default(),
        };
        let res = job.result.clone();
        self.files.job = Some(job);
        std::thread::spawn(move || {
            // header + index first (cheap), then the part that fits the memory limit
            let r = rec::read_window(&path, Some(0), None).and_then(|r| {
                let nch = r.header.active.iter().filter(|&&a| a).count().max(1) as u64;
                rec::read_window(&path, Some(budget * 9 / 10 / (2 * nch)), around)
                // room for a few captures
            });
            *res.lock().unwrap() = Some(JobResult::Loaded(
                r.map(|r| (path, r)).map_err(|e| e.to_string()),
            ));
        });
    }

    /// Put an opened recording into the stream navigator (stream mode, stopped).
    fn open_apply(&mut self, path: PathBuf, r: Recording) {
        let window = self.files.pending_row.take();
        let h = &r.header;
        if h.capture.is_some() {
            self.files_msg(Level::Warn, format!("{}: an event log / reference file (separate captures) — it can't be shown as a stream yet.", path.display()));
            return;
        }
        if r.data.iter().all(Vec::is_empty) || h.rate <= 0.0 {
            self.files_msg(
                Level::Warn,
                format!("{}: no data in the file", path.display()),
            );
            return;
        }
        let mut cfg = Config {
            active: h.active,
            ..Config::default()
        };
        for c in 0..CHANNELS {
            cfg.vscale[c] = VScale::ALL
                .into_iter()
                .find(|v| v.id() == h.vscale[c])
                .unwrap_or(VScale::X1);
        }
        let loaded_rows = r.data.iter().map(Vec::len).max().unwrap_or(0) as u64;
        // raw min/max per channel of the loaded part, for fitting the traces on screen
        let raw_span: [Option<(u16, u16)>; CHANNELS] = std::array::from_fn(|c| {
            let d = &r.data[c];
            (!d.is_empty()).then(|| {
                d.iter()
                    .fold((u16::MAX, 0), |(lo, hi), &v| (lo.min(v), hi.max(v)))
            })
        });
        let vscale = cfg.vscale;
        let partial = r.first_row > 0 || r.first_row + loaded_rows < r.total_rows;
        let started = Some(UNIX_EPOCH + Duration::from_secs_f64(h.started_unix.max(0.0)));
        let same_file = self.files.opened.as_ref().is_some_and(|o| o.path == path);
        if !same_file {
            self.files.opened = Some(Opened {
                path: path.clone(),
                started,
                chunks: r.chunks.clone(),
                chans: (0..CHANNELS).filter(|&c| h.active[c]).collect(),
                total_rows: r.total_rows,
                rate: h.rate,
                cols: None,
            });
        }
        {
            let mut hist = self.acq.history.lock().unwrap_or_else(|p| p.into_inner());
            hist.stream = crate::acq::StreamRec {
                data: r.data,
                active: h.active,
                rate: h.rate,
                cfg: Some(cfg),
                zero: h.zero,
                started,
                dropped: r.first_row,
            };
            hist.fit_stream();
        }
        if let (Some(row), true) = (window, same_file) {
            // another part of the same file: keep every view setting, show the requested row
            let screen = self.stream_rows_per_screen() as u64;
            self.stream_end =
                Some((row + screen / 2).clamp(r.first_row + 2, r.first_row + loaded_rows));
            self.stream_cache = None;
            self.files_msg(
                Level::Info,
                format!(
                    "Showing {} … {} of the recording.",
                    fmt_time(r.first_row as f64 / h.rate),
                    fmt_time((r.first_row + loaded_rows) as f64 / h.rate)
                ),
            );
            return;
        }
        if !h.probes.is_empty() {
            h.probes
                .split(',')
                .take(CHANNELS)
                .enumerate()
                .for_each(|(i, s)| {
                    if let Some(p) = Probe::decode(s.trim()) {
                        self.probes[i] = p;
                    }
                });
        }
        self.on = h.active;
        // fit vertically (like auto-set, V/div + offset only), so an opened file is never off-screen
        let meas: [Option<Meas>; CHANNELS] = std::array::from_fn(|c| {
            let (lo, hi) = raw_span[c]?;
            let vs = vscale[c];
            let (zu, vu) = self.uconv(c, vs, h.zero[vs.index()][c]);
            let (a, b) = ((lo as f32 - zu) * vu, (hi as f32 - zu) * vu);
            Some(Meas {
                min: a.min(b),
                max: a.max(b),
                ..Default::default()
            })
        });
        let scale: [f32; CHANNELS] = std::array::from_fn(|c| self.probes[c].scale);
        let fit = autoset_pick(&meas, &scale);
        for c in 0..CHANNELS {
            if let (Some(i), Some(o)) = (fit.vdiv[c], fit.offset[c]) {
                (self.vdiv[c], self.offset[c]) = (i, o);
            }
        }
        self.roll = Some(h.roll_id);
        self.view = None;
        if self.run != Run::Stopped {
            self.set_run(Run::Stopped);
        }
        // a stream-capable timebase showing ~1000 samples per screen
        if self.tb_id < TB_STREAM_MIN || h.rate * self.span_s() < 50.0 {
            if let Some(id) = (TB_STREAM_MIN..=TB_MAX).find(|&id| {
                !TB_SKIP.contains(&id)
                    && h.rate * burst_ns_per_div(id) as f64 * 1e-9 * DIV_X as f64 >= 1000.0
            }) {
                self.tb_id = id;
            }
        }
        self.stream_end = None;
        self.stream_cache = None;
        let secs = loaded_rows as f64 / h.rate;
        let mut m = format!(
            "Opened {} — {} of stream, probes from the file. Run starts a new live stream.",
            path.display(),
            fmt_time(secs)
        );
        if partial {
            m += &format!(
                " {} of {} fit the memory limit — click the overview strip to see other parts.",
                fmt_time(secs),
                fmt_time(r.total_rows as f64 / h.rate)
            );
        }
        if r.truncated {
            m += " The file ended early (crash?) — everything up to that point was read.";
        }
        self.files_msg(Level::Info, m);
    }
}

impl App {
    /// Overview of the whole opened recording above the stream navigator: min/max of every channel along the
    /// file, the part in memory shaded, the screen marked. Click or drag to go there; parts not in memory are
    /// loaded in the background.
    pub(super) fn file_overview(&mut self, ui: &mut egui::Ui) {
        let (shown_start, dropped, len) = {
            let h = self.acq.history.lock().unwrap_or_else(|p| p.into_inner());
            (h.stream.started, h.stream.dropped, h.stream.rows() as u64)
        };
        // a new live stream replaced the file → no overview
        if self
            .files
            .opened
            .as_ref()
            .is_some_and(|o| o.started != shown_start)
        {
            self.files.opened = None;
        }
        let screen = self.stream_rows_per_screen() as u64;
        let busy = self.files.job.is_some();
        let Some(o) = self.files.opened.as_mut() else {
            return;
        };
        if o.total_rows < 2 || o.chunks.is_empty() {
            return;
        }
        let (resp, painter) = ui.allocate_painter(
            Vec2::new(ui.available_width(), 34.0),
            Sense::click_and_drag(),
        );
        let r = resp.rect;
        let w = r.width().max(1.0) as usize;
        painter.rect_filled(r, 2.0, Color32::from_rgb(14, 16, 20));
        let total = o.total_rows as f32;
        let x_of = |row: u64| r.left() + row as f32 / total * r.width();
        // min/max per pixel column per channel, recomputed only when the width changes
        if o.cols.as_ref().is_none_or(|c| c.0 != w) {
            let mut cols = vec![vec![(u16::MAX, 0u16); w]; o.chans.len()];
            for c in &o.chunks {
                let a = ((c.first_row as f32 / total * w as f32) as usize).min(w - 1);
                let b = (((c.first_row + c.rows as u64) as f32 / total * w as f32).ceil() as usize)
                    .clamp(a + 1, w);
                for (k, col) in cols.iter_mut().enumerate() {
                    for px in col.iter_mut().take(b).skip(a) {
                        *px = (px.0.min(c.min[k]), px.1.max(c.max[k]));
                    }
                }
            }
            o.cols = Some((w, cols));
        }
        let cols = &o.cols.as_ref().unwrap().1;
        for (k, &ch) in o.chans.iter().enumerate() {
            if !self.on[ch] {
                continue;
            }
            let (lo, hi) = cols[k]
                .iter()
                .filter(|c| c.0 <= c.1)
                .fold((u16::MAX, 0u16), |a, c| (a.0.min(c.0), a.1.max(c.1)));
            let span = (hi.saturating_sub(lo)).max(1) as f32;
            let y = |v: u16| {
                r.bottom() - 3.0 - (v.saturating_sub(lo)) as f32 / span * (r.height() - 6.0)
            };
            let mut mesh = egui::Mesh::default();
            let col = COLORS[ch].gamma_multiply(0.7);
            for (px, &(a, b)) in cols[k].iter().enumerate() {
                if a > b {
                    continue;
                }
                let x = r.left() + px as f32;
                mesh.add_colored_rect(
                    Rect::from_min_max(Pos2::new(x, y(b)), Pos2::new(x + 1.0, y(a) + 1.0)),
                    col,
                );
            }
            painter.add(Shape::mesh(mesh));
        }
        // part in memory (shaded) and the screen (outlined)
        painter.rect_filled(
            Rect::from_x_y_ranges(
                x_of(dropped)..=x_of(dropped + len).max(x_of(dropped) + 1.0),
                r.y_range(),
            ),
            0.0,
            Color32::from_white_alpha(18),
        );
        let end = self.stream_end.unwrap_or(dropped + len);
        let sr = Rect::from_x_y_ranges(
            x_of(end.saturating_sub(screen))
                ..=x_of(end).max(x_of(end.saturating_sub(screen)) + 2.0),
            r.y_range(),
        );
        painter.rect_stroke(
            sr,
            0.0,
            Stroke::new(1.5, Color32::WHITE),
            egui::StrokeKind::Inside,
        );
        let secs = o.total_rows as f64 / o.rate;
        let path = o.path.clone();
        painter.text(
            r.right_top() + Vec2::new(-4.0, 2.0),
            Align2::RIGHT_TOP,
            fmt_time(secs),
            FontId::monospace(10.0),
            Color32::GRAY,
        );
        let resp = help(resp, "Whole recording file: min/max of each shown channel. The shaded part is in memory, the white box is the screen. Click or drag to go anywhere — parts not in memory load in the background (a few seconds for big files).");
        let release = resp.clicked() || resp.drag_stopped();
        if let Some(p) = resp
            .interact_pointer_pos()
            .filter(|_| release || resp.dragged())
        {
            let row = (((p.x - r.left()) / r.width()).clamp(0.0, 1.0) * total) as u64;
            if self.run != Run::Stopped {
                self.set_run(Run::Stopped);
            }
            if row >= dropped + screen / 2 && row + screen / 2 <= dropped + len {
                self.stream_end = Some(row + screen / 2); // already in memory
                self.stream_cache = None;
            } else if !busy && release {
                // not in memory: load the part around it (on click / when the drag ends, not every frame)
                self.files.pending_row = Some(row);
                self.open_window(path, Some(row));
            }
        }
    }
}

// ───────────────────────────── event logger ─────────────────────────────

/// Messages to the event-log writer thread.
enum EvMsg {
    /// new run: folder, file stamp, trend header line
    Start(PathBuf, String, String),
    /// trend row (CSV line)
    Trend(String),
    /// a matching capture: header (first one opens the event file) + channel data of the active channels
    Event(rec::RecHeader, Vec<Vec<u16>>),
    Stop,
}

/// Event logger state (part of `Files`): long runs keep every capture's measurements (trend CSV) and only the
/// captures that pass the match filter (events `.h8r`, one chunk per capture). Disk work on its own thread.
pub(super) struct EventLog {
    pub(super) on: bool,
    /// save every capture to the events file, not only the matching ones (= capture history to disk)
    pub(super) all: bool,
    /// trend columns (channels shown when the run started)
    chans: Vec<usize>,
    tx: Option<std::sync::mpsc::SyncSender<EvMsg>>,
    /// next capture number to log; captures before it are done
    next: u64,
    pub(super) events: u64,
    pub(super) rows: u64,
    lost: Arc<std::sync::atomic::AtomicU64>,
    started: Option<Instant>,
    file: String,
}

impl Default for EventLog {
    fn default() -> Self {
        EventLog {
            on: false,
            all: false,
            chans: Vec::new(),
            tx: None,
            next: u64::MAX,
            events: 0,
            rows: 0,
            lost: Arc::default(),
            started: None,
            file: String::new(),
        }
    }
}

fn spawn_event_writer(
    lost: Arc<std::sync::atomic::AtomicU64>,
) -> std::sync::mpsc::SyncSender<EvMsg> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<EvMsg>(4096);
    let _ = std::thread::Builder::new()
        .name("event-log".into())
        .spawn(move || {
            let (mut trend, mut events): (
                Option<BufWriter<std::fs::File>>,
                Option<(BufWriter<std::fs::File>, u64)>,
            ) = (None, None);
            let (mut dir, mut stamp) = (PathBuf::new(), String::new());
            let mut flushed = Instant::now();
            loop {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(EvMsg::Start(d, s, head)) => {
                        let _ = std::fs::create_dir_all(&d);
                        trend = std::fs::File::create(d.join(format!("trend_{s}.csv")))
                            .ok()
                            .map(BufWriter::new);
                        if let Some(t) = trend.as_mut() {
                            let _ = writeln!(t, "{head}");
                        }
                        events = None;
                        (dir, stamp) = (d, s);
                    }
                    Ok(EvMsg::Trend(l)) => {
                        if let Some(t) = trend.as_mut() {
                            if writeln!(t, "{l}").is_err() {
                                lost.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    Ok(EvMsg::Event(h, data)) => {
                        if events.is_none() {
                            events = std::fs::File::create(
                                dir.join(format!("events_{stamp}.{}", rec::EXT)),
                            )
                            .ok()
                            .map(|f| {
                                let mut w = BufWriter::new(f);
                                let _ = rec::write_header(&mut w, &h);
                                (w, 0)
                            });
                        }
                        if let Some((w, row)) = events.as_mut() {
                            let parts: Vec<&[u16]> = data.iter().map(Vec::as_slice).collect();
                            match rec::write_chunk(w, *row, &parts) {
                                Ok(_) => *row += data.first().map_or(0, |d| d.len() as u64),
                                Err(_) => {
                                    lost.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    Ok(EvMsg::Stop) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        trend.as_mut().map(|t| t.flush());
                        events.as_mut().map(|e| e.0.flush());
                        (trend, events) = (None, None);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
                if flushed.elapsed() > Duration::from_secs(2) {
                    // crash-safe enough: at most ~2 s of trend / one event lost
                    trend.as_mut().map(|t| t.flush());
                    events.as_mut().map(|e| e.0.flush());
                    flushed = Instant::now();
                }
            }
        });
    tx
}

impl App {
    /// Every frame: log the captures the match filter has already judged (bounded work per frame).
    pub(super) fn event_tick(&mut self) {
        let ev = &mut self.files.events;
        if !ev.on {
            if ev.tx.is_some() {
                if let Some(tx) = ev.tx.take() {
                    let _ = tx.try_send(EvMsg::Stop);
                }
                ev.started = None;
            }
            return;
        }
        let h = self.acq.history.lock().unwrap_or_else(|p| p.into_inner());
        let Some(last) = h.last_no() else { return };
        if ev.tx.is_none() {
            // a new run starts with the next capture
            let tx = spawn_event_writer(ev.lost.clone());
            let s = rec::stamp(SystemTime::now());
            let mut head = "unix_time,capture,match".to_string();
            ev.chans = (0..CHANNELS).filter(|&c| self.on[c]).collect(); // columns stay fixed for the run
            for &ch in &ev.chans {
                let u = &self.probes[ch].unit;
                head += &format!(
                    ",CH{n}_min_{u},CH{n}_max_{u},CH{n}_mean_{u},CH{n}_freq_Hz",
                    n = ch + 1
                );
            }
            let _ = tx.try_send(EvMsg::Start(
                PathBuf::from(&self.files.dir),
                s.clone(),
                head,
            ));
            ev.tx = Some(tx);
            ev.next = last + 1;
            (ev.events, ev.rows) = (0, 0);
            ev.started = Some(Instant::now());
            ev.file = s;
            return;
        }
        // only captures the match filter has judged (update_hits runs first each frame)
        let judged = if self.sw == SwTrig::Off {
            last + 1
        } else {
            self.hits_next.min(last + 1)
        };
        let start = ev.next.max(h.first_no);
        let end = judged.min(start + 300);
        let (tx, lost, all, chans) = (
            ev.tx.clone().unwrap(),
            ev.lost.clone(),
            ev.all,
            ev.chans.clone(),
        );
        let (mut rows, mut events) = (0u64, 0u64);
        for no in start..end {
            let Some(e) = h.get(no) else { continue };
            let Some(cfg) = e.snap.cfg.as_ref() else {
                continue;
            };
            let matched = self.sw != SwTrig::Off && self.hits.binary_search(&no).is_ok();
            let t =
                e.at.duration_since(UNIX_EPOCH)
                    .map_or(0.0, |d| d.as_secs_f64());
            let mut line = format!("{t:.3},{},{}", no + 1, matched as u8);
            for &ch in &chans {
                let d = &e.snap.frame.data[ch];
                if d.len() < 16 || !e.snap.frame.active[ch] {
                    line += ",,,,";
                    continue;
                }
                let vs = cfg.vscale[ch];
                let (zu, vu) = self.uconv(ch, vs, e.snap.zero[vs.index()][ch]);
                let m = measure(
                    d,
                    zu,
                    vu,
                    Some(capture_seconds(cfg.timebase_id) / d.len() as f64),
                );
                line += &format!(
                    ",{:.5},{:.5},{:.5},{}",
                    m.min,
                    m.max,
                    m.mean,
                    m.freq.map_or(String::new(), |f| format!("{f:.4}"))
                );
            }
            if tx.try_send(EvMsg::Trend(line)).is_err() {
                lost.fetch_add(1, Ordering::Relaxed);
            }
            rows += 1;
            if matched || all {
                let n = e.snap.frame.data.iter().map(Vec::len).max().unwrap_or(0);
                let hd = rec::RecHeader {
                    started_unix: t,
                    rate: n as f64 / capture_seconds(cfg.timebase_id),
                    roll_id: 0,
                    active: e.snap.frame.active,
                    vscale: std::array::from_fn(|c| cfg.vscale[c].id()),
                    zero: e.snap.zero,
                    gain: self.cal.gain,
                    probes: self
                        .probes
                        .iter()
                        .map(Probe::encode)
                        .collect::<Vec<_>>()
                        .join(","),
                    capture: Some((
                        cfg.timebase_id,
                        cfg.trig_pos,
                        e.snap.trig_off.unwrap_or(0.0),
                    )),
                };
                let data: Vec<Vec<u16>> = (0..CHANNELS)
                    .filter(|&c| e.snap.frame.active[c])
                    .map(|c| e.snap.frame.data[c].clone())
                    .collect();
                if tx.try_send(EvMsg::Event(hd, data)).is_err() {
                    lost.fetch_add(1, Ordering::Relaxed);
                }
                events += 1;
            }
        }
        let ev = &mut self.files.events;
        ev.rows += rows;
        ev.events += events;
        ev.next = end.max(ev.next);
    }

    /// Settings rows for the event logger.
    pub(super) fn event_settings(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Event log (long runs)").strong());
        ui.horizontal(|ui| {
            help(
                ui.checkbox(&mut self.files.events.on, "log events + trend"),
                "For long runs at full capture rate (hours, intermittent faults): every capture adds one row to trend_<time>.csv (time, capture, matched, min / max / mean / frequency per shown channel), and only the captures that pass the match filter are saved, to events_<time>.h8r. Both in the recording folder. Without a match filter only the trend is written.",
            );
            help(
                ui.checkbox(&mut self.files.events.all, "every capture"),
                "Save every capture to the events file, not only the ones passing the match filter (the capture history on disk). Full rate is ~4.8 MB/s ≈ 17 GB per hour — check the disk test and free space.",
            );
            let ev = &self.files.events;
            if let Some(t) = ev.started {
                ui.label(egui::RichText::new(format!("{} · {} captures · {} events", fmt_time(t.elapsed().as_secs_f64()), ev.rows, ev.events)).weak());
            }
        });
    }

    /// Auto-store on (for the REC badge).
    pub(super) fn files_auto(&self) -> bool {
        self.files.auto
    }

    /// Event log running: badge tooltip text.
    pub(super) fn files_event_state(&self) -> Option<String> {
        let ev = &self.files.events;
        ev.started.map(|t| {
            format!(
                "Event log running {}: {} captures logged, {} saved (files *_{}).",
                fmt_time(t.elapsed().as_secs_f64()),
                ev.rows,
                ev.events,
                ev.file
            )
        })
    }

    /// Info-line notice while the event log runs: now a badge; only problems are notices.
    pub(super) fn event_notice(&self, v: &mut Vec<(Level, String)>) {
        let ev = &self.files.events;
        let lost = ev.lost.load(Ordering::Relaxed);
        if lost > 0 {
            v.push((
                Level::Warn,
                format!(
                    "Event log: {lost} rows/events could not be written (disk too slow or full)."
                ),
            ));
        }
    }
}

// ───────────────────────────── screenshot (PNG) and references on disk ─────────────────────────────

// 5×7 ASCII font (0x20..0x7E), one byte per column, bit 0 = top row: the screenshot info strip is drawn off the UI
// thread straight into the pixels, without the GPU / egui fonts.
const FONT5X7: [[u8; 5]; 95] = [
    [0x00, 0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x5F, 0x00, 0x00],
    [0x00, 0x07, 0x00, 0x07, 0x00],
    [0x14, 0x7F, 0x14, 0x7F, 0x14],
    [0x24, 0x2A, 0x7F, 0x2A, 0x12],
    [0x23, 0x13, 0x08, 0x64, 0x62],
    [0x36, 0x49, 0x55, 0x22, 0x50],
    [0x00, 0x05, 0x03, 0x00, 0x00],
    [0x00, 0x1C, 0x22, 0x41, 0x00],
    [0x00, 0x41, 0x22, 0x1C, 0x00],
    [0x08, 0x2A, 0x1C, 0x2A, 0x08],
    [0x08, 0x08, 0x3E, 0x08, 0x08],
    [0x00, 0x50, 0x30, 0x00, 0x00],
    [0x08, 0x08, 0x08, 0x08, 0x08],
    [0x00, 0x60, 0x60, 0x00, 0x00],
    [0x20, 0x10, 0x08, 0x04, 0x02],
    [0x3E, 0x51, 0x49, 0x45, 0x3E],
    [0x00, 0x42, 0x7F, 0x40, 0x00],
    [0x42, 0x61, 0x51, 0x49, 0x46],
    [0x21, 0x41, 0x45, 0x4B, 0x31],
    [0x18, 0x14, 0x12, 0x7F, 0x10],
    [0x27, 0x45, 0x45, 0x45, 0x39],
    [0x3C, 0x4A, 0x49, 0x49, 0x30],
    [0x01, 0x71, 0x09, 0x05, 0x03],
    [0x36, 0x49, 0x49, 0x49, 0x36],
    [0x06, 0x49, 0x49, 0x29, 0x1E],
    [0x00, 0x36, 0x36, 0x00, 0x00],
    [0x00, 0x56, 0x36, 0x00, 0x00],
    [0x08, 0x14, 0x22, 0x41, 0x00],
    [0x14, 0x14, 0x14, 0x14, 0x14],
    [0x00, 0x41, 0x22, 0x14, 0x08],
    [0x02, 0x01, 0x51, 0x09, 0x06],
    [0x32, 0x49, 0x79, 0x41, 0x3E],
    [0x7E, 0x11, 0x11, 0x11, 0x7E],
    [0x7F, 0x49, 0x49, 0x49, 0x36],
    [0x3E, 0x41, 0x41, 0x41, 0x22],
    [0x7F, 0x41, 0x41, 0x22, 0x1C],
    [0x7F, 0x49, 0x49, 0x49, 0x41],
    [0x7F, 0x09, 0x09, 0x01, 0x01],
    [0x3E, 0x41, 0x41, 0x51, 0x32],
    [0x7F, 0x08, 0x08, 0x08, 0x7F],
    [0x00, 0x41, 0x7F, 0x41, 0x00],
    [0x20, 0x40, 0x41, 0x3F, 0x01],
    [0x7F, 0x08, 0x14, 0x22, 0x41],
    [0x7F, 0x40, 0x40, 0x40, 0x40],
    [0x7F, 0x02, 0x04, 0x02, 0x7F],
    [0x7F, 0x04, 0x08, 0x10, 0x7F],
    [0x3E, 0x41, 0x41, 0x41, 0x3E],
    [0x7F, 0x09, 0x09, 0x09, 0x06],
    [0x3E, 0x41, 0x51, 0x21, 0x5E],
    [0x7F, 0x09, 0x19, 0x29, 0x46],
    [0x46, 0x49, 0x49, 0x49, 0x31],
    [0x01, 0x01, 0x7F, 0x01, 0x01],
    [0x3F, 0x40, 0x40, 0x40, 0x3F],
    [0x1F, 0x20, 0x40, 0x20, 0x1F],
    [0x7F, 0x20, 0x18, 0x20, 0x7F],
    [0x63, 0x14, 0x08, 0x14, 0x63],
    [0x03, 0x04, 0x78, 0x04, 0x03],
    [0x61, 0x51, 0x49, 0x45, 0x43],
    [0x00, 0x7F, 0x41, 0x41, 0x00],
    [0x02, 0x04, 0x08, 0x10, 0x20],
    [0x00, 0x41, 0x41, 0x7F, 0x00],
    [0x04, 0x02, 0x01, 0x02, 0x04],
    [0x40, 0x40, 0x40, 0x40, 0x40],
    [0x00, 0x01, 0x02, 0x04, 0x00],
    [0x20, 0x54, 0x54, 0x54, 0x78],
    [0x7F, 0x48, 0x44, 0x44, 0x38],
    [0x38, 0x44, 0x44, 0x44, 0x20],
    [0x38, 0x44, 0x44, 0x48, 0x7F],
    [0x38, 0x54, 0x54, 0x54, 0x18],
    [0x08, 0x7E, 0x09, 0x01, 0x02],
    [0x08, 0x14, 0x54, 0x54, 0x3C],
    [0x7F, 0x08, 0x04, 0x04, 0x78],
    [0x00, 0x44, 0x7D, 0x40, 0x00],
    [0x20, 0x40, 0x44, 0x3D, 0x00],
    [0x00, 0x7F, 0x10, 0x28, 0x44],
    [0x00, 0x41, 0x7F, 0x40, 0x00],
    [0x7C, 0x04, 0x18, 0x04, 0x78],
    [0x7C, 0x08, 0x04, 0x04, 0x78],
    [0x38, 0x44, 0x44, 0x44, 0x38],
    [0x7C, 0x14, 0x14, 0x14, 0x08],
    [0x08, 0x14, 0x14, 0x18, 0x7C],
    [0x7C, 0x08, 0x04, 0x04, 0x08],
    [0x48, 0x54, 0x54, 0x54, 0x20],
    [0x04, 0x3F, 0x44, 0x40, 0x20],
    [0x3C, 0x40, 0x40, 0x20, 0x7C],
    [0x1C, 0x20, 0x40, 0x20, 0x1C],
    [0x3C, 0x40, 0x30, 0x40, 0x3C],
    [0x44, 0x28, 0x10, 0x28, 0x44],
    [0x0C, 0x50, 0x50, 0x50, 0x3C],
    [0x44, 0x64, 0x54, 0x4C, 0x44],
    [0x00, 0x08, 0x36, 0x41, 0x00],
    [0x00, 0x00, 0x7F, 0x00, 0x00],
    [0x00, 0x41, 0x36, 0x08, 0x00],
    [0x08, 0x04, 0x08, 0x10, 0x08],
];

/// The strip font is ASCII: µ, ×, ·, — etc. get ASCII stand-ins.
fn ascii(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'µ' => "u".to_string(),
            '×' => "x".to_string(),
            '·' | '|' => "|".to_string(),
            '—' | '–' | '−' => "-".to_string(),
            '…' => "..".to_string(),
            '≈' => "~".to_string(),
            '°' => "deg".to_string(),
            '≥' => ">=".to_string(),
            '≤' => "<=".to_string(),
            c if (' '..='~').contains(&c) => c.to_string(),
            _ => "?".to_string(),
        })
        .collect()
}

/// The screenshot with `lines` of info text in a dark strip appended below it (nothing on screen is covered).
fn add_strip(img: &egui::ColorImage, lines: &[String]) -> egui::ColorImage {
    let [w, h] = img.size;
    let s = ((w as f32 / 700.0).round() as usize).clamp(1, 4); // glyph scale: readable on 1200 px … 4K
    let (gw, lh, pad) = (6 * s, 9 * s, 3 * s);
    let sh = lines.len() * lh + 2 * pad;
    let bg = egui::Color32::from_gray(22);
    let fg = egui::Color32::from_gray(215);
    let mut px = img.pixels.clone();
    px.extend(std::iter::repeat_n(bg, w * sh));
    px[w * h..w * (h + 1)].fill(egui::Color32::from_gray(70)); // separator line
    for (li, line) in lines.iter().enumerate() {
        let y0 = h + pad + li * lh + s;
        for (ci, ch) in ascii(line).bytes().enumerate() {
            let x0 = pad + ci * gw;
            if x0 + 5 * s > w {
                break;
            }
            let g = FONT5X7[(ch.clamp(0x20, 0x7E) - 0x20) as usize];
            let down = usize::from(matches!(ch, b'g' | b'j' | b'p' | b'q' | b'y')); // descenders below the line
            for (col, bits) in g.iter().enumerate() {
                for row in 0..7 {
                    if bits >> row & 1 == 1 {
                        for dy in 0..s {
                            let o = (y0 + (row + down) * s + dy) * w + x0 + col * s;
                            px[o..o + s].fill(fg);
                        }
                    }
                }
            }
        }
    }
    egui::ColorImage::new([w, h + sh], px)
}

/// Wrap the strip text at ' | ' separators to `cols` characters.
fn wrap_strip(text: &str, cols: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for part in text.split(" | ") {
        let cur = lines.last_mut().unwrap();
        if !cur.is_empty() && cur.chars().count() + 3 + part.chars().count() > cols {
            lines.push(part.to_string());
        } else {
            if !cur.is_empty() {
                cur.push_str(" | ");
            }
            cur.push_str(part);
        }
    }
    lines
}

fn write_png(
    path: &PathBuf,
    img: &egui::ColorImage,
    meta: &[(&str, String)],
) -> std::io::Result<()> {
    let [w, h] = img.size;
    let mut e = png::Encoder::new(
        BufWriter::new(std::fs::File::create(path)?),
        w as u32,
        h as u32,
    );
    e.set_color(png::ColorType::Rgb);
    e.set_depth(png::BitDepth::Eight);
    // standard PNG text keys (exiftool, GIMP, file managers show them); iTXt = UTF-8
    for (k, v) in meta {
        e.add_itxt_chunk(k.to_string(), v.clone())
            .map_err(std::io::Error::other)?;
    }
    let mut wr = e.write_header().map_err(std::io::Error::other)?;
    let rgb: Vec<u8> = img
        .pixels
        .iter()
        .flat_map(|p| [p.r(), p.g(), p.b()])
        .collect();
    wr.write_image_data(&rgb).map_err(std::io::Error::other)
}

fn ref_path(i: usize) -> Option<PathBuf> {
    config_dir().map(|d| d.join(format!("ref{}.csv", i + 1)))
}

/// What a file dialog was opened for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum BrowseFor {
    Ref(usize),
    Recording,
}

/// The system's file dialog without a GUI-toolkit dependency, as a separate process (blocking — call it from a
/// thread): Linux zenity (GNOME / GTK) or kdialog (KDE), Windows its Open dialog via PowerShell.
/// Ok(None) = cancelled, Err = no dialog tool.
fn file_dialog(
    title: &str,
    dir: &std::path::Path,
    (fname, fglob): (&str, &str),
) -> std::result::Result<Option<PathBuf>, String> {
    use std::process::Command;
    // Windows: its own Open dialog through PowerShell (always installed), no console window, UTF-8 path out
    #[cfg(windows)]
    let out = {
        use std::os::windows::process::CommandExt;
        let q = |s: &str| s.replace('\'', "''");
        let ps = format!(
            "[Console]::OutputEncoding = [Text.Encoding]::UTF8; Add-Type -AssemblyName System.Windows.Forms; \
             $d = New-Object System.Windows.Forms.OpenFileDialog; $d.Title = '{}'; $d.InitialDirectory = '{}'; \
             $d.Filter = '{}|{}|All files|*.*'; if ($d.ShowDialog() -eq 'OK') {{ [Console]::Out.Write($d.FileName) }}",
            q(title),
            q(&dir.display().to_string()),
            q(fname),
            q(fglob)
        );
        Command::new("powershell")
            .args(["-NoProfile", "-STA", "-Command", &ps])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
            .map_err(|e| format!("File dialog (PowerShell) failed: {e} — type the path in Settings › Recording, event log, folder, open."))?
    };
    #[cfg(not(windows))]
    let start = format!("{}/", dir.display());
    #[cfg(not(windows))]
    let out = Command::new("zenity")
        .args(["--file-selection", &format!("--title={title}"), &format!("--filename={start}"), &format!("--file-filter={fname} | {fglob}"), "--file-filter=All files | *"])
        .output()
        .or_else(|_| Command::new("kdialog").args(["--title", title, "--getopenfilename", &start, &format!("{fglob}|{fname}")]).output())
        .map_err(|_| "No file dialog found (install zenity) — or type the path in Settings › Recording, event log, folder, open.".to_string())?;
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((out.status.success() && !p.is_empty()).then(|| PathBuf::from(p)))
}

/// Library file name → readable label: ref_2026-11-22_230132_capture-CH1 → "2026-11-22 23:01 UTC · capture · CH1".
fn ref_label(p: &std::path::Path) -> String {
    let stem = p
        .file_stem()
        .map_or(String::new(), |n| n.to_string_lossy().into_owned());
    let parts: Vec<&str> = stem
        .strip_prefix("ref_")
        .unwrap_or(&stem)
        .splitn(3, '_')
        .collect();
    match parts.as_slice() {
        [date, time, what]
            if date.len() == 10 && time.len() == 6 && time.bytes().all(|b| b.is_ascii_digit()) =>
        {
            let (kind, ch) = what.rsplit_once('-').unwrap_or((what, ""));
            format!("{date} {}:{} UTC · {kind} · {ch}", &time[..2], &time[2..4])
        }
        _ => stem,
    }
}

/// A reference waveform: one channel of a capture in its own unit, as a small CSV file (time, value + a commented
/// header) — the R1/R2 slots live in the settings folder, a free library in <recording folder>/references.
#[derive(Clone, Debug)]
pub(super) struct RefWave {
    pub(super) name: String,
    pub(super) unit: String,
    /// channel it was taken from
    pub(super) src: usize,
    pub(super) saved_unix: f64,
    pub(super) dt: f64,
    /// sample index of the trigger point (aligned to the live trigger marker)
    pub(super) trig_i: f64,
    pub(super) v: Vec<f32>,
    /// drawn with this channel's scale, offset and plot (default: its source channel)
    pub(super) under: usize,
    /// extra vertical shift (div)
    pub(super) offset: f32,
    pub(super) on: bool,
}

impl RefWave {
    fn to_csv(&self) -> String {
        use std::fmt::Write;
        let saved =
            rec::utc(UNIX_EPOCH + std::time::Duration::from_secs_f64(self.saved_unix.max(0.0)));
        let mut s = format!(
            "# LR Scope 1008C reference waveform\n# name={}\n# source=CH{}\n# unit={}\n# saved={saved}\n# saved_unix={}\n# dt={:e}\n# trigger_sample={}\n# under=CH{}\n# offset={}\n# on={}\nt_s,value_{}\n",
            self.name, self.src + 1, self.unit, self.saved_unix, self.dt, self.trig_i, self.under + 1, self.offset, self.on as u8, self.unit
        );
        for (i, v) in self.v.iter().enumerate() {
            let _ = writeln!(s, "{:e},{v}", (i as f64 - self.trig_i) * self.dt);
        }
        s
    }

    fn from_csv(txt: &str) -> Option<RefWave> {
        let ch = |v: &str| {
            v.trim()
                .trim_start_matches("CH")
                .parse::<usize>()
                .ok()
                .filter(|c| (1..=CHANNELS).contains(c))
                .map(|c| c - 1)
        };
        let mut r = RefWave {
            name: String::new(),
            unit: "V".into(),
            src: 0,
            saved_unix: 0.0,
            dt: 0.0,
            trig_i: 0.0,
            v: Vec::new(),
            under: 0,
            offset: 0.0,
            on: true,
        };
        let mut under = None;
        for line in txt.lines() {
            if let Some(kv) = line.strip_prefix("# ") {
                let Some((k, v)) = kv.split_once('=') else {
                    continue;
                };
                match k {
                    "name" => r.name = v.to_string(),
                    "source" => r.src = ch(v)?,
                    "unit" => r.unit = v.to_string(),
                    "saved_unix" => r.saved_unix = v.trim().parse().unwrap_or(0.0),
                    "dt" => r.dt = v.trim().parse().ok()?,
                    "trigger_sample" => r.trig_i = v.trim().parse().ok()?,
                    "under" => under = ch(v),
                    "offset" => {
                        r.offset = v
                            .trim()
                            .parse::<f32>()
                            .ok()
                            .filter(|o| o.is_finite())
                            .unwrap_or(0.0)
                            .clamp(-8.0, 8.0)
                    }
                    "on" => r.on = v.trim() != "0",
                    _ => {}
                }
            } else if let Some((_, val)) = line.split_once(',') {
                if let Ok(x) = val.trim().parse::<f32>() {
                    if x.is_finite() {
                        r.v.push(x);
                    }
                }
            }
        }
        r.under = under.unwrap_or(r.src);
        (r.dt > 0.0 && r.dt.is_finite() && r.trig_i.is_finite() && r.v.len() >= 2).then_some(r)
    }
}

/// Short name of a signal type for file names.
fn kind_short(k: crate::analyze::Kind) -> &'static str {
    use crate::analyze::Kind::*;
    match k {
        None => "",
        Injector => "injector",
        IgnitionPrimary => "ignition",
        PulseHigh | PulseLow => "pulses",
        CrankVr => "crank",
        Lambda => "lambda",
        Battery => "battery",
        Compression => "compression",
        WheelSpeed => "wheel-speed",
    }
}

impl App {
    /// Screenshot info: (lean file-name part, full PNG metadata, one-line strip text).
    fn shot_info(&self, now: SystemTime) -> (String, Vec<(&'static str, String)>, String) {
        use crate::analyze::{Kind, KINDS};
        let tb = match self.roll {
            Some(_) => format!(
                "stream {:.0} S/s, {}/div",
                self.shown_rate,
                fmt_time(self.span_s() / 10.0)
            ),
            None => format!("{}/div", fmt_time(self.span_s() / 10.0)),
        };
        let probe = |ch: usize| {
            let p = &self.probes[ch];
            PROBE_PRESETS
                .get(p.preset)
                .map_or(format!("custom x{} {}", p.scale, p.unit), |q| {
                    q.0.to_string()
                })
        };
        let shown: Vec<usize> = (0..CHANNELS).filter(|&c| self.on[c]).collect();
        // the first analysed channel names the shot; else the decoder; else the timebase
        let analysed: Vec<usize> = shown
            .iter()
            .copied()
            .filter(|&c| self.kinds[c] != Kind::None)
            .collect();
        let verdict = |ch: usize| {
            let a = &self.ana[ch];
            if !a.present {
                Some((None, "waiting for a signal".to_string()))
            } else {
                a.verdict.as_ref().map(|(ok, t)| (Some(*ok), t.clone()))
            }
        };
        let mut desc = match analysed.first() {
            Some(&ch) => {
                let v = match verdict(ch) {
                    Some((Some(true), _)) => "-OK",
                    Some((Some(false), _)) => "-CHECK",
                    Some((None, _)) => "-nosignal",
                    None => "",
                };
                format!("{}-CH{}{v}", kind_short(self.kinds[ch]), ch + 1)
            }
            None => match self.decoder_summary() {
                Some((short, _)) => short,
                None => fmt_time(self.span_s() / 10.0).replace(' ', "") + "-div",
            },
        };
        desc = ascii(&desc)
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        desc = desc.replace("--", "-").trim_matches('-').to_string();

        let tunit = self.probes[self.trig_src as usize].unit.clone();
        let trig = if self.roll.is_some() {
            "stream (no hardware trigger)".to_string()
        } else {
            format!(
                "CH{} {} {} at {} % {}",
                self.trig_src + 1,
                if self.trig_slope == Slope::Rising {
                    "rising"
                } else {
                    "falling"
                },
                fmt_u(self.trig_v, &tunit),
                self.trig_pos as f32 / 10.0,
                match self.trig_mode {
                    TrigMode::Auto => "auto",
                    TrigMode::Normal => "normal",
                    TrigMode::Free => "free run",
                }
            )
        };
        let when = rec::utc(now);
        let mut d = format!(
            "LR Scope 1008C {} | {} | {}\nTime: {when}\nTimebase: {tb}\nTrigger: {trig}\n",
            env!("CARGO_PKG_VERSION"),
            if self.sim {
                "simulator"
            } else {
                "Hantek 1008C"
            },
            match (self.run, self.shown_no) {
                (_, Some(n)) => format!("capture #{n} (history)"),
                (Run::Running, None) => "live".to_string(),
                _ => "stopped".to_string(),
            }
        );
        if let Some(name) = self.guide_name() {
            d += &format!("Guided test: {name}\n");
        }
        for &ch in &shown {
            d += &format!(
                "CH{}: {}/div, probe {}, offset {:.2} div",
                ch + 1,
                fmt_u(self.udiv(ch), &self.probes[ch].unit),
                probe(ch),
                self.offset[ch]
            );
            if self.kinds[ch] != Kind::None {
                let name = KINDS
                    .iter()
                    .find(|k| k.0 == self.kinds[ch])
                    .map_or("", |k| k.1);
                d += &format!("; analysed as {name}");
                if let Some((ok, t)) = verdict(ch) {
                    d += &format!(
                        ": {}{t}",
                        match ok {
                            Some(true) => "OK ",
                            Some(false) => "CHECK ",
                            None => "",
                        }
                    );
                }
                for (k, v) in self.ana[ch].readouts() {
                    d += &format!("; {k} {v}");
                }
            }
            d += "\n";
        }
        for (i, m) in self.math.iter().enumerate() {
            if m.op != MathOp::Off {
                d += &format!("M{}: {:?} of CH{} / CH{}\n", i + 1, m.op, m.a + 1, m.b + 1);
            }
        }
        if let Some((_, long)) = self.decoder_summary() {
            d += &format!("Decode: {long}\n");
        }
        for (k, r) in self.refs.iter().enumerate() {
            if let Some(r) = r.as_ref().filter(|r| r.on && self.refs_on) {
                d += &format!(
                    "Reference R{}: {} (from CH{}, unit {}), drawn on CH{} offset {:.2} div\n",
                    k + 1,
                    r.name,
                    r.src + 1,
                    r.unit,
                    r.under + 1,
                    r.offset
                );
            }
        }
        let meta = vec![
            ("Title", desc.clone()),
            (
                "Software",
                format!("LR Scope 1008C {}", env!("CARGO_PKG_VERSION")),
            ),
            ("Creation Time", when.clone()),
            ("Description", d),
        ];

        // strip: the essentials in one line (wrapped at ' | ' if the window is narrow)
        let mut st = vec![
            format!("LR Scope 1008C | {when}"),
            tb,
            format!("trig {trig}"),
        ];
        for &ch in &shown {
            st.push(format!(
                "CH{} {}/div {}",
                ch + 1,
                fmt_u(self.udiv(ch), &self.probes[ch].unit),
                probe(ch)
            ));
        }
        if let Some(name) = self.guide_name() {
            st.push(format!("test: {name}"));
        }
        for &ch in &analysed {
            if let Some((ok, t)) = verdict(ch) {
                st.push(format!(
                    "CH{}: {}{t}",
                    ch + 1,
                    match ok {
                        Some(true) => "OK ",
                        Some(false) => "CHECK ",
                        None => "",
                    }
                ));
            }
        }
        if let Some((_, long)) = self.decoder_summary() {
            st.push(long);
        }
        for (k, r) in self.refs.iter().enumerate() {
            if let Some(r) = r.as_ref().filter(|r| r.on && self.refs_on) {
                st.push(format!("R{} on CH{}: {}", k + 1, r.under + 1, r.name));
            }
        }
        (desc, meta, st.join(" | "))
    }

    /// F12 / plot menu: whole window → screenshot_<time>_<what>.png in the recording folder (encoded off the UI thread),
    /// details in the PNG metadata, optional info strip appended below.
    pub(super) fn screenshot_tick(&mut self, ctx: &egui::Context) {
        if let Some(r) = self.files.shot.lock().ok().and_then(|mut s| s.take()) {
            match r {
                Ok(p) => self.files_msg(Level::Info, format!("Screenshot saved: {}", p.display())),
                Err(e) => self.files_msg(Level::Warn, format!("Screenshot failed: {e}")),
            }
        }
        // a requested screenshot waits two frames, so the menu it was clicked in is closed in the picture
        if std::mem::take(&mut self.files.shot_req) {
            self.files.shot_wait = 2;
        }
        if self.files.shot_wait == 0 {
            return;
        }
        self.files.shot_wait -= 1;
        if self.files.shot_wait > 0 {
            ctx.request_repaint();
            return;
        }
        let dir = PathBuf::from(&self.files.dir);
        let now = SystemTime::now();
        let (desc, meta, strip) = self.shot_info(now);
        let path = dir.join(format!("screenshot_{}_{desc}.png", rec::stamp(now)));
        let strip = self.shot_strip.then_some(strip);
        let slot = self.files.shot.clone();
        ctx.request_screenshot(move |img| {
            let img = img.clone();
            std::thread::spawn(move || {
                let img = match &strip {
                    Some(t) => {
                        let cols = img.size[0]
                            / (6 * ((img.size[0] as f32 / 700.0).round() as usize).clamp(1, 4))
                            - 1;
                        add_strip(&img, &wrap_strip(t, cols))
                    }
                    None => (*img).clone(),
                };
                let r = std::fs::create_dir_all(&dir)
                    .and_then(|_| write_png(&path, &img, &meta))
                    .map(|_| path)
                    .map_err(|e| e.to_string());
                if let Ok(mut s) = slot.lock() {
                    *s = Some(r);
                }
            });
        });
        ctx.request_repaint();
    }

    /// Reference slot R1/R2 → settings folder (`ref1.csv`); cleared → file removed.
    pub(super) fn save_ref(&self, i: usize) {
        let Some(p) = ref_path(i) else { return };
        let r = match self.refs[i].as_ref() {
            None => std::fs::remove_file(&p).or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(e)
                }
            }),
            Some(rf) => p
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|_| std::fs::write(&p, rf.to_csv())),
        };
        if let Err(e) = r {
            log::warn!("reference {}: {e}", p.display());
        }
    }

    /// R1/R2 as they were at the last exit.
    pub(super) fn load_refs(&mut self) {
        for i in 0..2 {
            self.refs[i] = ref_path(i)
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|t| RefWave::from_csv(&t));
        }
    }

    /// The free reference library: <recording folder>/references.
    pub(super) fn ref_library_dir(&self) -> PathBuf {
        self.files.dir().join("references")
    }

    /// Library files, newest first (names start with the date).
    fn ref_library(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(self.ref_library_dir())
            .map(|d| {
                d.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().is_some_and(|x| x == "csv"))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v.reverse();
        v
    }

    /// Channel `ch` of the capture on screen → reference slot `i`, plus a copy in the library.
    pub(super) fn ref_take(&mut self, i: usize, snap: &Snapshot, ch: usize) {
        let Some(cfg) = snap.cfg.as_ref().filter(|_| !snap.roll) else {
            self.files_msg(
                Level::Warn,
                "References are captures: switch to a capture timebase first.".into(),
            );
            return;
        };
        let Some(v) = self.units_of(snap, ch) else {
            self.files_msg(
                Level::Warn,
                format!(
                    "CH{} has no data in the capture on screen — select a shown channel (1…8).",
                    ch + 1
                ),
            );
            return;
        };
        let n = v.len();
        let now = SystemTime::now();
        let kind = self.kinds[ch];
        let what = if kind == crate::analyze::Kind::None {
            "capture"
        } else {
            kind_short(kind)
        };
        let r = RefWave {
            name: format!("CH{} {what} {}", ch + 1, rec::utc(now)),
            unit: self.probes[ch].unit.clone(),
            src: ch,
            saved_unix: now
                .duration_since(UNIX_EPOCH)
                .map_or(0.0, |d| d.as_secs_f64()),
            dt: capture_seconds(cfg.timebase_id) / n as f64,
            trig_i: cfg.trig_pos as f64 / 1000.0 * n as f64 + snap.trig_off.unwrap_or(0.0) as f64,
            v,
            under: ch,
            offset: 0.0,
            on: true,
        };
        let dir = self.ref_library_dir();
        let file = dir.join(format!("ref_{}_{what}-CH{}.csv", rec::stamp(now), ch + 1));
        let lib = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&file, r.to_csv()));
        self.refs[i] = Some(r);
        self.refs_on = true;
        self.save_ref(i);
        self.files.ref_lib.0 = None;
        match lib {
            Ok(()) => self.files_msg(
                Level::Info,
                format!(
                    "R{} = CH{} (also in the library: {}). Ctrl+R hides references.",
                    i + 1,
                    ch + 1,
                    file.display()
                ),
            ),
            Err(e) => self.files_msg(
                Level::Warn,
                format!(
                    "R{} = CH{}, but the library copy failed: {e}",
                    i + 1,
                    ch + 1
                ),
            ),
        }
    }

    /// 'Load into Rn' list (Menu, plot menu, channel rows): the newest library references, readable, and Browse….
    pub(super) fn ref_load_menu(&mut self, ui: &mut egui::Ui, i: usize) {
        // listed once per opening (cached ≤ 2 s), not a directory read every frame
        if self
            .files
            .ref_lib
            .0
            .is_none_or(|t| t.elapsed().as_secs_f32() > 2.0)
        {
            self.files.ref_lib = (Some(Instant::now()), self.ref_library());
        }
        let lib = self.files.ref_lib.1.clone();
        ui.label(
            egui::RichText::new(format!("Library: {}", self.ref_library_dir().display()))
                .weak()
                .small(),
        );
        if lib.is_empty() {
            ui.label(egui::RichText::new("empty — save one with R / Shift+R").weak());
        }
        let mut load = None;
        egui::ScrollArea::vertical()
            .max_height(300.0)
            .show(ui, |ui| {
                for p in lib.iter().take(40) {
                    if ui
                        .button(ref_label(p))
                        .on_hover_text(p.display().to_string())
                        .clicked()
                    {
                        load = Some(p.clone());
                    }
                }
            });
        ui.separator();
        if help(ui.add_enabled(!self.files.browse_busy, egui::Button::new("Browse…")), "Pick a reference file anywhere with the system's file dialog (a .csv saved by this app, e.g. from another computer or years ago).").clicked() {
            self.browse_start(ui.ctx(), BrowseFor::Ref(i));
            ui.close();
        }
        if let Some(p) = load {
            self.ref_load(i, &p);
            ui.close();
        }
    }

    /// Opens the system file dialog in a thread (the UI keeps running); the result is picked up by `browse_tick`.
    pub(super) fn browse_start(&mut self, ctx: &egui::Context, what: BrowseFor) {
        if self.files.browse_busy {
            return;
        }
        let (title, dir, filter) = match what {
            BrowseFor::Ref(i) => (
                format!("Load reference into R{}", i + 1),
                self.ref_library_dir(),
                ("References (*.csv)", "*.csv"),
            ),
            BrowseFor::Recording => (
                "Open recording".to_string(),
                self.files.dir(),
                ("Recordings (*.h8r)", "*.h8r"),
            ),
        };
        self.files.browse_busy = true;
        let (slot, ctx) = (self.files.browse.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = file_dialog(&title, &dir, filter);
            if let Ok(mut s) = slot.lock() {
                *s = Some((what, r));
            }
            ctx.request_repaint();
        });
    }

    /// A file picked in the dialog: load the reference / open the recording.
    pub(super) fn browse_tick(&mut self) {
        let Some((what, r)) = self.files.browse.lock().ok().and_then(|mut s| s.take()) else {
            return;
        };
        self.files.browse_busy = false;
        match (what, r) {
            (_, Ok(None)) => {} // cancelled
            (BrowseFor::Ref(i), Ok(Some(p))) => self.ref_load(i, &p),
            (BrowseFor::Recording, Ok(Some(p))) => {
                self.files.open_path = p.display().to_string();
                self.open_start(p);
            }
            (_, Err(e)) => self.files_msg(Level::Warn, e),
        }
    }

    /// A reference file (library or any .csv in that format) → slot `i`, drawn with its source channel.
    fn ref_load(&mut self, i: usize, p: &std::path::Path) {
        match std::fs::read_to_string(p)
            .ok()
            .and_then(|t| RefWave::from_csv(&t))
        {
            Some(mut r) => {
                // drawn with its source channel if that is shown, else with the selected one
                let under = if self.on[r.src] { r.src } else { self.sel_ch };
                (r.under, r.offset, r.on) = (under, 0.0, true);
                let msg = format!(
                    "R{} = {} ({}), drawn on CH{}.",
                    i + 1,
                    r.name,
                    p.display(),
                    under + 1
                );
                self.refs[i] = Some(r);
                self.refs_on = true;
                self.save_ref(i);
                self.files_msg(Level::Info, msg);
            }
            None => self.files_msg(
                Level::Warn,
                format!("{}: not a reference file.", p.display()),
            ),
        }
    }
}

impl App {
    /// "☰ Menu": everything about files and the less frequent actions, reachable without Settings.
    pub(super) fn main_menu(&mut self, ui: &mut egui::Ui) {
        self.files.menu_open = true;
        let busy = self.files.job.is_some();
        ui.menu_button("Automotive tests", |ui| self.presets_menu(ui, false));
        ui.menu_button("Protocol decoding", |ui| self.presets_menu(ui, true));
        if ui
            .button(if self.meter.on {
                "Scope view  M"
            } else {
                "Meter mode  M"
            })
            .clicked()
        {
            self.meter.on = !self.meter.on;
            ui.close();
        }
        ui.separator();
        ui.menu_button("Open recording", |ui| {
            let files = self
                .files
                .folder
                .lock()
                .map_or(Vec::new(), |f| f.files.clone());
            if files.is_empty() {
                ui.label(egui::RichText::new("no recordings in the folder yet").weak());
            }
            for (p, n) in files.iter().rev().take(10) {
                let name = p
                    .file_name()
                    .map_or(String::new(), |n| n.to_string_lossy().into());
                if ui
                    .add_enabled(!busy, egui::Button::new(format!("{name}  ({})", gb(*n))))
                    .clicked()
                {
                    self.files.open_path = p.display().to_string();
                    self.open_start(p.clone());
                    ui.close();
                }
            }
            ui.separator();
            if help(
                ui.add_enabled(
                    !busy && !self.files.browse_busy,
                    egui::Button::new("Browse…"),
                ),
                "Pick a recording (.h8r) anywhere with the system's file dialog.",
            )
            .clicked()
            {
                self.browse_start(ui.ctx(), BrowseFor::Recording);
                ui.close();
            }
        });
        // stays open while the format is picked (a click inside used to close it); the export buttons close it
        egui::containers::menu::SubMenuButton::new("Export")
            .config(egui::containers::menu::MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside))
            .ui(ui, |ui| {
            // format first, then what: everything about exporting in one place (the folder is in Settings)
            ui.horizontal(|ui| {
                help(ui.selectable_value(&mut self.files.fmt, ExportFmt::Csv, "CSV"), "Text: time + one column per shown channel in its unit. Opens in any spreadsheet, Python, MATLAB.");
                help(ui.selectable_value(&mut self.files.fmt, ExportFmt::Wav, "WAV"), "Multi-channel 16-bit audio file (compact; Audacity, sigrok, Python). Units per step are in the file comment.");
                help(ui.selectable_value(&mut self.files.fmt, ExportFmt::Mf4, "MDF4"), "ASAM MDF 4.10 (.mf4), the automotive measurement standard: asammdf, CANape, INCA, MATLAB.");
                help(ui.selectable_value(&mut self.files.fmt, ExportFmt::Sr, "sigrok"), "sigrok session (.sr): opens in PulseView for its protocol decoders.");
            });
            ui.separator();
            let fmt = self.files.fmt.ext().to_uppercase();
            if ui.add_enabled(!busy, egui::Button::new(format!("Shown ({fmt})  Ctrl+E"))).on_hover_text("What is on screen: the shown capture, or the shown part of the stream.").clicked() {
                self.files.src = ExportSrc::Shown;
                self.files.export_req = true;
                ui.close();
            }
            // any stream recording still in memory counts, also after switching back to captures
            let secs = self.acq.history.lock().map_or(0.0, |h| if h.stream.rows() >= 2 { h.stream.rows() as f64 / h.stream.rate.max(1e-9) } else { 0.0 });
            let b = ui.add_enabled(!busy && secs > 0.0, egui::Button::new(if secs > 0.0 { format!("Whole stream recording, {} ({fmt})", fmt_time(secs)) } else { format!("Whole stream recording ({fmt})") }));
            if b.on_disabled_hover_text("No stream recording in memory. Streams are the slow, gap-free mode: pick a stream rate under Timebase (or a timebase of 200 ms/div and slower) and let it run; captures (faster timebases) are exported one at a time with 'Shown'.").clicked() {
                self.files.src = ExportSrc::Stream;
                self.files.export_req = true;
                ui.close();
            }
            let n_can = self.can_log_len();
            if ui.add_enabled(n_can > 0, egui::Button::new(format!("CAN log (.asc) — {n_can} frames"))).on_disabled_hover_text("Switch on CAN decoding (side panel: Protocol decoding, or its CAN guided test) and run: decoded frames are logged.").clicked() {
                self.export_can_log();
                ui.close();
            }
        });
        if ui.button("Screenshot (PNG)  F12").clicked() {
            self.files.shot_req = true;
            ui.close();
        }
        help(
            ui.checkbox(&mut self.shot_strip, "info strip under screenshots"),
            "Adds a thin line below the screenshot (time, timebase, trigger, channels, test, verdict) — the picture itself stays complete. The file name and the PNG metadata always carry the details.",
        );
        ui.separator();
        egui::containers::menu::SubMenuButton::new("References")
            .config(egui::containers::menu::MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside))
            .ui(ui, |ui| {
                let ch = self.sel_ch;
                for i in 0..2 {
                    let key = if i == 0 { "R" } else { "Shift+R" };
                    let b = ui.add_enabled(self.roll.is_none() && self.on[ch], egui::Button::new(format!("Save CH{} as R{}  {key}", ch + 1, i + 1)));
                    if b.on_hover_text("The selected channel (click its name or press 1…8) of the capture on screen. Also stored as a file in the library below, to load again any time.").clicked() {
                        self.ref_req = Some(i);
                        ui.close();
                    }
                }
                ui.separator();
                for i in 0..2 {
                    ui.menu_button(format!("Load into R{}", i + 1), |ui| self.ref_load_menu(ui, i));
                }
                if self.refs.iter().any(Option::is_some) {
                    ui.separator();
                    if ui.button(if self.refs_on { "Hide references  Ctrl+R" } else { "Show references  Ctrl+R" }).clicked() {
                        self.refs_on = !self.refs_on;
                    }
                    for i in 0..2 {
                        if self.refs[i].is_some() && ui.button(format!("Clear R{}", i + 1)).clicked() {
                            self.refs[i] = None;
                            self.save_ref(i);
                        }
                    }
                }
            });
        ui.separator();
        ui.checkbox(
            &mut self.files.events.on,
            "Event log (trend + matching captures)",
        );
        ui.separator();
        if ui.button("Settings…  F2").clicked() {
            self.settings_open = true;
            ui.close();
        }
        if ui.button("Keyboard shortcuts  F1").clicked() {
            self.show_keys = true;
            ui.close();
        }
    }
}

// regression suite, UI part (perf/TEST_PLAN.md): a child of files so it reaches the private UI items
#[cfg(test)]
#[path = "regress/ui.rs"]
mod regress_ui;

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> ExportData {
        ExportData {
            names: vec!["CH1_V".into(), "CH2_A".into()],
            cols: vec![vec![2048, 2058, 2038], vec![100, 200, 300]],
            conv: vec![(2048.0, 0.01), (0.0, 1.0)],
            t0: -0.001,
            dt: 0.0005,
            note: "test".into(),
        }
    }

    /// Sample files for checking with other tools: H8R_EXPORT_DIR=/dir cargo test --release export_samples -- --ignored
    #[test]
    #[ignore]
    fn export_samples() {
        let dir = PathBuf::from(std::env::var("H8R_EXPORT_DIR").unwrap());
        let d = ExportData {
            names: vec!["CH1_V".into(), "CH2_A".into()],
            cols: vec![
                (0..2000)
                    .map(|i| 2048 + ((i as f32 / 20.0).sin() * 500.0) as u16)
                    .collect(),
                (0..2000).map(|i| (i % 100) as u16).collect(),
            ],
            conv: vec![(2048.0, 0.01), (0.0, 0.5)],
            t0: -0.01,
            dt: 1e-5,
            note: String::new(),
        };
        write_mf4(&d, &dir.join("s.mf4"), &AtomicU32::new(0)).unwrap();
        write_sr(&d, &dir.join("s.sr"), &AtomicU32::new(0)).unwrap();
    }

    #[test]
    fn png_screenshot_roundtrip() {
        let p = std::env::temp_dir().join(format!("h8r-shot-{}.png", std::process::id()));
        let mut img = egui::ColorImage::new([3, 2], vec![Color32::BLACK; 6]);
        img.pixels[4] = Color32::from_rgb(10, 200, 30);
        write_png(&p, &img, &[("Title", "test".to_string())]).unwrap();
        let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&p).unwrap()));
        let mut r = dec.read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        let info = r.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (3, 2));
        assert_eq!(&buf[12..15], &[10, 200, 30]); // pixel (1, 1)
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn mf4_and_sr_export() {
        let dir = std::env::temp_dir().join(format!("h8r-export2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.mf4");
        write_mf4(&data(), &p, &AtomicU32::new(0)).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(&b[..8], b"MDF     ");
        assert_eq!(&b[64..68], b"##HD");
        // every block starts 8-byte aligned and its length stays inside the file
        let dt = b.windows(4).rposition(|w| w == b"##DT").unwrap();
        assert_eq!(dt % 8, 0);
        let len = u64::from_le_bytes(b[dt + 8..dt + 16].try_into().unwrap()) as usize;
        assert_eq!(len, 24 + 3 * (8 + 2 * 2)); // 3 rows × (time + 2 channels)
        assert!(dt + len <= b.len());
        let p = dir.join("a.sr");
        write_sr(&data(), &p, &AtomicU32::new(0)).unwrap();
        let z = std::fs::read(&p).unwrap();
        assert_eq!(&z[..4], &[0x50, 0x4b, 3, 4]);
        let t = String::from_utf8_lossy(&z);
        assert!(
            t.contains("samplerate=2000")
                && t.contains("analog2=CH2")
                && t.contains("analog-1-2-1")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn csv_and_wav_export() {
        let dir = std::env::temp_dir().join(format!("h8r-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.csv");
        write_csv(&data(), &p, &AtomicU32::new(0)).unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[2], "time_s,CH1_V,CH2_A");
        assert_eq!(lines[3], "-0.001000000,0.000,100.0");
        assert_eq!(lines[4], "-0.000500000,0.100,200.0");
        assert_eq!(lines.len(), 6);
        let p = dir.join("a.wav");
        write_wav(&data(), &p, &AtomicU32::new(0)).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(&b[..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize,
            b.len() - 8
        );
        assert_eq!(u16::from_le_bytes(b[22..24].try_into().unwrap()), 2); // channels
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 2000); // rate
        let d = b.windows(4).position(|w| w == b"data").unwrap() + 8;
        let s = |i: usize| i16::from_le_bytes([b[d + 2 * i], b[d + 2 * i + 1]]);
        assert_eq!((s(0), s(1), s(2), s(3)), (0, 800, 80, 1600));
    }
}

#[cfg(test)]
mod shot_tests {
    use super::*;

    #[test]
    fn strip_is_appended_below_and_wraps() {
        let img = egui::ColorImage::new([1400, 100], vec![egui::Color32::RED; 1400 * 100]);
        let lines = wrap_strip("LR Scope 1008C | 2026-11-22 21:14:03 UTC | 1 ms/div | trig CH6 falling 6 V | CH6 20 V/div x20", 40);
        assert!(
            lines.len() >= 3
                && lines
                    .iter()
                    .all(|l| l.chars().count() <= 40 || !l.contains(" | "))
        );
        let out = add_strip(&img, &lines);
        assert_eq!(out.size[0], 1400);
        assert!(out.size[1] > 100);
        // the screenshot itself is untouched
        assert!(out.pixels[..1400 * 100]
            .iter()
            .all(|&p| p == egui::Color32::RED));
        // and text was drawn into the strip
        assert!(out.pixels[1400 * 101..]
            .iter()
            .any(|&p| p == egui::Color32::from_gray(215)));
        assert_eq!(ascii("1 µs/div ×20 — ≈"), "1 us/div x20 - ~");
    }
}

#[cfg(test)]
mod shot_preview {
    use super::*;
    /// Writes a sample screenshot with the info strip to $SHOT_PREVIEW (to look at the font). Ignored by default.
    #[test]
    #[ignore]
    fn strip_preview() {
        let Ok(out) = std::env::var("SHOT_PREVIEW") else {
            return;
        };
        let img =
            egui::ColorImage::new([1200, 120], vec![egui::Color32::from_gray(40); 1200 * 120]);
        let t = "LR Scope 1008C | 2026-11-22 21:14:03 UTC | 1 ms/div | trig CH6 falling 6.000 V at 20 % auto | CH6 20 V/div ×20 (1008C attenuator) | test: Injector (voltage) | CH6: OK opens and closes";
        let img = super::add_strip(&img, &super::wrap_strip(t, 1200 / 12 - 1));
        let meta = [
            ("Title", "injector-CH6-OK".to_string()),
            (
                "Description",
                "CH6: 20 V/div; analysed as Injector (voltage): OK opens and closes".to_string(),
            ),
        ];
        super::write_png(&out.into(), &img, &meta).unwrap();
    }
}

#[cfg(test)]
mod ref_tests {
    use super::*;

    #[test]
    fn reference_csv_roundtrip_and_rejects() {
        let r = RefWave {
            name: "CH6 injector 2026-11-22".into(),
            unit: "V".into(),
            src: 5,
            saved_unix: 1.8e9,
            dt: 2.5e-6,
            trig_i: 800.25,
            v: vec![13.8, 0.4, 70.0, 13.9],
            under: 2,
            offset: -1.5,
            on: false,
        };
        let back = RefWave::from_csv(&r.to_csv()).unwrap();
        assert_eq!(
            (back.src, back.under, back.on, back.v.len()),
            (5, 2, false, 4)
        );
        assert_eq!(
            (back.name.as_str(), back.unit.as_str()),
            ("CH6 injector 2026-11-22", "V")
        );
        assert!(
            (back.dt - 2.5e-6).abs() < 1e-15
                && (back.trig_i - 800.25).abs() < 1e-9
                && (back.offset + 1.5).abs() < 1e-6
        );
        assert_eq!(back.v, r.v);
        // MC/DC on the checks: no dt / bad channel / too few values / garbage → None
        assert!(RefWave::from_csv(&r.to_csv().replace("# dt=", "# x=")).is_none());
        assert!(RefWave::from_csv(&r.to_csv().replace("source=CH6", "source=CH9")).is_none());
        assert!(
            RefWave::from_csv("# dt=1e-6\n# trigger_sample=0\n# source=CH1\nt,v\n0,1\n").is_none()
        );
        assert!(RefWave::from_csv("hello").is_none());
        // no 'under' line (hand-made file): drawn with its source channel
        assert_eq!(
            RefWave::from_csv(&r.to_csv().replace("# under=CH3\n", ""))
                .unwrap()
                .under,
            5
        );
    }
}

#[cfg(test)]
mod browse_tests {
    use super::*;

    #[test]
    fn library_names_are_readable() {
        assert_eq!(
            ref_label(std::path::Path::new(
                "/x/ref_2026-11-22_230132_capture-CH1.csv"
            )),
            "2026-11-22 23:01 UTC · capture · CH1"
        );
        assert_eq!(
            ref_label(std::path::Path::new(
                "ref_2026-11-22_221030_wheel-speed-CH2.csv"
            )),
            "2026-11-22 22:10 UTC · wheel-speed · CH2"
        );
        // anything else (a hand-made / renamed file): the file name as it is
        assert_eq!(
            ref_label(std::path::Path::new("my good injector.csv")),
            "my good injector"
        );
    }
}
