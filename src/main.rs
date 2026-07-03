//! `clipd` binary entry point.
//!
//! At this milestone the engine (capture/encode/audio/ring/mux threads) is not
//! yet wired — this shell exists to prove the pure-logic modules build into a
//! runnable binary and to provide the `--check-config` calibration surface
//! (`01-PROJECT-PLAN.md §3` pitfall 30). Per `CLAUDE.md`, `expect`/`unwrap` are
//! permitted here because this runs before any worker thread starts.

use std::path::PathBuf;
use std::process::ExitCode;

use clipd::capture::convert::Converter;
use clipd::capture::wgc::WgcCapture;
use clipd::com::ComMta;
use clipd::config::{default_config_path, Config};
use clipd::gpu::{self, AdapterSelection, GpuContext};
use clipd::spec_constants::PRODUCT_NAME;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_usage() {
    println!(
        "{PRODUCT_NAME} {VERSION}\n\
         \n\
         USAGE:\n    \
             {PRODUCT_NAME} [OPTIONS]\n\
         \n\
         OPTIONS:\n    \
             --check-config [PATH]   Validate config (default: %APPDATA%\\{PRODUCT_NAME}\\config.toml)\n                            \
                                     and print the effective settings, then exit.\n    \
             probe-gpu               Print the GPU/output topology and the adapter the\n                            \
                                     shared device lands on, then exit.\n    \
             capture-probe [SECS]    Capture the primary monitor for SECS (default 3) and\n                            \
                                     report delivered frames + texture format, then exit.\n    \
             convert-probe           Capture one frame, convert BGRA->NV12 on the video\n                            \
                                     processor, and report the NV12 output, then exit.\n    \
             -V, --version           Print version and exit.\n    \
             -h, --help              Print this help and exit.\n\
         \n\
         With no options the engine would start; it is not yet implemented\n\
         (Milestone 0 pending)."
    );
}

/// Run `probe-gpu`: print the full adapter/output topology and which adapter the
/// `Auto`-selected shared device lands on. This closes the `04-TEST-MACHINE.md`
/// "adapter topology" pre-Milestone-1 task on real hardware.
fn run_probe_gpu() -> ExitCode {
    let topology = match gpu::enumerate_topology() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: could not enumerate GPU topology: {e}");
            return ExitCode::from(2);
        }
    };
    print!("{topology}");

    match GpuContext::new(AdapterSelection::Auto) {
        Ok(ctx) => {
            let co_located = topology
                .primary_adapter_index()
                .and_then(|i| topology.adapters.get(i as usize))
                .map(|a| a.luid == ctx.adapter_luid)
                .unwrap_or(false);
            println!(
                "\nAuto-selected device adapter: {} (luid {:#018x})",
                ctx.adapter_description, ctx.adapter_luid
            );
            println!(
                "Co-located with the primary-output adapter: {}",
                if co_located {
                    "yes (same-adapter WGC copy)"
                } else {
                    "no (WGC does a cross-adapter copy into this device's pool)"
                }
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: could not create the shared D3D11 device: {e}");
            ExitCode::from(2)
        }
    }
}

/// Run `capture-probe`: capture the primary monitor for a few seconds through
/// the real `capture::wgc` module and report delivered frames, measured fps, and
/// the backing texture format. Exercises Milestone-1 Task B on hardware without
/// the encode/mux stages.
fn run_capture_probe(seconds: u64) -> ExitCode {
    // The engine is all-MTA; this diagnostic runs on the main thread, so it owns
    // the apartment guard for its lifetime.
    let _com = ComMta::initialize();

    let gpu = match GpuContext::new(AdapterSelection::Auto) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: could not create the shared D3D11 device: {e}");
            return ExitCode::from(2);
        }
    };

    let capture = match WgcCapture::start_primary(&gpu, true) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: could not start capture: {e}");
            return ExitCode::from(2);
        }
    };
    println!(
        "capturing primary monitor {}x{} for {seconds}s on {} — move the mouse / play a video for a real fps",
        capture.width(),
        capture.height(),
        gpu.adapter_description,
    );

    let start = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_secs(seconds));
    let elapsed = start.elapsed().as_secs_f64();

    let frames = capture.frames_delivered();
    let fps = if elapsed > 0.0 {
        frames as f64 / elapsed
    } else {
        0.0
    };
    println!("delivered {frames} frames in {elapsed:.2}s ({fps:.1} fps)");

    match capture.take_latest() {
        Some(frame) => match frame.descriptor() {
            Ok((format, w, h)) => {
                // 87 == DXGI_FORMAT_B8G8R8A8_UNORM (the SDR pool format we request).
                println!(
                    "latest frame: DXGI_FORMAT={format} {w}x{h}, SystemRelativeTime={} ticks",
                    frame.system_relative_time
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: could not read frame descriptor: {e}");
                ExitCode::from(2)
            }
        },
        None => {
            eprintln!(
                "warning: no frame captured — the screen was fully static; re-run with on-screen motion"
            );
            ExitCode::from(1)
        }
    }
}

/// Run `convert-probe`: capture one frame and convert it BGRA→NV12 on the video
/// processor, reporting the output descriptor. Exercises Milestone-1 Task C on
/// hardware. Full colour verification (BT.709 limited) needs a saved clip +
/// RenderDoc (Task F1); this just proves the video-processor Blt succeeds and
/// yields NV12.
fn run_convert_probe() -> ExitCode {
    let _com = ComMta::initialize();

    let gpu = match GpuContext::new(AdapterSelection::Auto) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: could not create the shared D3D11 device: {e}");
            return ExitCode::from(2);
        }
    };
    let capture = match WgcCapture::start_primary(&gpu, true) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: could not start capture: {e}");
            return ExitCode::from(2);
        }
    };

    // Wait for a frame to arrive (up to ~1 s), nudging past a static screen.
    let mut frame = None;
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        if let Some(f) = capture.take_latest() {
            frame = Some(f);
            break;
        }
    }
    let Some(frame) = frame else {
        eprintln!("warning: no frame captured within 1s; re-run with on-screen motion");
        return ExitCode::from(1);
    };

    let (w, h) = (capture.width(), capture.height());
    let mut converter = match Converter::new(&gpu, w, h, 60) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: could not create the converter: {e}");
            return ExitCode::from(2);
        }
    };
    let input = match frame.texture() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: could not reach the input texture: {e}");
            return ExitCode::from(2);
        }
    };
    match converter.convert(&input) {
        Ok(_nv12) => {
            let (ow, oh) = converter.dimensions();
            println!(
                "converted BGRA {w}x{h} -> NV12 (DXGI_FORMAT=103) {ow}x{oh} on the video processor: OK"
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: BGRA->NV12 conversion failed: {e}");
            ExitCode::from(2)
        }
    }
}

/// Run `--check-config`: load (or default) the config at `path`, print the
/// effective TOML, and return the process exit code.
fn run_check_config(path: PathBuf) -> ExitCode {
    if path.exists() {
        match Config::load(&path) {
            Ok(cfg) => {
                println!("# effective config from {}", path.display());
                match cfg.to_toml() {
                    Ok(toml) => {
                        print!("{toml}");
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        ExitCode::from(2)
                    }
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        }
    } else {
        // No file: show the defaults so the user sees what would take effect.
        let cfg = Config::default();
        println!(
            "# no config file at {}; showing built-in defaults",
            path.display()
        );
        match cfg.to_toml() {
            Ok(toml) => {
                print!("{toml}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        }
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("-h") | Some("--help") => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some("-V") | Some("--version") => {
            println!("{PRODUCT_NAME} {VERSION}");
            ExitCode::SUCCESS
        }
        Some("--check-config") => {
            let path = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(default_config_path);
            run_check_config(path)
        }
        Some("probe-gpu") => run_probe_gpu(),
        Some("capture-probe") => {
            let seconds = args.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(3);
            run_capture_probe(seconds)
        }
        Some("convert-probe") => run_convert_probe(),
        Some(other) => {
            eprintln!("error: unrecognized option '{other}'\n");
            print_usage();
            ExitCode::from(2)
        }
        None => {
            println!(
                "{PRODUCT_NAME} {VERSION}: engine not yet implemented (Milestone 0 pending).\n\
                 Try `{PRODUCT_NAME} --check-config` or `{PRODUCT_NAME} --help`."
            );
            ExitCode::SUCCESS
        }
    }
}
