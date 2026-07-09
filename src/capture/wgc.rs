//! `capture::wgc` — Windows Graphics Capture of a monitor into a latest-frame
//! cell.
//!
//! Cannibalized from Milestone-0 spike #2. Creates a `GraphicsCaptureItem` for
//! the primary monitor, runs a **free-threaded** frame pool, and on each
//! delivered frame stores the backing `ID3D11Texture2D` (via the WinRT surface)
//! plus its `SystemRelativeTime` in a shared cell. The pixels stay on the GPU
//! (`CLAUDE.md` rule 6) — we hand the pacing grid the texture, never mapped
//! bytes.
//!
//! ## Threading
//! `FrameArrived` fires on a WinRT thread-pool thread; the pacing grid consumes
//! the cell from the capture thread. The cell is an `Arc<Mutex<Option<..>>>`;
//! [`CapturedFrame`] carries a `SAFETY`-justified `unsafe impl Send` because the
//! whole engine is MTA (see [`crate::com`]).
//!
//! ## Keep-latest
//! Storing a new frame drops (and thus `Close`s) any prior unconsumed frame, so
//! a source faster than the capture consumer never queues stale frames — the
//! `02-AV-SYNC-SPEC.md §1.4` "keep the latest, release the rest before
//! conversion" rule. The pool is sized for one frame held in the cell, one
//! in-flight in the consumer, and one the pool is composing into.
//!
//! ## Apartment
//! The calling thread must already be in the MTA ([`crate::com::ComMta`]); WGC
//! and the WinRT interop bridges require an initialized apartment.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tracing::warn;
use windows::core::BOOL;
use windows::core::{IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D, D3D11_TEXTURE2D_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HDC, HMONITOR,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsWindow};

use crate::gpu::GpuContext;

/// What to capture. An engine-facing, config-agnostic descriptor (`main.rs` maps
/// `config::CaptureTarget` onto it) so the capture layer never depends on the
/// config schema. `01-PROJECT-PLAN.md §3` pitfall 31: the target is chosen
/// explicitly, never guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSource {
    /// The Windows primary monitor.
    PrimaryMonitor,
    /// A specific monitor by zero-based OS-enumeration index
    /// ([`EnumDisplayMonitors`] order).
    Monitor(u32),
    /// The window focused when capture starts (borderless/windowed). Resolved once
    /// at start via [`GetForegroundWindow`]; a true exclusive-fullscreen title or an
    /// unresolvable foreground falls back to the primary monitor (pitfall 8).
    FocusedWindow,
    /// A specific already-resolved window, by `HWND` value (as `isize`). Not a config
    /// target — the engine uses this to **re-target the same window** across an M4-2
    /// resize epoch restart (`FocusedWindow` would re-resolve to whatever is focused
    /// then). If the window is gone by rebuild time, capture falls back to the
    /// primary monitor.
    Window(isize),
}

/// Pool surfaces: one held in the cell, one in-flight in the consumer, one the
/// pool composes into. Extra headroom over the spike's 2 to avoid dropped
/// deliveries while the consumer holds a frame during conversion.
const FRAME_POOL_BUFFERS: i32 = 3;

/// Errors from setting up or running capture.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// `GraphicsCaptureSession::IsSupported()` returned false.
    #[error("Windows Graphics Capture is not supported on this system")]
    Unsupported,
    /// `MonitorFromPoint` found no primary monitor.
    #[error("no primary monitor found")]
    NoPrimaryMonitor,
    /// A WGC / Direct3D call failed.
    #[error("WGC/D3D call failed: {0}")]
    Windows(#[from] windows::core::Error),
}

/// One captured frame: the WinRT frame (owning its pool surface) plus the
/// master-clock arrival stamp. Dropping returns the surface to the pool.
pub struct CapturedFrame {
    frame: Direct3D11CaptureFrame,
    /// WGC `SystemRelativeTime` in 100 ns ticks — the master-clock arrival stamp
    /// (`02-AV-SYNC-SPEC.md §1.1`). Used verbatim; never restamped with the
    /// callback's own arrival time.
    pub system_relative_time: i64,
}

// SAFETY: `Direct3D11CaptureFrame` and its backing D3D11 texture come from the
// WGC free-threaded frame pool and are safe to touch from any thread in the
// multithreaded apartment the whole engine runs in (see `crate::com`). A
// `CapturedFrame` is only moved between threads by ownership transfer through
// the latest-frame cell's `Mutex`; it is never aliased mutably across threads.
unsafe impl Send for CapturedFrame {}

impl CapturedFrame {
    /// The backing `ID3D11Texture2D` (BGRA8). Pixels stay on the GPU — the
    /// resource is returned, never mapped to system RAM.
    pub fn texture(&self) -> Result<ID3D11Texture2D, CaptureError> {
        // SAFETY: reach the backing texture via the DXGI interface-access bridge
        // on the WinRT surface; descriptor/resource only.
        unsafe {
            let surface = self.frame.Surface()?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
            Ok(access.GetInterface()?)
        }
    }

    /// The frame's `ContentSize` (the source's current logical size) in pixels.
    /// On a window **resize** this changes while the pool textures keep their old
    /// size until `Recreate`d — the M4-2 signal that the epoch must restart at the
    /// new resolution (`§0`/pitfall 11). Cheap (no pixels touched).
    pub fn content_size(&self) -> Result<(u32, u32), CaptureError> {
        let s = self.frame.ContentSize()?;
        Ok((s.Width.max(0) as u32, s.Height.max(0) as u32))
    }

    /// The backing texture's `(DXGI_FORMAT, width, height)` — descriptor only, no
    /// pixels touched. Used by the `capture-probe` diagnostic.
    pub fn descriptor(&self) -> Result<(u32, u32, u32), CaptureError> {
        let texture = self.texture()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `GetDesc` fills a caller-owned struct; it reads no pixel data.
        unsafe { texture.GetDesc(&mut desc) };
        Ok((desc.Format.0 as u32, desc.Width, desc.Height))
    }
}

impl Drop for CapturedFrame {
    fn drop(&mut self) {
        // Return the surface to the pool; ignore errors during teardown.
        let _ = self.frame.Close();
    }
}

/// A running WGC capture of the primary monitor. Lives on the capture thread;
/// dropping it removes the handler and closes the session and pool.
pub struct WgcCapture {
    /// The capture item — retained so its `Closed` handler stays registered (a
    /// window closing fires it; the M4-2 close→monitor-fallback signal).
    item: GraphicsCaptureItem,
    /// The WinRT device, retained to `Recreate` the pool on a window resize (M4-2:
    /// repool at the new content size, then rescale into the fixed canvas).
    winrt_device: IDirect3DDevice,
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    /// `FrameArrived` registration token (a bare `i64` in `windows` 0.62).
    token: i64,
    /// `Closed` registration token on the item.
    closed_token: i64,
    latest: Arc<Mutex<Option<CapturedFrame>>>,
    frames_delivered: Arc<AtomicU64>,
    /// Set true when the item's `Closed` event fires. NB: observed NOT to fire on
    /// window close on Win11 (M4-2 probe) — the engine polls [`is_window`] for that;
    /// this remains a best-effort signal (e.g. monitor removal).
    closed: Arc<AtomicBool>,
    /// The captured window's `HWND` (as `isize`) when the target is a window; `None`
    /// for a monitor. Lets the engine poll close and re-target on resize.
    hwnd: Option<isize>,
    size: SizeInt32,
}

impl WgcCapture {
    /// Start capturing `source` with the shared device. `capture_cursor` maps to
    /// `config.capture.cursor`. The single entry point the engine uses; window and
    /// monitor-index sources resolve to a `GraphicsCaptureItem` and share
    /// [`Self::start_for_item`].
    ///
    /// [`CaptureSource::FocusedWindow`] resolves the foreground window **once**, here
    /// (whatever is focused when capture starts — from a terminal that is the
    /// terminal window; the M5 tray makes this ergonomic). If there is no foreground
    /// window, or the window is not capturable (an exclusive-fullscreen title —
    /// pitfall 8), it falls back to the primary monitor and logs, so a replay buffer
    /// never silently dies on a stubborn game.
    pub fn start(
        gpu: &GpuContext,
        source: CaptureSource,
        capture_cursor: bool,
    ) -> Result<Self, CaptureError> {
        if !GraphicsCaptureSession::IsSupported()? {
            return Err(CaptureError::Unsupported);
        }

        // Resolve the source to a capture item, tracking the window HWND when the
        // target is a window (so the engine can poll `IsWindow` for close and
        // re-target the same window on a resize restart). Any window failure falls
        // back to the primary monitor (pitfall 8), which has no HWND.
        let (item, hwnd): (GraphicsCaptureItem, Option<isize>) = match source {
            CaptureSource::PrimaryMonitor => (create_item_for_monitor(primary_monitor()?)?, None),
            CaptureSource::Monitor(index) => match monitor_handle_by_index(index) {
                Some(hmon) => (create_item_for_monitor(hmon)?, None),
                None => {
                    warn!(index, "monitor index out of range; falling back to primary");
                    (create_item_for_monitor(primary_monitor()?)?, None)
                }
            },
            CaptureSource::FocusedWindow => match foreground_window() {
                Some(hwnd) => match create_item_for_window(hwnd) {
                    Ok(item) => (item, Some(hwnd.0 as isize)),
                    Err(e) => {
                        warn!(error = %e, "window capture failed; falling back to primary monitor");
                        (create_item_for_monitor(primary_monitor()?)?, None)
                    }
                },
                None => {
                    warn!("no foreground window; falling back to primary monitor (pitfall 8)");
                    (create_item_for_monitor(primary_monitor()?)?, None)
                }
            },
            CaptureSource::Window(h) => {
                let hwnd = HWND(h as *mut core::ffi::c_void);
                match create_item_for_window(hwnd) {
                    Ok(item) => (item, Some(h)),
                    Err(e) => {
                        warn!(error = %e, "resolved window gone; falling back to primary monitor");
                        (create_item_for_monitor(primary_monitor()?)?, None)
                    }
                }
            }
        };

        Self::start_for_item(gpu, item, hwnd, capture_cursor)
    }

    /// Start capturing the primary monitor. Retained thin wrapper for the
    /// diagnostic probes (`capture-probe`, `convert-probe`, `encode-probe`).
    pub fn start_primary(gpu: &GpuContext, capture_cursor: bool) -> Result<Self, CaptureError> {
        Self::start(gpu, CaptureSource::PrimaryMonitor, capture_cursor)
    }

    /// Build the pool + free-threaded handler + session for an already-resolved
    /// capture item and start it. Shared by every [`CaptureSource`].
    fn start_for_item(
        gpu: &GpuContext,
        item: GraphicsCaptureItem,
        hwnd: Option<isize>,
        capture_cursor: bool,
    ) -> Result<Self, CaptureError> {
        let winrt_device = create_winrt_device(gpu.device())?;
        let size = item.Size()?;

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            FRAME_POOL_BUFFERS,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;

        // Cursor is config-driven; border removal (pitfall 9) needs Win10 2104+/
        // Win11. Both are best-effort — log and continue on older builds.
        if let Err(e) = session.SetIsCursorCaptureEnabled(capture_cursor) {
            warn!(error = %e, "SetIsCursorCaptureEnabled failed; using system default");
        }
        if let Err(e) = session.SetIsBorderRequired(false) {
            warn!(error = %e, "SetIsBorderRequired(false) failed; capture border may be shown");
        }

        let latest: Arc<Mutex<Option<CapturedFrame>>> = Arc::new(Mutex::new(None));
        let frames_delivered = Arc::new(AtomicU64::new(0));

        let handler = {
            let latest = latest.clone();
            let frames_delivered = frames_delivered.clone();
            TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
                move |sender, _args| {
                    let Some(pool) = sender.as_ref() else {
                        return Ok(());
                    };
                    match pool.TryGetNextFrame() {
                        Ok(frame) => {
                            let srt = match frame.SystemRelativeTime() {
                                Ok(t) => t.Duration,
                                Err(e) => {
                                    warn!(error = %e, "frame missing SystemRelativeTime; dropped");
                                    return Ok(());
                                }
                            };
                            let captured = CapturedFrame {
                                frame,
                                system_relative_time: srt,
                            };
                            // keep-latest: replacing Closes any prior unconsumed frame.
                            // Recover a poisoned lock rather than panic — this runs
                            // on a WGC thread-pool thread outside the engine's
                            // catch_unwind boundary, so a panic here would unwind
                            // across the WinRT FFI callback (UB).
                            *latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(captured);
                            frames_delivered.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => warn!(error = %e, "TryGetNextFrame failed"),
                    }
                    Ok(())
                },
            )
        };
        let token = pool.FrameArrived(&handler)?;

        // The item's `Closed` event fires when the captured window is closed (or the
        // monitor is removed) — M4-2 uses this to fall back to the primary monitor.
        let closed = Arc::new(AtomicBool::new(false));
        let closed_handler = {
            let closed = closed.clone();
            TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_item, _args| {
                closed.store(true, Ordering::Relaxed);
                Ok(())
            })
        };
        let closed_token = item.Closed(&closed_handler)?;

        session.StartCapture()?;

        Ok(Self {
            item,
            winrt_device,
            session,
            pool,
            token,
            closed_token,
            latest,
            frames_delivered,
            closed,
            hwnd,
            size,
        })
    }

    /// Recreate the frame pool at `new_size` (M4-2 window resize): WGC then delivers
    /// frames at the new content size, which the converter rescales into the fixed
    /// canvas — no encoder/epoch change. The `FrameArrived` subscription survives a
    /// `Recreate` (same pool object). The brief gap until the first new-size frame is
    /// covered by the pacing grid's resubmit rule (`§1.2`).
    pub fn recreate_pool(&mut self, new_size: (u32, u32)) -> Result<(), CaptureError> {
        let size = SizeInt32 {
            Width: new_size.0.max(1) as i32,
            Height: new_size.1.max(1) as i32,
        };
        self.pool.Recreate(
            &self.winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            FRAME_POOL_BUFFERS,
            size,
        )?;
        self.size = size;
        Ok(())
    }

    /// A clone of the latest-frame cell for the pacing grid to consume.
    pub fn latest_cell(&self) -> Arc<Mutex<Option<CapturedFrame>>> {
        self.latest.clone()
    }

    /// Take the most recent captured frame, if one is waiting.
    pub fn take_latest(&self) -> Option<CapturedFrame> {
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Total frames WGC has delivered since start (liveness / fps).
    pub fn frames_delivered(&self) -> u64 {
        self.frames_delivered.load(Ordering::Relaxed)
    }

    /// Whether the item's `Closed` event has fired (best-effort — see the field
    /// note; window close is detected by polling [`is_window`] instead).
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// The captured window's `HWND` (as `isize`) when capturing a window; `None` for
    /// a monitor. The engine polls [`is_window`] on this to detect a close and passes
    /// it as [`CaptureSource::Window`] to re-target the same window on a resize.
    pub fn window_hwnd(&self) -> Option<isize> {
        self.hwnd
    }

    /// Capture width in pixels.
    pub fn width(&self) -> u32 {
        self.size.Width.max(0) as u32
    }

    /// Capture height in pixels.
    pub fn height(&self) -> u32 {
        self.size.Height.max(0) as u32
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.pool.RemoveFrameArrived(self.token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// Bridge the shared D3D11 device to a WinRT `IDirect3DDevice` for the pool.
fn create_winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice, CaptureError> {
    // SAFETY: standard DXGI→WinRT device bridge; the returned inspectable is the
    // WinRT device we cast to the typed interface.
    unsafe {
        let dxgi: IDXGIDevice = device.cast()?;
        let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?;
        Ok(inspectable.cast()?)
    }
}

/// Build a capture item for a monitor via the interop factory.
fn create_item_for_monitor(hmon: HMONITOR) -> Result<GraphicsCaptureItem, CaptureError> {
    // SAFETY: the interop factory bridges a Win32 HMONITOR to a WinRT capture
    // item; `CreateForMonitor` returns an owned `GraphicsCaptureItem`.
    unsafe {
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        Ok(interop.CreateForMonitor::<GraphicsCaptureItem>(hmon)?)
    }
}

/// Build a capture item for a window (borderless/windowed) via the interop
/// factory. `01-PROJECT-PLAN.md §3` pitfall 8: window capture for
/// borderless/windowed; exclusive-fullscreen falls back to the monitor upstream.
fn create_item_for_window(hwnd: HWND) -> Result<GraphicsCaptureItem, CaptureError> {
    // SAFETY: the interop factory bridges a Win32 HWND to a WinRT capture item;
    // `CreateForWindow` returns an owned `GraphicsCaptureItem` (or errors if the
    // window is not capturable — the caller falls back to the monitor).
    unsafe {
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        Ok(interop.CreateForWindow::<GraphicsCaptureItem>(hwnd)?)
    }
}

/// The primary monitor handle, or [`CaptureError::NoPrimaryMonitor`].
fn primary_monitor() -> Result<HMONITOR, CaptureError> {
    // SAFETY: `MonitorFromPoint` is a pure query; the origin is inside the primary
    // monitor by definition.
    let hmon = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    if hmon.is_invalid() {
        return Err(CaptureError::NoPrimaryMonitor);
    }
    Ok(hmon)
}

/// The `index`-th monitor in [`EnumDisplayMonitors`] order, if present. Pitfall 31
/// (`monitor = index`): the target is chosen explicitly by OS-enumeration order.
fn monitor_handle_by_index(index: u32) -> Option<HMONITOR> {
    enumerate_monitors().into_iter().nth(index as usize)
}

/// The number of monitors attached (OS-enumeration order). Used by the settings UI (F7) to
/// decide whether to offer per-screen capture choices — a single-monitor machine shows none.
pub fn monitor_count() -> usize {
    enumerate_monitors().len()
}

/// Enumerate every monitor handle in OS-enumeration order.
fn enumerate_monitors() -> Vec<HMONITOR> {
    // The callback pushes each monitor into the `Vec` reached through `lparam`.
    // SAFETY: `lparam` carries a `&mut Vec<HMONITOR>` that outlives the synchronous
    // `EnumDisplayMonitors` call below; the enumerator invokes this on the calling
    // thread only, so the exclusive borrow is not aliased.
    unsafe extern "system" fn cb(mon: HMONITOR, _hdc: HDC, _rc: *mut RECT, data: LPARAM) -> BOOL {
        let list = &mut *(data.0 as *mut Vec<HMONITOR>);
        list.push(mon);
        BOOL(1) // continue enumeration
    }
    let mut list: Vec<HMONITOR> = Vec::new();
    // SAFETY: standard `EnumDisplayMonitors` over the whole virtual desktop (null
    // HDC / clip rect); `cb` only appends to `list`, whose pointer we pass as the
    // lparam and which lives across the (synchronous) call.
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(cb),
            LPARAM(&mut list as *mut Vec<HMONITOR> as isize),
        );
    }
    list
}

/// The foreground window, or `None` if there is none. Resolved once at capture
/// start; the caller falls back to the primary monitor on `None` or an uncapturable
/// window. Note: a console app owns no top-level window, so no reliable "is this my
/// launching terminal?" test exists (worse under ConPTY/Windows Terminal, where the
/// terminal is a separate process) — `focused-window` therefore captures whatever
/// is foreground at start; the M5 tray removes the console entirely. A true
/// exclusive-fullscreen game typically still yields an HWND here but delivers no
/// frames — the M4-2 no-frame watchdog (`§6.3`) is what swaps such a source to the
/// monitor.
fn foreground_window() -> Option<HWND> {
    // SAFETY: `GetForegroundWindow` is a pure query with no preconditions.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        None
    } else {
        Some(hwnd)
    }
}

/// Whether `hwnd` (an `isize` HWND value) still refers to a live window. The M4-2
/// window-close detector: WGC's `Closed` event does not fire on window close on
/// Win11 (probe finding), but `IsWindow` flips to false when the window is destroyed
/// — and stays true while merely minimized, so a minimize is not mistaken for a
/// close.
pub fn is_window(hwnd: isize) -> bool {
    // SAFETY: `IsWindow` is a pure query; a stale/invalid handle simply returns
    // false (its documented behavior).
    unsafe { IsWindow(Some(HWND(hwnd as *mut core::ffi::c_void))).as_bool() }
}

/// The resolution of the monitor `hwnd` is on — the basis for the fixed output canvas
/// (M4-2: a window is captured at native size but encoded into a canvas derived from
/// its monitor, not its own start size). `None` if the handle is invalid.
pub fn window_monitor_size(hwnd: isize) -> Option<(u32, u32)> {
    // SAFETY: `MonitorFromWindow` / `GetMonitorInfoW` are pure queries; `MONITORINFO`
    // is a caller-owned struct with `cbSize` set as the API requires.
    unsafe {
        let hmon = MonitorFromWindow(
            HWND(hwnd as *mut core::ffi::c_void),
            MONITOR_DEFAULTTONEAREST,
        );
        if hmon.is_invalid() {
            return None;
        }
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            let r = mi.rcMonitor;
            Some((
                (r.right - r.left).max(1) as u32,
                (r.bottom - r.top).max(1) as u32,
            ))
        } else {
            None
        }
    }
}
