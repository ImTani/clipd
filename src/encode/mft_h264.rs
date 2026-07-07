//! `encode::mft_h264` — the asynchronous hardware H.264 encoder.
//!
//! Cannibalized from Milestone-0 spike #1 (the "two weeks of pain" component,
//! plan pitfall 17). Drives the async Media Foundation Transform event state
//! machine (`METransformNeedInput`/`METransformHaveOutput`/`…DrainComplete`) with
//! an `IMFDXGIDeviceManager` bound to the **shared** D3D11 device, so it takes
//! GPU-resident NV12 textures straight from the video processor — no system-RAM
//! copy (`CLAUDE.md` rule 6).
//!
//! ## What changed from the spike (Milestone 1)
//! - Feeds the **real** NV12 texture from [`crate::capture::convert`], not a
//!   synthetic pattern.
//! - **Bitrate-target rate control** (§6.1 amendment, DECISIONS 2026-07-07):
//!   PeakConstrainedVBR at the §6.2 table bitrate (average +
//!   [`crate::spec_constants::encoder::video_target_bitrate_bps`], peak = 1.5×).
//!   The original §6.1 CQP design is unreachable on the NVENC MFT — measured (T0,
//!   M7-M8-PLAN §1): `AVEncVideoEncodeQP` is rejected and `AVEncCommonQuality` is
//!   a no-op, so the bitrate target is the only lever. Closed GOP = 2 s IDR
//!   interval, no B-frames (spec §3).
//! - **BT.709 limited-range VUI tags** on the output type so a player
//!   reconstructs the same primaries/matrix/range the video processor produced
//!   (the other half of "correct colours").
//!
//! ## Threading
//! The encoder is driven by [`H264Encoder::run`], a blocking event loop meant to
//! own the **encode thread**. It is not `Send`; create it on the thread that runs
//! it (in the MTA). [`InputFrame`] carries a `SAFETY`-justified `unsafe impl
//! Send` so a captured NV12 texture can be handed to that thread over a channel.

use std::ffi::c_void;
use std::sync::Arc;

use tracing::{info, warn};
use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncCommonRateControlMode_GlobalVBR,
    eAVEncCommonRateControlMode_LowDelayVBR, eAVEncCommonRateControlMode_PeakConstrainedVBR,
    eAVEncCommonRateControlMode_Quality, eAVEncCommonRateControlMode_UnconstrainedVBR,
    eAVEncH264VProfile_Main, ICodecAPI, IMF2DBuffer, IMFActivate, IMFDXGIDeviceManager,
    IMFMediaBuffer, IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFTransform,
    METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFNominalRange_16_235, MFSampleExtension_CleanPoint, MFTEnumEx,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709,
    MFVideoTransFunc_709, MFVideoTransferMatrix_BT709, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES,
    MF_MT_YUV_MATRIX, MF_TRANSFORM_ASYNC_UNLOCK,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_UI4, VT_UI8,
};

// CODECAPI property GUIDs are exposed by the crate under MediaFoundation.
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonQuality,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoEncodeQP,
};

use crate::gpu::GpuContext;

/// Errors from encoder setup or the encode loop.
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    /// No hardware H.264 encoder MFT (NV12→H.264) was found.
    #[error("no hardware H.264 encoder MFT found (NV12 -> H.264)")]
    NoEncoder,
    /// A Media Foundation / Direct3D call failed.
    #[error("Media Foundation call failed: {0}")]
    Windows(#[from] windows::core::Error),
    /// `ProcessOutput` produced no sample.
    #[error("encoder produced no output sample")]
    NoOutput,
}

/// Encoder configuration derived from the config + spec constants.
#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Output frame rate (the CFR grid rate).
    pub fps: u32,
    /// Constant quality / QP (spec §6.1 — NVENC CQ 23 at 1080p60).
    pub cq: u32,
    /// Closed-GOP IDR interval in frames (spec §3 — `2·fps`).
    pub gop_frames: u32,
    /// Shipping average VIDEO bitrate target (bits/s) for PeakConstrainedVBR
    /// (`spec_constants::encoder::video_target_bitrate_bps`). Per the 2026-07-07
    /// §6.1 amendment this — not CQ — is the lever that controls output. Ignored
    /// when [`EncoderOverrides`] carry an explicit rate-control intent.
    pub target_bitrate_bps: u32,
    /// Shipping peak VIDEO bitrate cap (bits/s) for PeakConstrainedVBR = 1.5×
    /// [`Self::target_bitrate_bps`] (`video_peak_bitrate_bps`). Keeps the stream
    /// inside the ring byte cap (§6.1 amendment).
    pub peak_bitrate_bps: u32,
    /// T0 calibration-probe overrides (M7-M8-PLAN §1). All-`None` (`Default`) in
    /// shipping config, where the bitrate-target path below runs. Hidden test hooks.
    pub overrides: EncoderOverrides,
}

/// Rate-control probe overrides for the T0 encoder-quality calibration
/// (M7-M8-PLAN §1). Every field is a hidden `--encode-*` test hook; all-`None`
/// ([`Default`]) reproduces the shipping derived-quality path. The probe exists
/// because true CQP (`AVEncVideoEncodeQP`) is rejected by the NVENC MFT and its
/// `AVEncCommonQuality` knob was measured to be a no-op — so these let a single
/// unattended sweep test every viable rate-control mode without recompiling.
#[derive(Debug, Clone, Copy, Default)]
pub struct EncoderOverrides {
    /// `eAVEncCommonRateControlMode` raw value (via [`rc_mode_from_str`]). `None`
    /// = the shipping default (`Quality`).
    pub rc_mode: Option<u32>,
    /// `AVEncCommonQuality` (0–100). `None` = derive from [`EncoderConfig::cq`].
    pub quality: Option<u32>,
    /// `AVEncVideoEncodeQP` constant QP (packed I=P=B). `None` = don't attempt.
    /// Logged as accepted/rejected so the probe learns if true CQP is available.
    pub qp: Option<u32>,
    /// Average target: `MF_MT_AVG_BITRATE` + `AVEncCommonMeanBitRate` (bits/s).
    pub avg_bitrate_bps: Option<u32>,
    /// Peak cap: `AVEncCommonMaxBitRate` (bits/s), for peak-constrained VBR.
    pub max_bitrate_bps: Option<u32>,
}

impl EncoderOverrides {
    /// True when no probe override is set — the shipping bitrate-target path runs.
    fn is_default(&self) -> bool {
        self.rc_mode.is_none()
            && self.quality.is_none()
            && self.qp.is_none()
            && self.avg_bitrate_bps.is_none()
            && self.max_bitrate_bps.is_none()
    }
}

/// Map a rate-control mode name to its `eAVEncCommonRateControlMode` raw value.
/// Names match the enum: `cbr`, `pcvbr`, `uvbr`, `quality`, `ldvbr`, `gvbr`.
/// Returns `None` for an unknown name (caller keeps the shipping default).
pub fn rc_mode_from_str(name: &str) -> Option<u32> {
    let mode = match name.to_ascii_lowercase().as_str() {
        "cbr" => eAVEncCommonRateControlMode_CBR,
        "pcvbr" => eAVEncCommonRateControlMode_PeakConstrainedVBR,
        "uvbr" => eAVEncCommonRateControlMode_UnconstrainedVBR,
        "quality" => eAVEncCommonRateControlMode_Quality,
        "ldvbr" => eAVEncCommonRateControlMode_LowDelayVBR,
        "gvbr" => eAVEncCommonRateControlMode_GlobalVBR,
        _ => return None,
    };
    Some(mode.0 as u32)
}

/// One NV12 frame to encode, with its CFR grid PTS.
pub struct InputFrame {
    /// NV12 texture on the shared device (from the video processor).
    pub texture: ID3D11Texture2D,
    /// Presentation timestamp in ticks (the slot boundary from the pacing grid).
    pub pts: i64,
    /// Frame duration in ticks.
    pub duration: i64,
    /// Epoch this frame belongs to (`02-AV-SYNC-SPEC §0`).
    pub epoch_id: u32,
}

// SAFETY: the NV12 texture is a shared-device (multithread-protected) resource;
// an `InputFrame` is handed from the capture thread to the encode thread by
// ownership transfer over a channel, never aliased mutably across threads. Both
// threads are in the MTA (see `crate::com`).
unsafe impl Send for InputFrame {}

/// One encoded H.264 access unit.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    /// Encoded bytes (Annex-B for the raw stream; the muxer strips/repackages).
    /// `Arc<[u8]>` so the M3 ring can retain packets and a save can snapshot a
    /// window by cloning handles — no bulk byte copy (`02-AV-SYNC-SPEC.md §3`;
    /// the RAM budget in `01-PROJECT-PLAN.md §1`).
    pub data: Arc<[u8]>,
    /// Presentation timestamp in ticks (propagated from the input).
    pub pts: i64,
    /// Frame duration in ticks.
    pub duration: i64,
    /// Whether this access unit is an IDR/keyframe (a clean seek point).
    pub is_keyframe: bool,
    /// Epoch this packet belongs to.
    pub epoch_id: u32,
}

/// The asynchronous hardware H.264 encoder.
pub struct H264Encoder {
    transform: IMFTransform,
    event_gen: IMFMediaEventGenerator,
    /// Kept alive for the encoder's lifetime (binds the shared device).
    _manager: IMFDXGIDeviceManager,
    provides_samples: bool,
    /// Epoch stamped onto outputs — tracks the most recently submitted input.
    current_epoch: u32,
}

impl H264Encoder {
    /// Activate a hardware H.264 encoder on the shared device and configure it
    /// for CQP with BT.709-limited output.
    pub fn new(gpu: &GpuContext, config: EncoderConfig) -> Result<Self, EncodeError> {
        let manager = create_device_manager(gpu)?;
        let transform = activate_h264_encoder()?;
        configure_encoder(&transform, &manager, &config)?;

        let event_gen: IMFMediaEventGenerator = transform.cast()?;
        let provides_samples = output_provides_samples(&transform)?;

        Ok(Self {
            transform,
            event_gen,
            _manager: manager,
            provides_samples,
            current_epoch: 0,
        })
    }

    /// Begin streaming. Must be called before [`Self::output_media_type`] or
    /// [`Self::pump`].
    pub fn begin(&self) -> Result<(), EncodeError> {
        // SAFETY: begin/start streaming on the configured transform.
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(())
    }

    /// The negotiated output media type — valid after [`Self::begin`]. It carries
    /// `MF_MT_MPEG_SEQUENCE_HEADER` (SPS/PPS), which the muxer needs for the MP4
    /// `avcC` box (spike #4 finding).
    pub fn output_media_type(&self) -> Result<IMFMediaType, EncodeError> {
        // SAFETY: read the negotiated output type after streaming has begun.
        Ok(unsafe { self.transform.GetOutputCurrentType(0)? })
    }

    /// Convenience: [`Self::begin`] then [`Self::pump`] (used by the standalone
    /// `encode-probe`, which does not need the output media type).
    pub fn run<S, K>(&mut self, next_input: S, on_packet: K) -> Result<(), EncodeError>
    where
        S: FnMut() -> Option<InputFrame>,
        K: FnMut(EncodedPacket),
    {
        self.begin()?;
        self.pump(next_input, on_packet)
    }

    /// Run the async encode loop until the source is exhausted and the encoder
    /// drains ([`Self::begin`] must have been called). `next_input` is called on
    /// `NeedInput` (returning `None` ends the stream); `on_packet` receives each
    /// encoded packet in order.
    pub fn pump<S, K>(&mut self, mut next_input: S, mut on_packet: K) -> Result<(), EncodeError>
    where
        S: FnMut() -> Option<InputFrame>,
        K: FnMut(EncodedPacket),
    {
        let mut draining = false;
        loop {
            // SAFETY: blocks until the next transform event or shutdown.
            let event = unsafe { self.event_gen.GetEvent(Default::default())? };
            let event_type = unsafe { event.GetType()? };

            match event_type {
                t if t == METransformNeedInput.0 as u32 => {
                    if draining {
                        continue;
                    }
                    match next_input() {
                        Some(frame) => {
                            self.current_epoch = frame.epoch_id;
                            let sample = wrap_nv12_sample(&frame)?;
                            // SAFETY: one input per NeedInput, as the async contract requires.
                            unsafe { self.transform.ProcessInput(0, &sample, 0)? };
                        }
                        None => {
                            // SAFETY: end-of-stream then drain flushes tail outputs.
                            unsafe {
                                self.transform
                                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
                                self.transform
                                    .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
                            }
                            draining = true;
                        }
                    }
                }
                t if t == METransformHaveOutput.0 as u32 => {
                    let packet = self.pull_output()?;
                    on_packet(packet);
                }
                t if t == METransformDrainComplete.0 as u32 => break,
                other => warn!(event_type = other, "ignoring unexpected transform event"),
            }
        }
        Ok(())
    }

    /// Pull exactly one encoded packet in response to `HaveOutput`.
    fn pull_output(&self) -> Result<EncodedPacket, EncodeError> {
        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            ..Default::default()
        };

        if !self.provides_samples {
            // SAFETY: supply an output sample sized to the stream info.
            let info = unsafe { self.transform.GetOutputStreamInfo(0)? };
            let sample = unsafe { MFCreateSample()? };
            let buffer = unsafe { MFCreateMemoryBuffer(info.cbSize.max(1))? };
            unsafe { sample.AddBuffer(&buffer)? };
            output.pSample = std::mem::ManuallyDrop::new(Some(sample));
        }

        let mut status = 0u32;
        // SAFETY: fills the single output buffer; on PROVIDES_SAMPLES the MFT sets
        // pSample for us.
        unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)?;
        }

        // SAFETY: after a successful ProcessOutput pSample is a valid owned sample.
        let sample: IMFSample = unsafe {
            std::mem::ManuallyDrop::take(&mut output.pSample).ok_or(EncodeError::NoOutput)?
        };
        // SAFETY: release any events collection the MFT attached.
        unsafe {
            let _ = std::mem::ManuallyDrop::take(&mut output.pEvents);
        }

        // SAFETY: read metadata then flatten+copy the encoded bytes.
        unsafe {
            let pts = sample.GetSampleTime().unwrap_or(0);
            let duration = sample.GetSampleDuration().unwrap_or(0);
            let is_keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;

            let buffer: IMFMediaBuffer = sample.ConvertToContiguousBuffer()?;
            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut cur_len = 0u32;
            buffer.Lock(&mut data_ptr, None, Some(&mut cur_len))?;
            // `&[u8] → Arc<[u8]>` copies the bytes into the Arc's allocation while
            // the buffer is still locked (one copy, same as the prior `to_vec`).
            let data: Arc<[u8]> = std::slice::from_raw_parts(data_ptr, cur_len as usize).into();
            buffer.Unlock()?;

            Ok(EncodedPacket {
                data,
                pts,
                duration,
                is_keyframe,
                epoch_id: self.current_epoch,
            })
        }
    }
}

/// Wrap an NV12 texture as a timestamped MF sample (no pixel copy).
fn wrap_nv12_sample(frame: &InputFrame) -> Result<IMFSample, EncodeError> {
    // SAFETY: wrap the GPU texture as a DXGI surface buffer, fix its length (DXGI
    // buffers start at 0), and stamp the CFR grid PTS/duration.
    unsafe {
        let buffer: IMFMediaBuffer =
            MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &frame.texture, 0, false)?;
        let two_d: IMF2DBuffer = buffer.cast()?;
        buffer.SetCurrentLength(two_d.GetContiguousLength()?)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(frame.pts)?;
        sample.SetSampleDuration(frame.duration)?;
        Ok(sample)
    }
}

/// Bind the shared D3D11 device to an `IMFDXGIDeviceManager`.
fn create_device_manager(gpu: &GpuContext) -> Result<IMFDXGIDeviceManager, EncodeError> {
    let mut reset_token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    // SAFETY: out-params written on success; ResetDevice binds our shared device
    // using the token just handed back.
    unsafe {
        MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)?;
        let manager = manager.ok_or(EncodeError::NoOutput)?;
        manager.ResetDevice(gpu.device(), reset_token)?;
        Ok(manager)
    }
}

// Re-exported here to keep the device-manager helper self-contained.
use windows::Win32::Media::MediaFoundation::MFCreateDXGIDeviceManager;

/// Enumerate hardware NV12→H.264 encoder MFTs and activate the first.
fn activate_h264_encoder() -> Result<IMFTransform, EncodeError> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    let mut activate_arr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx allocates a CoTaskMem array of IMFActivate*; we own it and
    // free it below.
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
        return Err(EncodeError::NoEncoder);
    }

    // SAFETY: `activate_arr` points to `count` initialized entries. Take element 0,
    // drop the rest (Release), then free the CoTaskMem block.
    let transform = unsafe {
        let slice = std::slice::from_raw_parts(activate_arr, count as usize);
        let first = slice[0].clone().ok_or(EncodeError::NoEncoder)?;
        for entry in slice.iter() {
            std::ptr::drop_in_place(
                entry as *const Option<IMFActivate> as *mut Option<IMFActivate>,
            );
        }
        CoTaskMemFree(Some(activate_arr as *const c_void));
        first.ActivateObject::<IMFTransform>()?
    };
    Ok(transform)
}

/// Unlock async, bind the D3D manager, negotiate output-then-input types (with
/// BT.709-limited VUI), and apply CQP rate control via `ICodecAPI`.
fn configure_encoder(
    transform: &IMFTransform,
    manager: &IMFDXGIDeviceManager,
    config: &EncoderConfig,
) -> Result<(), EncodeError> {
    // SAFETY: all calls below are on the freshly activated transform.
    unsafe {
        let attrs = transform.GetAttributes()?;
        attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
        transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

        // Effective rate control (§6.1 amendment, DECISIONS 2026-07-07). CQP is
        // unreachable on this NVENC MFT (AVEncVideoEncodeQP rejected,
        // AVEncCommonQuality a measured no-op — T0), so the shipping path (no probe
        // overrides) targets a bitrate via PeakConstrainedVBR. Probe overrides from
        // the T0 harness take precedence and suppress the shipping defaults so a
        // knob can be tested in isolation.
        let ov = config.overrides;
        let shipping = ov.is_default();
        let rc_mode = ov.rc_mode.unwrap_or(if shipping {
            eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32
        } else {
            eAVEncCommonRateControlMode_Quality.0 as u32
        });
        let avg_bitrate = ov
            .avg_bitrate_bps
            .or(shipping.then_some(config.target_bitrate_bps));
        let peak_bitrate = ov
            .max_bitrate_bps
            .or(shipping.then_some(config.peak_bitrate_bps));

        // Output type first (H.264 encoders require it). The average-bitrate target
        // is stamped on it below; the CODECAPI mean/max + mode go on the transform.
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
        // BT.709 limited-range VUI — must match the video processor's output so a
        // player reconstructs the same colours (the other half of "correct colours").
        out.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        out.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        out.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        out.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        set_frame_size(&out, config)?;
        set_frame_rate(&out, config)?;
        set_pixel_aspect_ratio(&out)?;
        // Average-bitrate target on the output type. Measured to dominate output
        // on this NVENC MFT (60M target → 60.4 Mbps); set alongside the CODECAPI
        // mean below for redundancy across driver revisions.
        if let Some(bps) = avg_bitrate {
            out.SetUINT32(&MF_MT_AVG_BITRATE, bps)?;
        }
        transform.SetOutputType(0, &out, 0)?;

        // Input type second.
        let inp = MFCreateMediaType()?;
        inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        set_frame_size(&inp, config)?;
        set_frame_rate(&inp, config)?;
        set_pixel_aspect_ratio(&inp)?;
        transform.SetInputType(0, &inp, 0)?;

        // Rate control + GOP via ICodecAPI. Each is best-effort: vendors vary in
        // which properties they honour (plan pitfall 18) — a rejected property is
        // logged, not fatal. NVENC-via-MF quirks measured on the RTX 4050 (T0,
        // M7-M8-PLAN §1): AVEncCommonQuality is accepted but a no-op; only
        // MF_MT_AVG_BITRATE / AVEncCommonMeanBitRate actually move output.
        if let Ok(codec) = transform.cast::<ICodecAPI>() {
            // rc_mode / avg_bitrate / peak_bitrate were resolved above (shipping vs
            // probe). Apply the mode here on the transform.
            set_codec_ui4(&codec, &CODECAPI_AVEncCommonRateControlMode, rc_mode);

            // Quality knob: a measured no-op on this MFT (kept only so an explicit
            // probe value is still applied); shipping leaves it at the CQ-derived
            // value for provenance. Real control comes from the bitrate target.
            let derived_quality = 100u32.saturating_sub(config.cq.min(51) * 100 / 51);
            let common_quality = ov.quality.unwrap_or(derived_quality);
            set_codec_ui4(&codec, &CODECAPI_AVEncCommonQuality, common_quality);

            // True CQP probe: AVEncVideoEncodeQP (constant QP, same for I/P/B). This
            // MFT rejects it (confirmed, T0) — log the outcome explicitly.
            let qp_status = match ov.qp {
                Some(qp) => {
                    let packed = pack_qp(qp);
                    match set_codec_ui8_checked(&codec, &CODECAPI_AVEncVideoEncodeQP, packed) {
                        Ok(()) => "accepted",
                        Err(_) => "rejected",
                    }
                }
                None => "unset",
            };

            // Bitrate target/cap: mean drives the VBR average; max caps the peak for
            // PeakConstrainedVBR. Redundant with MF_MT_AVG_BITRATE on the output type.
            if let Some(bps) = avg_bitrate {
                set_codec_ui4(&codec, &CODECAPI_AVEncCommonMeanBitRate, bps);
            }
            if let Some(bps) = peak_bitrate {
                set_codec_ui4(&codec, &CODECAPI_AVEncCommonMaxBitRate, bps);
            }

            set_codec_ui4(&codec, &CODECAPI_AVEncMPVGOPSize, config.gop_frames);
            // Self-documenting log so a probe run can correlate each saved clip to
            // the exact encoder settings that produced it (M7-M8-PLAN §1 / T0).
            info!(
                cq = config.cq,
                shipping,
                rc_mode,
                applied_quality = common_quality,
                qp = ?ov.qp,
                qp_status,
                avg_bitrate_bps = ?avg_bitrate,
                peak_bitrate_bps = ?peak_bitrate,
                gop_frames = config.gop_frames,
                "H.264 encoder configured"
            );
            // No B-frames (spec §3): NVENC-via-MF defaults to 0 B-frames (verified
            // has_b_frames=0); the explicit AVEncMPVDefaultBPictureCount property is
            // rejected by this MFT, so we rely on the default.
        } else {
            warn!("encoder MFT has no ICodecAPI; rate control/GOP not applied");
        }
    }
    Ok(())
}

/// Does the MFT allocate its own output samples (hardware encoders usually do)?
fn output_provides_samples(transform: &IMFTransform) -> Result<bool, EncodeError> {
    // SAFETY: read-only stream-info query on the configured transform.
    let info = unsafe { transform.GetOutputStreamInfo(0)? };
    let mask =
        (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
    Ok(info.dwFlags & mask != 0)
}

// MF packs 2-D attributes as a single u64 (hi:lo); the C-header helpers are not
// exposed by the `windows` crate, so pack by hand (spike #1 finding).
fn set_frame_size(mt: &IMFMediaType, c: &EncoderConfig) -> Result<(), EncodeError> {
    // SAFETY: attribute setter on a valid media type.
    unsafe {
        mt.SetUINT64(
            &MF_MT_FRAME_SIZE,
            ((c.width as u64) << 32) | c.height as u64,
        )?
    };
    Ok(())
}
fn set_frame_rate(mt: &IMFMediaType, c: &EncoderConfig) -> Result<(), EncodeError> {
    // SAFETY: attribute setter; rate is fps/1.
    unsafe { mt.SetUINT64(&MF_MT_FRAME_RATE, ((c.fps as u64) << 32) | 1)? };
    Ok(())
}
fn set_pixel_aspect_ratio(mt: &IMFMediaType) -> Result<(), EncodeError> {
    // SAFETY: attribute setter; square pixels 1:1.
    unsafe { mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)? };
    Ok(())
}

/// Set an `ICodecAPI` property from a `u32` (VT_UI4), logging on rejection.
///
/// # Safety
/// `codec` must be a valid `ICodecAPI` from the configured transform.
unsafe fn set_codec_ui4(codec: &ICodecAPI, api: &GUID, value: u32) {
    let var = variant_ui4(value);
    if let Err(e) = codec.SetValue(api, &var) {
        warn!(hr = %e, property = ?api, "ICodecAPI SetValue (u32) rejected; continuing");
    }
}

/// Set an `ICodecAPI` property from a `u64` (VT_UI8), returning the call result
/// so the probe can log accepted/rejected (unlike [`set_codec_ui4`], which just
/// warns). Used for `AVEncVideoEncodeQP` (T0 CQP probe).
///
/// # Safety
/// `codec` must be a valid `ICodecAPI` from the configured transform.
unsafe fn set_codec_ui8_checked(
    codec: &ICodecAPI,
    api: &GUID,
    value: u64,
) -> Result<(), windows::core::Error> {
    let var = variant_ui8(value);
    codec.SetValue(api, &var)
}

/// Pack a single QP into the `AVEncVideoEncodeQP` layout: QP for I, P and B frames
/// live in the low three 16-bit-ish fields; NVENC-via-MF reads the same QP for all
/// when they match. We set every frame type to `qp` for constant QP.
fn pack_qp(qp: u32) -> u64 {
    let q = qp as u64;
    q | (q << 16) | (q << 32)
}

/// Build a `VT_UI4` VARIANT. No heap allocation, so no `VariantClear` is needed.
fn variant_ui4(value: u32) -> VARIANT {
    variant_scalar(VT_UI4, VARIANT_0_0_0 { ulVal: value })
}

/// Build a `VT_UI8` VARIANT. No heap allocation, so no `VariantClear` is needed.
fn variant_ui8(value: u64) -> VARIANT {
    variant_scalar(VT_UI8, VARIANT_0_0_0 { ullVal: value })
}

/// Assemble a scalar VARIANT from a variant tag and its (already-set) union.
fn variant_scalar(vt: VARENUM, anonymous: VARIANT_0_0_0) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: anonymous,
            }),
        },
    }
}
