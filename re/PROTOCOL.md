# Hantek 1008C USB protocol (living spec)

Sources: hantek1008py (starting map), our own on-device experiments (`hantek1008c probe` / `check`),
and a usbmon capture of the vendor software (Win10 VM, 2026-11-20, `re/captures/vendor_session1.pcapng`,
decoded with `re/decode.py`). ✔ = confirmed on this unit, ? = unverified.

## Transport
- VID:PID `0783:5725` ("YDJ-2088"), USB full speed, bulk OUT `0x02`, bulk IN `0x81`, 64-byte packets. ✔
- Strict request/response: one command, one reply; a second request before the reply is read times out. ✔
- Most commands echo their id as the first reply byte; data/status queries do not. ✔

## Commands
| cmd | params | reply | meaning |
|---|---|---|---|
| `a0 n` | channel count | echo | number of active channels (vendor always sends 8) ✔ |
| `aa` ×8 | 0/1 per channel | echo | active channel map ✔ |
| `a2` ×8 | range id 1/2/3 per channel | echo, **~213 ms** | hardware range (×0.02 / ×0.125 / ×1, gains uncalibrated) ✔ |
| `a3 id` | timebase id | echo | 0x00–0x17 burst (1 ns…50 ms/div by 1-2-5 id, see `burst_ns_per_div`), 0x18–0x24 slow/scan ✔ |
| `ac` | `pos:u16 pre_us:u24 post_us:u24` (BE) | echo | **capture window + trigger position**. `pos` = trigger position in ‰ of screen (500 = centre); `pre = (W−2)·pos/1000 + 1`, `post = W − pre`, `W` = 10 div + 2 µs, min 1668 µs (4000 samples at 2.4 MS/s). Vendor re-sends after every `a3` and while dragging the trigger position. ≥ 100 ms/div (scan): `ac pos 1 1`. ✔ (W at 200 µs/div: vendor sent 1402 — ?) |
| `c1 src slope` | channel 0–7, 0 = rising / 1 = falling | echo | trigger source/slope ✔ |
| `ab hi lo` | 12-bit level, BE, raw ADC counts | echo | trigger level (vendor 0 V = zero offset, e.g. `07 c6`) ✔ |
| `a7 00 00` | | echo + `00` | sent at init; other values had no visible effect ? |
| `a4 01` | | echo | arm a burst capture (vendor only ever uses 01; `00` = never arms; `02` = roll) ✔ |
| `c0` | | echo | start acquisition after `a4` ✔ |
| `c2` | | echo | **force trigger** (capture now). Vendor: init zero-offset captures, and in Auto mode after ~50 ms without a trigger ✔ |
| `a5 5a` | | echo + status | **0 = armed, waiting for trigger · 1 = triggered, filling · 2 = done** (3 never seen) ✔ |
| `c6 bank` | bank 2 / 3 | len u16 BE | bytes waiting in bank ✔ |
| `a6 bank` | | 64 B | next 64 bytes of bank (u16 LE samples, channel-interleaved) ✔ |
| `e4 01` / `e6 01` | | echo / 10 B | vendor brackets every capture with these; 10 B ≈ 5 × u16 ~0x06d9 (temps/refs?) — not needed ✔ |
| `f3` | | echo | keep-alive; vendor pings between `a5` polls while waiting ✔ |
| `c7` / `c8` | | len / 64 B | roll mode data (hantek1008py) ✔ |
| `c9` / `ca` | | len u16 / 64 B | **vendor slow/scan mode data** (≥ 100 ms/div), replaces c7/c8 ? |
| `b0 b5 b6 e5 f5 f6 f7 f8 fa e9 b7 b9 bb` | | | init / calibration reads / generator — see `Hantek::init` |

## Capture loop (vendor)
`f3 · e4/e6 · a4 01 · c0 · a5 5a … (until 2; Auto: c2 after ~50 ms of 0) · c6 02 · a6 02 ×n · c6 03 · a6 03 ×n · e4/e6`

## Sample rate (this unit, CH1 = 1 kHz generator, `check`)
- ≤ 100 µs/div: fixed 2.4 MS/s aggregate (window 1.67 ms whatever the setting).
- 8 channels: 500 µs, 1 ms, 2 ms/div exact; 5 ms/div 0.9375×; 10 ms/div 0.925×; 200 µs/div ~0.3×.
- Fewer than 8 channels: 500 µs exact, 1 ms / 2 ms / 200 µs / 10 ms off and unstable → vendor always uses all 8 (`a0 08`).

## Slow timebases (vendor capture, 2026-11-20)
- **100 / 200 ms/div** (`a3 18/19`): normal burst capture, `ac <pos> 000001 000001`; the capture just takes 1–2 s
  (banks e.g. 7200 + 800 bytes = still 4000 samples).
- **≥ 500 ms/div** (`a3 1a…23`): **progressive scan** — arms once per timebase change (`ac 0000·000001·000001`, `a4 01`, `c0`), then streams `c9`/`ca` continuously without re-arming (~78–100 rows/s at 500 ms/div, 8 ch); i.e. a stream with a triggered start. After arming (`a4 01`, `c0`) the vendor polls `f3 · a5 5a`
  (0 = waiting for trigger). Once `a5` = 1 (triggered, filling) it streams while filling:
  `f3 · c9` → u16 BE bytes ready (16 = one row of 8 × u16 LE samples), then `ca` → 64-byte packet holding those bytes.
  Rows arrive as they are acquired, so the trace can be drawn left → right live.

## Channel timing (deskew, 2026-11-20, CH1+CH2 on the 1 kHz generator, 40 bursts per timebase)
- The active channels are converted one after another by one ADC, back to back at 2.4 MS/s, at every timebase: CH2 − CH1 = 0.42 µs (10 µs … 2 ms/div; medians 0.40–0.53 µs). Not simultaneous; not spread over the per-channel period.
- Correction used by the app: channel at scan index k is sampled k / 2.4 MHz after the first active one (CH3–CH8 extrapolated from the model).
- Same signal on CH1/CH2: 1-count offset, no measurable gain difference; CH2 settles ~10 µs slower after an edge.

## Trigger point jitter (measured 2026-11-21, `probe --jitter`)
8 channels, CH1 on the 1 kHz generator, rising edge at mid level, position 50 %, Normal mode, 200 captures per timebase.
Offset = interpolated trigger-level crossing − pos × samples:
| timebase | sample | offset mean | spread (p-p) | pattern |
|---|---|---|---|---|
| 10–100 µs/div (fixed 1668 µs window, 500 samples) | 3.34 µs | −2.1 … −2.5 | 5.6–6.5 samples ≈ 19–22 µs | two groups: ~75 % at −1…−2, ~25 % at −4…−7 samples; random from capture to capture (lag-1 ≈ 0) |
| 500 µs/div | 10 µs | −1.8 | 2.7 samples ≈ 27 µs | mostly −1…−2, some −3 |
| 1 ms/div | 20 µs | −1.7 | 1.6 samples | −1.5 / −2.5 (one-sample steps) |
| 5 ms/div | 107 µs | −1.8 | 1.5 samples | −1.5 / −2.5 |
- The crossing is always 1–2 samples *before* the nominal trigger point (constant bias ≈ −1.7 samples).
- ±½ sample is the unavoidable sampling phase of a sharp edge. At the fast timebases a quarter of the captures trigger
  3–5 samples (10–17 µs) late — the hardware trigger decision is not tied to a single sample there.
- Consequence: the scope's trigger position is only good to a few samples → the GUI must align each capture on the
  crossing it can see in the data (software trigger-point correction).
