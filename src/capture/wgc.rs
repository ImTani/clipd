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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tracing::warn;
use windows::core::{IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D, D3D11_TEXTURE2D_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, HMONITOR, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::gpu::GpuContext;

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
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    /// `FrameArrived` registration token (a bare `i64` in `windows` 0.62).
    token: i64,
    latest: Arc<Mutex<Option<CapturedFrame>>>,
    frames_delivered: Arc<AtomicU64>,
    size: SizeInt32,
}

impl WgcCapture {
    /// Start capturing the primary monitor with the shared device. `capture_cursor`
    /// maps to `config.capture.cursor`.
    pub fn start_primary(gpu: &GpuContext, capture_cursor: bool) -> Result<Self, CaptureError> {
        if !GraphicsCaptureSession::IsSupported()? {
            return Err(CaptureError::Unsupported);
        }

        // SAFETY: `MonitorFromPoint` is a pure query; the origin is inside the
        // primary monitor by definition.
        let hmon = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        if hmon.is_invalid() {
            return Err(CaptureError::NoPrimaryMonitor);
        }

        let winrt_device = create_winrt_device(gpu.device())?;
        let item = create_item_for_monitor(hmon)?;
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
                            *latest.lock().unwrap() = Some(captured);
                            frames_delivered.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => warn!(error = %e, "TryGetNextFrame failed"),
                    }
                    Ok(())
                },
            )
        };
        let token = pool.FrameArrived(&handler)?;
        session.StartCapture()?;

        Ok(Self {
            session,
            pool,
            token,
            latest,
            frames_delivered,
            size,
        })
    }

    /// A clone of the latest-frame cell for the pacing grid to consume.
    pub fn latest_cell(&self) -> Arc<Mutex<Option<CapturedFrame>>> {
        self.latest.clone()
    }

    /// Take the most recent captured frame, if one is waiting.
    pub fn take_latest(&self) -> Option<CapturedFrame> {
        self.latest.lock().unwrap().take()
    }

    /// Total frames WGC has delivered since start (liveness / fps).
    pub fn frames_delivered(&self) -> u64 {
        self.frames_delivered.load(Ordering::Relaxed)
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
