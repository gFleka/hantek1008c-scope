//! UI part of the regression suite. Declared from `ui/files.rs` (`#[cfg(test)] #[path] mod regress_ui;`), i.e.
//! a child of `ui::files`, so it reaches the private UI and files items without any visibility change.
//! - `logic`: MC/DC tables for UI decision functions, codecs (settings garbage), formatters, exporters
//! - `app`: the real `ui::App` on the simulator, driven frame by frame without a window (no GPU)

#[path = "ui_app.rs"]
mod app;
#[path = "ui_logic.rs"]
mod logic;
