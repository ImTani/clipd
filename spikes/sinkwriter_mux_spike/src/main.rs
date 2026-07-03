//! Milestone-0 spike #4 — **MF Sink Writer viability** (the mux decision).
//!
//! 01-PROJECT-PLAN §5.2 / tracker M0 #4: "can you feed [the MF Sink Writer]
//! pre-encoded samples with your timestamps without it re-encoding or fighting
//! you? If yes, use it for v1. If it fights, commit to hand-rolled fMP4."
//!
//! This reuses spike #1's NVENC path to produce H.264 `IMFSample`s, but instead
//! of writing a raw `.h264`, it feeds each encoded sample **in passthrough**
//! (sink-writer input media type == output media type == H.264, so no encoder
//! MFT is inserted) into an `IMFSinkWriter` → `.mp4`. The encoded samples carry
//! the QPC-grid timestamps that originated on our NV12 input frames, so this is
//! literally "feed pre-encoded samples with our timestamps."
//!
//! ## The decision this informs
//! `02-AV-SYNC-SPEC §4` is frozen around **hand-rolled fragmented MP4** for
//! crash-safety (moof/mdat per 1 s, atomic rename). This spike measures whether
//! the Sink Writer is a viable *alternative*: does it accept our pre-encoded
//! H.264 + timestamps and mux a valid, correctly-timed MP4? The finding
//! (viable-but-still-not-chosen, or not-viable) goes in DECISIONS.md.
//!
//! Throwaway, standalone crate; never linked into `clipd`.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use tracing::{error, info, warn};
use windows::core::{Interface, Result, HSTRING};
use windows::Win32::Foundation::{E_FAIL, HMODULE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource,
    ID3D11Texture2D, D3D11_BIND_FLAG, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::eAVEncH264VProfile_Main;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaEventGenerator, IMFMediaType,
    IMFSample, IMFSinkWriter, IMFTransform, METransformDrainComplete, METransformHaveOutput,
    METransformNeedInput, MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateMediaType, MFCreateSample, MFCreateSinkWriterFromURL, MFMediaType_Video, MFShutdown,
    MFStartup, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_SINK_WRITER_DISABLE_THROTTLING, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FPS: u32 = 60;
const FRAME_COUNT: u32 = 120;
const BITRATE_BPS: u32 = 8_000_000;
const TICKS_PER_SECOND: i64 = 10_000_000;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match run() {
        Ok(path) => {
            info!(output = %path.display(), "spike OK — MP4 muxed via Sink Writer");
            info!(
                "VALIDATE: ffprobe the .mp4 (see spike README) — frame count, CFR, no re-encode."
            );
        }
        Err(e) => {
            error!(error = %e, hresult = format!("0x{:08X}", e.code().0 as u32), "spike FAILED");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<PathBuf> {
    // SAFETY: MTA init; ignoring S_FALSE (already-init).
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    // SAFETY: MFStartup pairs with MFShutdown below.
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };
    let result = encode_and_mux();
    // SAFETY: balance startup calls.
    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
    result
}

fn encode_and_mux() -> Result<PathBuf> {
    let (device, context) = create_d3d11_device()?;
    // SAFETY: multithread protection for the shared device (async MFT worker).
    let multithread: ID3D11Multithread = context.cast()?;
    let _ = unsafe { multithread.SetMultithreadProtected(true) };

    let manager = create_device_manager(&device)?;
    let transform = activate_h264_encoder()?;
    configure_encoder(&transform, &manager)?;

    // SAFETY: begin streaming so the encoder finalizes its output type (incl. the
    // MF_MT_MPEG_SEQUENCE_HEADER the MP4 avcC box needs).
    unsafe {
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
    }
    // SAFETY: read the encoder's negotiated H.264 output type.
    let h264_type = unsafe { transform.GetOutputCurrentType(0)? };

    let out_path = std::env::temp_dir().join("clipd_spike_sinkwriter.mp4");
    let (sink, stream_idx) = create_sink_writer(&out_path, &h264_type)?;
    // SAFETY: BeginWriting before the first WriteSample.
    unsafe { sink.BeginWriting()? };

    let frames_out = run_encode_loop(&transform, &device, &context, &sink, stream_idx)?;

    // SAFETY: Finalize flushes the moov/mdat and closes the file.
    unsafe { sink.Finalize()? };
    info!(frames_out, "sink writer finalized");
    Ok(out_path)
}

/// Create the Sink Writer for an `.mp4` and add our H.264 stream in **passthrough**
/// (input type == output type ⇒ the writer inserts no encoder).
fn create_sink_writer(path: &Path, h264_type: &IMFMediaType) -> Result<(IMFSinkWriter, u32)> {
    // SAFETY: standard Sink Writer construction; DISABLE_THROTTLING lets us push
    // samples as fast as we produce them.
    unsafe {
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;

        let url = HSTRING::from(path.to_string_lossy().as_ref());
        let sink = MFCreateSinkWriterFromURL(&url, None, &attrs)?;

        // Target (container) type = our encoded H.264 type → AddStream returns its
        // index; SetInputMediaType with the SAME type = passthrough.
        let stream_idx = sink.AddStream(h264_type)?;
        sink.SetInputMediaType(stream_idx, h264_type, None)?;
        info!(stream_idx, "sink writer stream added (passthrough H.264)");
        Ok((sink, stream_idx))
    }
}

/// Async encode loop — identical to spike #1 except each output H.264 sample is
/// handed to the Sink Writer instead of written raw.
fn run_encode_loop(
    transform: &IMFTransform,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    sink: &IMFSinkWriter,
    stream_idx: u32,
) -> Result<u32> {
    let provides_samples = output_provides_samples(transform)?;
    let staging =
        create_nv12_texture(device, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_WRITE.0 as u32)?;
    let event_gen: IMFMediaEventGenerator = transform.cast()?;

    let mut frames_in: u32 = 0;
    let mut frames_out: u32 = 0;
    let mut draining = false;

    loop {
        // SAFETY: block for the next transform event.
        let event = unsafe { event_gen.GetEvent(Default::default())? };
        // SAFETY: read the event type code.
        let event_type = unsafe { event.GetType()? };
        match event_type {
            t if t == METransformNeedInput.0 as u32 => {
                if frames_in < FRAME_COUNT {
                    let sample = build_input_sample(device, context, &staging, frames_in)?;
                    // SAFETY: feed one input per NeedInput.
                    unsafe { transform.ProcessInput(0, &sample, 0)? };
                    frames_in += 1;
                } else if !draining {
                    // SAFETY: end-of-stream + drain to flush the tail.
                    unsafe {
                        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
                        transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
                    }
                    draining = true;
                }
            }
            t if t == METransformHaveOutput.0 as u32 => {
                let sample = pull_output(transform, provides_samples)?;
                // Feed the pre-encoded sample straight into the muxer — its
                // sample time is the encoder-propagated QPC-grid time from our
                // input frame. This is the passthrough the decision hinges on.
                // SAFETY: WriteSample muxes the sample using its own timestamp.
                unsafe { sink.WriteSample(stream_idx, &sample)? };
                frames_out += 1;
            }
            t if t == METransformDrainComplete.0 as u32 => break,
            other => warn!(event_type = other, "ignoring unexpected transform event"),
        }
    }
    if frames_out != FRAME_COUNT {
        warn!(frames_in, frames_out, "output count != input count");
    }
    Ok(frames_out)
}

fn pull_output(transform: &IMFTransform, provides_samples: bool) -> Result<IMFSample> {
    let mut output = MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        ..Default::default()
    };
    if !provides_samples {
        // SAFETY: allocate an output sample if the MFT does not provide one.
        let info = unsafe { transform.GetOutputStreamInfo(0)? };
        let sample = unsafe { MFCreateSample()? };
        let buffer = unsafe {
            windows::Win32::Media::MediaFoundation::MFCreateMemoryBuffer(info.cbSize.max(1))?
        };
        unsafe { sample.AddBuffer(&buffer)? };
        output.pSample = std::mem::ManuallyDrop::new(Some(sample));
    }
    let mut status = 0u32;
    // SAFETY: pull one encoded sample.
    unsafe { transform.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)? };
    // SAFETY: pSample is valid after a successful ProcessOutput.
    let sample = unsafe {
        std::mem::ManuallyDrop::take(&mut output.pSample)
            .ok_or_else(|| windows::core::Error::from(E_FAIL))?
    };
    // SAFETY: release any attached events collection.
    unsafe {
        let _ = std::mem::ManuallyDrop::take(&mut output.pEvents);
    }
    Ok(sample)
}

// ─── The rest is spike #1's encoder/D3D/NV12 machinery, unchanged. ───────────

fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut device = None;
    let mut context = None;
    // SAFETY: standard device creation; out-params written on S_OK.
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
    let device = device.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
    log_adapter(&device)?;
    Ok((
        device,
        context.ok_or_else(|| windows::core::Error::from(E_FAIL))?,
    ))
}

fn log_adapter(device: &ID3D11Device) -> Result<()> {
    // SAFETY: read-only adapter query.
    unsafe {
        let dxgi: IDXGIDevice = device.cast()?;
        let desc = dxgi.GetAdapter()?.GetDesc()?;
        let end = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        info!(adapter = %String::from_utf16_lossy(&desc.Description[..end]), "D3D11 device");
    }
    Ok(())
}

fn create_device_manager(device: &ID3D11Device) -> Result<IMFDXGIDeviceManager> {
    let mut token = 0u32;
    let mut manager = None;
    // SAFETY: create + bind the device manager.
    unsafe {
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        manager.ResetDevice(device, token)?;
        Ok(manager)
    }
}

fn activate_h264_encoder() -> Result<IMFTransform> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx allocates a CoTaskMem array we own.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut arr,
            &mut count,
        )?;
    }
    if count == 0 || arr.is_null() {
        error!("no hardware H.264 encoder MFT found");
        return Err(windows::core::Error::from(E_FAIL));
    }
    // SAFETY: take element 0, drop the rest, free the block.
    let transform = unsafe {
        let slice = std::slice::from_raw_parts(arr, count as usize);
        let first = slice[0]
            .clone()
            .ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        for e in slice.iter() {
            std::ptr::drop_in_place(e as *const Option<IMFActivate> as *mut Option<IMFActivate>);
        }
        CoTaskMemFree(Some(arr as *const c_void));
        first.ActivateObject::<IMFTransform>()?
    };
    Ok(transform)
}

fn configure_encoder(transform: &IMFTransform, manager: &IMFDXGIDeviceManager) -> Result<()> {
    // SAFETY: all calls on the freshly activated transform.
    unsafe {
        transform
            .GetAttributes()?
            .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
        transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, BITRATE_BPS)?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
        set_size_rate_par(&out)?;
        transform.SetOutputType(0, &out, 0)?;

        let inp = MFCreateMediaType()?;
        inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        set_size_rate_par(&inp)?;
        transform.SetInputType(0, &inp, 0)?;
    }
    info!(
        width = WIDTH,
        height = HEIGHT,
        fps = FPS,
        "encoder configured"
    );
    Ok(())
}

fn set_size_rate_par(mt: &IMFMediaType) -> Result<()> {
    // SAFETY: pack MF's u64 2-D attributes by hand (helpers not in the crate).
    unsafe {
        mt.SetUINT64(&MF_MT_FRAME_SIZE, ((WIDTH as u64) << 32) | HEIGHT as u64)?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, ((FPS as u64) << 32) | 1)?;
        mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
    }
    Ok(())
}

fn output_provides_samples(transform: &IMFTransform) -> Result<bool> {
    // SAFETY: read-only stream-info query.
    let info = unsafe { transform.GetOutputStreamInfo(0)? };
    let mask =
        (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
    Ok(info.dwFlags & mask != 0)
}

fn build_input_sample(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    index: u32,
) -> Result<IMFSample> {
    let input_tex = create_nv12_texture(device, D3D11_USAGE_DEFAULT, 0)?;
    fill_synthetic_nv12(context, staging, index)?;
    // SAFETY: copy staging → encoder input, wrap as an MF sample with grid time.
    let sample = unsafe {
        let src: ID3D11Resource = staging.cast()?;
        let dst: ID3D11Resource = input_tex.cast()?;
        context.CopyResource(&dst, &src);

        let buffer: IMFMediaBuffer =
            MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &input_tex, 0, false)?;
        let two_d: windows::Win32::Media::MediaFoundation::IMF2DBuffer = buffer.cast()?;
        buffer.SetCurrentLength(two_d.GetContiguousLength()?)?;

        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        // 02-AV-SYNC-SPEC §1.2 CFR grid: n*10^7/fps, integer each time.
        let pts = (index as i64 * TICKS_PER_SECOND) / FPS as i64;
        let next = ((index as i64 + 1) * TICKS_PER_SECOND) / FPS as i64;
        sample.SetSampleTime(pts)?;
        sample.SetSampleDuration(next - pts)?;
        sample
    };
    Ok(sample)
}

fn create_nv12_texture(
    device: &ID3D11Device,
    usage: D3D11_USAGE,
    cpu: u32,
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
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    };
    let mut tex = None;
    // SAFETY: CreateTexture2D writes the out-param on success.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.ok_or_else(|| windows::core::Error::from(E_FAIL))
}

fn fill_synthetic_nv12(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    index: u32,
) -> Result<()> {
    // SAFETY: map staging, write both NV12 planes at the driver row pitch, unmap.
    unsafe {
        let resource: ID3D11Resource = staging.cast()?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&resource, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))?;
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData as *mut u8;
        let shift = (index * 4) % WIDTH;
        for y in 0..HEIGHT as usize {
            let row = base.add(y * pitch);
            for x in 0..WIDTH as usize {
                *row.add(x) = (((x as u32 + shift) * 256 / WIDTH) & 0xFF) as u8;
            }
        }
        let uv = base.add(pitch * HEIGHT as usize);
        for y in 0..(HEIGHT as usize / 2) {
            let row = uv.add(y * pitch);
            for x in 0..WIDTH as usize {
                *row.add(x) = 128;
            }
        }
        context.Unmap(&resource, 0);
    }
    Ok(())
}
