//! Milestone-0 spike #2 — **WGC primary-monitor capture in isolation**.
//!
//! Tracker M0 #2: "capture primary monitor, count fps, verify texture format on
//! SDR + HDR display." Proves the Windows.Graphics.Capture (WGC) path end to
//! end without any encoder or mux: create a `GraphicsCaptureItem` for the
//! primary monitor, run a free-threaded frame pool, and for each delivered
//! frame reach the backing `ID3D11Texture2D` — **the pixels stay on the GPU**
//! (CLAUDE.md rule 6); we only read the texture *descriptor*, never its bytes.
//!
//! ## What it reports
//! - Adapter, whether `GraphicsCaptureSession::IsSupported()`.
//! - The primary output's colour space → **SDR vs HDR**, and therefore the pool
//!   pixel format we request (`B8G8R8A8UIntNormalized` vs `R16G16B16A16Float`).
//! - The **actual `DXGI_FORMAT` + size** of the first captured texture, asserted
//!   against what the colour space predicts (pitfall 12: HDR hands you FP16, and
//!   a naïve BGRA8 assumption yields garbage).
//! - **Measured fps** over a ~3 s window (reflects on-screen activity — WGC
//!   delivers a frame per DWM present, so wiggle the mouse / play a video).
//!
//! ## Not this spike's job
//! No colour conversion, no BGRA→NV12, no encode, no HDR tone-map. Just: does
//! WGC deliver frames on this hybrid-graphics laptop, and in what format.
//!
//! ## Safety / threading
//! `FrameArrived` fires on a WinRT thread-pool thread, so the shared counters
//! are `Arc<Atomic*>` / `Arc<Mutex<..>>`. Every `unsafe` block is a COM/D3D FFI
//! call with its invariant stated. Throwaway crate; never linked into `clipd`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{error, info, warn};
use windows::core::{Interface, Result};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{E_FAIL, HMODULE, POINT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1, IDXGIOutput6,
};
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, HMONITOR, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

const CAPTURE_SECONDS: u64 = 3;
const FRAME_POOL_BUFFERS: i32 = 2;

/// What the first captured texture actually looked like.
#[derive(Clone, Copy)]
struct FirstFrame {
    format: DXGI_FORMAT,
    width: u32,
    height: u32,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run() {
        Ok(()) => info!("spike OK"),
        Err(e) => {
            error!(error = %e, hresult = format!("0x{:08X}", e.code().0 as u32), "spike FAILED");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<()> {
    // SAFETY: MTA init for this thread; WGC/WinRT interop requires an
    // initialized apartment. S_FALSE (already-init) is not an error.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let result = capture();
    // SAFETY: balance the CoInitializeEx above.
    unsafe { CoUninitialize() };
    result
}

fn capture() -> Result<()> {
    if !GraphicsCaptureSession::IsSupported()? {
        error!("GraphicsCaptureSession::IsSupported() = false — WGC unavailable");
        return Err(windows::core::Error::from(E_FAIL));
    }

    let device = create_d3d11_device()?;
    log_adapter(&device)?;

    // Primary monitor handle for the capture item.
    // SAFETY: MonitorFromPoint is a pure query; the origin is always inside the
    // primary monitor.
    let hmon = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    if hmon.is_invalid() {
        error!("no primary monitor");
        return Err(windows::core::Error::from(E_FAIL));
    }

    let disp = probe_primary_output()?;
    let (pixel_format, expected_dxgi) = if disp.hdr {
        (
            DirectXPixelFormat::R16G16B16A16Float,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
        )
    } else {
        (
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            DXGI_FORMAT_B8G8R8A8_UNORM,
        )
    };
    info!(
        hdr = disp.hdr,
        color_space = disp.color_space.0,
        bits_per_color = disp.bits_per_color,
        width = disp.width,
        height = disp.height,
        requested_pool_format = pixel_format.0,
        "primary display probed"
    );

    // Bridge the D3D11 device to a WinRT IDirect3DDevice for the frame pool.
    let winrt_device = create_winrt_device(&device)?;

    // Build the capture item for the monitor via the interop factory.
    let item = create_item_for_monitor(hmon)?;
    let size = item.Size()?;
    info!(
        width = size.Width,
        height = size.Height,
        "capture item created for primary monitor"
    );

    // Free-threaded pool: FrameArrived is delivered on a WinRT pool thread.
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        pixel_format,
        FRAME_POOL_BUFFERS,
        size,
    )?;
    let session = pool.CreateCaptureSession(&item)?;

    // Shared state written from the FrameArrived thread.
    let frame_count = Arc::new(AtomicU32::new(0));
    let first_frame: Arc<Mutex<Option<FirstFrame>>> = Arc::new(Mutex::new(None));

    let handler = {
        let frame_count = frame_count.clone();
        let first_frame = first_frame.clone();
        TypedEventHandler::<Direct3D11CaptureFramePool, windows::core::IInspectable>::new(
            move |sender, _args| {
                let Some(pool) = sender.as_ref() else {
                    return Ok(());
                };
                // Draining every delivered frame keeps the pool from stalling.
                match pool.TryGetNextFrame() {
                    Ok(frame) => {
                        frame_count.fetch_add(1, Ordering::Relaxed);
                        if first_frame.lock().unwrap().is_none() {
                            if let Ok(info) = read_frame_format(&frame) {
                                *first_frame.lock().unwrap() = Some(info);
                            }
                        }
                        // Return the surface to the pool.
                        let _ = frame.Close();
                    }
                    Err(e) => warn!(error = %e, "TryGetNextFrame failed"),
                }
                Ok(())
            },
        )
    };
    let token = pool.FrameArrived(&handler)?;

    info!(
        seconds = CAPTURE_SECONDS,
        "starting capture — wiggle the mouse / play a video for a real fps"
    );
    session.StartCapture()?;

    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(CAPTURE_SECONDS));
    let elapsed = start.elapsed().as_secs_f64();

    // Stop cleanly.
    pool.RemoveFrameArrived(token)?;
    session.Close()?;
    pool.Close()?;

    let frames = frame_count.load(Ordering::Relaxed);
    let fps = frames as f64 / elapsed;
    info!(
        frames,
        elapsed_s = format!("{elapsed:.2}"),
        fps = format!("{fps:.1}"),
        "capture stopped"
    );

    // Verify the texture format the compositor actually handed us.
    match *first_frame.lock().unwrap() {
        Some(ff) => {
            let matches = ff.format == expected_dxgi;
            info!(
                actual_format = ff.format.0,
                expected_format = expected_dxgi.0,
                matches,
                width = ff.width,
                height = ff.height,
                "first-frame texture format"
            );
            if !matches {
                warn!("actual texture format != expected for this colour space (investigate pitfall 12)");
            }
        }
        None => {
            warn!("no frame captured — screen was fully static, or capture failed; re-run with on-screen motion");
        }
    }

    if frames == 0 {
        return Err(windows::core::Error::from(E_FAIL));
    }
    Ok(())
}

fn create_d3d11_device() -> Result<ID3D11Device> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut device: Option<ID3D11Device> = None;
    // SAFETY: standard device creation; BGRA support is required for WGC-backed
    // surfaces. The out-param is written on S_OK.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }
    device.ok_or_else(|| windows::core::Error::from(E_FAIL))
}

fn log_adapter(device: &ID3D11Device) -> Result<()> {
    // SAFETY: read-only cast + adapter description query.
    unsafe {
        let dxgi: IDXGIDevice = device.cast()?;
        let adapter = dxgi.GetAdapter()?;
        let desc = adapter.GetDesc()?;
        let name = utf16_to_string(&desc.Description);
        info!(adapter = %name, vendor = format!("0x{:04X}", desc.VendorId), "D3D11 device");
    }
    Ok(())
}

struct DisplayInfo {
    hdr: bool,
    color_space: windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_TYPE,
    bits_per_color: u32,
    width: i32,
    height: i32,
}

/// Find the primary output (the one whose desktop rect starts at the origin)
/// across ALL adapters — on a hybrid laptop the D3D device's own adapter may
/// drive no outputs, so we enumerate the whole DXGI factory.
fn probe_primary_output() -> Result<DisplayInfo> {
    // SAFETY: DXGI enumeration is read-only; every interface is released on drop.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut adapter_idx = 0;
        while let Ok(adapter) = factory.EnumAdapters1(adapter_idx) {
            adapter_idx += 1;
            let mut output_idx = 0;
            while let Ok(output) = adapter.EnumOutputs(output_idx) {
                output_idx += 1;
                let output6: IDXGIOutput6 = match output.cast() {
                    Ok(o) => o,
                    Err(_) => continue,
                };
                let desc = output6.GetDesc1()?;
                let rect = desc.DesktopCoordinates;
                if rect.left == 0 && rect.top == 0 {
                    let hdr = desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
                    return Ok(DisplayInfo {
                        hdr,
                        color_space: desc.ColorSpace,
                        bits_per_color: desc.BitsPerColor,
                        width: rect.right - rect.left,
                        height: rect.bottom - rect.top,
                    });
                }
            }
        }
    }
    error!("could not find the primary DXGI output");
    Err(windows::core::Error::from(E_FAIL))
}

fn create_winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice> {
    // SAFETY: bridge the D3D11 device to a WinRT IDirect3DDevice; the returned
    // IInspectable is the WinRT device we cast to the typed interface.
    unsafe {
        let dxgi: IDXGIDevice = device.cast()?;
        let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?;
        inspectable.cast()
    }
}

fn create_item_for_monitor(hmon: HMONITOR) -> Result<GraphicsCaptureItem> {
    // SAFETY: the interop factory bridges a Win32 HMONITOR to a WinRT capture
    // item; CreateForMonitor returns an owned GraphicsCaptureItem.
    unsafe {
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        interop.CreateForMonitor::<GraphicsCaptureItem>(hmon)
    }
}

/// Read the DXGI format + size of a captured frame's backing texture — the
/// descriptor only; the pixels are never mapped to system RAM.
fn read_frame_format(
    frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
) -> Result<FirstFrame> {
    // SAFETY: reach the ID3D11Texture2D behind the WinRT surface via the DXGI
    // interface-access bridge, then read its descriptor.
    unsafe {
        let surface = frame.Surface()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        let texture: ID3D11Texture2D = access.GetInterface()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        Ok(FirstFrame {
            format: desc.Format,
            width: desc.Width,
            height: desc.Height,
        })
    }
}

fn utf16_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
