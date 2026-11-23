// Windows: a GUI program (no black console window next to the scope); the CLI subcommands attach to the console
// they were started from (attach_console).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod acq;
mod analyze;
mod bench;
mod calibrate;
mod check;
mod dsp;
mod hantek;
mod probe;
mod rec;
#[cfg(test)]
mod regress;
mod soak;
mod ui;

const USAGE: &str = "usage: hantek1008c [--sim]            scope GUI (--sim: simulated scope)
       hantek1008c check            hardware self-test (CH1 on the internal 1 kHz 2 Vpp generator)
       hantek1008c calibrate --ch N --volts V   gain calibration against a multimeter-measured DC source
       hantek1008c soak [minutes] [dir]   headless long run on the simulator (memory, losses)
       hantek1008c bench [--timing fast|reference|lean|none] [--set field=value]... [--frames N] [--dump]";

/// Windows: a GUI-subsystem program has no console; a subcommand started from cmd / PowerShell prints into that one.
#[cfg(windows)]
fn attach_console() {
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    // SAFETY: plain Win32 call without pointers; failure (started without a console) is harmless
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    if args.first().is_some_and(|a| a != "--sim") {
        attach_console(); // before the logger takes stderr
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let r = match args.first().map(String::as_str) {
        Some("bench") => bench::run(&args[1..]),
        Some("check") => check::run(&args[1..]),
        Some("soak") => soak::run(&args[1..]),
        Some("calibrate") => calibrate::run(&args[1..]),
        Some("probe") => probe::run(&args[1..]),
        None | Some("--sim") => gui(!args.is_empty()),
        _ => Err(USAGE.into()),
    };
    if let Err(e) = r {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

/// Window icon (also installed for the launcher by scripts/install-linux.sh).
fn icon() -> Option<eframe::egui::IconData> {
    let dec = png::Decoder::new(std::io::Cursor::new(include_bytes!(
        "../assets/icons/lrscope_256.png"
    )));
    let mut r = dec.read_info().ok()?;
    let mut buf = vec![0; r.output_buffer_size()?];
    let info = r.next_frame(&mut buf).ok()?;
    (info.color_type == png::ColorType::Rgba).then(|| eframe::egui::IconData {
        rgba: buf[..info.buffer_size()].to_vec(),
        width: info.width,
        height: info.height,
    })
}

fn gui(sim: bool) -> Result<(), String> {
    let opts = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 850.0])
            .with_title("LR Scope 1008C")
            // = the desktop file's name, so the dock/launcher groups the window with its launcher icon
            .with_app_id("lrscope")
            .with_icon(icon().unwrap_or_default()),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "hantek1008c",
        opts,
        Box::new(move |cc| Ok(Box::new(ui::App::new(cc, sim)))),
    )
    .map_err(|e| e.to_string())
}
