//! `gpu` — the shared Direct3D 11 device and adapter topology.
//!
//! Cites `01-PROJECT-PLAN.md §2` data-flow rule 1 (pixels stay on the GPU) and
//! pitfall 14 (co-locate the encoder with the captured texture's adapter), plus
//! the `04-TEST-MACHINE.md` "adapter topology" pre-Milestone-1 task.
//!
//! One D3D11 device is shared by the capture thread (WGC frame pool +
//! `ID3D11VideoProcessor`) and the encode thread (MF async MFT). Because they
//! share the device, the NV12 texture never crosses an adapter between
//! conversion and encode — co-location (pitfall 14) is structural, not a
//! per-frame copy. The device is created with:
//! - `VIDEO_SUPPORT` — required by both the video processor and the encoder's
//!   `IMFDXGIDeviceManager` (spike #1 finding);
//! - `BGRA_SUPPORT` — required for WGC-backed surfaces (spike #2);
//!
//! and **multithread protection is enabled** because the async MFT touches the
//! device from its own worker thread while the capture thread also uses it
//! (spike #1; `DECISIONS.md`).
//!
//! ## Adapter selection ([`AdapterSelection`])
//! [`AdapterSelection::Auto`] reproduces the Milestone-0-proven path
//! (`D3D_DRIVER_TYPE_HARDWARE`, default pick — landed on the RTX 4050 dGPU on the
//! test machine, with WGC doing an automatic cross-adapter copy from the
//! iGPU-driven panel). The alternatives pin a specific adapter so the
//! device-on-display-adapter (QSV, same-adapter WGC copy) vs
//! device-on-dGPU (NVENC, cross-adapter copy) tradeoff can be measured on the
//! Nitro and recorded in `DECISIONS.md`. Correctness is identical either way;
//! only the copy/encoder cost differs.
//!
//! ## Apartment
//! This module performs no COM apartment init — `D3D11CreateDevice` and DXGI
//! factory creation do not require it. Threads that additionally drive WGC or
//! Media Foundation own their `CoInitializeEx(COINIT_MULTITHREADED)`.

use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIOutput6,
};

/// How to choose the adapter the shared device is created on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AdapterSelection {
    /// `D3D_DRIVER_TYPE_HARDWARE` with the default adapter pick — the
    /// Milestone-0-proven path (dGPU on the test machine). Default.
    #[default]
    Auto,
    /// The adapter that drives the primary output (co-locate the device with the
    /// display so the WGC copy is same-adapter). Measure against `Auto` on
    /// hybrid laptops.
    PrimaryOutput,
    /// A specific `EnumAdapters1` index.
    Index(u32),
    /// A specific adapter LUID (packed high:low as `i64`; see [`luid_to_i64`]).
    Luid(i64),
}

/// Errors from device creation / adapter enumeration.
#[derive(Debug, thiserror::Error)]
pub enum GpuError {
    /// A Direct3D or DXGI call failed.
    #[error("Direct3D/DXGI call failed: {0}")]
    Windows(#[from] windows::core::Error),
    /// The requested [`AdapterSelection`] matched no enumerated adapter.
    #[error("no DXGI adapter matched the requested selection")]
    AdapterNotFound,
    /// No output reported a desktop rectangle at the origin.
    #[error("no primary DXGI output (desktop rect at origin) found")]
    PrimaryOutputNotFound,
    /// `D3D11CreateDevice` succeeded but wrote no device/context out-param.
    #[error("D3D11CreateDevice returned success but produced no device")]
    NoDevice,
}

/// One display output attached to an adapter.
#[derive(Debug, Clone)]
pub struct OutputInfo {
    /// GDI device name (e.g. `\\.\DISPLAY1`).
    pub device_name: String,
    /// Desktop rectangle starts at the origin — the Windows "primary" display.
    pub is_primary: bool,
    /// Output colour space is HDR10 (`RGB_FULL_G2084_NONE_P2020`).
    pub hdr: bool,
    /// Bits per colour channel reported by the output.
    pub bits_per_color: u32,
    /// Desktop width in pixels.
    pub width: i32,
    /// Desktop height in pixels.
    pub height: i32,
}

/// One graphics adapter and the outputs it drives.
#[derive(Debug, Clone)]
pub struct AdapterInfo {
    /// `EnumAdapters1` index.
    pub index: u32,
    /// Adapter LUID packed high:low as `i64`.
    pub luid: i64,
    /// Human-readable adapter name.
    pub description: String,
    /// PCI vendor id (`0x10DE` NVIDIA, `0x8086` Intel, `0x1002` AMD).
    pub vendor_id: u32,
    /// PCI device id.
    pub device_id: u32,
    /// Dedicated VRAM in mebibytes.
    pub dedicated_vram_mb: u64,
    /// Outputs this adapter drives (empty for a render-only dGPU on a laptop).
    pub outputs: Vec<OutputInfo>,
}

/// The full adapter/output topology of the machine.
#[derive(Debug, Clone)]
pub struct Topology {
    /// Adapters in `EnumAdapters1` order.
    pub adapters: Vec<AdapterInfo>,
}

impl Topology {
    /// Index of the adapter that drives the primary output, if any.
    pub fn primary_adapter_index(&self) -> Option<u32> {
        self.adapters
            .iter()
            .find(|a| a.outputs.iter().any(|o| o.is_primary))
            .map(|a| a.index)
    }
}

impl std::fmt::Display for Topology {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "GPU topology: {} adapter(s)", self.adapters.len())?;
        for a in &self.adapters {
            writeln!(
                f,
                "  [{}] {} (vendor {:#06x}, device {:#06x}, {} MiB VRAM, luid {:#018x})",
                a.index, a.description, a.vendor_id, a.device_id, a.dedicated_vram_mb, a.luid
            )?;
            if a.outputs.is_empty() {
                writeln!(f, "        (drives no outputs — render-only)")?;
            }
            for o in &a.outputs {
                writeln!(
                    f,
                    "        output {} {}x{} {}{}",
                    o.device_name,
                    o.width,
                    o.height,
                    if o.hdr { "HDR" } else { "SDR" },
                    if o.is_primary { " [PRIMARY]" } else { "" },
                )?;
            }
        }
        Ok(())
    }
}

/// Pack a Windows [`LUID`] (high `i32`, low `u32`) into a single `i64`.
pub fn luid_to_i64(luid: LUID) -> i64 {
    ((luid.HighPart as i64) << 32) | (luid.LowPart as i64 & 0xFFFF_FFFF)
}

/// Enumerate every adapter and the outputs it drives.
///
/// Walks the whole DXGI factory (not just the device's own adapter) because on a
/// hybrid laptop the render adapter may drive zero outputs (spike #2).
pub fn enumerate_topology() -> Result<Topology, GpuError> {
    // SAFETY: DXGI enumeration is read-only; every interface is released on drop.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut adapters = Vec::new();
        let mut adapter_idx = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(adapter_idx) {
            let desc = adapter.GetDesc1()?;
            let mut outputs = Vec::new();
            let mut output_idx = 0u32;
            while let Ok(output) = adapter.EnumOutputs(output_idx) {
                output_idx += 1;
                let Ok(output6) = output.cast::<IDXGIOutput6>() else {
                    continue;
                };
                let od = output6.GetDesc1()?;
                let rect = od.DesktopCoordinates;
                outputs.push(OutputInfo {
                    device_name: utf16_to_string(&od.DeviceName),
                    is_primary: rect.left == 0 && rect.top == 0,
                    hdr: od.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
                    bits_per_color: od.BitsPerColor,
                    width: rect.right - rect.left,
                    height: rect.bottom - rect.top,
                });
            }
            adapters.push(AdapterInfo {
                index: adapter_idx,
                luid: luid_to_i64(desc.AdapterLuid),
                description: utf16_to_string(&desc.Description),
                vendor_id: desc.VendorId,
                device_id: desc.DeviceId,
                dedicated_vram_mb: (desc.DedicatedVideoMemory as u64) / (1024 * 1024),
                outputs,
            });
            adapter_idx += 1;
        }
        Ok(Topology { adapters })
    }
}

/// The shared Direct3D 11 device, its immediate context, and which adapter it
/// landed on. Cloning is a cheap COM `AddRef` on the underlying interfaces.
#[derive(Clone)]
pub struct GpuContext {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// LUID of the adapter the device was created on.
    pub adapter_luid: i64,
    /// Description of the adapter the device was created on.
    pub adapter_description: String,
}

impl std::fmt::Debug for GpuContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuContext")
            .field("adapter_luid", &format_args!("{:#018x}", self.adapter_luid))
            .field("adapter_description", &self.adapter_description)
            .finish()
    }
}

impl GpuContext {
    /// Create the shared device on the adapter chosen by `selection`, enable
    /// multithread protection, and record which adapter it landed on.
    pub fn new(selection: AdapterSelection) -> Result<Self, GpuError> {
        let adapter = resolve_adapter(selection)?;
        let (device, context) = create_device(adapter.as_ref())?;
        enable_multithread_protection(&context)?;
        let (adapter_luid, adapter_description) = describe_device_adapter(&device)?;
        Ok(Self {
            device,
            context,
            adapter_luid,
            adapter_description,
        })
    }

    /// The shared D3D11 device (capture + encode).
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// The device's immediate context (multithread-protected).
    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }
}

/// Resolve an [`AdapterSelection`] to a concrete adapter handle. `Auto` yields
/// `None` (use `D3D_DRIVER_TYPE_HARDWARE`); every other variant pins an adapter.
fn resolve_adapter(selection: AdapterSelection) -> Result<Option<IDXGIAdapter>, GpuError> {
    let target_index = match selection {
        AdapterSelection::Auto => return Ok(None),
        AdapterSelection::Index(i) => Some(i),
        AdapterSelection::PrimaryOutput => Some(
            enumerate_topology()?
                .primary_adapter_index()
                .ok_or(GpuError::PrimaryOutputNotFound)?,
        ),
        AdapterSelection::Luid(_) => None,
    };

    // SAFETY: DXGI enumeration is read-only; the adapter handle is refcounted.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        match selection {
            AdapterSelection::Luid(luid) => {
                let mut idx = 0u32;
                while let Ok(adapter) = factory.EnumAdapters1(idx) {
                    if luid_to_i64(adapter.GetDesc1()?.AdapterLuid) == luid {
                        return Ok(Some(adapter.cast()?));
                    }
                    idx += 1;
                }
                Err(GpuError::AdapterNotFound)
            }
            _ => {
                let idx = target_index.ok_or(GpuError::AdapterNotFound)?;
                let adapter = factory
                    .EnumAdapters1(idx)
                    .map_err(|_| GpuError::AdapterNotFound)?;
                Ok(Some(adapter.cast()?))
            }
        }
    }
}

/// Create the D3D11 device + immediate context with `VIDEO | BGRA` support.
fn create_device(
    adapter: Option<&IDXGIAdapter>,
) -> Result<(ID3D11Device, ID3D11DeviceContext), GpuError> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    // A pinned adapter requires driver type UNKNOWN; the default pick uses HARDWARE.
    let driver_type = if adapter.is_some() {
        D3D_DRIVER_TYPE_UNKNOWN
    } else {
        D3D_DRIVER_TYPE_HARDWARE
    };
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: standard device creation. VIDEO_SUPPORT is required by the video
    // processor and the encoder's device manager; BGRA_SUPPORT by WGC surfaces.
    // The out-params are written on S_OK.
    unsafe {
        D3D11CreateDevice(
            adapter,
            driver_type,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    match (device, context) {
        (Some(d), Some(c)) => Ok((d, c)),
        _ => Err(GpuError::NoDevice),
    }
}

/// Enable multithread protection so the async MFT worker thread can share the
/// device with the capture thread (spike #1).
fn enable_multithread_protection(context: &ID3D11DeviceContext) -> Result<(), GpuError> {
    // SAFETY: ID3D11Multithread is a standard cast of the immediate context;
    // SetMultithreadProtected serializes device access across threads.
    unsafe {
        let multithread: ID3D11Multithread = context.cast()?;
        let _ = multithread.SetMultithreadProtected(true);
    }
    Ok(())
}

/// Read the LUID + description of the adapter a device actually landed on.
fn describe_device_adapter(device: &ID3D11Device) -> Result<(i64, String), GpuError> {
    // SAFETY: read-only cast + adapter description query.
    unsafe {
        let dxgi: IDXGIDevice = device.cast()?;
        let adapter: IDXGIAdapter1 = dxgi.GetAdapter()?.cast()?;
        let desc = adapter.GetDesc1()?;
        Ok((
            luid_to_i64(desc.AdapterLuid),
            utf16_to_string(&desc.Description),
        ))
    }
}

/// Decode a NUL-terminated fixed-size UTF-16 buffer into a `String`.
fn utf16_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
