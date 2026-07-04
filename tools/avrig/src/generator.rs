//! The flash/click generator (`02-AV-SYNC-SPEC.md §5`): a full-screen window that
//! flashes white while, at the same instant, a click is emitted through the
//! default render endpoint. `clipd` records the monitor (WGC) and the desktop
//! loopback simultaneously, so the offset between flash and click in the saved
//! clip IS the A/V sync error the rig measures.
//!
//! Two threads: this (UI) thread owns the window + the event schedule; a render
//! thread keeps the WASAPI render buffer fed with silence and injects the click
//! waveform when the UI thread flips `click_now`. Flash and click are therefore
//! emitted within one buffer period of each other — a small, ~constant offset the
//! §5 budget (AV-1) tolerates and the drift test (AV-2) cancels.
//!
//! ## `unsafe`
//! All the Win32 windowing/GDI calls are `unsafe` (this is a throwaway tool, not
//! the core binary, but SAFETY notes are kept). WASAPI COM is wrapped by the
//! `wasapi` crate.

use std::f64::consts::PI;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tracing::{info, warn};
use wasapi::{initialize_mta, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, UpdateWindow,
    PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetSystemMetrics, PeekMessageW, PostQuitMessage, RegisterClassW, ShowWindow, TranslateMessage,
    MSG, PM_REMOVE, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, WM_DESTROY, WM_KEYDOWN, WM_PAINT, WM_QUIT,
    WNDCLASSW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

/// Shared flash state read by the window procedure (which cannot capture state).
static FLASH_WHITE: AtomicBool = AtomicBool::new(false);
/// VK_ESCAPE — end the run early.
const VK_ESCAPE: usize = 0x1B;

/// Run the generator for `seconds`, flashing every `interval_ms` for `flash_ms`.
pub fn run(seconds: u64, interval_ms: u64, flash_ms: u64) -> Result<(), String> {
    let stop = Arc::new(AtomicBool::new(false));
    let click_now = Arc::new(AtomicBool::new(false));

    // Render thread: feed silence, inject a click when `click_now` is set.
    let render = {
        let stop = stop.clone();
        let click_now = click_now.clone();
        thread::spawn(move || {
            if let Err(e) = render_loop(&stop, &click_now) {
                warn!(error = %e, "render thread failed — clicks will be silent");
            }
        })
    };

    let result = ui_loop(seconds, interval_ms, flash_ms, &click_now);

    stop.store(true, Ordering::Relaxed);
    let _ = render.join();
    result
}

/// The UI thread: create the full-screen window and drive the flash schedule.
fn ui_loop(
    seconds: u64,
    interval_ms: u64,
    flash_ms: u64,
    click_now: &Arc<AtomicBool>,
) -> Result<(), String> {
    let hwnd = unsafe { create_fullscreen_window() }?;
    // SAFETY: the window was created on this thread; show it.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }

    let total = Duration::from_secs(seconds);
    let interval = Duration::from_millis(interval_ms);
    let flash = Duration::from_millis(flash_ms);
    let start = Instant::now();
    let mut next_event = interval;
    let mut flash_started: Option<Instant> = None;
    let mut event_index = 0u64;

    info!(
        seconds,
        interval_ms, flash_ms, "avrig flash started — press Esc to stop early"
    );

    while start.elapsed() < total {
        // Drain pending window messages without blocking.
        if pump_messages() {
            break; // WM_QUIT (Esc / close)
        }

        let elapsed = start.elapsed();
        // Fire a flash + click.
        if flash_started.is_none() && elapsed >= next_event {
            FLASH_WHITE.store(true, Ordering::Relaxed);
            click_now.store(true, Ordering::Relaxed);
            unsafe { repaint(hwnd) };
            flash_started = Some(Instant::now());
            next_event += interval;
            event_index += 1;
            info!(
                event = event_index,
                at_ms = elapsed.as_millis() as u64,
                "flash+click"
            );
        }
        // End the flash after `flash_ms`.
        if let Some(t) = flash_started {
            if t.elapsed() >= flash {
                FLASH_WHITE.store(false, Ordering::Relaxed);
                unsafe { repaint(hwnd) };
                flash_started = None;
            }
        }
        thread::sleep(Duration::from_millis(1));
    }

    // SAFETY: destroy our own window; ignore the result during teardown.
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    info!(events = event_index, "avrig flash finished");
    Ok(())
}

/// Force a synchronous repaint so the flash change is on-screen promptly.
unsafe fn repaint(hwnd: HWND) {
    let _ = InvalidateRect(Some(hwnd), None, true);
    let _ = UpdateWindow(hwnd);
}

/// Non-blocking message pump; returns `true` on `WM_QUIT`.
fn pump_messages() -> bool {
    let mut msg = MSG::default();
    // SAFETY: standard PeekMessage/Translate/Dispatch loop on our thread.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                return true;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    false
}

/// Register the class and create a topmost, borderless, full-primary-monitor
/// window painted black/white by [`wndproc`].
unsafe fn create_fullscreen_window() -> Result<HWND, String> {
    let hinstance = GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW: {e}"))?;
    let class_name = w!("avrig_flash_window");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance.into(),
        lpszClassName: class_name,
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        return Err("RegisterClassW failed".into());
    }
    let w = GetSystemMetrics(SM_CXSCREEN);
    let h = GetSystemMetrics(SM_CYSCREEN);
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST,
        class_name,
        w!("avrig"),
        WS_POPUP | WS_VISIBLE,
        0,
        0,
        w,
        h,
        None,
        None,
        Some(hinstance.into()),
        None,
    )
    .map_err(|e| format!("CreateWindowExW: {e}"))?;
    Ok(hwnd)
}

/// Window procedure: paint the whole client area black or white per
/// [`FLASH_WHITE`], and quit on Esc / destroy.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let color = if FLASH_WHITE.load(Ordering::Relaxed) {
                COLORREF(0x00FF_FFFF)
            } else {
                COLORREF(0x0000_0000)
            };
            let brush = CreateSolidBrush(color);
            FillRect(hdc, &rc, brush);
            let _ = DeleteObject(brush.into());
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_KEYDOWN if wp.0 == VK_ESCAPE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// The WASAPI render loop: keep the shared render buffer fed with silence, and
/// splice in the click waveform (once) each time `click_now` is set.
fn render_loop(stop: &AtomicBool, click_now: &AtomicBool) -> Result<(), String> {
    initialize_mta()
        .ok()
        .map_err(|e| format!("MTA init: {e}"))?;
    let enumerator = DeviceEnumerator::new().map_err(map)?;
    let device = enumerator
        .get_default_device(&Direction::Render)
        .map_err(map)?;
    let mut client = device.get_iaudioclient().map_err(map)?;
    let mix = client.get_mixformat().map_err(map)?;
    let rate = mix.get_samplespersec();
    // Render f32 stereo at the device rate; autoconvert handles the endpoint format.
    let format = WaveFormat::new(32, 32, &SampleType::Float, rate as usize, 2, None);
    let (def_period, _) = client.get_device_period().map_err(map)?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: def_period * 4,
    };
    client
        .initialize_client(&format, &Direction::Render, &mode)
        .map_err(map)?;
    let h_event = client.set_get_eventhandle().map_err(map)?;
    let render = client.get_audiorenderclient().map_err(map)?;

    let click = render_click(rate); // interleaved f32 stereo
    let click_frames = click.len() / 2;
    let mut click_pos: Option<usize> = None;

    client.start_stream().map_err(map)?;
    while !stop.load(Ordering::Relaxed) {
        if h_event.wait_for_event(200).is_err() {
            continue;
        }
        let avail = client.get_available_space_in_frames().map_err(map)? as usize;
        if avail == 0 {
            continue;
        }
        if click_pos.is_none() && click_now.swap(false, Ordering::Relaxed) {
            click_pos = Some(0);
        }
        let mut frames = vec![0f32; avail * 2];
        if let Some(pos) = click_pos {
            let take = (click_frames - pos).min(avail);
            frames[..take * 2].copy_from_slice(&click[pos * 2..(pos + take) * 2]);
            let new_pos = pos + take;
            click_pos = (new_pos < click_frames).then_some(new_pos);
        }
        // f32 → little-endian bytes.
        let mut bytes = Vec::with_capacity(frames.len() * 4);
        for s in &frames {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        render.write_to_device(avail, &bytes, None).map_err(map)?;
    }
    let _ = client.stop_stream();
    Ok(())
}

/// A 5 ms, 2 kHz click with a sharp onset and linear fade — loud enough to detect,
/// short enough to localise. Interleaved f32 stereo at `rate`.
fn render_click(rate: u32) -> Vec<f32> {
    let n = (rate as f64 * 0.005) as usize;
    let mut v = Vec::with_capacity(n * 2);
    for i in 0..n {
        let t = i as f64 / rate as f64;
        let env = 1.0 - (i as f64 / n as f64); // sharp onset, linear fade-out
        let s = ((2.0 * PI * 2000.0 * t).sin() * 0.9 * env) as f32;
        v.push(s);
        v.push(s);
    }
    v
}

/// Map a `wasapi` boxed error to a message.
fn map<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

// PCWSTR is used via the `w!` macro; keep the import referenced for clarity.
const _: PCWSTR = w!("");
