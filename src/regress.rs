//! Automatic regression suite (perf/TEST_PLAN.md). Runs on the simulator only: `cargo test --release`
//! or `scripts/regress.sh`. Layers:
//! - `logic`, `dsp`, `history`, `recfile` — data-driven MC/DC tables for the pure decision functions
//! - `sim` — the real acquisition thread on the simulator (modes, timebases, channel counts, Run/Stop/Single,
//!   stream recording to disk)
//! - UI logic + headless App scenarios live in `src/regress/ui*.rs` (declared from `ui/files.rs`, so they can
//!   reach the private UI items without changing their visibility).
//!
//! A case = one row in a table: `(name, inputs…, expected)`. Failures name the row.

mod dsp;
mod history;
mod logic;
mod recfile;
mod sim;
pub(crate) mod util;
