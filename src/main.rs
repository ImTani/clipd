//! `clipd` binary entry point.
//!
//! At this milestone the engine (capture/encode/audio/ring/mux threads) is not
//! yet wired — this shell exists to prove the pure-logic modules build into a
//! runnable binary and to provide the `--check-config` calibration surface
//! (`01-PROJECT-PLAN.md §3` pitfall 30). Per `CLAUDE.md`, `expect`/`unwrap` are
//! permitted here because this runs before any worker thread starts.

use std::path::PathBuf;
use std::process::ExitCode;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clipd::capture::convert::Converter;
use clipd::capture::wgc::WgcCapture;
use clipd::com::{ComMta, MediaFoundation};
use clipd::config::{default_config_path, Config};
use clipd::encode::mft_h264::{EncoderConfig, H264Encoder, InputFrame};
use clipd::engine::{RecordOutcome, RecordParams, RecordingEngine};
use clipd::gpu::{self, AdapterSelection, GpuContext, GpuError};
use clipd::spec_constants::{self, PRODUCT_NAME};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_usage() {
    println!(
        "{PRODUCT_NAME} {VERSION}\n\
         \n\
         USAGE:\n    \
             {PRODUCT_NAME} [OPTIONS]\n\
         \n\
         OPTIONS:\n    \
             record [--seconds N]    Record the primary monitor to an MP4 (Milestone 1).\n           \
                    [--out PATH]     Stops after N seconds, or on Enter if omitted.\n    \
             --check-config [PATH]   Validate config (default: %APPDATA%\\{PRODUCT_NAME}\\config.toml)\n                            \
                                     and print the effective settings, then exit.\n    \
             probe-gpu               Print the GPU/output topology and the adapter the\n                            \
                                     shared device lands on, then exit.\n    \
             capture-probe [SECS]    Capture the primary monitor for SECS (default 3) and\n                            \
                                     report delivered frames + texture format, then exit.\n    \
             convert-probe           Capture one frame, convert BGRA->NV12 on the video\n                            \
                                     processor, and report the NV12 output, then exit.\n    \
             encode-probe [SECS]     Capture->convert->encode H.264 CQP for SECS (default 2)\n                            \
                                     to a .h264 file for ffprobe, then exit.\n    \
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

/// Run `encode-probe`: drive the real capture→convert→encode path for a few
/// seconds and write an Annex-B `.h264` elementary stream for `ffprobe`.
/// Exercises Milestone-1 Task E on hardware (the async MFT + CQP) without the
/// mux. Colour/CQP correctness is judged from the ffprobe output.
fn run_encode_probe(seconds: u64) -> ExitCode {
    let _com = ComMta::initialize();
    let _mf = match MediaFoundation::startup() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: MFStartup failed: {e}");
            return ExitCode::from(2);
        }
    };

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
    let (w, h) = (capture.width(), capture.height());
    let fps = spec_constants::video::DEFAULT_FPS;
    let mut converter = match Converter::new(&gpu, w, h, fps) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: converter: {e}");
            return ExitCode::from(2);
        }
    };

    let cq = spec_constants::encoder::NVENC_CQ[0] as u32;
    let gop = spec_constants::ring::gop_frames(spec_constants::ring::IDR_INTERVAL_SECONDS, fps);
    let config = EncoderConfig {
        width: w,
        height: h,
        fps,
        cq,
        gop_frames: gop,
    };
    let mut encoder = match H264Encoder::new(&gpu, config) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: encoder init failed: {e}");
            return ExitCode::from(2);
        }
    };

    let out_path = std::env::temp_dir().join(format!("{PRODUCT_NAME}_encode_probe.h264"));
    let file = match File::create(&out_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: could not create {}: {e}", out_path.display());
            return ExitCode::from(2);
        }
    };
    let mut writer = BufWriter::new(file);
    println!(
        "encoding {w}x{h}@{fps} CQ{cq} (GOP {gop}) for {seconds}s -> {}",
        out_path.display()
    );

    let ticks_per_second = spec_constants::units::TICKS_PER_SECOND;
    let target: u64 = seconds * fps as u64;
    let duration = ticks_per_second / fps as i64;

    let mut index: u64 = 0;
    let mut last_nv12 = None;
    let mut frames_in: u64 = 0;

    // Pull-based source: convert a fresh frame when one is available, else reuse
    // the last NV12 (a static screen delivers few WGC frames — the pacing grid
    // does this properly in the engine; the probe approximates it).
    let next_input = || -> Option<InputFrame> {
        if index >= target {
            return None;
        }
        let nv12 = loop {
            if let Some(frame) = capture.take_latest() {
                if let Ok(bgra) = frame.texture() {
                    if let Ok(n) = converter.convert(&bgra) {
                        last_nv12 = Some(n.clone());
                        break n;
                    }
                }
            }
            if let Some(n) = last_nv12.clone() {
                break n;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        let pts = (index as i64 * ticks_per_second) / fps as i64;
        index += 1;
        frames_in += 1;
        Some(InputFrame {
            texture: nv12,
            pts,
            duration,
            epoch_id: 0,
        })
    };

    let mut frames_out: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut keyframes: u64 = 0;
    let mut write_err = None;
    let on_packet = |pkt: clipd::encode::mft_h264::EncodedPacket| {
        if let Err(e) = writer.write_all(&pkt.data) {
            write_err = Some(e);
        }
        frames_out += 1;
        total_bytes += pkt.data.len() as u64;
        if pkt.is_keyframe {
            keyframes += 1;
        }
    };

    if let Err(e) = encoder.run(next_input, on_packet) {
        eprintln!("error: encode loop failed: {e}");
        return ExitCode::from(2);
    }
    if let Err(e) = writer.flush() {
        eprintln!("error: flush failed: {e}");
        return ExitCode::from(2);
    }
    if let Some(e) = write_err {
        eprintln!("error: writing encoded stream failed: {e}");
        return ExitCode::from(2);
    }

    let avg_kbps = (total_bytes * 8 / 1000).checked_div(seconds).unwrap_or(0);
    println!(
        "done: {frames_in} in / {frames_out} out, {keyframes} keyframes, {total_bytes} bytes (~{avg_kbps} kbps avg)"
    );
    println!("validate: ffprobe -show_streams \"{}\"", out_path.display());
    ExitCode::SUCCESS
}

/// Initialize the `tracing` file/console subscriber (idempotent). `RUST_LOG`
/// controls the filter; defaults to `info`.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Resolve the output path when `--out` is not given: `<output.dir or CWD>/`
/// `clipd_<unix_secs>.mp4`. Full `filename_template` resolution (date/time
/// placeholders) is a later-milestone polish.
fn default_output_path(cfg: &Config) -> PathBuf {
    let dir = if cfg.output.dir.is_empty() {
        std::env::current_dir().unwrap_or_default()
    } else {
        PathBuf::from(&cfg.output.dir)
    };
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    dir.join(format!("{PRODUCT_NAME}_{secs}.mp4"))
}

/// Run `record`: the Milestone-1 dumb recorder. Records the primary monitor to an
/// MP4 for `--seconds N`, or until Enter when omitted.
fn run_record(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut seconds: Option<u64> = None;
    let mut out: Option<PathBuf> = None;
    let mut simulate: Option<u64> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => seconds = args.next().and_then(|s| s.parse().ok()),
            "--out" => out = args.next().map(PathBuf::from),
            // Test hook: inject a synthetic device loss after N seconds to exercise
            // the epoch-restart path without an actual sleep/resume.
            "--simulate-device-loss" => simulate = args.next().and_then(|s| s.parse().ok()),
            other => {
                eprintln!("record: unrecognized argument '{other}'");
                return ExitCode::from(2);
            }
        }
    }

    init_tracing();
    let _com = ComMta::initialize();
    let _mf = match MediaFoundation::startup() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: MFStartup failed: {e}");
            return ExitCode::from(2);
        }
    };

    let cfg = {
        let path = default_config_path();
        if path.exists() {
            Config::load(&path).unwrap_or_default()
        } else {
            Config::default()
        }
    };

    let fps = cfg.capture.fps;
    let cursor = cfg.capture.cursor;
    let cq = spec_constants::encoder::NVENC_CQ[0] as u32;
    let gop = spec_constants::ring::gop_frames(spec_constants::ring::IDR_INTERVAL_SECONDS, fps);
    let base_path = out.unwrap_or_else(|| default_output_path(&cfg));

    let stop = Arc::new(AtomicBool::new(false));
    arm_stop(&stop, seconds);
    println!(
        "recording primary monitor @ {fps} fps (CQ{cq}); output base {}",
        base_path.display()
    );

    // Epoch loop: each epoch is one segment file. A device loss (sleep/resume,
    // driver reset — spec §7) ends the epoch; the segment is finalized and the
    // pipeline is rebuilt for the next one (a clip must not span epochs, §0).
    let mut epoch: u32 = 0;
    loop {
        let segment = segment_path(&base_path, epoch);
        let gpu = match build_gpu(epoch > 0) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("error: could not create the shared D3D11 device: {e}");
                return ExitCode::from(2);
            }
        };
        if epoch == 0 {
            println!("-> {}", segment.display());
        } else {
            println!(
                "epoch {epoch}: rebuilt after device loss -> {}",
                segment.display()
            );
        }

        let params = RecordParams {
            output_path: segment,
            fps,
            cursor,
            cq,
            gop_frames: gop,
            // Only the first epoch simulates a loss, so the rebuild doesn't loop.
            simulate_loss_after: if epoch == 0 { simulate } else { None },
        };
        // The engine owns its own stop flag; `stop` here is the user-stop that
        // ends the whole recording (not a per-epoch signal).
        let engine = RecordingEngine::start(gpu, params);

        // Wait until a stop is requested or a worker exits early (device loss).
        let mut ticks = 0u32;
        while !stop.load(Ordering::Relaxed) && !engine.any_worker_finished() {
            std::thread::sleep(Duration::from_millis(100));
            ticks += 1;
            if ticks.is_multiple_of(10) {
                engine.stats().check_divergence();
            }
        }

        match engine.stop_and_join() {
            Ok(RecordOutcome::Completed(stats)) => {
                println!(
                    "done: {} captured / {} encoded / {} muxed -> {}",
                    stats.captured,
                    stats.encoded,
                    stats.muxed,
                    stats.output_path.display()
                );
                return ExitCode::SUCCESS;
            }
            Ok(RecordOutcome::DeviceLost(stats)) => {
                println!(
                    "device lost after {} frames; segment saved -> {}",
                    stats.muxed,
                    stats.output_path.display()
                );
                if stop.load(Ordering::Relaxed) {
                    return ExitCode::SUCCESS; // stop was also requested
                }
                epoch += 1;
                // Epoch-restart budget (spec §7): let the device return.
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                eprintln!("record failed: {e}");
                return ExitCode::from(2);
            }
        }
    }
}

/// Arm the stop trigger: a timer for `--seconds`, or an Enter-key watcher.
fn arm_stop(stop: &Arc<AtomicBool>, seconds: Option<u64>) {
    let stop = stop.clone();
    match seconds {
        Some(n) => {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(n));
                stop.store(true, Ordering::Relaxed);
            });
        }
        None => {
            println!("press Enter to stop recording");
            std::thread::spawn(move || {
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                stop.store(true, Ordering::Relaxed);
            });
        }
    }
}

/// The output path for epoch `epoch`: the base for epoch 0, else `stem-N.ext`.
fn segment_path(base: &std::path::Path, epoch: u32) -> PathBuf {
    if epoch == 0 {
        return base.to_path_buf();
    }
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
    let ext = base.extension().and_then(|s| s.to_str()).unwrap_or("mp4");
    let name = format!("{stem}-{epoch}.{ext}");
    match base.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

/// Create the shared device. On a rebuild, retry within the epoch-restart budget
/// (~2 s) while the device comes back after sleep/resume.
fn build_gpu(is_rebuild: bool) -> Result<GpuContext, GpuError> {
    if !is_rebuild {
        return GpuContext::new(AdapterSelection::Auto);
    }
    let mut last_err = None;
    for _ in 0..20 {
        match GpuContext::new(AdapterSelection::Auto) {
            Ok(gpu) => return Ok(gpu),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    Err(last_err.expect("at least one attempt was made"))
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
        Some("record") => run_record(args),
        Some("convert-probe") => run_convert_probe(),
        Some("encode-probe") => {
            let seconds = args.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(2);
            run_encode_probe(seconds)
        }
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
