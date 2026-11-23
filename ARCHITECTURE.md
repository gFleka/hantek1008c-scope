# Architecture & dataflow

Flat on purpose: one binary, a handful of top-level files, no traits or layers until a second implementation actually exists.

| File | What it owns |
|---|---|
| `src/hantek.rs` | Everything device: types (`Config`, `Frame`, `VScale`, timebase ids), sample decoder, USB driver (`Hantek`) |
| `src/bench.rs` | `hantek1008c bench` — fixed speed scenarios → table + `perf/history.csv` |
| `src/main.rs` | CLI dispatch (later: GUI entry) |
| `src/acq.rs` | Acquisition thread + simulator (`--sim`) |
| `src/ui.rs` | egui app: controls, custom scope painter, status bar |
| `src/ui` children (`#[path]`, `impl App`) | `files.rs` (open/export), `presets.rs` (guided tests), `meter.rs`, `engine.rs` (TDC markers), `decode.rs` (LIN/K-line/CAN overlay, CAN log, fast 1-ch CAN), `waves.rs` (analysis overlay, typical waveforms), `testrun.rs` (test runs → report) |
| `src/dsp.rs`, `src/analyze.rs` | Pure, tested signal code: filters, FFT, rpm, crank teeth, serial decoders / phase analyzers per signal type |
| `src/rec.rs` | `.h8r` recordings (writer thread + RAM spool) |

Test runs: on each new capture (once per screen in streams) `test_tick` runs the analyzers of the analysed channels and
accumulates counts / readings; nothing runs when no test is active, so the plot path is unchanged.

## Dataflow

```
              Settings (Config)                           latest Frame
  ┌────────┐  crossbeam channel   ┌───────────────────┐  triple buffer   ┌──────────────┐
  │ UI     │ ───────────────────► │ acq thread        │ ───────────────► │ UI           │
  │ thread │                      │ (owns Hantek)     │  (lock-free,     │ plot painter │
  │ egui   │ ◄─── request_repaint │                   │   drops stale)   │              │
  └────────┘                      └─────────┬─────────┘                  └──────────────┘
                                            │ bulk OUT cmd / bulk IN reply, 64-byte packets
                                            ▼
                                   ┌─────────────────┐
                                   │ Hantek 1008C    │  USB full speed
                                   └─────────────────┘
```

### 1. Wire (device ↔ `Hantek`)
- Every command is one bulk OUT write `[cmd, params…]` followed (usually) by one bulk IN read. Strictly request/response — the round-trip count per frame is what limits speed.
- Burst capture (`Hantek::capture`): `[f3] [e4 e6] a4 01, c0, c2, a5 5a…(until ready), c6 02 → len, a6 02 ×⌈len/64⌉, c6 03 → len, a6 03 ×⌈len/64⌉ [e4 e6]`. `[…]` = optional, controlled by `Timing`.
- Roll mode (`roll_start`, `roll_poll`): `f3, c7 → len, c8 ×⌈len/64⌉` per poll; includes an extra 9th column that gets dropped.
- Settings: `configure()` diffs against what was last sent and only transmits changed fields (`a0/aa` channels, `a2` ranges, `a3` timebase, `c1` trigger src/slope, `ab` level).
- **Measured device constraints** (perf/REVIEW.md #1, 2026-11-20):
  - USB full speed, 64-byte bulk packets; ~0.18 ms per request/response round-trip. A burst frame (4000 samples total) is ~150 round-trips ≈ 26 ms → ~38 frames/s ceiling at fast timebases.
  - **No command pipelining**: a second request before the previous reply has been read times out (and repeated attempts can knock the scope off the bus). Every packet costs a full round-trip.
  - No sleeps are needed anywhere; the device blocks its replies until ready.
  - **Hardware range switch (`a2`) takes ~213 ms** inside the device.
  - Ready polling (`a5 5a`) is paced: for acquisitions ≥ 20 ms the driver sleeps for the acquisition time first (−83 % USB traffic at 10 ms/div, same frame rate).

### 2. Decode (`decode_append`)
- Bytes are little-endian u16, interleaved by active channel (ascending index). One pass, writes straight into the `Frame`'s per-channel `Vec<u16>`s. Buffers are reused, so nothing is allocated once warm.
- Raw stays raw (`u16`, 12-bit) through the pipeline; conversion to volts happens only at draw/measurement time, using `zero[range][ch]` (from init) and the per-range gain (calibration TBD).

### 3. Acquisition thread (`acq.rs`)
- Owns the `Hantek` and is the only code that touches USB. Loop: drain the settings channel → `configure()` → `capture()` (or `roll_poll()`) → publish the frame into the triple buffer → `ctx.request_repaint()`.
- Run/Stop/Single live here; while stopped it sends `f3` keep-alive pings every 10 ms (as the reference does when paused). Auto/Normal trigger mode: not yet — the device currently always completes a capture (trigger behaviour is an RE task).
- Roll mode keeps the last `ROLL_KEEP` rows and is restarted after any settings change (configure re-sends the burst timebase).
- Any USB error → drop the device, show the status, retry open+init every 0.5 s (hot replug).
- Publishes acquisition frames/s and USB ms/frame along with the frame. The status string is copied into every published buffer (triple buffer rotates 3 copies).

### 3a′. Trigger jitter correction
- The 1008C knows its trigger point only to a few samples (1–7 samples, random per capture; `re/PROTOCOL.md`). For every
  hardware-triggered burst the acq thread looks for the real trigger-level crossing within ±12 samples of the nominal
  point (`hantek::trigger_offset`, ~40 ns) and stores it in `Snapshot::trig_off`; `App::trace` shifts that capture by
  it when Settings → "trigger jitter correction" is on (default). History, persistence, cursors and math use the same
  mapping, so they follow.

### 3b. Capture history (`acq.rs` → `History`)
- Third data path (review #5): acq → `Arc<Mutex<History>>` → UI. Every triggered capture (snapshot + timestamp) is appended after publishing; the lock is held only for the push / a lookup. Unlike the triple buffer, nothing is dropped.
- One memory limit (default 512 MB ≈ 64 000 8-channel captures ≈ 30 min; Settings, 64 MB … 75 % of RAM) shared by the captures and the stream recording (`StreamRec`, appended by acq in roll mode). `History::trim`: whatever is recorded now pushes out the other kind first, then its own oldest data (stream rows in 10 % steps, so the buffer is not moved per row); captures keep stable numbers (`first_no`), stream rows stay absolute (`dropped`).
- UI: when stopped, the plot shows any stored capture (navigator, slider, keys); statistics (min/mean/max/σ of frequency, amplitude, Vpp) over a user-selected range, default the whole history, computed once per range change.
- Stream mode feeds `StreamRec` (replaced at the first rows of a new stream, so a recording opened from a file stays browsable while stopped).

### 3c. Recording to disk (`acq.rs` → `rec.rs` writer thread → `.h8r` file)
- Fourth data path (Phase 3): in stream mode acq copies each poll's new rows (outside the History lock) into a `RecMsg::Rows` and `try_send`s it into a bounded channel (4096 blocks ≈ 18 s of disk stall). acq never blocks on the disk; a full channel counts lost blocks (shown in the info line).
- Writer thread (`rec-writer`): Start (header from acq: rate, ranges, zero offsets; + calibration and probes the UI sent as `Meta`) → chunks of 4096 rows or 2 s, each flushed whole with a checksum, fsync every 30 s → Stop. Disk guard every 8 chunks: rotate at 2 GB, delete the oldest other recordings above the folder limit, stop below keep-free (statvfs, Unix).
- File: magic + text header + chunks (first row, rows, per-channel min/max, channel-planar u16, FNV-1a). The chunk headers are the index; the reader seeks over them, loads the newest part that fits the memory limit and stops at a torn last chunk.
- UI side lives in `files.rs` (child module of `ui`): Settings section, export (CSV / WAV in a background thread, progress in the info line), open a file → `StreamRec` → the existing stream navigator.

### 4. UI thread (`ui.rs`)
- Each repaint takes the newest frame from the triple buffer (never blocks, never queues).
- Painter: per channel, min/max into one bucket per pixel column → one polyline. Cost is O(samples + width).
- Repaints only on a new frame or user input. A setting change goes out on the channel immediately and shows up in the next captured frame.
- **V/div is a software zoom** on the raw data within the current hardware range (instant). The hardware range (3 per channel) changes automatically only when the trace clips or uses too little of the ADC span, debounced, because each switch costs ~213 ms. (Approved in review #1.) Implemented so far: the range is the smallest one whose span covers the screen (V/div × 4 div + offset), so it only switches when V/div/offset crosses a range boundary; clip-based switching still to do.
- Volts = (raw − zero[range][ch]) × 0.01 × range factor (0.02 / 0.125 / 1.0), as in hantek1008py — uncalibrated.
- Measured (2026-11-20, Intel HD 630, Vulkan): 60 fps (vsync), plot paint ≈ 0.02 ms for 4 × 1000 samples.

### 4a. UI child modules (flat: each an `impl App` block in its own file, declared with `#[path]` in ui.rs)
- `files.rs` — Settings > recording/event log/open/export, the Menu, screenshot, references on disk, event logger.
- `presets.rs` — guided tests (data table + one apply function + guide window).
- `meter.rs` — meter mode (big numbers + trend per channel, 4 updates/s, same `measure()`).
- `engine.rs` — TDC markers from the crank gap (`dsp::crank_teeth`), cam phase for the 720° cycle.
- Adaptive feature dropout (in ui.rs): per-frame CPU work → `Dropout` controller → `App::dropped(level)` checked by
  persistence, math/FFT, references, statistics, jitter correction (one flag each).

## Speed & architecture review loop
- Every commit touching the driver, acq or plot path runs `bench` 3× (`perf/sweep.sh`) and quotes the delta in its message. `perf/check.sh` compares medians of the last 3 runs with the 3 before and flags >5 % regressions (single runs vary ±5 %).
- Every 5 commits / each milestone: a review entry in `perf/REVIEW.md` (numbers, profile, open questions, proposed changes to this document). **Work pauses until the owner has reviewed and signed off the entry**, so architecture changes go through the owner.
