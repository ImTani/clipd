//! `ui::pill` — the save-confirmation overlay pill (P1c). A small, self-drawn, topmost,
//! click-through window that reads "Clip saved · N s" (accent) or "Clip NOT saved — …" (red)
//! on the active monitor for a few seconds after a save.
//!
//! ## Why a self-drawn window
//! Win11's gaming-DND suppresses the tray toast during play (DECISIONS 2026-07-09), and over
//! exclusive fullscreen no notification draws at all. A topmost layered window we own is the
//! only *visual* channel not subject to notification policy. This is **NOT an in-game
//! overlay**: no injection, no hooking, no DirectX present-hook — just a plain
//! `WS_EX_LAYERED` top-level window (consistent with `06-SAFETY-AND-VMS.md`). It therefore
//! also cannot draw over a true exclusive-fullscreen surface (a documented limitation); the
//! sound (P1b) + Action-Center toast + log remain the backstops there.
//!
//! ## Four sinks, one event
//! The pill consumes the SAME `ShellSignal::Saved` outcome as the toast, sound, and log, so
//! they can never disagree. It runs on **its own thread** with a per-thread
//! per-monitor-DPI-aware context (so it sizes in physical pixels without a process manifest),
//! owns the window + a message pump, and animates a fade in/hold/out. Latest-wins: a newer
//! save replaces the text instantly. No click handling in v1 (the window is click-through);
//! clicks stay on the toast. `unsafe` is confined here, each block with a `// SAFETY:` note.

use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::warn;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW, GdiFlush,
    GetDC, GetMonitorInfoW, MonitorFromWindow, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BITMAPINFO, BI_RGB, BLENDFUNCTION,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CALCRECT, DT_CENTER, DT_NOCLIP,
    DT_SINGLELINE, DT_VCENTER, HBITMAP, HDC, HFONT, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
    OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
    PeekMessageW, RegisterClassW, SetWindowPos, ShowWindow, TranslateMessage, UpdateLayeredWindow,
    HWND_TOPMOST, MSG, PM_REMOVE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE,
    ULW_ALPHA, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

use super::theme;

/// The window-class name (registered once per process).
const CLASS_NAME: PCWSTR = w!("clipd_pill_window");

/// Fade + hold timing (P1c: success ~3 s, failure ~6 s).
const FADE_IN: Duration = Duration::from_millis(140);
const FADE_OUT: Duration = Duration::from_millis(260);
const HOLD_SUCCESS: Duration = Duration::from_millis(3000);
const HOLD_FAILURE: Duration = Duration::from_millis(6000);
/// Animation frame cadence (~60 fps).
const FRAME: Duration = Duration::from_millis(16);

/// Pill geometry, in device-independent px (scaled by the monitor DPI at show time).
const PAD_X: f32 = 16.0;
const PAD_Y: f32 = 9.0;
const FONT_DIP: f32 = 15.0;
const MARGIN_DIP: f32 = 28.0; // inset from the monitor work-area corner
const MAX_TEXT_DIP: f32 = 460.0; // clamp very long failure reasons

/// A save outcome to display (the pill's content). Pure data.
#[derive(Debug, Clone)]
struct PillContent {
    ok: bool,
    text: String,
}

/// A command to the pill thread.
enum PillCommand {
    Show(PillContent),
    Quit,
}

/// Handle to the pill thread, owned by the tray [`super::Shell`]. Cheap to construct; all
/// Win32 work happens on the spawned thread.
pub struct PillHandle {
    tx: Option<Sender<PillCommand>>,
    thread: Option<JoinHandle<()>>,
}

impl PillHandle {
    /// Spawn the pill thread + create its (hidden) overlay window. Returns a handle even if
    /// the thread can't spawn (then `show` is a silent no-op) — the pill is a convenience,
    /// never load-bearing.
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("save-pill".to_string())
            .spawn(move || run(rx))
            .ok();
        if thread.is_none() {
            warn!("could not spawn the save-pill thread; overlay pill disabled this session");
        }
        Self {
            tx: thread.is_some().then_some(tx),
            thread,
        }
    }

    /// Show the pill for a save outcome (both success and failure). `recording` words a
    /// finalized recording differently (F2). Latest-wins on the thread. Non-blocking.
    pub fn show(&self, ok: bool, seconds: f32, reason: &str, recording: bool) {
        if let Some(tx) = &self.tx {
            let content = PillContent {
                ok,
                text: pill_text(ok, seconds, reason, recording),
            };
            let _ = tx.send(PillCommand::Show(content));
        }
    }

    /// Ask the pill thread to tear down its window and exit; join within a bound so process
    /// quit is never stalled. Idempotent.
    pub fn shutdown(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(PillCommand::Quit);
        }
        if let Some(thread) = self.thread.take() {
            // The thread wakes from `recv` immediately on the Quit above (or on the channel
            // dropping) and tears down; a small grace join keeps teardown tidy.
            let _ = thread.join();
        }
    }
}

impl Drop for PillHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The pill text for an outcome (pure, so it is unit-tested). Mirrors the toast/log wording,
/// incl. the clip-vs-recording distinction (F2). A long failure reason is truncated so the
/// single-line pill can't grow off the monitor.
fn pill_text(ok: bool, seconds: f32, reason: &str, recording: bool) -> String {
    let noun = if recording { "Recording" } else { "Clip" };
    if ok {
        let len = if recording && seconds >= 60.0 {
            format!("{:.0} min", seconds / 60.0)
        } else {
            format!("{seconds:.0} s")
        };
        format!("{noun} saved · {len}")
    } else {
        format!("{noun} NOT saved — {}", truncate_reason(reason))
    }
}

/// Truncate a failure reason to a bounded, character-safe length (…-elided). Pure.
fn truncate_reason(reason: &str) -> String {
    const MAX_CHARS: usize = 72;
    if reason.chars().count() <= MAX_CHARS {
        return reason.to_string();
    }
    let kept: String = reason.chars().take(MAX_CHARS - 1).collect();
    format!("{kept}…")
}

/// The hold duration for an outcome (failure lingers longer so the reason can be read).
fn hold_for(ok: bool) -> Duration {
    if ok {
        HOLD_SUCCESS
    } else {
        HOLD_FAILURE
    }
}

/// The pill thread: own the overlay window + message pump, block on the command channel, and
/// animate each requested pill to completion (honoring newer commands mid-animation).
fn run(rx: Receiver<PillCommand>) {
    // SAFETY: per-thread DPI awareness so monitor rects + our sizes are physical pixels; this
    // affects only this thread (the tray/eframe threads keep their own awareness). The window
    // + all GDI objects are created and destroyed on this thread; every FFI call is checked.
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let Some(hwnd) = (unsafe { create_window() }) else {
        warn!("could not create the pill overlay window; pill disabled this session");
        // Drain until the handle drops so senders don't error noisily.
        while let Ok(cmd) = rx.recv() {
            if matches!(cmd, PillCommand::Quit) {
                break;
            }
        }
        return;
    };

    // Block for the next Show; a `Quit` (or the channel dropping) ends the while-let and
    // tears down. `animate` returns `true` if a Quit arrived mid-animation.
    while let Ok(PillCommand::Show(content)) = rx.recv() {
        if animate(hwnd, &rx, content) {
            break;
        }
    }

    // SAFETY: destroying our own window on the thread that created it.
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// Animate one pill: fade in → hold → fade out, then hide. Returns `true` if a `Quit` was
/// received (caller should exit). A newer `Show` replaces the content at full opacity and
/// restarts the hold (latest-wins) without a blink.
fn animate(hwnd: HWND, rx: &Receiver<PillCommand>, mut content: PillContent) -> bool {
    let mut canvas = match PillCanvas::render(&content) {
        Some(c) => c,
        None => return false, // couldn't build the bitmap; skip silently
    };
    // Position on the active monitor + assert topmost ONCE per show. We do NOT poll topmost
    // every frame: if a persistent game/Discord overlay is topmost above us it wins, and a
    // z-order polling war is out of scope (the sound + toast + log remain) — see DECISIONS.
    unsafe {
        place_and_show(hwnd, &canvas);
    }

    let mut hold = hold_for(content.ok);
    let start = Instant::now();
    let mut phase_start = start;
    loop {
        // Drain any pending commands; keep only the newest Show (latest-wins).
        loop {
            match rx.try_recv() {
                Ok(PillCommand::Show(new)) => {
                    content = new;
                    if let Some(c) = PillCanvas::render(&content) {
                        canvas = c;
                    }
                    hold = hold_for(content.ok);
                    phase_start = Instant::now()
                        .checked_sub(FADE_IN)
                        .unwrap_or_else(Instant::now);
                    // Re-place: the active monitor may have changed since the last show.
                    unsafe { place_and_show(hwnd, &canvas) };
                }
                Ok(PillCommand::Quit) => {
                    unsafe { hide(hwnd) };
                    return true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    unsafe { hide(hwnd) };
                    return true;
                }
            }
        }

        let elapsed = phase_start.elapsed();
        let total = FADE_IN + hold + FADE_OUT;
        if elapsed >= total {
            unsafe { hide(hwnd) };
            return false;
        }
        let alpha = alpha_at(elapsed, hold);
        unsafe { canvas.update(hwnd, alpha) };

        // Pump our window's messages so the shell doesn't consider it unresponsive.
        unsafe { pump(hwnd) };
        std::thread::sleep(FRAME);
    }
}

/// The fade envelope: 0→255 over [`FADE_IN`], flat 255 for `hold`, 255→0 over [`FADE_OUT`].
fn alpha_at(elapsed: Duration, hold: Duration) -> u8 {
    if elapsed < FADE_IN {
        (255.0 * (elapsed.as_secs_f32() / FADE_IN.as_secs_f32())).round() as u8
    } else if elapsed < FADE_IN + hold {
        255
    } else {
        let t = (elapsed - FADE_IN - hold).as_secs_f32() / FADE_OUT.as_secs_f32();
        (255.0 * (1.0 - t).clamp(0.0, 1.0)).round() as u8
    }
}

/// A rendered pill bitmap on a memory DC, ready for `UpdateLayeredWindow`. Owns its GDI
/// objects; `Drop` frees them. Rebuilt whenever the content changes.
struct PillCanvas {
    memdc: HDC,
    bitmap: HBITMAP,
    old_bitmap: HGDIOBJ,
    width: i32,
    height: i32,
    x: i32,
    y: i32,
}

impl PillCanvas {
    /// Rasterize `content` into a premultiplied-BGRA DIB on the active monitor (sized for its
    /// DPI), returning the canvas or `None` on any GDI failure.
    fn render(content: &PillContent) -> Option<Self> {
        // SAFETY: standard GDI: measure text, create a top-down 32bpp DIB, fill the rounded-
        // rect background (direct writes into the DIB bits we own), draw the text, and hold
        // the DC/bitmap for `update`. Every handle is checked; `Drop` frees them.
        unsafe { render_canvas(content) }
    }

    /// Push the current bitmap to the window at global opacity `alpha` (0..=255) via
    /// `UpdateLayeredWindow` — per-pixel alpha times the fade.
    ///
    /// # Safety
    /// `hwnd` is the pill's layered window; `self` holds a live source DC + bitmap.
    unsafe fn update(&self, hwnd: HWND, alpha: u8) {
        let src = POINT { x: 0, y: 0 };
        let dst = POINT {
            x: self.x,
            y: self.y,
        };
        let size = SIZE {
            cx: self.width,
            cy: self.height,
        };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            None,
            Some(&dst),
            Some(&size),
            Some(self.memdc),
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
    }
}

impl Drop for PillCanvas {
    fn drop(&mut self) {
        // SAFETY: restore + free the GDI objects we created in `render_canvas`.
        unsafe {
            SelectObject(self.memdc, self.old_bitmap);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.memdc);
        }
    }
}

/// Rasterize the pill for `content`. See [`PillCanvas::render`] for the safety contract.
unsafe fn render_canvas(content: &PillContent) -> Option<PillCanvas> {
    // Active-monitor geometry + DPI (physical pixels, since the thread is per-monitor aware).
    let (work, scale) = active_monitor();
    let px = |dip: f32| (dip * scale).round() as i32;

    // A font at the DPI-scaled size (negative height = character cell height).
    let font: HFONT = CreateFontW(
        -px(FONT_DIP),
        0,
        0,
        0,
        600, // semibold
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        ANTIALIASED_QUALITY,
        0,
        w!("Segoe UI"),
    );
    if font.is_invalid() {
        return None;
    }

    let mut text: Vec<u16> = content.text.encode_utf16().collect();

    // Measure the text on a screen DC with the font selected.
    let screen = GetDC(None);
    let measure_dc = CreateCompatibleDC(Some(screen));
    let old_font_m = SelectObject(measure_dc, HGDIOBJ(font.0));
    let mut tr = RECT {
        left: 0,
        top: 0,
        right: px(MAX_TEXT_DIP),
        bottom: 0,
    };
    // DT_CALCRECT fills `tr` with the drawn extent.
    DrawTextW(
        measure_dc,
        &mut text,
        &mut tr,
        DT_SINGLELINE | DT_NOCLIP | DT_CALCRECT,
    );
    SelectObject(measure_dc, old_font_m);
    let _ = DeleteDC(measure_dc);
    ReleaseDC(None, screen);

    let text_w = (tr.right - tr.left).max(1);
    let text_h = (tr.bottom - tr.top).max(1);
    let width = text_w + 2 * px(PAD_X);
    let height = (text_h + 2 * px(PAD_Y)).max(px(FONT_DIP) + 2 * px(PAD_Y));
    let radius = (height as f32) * 0.5; // fully rounded ends → a "pill"

    // Create the top-down 32bpp DIB.
    let bmi = BITMAPINFO {
        bmiHeader: windows::Win32::Graphics::Gdi::BITMAPINFOHEADER {
            biSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height, // negative → top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let bitmap = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteObject(HGDIOBJ(font.0));
        return None;
    }

    // Fill the rounded-rect background (premultiplied BGRA) directly into the DIB bits.
    let [br, bg_, bb] = pill_bg(content.ok);
    let pixels = std::slice::from_raw_parts_mut(bits as *mut u8, (width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let cov = rounded_rect_coverage(x as f32 + 0.5, y as f32 + 0.5, width, height, radius);
            let o = ((y * width + x) * 4) as usize;
            // Premultiplied by coverage; DIB memory order is B, G, R, A.
            pixels[o] = (bb as f32 * cov) as u8;
            pixels[o + 1] = (bg_ as f32 * cov) as u8;
            pixels[o + 2] = (br as f32 * cov) as u8;
            pixels[o + 3] = (255.0 * cov) as u8;
        }
    }

    // Draw the text over the (opaque, alpha-255) pill interior; GDI leaves alpha untouched.
    let memdc = CreateCompatibleDC(None);
    if memdc.is_invalid() {
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteObject(HGDIOBJ(font.0));
        return None;
    }
    let old_bitmap = SelectObject(memdc, HGDIOBJ(bitmap.0));
    let old_font = SelectObject(memdc, HGDIOBJ(font.0));
    SetBkMode(memdc, TRANSPARENT);
    let [tr_, tg, tb] = PILL_TEXT;
    SetTextColor(memdc, COLORREF(rgb(tr_, tg, tb)));
    let mut draw_rect = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    DrawTextW(
        memdc,
        &mut text,
        &mut draw_rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOCLIP,
    );
    let _ = GdiFlush();
    // The font was selected only for drawing; restore + free it (the bitmap stays selected
    // for `update` and is freed in `Drop`).
    SelectObject(memdc, old_font);
    let _ = DeleteObject(HGDIOBJ(font.0));

    // Bottom-right corner of the work area, inset by the margin.
    let margin = px(MARGIN_DIP);
    let x = work.right - width - margin;
    let y = work.bottom - height - margin;

    Some(PillCanvas {
        memdc,
        bitmap,
        old_bitmap,
        width,
        height,
        x,
        y,
    })
}

/// The active monitor's work rect + effective DPI scale, from the foreground window (the game
/// during play → the monitor you're on). Falls back to the primary monitor / 1.0 scale.
///
/// # Safety
/// Calls raw Win32 monitor APIs; all outputs are checked/defaulted.
unsafe fn active_monitor() -> (RECT, f32) {
    let hmon = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let work = if GetMonitorInfoW(hmon, &mut mi).as_bool() {
        mi.rcWork
    } else {
        RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        }
    };
    let (mut dpix, mut dpiy) = (96u32, 96u32);
    let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpix, &mut dpiy);
    (work, (dpix as f32 / 96.0).max(1.0))
}

/// Position the window at the canvas corner, assert topmost, and show it without stealing
/// focus. Called once per show (+ on a monitor change) — deliberately not per frame.
///
/// # Safety
/// `hwnd` is the pill's layered window.
unsafe fn place_and_show(hwnd: HWND, canvas: &PillCanvas) {
    // Push content first so the very first frame isn't a blank rectangle.
    canvas.update(hwnd, 0);
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
}

/// Hide the window.
///
/// # Safety
/// `hwnd` is the pill's layered window.
unsafe fn hide(hwnd: HWND) {
    let _ = ShowWindow(hwnd, SW_HIDE);
}

/// Drain this thread's message queue (non-blocking).
///
/// # Safety
/// Standard `PeekMessageW`/`DispatchMessageW` loop on the thread that owns `hwnd`.
unsafe fn pump(_hwnd: HWND) {
    let mut msg = MSG::default();
    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// Register the class + create the hidden click-through layered overlay window.
///
/// # Safety
/// Calls raw Win32; returns `None` on failure so the caller degrades.
unsafe fn create_window() -> Option<HWND> {
    let hinstance = GetModuleHandleW(None).ok()?;
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance.into(),
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    RegisterClassW(&wc);
    // WS_EX_LAYERED (per-pixel alpha) | TRANSPARENT (click-through) | NOACTIVATE (never
    // steals focus) | TOOLWINDOW (no taskbar) | TOPMOST. WS_POPUP, created hidden.
    CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        CLASS_NAME,
        w!("clipd"),
        WS_POPUP,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(hinstance.into()),
        None,
    )
    .ok()
}

/// Minimal window procedure — the pill takes no input (it is click-through), so everything
/// falls through to the default handler.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: the documented default-handler contract for a WNDPROC.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Pill background colour (opaque RGB) for an outcome: a deep accent for success, a deep red
/// for failure — both carry the near-white [`PILL_TEXT`] legibly.
fn pill_bg(ok: bool) -> [u8; 3] {
    if ok {
        let [r, g, b, _] = theme::ACCENT_FILL.to_array();
        [r, g, b]
    } else {
        [0x7A, 0x2A, 0x24] // deep red (a darkened `theme::BAD`)
    }
}

/// Near-white pill text (from the palette's on-fill ink).
const PILL_TEXT: [u8; 3] = [0xF5, 0xF3, 0xFF];

/// Pack an (r,g,b) into a GDI `COLORREF` (`0x00BBGGRR`).
fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// Anti-aliased coverage (0..=1) of pixel centre `(px, py)` inside a `w×h` rounded rect with
/// corner radius `r`, via the rounded-rect signed distance. Pure — unit-tested.
fn rounded_rect_coverage(px: f32, py: f32, w: i32, h: i32, r: f32) -> f32 {
    let hw = w as f32 * 0.5;
    let hh = h as f32 * 0.5;
    let r = r.min(hw).min(hh);
    // Distance from centre, folded into the first quadrant, measured against the inner box.
    let qx = (px - hw).abs() - (hw - r);
    let qy = (py - hh).abs() - (hh - r);
    let dx = qx.max(0.0);
    let dy = qy.max(0.0);
    let outside = (dx * dx + dy * dy).sqrt();
    // Inside distance (negative) when both q are within the inner box.
    let inside = qx.max(qy).min(0.0);
    let dist = outside + inside - r;
    // 1px anti-aliased edge: coverage = clamp(0.5 - dist, 0, 1).
    (0.5 - dist).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pill_text_mirrors_the_toast_wording() {
        assert_eq!(pill_text(true, 30.4, "", false), "Clip saved · 30 s");
        let fail = pill_text(false, 0.0, "disk full", false);
        assert!(fail.contains("NOT saved"), "{fail}");
        assert!(fail.contains("disk full"), "{fail}");
        // A finalized recording is worded distinctly (F2).
        assert_eq!(pill_text(true, 132.0, "", true), "Recording saved · 2 min");
        assert!(pill_text(false, 0.0, "disk full", true).starts_with("Recording NOT saved"));
    }

    #[test]
    fn hold_is_longer_for_failures() {
        assert!(hold_for(false) > hold_for(true));
    }

    #[test]
    fn alpha_envelope_fades_in_holds_and_out() {
        let hold = Duration::from_millis(3000);
        assert_eq!(alpha_at(Duration::ZERO, hold), 0); // start of fade-in
        assert_eq!(alpha_at(FADE_IN, hold), 255); // full at end of fade-in
        assert_eq!(alpha_at(FADE_IN + Duration::from_millis(1500), hold), 255); // hold
        assert_eq!(alpha_at(FADE_IN + hold + FADE_OUT, hold), 0); // fully out
                                                                  // Mid fade-out is partial.
        let mid = alpha_at(FADE_IN + hold + FADE_OUT / 2, hold);
        assert!(mid > 0 && mid < 255, "mid fade-out alpha = {mid}");
    }

    #[test]
    fn coverage_is_one_in_the_centre_and_zero_outside_the_corner() {
        // Centre of a 100×40 pill (radius 20) is fully covered.
        assert!((rounded_rect_coverage(50.0, 20.0, 100, 40, 20.0) - 1.0).abs() < 1e-3);
        // The extreme corner pixel is outside the rounded arc → uncovered.
        assert!(rounded_rect_coverage(0.5, 0.5, 100, 40, 20.0) < 0.5);
        // A point well inside the left cap is covered.
        assert!(rounded_rect_coverage(21.0, 20.0, 100, 40, 20.0) > 0.9);
    }
}
