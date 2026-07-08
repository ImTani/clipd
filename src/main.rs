//! `clipd` binary entry point.
//!
//! Argument dispatch + the top-level subcommands (`record`, `buffer`,
//! `--check-config`, and the `*-probe` diagnostics) that wire the engine
//! (capture/encode/audio/ring/mux threads) together. Per `CLAUDE.md`,
//! `expect`/`unwrap` are permitted here because argument/config handling runs
//! before any worker thread starts.

use std::path::PathBuf;
use std::process::ExitCode;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clipd::capture::convert::Converter;
use clipd::capture::wgc::{CaptureSource, WgcCapture};
use clipd::com::{ComMta, MediaFoundation};
use clipd::config::{
    default_config_path, default_output_dir, resolve_output_dir, CaptureTarget, Config, NamedTarget,
};
use clipd::encode::mft_h264::{
    rc_mode_from_str, EncoderConfig, EncoderOverrides, H264Encoder, InputFrame,
};
use clipd::engine::{BufferEngine, BufferParams};
use clipd::gpu::{self, AdapterSelection, GpuContext, GpuError};
use clipd::hotkey::HotkeyPump;
use clipd::spec_constants::{self, PRODUCT_NAME};
use clipd::ui;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_usage() {
    println!(
        "{PRODUCT_NAME} {VERSION}\n\
         \n\
         USAGE:\n    \
             {PRODUCT_NAME} [OPTIONS]\n\
         \n\
         OPTIONS:\n    \
             record [--seconds N]    Record the capture target straight to an MP4.\n           \
                    [--out PATH]     Stops after N seconds, or on Enter if omitted.\n    \
             buffer [--seconds N]    Replay buffer (Milestone 3): capture into an in-memory\n                            \
                                     ring; the save hotkey writes the last N seconds\n                            \
                                     ([buffer].seconds, override with --seconds). Enter quits.\n    \
             --check-config [PATH]   Validate config (default: %APPDATA%\\{PRODUCT_NAME}\\config.toml)\n                            \
                                     and print the effective settings, then exit.\n    \
             probe-gpu               Print the GPU/output topology and the adapter the\n                            \
                                     shared device lands on, then exit.\n    \
             capture-probe [SECS]    Capture the primary monitor for SECS (default 3) and\n                            \
                                     report delivered frames + texture format, then exit.\n    \
             window-capture-probe [SECS]  After a countdown, capture the FOCUSED WINDOW for\n                            \
                                     SECS (default 5) and report frames + size (M4), then exit.\n    \
             window-events-probe [SECS]   Watch the FOCUSED WINDOW for SECS (default 30) and log\n                            \
                                     resize (ContentSize) + close events (M4-2), then exit.\n    \
             convert-probe           Capture one frame, convert BGRA->NV12 on the video\n                            \
                                     processor, and report the NV12 output, then exit.\n    \
             encode-probe [SECS]     Capture->convert->encode H.264 CQP for SECS (default 2)\n                            \
                                     to a .h264 file for ffprobe, then exit.\n    \
             audio-probe [SECS]      Capture desktop-loopback + mic for SECS (default 6) and\n                            \
                                     report per-stream packet/frame/silence/gap stats, then exit.\n    \
             binding-probe [SECS]    Print detected game / voice-chat PIDs for SECS (default 30)\n                            \
                                     via the B3 binding OS providers, then exit.\n    \
             list-audio-devices      List active capture (microphone) endpoints (id + name)\n                            \
                                     via the B3.5 device enumeration, then exit.\n    \
             toast-test              Fire success + failure save balloons (hidden + visible\n                            \
                                     entry) and print the Shell_NotifyIcon results, then exit.\n    \
             aac-probe [SECS]        Encode a SECS (default 2) tone through the AAC-LC MFT and\n                            \
                                     report access-unit count + AudioSpecificConfig, then exit.\n    \
             -V, --version           Print version and exit.\n    \
             -h, --help              Print this help and exit.\n\
         \n\
         With no options, prints this help. Run `buffer` for the replay buffer or\n\
         `record` to record straight to disk."
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

/// Run `window-capture-probe`: after a short countdown (switch to your target
/// window), capture the **focused window** for a few seconds and report delivered
/// frames, measured fps, and the backing size/format. Exercises the M4-1
/// focused-window path on hardware (`CreateForWindow` + foreground resolution).
/// If the reported size matches your monitor, capture fell back to the primary
/// monitor — check the log for the fallback warning (own console / exclusive-FS).
fn run_window_capture_probe(seconds: u64) -> ExitCode {
    init_tracing();
    let _com = ComMta::initialize();

    let gpu = match GpuContext::new(AdapterSelection::Auto) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: could not create the shared D3D11 device: {e}");
            return ExitCode::from(2);
        }
    };

    // Countdown so the tester can alt-tab to the borderless/windowed target — the
    // foreground window is resolved once, when capture starts (M4 v1 behavior).
    for n in (1..=3).rev() {
        println!("focus your target window — capturing the foreground window in {n}...");
        std::thread::sleep(Duration::from_secs(1));
    }

    // Cursor off for a game window (pitfall 10 default); the source resolves the
    // foreground window and falls back to the primary monitor if it can't.
    let capture = match WgcCapture::start(&gpu, CaptureSource::FocusedWindow, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: could not start capture: {e}");
            return ExitCode::from(2);
        }
    };
    println!(
        "capturing focused window {}x{} for {seconds}s on {} — keep the window active for a real fps",
        capture.width(),
        capture.height(),
        gpu.adapter_description,
    );

    let start = std::time::Instant::now();
    std::thread::sleep(Duration::from_secs(seconds));
    let elapsed = start.elapsed().as_secs_f64();

    let frames = capture.frames_delivered();
    let fps = if elapsed > 0.0 {
        frames as f64 / elapsed
    } else {
        0.0
    };
    println!(
        "delivered {frames} frames in {elapsed:.2}s ({fps:.1} fps) at {}x{}",
        capture.width(),
        capture.height()
    );
    if frames == 0 {
        eprintln!(
            "warning: no frames — a fully static or exclusive-fullscreen window delivers none; \
             re-run with the window active/borderless"
        );
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// Run `window-events-probe`: capture the focused window and watch for the WGC
/// events the M4-2 epoch restart will hinge on — `ContentSize` changes (a **resize**,
/// incl. moving across monitors with different DPI) and the item's `Closed` event (a
/// **close**). Logs each event so the exact on-hardware behaviour can be observed
/// before the resize/close triggers are wired. Resize the window, drag it to another
/// monitor, then close it, and report what this prints.
fn run_window_events_probe(seconds: u64) -> ExitCode {
    init_tracing();
    let _com = ComMta::initialize();

    let gpu = match GpuContext::new(AdapterSelection::Auto) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: could not create the shared D3D11 device: {e}");
            return ExitCode::from(2);
        }
    };
    for n in (1..=3).rev() {
        println!("focus your target window — watching the foreground window in {n}...");
        std::thread::sleep(Duration::from_secs(1));
    }
    let capture = match WgcCapture::start(&gpu, CaptureSource::FocusedWindow, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: could not start capture: {e}");
            return ExitCode::from(2);
        }
    };
    let pool_size = (capture.width(), capture.height());
    println!(
        "watching focused window {}x{} for {seconds}s — RESIZE it, drag it to another monitor, \
         then CLOSE it; each ContentSize change and the Closed event is logged",
        pool_size.0, pool_size.1
    );

    let start = std::time::Instant::now();
    let mut last_size = pool_size;
    let mut resize_events = 0u64;
    let mut closed_seen = false;
    while start.elapsed() < Duration::from_secs(seconds) {
        if let Some(frame) = capture.take_latest() {
            if let Ok(cs) = frame.content_size() {
                if cs != last_size {
                    resize_events += 1;
                    println!(
                        "[resize] ContentSize {}x{} -> {}x{} (pool still {}x{})",
                        last_size.0, last_size.1, cs.0, cs.1, pool_size.0, pool_size.1
                    );
                    last_size = cs;
                }
            }
        }
        if !closed_seen && capture.is_closed() {
            closed_seen = true;
            println!("[closed] the item's Closed event fired (window closed / display removed)");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    println!(
        "done: {resize_events} ContentSize change(s), closed={closed_seen}, final content size {}x{}",
        last_size.0, last_size.1
    );
    ExitCode::SUCCESS
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
    // Probe captures the monitor: canvas = the evened monitor size (no letterbox).
    let mut converter = match Converter::new(&gpu, (w, h), (w & !1, h & !1), 60) {
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
    let mut converter = match Converter::new(&gpu, (w, h), (w & !1, h & !1), fps) {
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
        target_bitrate_bps: spec_constants::encoder::video_target_bitrate_bps(
            w,
            h,
            fps,
            spec_constants::encoder::QUALITY_MULT_DEFAULT,
        ),
        peak_bitrate_bps: spec_constants::encoder::video_peak_bitrate_bps(
            w,
            h,
            fps,
            spec_constants::encoder::QUALITY_MULT_DEFAULT,
        ),
        overrides: EncoderOverrides::default(),
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
        "encoding {w}x{h}@{fps} VBR (GOP {gop}) for {seconds}s -> {}",
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

/// Run `binding-probe`: exercise the B3 game/voice-chat **binding** OS providers
/// (`audio::binding::{enumerate_processes, foreground_window}`) + the pure selectors,
/// printing the currently-detected Game / Voice-chat PIDs once per scan for `seconds`.
/// This is the manual HW instrument for B3 — it runs the EXACT code the engine's
/// binding watcher runs (no re-implementation to drift), just without spawning capture.
///
/// ## B7 checklist (run on the Nitro; `just run -- binding-probe 30`)
/// - **Voice chat / Discord tray-minimized:** launch Discord and let it minimize to the
///   tray (no window). The probe should print a `voice-chat` binding within one scan,
///   with a PID whose parent is NOT Discord (the Electron main, include-tree). Quitting
///   Discord clears it (→ `none`) within one scan.
/// - **VC config order:** with several `vc_apps` running, the first *enabled* one in
///   `config.toml` order is bound.
/// - **Game / borderless-fullscreen:** run a game (or any app) borderless-fullscreen on
///   the primary monitor → a `game` binding appears with that app's PID. Alt-tab to a
///   *windowed* app → the game binding clears (foreground no longer fullscreen). A
///   *different* fullscreen app retargets the PID.
/// - **No false game bind on the desktop:** with only the Windows desktop / a maximized
///   (taskbar-visible) window foreground, `game` stays `none`.
/// - Cross-check the printed PIDs against Task Manager's "Details" tab (PID column).
fn run_binding_probe(seconds: u64) -> ExitCode {
    use clipd::audio::binding::{
        classify_game, enumerate_processes, foreground_window, select_vc_pid, Binding, GameDetect,
    };

    init_tracing();

    let cfg = {
        let path = default_config_path();
        if path.exists() {
            Config::load(&path).unwrap_or_default()
        } else {
            Config::default()
        }
    };
    let vc_apps = cfg.audio.vc_apps;

    if !clipd::audio::process_loopback::process_loopback_supported() {
        println!(
            "WARNING: this Windows build is below the 2004 process-loopback floor — the \
             engine hides the per-app tracks here; the probe still shows detection."
        );
    }
    let enabled_vc: Vec<&str> = vc_apps
        .iter()
        .filter(|a| a.enabled)
        .map(|a| a.name.as_str())
        .collect();
    println!(
        "binding-probe for {seconds}s (monitor-mode foreground-fullscreen game detection; \
         scanning for VC apps: {enabled_vc:?})\n\
         → open Discord (tray-minimized), run a borderless-fullscreen app; Ctrl+C aborts"
    );

    let fmt = |b: Option<Binding>| match b {
        Some(b) => format!("pid {} (include_tree={})", b.pid, b.include_tree),
        None => "none".to_string(),
    };

    let stop_at = Instant::now() + Duration::from_secs(seconds);
    let mut last_game: Option<Option<Binding>> = None;
    let mut last_vc: Option<Option<Binding>> = None;
    while Instant::now() < stop_at {
        let procs = enumerate_processes();
        let fg = foreground_window();
        let raw_game = classify_game(GameDetect::ForegroundFullscreen, fg);
        // Mirror the watcher's liveness filter: only bind a PID that is actually running.
        let game = raw_game.filter(|b| procs.iter().any(|p| p.pid == b.pid));
        let vc = select_vc_pid(&procs, &vc_apps);

        if last_game.as_ref() != Some(&game) || last_vc.as_ref() != Some(&vc) {
            println!(
                "  processes={:4}  foreground={:?}  game={}  voice-chat={}",
                procs.len(),
                fg.map(|f| f.pid),
                fmt(game),
                fmt(vc)
            );
            last_game = Some(game);
            last_vc = Some(vc);
        }
        std::thread::sleep(Duration::from_millis(600));
    }
    println!("binding-probe done");
    ExitCode::SUCCESS
}

/// Run `list-audio-devices`: print the active capture (microphone) endpoints — each
/// `id <TAB> friendly-name` — via the EXACT `audio::devices::enumerate_capture_devices`
/// path the settings mic picker (B3.5) uses (no re-implementation to drift). The manual
/// HW instrument for B3.5.
///
/// ## B7 checklist (run on the Nitro; `just run -- list-audio-devices`)
/// - The FIFINE mic + any other capture endpoints are listed with sane friendly names.
/// - The printed id is what `[audio].mic` wants: set `[audio].mic = "<that id>"`,
///   restart `buffer`, and confirm the mic track opens that device (log / VU meter).
/// - Unplug the FIFINE → re-run → it drops from the list; replug → it returns (proves
///   the enumeration is live, backing the Settings dropdown's refresh-on-reopen).
/// - In Settings → Microphone the same devices appear; picking one + Save + restart
///   uses it; a previously-pinned-but-now-unplugged device shows as `Unavailable: <id>`
///   and is NOT silently replaced by another device (`§7`).
fn run_list_audio_devices() -> ExitCode {
    init_tracing();
    let devices = clipd::audio::devices::enumerate_capture_devices();
    if devices.is_empty() {
        println!(
            "no active capture (microphone) endpoints found \
             (or enumeration failed — check the log)"
        );
    } else {
        println!("active capture (microphone) endpoints:");
        for d in &devices {
            println!("  {}\t{}", d.id, d.name);
        }
        println!(
            "\npin one by setting [audio].mic = \"<id>\" in config.toml \
             (or use Settings → Microphone)."
        );
    }
    ExitCode::SUCCESS
}

/// Run `audio-probe`: capture the desktop-loopback and mic streams for a few
/// seconds through the real `audio::wasapi_stream` worker and report per-stream
/// packet/frame/silence/gap stats. Exercises Milestone-2 Task 2 on hardware
/// (WASAPI capture + QPC stamping) without the resample/AAC/mux stages.
fn run_audio_probe(seconds: u64) -> ExitCode {
    use clipd::audio::devices::DeviceSelection;
    use clipd::audio::wasapi_stream::{run_capture, AudioPacket, AudioSource, AudioTrackKind};
    use clipd::spec_constants::units::TICKS_PER_SECOND;
    use crossbeam_channel::bounded;

    init_tracing();

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = bounded::<AudioPacket>(256);

    let mut workers = Vec::new();
    for kind in [AudioTrackKind::Mix, AudioTrackKind::Mic] {
        let tx = tx.clone();
        let stop = stop.clone();
        // The probe always follows the default endpoint (§7 selection is exercised
        // by the real record path): Mix = render loopback, Mic = capture endpoint.
        // Exhaustive so the compiler forces a decision if this probe ever grows to
        // spawn the per-app tracks — those must become `ProcessLoopback` (B2), not
        // the endpoint loopback this only-Mix+Mic loop maps them to defensively.
        let source = match kind {
            AudioTrackKind::Mic => AudioSource::MicEndpoint(DeviceSelection::DefaultFollow),
            AudioTrackKind::Mix
            | AudioTrackKind::Game
            | AudioTrackKind::VoiceChat
            | AudioTrackKind::OtherSystem => AudioSource::EndpointLoopback,
        };
        workers.push(std::thread::spawn(move || {
            // No live mic control in this audio-probe tool (T2b) — fixed selection.
            run_capture(kind, source, None, tx, stop)
        }));
    }
    drop(tx); // only the workers hold senders now → rx closes when they exit

    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(seconds));
            stop.store(true, Ordering::Relaxed);
        });
    }
    println!(
        "capturing desktop-loopback + mic for {seconds}s — PLAY audio then let it go SILENT \
         (loopback gap), speak into the mic; Ctrl+C aborts"
    );

    // Per-stream aggregation. Gap = QPC advance minus frames delivered (mirrors
    // spike #3): a jump beyond the §2.3 jitter bound is the loopback-silence hole
    // the resample/gap stage must fill.
    #[derive(Default)]
    struct Stat {
        packets: u64,
        frames: u64,
        silent: u64,
        rate: u32,
        prev: Option<(i64, u32)>,
        max_gap_ticks: i64,
    }
    let mut desktop = Stat::default();
    let mut mic = Stat::default();

    while let Ok(pkt) = rx.recv() {
        let s = match pkt.stream {
            AudioTrackKind::Mic => &mut mic,
            // This probe only spawns Mix + Mic; the per-source system tracks (B2) never
            // reach here, but count them on the desktop side to stay exhaustive.
            AudioTrackKind::Mix
            | AudioTrackKind::Game
            | AudioTrackKind::VoiceChat
            | AudioTrackKind::OtherSystem => &mut desktop,
        };
        s.packets += 1;
        s.frames += pkt.frames as u64;
        s.rate = pkt.sample_rate;
        if pkt.silent {
            s.silent += 1;
        }
        if let Some((pp, pf)) = s.prev {
            let expected = pp
                + (pf as i128 * TICKS_PER_SECOND as i128 / pkt.sample_rate.max(1) as i128) as i64;
            let gap = pkt.pts - expected;
            if gap > s.max_gap_ticks {
                s.max_gap_ticks = gap;
            }
        }
        s.prev = Some((pkt.pts, pkt.frames));
    }

    let mut ok = true;
    for w in workers {
        match w.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("audio worker error: {e}");
                ok = false;
            }
            Err(_) => {
                eprintln!("audio worker panicked");
                ok = false;
            }
        }
    }

    for (label, s) in [("desktop", &desktop), ("mic", &mic)] {
        let secs = s.frames as f64 / s.rate.max(1) as f64;
        println!(
            "{label}: {} packets, {} frames @ {} Hz (~{:.2}s), {} silent, max_gap {:.1} ms",
            s.packets,
            s.frames,
            s.rate,
            secs,
            s.silent,
            s.max_gap_ticks as f64 / 10_000.0
        );
        if s.packets == 0 {
            println!("  (no packets — silent endpoint / no mic? loopback needs audio playing)");
        }
    }

    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// Run `aac-probe`: encode a synthetic tone through the real
/// `encode::mft_aac` AAC-LC MFT and report the access-unit count, average
/// bitrate, and the extracted `AudioSpecificConfig`. Exercises Milestone-2 Task 4
/// on hardware (the AAC encoder + ASC extraction the muxer needs) without the
/// capture/resample stages. Expected ASC for 48 kHz stereo AAC-LC: `11 90`.
fn run_aac_probe(seconds: u64) -> ExitCode {
    use clipd::audio::wasapi_stream::AudioTrackKind;
    use clipd::encode::mft_aac::{f32_to_i16, AacEncoder};
    use clipd::spec_constants::audio::aac::{BITRATE_DEFAULT_BPS, FRAME_SAMPLES};
    use clipd::spec_constants::audio::SAMPLE_RATE_HZ;

    init_tracing();
    let _com = ComMta::initialize();
    let _mf = match MediaFoundation::startup() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: MFStartup failed: {e}");
            return ExitCode::from(2);
        }
    };

    let mut encoder = match AacEncoder::new(AudioTrackKind::Mix, BITRATE_DEFAULT_BPS) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: AAC encoder init failed: {e}");
            return ExitCode::from(2);
        }
    };
    let asc = encoder.audio_specific_config().to_vec();
    let asc_hex = asc
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "AAC-LC encoder ready @ {} kbps; AudioSpecificConfig = [{asc_hex}] (expect \"11 90\" for 48 kHz stereo LC)",
        BITRATE_DEFAULT_BPS / 1000
    );

    // Feed a 440 Hz stereo tone in 1024-frame blocks.
    let total_frames = seconds * SAMPLE_RATE_HZ as u64;
    let block = FRAME_SAMPLES as u64;
    let mut aus = 0u64;
    let mut bytes = 0u64;
    let mut pts = 0i64;
    let tick = clipd::spec_constants::units::TICKS_PER_SECOND;
    let mut i = 0u64;
    let emit =
        |pkts: Vec<clipd::encode::mft_aac::EncodedAudioPacket>, aus: &mut u64, bytes: &mut u64| {
            for p in pkts {
                *aus += 1;
                *bytes += p.data.len() as u64;
            }
        };
    while i < total_frames {
        let n = block.min(total_frames - i);
        let mut buf = Vec::with_capacity(n as usize * 2);
        for k in 0..n {
            let t = (i + k) as f32 / SAMPLE_RATE_HZ as f32;
            let v = (std::f32::consts::TAU * 440.0 * t).sin() * 0.25;
            buf.push(v);
            buf.push(v);
        }
        let pcm = f32_to_i16(&buf);
        match encoder.encode(&pcm, pts) {
            Ok(pkts) => emit(pkts, &mut aus, &mut bytes),
            Err(e) => {
                eprintln!("error: AAC encode failed: {e}");
                return ExitCode::from(2);
            }
        }
        pts += (n as i128 * tick as i128 / SAMPLE_RATE_HZ as i128) as i64;
        i += n;
    }
    match encoder.finish() {
        Ok(pkts) => emit(pkts, &mut aus, &mut bytes),
        Err(e) => {
            eprintln!("error: AAC drain failed: {e}");
            return ExitCode::from(2);
        }
    }

    let avg_kbps = (bytes * 8 / 1000).checked_div(seconds).unwrap_or(0);
    let expected_aus = total_frames / block;
    println!(
        "encoded {seconds}s tone: {aus} access units (~{expected_aus} expected), {bytes} bytes (~{avg_kbps} kbps avg)"
    );
    ExitCode::SUCCESS
}

/// Initialize console-only `tracing` for the short-lived `*-probe` diagnostics
/// (they should not spatter the rolling log directory). `RUST_LOG` controls the
/// filter; defaults to `info`. The long-running `buffer`/`record` paths instead
/// call [`clipd::logging::init_session`] to also write the rotating file log.
fn init_tracing() {
    clipd::logging::init_console();
}

/// Resolve `[output].dir` to a concrete, existing directory for the engine to write
/// clips into. Empty ⇒ the OS Videos default ([`resolve_output_dir`]). The directory
/// is created if missing (mirrors `logging.rs`, which creates its log dir); if that
/// fails (bad path / permission), we log and fall back to the Videos default so a
/// mistyped folder can never turn every save into a silent I/O failure — the incumbent
/// "why didn't my clip save?" trap. Returns whatever path ended up usable (the log
/// says which, and whether a fallback fired).
fn prepare_output_dir(cfg_dir: &str) -> PathBuf {
    let resolved = resolve_output_dir(cfg_dir);
    if let Err(e) = std::fs::create_dir_all(&resolved) {
        let fallback = default_output_dir();
        tracing::warn!(
            dir = %resolved.display(), error = %e, fallback = %fallback.display(),
            "output dir not creatable — falling back to the OS Videos folder"
        );
        // Best-effort create the fallback too; if even that fails the save path will
        // log the concrete I/O error per-clip (status strip shows "failed").
        if let Err(e) = std::fs::create_dir_all(&fallback) {
            tracing::error!(
                dir = %fallback.display(), error = %e,
                "fallback output dir not creatable either — saves may fail"
            );
        }
        return fallback;
    }
    resolved
}

/// Map the config capture target (`§3` pitfall 31) to the engine's
/// [`CaptureSource`]. Keeps the capture layer free of the config schema.
fn capture_source(target: &CaptureTarget) -> CaptureSource {
    match target {
        CaptureTarget::Named(NamedTarget::Primary) => CaptureSource::PrimaryMonitor,
        CaptureTarget::Named(NamedTarget::FocusedWindow) => CaptureSource::FocusedWindow,
        CaptureTarget::Monitor(index) => CaptureSource::Monitor(*index),
    }
}

/// A short human label for the capture target (for the startup banner).
fn target_label(target: &CaptureTarget) -> String {
    match target {
        CaptureTarget::Named(NamedTarget::Primary) => "primary monitor".to_string(),
        CaptureTarget::Named(NamedTarget::FocusedWindow) => "focused window".to_string(),
        CaptureTarget::Monitor(index) => format!("monitor {index}"),
    }
}

/// Run `record`: record the capture target straight to an MP4 for `--seconds N`
/// (or until Enter when omitted), `--out PATH` optional. Runs on the converged
/// ring+disk-sink path (`BufferEngine` with `record_autostart`); the M1/M2
/// `RecordingEngine` it replaced was retired (DECISIONS 2026-07-05).
fn run_record(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut seconds: Option<u64> = None;
    let mut out: Option<PathBuf> = None;
    let mut simulate: Option<u64> = None;
    let mut overrides = EncoderOverrides::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => seconds = args.next().and_then(|s| s.parse().ok()),
            "--out" => out = args.next().map(PathBuf::from),
            // T0 calibration-probe hooks (M7-M8-PLAN §1), hidden. Rate-control mode
            // + quality/QP/bitrate overrides for the unattended encoder sweep.
            "--encode-rc-mode" => {
                overrides.rc_mode = args.next().as_deref().and_then(rc_mode_from_str)
            }
            "--encode-quality" => overrides.quality = args.next().and_then(|s| s.parse().ok()),
            "--encode-qp" => overrides.qp = args.next().and_then(|s| s.parse().ok()),
            "--encode-avg-bitrate" => {
                overrides.avg_bitrate_bps = args.next().and_then(|s| s.parse().ok())
            }
            "--encode-max-bitrate" => {
                overrides.max_bitrate_bps = args.next().and_then(|s| s.parse().ok())
            }
            // Test hook: inject a synthetic device loss after N seconds. On the
            // converged ring+disk path a device loss STOPS the recording (v1
            // behavior; the buffer itself survives and rebuilds).
            "--simulate-device-loss" => simulate = args.next().and_then(|s| s.parse().ok()),
            other => {
                eprintln!("record: unrecognized argument '{other}'");
                return ExitCode::from(2);
            }
        }
    }

    // Session logging: console + rotating file (held for the whole record run).
    let _log_guard = clipd::logging::init_session();
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
    let idr_secs = if cfg.buffer.precise_mode {
        spec_constants::ring::PRECISE_MODE_IDR_INTERVAL_SECONDS
    } else {
        spec_constants::ring::IDR_INTERVAL_SECONDS
    };
    let gop = spec_constants::ring::gop_frames(idr_secs, fps);
    let base_out_dir = prepare_output_dir(&cfg.output.dir);

    // Audio track selection (`§2.5`): desktop per `[audio].desktop`, mic per
    // `[audio].mic` ("off" disables the mic track).
    let desktop_audio = cfg.audio.desktop;
    let mic_audio = cfg.audio.mic.trim() != "off";
    let mic_selection = clipd::audio::devices::DeviceSelection::for_mic(&cfg.audio.mic);
    let audio_bitrate_bps = cfg.audio.bitrate_bps;
    // Slice-B track topology (D1): `separate_tracks` off = Mix+Mic (default); on = the
    // full per-source set. The extra tracks are planned but not spawned until B2/B4.
    let separate_tracks = cfg.audio.separate_tracks;
    let track_game = cfg.audio.tracks.game;
    let track_voice_chat = cfg.audio.tracks.voice_chat;
    let track_other_system = cfg.audio.tracks.other_system;

    let gpu = match build_gpu(false) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: could not create the shared D3D11 device: {e}");
            return ExitCode::from(2);
        }
    };

    // The captured set B1 spawns is Mix (+ Mic); per-source system tracks are deferred
    // to B2/B4, so the banner names the mix/mic reality (a per-track log covers the rest).
    let audio_desc = match (desktop_audio, mic_audio) {
        (true, true) => "mix+mic",
        (true, false) => "mix",
        (false, true) => "mic",
        (false, false) => "none",
    };
    match seconds {
        Some(n) => println!(
            "recording {} @ {fps} fps (VBR) for {n}s; audio: {audio_desc}",
            target_label(&cfg.capture.target)
        ),
        None => println!(
            "recording {} @ {fps} fps (VBR) until Enter; audio: {audio_desc}",
            target_label(&cfg.capture.target)
        ),
    }
    match &out {
        Some(p) => println!("-> {}", p.display()),
        None => println!("-> {}\\{PRODUCT_NAME}_rec_*.mp4", base_out_dir.display()),
    }

    // `record` runs on the converged ring+disk-sink path (M1/M2 `RecordingEngine`
    // retired; DECISIONS 2026-07-05): a MINIMAL ring is held only so the recording
    // can tee off it — the ring itself is never read for the recorded file, so its
    // size is irrelevant to the recording and kept small to stay well inside the RAM
    // budget. `record_autostart` begins the recording at the first IDR; `--seconds N`
    // also auto-stops it (with the `§4`-clean tail-drain) after N, else it records
    // until Enter. `--out` overrides the default `<product>_rec_<ms>.mp4` name.
    const RECORD_RING_SECONDS: u32 = 2; // minimal ring; recording tees live off it
    const RECORD_EXIT_GRACE_SECS: u64 = 2; // let the tail-drain finalize before exit

    let params = BufferParams {
        capture_source: capture_source(&cfg.capture.target),
        adapter: AdapterSelection::Auto,
        max_encode_height: cfg.encode.effective_max_height(),
        fps,
        cursor,
        cq,
        quality_mult: cfg.encode.quality.multiplier(),
        gop_frames: gop,
        overrides,
        desktop_audio,
        mic_audio,
        mic_selection,
        separate_tracks,
        track_game,
        track_voice_chat,
        track_other_system,
        vc_apps: cfg.audio.vc_apps.clone(),
        audio_bitrate_bps,
        buffer_seconds: RECORD_RING_SECONDS,
        clear_after_save: cfg.buffer.clear_after_save,
        output_dir: base_out_dir,
        // No hotkeys in record mode — these ids never match a real hotkey event.
        save_hotkey_id: 0,
        record_hotkey_id: 0,
        autosave: None,
        record_auto: seconds.map(Duration::from_secs),
        record_out: out,
        record_autostart: true,
        simulate_loss_after: simulate,
    };

    // With `--seconds N` the ring drains + finalizes the recording at ~N; stop the
    // whole engine a short grace later so the tail-drain (≤ 500 ms, spec §4/M4-3)
    // completes first. Without `--seconds`, Enter stops it.
    let stop = Arc::new(AtomicBool::new(false));
    arm_stop(&stop, seconds.map(|n| n + RECORD_EXIT_GRACE_SECS));
    let engine = BufferEngine::start(gpu, params);

    let mut ticks = 0u32;
    while !stop.load(Ordering::Relaxed) && !engine.any_worker_finished() {
        std::thread::sleep(Duration::from_millis(100));
        ticks += 1;
        if ticks.is_multiple_of(10) {
            engine.stats().check_divergence();
        }
    }

    let (captured, encoded, muxed) = engine.stats().snapshot();
    match engine.stop_and_join() {
        Ok(()) => {
            println!("done: {captured} captured / {encoded} encoded / {muxed} muxed");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("record failed: {e}");
            ExitCode::from(1)
        }
    }
}

/// Run `buffer`: the Milestone-3 replay buffer. Captures continuously into the
/// in-memory ring; the configured save hotkey writes the last N seconds to a clean
/// fMP4. Runs until Enter (or until a worker exits, e.g. a device loss — buffer-mode
/// epoch restart is a follow-up). `--seconds N` overrides `[buffer].seconds`.
fn run_buffer(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut seconds_override: Option<u32> = None;
    let mut autosave: Option<u64> = None;
    let mut simulate: Option<u64> = None;
    let mut record_secs: Option<u64> = None;
    let mut reopen_settings = false;
    let mut overrides = EncoderOverrides::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => seconds_override = args.next().and_then(|s| s.parse().ok()),
            // Set by the auto-restart relaunch (T2): re-open the settings window on start.
            REOPEN_SETTINGS_FLAG => reopen_settings = true,
            // T0 calibration-probe hooks (M7-M8-PLAN §1), same as `record`.
            "--encode-rc-mode" => {
                overrides.rc_mode = args.next().as_deref().and_then(rc_mode_from_str)
            }
            "--encode-quality" => overrides.quality = args.next().and_then(|s| s.parse().ok()),
            "--encode-qp" => overrides.qp = args.next().and_then(|s| s.parse().ok()),
            "--encode-avg-bitrate" => {
                overrides.avg_bitrate_bps = args.next().and_then(|s| s.parse().ok())
            }
            "--encode-max-bitrate" => {
                overrides.max_bitrate_bps = args.next().and_then(|s| s.parse().ok())
            }
            // Hidden test hook: auto-start a timed recording at buffer start and stop it
            // after N seconds, so the recorded file can be `just verify`d unattended.
            "--record-secs" => record_secs = args.next().and_then(|s| s.parse().ok()),
            // Hidden acceptance-test hook: auto-fire a save every N seconds (the
            // 50-consecutive-saves + 24-hour-soak criteria run unattended). Not in
            // --help; exercises the same §4 save path as the hotkey.
            "--autosave" => autosave = args.next().and_then(|s| s.parse().ok()),
            // Hidden test hook: inject a synthetic device loss after N seconds to
            // exercise the buffer-mode epoch restart (§7) without an actual
            // sleep/resume. The ring must survive and a save after the restart must
            // still succeed.
            "--simulate-device-loss" => simulate = args.next().and_then(|s| s.parse().ok()),
            other => {
                eprintln!("buffer: unrecognized argument '{other}'");
                return ExitCode::from(2);
            }
        }
    }

    // Session logging: console + rotating file (held for the whole buffer run).
    let _log_guard = clipd::logging::init_session();
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
    let idr_secs = if cfg.buffer.precise_mode {
        spec_constants::ring::PRECISE_MODE_IDR_INTERVAL_SECONDS
    } else {
        spec_constants::ring::IDR_INTERVAL_SECONDS
    };
    let gop = spec_constants::ring::gop_frames(idr_secs, fps);
    let buffer_seconds = seconds_override
        .unwrap_or(cfg.buffer.seconds)
        .clamp(1, spec_constants::ring::MAX_BUFFER_SECONDS);

    let output_dir = prepare_output_dir(&cfg.output.dir);

    // Audio track selection (`§2.5`), same as `record`.
    let desktop_audio = cfg.audio.desktop;
    let mic_audio = cfg.audio.mic.trim() != "off";
    let mic_selection = clipd::audio::devices::DeviceSelection::for_mic(&cfg.audio.mic);
    let audio_bitrate_bps = cfg.audio.bitrate_bps;
    // Slice-B track topology (D1): `separate_tracks` off = Mix+Mic (default); on = the
    // full per-source set. The extra tracks are planned but not spawned until B2/B4.
    let separate_tracks = cfg.audio.separate_tracks;
    let track_game = cfg.audio.tracks.game;
    let track_voice_chat = cfg.audio.tracks.voice_chat;
    let track_other_system = cfg.audio.tracks.other_system;

    // Register the global save + record-toggle hotkeys (their own message-pump
    // thread). A parse or registration failure (e.g. a combo is taken) is fatal to
    // buffer mode. Index 0 = save_clip, 1 = record_toggle.
    let pump = match HotkeyPump::spawn(&[&cfg.hotkeys.save_clip, &cfg.hotkeys.record_toggle]) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let save_hotkey_id = pump.hotkey_id(0);
    let record_hotkey_id = pump.hotkey_id(1);
    // A control handle for the settings editor's live "combo already taken" check —
    // it asks the pump (which owns the `!Send` hotkey manager) to test-register a
    // freshly-bound combo. Cloneable and independent of `pump`'s lifetime.
    let hotkey_ctl = pump.control();

    let gpu = match build_gpu(false) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: could not create the shared D3D11 device: {e}");
            pump.request_quit();
            pump.join();
            return ExitCode::from(2);
        }
    };

    // The captured set B1 spawns is Mix (+ Mic); per-source system tracks are deferred
    // to B2/B4, so the banner names the mix/mic reality (a per-track log covers the rest).
    let audio_desc = match (desktop_audio, mic_audio) {
        (true, true) => "mix+mic",
        (true, false) => "mix",
        (false, true) => "mic",
        (false, false) => "none",
    };
    println!(
        "buffering {} @ {fps} fps (VBR); audio: {audio_desc}; \
         last {buffer_seconds}s retained; clips -> {}",
        target_label(&cfg.capture.target),
        output_dir.display()
    );
    println!(
        "press [{}] to save the last {buffer_seconds}s; [{}] to start/stop recording; \
         press Enter to quit",
        cfg.hotkeys.save_clip, cfg.hotkeys.record_toggle
    );

    // The tray shell (M5) owns the main thread in normal buffer mode. The hidden
    // acceptance hooks (`--autosave`/`--record-secs`/`--simulate-device-loss`) run
    // unattended, so they keep the headless Enter/timer loop and never pop a tray.
    let use_tray = autosave.is_none() && record_secs.is_none() && simulate.is_none();
    let shell_output_dir = output_dir.clone();

    let params = BufferParams {
        capture_source: capture_source(&cfg.capture.target),
        adapter: AdapterSelection::Auto,
        max_encode_height: cfg.encode.effective_max_height(),
        fps,
        cursor,
        cq,
        quality_mult: cfg.encode.quality.multiplier(),
        gop_frames: gop,
        overrides,
        desktop_audio,
        mic_audio,
        mic_selection,
        separate_tracks,
        track_game,
        track_voice_chat,
        track_other_system,
        vc_apps: cfg.audio.vc_apps.clone(),
        audio_bitrate_bps,
        buffer_seconds,
        clear_after_save: cfg.buffer.clear_after_save,
        output_dir,
        save_hotkey_id,
        record_hotkey_id,
        autosave: autosave.map(Duration::from_secs),
        record_auto: record_secs.map(Duration::from_secs),
        record_out: None, // buffer mode uses the default `<product>_rec_<ms>.mp4` name
        // `--record-secs` auto-starts a recording; normal buffer mode is hotkey-driven.
        record_autostart: record_secs.is_some(),
        simulate_loss_after: simulate,
    };
    if let Some(secs) = autosave {
        println!("(--autosave {secs}s: auto-firing a save every {secs}s for acceptance testing)");
    }
    if let Some(secs) = simulate {
        println!("(--simulate-device-loss {secs}s: injecting a synthetic device loss to test the §7 restart)");
    }
    if let Some(secs) = record_secs {
        println!("(--record-secs {secs}s: auto-recording {secs}s to disk for acceptance testing)");
    }

    let engine = BufferEngine::start(gpu, params);

    // Drive the session: the tray shell (Quit / a worker dying ends it), or the
    // headless loop for the unattended hooks. If the tray can't be created, fall
    // back to headless so the engine still runs (the satellite rule — the engine
    // must never depend on the UI).
    let outcome = if use_tray {
        match ui::Shell::new(
            engine.command_sender(),
            shell_output_dir,
            engine.audio_levels(),
            engine.audio_streams(),
            engine.status(),
            hotkey_ctl,
        ) {
            Ok(mut shell) => {
                // After an auto-restart, re-open the settings window so it doesn't vanish
                // (which reads as a crash) — T2.
                if reopen_settings {
                    shell.open_settings_on_start();
                }
                shell.run(&engine)
            }
            Err(e) => {
                eprintln!("warning: could not create the tray ({e}); running without it");
                run_headless_session(&engine);
                ui::ShellOutcome::Quit
            }
        }
    } else {
        run_headless_session(&engine);
        ui::ShellOutcome::Quit
    };

    let result = engine.stop_and_join();
    pump.request_quit();
    pump.join();

    // U7 auto-restart: relaunch a fresh instance ONLY now — after `stop_and_join`
    // (capture/audio devices released) and `pump.join()` (global hotkeys released) — so
    // the new process can grab the same hotkeys/devices without a registration-retry
    // hack. The spawn lives here (not in `ui`) so the satellite law holds: the UI only
    // signalled intent; `main` owns process lifecycle.
    if matches!(outcome, ui::ShellOutcome::Restart) {
        relaunch_self();
    }

    match result {
        Ok(()) => {
            println!("buffer stopped.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("buffer failed: {e}");
            ExitCode::from(1)
        }
    }
}

/// Relaunch this executable with the same argv (U7 auto-restart). Called ONLY after the
/// current process has released its global hotkeys (`pump.join()`) and capture/audio
/// devices (`stop_and_join`), so the fresh instance can re-register/re-open them without
/// a registration-retry hack. The child is spawned **detached** (`DETACHED_PROCESS |
/// CREATE_NEW_PROCESS_GROUP`) so it fully outlives the exiting parent.
/// Argv flag the auto-restart appends so the relaunched instance re-opens the settings
/// window (T2 — a vanished window after a restart reads as a crash).
const REOPEN_SETTINGS_FLAG: &str = "--reopen-settings";

fn relaunch_self() {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "restart: could not resolve the current exe; not relaunching");
            return;
        }
    };
    // Re-pass the same arguments (`buffer [--seconds N] …`). Only the tray path reaches a
    // restart, and it excludes the headless-only hooks (`--autosave`/`--record-secs`/
    // `--simulate-device-loss`), so the child comes back up in the same tray mode. Append
    // `--reopen-settings` (deduped) so the fresh instance re-opens the settings window —
    // a window that vanished after a restart reads as a crash (T2).
    let mut args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != REOPEN_SETTINGS_FLAG)
        .collect();
    args.push(REOPEN_SETTINGS_FLAG.to_string());
    match std::process::Command::new(&exe)
        .args(&args)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
    {
        Ok(_) => {
            println!("restarting clipd to apply settings…");
            tracing::info!(exe = %exe.display(), "relaunched clipd to apply restart-required settings");
        }
        Err(e) => tracing::warn!(error = %e, "restart: could not relaunch clipd"),
    }
}

/// The headless buffer session loop (no tray): quit on Enter, or when a worker
/// exits (device-loss rebuilds keep it running). Used for the unattended
/// acceptance hooks and as the fallback when the tray cannot be created.
fn run_headless_session(engine: &BufferEngine) {
    let stop = Arc::new(AtomicBool::new(false));
    arm_stop(&stop, None); // Enter to quit
    let mut ticks = 0u32;
    while !stop.load(Ordering::Relaxed) && !engine.any_worker_finished() {
        std::thread::sleep(Duration::from_millis(100));
        ticks += 1;
        if ticks.is_multiple_of(10) {
            engine.stats().check_divergence();
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
        Some("window-capture-probe") => {
            let seconds = args.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(5);
            run_window_capture_probe(seconds)
        }
        Some("window-events-probe") => {
            let seconds = args
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(30);
            run_window_events_probe(seconds)
        }
        Some("record") => run_record(args),
        Some("buffer") => run_buffer(args),
        Some("convert-probe") => run_convert_probe(),
        Some("audio-probe") => {
            let seconds = args.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(6);
            run_audio_probe(seconds)
        }
        Some("binding-probe") => {
            let seconds = args
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(30);
            run_binding_probe(seconds)
        }
        Some("list-audio-devices") => run_list_audio_devices(),
        Some("toast-test") => {
            clipd::ui::run_toast_diagnostic();
            ExitCode::SUCCESS
        }
        Some("aac-probe") => {
            let seconds = args.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(2);
            run_aac_probe(seconds)
        }
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
            print_usage();
            ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The config target → engine [`CaptureSource`] mapping is total and exact
    /// (pitfall 31: the target is chosen explicitly, never guessed). Config
    /// *parsing* of the string/int forms is covered in `config.rs`.
    #[test]
    fn capture_target_maps_to_source() {
        assert_eq!(
            capture_source(&CaptureTarget::Named(NamedTarget::Primary)),
            CaptureSource::PrimaryMonitor
        );
        assert_eq!(
            capture_source(&CaptureTarget::Named(NamedTarget::FocusedWindow)),
            CaptureSource::FocusedWindow
        );
        assert_eq!(
            capture_source(&CaptureTarget::Monitor(2)),
            CaptureSource::Monitor(2)
        );
    }

    #[test]
    fn target_label_is_human_readable() {
        assert_eq!(
            target_label(&CaptureTarget::Named(NamedTarget::Primary)),
            "primary monitor"
        );
        assert_eq!(
            target_label(&CaptureTarget::Named(NamedTarget::FocusedWindow)),
            "focused window"
        );
        assert_eq!(target_label(&CaptureTarget::Monitor(1)), "monitor 1");
    }
}
