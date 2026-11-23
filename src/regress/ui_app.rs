//! Layer 3: the real `ui::App` (simulator) driven frame by frame without a window or GPU: an `egui::Context`
//! plus `eframe::CreationContext::_new_kittest` (public in eframe 0.37), then per frame `raw_input_hook` +
//! `Context::run_ui` → `App::ui`, exactly as eframe does it. No dev-dependency needed (egui_kittest would add
//! a renderer and accesskit for what is plain input here). Every test runs in an isolated HOME / config
//! folder (`util::isolated`), one at a time.

use super::super::*;
use crate::regress::util::{isolated, real_config_dir, wait_until, Isolated};
use eframe::egui::{self, Event, Key, Modifiers, PointerButton};
use std::time::{Duration, Instant};

const SCREEN: Vec2 = Vec2::new(1400.0, 850.0);

struct H {
    app: App,
    ctx: egui::Context,
    frame: eframe::Frame,
    t0: Instant,
    mods: Modifiers,
    pointer: Pos2,
    iso: Isolated,
}

fn harness(name: &str, settings: Option<&str>) -> H {
    let iso = isolated(name);
    if let Some(s) = settings {
        iso.write("settings.txt", s);
    }
    let ctx = egui::Context::default();
    let app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()), true);
    let mut h = H {
        app,
        ctx,
        frame: eframe::Frame::_new_kittest(),
        t0: Instant::now(),
        mods: Modifiers::NONE,
        pointer: Pos2::new(5.0, 5.0),
        iso,
    };
    h.step(vec![]);
    h
}

impl H {
    /// One frame with these input events.
    fn step(&mut self, events: Vec<Event>) {
        // modifiers reach egui as ModifiersChanged events (see `key`)
        let mut raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SCREEN)),
            time: Some(self.t0.elapsed().as_secs_f64()),
            focused: true,
            events,
            ..Default::default()
        };
        eframe::App::raw_input_hook(&mut self.app, &self.ctx, &mut raw);
        let (app, frame) = (&mut self.app, &mut self.frame);
        let _ = self.ctx.run_ui(raw, |ui| eframe::App::ui(app, ui, frame));
    }
    /// Frames for `secs` of real time (~60 fps).
    fn run(&mut self, secs: f32) {
        let t = Instant::now();
        while t.elapsed().as_secs_f32() < secs {
            self.step(vec![]);
            std::thread::sleep(Duration::from_millis(16));
        }
    }
    /// Frames until `cond` holds (max `secs`).
    fn until(&mut self, secs: f32, mut cond: impl FnMut(&App) -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed().as_secs_f32() < secs {
            self.step(vec![]);
            if cond(&self.app) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        false
    }
    /// Press and release a key (with modifiers), one frame each.
    fn key(&mut self, key: Key, mods: Modifiers) {
        self.mods = mods;
        let ev = |pressed| Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers: mods,
        };
        self.step(vec![Event::ModifiersChanged(mods), ev(true)]);
        self.step(vec![ev(false)]);
        self.mods = Modifiers::NONE;
        self.step(vec![Event::ModifiersChanged(Modifiers::NONE)]);
    }
    fn press(&mut self, key: Key) {
        self.key(key, Modifiers::NONE);
    }
    fn pointer_to(&mut self, p: Pos2) {
        self.pointer = p;
        self.step(vec![Event::PointerMoved(p)]);
    }
    fn button(&mut self, pressed: bool) {
        let p = self.pointer;
        self.step(vec![Event::PointerButton {
            pos: p,
            button: PointerButton::Primary,
            pressed,
            modifiers: self.mods,
        }]);
    }
    /// The plot pane's rect: the largest click+drag widget under a point of the graph area.
    fn plot_rect(&mut self) -> Rect {
        self.pointer_to(Pos2::new(300.0, 200.0));
        self.step(vec![]);
        let ids = self.ctx.interaction_snapshot(|s| s.hovered.clone());
        ids.iter()
            .filter_map(|&id| self.ctx.read_response(id))
            .filter(|r| r.sense.senses_drag() && r.rect.contains(Pos2::new(300.0, 200.0)))
            .map(|r| r.rect)
            .max_by(|a, b| a.area().total_cmp(&b.area()))
            .expect("plot rect under the pointer")
    }
    /// Wait for live captures on screen.
    fn live(&mut self) {
        assert!(
            self.until(5.0, |a| a.last_seq > 2),
            "no captures from the simulator"
        );
    }
    fn history_len(&self) -> usize {
        self.app.acq.history.lock().unwrap().len()
    }
}

const SHIFT: Modifiers = Modifiers::SHIFT;
const CTRL: Modifiers = Modifiers::COMMAND;

// ───────────────────────────── keyboard ─────────────────────────────

#[test]
fn keys_run_stop_single() {
    let mut h = harness("keys-run", None);
    h.live();
    assert_eq!(h.app.run, Run::Running);
    h.press(Key::Space);
    assert_eq!(h.app.run, Run::Stopped);
    h.run(0.2);
    let n = h.history_len();
    h.run(0.4);
    assert_eq!(h.history_len(), n, "Space stopped the acquisition");
    h.press(Key::Space);
    assert_eq!(h.app.run, Run::Running);
    assert!(h.until(2.0, |a| a.acq.history.lock().unwrap().len() > n + 2));
    // S: single → the UI follows the acq thread back to Stopped after one capture
    h.press(Key::S);
    assert_eq!(h.app.run, Run::Single);
    assert!(
        h.until(3.0, |a| a.run == Run::Stopped),
        "single ends in Stopped"
    );
}

#[test]
fn keys_timebase_skips_broken_ids() {
    let mut h = harness("keys-tb", None);
    #[rustfmt::skip]
    let cases: &[(u8, Key, u8)] = &[
        (17, Key::Comma, 15),   // 500 µs → 100 µs: 200 µs (16) skipped
        (15, Key::Period, 17),
        (22, Key::Period, 24),  // 20 ms → 100 ms: 50 ms (23) skipped
        (24, Key::Comma, 22),
        (0, Key::Comma, 0),     // fastest stays
        (TB_MAX, Key::Period, TB_MAX),
        (18, Key::Period, 19),
    ];
    for &(from, key, to) in cases {
        h.app.tb_id = from;
        h.step(vec![]);
        h.press(key);
        assert_eq!(h.app.tb_id, to, "{from} {key:?}");
    }
    // slow timebases force streaming, fast ones leave it
    h.app.tb_id = 27;
    h.run(0.1);
    assert!(h.app.roll.is_some(), "1 s/div streams");
    h.app.tb_id = 17;
    h.run(0.1);
    assert!(h.app.roll.is_none());
}

#[test]
fn keys_digits_select_and_shift_toggles() {
    let mut h = harness("keys-digits", None);
    assert_eq!(
        h.app.on,
        [true, true, false, false, false, false, false, false]
    );
    h.press(Key::Num3);
    assert_eq!(h.app.sel_ch, 2);
    h.key(Key::Num3, SHIFT);
    assert!(h.app.on[2], "Shift+3 shows CH3");
    assert_eq!(h.app.sel_ch, 2, "Shift+digit doesn't select");
    h.key(Key::Num3, SHIFT);
    assert!(!h.app.on[2], "Shift+3 hides CH3");
    // the last shown channel can't be hidden
    h.key(Key::Num2, SHIFT);
    assert_eq!(h.app.on[..2], [true, false]);
    h.key(Key::Num1, SHIFT);
    assert!(h.app.on[0], "CH1 is the only one: stays on");
    h.press(Key::Num8);
    assert_eq!(h.app.sel_ch, 7);
}

#[test]
fn keys_vdiv_offset_trigger() {
    let mut h = harness("keys-arrows", None);
    h.press(Key::Num1);
    let v = h.app.vdiv[0];
    h.press(Key::ArrowUp);
    assert_eq!(h.app.vdiv[0], v + 1);
    h.press(Key::ArrowDown);
    h.press(Key::ArrowDown);
    assert_eq!(h.app.vdiv[0], v - 1);
    // bounds
    h.app.vdiv[0] = VDIVS.len() - 1;
    h.press(Key::ArrowUp);
    assert_eq!(h.app.vdiv[0], VDIVS.len() - 1);
    h.app.vdiv[0] = 0;
    h.press(Key::ArrowDown);
    assert_eq!(h.app.vdiv[0], 0);
    h.app.vdiv[0] = 9;
    // Shift+↑: offset +¼ div, clamped at ±4
    h.key(Key::ArrowUp, SHIFT);
    assert_eq!(h.app.offset[0], 0.25);
    assert_eq!(h.app.vdiv[0], 9, "Shift+↑ doesn't change V/div");
    h.app.offset[0] = 3.9;
    h.key(Key::ArrowUp, SHIFT);
    assert_eq!(h.app.offset[0], 4.0);
    // Ctrl+↑/↓: trigger level by a tenth of a division; Ctrl+←/→ trigger position ±5 %, clamped
    let tv = h.app.trig_v;
    let step = h.app.udiv(0) / 10.0;
    h.key(Key::ArrowUp, CTRL);
    assert!((h.app.trig_v - tv - step).abs() < 1e-5);
    h.app.trig_pos = 980;
    h.key(Key::ArrowRight, CTRL);
    assert_eq!(h.app.trig_pos, 1000);
    h.app.trig_pos = 20;
    h.key(Key::ArrowLeft, CTRL);
    assert_eq!(h.app.trig_pos, 0);
}

#[test]
fn keys_windows_and_modes() {
    let mut h = harness("keys-windows", None);
    #[rustfmt::skip]
    let toggles: &[(&str, Key, Modifiers, fn(&App) -> bool)] = &[
        ("F2 settings",    Key::F2, Modifiers::NONE, |a| a.settings_open),
        ("F1 shortcuts",   Key::F1, Modifiers::NONE, |a| a.show_keys),
        ("H shortcuts",    Key::H,  Modifiers::NONE, |a| a.show_keys),
        ("M meter",        Key::M,  Modifiers::NONE, |a| a.meter.on),
        ("P persistence",  Key::P,  Modifiers::NONE, |a| a.persist_on),
        ("C cursors",      Key::C,  Modifiers::NONE, |a| a.cursors.on),
        ("Ctrl+R refs",    Key::R,  CTRL,            |a| a.refs_on),
    ];
    for &(name, key, mods, get) in toggles {
        let before = get(&h.app);
        h.key(key, mods);
        assert_eq!(get(&h.app), !before, "{name} toggles");
        h.key(key, mods);
        assert_eq!(get(&h.app), before, "{name} toggles back");
    }
    // Ctrl+M is not the meter
    h.key(Key::M, CTRL);
    assert!(!h.app.meter.on);
    // Esc closes settings + shortcut list + FFT windows
    h.app.math[1].op = MathOp::Fft;
    h.press(Key::F2);
    h.press(Key::F1);
    assert!(h.app.settings_open && h.app.show_keys);
    h.press(Key::Escape);
    assert!(!h.app.settings_open && !h.app.show_keys);
    assert_eq!(h.app.math[1].op, MathOp::Off);
    // Ctrl+E asks for an export, F12 for a screenshot (handled by files_tick)
    h.key(Key::E, CTRL);
    h.press(Key::F12);
    h.run(0.1);
}

#[test]
fn keys_tab_only_with_cursors() {
    let mut h = harness("keys-tab", None);
    // cursors off: Tab is egui's (focus), not taken
    h.press(Key::Tab);
    assert!(!h.app.cursors.on && h.app.cursors.active == 0);
    // (egui moved the focus into a field: shortcuts pause while a field has it — give it back)
    h.ctx.memory_mut(|m| {
        if let Some(id) = m.focused() {
            m.surrender_focus(id);
        }
    });
    h.press(Key::C);
    assert!(h.app.cursors.on);
    for want in [1, 2, 3, 0, 1] {
        h.press(Key::Tab);
        assert_eq!(h.app.cursors.active, want, "Tab cycles T1 T2 V1 V2");
    }
    // settings open: Tab belongs to the fields again
    h.press(Key::F2);
    let a = h.app.cursors.active;
    h.press(Key::Tab);
    assert_eq!(h.app.cursors.active, a);
    // Alt+arrows move the active cursor
    h.press(Key::Escape);
    h.app.cursors.active = 0;
    let t = h.app.cursors.t[0];
    h.key(Key::ArrowRight, Modifiers::ALT);
    assert!(h.app.cursors.t[0] > t);
}

#[test]
fn keys_zoom_and_backspace_undo() {
    let mut h = harness("keys-zoom", None);
    h.live();
    let full = Zoom::default();
    h.press(Key::Plus);
    assert!(
        (h.app.zoom.x1 - h.app.zoom.x0 - 0.5).abs() < 1e-5,
        "+ halves the time span"
    );
    assert_eq!((h.app.zoom.y0, h.app.zoom.y1), (0.0, 1.0));
    // '-' zooms out both axes
    h.app.zoom = Zoom {
        x0: 0.25,
        x1: 0.75,
        y0: 0.25,
        y1: 0.75,
    };
    h.press(Key::Minus);
    assert_eq!(h.app.zoom, full, "− zooms out time and volts");
    h.press(Key::Plus);
    h.press(Key::Plus);
    let z2 = h.app.zoom;
    h.press(Key::Num0);
    assert_eq!(h.app.zoom, full);
    // Backspace undoes the last zoom step
    h.press(Key::Backspace);
    assert_eq!(h.app.zoom, z2);
    // after an auto-set, Backspace undoes the auto-set first (and not the zoom)
    h.app.tb_id = 20;
    h.app.vdiv[0] = 3;
    h.step(vec![]);
    let undo_len = h.app.zoom_undo.len();
    h.press(Key::A);
    assert!(h.app.autoset.is_some());
    assert!(
        h.until(6.0, |a| a.autoset.is_none()),
        "auto-set finishes on the simulator"
    );
    assert!(
        h.app
            .autoset_msg
            .as_ref()
            .is_some_and(|m| m.0.starts_with("Auto-set:")),
        "{:?}",
        h.app.autoset_msg
    );
    h.press(Key::Backspace);
    assert_eq!(
        (h.app.tb_id, h.app.vdiv[0], h.app.zoom),
        (20, 3, z2),
        "auto-set undone"
    );
    assert_eq!(h.app.zoom_undo.len(), undo_len, "zoom stack untouched");
    h.press(Key::Backspace);
    assert_ne!(h.app.zoom, z2, "next Backspace is the zoom undo again");
}

/// Number keys select channels; with Shift held (physical key) they toggle — even on layouts where
/// Shift+1 types "!" (the hook matches the physical key).
#[test]
fn shift_digit_uses_the_physical_key() {
    let mut h = harness("keys-physical", None);
    let ev = Event::Key {
        key: Key::Exclamationmark,
        physical_key: Some(Key::Num4),
        pressed: true,
        repeat: false,
        modifiers: SHIFT,
    };
    h.mods = SHIFT;
    h.step(vec![Event::ModifiersChanged(SHIFT), ev]);
    h.mods = Modifiers::NONE;
    h.step(vec![Event::ModifiersChanged(Modifiers::NONE)]);
    assert!(h.app.on[3], "CH4 toggled by Shift + physical 4");
}

// ───────────────────────────── pointer ─────────────────────────────

/// Grabbing a channel handle moves the trace in the very first drag frame (egui's drag threshold movement
/// is applied too), and selects the channel.
#[test]
fn drag_channel_handle_starts_instantly() {
    let mut h = harness("drag-handle", None);
    h.live();
    let r = h.plot_rect();
    let dyv = r.height() / DIV_Y;
    let hy = r.center().y; // CH1 at offset 0
    h.app.sel_ch = 5;
    h.pointer_to(Pos2::new(r.left() + 6.0, hy));
    h.button(true);
    h.pointer_to(Pos2::new(r.left() + 6.0, hy + 12.0));
    assert_eq!(h.app.drag, Some(Drag::Channel(0)), "handle grabbed");
    assert_eq!(h.app.sel_ch, 0, "grabbing selects the channel");
    assert!(
        h.app.offset[0] <= -12.0 / dyv + 0.02,
        "moved in the first frame: {}",
        h.app.offset[0]
    );
    let o1 = h.app.offset[0];
    h.pointer_to(Pos2::new(r.left() + 6.0, hy + 12.0 + dyv));
    assert!(
        (h.app.offset[0] - o1 + 1.0).abs() < 0.02,
        "then 1:1 with the pointer: {}",
        h.app.offset[0]
    );
    h.button(false);
    h.step(vec![]);
    assert_eq!(h.app.drag, None);
    // CH2's handle sits next to CH1's (same 0 V line): x + 20
    h.app.offset = [0.0; CHANNELS];
    h.step(vec![]);
    h.pointer_to(Pos2::new(r.left() + 26.0, hy));
    h.button(true);
    h.pointer_to(Pos2::new(r.left() + 26.0, hy - 12.0));
    assert_eq!(h.app.drag, Some(Drag::Channel(1)));
    h.button(false);
    h.step(vec![]);
}

/// The trace should stay under the pointer: after dragging the handle by N px it has moved N px. In the frame the
/// drag starts, plot_pane applies the movement since the press *and* `drag_delta` of that frame (`resp.dragged()`
/// is already true), so that frame's movement counts twice (12 px in one frame → the trace moves 24 px).
#[test]
fn drag_channel_handle_follows_the_pointer_exactly() {
    let mut h = harness("drag-exact", None);
    h.live();
    let r = h.plot_rect();
    let dyv = r.height() / DIV_Y;
    let hy = r.center().y;
    h.pointer_to(Pos2::new(r.left() + 6.0, hy));
    h.button(true);
    for k in 1..=10 {
        h.pointer_to(Pos2::new(r.left() + 6.0, hy + 3.0 * k as f32));
    }
    let want = -30.0 / dyv;
    assert!(
        (h.app.offset[0] - want).abs() < 0.5 / dyv,
        "30 px down → {} div, want {want}",
        h.app.offset[0]
    );
    h.button(false);
}

/// Drag on empty graph area = box zoom on release, both axes; tiny boxes are ignored.
#[test]
fn drag_box_zoom() {
    let mut h = harness("drag-box", None);
    h.live();
    let r = h.plot_rect();
    let at = |fx: f32, fy: f32| Pos2::new(r.left() + fx * r.width(), r.top() + fy * r.height());
    h.pointer_to(at(0.3, 0.1));
    h.button(true);
    h.pointer_to(at(0.45, 0.2));
    h.pointer_to(at(0.6, 0.35));
    h.button(false);
    h.step(vec![]);
    let z = h.app.zoom;
    assert!(
        (z.x0 - 0.3).abs() < 0.02 && (z.x1 - 0.6).abs() < 0.02,
        "{z:?}"
    );
    assert!(
        (z.y0 - 0.1).abs() < 0.02 && (z.y1 - 0.35).abs() < 0.02,
        "{z:?}"
    );
    // a box narrower than 8 px does nothing
    h.press(Key::Num0);
    h.pointer_to(at(0.5, 0.1));
    h.button(true);
    h.pointer_to(at(0.5, 0.2));
    h.pointer_to(Pos2::new(at(0.5, 0.3).x + 4.0, at(0.5, 0.3).y));
    h.button(false);
    h.step(vec![]);
    assert_eq!(h.app.zoom, Zoom::default());
}

// ───────────────────────────── settings file ─────────────────────────────

/// Garbage, out-of-range and old-format lines: every field ends up valid.
#[test]
fn settings_garbage_is_sanitised() {
    let txt = "\
on=0,0,0,0,0,0,0,0,1,1
vdiv=99,x,-1,3
tb=16
roll=999
trig_src=200
trig_slope=sideways
trig_mode=weird
trig_pos=5000
sw=width,high
sw_level=zzz
panes=9
pane_of=9,x,2
mem_mb=1
persist=1,100000
probes=0:0:0:V,garbage,1:10:0:V
math=99:0:0:1,1:9:9:2
engine=1:0:-1:60:2:0:x
no equals sign here
=
unknown=1
";
    {
        let h = harness("settings-garbage", Some(txt));
        let a = &h.app;
        assert_eq!(
            a.on,
            [true, false, false, false, false, false, false, false],
            "all off → CH1 on"
        );
        assert_eq!(a.vdiv[0], VDIVS.len() - 1);
        assert_eq!(a.vdiv[1], 9, "unparsable → default");
        assert_eq!(a.vdiv[3], 3);
        assert_eq!(a.tb_id, 17, "skipped 200 µs → next valid");
        assert_eq!(a.roll, None);
        assert_eq!(a.trig_src, 7);
        assert_eq!(a.trig_slope, Slope::Rising);
        assert_eq!(a.trig_mode, TrigMode::Auto);
        assert_eq!(a.trig_pos, 1000);
        assert_eq!(a.sw, SwTrig::Off);
        assert_eq!(a.sw_level, SwLevel::Auto);
        assert_eq!(a.panes, MAX_PANES);
        assert_eq!(a.pane_of[..3], [MAX_PANES - 1, 0, 2]);
        assert_eq!(a.mem_mb, 64);
        assert_eq!((a.persist_on, a.persist_n), (true, 100));
        assert_eq!(a.probes[0], Probe::default(), "scale 0 refused");
        assert_eq!(a.probes[2].scale, 10.0);
        assert_eq!(a.math[0], Math::default(), "op 99 refused");
        assert_eq!((a.math[1].a, a.math[1].b), (7, 7));
        assert!(!a.engine.on, "no cylinders → refused");
    } // (releases the isolation lock for the next harness)
      // other variants of single keys
    for (line, check) in [
        (
            "tb=23",
            Box::new(|a: &App| a.tb_id == 24) as Box<dyn Fn(&App) -> bool>,
        ),
        ("tb=255", Box::new(|a: &App| a.tb_id == TB_MAX)),
        ("tb=25\nroll=25", Box::new(|a: &App| a.roll == Some(25))), // (roll only kept on stream-capable timebases)
        (
            "trig_slope=falling",
            Box::new(|a: &App| a.trig_slope == Slope::Falling),
        ),
        (
            "trig_mode=normal",
            Box::new(|a: &App| a.trig_mode == TrigMode::Normal),
        ),
        ("mem_mb=99999999", Box::new(|a: &App| a.mem_mb == 16 * 1024)),
        ("mem_mb=x", Box::new(|a: &App| a.mem_mb == 512)),
        (
            "offset=9,-9,inf,1.5",
            Box::new(|a: &App| a.offset[..4] == [4.0, -4.0, 4.0, 1.5]),
        ),
        (
            "pane_h=0,1000,x",
            Box::new(|a: &App| a.pane_h[..3] == [0.01, 100.0, 1.0]),
        ),
        (
            "persist=0",
            Box::new(|a: &App| !a.persist_on && a.persist_n == 30),
        ),
        (
            "on=1,1,1",
            Box::new(|a: &App| a.on[..4] == [true, true, true, false]),
        ),
        (
            "",
            Box::new(|a: &App| a.on[..2] == [true, true] && a.tb_id == 17),
        ),
    ] {
        h_app_with(line, check);
    }
}

fn h_app_with(line: &str, check: Box<dyn Fn(&App) -> bool>) {
    let h = harness("settings-line", Some(line));
    assert!(check(&h.app), "settings line {line:?}");
}

/// NaN parses as f32, and `NaN.clamp(..)` stays NaN: a corrupt settings file makes a trace offset / the
/// trigger level / a pane height NaN (trace not drawn, trigger level 0).
#[test]
fn settings_nan_is_refused() {
    let h = harness(
        "settings-nan",
        Some("offset=NaN,1\ntrig_v=NaN\npane_h=NaN,1\n"),
    );
    assert!(h.app.offset[0].is_finite(), "offset {}", h.app.offset[0]);
    assert!(h.app.trig_v.is_finite(), "trig_v {}", h.app.trig_v);
    assert!(h.app.pane_h[0].is_finite(), "pane_h {}", h.app.pane_h[0]);
}

/// Changed settings are saved (≤ 1×/s) and load back identically in a new App; the owner's real config
/// folder is never written.
#[test]
fn settings_save_and_reload() {
    let real = real_config_dir().map(|d| d.join("settings.txt"));
    let real_before = real
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok()?.modified().ok());
    let mut h = harness("settings-save", None);
    {
        let a = &mut h.app;
        a.on = [true, false, true, false, false, false, true, false];
        a.vdiv = [1, 2, 3, 4, 5, 6, 7, 8];
        a.offset[2] = -1.75;
        a.tb_id = 21;
        a.trig_src = 2;
        a.trig_slope = Slope::Falling;
        a.trig_v = 0.375;
        a.trig_mode = TrigMode::Normal;
        a.trig_pos = 250;
        a.sw = SwTrig::Window { lo: -1.0, hi: 2.5 };
        a.sw_level = SwLevel::Custom(1.25);
        a.sw_stop = true;
        a.set_panes(3);
        a.persist_on = true;
        a.persist_n = 12;
        a.math[0] = Math {
            op: MathOp::Sub,
            a: 0,
            b: 2,
            k: 1.0,
            vdiv: 11,
            offset: 1.0,
            pane: 1,
        };
        a.probes[2] = Probe::from_preset(3);
        a.mem_mb = 1024;
        a.show_perf = true;
        a.trig_corr = false;
        a.adaptive = false;
    }
    let want = h.app.settings_hash();
    h.app.saved_at = Instant::now() - Duration::from_secs(2);
    h.step(vec![]);
    let path = h.iso.cfg().join("settings.txt");
    assert!(path.exists(), "saved into the isolated folder");
    let h2 = harness_same_dir(h);
    assert_eq!(h2.app.settings_hash(), want, "reloaded identically");
    assert_eq!(h2.app.tb_id, 21);
    assert_eq!(h2.app.sw, SwTrig::Window { lo: -1.0, hi: 2.5 });
    let real_after = real
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok()?.modified().ok());
    assert_eq!(
        real_before, real_after,
        "the real ~/.config settings were not touched"
    );
}

/// A second App in the same isolated folder (the first one is dropped, lock kept).
fn harness_same_dir(h: H) -> H {
    let H { iso, .. } = h;
    let ctx = egui::Context::default();
    let app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()), true);
    H {
        app,
        ctx,
        frame: eframe::Frame::_new_kittest(),
        t0: Instant::now(),
        mods: Modifiers::NONE,
        pointer: Pos2::ZERO,
        iso,
    }
}

// ───────────────────────────── smoke: every view / mode ─────────────────────────────

/// Every display feature and mode for a few frames each: no panic, frames keep coming.
#[test]
fn smoke_all_views_and_modes() {
    let mut h = harness("smoke", None);
    h.live();
    #[rustfmt::skip]
    let setups: Vec<(&str, Box<dyn Fn(&mut App)>)> = vec![
        ("8 channels",      Box::new(|a| a.on = [true; CHANNELS])),
        ("4 panes",         Box::new(|a| a.set_panes(4))),
        ("persistence",     Box::new(|a| a.persist_on = true)),
        ("cursors",         Box::new(|a| { a.cursors.on = true; a.cursors_init(); })),
        ("math all ops",    Box::new(|a| { a.math[0].op = MathOp::Rpm; a.math[0].k = 2.0; a.math[1].op = MathOp::LowPass; a.math[1].k = 100.0; })),
        ("fft",             Box::new(|a| a.math[1].op = MathOp::Fft)),
        ("sw width",        Box::new(|a| a.sw = sw_default(1, 1.0))),
        ("sw zone",         Box::new(|a| a.sw = SwTrig::Zone { t0: -1e-4, t1: 1e-4, v0: -1.0, v1: 1.0, enter: false })),
        ("engine",          Box::new(|a| a.engine.set_channels(2, Some(1)))),
        ("meter",           Box::new(|a| a.meter.on = true)),
        ("settings window", Box::new(|a| { a.meter.on = false; a.settings_open = true; })),
        ("shortcuts",       Box::new(|a| { a.settings_open = false; a.show_keys = true; })),
        ("perf bar",        Box::new(|a| { a.show_keys = false; a.show_perf = true; })),
        ("zoomed",          Box::new(|a| a.zoom = Zoom { x0: 0.4, x1: 0.45, y0: 0.2, y1: 0.4 })),
        ("normal trigger",  Box::new(|a| a.trig_mode = TrigMode::Normal)),
        ("fast timebase",   Box::new(|a| a.tb_id = 0)),
        ("stream 1 s/div",  Box::new(|a| a.tb_id = 27)),
        ("stream stopped",  Box::new(|a| a.set_run(Run::Stopped))),
        ("burst, history",  Box::new(|a| { a.tb_id = 17; a.set_run(Run::Running); })),
        ("history browse",  Box::new(|a| { a.set_run(Run::Stopped); a.view = a.acq.history.lock().unwrap().last_no(); })),
        ("1 pane, 1 ch",    Box::new(|a| { a.view = None; a.set_run(Run::Running); a.on = [true, false, false, false, false, false, false, false]; a.set_panes(1); })),
    ];
    for (name, f) in setups {
        f(&mut h.app);
        h.run(0.25);
        assert!(h.app.ui_fps.is_finite(), "{name}");
    }
    // still alive: new captures
    let n = h.app.last_seq;
    assert!(h.until(2.0, |a| a.last_seq > n), "captures keep coming");
}

/// References: R / Shift+R store the selected channel (slot file in the isolated config folder + library copy) and reload.
#[test]
fn references_saved_and_reloaded() {
    let mut h = harness("refs", None);
    h.live();
    h.press(Key::R);
    h.key(Key::R, SHIFT);
    h.step(vec![]);
    assert!(h.app.refs[0].is_some() && h.app.refs[1].is_some());
    let f = h.iso.cfg().join("ref1.csv");
    assert!(wait_until(2.0, || f.exists()), "{}", f.display());
    let h2 = harness_same_dir(h);
    assert!(h2.app.refs[0].is_some(), "reference reloaded at start");
    // in stream mode a reference is refused with a warning, nothing stored
    let mut h3 = harness_same_dir(h2);
    h3.app.refs = [None, None];
    h3.app.tb_id = 27;
    h3.run(1.0); // streaming now
    h3.press(Key::R);
    h3.step(vec![]);
    assert!(h3.app.refs[0].is_none());
}

/// The side panel fits its content (owner: it was cut off again and again): with 2 plots (plot column), both
/// references (wider rows), math and an analysed channel, after a couple of frames nothing sticks out.
#[test]
fn side_panel_fits_its_content() {
    let mut h = harness("panel_fit", None);
    h.live();
    h.app.panes = 2;
    for k in 0..2 {
        h.app.refs[k] = Some(crate::ui::files::RefWave {
            name: "CH1 injector reference with a long name".into(),
            unit: "V".into(),
            src: 0,
            saved_unix: 0.0,
            dt: 1e-5,
            trig_i: 0.0,
            v: vec![0.0; 10],
            under: 0,
            offset: 0.0,
            on: true,
        });
    }
    h.app.kinds[0] = crate::analyze::Kind::WheelSpeed;
    for _ in 0..4 {
        h.step(vec![]);
    }
    assert!(
        h.app.panel_over <= 0.5,
        "panel content {} px wider than the panel ({} px)",
        h.app.panel_over,
        h.app.panel_w
    );
    // this layout really is wider than the old fixed 290 px (the case from the owner's screenshot)
    assert!(h.app.panel_w > super::super::PANEL_W, "{}", h.app.panel_w);
    // back to one plot without references: the panel returns to its base width
    h.app.panes = 1;
    h.app.refs = [None, None];
    h.app.kinds[0] = crate::analyze::Kind::None;
    for _ in 0..4 {
        h.step(vec![]);
    }
    assert!(h.app.panel_over <= 0.5);
    assert!(
        h.app.panel_w <= super::super::PANEL_W + 40.0,
        "{}",
        h.app.panel_w
    );
}

// ─────────────────────────── use-case sweep (owner, 2026-11-23: "very comprehensive testing of all use cases") ───────────────────────────

/// Every guided test on the simulator, as a user would start it: the right channel is selected and shown, the panel
/// fits, analysed channels get a signal and a verdict (in the simulator every test but coil current has one), bus
/// tests decode. Prints one line per test (cargo test -- --nocapture) as the use-case log.
#[test]
fn usecase_every_guided_test() {
    use crate::analyze::Kind;
    let mut h = harness("uc_guides", None);
    h.live();
    for i in 0..crate::ui::presets::PRESETS.len() {
        let p = &crate::ui::presets::PRESETS[i];
        h.app.apply_preset(i);
        let ch = h.app.sel_ch;
        assert!(
            h.app.on[ch],
            "{}: selected channel CH{} is shown",
            p.name,
            ch + 1
        );
        assert_eq!(h.app.guide, Some(i), "{}: guide window open", p.name);
        let analysed: Vec<usize> = (0..CHANNELS)
            .filter(|&c| h.app.kinds[c] != Kind::None)
            .collect();
        let simulated = !p.name.contains("coil primary current");
        let mut line = format!("{:<45} CH{}", p.name, ch + 1);
        if let (Some(&a0), true) = (analysed.first(), simulated) {
            // a verdict, or (pulses: no pass / fail rule) readings
            let ok = h.until(8.0, |a| {
                a.ana[a0].present
                    && (a.ana[a0].verdict.is_some() || !a.ana[a0].readouts().is_empty())
            });
            assert!(
                ok,
                "{}: CH{} analysed, signal + result within 8 s (waiting: {:?})",
                p.name,
                a0 + 1,
                h.app.ana[a0].waiting
            );
            match h.app.ana[a0].verdict.clone() {
                Some(v) => line += &format!(" | {} {}", if v.0 { "OK" } else { "CHECK" }, v.1),
                None => line += &format!(" | readings: {:?}", h.app.ana[a0].readouts()),
            }
            // judged with this test's settings, not those of the previous one (5 V pulses, not clipped at 2.56 V)
            if p.name.starts_with("Ignition trigger") {
                assert!(
                    h.app.ana[a0]
                        .readouts()
                        .iter()
                        .any(|r| r.0 == "levels" && r.1.contains("5.00 V")),
                    "{:?}",
                    h.app.ana[a0].readouts()
                );
            }
            // coil −: the ×20 range keeps the 350 V firing spike unclipped and the analysis is on (owner, 2026-11-23:
            // this guided test used to set up nothing)
            if p.name.starts_with("Ignition primary") {
                assert_eq!(h.app.kinds[ch], Kind::IgnitionPrimary, "{}", p.name);
                assert!(
                    h.app.ana[a0]
                        .readouts()
                        .iter()
                        .any(|r| r.0 == "firing peak" && r.1 == "350 V"),
                    "{:?}",
                    h.app.ana[a0].readouts()
                );
            }
        }
        if let Some((short, _)) = h.app.decoder_summary() {
            line += &format!(" | decoder {short}");
            if short.starts_with("CAN") {
                let n0 = h.app.can_log_len();
                assert!(
                    h.until(5.0, |a| a.can_log_len() > n0),
                    "{}: CAN frames are logged",
                    p.name
                );
                line += &format!(", {} frames logged", h.app.can_log_len());
            }
        }
        h.run(0.3);
        assert!(
            h.app.panel_over <= 0.5,
            "{}: side panel cut off by {} px",
            p.name,
            h.app.panel_over
        );
        println!("usecase guide: {line}");
    }
    // the simulator's demo values: healthy injector / ignition trigger / crank, the weak cylinder is found
    let n = h.app.last_seq;
    assert!(h.until(2.0, |a| a.last_seq > n), "captures keep coming");
}

/// Every signal type on every channel (also the 'wrong' ones: lambda on a CAN bus, battery on a crank sensor …):
/// no panic, and the state is coherent — waiting ⇔ no verdict, a verdict only with a signal.
#[test]
fn usecase_every_signal_type_on_every_channel() {
    use crate::analyze::KINDS;
    let mut h = harness("uc_kinds", None);
    h.live();
    h.app.on = [true; CHANNELS];
    for (kind, name) in KINDS.iter().skip(1) {
        h.app.kinds = [*kind; CHANNELS];
        h.run(0.4);
        for ch in 0..CHANNELS {
            let a = &h.app.ana[ch];
            assert!(
                a.present || a.verdict.is_none(),
                "{name} on CH{}: a verdict while waiting for a signal",
                ch + 1
            );
        }
    }
    // a type that doesn't fit the signal at all waits calmly: a battery test on the ±5 V sine (mean ≈ 0 V)
    h.app.kinds = [crate::analyze::Kind::None; CHANNELS];
    h.app.kinds[0] = crate::analyze::Kind::Battery;
    h.run(0.6);
    assert!(
        !h.app.ana[0].present
            && h.app.ana[0]
                .waiting
                .as_deref()
                .is_some_and(|w| w.contains("not a 12 V battery")),
        "{:?}",
        h.app.ana[0].waiting
    );
}

/// A test run (injector test) from start to report: injections counted, none failed on the healthy simulator, no
/// 'no signal' captures, the report window opens; and a new guided test ends a running one (report, no mix).
#[test]
fn usecase_test_run_report() {
    let mut h = harness("uc_testrun", None);
    h.live();
    let inj = crate::ui::presets::PRESETS
        .iter()
        .position(|p| p.name.starts_with("Injector"))
        .unwrap();
    h.app.apply_preset(inj);
    assert!(h.until(5.0, |a| a.ana[a.sel_ch].verdict.is_some()));
    h.app.test.duration_s = 2.0;
    h.app.test_start();
    assert!(h.app.test_running());
    assert!(
        h.until(6.0, |a| a.test_report().is_some()),
        "report after 2 s"
    );
    h.step(vec![]);
    assert!(h.app.test_show, "report window opens by itself");
    let r = h.app.test_report().unwrap().to_string();
    println!("usecase testrun report:\n{r}");
    assert!(
        r.contains("injections: ") && r.contains("failed injections (no flyback spike): 0**"),
        "{r}"
    );
    assert!(!r.contains("without a signal"), "{r}");
    // a running test is ended (with its report) by starting another guided test
    h.app.test.duration_s = 60.0;
    h.app.test_start();
    h.run(0.3);
    h.app.apply_preset(0);
    assert!(
        !h.app.test_running(),
        "a new guided test ends the running test run"
    );
}

/// References as a user works with them: save the selected channel (R), the library file is a readable CSV, load it
/// into the other slot from the library, draw it on another channel with an offset, clear; persisted slots.
#[test]
fn usecase_references_library() {
    let mut h = harness("uc_refs", None);
    h.live();
    h.app.sel_ch = 0;
    h.press(Key::R);
    h.step(vec![]);
    let r1 = h.app.refs[0].clone().expect("R1 saved");
    assert_eq!((r1.src, r1.under, r1.unit.as_str()), (0, 0, "V"));
    h.app.files.ref_lib.0 = None;
    let lib = h.app.ref_library();
    assert_eq!(lib.len(), 1, "one library file");
    let txt = std::fs::read_to_string(&lib[0]).unwrap();
    assert!(txt.starts_with("# LR Scope 1008C reference waveform") && txt.contains("t_s,value_V"));
    h.app.ref_load(1, &lib[0]);
    let r2 = h.app.refs[1].clone().expect("R2 loaded from the library");
    assert_eq!(r2.v, r1.v);
    // draw R2 on CH2 (shown) shifted; with CH2 off it is not drawn but stays in its slot
    h.app.on[1] = true;
    if let Some(r) = h.app.refs[1].as_mut() {
        (r.under, r.offset) = (1, 1.5);
    }
    h.app.ref_dirty[1] = Some(Instant::now()); // as the R2 row does on an edit
    h.run(0.8); // saved 0.5 s after the edit
    let slot = std::fs::read_to_string(h.iso.cfg().join("ref2.csv")).unwrap();
    assert!(
        slot.contains("# under=CH2") && slot.contains("# offset=1.5"),
        "edit persisted"
    );
    h.app.refs[0] = None;
    h.app.save_ref(0);
    assert!(
        !h.iso.cfg().join("ref1.csv").exists(),
        "cleared slot file removed"
    );
    assert!(lib[0].exists(), "the library file stays");
    // a file that is not a reference is refused with a message, the slot keeps its reference
    let junk = h.iso.cfg().join("junk.csv");
    std::fs::write(&junk, "a,b\n1,2\n").unwrap();
    h.app.ref_load(1, &junk);
    assert!(h.app.refs[1].is_some());
}

/// Export of what is shown in every format, and of a whole stream recording: files appear with the right extension
/// and content start.
#[test]
fn usecase_exports_all_formats() {
    let mut h = harness("uc_export", None);
    h.live();
    let dir = h.app.files.dir();
    let files = |d: &std::path::Path| -> Vec<std::path::PathBuf> {
        std::fs::read_dir(d)
            .map(|r| r.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default()
    };
    for (fmt, ext) in [
        (ExportFmt::Csv, "csv"),
        (ExportFmt::Wav, "wav"),
        (ExportFmt::Mf4, "mf4"),
        (ExportFmt::Sr, "sr"),
    ] {
        let n0 = files(&dir)
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == ext))
            .count();
        h.app.files.fmt = fmt;
        h.app.files.src = ExportSrc::Shown;
        h.app.files.export_req = true;
        let ok = h.until(5.0, |a| {
            a.files.job.is_none()
                && files(&dir)
                    .iter()
                    .filter(|p| p.extension().is_some_and(|e| e == ext))
                    .count()
                    > n0
        });
        assert!(ok, "export {ext} written to {}", dir.display());
    }
    let csv = files(&dir)
        .into_iter()
        .find(|p| p.extension().is_some_and(|e| e == "csv"))
        .unwrap();
    let head = std::fs::read_to_string(&csv).unwrap();
    assert!(head.lines().count() > 100, "CSV has the capture's rows");
    let wav = std::fs::read(
        files(&dir)
            .into_iter()
            .find(|p| p.extension().is_some_and(|e| e == "wav"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(&wav[..4], b"RIFF");
    // whole stream: stream for a moment, then export it (also after switching back to captures)
    h.app.tb_id = 27;
    h.run(1.5);
    h.app.tb_id = 17;
    h.run(0.3);
    let n0 = files(&dir).len();
    h.app.files.fmt = ExportFmt::Csv;
    h.app.files.src = ExportSrc::Stream;
    h.app.files.export_req = true;
    assert!(
        h.until(5.0, |a| a.files.job.is_none() && files(&dir).len() > n0),
        "whole-stream export after switching back"
    );
}

/// Screenshot information for typical states: lean file-name part, full metadata, strip text — and µ in names.
#[test]
fn usecase_screenshot_info() {
    let mut h = harness("uc_shot", None);
    h.live();
    let inj = crate::ui::presets::PRESETS
        .iter()
        .position(|p| p.name.starts_with("Injector"))
        .unwrap();
    h.app.apply_preset(inj);
    assert!(h.until(5.0, |a| a.ana[a.sel_ch].verdict.is_some()));
    let (desc, meta, strip) = h.app.shot_info(std::time::SystemTime::now());
    assert!(desc.starts_with("injector-CH6"), "{desc}");
    let d = &meta.iter().find(|m| m.0 == "Description").unwrap().1;
    assert!(
        d.contains("Guided test: Injector")
            && d.contains("CH6: ")
            && d.contains("analysed as Injector"),
        "{d}"
    );
    assert!(
        strip.contains("CH6") && strip.contains("trig CH6 falling"),
        "{strip}"
    );
    // nothing analysed, 500 µs/div: the timebase names the shot, ASCII only
    h.app.kinds = [crate::analyze::Kind::None; CHANNELS];
    h.app.decoder.off();
    h.app.tb_id = (0..=TB_MAX)
        .find(|&id| burst_ns_per_div(id) == 500_000)
        .unwrap();
    h.run(0.3);
    let (desc, _, _) = h.app.shot_info(std::time::SystemTime::now());
    assert_eq!(desc, "500us-div");
}

/// Experimental fast CAN: only the bus channel is captured, frames still decode; off again → all channels.
#[test]
fn usecase_fast_can() {
    let mut h = harness("uc_fastcan", None);
    h.live();
    let can = crate::ui::presets::PRESETS
        .iter()
        .position(|p| p.name.starts_with("CAN"))
        .unwrap();
    h.app.apply_preset(can);
    h.app.decoder.fast = true;
    let ch = h
        .app
        .decoder
        .fast_channel()
        .expect("fast mode on a CAN decoder");
    assert!(
        h.until(3.0, |a| a.sent.as_ref().is_some_and(|c| c
            .active
            .iter()
            .filter(|&&x| x)
            .count()
            == 1
            && c.active[ch])),
        "only CH{} captured",
        ch + 1
    );
    let n0 = h.app.can_log_len();
    assert!(
        h.until(5.0, |a| a.can_log_len() > n0),
        "frames decode in fast mode"
    );
    h.app.decoder.fast = false;
    assert!(
        h.until(3.0, |a| a
            .sent
            .as_ref()
            .is_some_and(|c| c.active.iter().all(|&x| x))),
        "back to all 8 channels"
    );
}

fn preset_index(start: &str) -> usize {
    crate::ui::presets::PRESETS
        .iter()
        .position(|p| p.name.starts_with(start))
        .expect(start)
}

/// Record a stream to disk while a guided test runs (lambda), then open the file again like a user (Browse… / Open)
/// and get the same analysis on the opened recording.
#[test]
fn usecase_record_then_reopen_with_analysis() {
    let mut h = harness("uc_record", None);
    h.live();
    h.app.files.auto = true;
    let lam = preset_index("Lambda");
    h.app.apply_preset(lam);
    let ch = h.app.sel_ch;
    assert!(
        h.until(8.0, |a| a.ana[ch].verdict.is_some()),
        "lambda live: {:?}",
        h.app.ana[ch].waiting
    );
    h.run(3.0);
    // the next test (captures) ends the stream: the recording is closed on disk
    h.app.apply_preset(preset_index("Injector"));
    h.run(1.0);
    let dir = h.app.files.dir();
    let h8r = || -> Vec<std::path::PathBuf> {
        let mut v: Vec<_> = std::fs::read_dir(&dir)
            .map(|r| {
                r.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().is_some_and(|e| e == crate::rec::EXT))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    };
    assert!(
        wait_until(4.0, || !h8r().is_empty()),
        "a recording file in {}",
        dir.display()
    );
    let f = h8r().pop().unwrap();
    // open it with the lambda analysis set up again
    h.app.apply_preset(lam);
    h.run(0.3);
    h.app.open_start(f.clone());
    assert!(
        h.until(6.0, |a| a.files.job.is_none()
            && a.run == Run::Stopped
            && a.roll.is_some()),
        "opened {}",
        f.display()
    );
    assert!(
        h.until(3.0, |a| a.ana[ch].present && a.ana[ch].verdict.is_some()),
        "analysis on the opened recording: {:?}",
        h.app.ana[ch].waiting
    );
    println!(
        "usecase reopen: {} → {:?}",
        f.display(),
        h.app.ana[ch].verdict
    );
}

/// CAN logger as a user uses it: the CAN guided test logs frames, Export writes a Vector ASC file with the sim's
/// frame (ID 2C0, 4 bytes 01 02 03 04), Clear empties the log.
#[test]
fn usecase_can_log_export_asc() {
    let mut h = harness("uc_asc", None);
    h.live();
    h.app.apply_preset(preset_index("CAN"));
    assert!(h.until(6.0, |a| a.can_log_len() >= 3), "frames logged");
    h.app.export_can_log();
    let dir = h.app.files.dir();
    let asc = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|e| e == "asc"))
        .expect("an .asc file");
    let t = std::fs::read_to_string(&asc).unwrap();
    assert!(
        t.starts_with("date ")
            && t.contains("Begin Triggerblock")
            && t.contains("End TriggerBlock"),
        "{t}"
    );
    let frame = t
        .lines()
        .find(|l| l.contains(" 2C0 "))
        .unwrap_or_else(|| panic!("frame 2C0 in\n{t}"));
    assert!(
        frame.contains("Rx") && frame.contains("d 4 01 02 03 04"),
        "{frame}"
    );
    h.app.decoder.clear_log();
    assert_eq!(h.app.can_log_len(), 0);
}

/// Analysis while looking back: stopped on a capture, browsing the history (older captures), meter mode, two plots
/// with a reference — the box follows what is shown, nothing panics, the panel fits.
#[test]
fn usecase_browse_history_meter_and_layouts() {
    let mut h = harness("uc_browse", None);
    h.live();
    h.app.apply_preset(preset_index("Injector"));
    let ch = h.app.sel_ch;
    assert!(h.until(6.0, |a| a.ana[ch].verdict.is_some()));
    h.run(1.0);
    h.app.set_run(Run::Stopped);
    h.run(0.3);
    let last = h
        .app
        .acq
        .history
        .lock()
        .unwrap()
        .last_no()
        .expect("history");
    for back in [0u64, 1, 5, 10] {
        h.app.view = Some(last.saturating_sub(back));
        h.run(0.15);
        assert!(
            h.app.ana[ch].present && h.app.ana[ch].verdict.as_ref().is_some_and(|v| v.0),
            "capture -{back}: {:?}",
            h.app.ana[ch].verdict
        );
    }
    h.app.view = None;
    h.app.set_run(Run::Running);
    // meter mode on the guided test, then 2 plots + R1 from this channel drawn on itself
    h.app.meter.on = true;
    h.run(0.5);
    h.app.meter.on = false;
    h.press(Key::R);
    h.app.set_panes(2);
    h.run(0.6);
    assert!(h.app.refs[0].is_some() && h.app.panel_over <= 0.5);
    let n = h.app.last_seq;
    assert!(h.until(2.0, |a| a.last_seq > n), "captures keep coming");
}

// ─────────────────────────── on the real scope (opt-in: cargo test --release -- --ignored hw_ --test-threads=1) ───────────────────────────

/// The real App on the real Hantek 1008C (no window): CH1 on the internal 1 kHz generator, CH2 open (nothing
/// connected). Owner's complaint (2026-11-22): with nothing connected the guided diagnostic flickered.
/// 20 s live: the open input must stay 'waiting for a signal' without a single flip or verdict; the 1 kHz square must
/// be found steadily, read 1 kHz, and not flip either.
#[test]
#[ignore]
fn hw_open_input_calm_and_square_steady() {
    use crate::analyze::Kind;
    let settings = "on=1,1,0,0,0,0,0,0\nvdiv=7,7,9,9,9,9,9,9\noffset=-1,0,0,0,0,0,0,0\ntb=17\ntrig_src=0\ntrig_slope=rising\ntrig_v=1\ntrig_mode=auto\ntrig_pos=200\n";
    let iso = isolated("hw_calm");
    iso.write("settings.txt", settings);
    let ctx = egui::Context::default();
    let app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()), false);
    let mut h = H {
        app,
        ctx,
        frame: eframe::Frame::_new_kittest(),
        t0: Instant::now(),
        mods: Modifiers::NONE,
        pointer: Pos2::new(5.0, 5.0),
        iso,
    };
    assert!(
        h.until(10.0, |a| a.last_seq > 5),
        "no captures from the scope (connected? app closed?)"
    );
    h.app.kinds[0] = Kind::PulseHigh;
    h.app.kinds[1] = Kind::WheelSpeed;
    h.run(1.0);
    let (mut flips, mut prev) = ([0u32; 2], [h.app.ana[0].present, h.app.ana[1].present]);
    let mut verdict_open = 0u32;
    let mut freqs: Vec<f64> = Vec::new();
    let (t, n0) = (Instant::now(), h.app.last_seq);
    // HW_SECS=600 for an endurance run (default 20 s)
    let secs: f32 = std::env::var("HW_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20.0);
    while t.elapsed().as_secs_f32() < secs {
        h.step(vec![]);
        for ch in 0..2 {
            if h.app.ana[ch].present != prev[ch] {
                flips[ch] += 1;
                prev[ch] = h.app.ana[ch].present;
            }
        }
        if h.app.ana[1].verdict.is_some() {
            verdict_open += 1;
        }
        if let Some(f) = h.app.ana[0]
            .readouts()
            .iter()
            .find(|r| r.0 == "frequency")
            .and_then(|r| r.1.split_whitespace().next()?.parse::<f64>().ok())
        {
            freqs.push(f);
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    let caps = h.app.last_seq - n0;
    let (fmin, fmax) = freqs
        .iter()
        .fold((f64::MAX, f64::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    println!(
        "hw: {caps} captures in {secs} s; CH2 open: present={} flips={} verdict-frames={verdict_open} waiting={:?}; CH1 1 kHz: present={} flips={} freq {fmin:.1}..{fmax:.1} Hz readouts {:?}",
        h.app.ana[1].present, flips[1], h.app.ana[1].waiting, h.app.ana[0].present, flips[0], h.app.ana[0].readouts()
    );
    assert!(caps as f32 > 10.0 * secs, "captures keep coming ({caps})");
    assert!(
        !h.app.ana[1].present && flips[1] == 0 && verdict_open == 0,
        "open input must stay calm"
    );
    assert!(
        h.app.ana[0].present && flips[0] == 0,
        "the 1 kHz square must be found steadily"
    );
    assert!(
        !freqs.is_empty() && fmin > 990.0 && fmax < 1010.0,
        "1 kHz read as {fmin}..{fmax}"
    );
}

// ───────────────────────────── speed review: frame cost per feature ─────────────────────────────

/// Speed review (2026-11-23): CPU cost of one UI frame on the real App (simulator, no window) for a cumulative
/// worst-case build-up — base → 8 ch / 4 plots → persistence → math + FFT → analysis → decoder → references →
/// cursors → match filter → stream modes. Per scenario: frame (App::ui) and tessellation ms (mean / p95 / max),
/// allocations per frame (all threads, so the acq thread's share is included), and the per-section breakdown from
/// `ui::prof`. Frames are paced at ~60 fps like the real app.
/// Run: cargo test --release frame_cost -- --ignored --nocapture --test-threads=1
#[test]
#[ignore]
fn frame_cost() {
    use crate::regress::util::alloc_count;
    let secs: f32 = std::env::var("FRAME_COST_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3.0);
    let only = std::env::var("FRAME_COST_ONLY").ok();
    let mut h = harness("frame_cost", None);
    h.live();
    let refwave = |k: usize, under: usize| crate::ui::files::RefWave {
        name: format!("R{k}"),
        unit: "V".into(),
        src: under,
        saved_unix: 0.0,
        dt: 1e-6,
        trig_i: 2000.0,
        v: (0..4000).map(|i| (i as f32 * 0.01).sin() * 3.0).collect(),
        under,
        offset: 0.0,
        on: true,
    };
    #[rustfmt::skip]
    let setups: Vec<(&str, Box<dyn Fn(&mut App)>)> = vec![
        ("base (2 ch, 1 plot)",      Box::new(|_| {})),
        ("8 ch, 4 plots",            Box::new(|a| { a.on = [true; CHANNELS]; a.set_panes(4); })),
        ("+ persistence 30",         Box::new(|a| { a.persist_on = true; a.persist_n = 30; })),
        ("+ persistence 100",        Box::new(|a| a.persist_n = 100)),
        ("+ math A-B, FFT",          Box::new(|a| { a.math[0].op = MathOp::Sub; a.math[0].a = 0; a.math[0].b = 1; a.math[1].op = MathOp::Fft; a.math[1].a = 0; })),
        ("+ analysis 4 ch",          Box::new(|a| { use crate::analyze::Kind; a.kinds[4] = Kind::IgnitionPrimary; a.kinds[5] = Kind::Injector; a.kinds[0] = Kind::PulseHigh; a.kinds[3] = Kind::Battery; })),
        ("+ CAN decoder CH7",        Box::new(|a| a.decoder.set(6, "canh:100000"))),
        ("+ references R1 R2",       Box::new(move |a| { a.refs = [Some(refwave(1, 0)), Some(refwave(2, 1))]; a.refs_on = true; })),
        ("+ cursors",                Box::new(|a| { a.cursors.on = true; a.cursors_init(); })),
        ("+ match filter width",     Box::new(|a| a.sw = sw_default(1, 1.0))),
        ("all, stopped (idle)",      Box::new(|a| a.set_run(Run::Stopped))),
        ("all, history browse",      Box::new(|a| { let n = a.acq.history.lock().unwrap().last_no(); a.view = n.map(|n| n.saturating_sub(5)); })),
        ("stream 20 s/div 8 ch",     Box::new(|a| { a.view = None; a.sw = SwTrig::Off; a.set_run(Run::Running); a.tb_id = 32; })),
        ("stream 20 s/div stopped",  Box::new(|a| a.set_run(Run::Stopped))),
        ("stream full screen 8 ch, stopped", Box::new(|a| fill_stream(a))),
        ("  measurements hidden (F3)", Box::new(|a| a.meas_on = false)),
        ("  measurements shown again", Box::new(|a| a.meas_on = true)),
        ("  - FFT window",           Box::new(|a| a.math[1].op = MathOp::Off)),
        ("  - analysis highlight",   Box::new(|a| a.highlight_on = false)),
        ("  - analysis",             Box::new(|a| a.kinds = [crate::analyze::Kind::None; CHANNELS])),
        ("  - math, decoder, cursors", Box::new(|a| { a.math[0].op = MathOp::Off; a.decoder.set(6, "canh:100000"); a.decoder = Default::default(); a.cursors.on = false; })),
        ("stream 20 s/div 1 ch",     Box::new(|a| { a.set_run(Run::Running); a.on = [true, false, false, false, false, false, false, false]; a.set_panes(1); a.kinds = [crate::analyze::Kind::None; CHANNELS]; a.kinds[0] = crate::analyze::Kind::PulseHigh; a.highlight_on = true; a.math[1].op = MathOp::Fft; a.math[1].a = 0; })),
        ("stream full screen 1 ch, stopped", Box::new(|a| fill_stream(a))),
    ];
    crate::ui::prof::enable(true);
    println!(
        "frame_cost: {secs} s per scenario, window {}×{}",
        SCREEN.x, SCREEN.y
    );
    for (name, f) in setups {
        f(&mut h.app);
        // warm-up: settings reach the sim, caches fill, persistence history exists
        let warm = if name.starts_with("stream") { 4.0 } else { 1.5 };
        h.run(warm);
        if only
            .as_ref()
            .is_some_and(|o| !name.contains(o.as_str()) && !name.starts_with("  -"))
        {
            continue;
        }
        let _ = crate::ui::prof::take();
        let (mut fr, mut tess, mut new_fr) = (Vec::new(), Vec::new(), 0u32);
        let (mut verts, mut shapes) = (0usize, 0usize);
        let a0 = alloc_count::now();
        let u0 = alloc_count::this_thread();
        let t = Instant::now();
        while t.elapsed().as_secs_f32() < secs {
            let seq = h.app.last_seq;
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SCREEN)),
                time: Some(h.t0.elapsed().as_secs_f64()),
                focused: true,
                ..Default::default()
            };
            let t1 = Instant::now();
            let (app, frame) = (&mut h.app, &mut h.frame);
            let out = h.ctx.run_ui(raw, |ui| eframe::App::ui(app, ui, frame));
            let t2 = Instant::now();
            shapes += out.shapes.len();
            let prims = h.ctx.tessellate(out.shapes, out.pixels_per_point);
            let t3 = Instant::now();
            verts += prims
                .iter()
                .map(|p| match &p.primitive {
                    egui::epaint::Primitive::Mesh(m) => m.vertices.len(),
                    _ => 0,
                })
                .sum::<usize>();
            fr.push((t2 - t1).as_secs_f64() * 1e3);
            tess.push((t3 - t2).as_secs_f64() * 1e3);
            if h.app.last_seq != seq {
                new_fr += 1;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        let a1 = alloc_count::now();
        let u1 = alloc_count::this_thread();
        let n = fr.len();
        let stat = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            (mean, v[v.len() * 95 / 100], v[v.len() - 1])
        };
        let (fm, fp, fx) = stat(&mut fr);
        let (tm, tp, tx) = stat(&mut tess);
        println!(
            "\n== {name}: {n} frames ({new_fr} with a new capture) | frame {fm:.2} / p95 {fp:.2} / max {fx:.2} ms | tessellate {tm:.2} / p95 {tp:.2} / max {tx:.2} ms | UI thread {:.0} allocs, {:.0} KB per frame (all threads {:.0} / {:.0} KB) | {} shapes, {} vertices per frame",
            (u1.0 - u0.0) as f64 / n as f64,
            (u1.1 - u0.1) as f64 / n as f64 / 1024.0,
            (a1.0 - a0.0) as f64 / n as f64,
            (a1.1 - a0.1) as f64 / n as f64 / 1024.0,
            shapes / n,
            verts / n
        );
        let mut secs_v = crate::ui::prof::take();
        secs_v.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (s, ms, calls) in secs_v.iter().filter(|s| s.1 / n as f64 >= 0.005) {
            println!("   {s:<32} {:7.3} ms/frame  ({calls} calls)", ms / n as f64);
        }
    }
    crate::ui::prof::enable(false);
}

/// Stopped stream: top the stream recording up to one full screen (the worst case after 200 s at 20 s/div) with a
/// 7 Hz square + noise, so the per-frame stream paths run on full-size data without waiting minutes.
fn fill_stream(a: &mut App) {
    a.set_run(Run::Stopped);
    std::thread::sleep(Duration::from_millis(100));
    let need = a.stream_rows_per_screen();
    let mut h = a.acq.history.lock().unwrap();
    let have = h.stream.rows();
    let rows = need.saturating_sub(have) + 10;
    let mut f = crate::hantek::Frame::default();
    for ch in 0..CHANNELS {
        f.active[ch] = h.stream.active[ch];
        f.data[ch] = (0..rows)
            .map(|i| 2048 + if (i / 31) % 2 == 0 { 600 } else { 0 } + (i * 7919 % 13) as u16)
            .collect();
    }
    h.append_stream(&f, rows);
    println!(
        "(stream filled to {} rows, {} per screen)",
        h.stream.rows(),
        need
    );
}

/// Speed review (2026-11-23): how long the UI holds the shared history mutex (= how long the acq thread's next
/// `push` / `append_stream` can be blocked) with a long history (N 8-channel captures, default 20 000 ≈ 9 min at
/// 38/s; 64 000 fit the default 512 MB). A prober thread locks the history every ~0.5 ms like the acq thread does
/// and records its wait. Events: steady running, channel toggle (running statistics recomputed: update_acc),
/// statistics range set (current_stats), match filter changed (update_hits rescan), persistence on.
/// Run: cargo test --release history_lock_hold -- --ignored --nocapture --test-threads=1
#[test]
#[ignore]
fn history_lock_hold() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let n: usize = std::env::var("LOCK_CAPTURES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    let mut h = harness("lock_hold", None);
    h.app.on = [true; CHANNELS];
    h.app.mem_mb = 2048;
    h.run(1.5);
    // stop, then grow the history with copies of the last capture (no acq pushes while measuring the UI side)
    h.app.set_run(Run::Stopped);
    h.run(0.3);
    {
        let mut hist = h.app.acq.history.lock().unwrap();
        hist.set_budget(4 << 30);
        let last = hist.get(hist.last_no().unwrap()).unwrap();
        let t = Instant::now();
        for _ in 0..n {
            hist.push(crate::acq::HEntry {
                snap: last.snap.clone(),
                at: last.at,
            });
        }
        println!(
            "history: {} captures, {} MB, filled in {:.0} ms",
            hist.len(),
            hist.used_bytes() >> 20,
            t.elapsed().as_secs_f64() * 1e3
        );
    }
    let hist = h.app.acq.history.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let waits = Arc::new(std::sync::Mutex::new(Vec::<f64>::new()));
    let prober = {
        let (stop, waits) = (stop.clone(), waits.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let t = Instant::now();
                drop(hist.lock().unwrap());
                waits.lock().unwrap().push(t.elapsed().as_secs_f64() * 1e3);
                std::thread::sleep(Duration::from_micros(500));
            }
        })
    };
    let report = |h: &mut H, what: &str, f: &dyn Fn(&mut App), secs: f32| {
        // let the previous catch-up finish first
        h.run(0.2);
        waits.lock().unwrap().clear();
        f(&mut h.app);
        let mut frames = Vec::new();
        let t = Instant::now();
        while t.elapsed().as_secs_f32() < secs {
            let t1 = Instant::now();
            h.step(vec![]);
            frames.push(t1.elapsed().as_secs_f64() * 1e3);
            std::thread::sleep(Duration::from_millis(16));
        }
        let mut w = std::mem::take(&mut *waits.lock().unwrap());
        w.sort_by(f64::total_cmp);
        frames.sort_by(f64::total_cmp);
        let over = |v: &[f64], ms: f64| v.iter().filter(|&&x| x > ms).count();
        println!(
            "{what:<44} lock wait max {:7.2} ms, p99 {:6.2}, >1 ms {:4}, >10 ms {:3} | UI frame max {:7.2} ms, frames >16 ms {:3} of {}",
            w.last().copied().unwrap_or(0.0),
            w.get(w.len() * 99 / 100).copied().unwrap_or(0.0),
            over(&w, 1.0),
            over(&w, 10.0),
            frames.last().copied().unwrap_or(0.0),
            over(&frames, 16.0),
            frames.len()
        );
    };
    report(&mut h, "stopped, idle", &|_| {}, 1.0);
    report(
        &mut h,
        "channel CH8 off (stats recomputed)",
        &|a| a.on[7] = false,
        3.0,
    );
    report(
        &mut h,
        "probe scale CH1 ×10 (stats recomputed)",
        &|a| a.probes[0].scale = 10.0,
        3.0,
    );
    report(
        &mut h,
        "statistics range: whole history",
        &|a| {
            a.stats_from = Some(0);
            a.stats_to = Some(u64::MAX);
        },
        2.0,
    );
    report(
        &mut h,
        "match filter width on (rescan)",
        &|a| a.sw = sw_default(1, 1.0),
        3.0,
    );
    report(
        &mut h,
        "persistence 100 on",
        &|a| {
            a.persist_on = true;
            a.persist_n = 100;
        },
        1.0,
    );
    report(
        &mut h,
        "running again (acq pushes too)",
        &|a| a.set_run(Run::Running),
        2.0,
    );
    stop.store(true, Ordering::Relaxed);
    prober.join().unwrap();
}

/// Speed review (2026-11-23): persistence while the view changes every frame (pan / zoom drag on the touchpad):
/// the cached image is rebuilt from all N captures per pane each frame. 8 channels, 4 plots, N = 30 / 100.
/// Also: App start (App::new + first frame) on the simulator.
/// Run: cargo test --release persist_pan_cost -- --ignored --nocapture --test-threads=1
#[test]
#[ignore]
fn persist_pan_cost() {
    let t = Instant::now();
    let mut h = harness("persist_pan", None);
    println!(
        "App::new + first frame: {:.1} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
    h.app.on = [true; CHANNELS];
    h.app.set_panes(4);
    h.run(4.0); // at least 100 captures in the history
    for n in [30usize, 100] {
        for (what, pan) in [("static view", false), ("panning every frame", true)] {
            h.app.persist_on = true;
            h.app.persist_n = n;
            h.app.zoom = Zoom {
                x0: 0.2,
                x1: 0.7,
                y0: 0.0,
                y1: 1.0,
            };
            h.run(0.5);
            let _ = crate::ui::prof::take();
            crate::ui::prof::enable(true);
            let mut fr = Vec::new();
            for i in 0..90 {
                if pan {
                    let d = 0.002 * (i % 40) as f32;
                    h.app.zoom = Zoom {
                        x0: 0.2 + d,
                        x1: 0.7 + d,
                        y0: 0.0,
                        y1: 1.0,
                    };
                }
                let t1 = Instant::now();
                h.step(vec![]);
                fr.push(t1.elapsed().as_secs_f64() * 1e3);
                std::thread::sleep(Duration::from_millis(16));
            }
            crate::ui::prof::enable(false);
            let p = crate::ui::prof::take();
            let pi = p
                .iter()
                .find(|s| s.0 == "plot:persist_image")
                .map_or(0.0, |s| s.1)
                / fr.len() as f64;
            fr.sort_by(f64::total_cmp);
            println!(
                "persistence {n:3}, {what:20}: frame mean {:6.2} ms, p95 {:6.2}, max {:6.2} | persist_image {pi:6.2} ms/frame",
                fr.iter().sum::<f64>() / fr.len() as f64,
                fr[fr.len() * 95 / 100],
                fr[fr.len() - 1]
            );
        }
    }
}

/// Statistics over a custom range are now built up incrementally outside the history lock (speed review
/// 2026-11-23). They must equal a full recompute over the same captures — also while new captures arrive.
#[test]
fn range_statistics_incremental_equal_full_recompute() {
    let mut h = harness("stats_incr", None);
    h.live();
    assert!(
        h.until(5.0, |a| a.acq.history.lock().unwrap().len() > 20),
        "some captures"
    );
    let first = h.app.acq.history.lock().unwrap().first_no;
    h.app.stats_from = Some(first + 5);
    h.app.stats_to = None; // up to the latest: grows while running
    h.run(1.0);
    h.app.set_run(Run::Stopped);
    // let it finish adding up
    let mut done = false;
    for _ in 0..200 {
        h.step(vec![]);
        if !h.app.clone_stats_label().1.ends_with("(adding up…)") {
            done = true;
            break;
        }
    }
    assert!(done, "statistics finished adding up");
    let (inc, label) = h.app.clone_stats_label();
    let (from, to) = (
        first + 5,
        h.app.acq.history.lock().unwrap().last_no().unwrap(),
    );
    assert!(
        label.contains(&format!("#{}–#{}", from + 1, to + 1)),
        "{label}"
    );
    let mut full: Stats = Default::default();
    for (_, e) in h.app.history_items(from, to) {
        add_capture(&mut full, &e, &h.app.on, &h.app.cal, &h.app.probes);
    }
    for ch in 0..CHANNELS {
        for q in 0..NQ {
            let (a, b) = (&inc[ch][q], &full[ch][q]);
            assert_eq!(a.n, b.n, "CH{} q{q}: count", ch + 1);
            if a.n > 0 {
                assert!(
                    (a.sum - b.sum).abs() <= 1e-9 * b.sum.abs().max(1.0),
                    "CH{} q{q}: sum {} vs {}",
                    ch + 1,
                    a.sum,
                    b.sum
                );
                assert_eq!((a.min, a.max), (b.min, b.max), "CH{} q{q}", ch + 1);
            }
        }
    }
    // changing the range starts over and again matches
    h.app.stats_to = Some(from + 3);
    h.step(vec![]);
    let (inc2, _) = h.app.clone_stats_label();
    let mut full2: Stats = Default::default();
    for (_, e) in h.app.history_items(from, from + 3) {
        add_capture(&mut full2, &e, &h.app.on, &h.app.cal, &h.app.probes);
    }
    assert_eq!(inc2[0][0].n, full2[0][0].n);
}

/// Collapsed side panel (owner: more room for the waveform on small screens): F4 collapses and expands, the
/// app keeps capturing and drawing in both states, the setting is saved.
#[test]
fn side_panel_collapses_with_f4() {
    let mut h = harness("panel_min", None);
    h.live();
    assert!(!h.app.panel_min);
    h.key(Key::F4, Modifiers::NONE);
    assert!(h.app.panel_min, "F4 collapses");
    let n = h.app.last_seq;
    assert!(
        h.until(2.0, |a| a.last_seq > n),
        "captures keep coming while collapsed"
    );
    h.run(1.2); // settings saved
    let saved = std::fs::read_to_string(h.iso.cfg().join("settings.txt")).unwrap();
    assert!(saved.contains("panel_min=1"), "saved");
    h.key(Key::F4, Modifiers::NONE);
    assert!(!h.app.panel_min, "F4 expands again");
    h.run(0.3);
    assert!(h.app.panel_over <= 0.5);
}

/// Measurement table hidden with F3 (owner, 2026-11-23: with 8 channels it took half the screen): one line left,
/// the graph gets the room, captures keep coming, the setting is saved, F3 brings it back.
#[test]
fn measurements_hide_with_f3() {
    let mut h = harness("meas_hide", None);
    h.live();
    h.app.on = [true; CHANNELS];
    h.run(0.5);
    let shown = h.plot_rect().height();
    assert!(h.app.meas_on);
    h.key(Key::F3, Modifiers::NONE);
    h.run(0.3);
    assert!(!h.app.meas_on, "F3 hides");
    let hidden = h.plot_rect().height();
    assert!(
        hidden > shown + 100.0,
        "the graph gets the room: {shown} → {hidden}"
    );
    let n = h.app.last_seq;
    assert!(
        h.until(2.0, |a| a.last_seq > n),
        "captures keep coming while hidden"
    );
    h.run(1.2); // settings saved
    let saved = std::fs::read_to_string(h.iso.cfg().join("settings.txt")).unwrap();
    assert!(saved.contains("meas=0"), "saved");
    h.key(Key::F3, Modifiers::NONE);
    assert!(h.app.meas_on, "F3 shows it again");
    h.run(0.3);
    println!("measurements: plot height {shown} px shown, {hidden} px hidden");
}
