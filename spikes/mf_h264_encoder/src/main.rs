//! Milestone-0 spike #1 — **MF async hardware H.264 encoder in isolation**.
//!
//! Highest-risk component (01-PROJECT-PLAN.md §5.1, pitfall 17: "the two weeks
//! of pain"). Goal: prove the asynchronous Media Foundation Transform (MFT)
//! event state machine — `METransformNeedInput` / `METransformHaveOutput` —
//! plus the D3D11 device-manager plumbing, feeding **D3D11-texture-backed NV12
//! samples** (pixels stay on the GPU, CLAUDE.md rule 6) and emitting a playable
//! H.264 Annex-B elementary stream. No capture, no audio, no mux — those are
//! later spikes/milestones. `.mp4` muxing is spike #4 (Sink Writer vs fMP4).
//!
//! ## What it does
//! 1. Boots COM (MTA) + Media Foundation.
//! 2. Creates a hardware D3D11 device (VIDEO_SUPPORT), enables multithread
//!    protection, wraps it in an `IMFDXGIDeviceManager`.
//! 3. Enumerates hardware H.264 encoder MFTs (NV12 in → H.264 out), activates
//!    the first, unlocks async, hands it the D3D manager, negotiates types.
//! 4. Runs the async event loop: on NeedInput, uploads a synthetic moving-bars
//!    NV12 frame to a GPU texture and `ProcessInput`s it; on HaveOutput,
//!    `ProcessOutput`s the encoded sample and appends its bytes to the file.
//! 5. After the last frame, drains (END_OF_STREAM + DRAIN) and stops on
//!    `METransformDrainComplete`.
//!
//! ## What it proves / does NOT prove
//! Proves: the async MFT contract, D3D manager acceptance of GPU textures,
//! and that the hardware encoder produces a decodable stream on this machine.
//! Does NOT prove: colour correctness (BT.709 limited is Milestone 1) — the
//! synthetic chroma is neutral grey on purpose; nor CFR pacing, nor mux.
//!
//! ## Safety / threading
//! Single-threaded spike; everything runs on `main`. Every `unsafe` block is a
//! COM/D3D/MF FFI call whose invariant is stated in its `// SAFETY:` comment.
//! This crate is throwaway and is never linked into `clipd`.

use std::ffi::c_void;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use tracing::{error, info, warn};
use windows::core::{Interface, Result, PWSTR};
use windows::Win32::Foundation::{E_FAIL, HMODULE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource,
    ID3D11Texture2D, D3D11_BIND_FLAG, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::eAVEncH264VProfile_Main;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaEventGenerator, IMFMediaType,
    IMFSample, IMFTransform, METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
    MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFShutdown, MFStartup, MFTEnumEx, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};

// ── Spike parameters (small + fast; a spike proves the path, not the product) ──
const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FPS: u32 = 60;
const FRAME_COUNT: u32 = 120; // 2 s of video
const BITRATE_BPS: u32 = 8_000_000; // 8 Mbps CBR — spike uses bitrate RC; CQP is M1+
const TICKS_PER_SECOND: i64 = 10_000_000; // 100 ns ticks (02-AV-SYNC-SPEC.md §0)

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run() {
        Ok(path) => {
            info!(output = %path.display(), "spike OK — encoded stream written");
            info!("VALIDATE: run `mediainfo` / `ffprobe` on the file (see spike README).");
        }
        Err(e) => {
            error!(error = %e, hresult = format!("0x{:08X}", e.code().0 as u32), "spike FAILED");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<PathBuf> {
    // SAFETY: initializing COM for this thread as MTA (CLAUDE.md COM threading
    // rule). Ignoring the HRESULT is intentional: S_FALSE (already initialized)
    // is not an error here.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    // SAFETY: MFStartup pairs with MFShutdown below; MF_VERSION is the crate's
    // matching version constant.
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };

    let result = encode();

    // SAFETY: tear down MF then COM regardless of the encode outcome, matching
    // the startup calls above.
    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
    result
}

fn encode() -> Result<PathBuf> {
    let (device, context) = create_d3d11_device()?;
    log_adapter(&device)?;

    // Multithread protection is mandatory once a D3D11 device is shared with an
    // async MFT that touches it from its own worker thread.
    // SAFETY: ID3D11Multithread is a supported cast of the immediate context.
    let multithread: ID3D11Multithread = context.cast()?;
    // SAFETY: FFI toggle; returns the prior state, which we don't need.
    let _ = unsafe { multithread.SetMultithreadProtected(true) };

    let manager = create_device_manager(&device)?;

    let transform = activate_h264_encoder()?;
    configure_encoder(&transform, &manager)?;

    let path = encode_loop(&transform, &device, &context)?;
    Ok(path)
}

/// Create a hardware D3D11 device with video support (needed for the encoder's
/// device manager) and BGRA support (harmless, keeps the device usable by the
/// later WGC/VideoProcessor spikes if this code is cannibalised).
fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: standard D3D11CreateDevice call; out-params are written on S_OK.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    Ok((
        device.ok_or_else(|| windows::core::Error::from(E_FAIL))?,
        context.ok_or_else(|| windows::core::Error::from(E_FAIL))?,
    ))
}

/// Print adapter description + IDs so pasted-back spike output is
/// self-documenting (07-DEVFLOW.md §5).
fn log_adapter(device: &ID3D11Device) -> Result<()> {
    // SAFETY: IDXGIDevice is a supported cast of ID3D11Device; GetAdapter and
    // GetDesc are read-only queries.
    unsafe {
        let dxgi: IDXGIDevice = device.cast()?;
        let adapter = dxgi.GetAdapter()?;
        let desc = adapter.GetDesc()?;
        let name = String::from_utf16_lossy(
            &desc.Description[..desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len())],
        );
        info!(
            adapter = %name,
            vendor = format!("0x{:04X}", desc.VendorId),
            device_id = format!("0x{:04X}", desc.DeviceId),
            vram_mb = desc.DedicatedVideoMemory / (1024 * 1024),
            "D3D11 hardware device created"
        );
    }
    Ok(())
}

/// Wrap the D3D11 device in an `IMFDXGIDeviceManager` so the encoder can take
/// GPU-resident NV12 textures directly (no system-RAM copy).
fn create_device_manager(device: &ID3D11Device) -> Result<IMFDXGIDeviceManager> {
    let mut reset_token: u32 = 0;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    // SAFETY: out-params written on success; ResetDevice binds our device to the
    // manager using the token it just handed back.
    unsafe {
        MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)?;
        let manager = manager.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        manager.ResetDevice(device, reset_token)?;
        Ok(manager)
    }
}

/// Enumerate hardware NV12→H.264 encoder MFTs and activate the first.
fn activate_h264_encoder() -> Result<IMFTransform> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    let mut activate_arr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    // SAFETY: MFTEnumEx allocates an array of IMFActivate* via CoTaskMemAlloc;
    // we own it and free it below.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut activate_arr,
            &mut count,
        )?;
    }
    if count == 0 || activate_arr.is_null() {
        error!("no hardware H.264 encoder MFT found (NV12 → H.264)");
        return Err(windows::core::Error::from(E_FAIL));
    }
    info!(count, "hardware H.264 encoder MFT(s) enumerated");

    // SAFETY: `activate_arr` points to `count` initialized `Option<IMFActivate>`
    // entries. We take ownership of element 0, drop the rest (Release), then
    // free the CoTaskMem block.
    let transform = unsafe {
        let slice = std::slice::from_raw_parts(activate_arr, count as usize);
        let first = slice[0]
            .clone()
            .ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        for entry in slice.iter() {
            std::ptr::drop_in_place(
                entry as *const Option<IMFActivate> as *mut Option<IMFActivate>,
            );
        }
        CoTaskMemFree(Some(activate_arr as *const c_void));

        log_activate_name(&first);
        first.ActivateObject::<IMFTransform>()?
    };
    Ok(transform)
}

/// Best-effort log of the MFT's friendly name (which vendor block we got).
fn log_activate_name(activate: &IMFActivate) {
    // MFT_FRIENDLY_NAME_Attribute; read via GetAllocatedString. Failure is
    // non-fatal — it's a diagnostic only.
    // SAFETY: read-only attribute query; PWSTR freed with CoTaskMemFree.
    unsafe {
        let mut ptr = PWSTR::null();
        let mut len = 0u32;
        // MFT_FRIENDLY_NAME_Attribute GUID.
        let key = windows::core::GUID::from_u128(0x314ffbae_5b41_4c95_9c19_4e7d586face3);
        if activate
            .GetAllocatedString(&key, &mut ptr, &mut len)
            .is_ok()
            && !ptr.is_null()
        {
            let name = ptr.to_string().unwrap_or_default();
            info!(encoder = %name, "activated encoder MFT");
            CoTaskMemFree(Some(ptr.0 as *const c_void));
        }
    }
}

/// Unlock async, hand over the D3D manager, and negotiate output-then-input
/// media types (H.264 encoders require the output type be set first).
fn configure_encoder(transform: &IMFTransform, manager: &IMFDXGIDeviceManager) -> Result<()> {
    // SAFETY: all calls below are on the freshly activated transform.
    unsafe {
        // Async hardware MFTs must be explicitly unlocked before use.
        let attrs = transform.GetAttributes()?;
        attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;

        // Bind the D3D device manager (ulparam is the interface pointer).
        transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

        // Output type first.
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, BITRATE_BPS)?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
        set_frame_size(&out)?;
        set_frame_rate(&out)?;
        set_pixel_aspect_ratio(&out)?;
        transform.SetOutputType(0, &out, 0)?;

        // Input type second.
        let inp = MFCreateMediaType()?;
        inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        set_frame_size(&inp)?;
        set_frame_rate(&inp)?;
        set_pixel_aspect_ratio(&inp)?;
        transform.SetInputType(0, &inp, 0)?;
    }
    info!(
        width = WIDTH,
        height = HEIGHT,
        fps = FPS,
        bitrate_bps = BITRATE_BPS,
        "encoder configured (NV12 in → H.264 Main out)"
    );
    Ok(())
}

// MF packs 2-D attributes as a single u64 (hi:lo). The C-header helpers
// (MFSetAttributeSize/Ratio) are not exposed by the `windows` crate, so pack by
// hand.
fn set_frame_size(mt: &IMFMediaType) -> Result<()> {
    // SAFETY: attribute setter on a valid media type.
    unsafe { mt.SetUINT64(&MF_MT_FRAME_SIZE, ((WIDTH as u64) << 32) | HEIGHT as u64) }
}
fn set_frame_rate(mt: &IMFMediaType) -> Result<()> {
    // SAFETY: attribute setter; rate is FPS/1.
    unsafe { mt.SetUINT64(&MF_MT_FRAME_RATE, ((FPS as u64) << 32) | 1) }
}
fn set_pixel_aspect_ratio(mt: &IMFMediaType) -> Result<()> {
    // SAFETY: attribute setter; square pixels 1:1.
    unsafe { mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1) }
}

/// The async event loop: drive NeedInput/HaveOutput to completion.
fn encode_loop(
    transform: &IMFTransform,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
) -> Result<PathBuf> {
    let out_path = std::env::temp_dir().join("clipd_spike_mf_h264.h264");
    let file = File::create(&out_path).map_err(|e| {
        error!(error = %e, "failed to create output file");
        windows::core::Error::from(E_FAIL)
    })?;
    let mut writer = BufWriter::new(file);

    let provides_samples = output_provides_samples(transform)?;
    info!(provides_samples, "encoder output allocation mode");

    // Reusable GPU textures: a STAGING texture we fill on the CPU (synthetic
    // frames only — the real path never touches system RAM) and a DEFAULT
    // texture the encoder reads. Real capture will hand us the WGC texture
    // directly; here we just need *a* GPU-resident NV12 surface.
    let staging =
        create_nv12_texture(device, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_WRITE.0 as u32)?;

    let event_gen: IMFMediaEventGenerator = transform.cast()?;

    // SAFETY: begin/start streaming precede the first event; all FFI below is on
    // the configured transform.
    unsafe {
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
    }

    let mut frames_in: u32 = 0;
    let mut frames_out: u32 = 0;
    let mut total_bytes: u64 = 0;
    let mut draining = false;

    loop {
        // Blocking wait for the next transform event.
        // SAFETY: GetEvent blocks until an event is available or the generator
        // is shut down; MF_EVENT_FLAG_NONE == 0.
        let event = unsafe { event_gen.GetEvent(Default::default())? };
        // SAFETY: GetType reads the event's type code.
        let event_type = unsafe { event.GetType()? };

        match event_type {
            t if t == METransformNeedInput.0 as u32 => {
                if frames_in < FRAME_COUNT {
                    let sample = build_input_sample(device, context, &staging, frames_in)?;
                    // SAFETY: feeding one input sample in response to NeedInput,
                    // as the async contract requires.
                    unsafe { transform.ProcessInput(0, &sample, 0)? };
                    frames_in += 1;
                } else if !draining {
                    // All frames fed: drain.
                    // SAFETY: end-of-stream then drain triggers the tail
                    // HaveOutput events and the final DrainComplete.
                    unsafe {
                        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
                        transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
                    }
                    draining = true;
                    info!(frames_in, "all frames submitted; draining");
                }
            }
            t if t == METransformHaveOutput.0 as u32 => {
                let bytes = drain_one_output(transform, provides_samples, &mut writer)?;
                total_bytes += bytes as u64;
                frames_out += 1;
            }
            t if t == METransformDrainComplete.0 as u32 => {
                info!("drain complete");
                break;
            }
            other => {
                warn!(event_type = other, "ignoring unexpected transform event");
            }
        }
    }

    writer.flush().map_err(|e| {
        error!(error = %e, "flush failed");
        windows::core::Error::from(E_FAIL)
    })?;

    info!(
        frames_in,
        frames_out,
        total_bytes,
        avg_frame_bytes = total_bytes / frames_out.max(1) as u64,
        "encode finished"
    );
    if frames_out < FRAME_COUNT {
        warn!(
            frames_in,
            frames_out, "fewer outputs than inputs — check drain / stream-change handling"
        );
    }
    Ok(out_path)
}

/// Does the MFT allocate its own output samples (hardware encoders typically
/// do), or must we supply the buffer?
fn output_provides_samples(transform: &IMFTransform) -> Result<bool> {
    // SAFETY: read-only stream-info query on the configured transform.
    let info = unsafe { transform.GetOutputStreamInfo(0)? };
    let flags = info.dwFlags;
    let mask =
        (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
    Ok(flags & mask != 0)
}

/// Pull exactly one encoded sample and append its bytes to `writer`. Returns the
/// byte count written.
fn drain_one_output(
    transform: &IMFTransform,
    provides_samples: bool,
    writer: &mut BufWriter<File>,
) -> Result<usize> {
    let mut output = MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        ..Default::default()
    };

    // When the MFT does NOT provide samples we must supply an output buffer.
    if !provides_samples {
        // SAFETY: allocate a sample+buffer sized to the stream info.
        let info = unsafe { transform.GetOutputStreamInfo(0)? };
        let sample = unsafe { MFCreateSample()? };
        let buffer = unsafe { MFCreateMemoryBuffer(info.cbSize.max(1))? };
        // SAFETY: attach the buffer to the sample.
        unsafe { sample.AddBuffer(&buffer)? };
        output.pSample = std::mem::ManuallyDrop::new(Some(sample));
    }

    let mut status: u32 = 0;
    // SAFETY: ProcessOutput consumes/fills the single output buffer; on
    // PROVIDES_SAMPLES the MFT sets pSample for us.
    unsafe {
        transform.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)?;
    }

    // SAFETY: after a successful ProcessOutput pSample is a valid sample we own.
    let sample: IMFSample = unsafe {
        std::mem::ManuallyDrop::take(&mut output.pSample)
            .ok_or_else(|| windows::core::Error::from(E_FAIL))?
    };

    let written = write_sample_bytes(&sample, writer)?;

    // Release any events collection the MFT attached.
    // SAFETY: pEvents, if set, is a collection we own; dropping releases it.
    unsafe {
        let _ = std::mem::ManuallyDrop::take(&mut output.pEvents);
    }
    Ok(written)
}

/// Copy the contiguous bytes of an encoded H.264 sample into the output file.
fn write_sample_bytes(sample: &IMFSample, writer: &mut BufWriter<File>) -> Result<usize> {
    // SAFETY: flatten to a single buffer, lock it, copy out, unlock.
    unsafe {
        let buffer: IMFMediaBuffer = sample.ConvertToContiguousBuffer()?;
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut max_len: u32 = 0;
        let mut cur_len: u32 = 0;
        buffer.Lock(&mut data, Some(&mut max_len), Some(&mut cur_len))?;
        let slice = std::slice::from_raw_parts(data, cur_len as usize);
        let res = writer.write_all(slice);
        buffer.Unlock()?;
        res.map_err(|e| {
            error!(error = %e, "write_all failed");
            windows::core::Error::from(E_FAIL)
        })?;
        Ok(cur_len as usize)
    }
}

/// Build a D3D11-texture-backed NV12 `IMFSample` for frame `index`, timestamped
/// on the exact CFR grid (02-AV-SYNC-SPEC.md §1.2: `n * 10^7 / fps`, computed
/// integer each time — never accumulate a rounded duration).
fn build_input_sample(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    index: u32,
) -> Result<IMFSample> {
    // A fresh DEFAULT texture per frame keeps the encoder from reading a surface
    // we're simultaneously rewriting (correctness over allocation churn — this
    // is a spike). The real pipeline hands the encoder the capture texture.
    let input_tex = create_nv12_texture(device, D3D11_USAGE_DEFAULT, 0)?;
    fill_synthetic_nv12(context, staging, index)?;

    // SAFETY: copy the freshly-filled staging surface into the encoder input
    // texture, then wrap it as an MF sample.
    let sample = unsafe {
        let src: ID3D11Resource = staging.cast()?;
        let dst: ID3D11Resource = input_tex.cast()?;
        context.CopyResource(&dst, &src);

        let buffer: IMFMediaBuffer =
            MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &input_tex, 0, false)?;
        // DXGI surface buffers start with length 0; set it to the plane's
        // contiguous length so ProcessInput sees a non-empty buffer.
        let two_d: windows::Win32::Media::MediaFoundation::IMF2DBuffer = buffer.cast()?;
        let contiguous = two_d.GetContiguousLength()?;
        buffer.SetCurrentLength(contiguous)?;

        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        let pts = (index as i64 * TICKS_PER_SECOND) / FPS as i64;
        let next = ((index as i64 + 1) * TICKS_PER_SECOND) / FPS as i64;
        sample.SetSampleTime(pts)?;
        sample.SetSampleDuration(next - pts)?;
        sample
    };
    Ok(sample)
}

/// Create an NV12 `ID3D11Texture2D` with the given usage/CPU-access.
fn create_nv12_texture(
    device: &ID3D11Device,
    usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE,
    cpu_access: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: WIDTH,
        Height: HEIGHT,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: D3D11_BIND_FLAG(0).0 as u32,
        CPUAccessFlags: cpu_access,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    // SAFETY: CreateTexture2D writes the out-param on success.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.ok_or_else(|| windows::core::Error::from(E_FAIL))
}

/// Fill the staging NV12 texture with a synthetic moving-bars pattern: a luma
/// (Y) gradient that scrolls with `index` so frame-stepping shows motion, and
/// neutral grey chroma (UV = 128) — colour correctness is a Milestone-1
/// concern, not this spike's.
fn fill_synthetic_nv12(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    index: u32,
) -> Result<()> {
    // SAFETY: Map a staging resource for CPU write, fill both NV12 planes using
    // the driver-returned row pitch, then Unmap.
    unsafe {
        let resource: ID3D11Resource = staging.cast()?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&resource, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))?;

        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData as *mut u8;
        let shift = (index * 4) % WIDTH;

        // Y plane: HEIGHT rows of WIDTH luma samples.
        for y in 0..HEIGHT as usize {
            let row = base.add(y * pitch);
            for x in 0..WIDTH as usize {
                // Scrolling vertical gradient (0..255 across width, offset by frame).
                let v = (((x as u32 + shift) * 256 / WIDTH) & 0xFF) as u8;
                *row.add(x) = v;
            }
        }
        // UV plane: begins at base + pitch*HEIGHT, HEIGHT/2 rows of interleaved
        // U,V. Neutral grey = 128 for both.
        let uv_base = base.add(pitch * HEIGHT as usize);
        for y in 0..(HEIGHT as usize / 2) {
            let row = uv_base.add(y * pitch);
            for x in 0..WIDTH as usize {
                *row.add(x) = 128;
            }
        }

        context.Unmap(&resource, 0);
    }
    Ok(())
}
