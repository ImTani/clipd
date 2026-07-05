//! `encode::mft_aac` — the Media Foundation AAC-LC audio encoder MFT
//! (`02-AV-SYNC-SPEC.md §2.6`).
//!
//! One encoder per track (desktop / mic — `§2.5`). Unlike the async hardware
//! H.264 MFT, the Microsoft AAC encoder is a **synchronous software MFT**, so it
//! is driven with the classic `ProcessInput`/`ProcessOutput` pull loop rather
//! than the event state machine.
//!
//! ## Input / output
//! Input is 16-bit PCM (the AAC encoder does not accept float), so the resampled
//! f32 stream is converted with [`f32_to_i16`]. Output is **raw** AAC-LC access
//! units (payload type 0 — no ADTS), 1024 samples each, at 48 kHz stereo. The
//! encoder's `MF_MT_USER_DATA` carries the `AudioSpecificConfig` the muxer needs
//! for the MP4 `esds` box (the audio analogue of `avcC`); [`AacEncoder::audio_specific_config`]
//! exposes it.
//!
//! ## Priming delay (`§2.6`)
//! AAC encoders prepend priming samples; ignored, audio leads video by ~21 ms.
//! Output PTS is computed from the access-unit index (not the encoder's own
//! sample times — the input stream from `audio::resample` is already continuous
//! and QPC-locked, so counting output samples is honest): `pts = anchor +
//! (au_index·1024 − priming)·ticks/48_000`, and any AU whose entire content is
//! priming is dropped. `priming` is [`crate::spec_constants::audio::aac::DELAY_SAMPLES_FALLBACK`]
//! (1024) until the one-time impulse measurement (`§2.6`) pins the exact value on
//! hardware — an error here is a *constant* offset, which the `§5` AV-1 test
//! catches immediately.
//!
//! ## Threading / `unsafe`
//! `unsafe` is confined to the COM calls (this is an MF wrapper module, per
//! CLAUDE.md). The encoder is not `Send`; create and drive it on the audio-encode
//! thread in the MTA.

use std::ffi::c_void;
use std::sync::Arc;

use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MFAudioFormat_AAC,
    MFAudioFormat_PCM, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Audio,
    MFTEnumEx, MFT_CATEGORY_AUDIO_ENCODER, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_REGISTER_TYPE_INFO, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_MT_USER_DATA,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::audio::wasapi_stream::AudioStreamKind;
use crate::spec_constants::audio::aac::{DELAY_SAMPLES_FALLBACK, FRAME_SAMPLES};
use crate::spec_constants::audio::{CHANNELS, SAMPLE_RATE_HZ};
use crate::spec_constants::units::TICKS_PER_SECOND;

/// AAC-LC profile level indication (`0x29`) for `MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION`.
const AAC_LC_PROFILE_LEVEL: u32 = 0x29;
/// Bytes of `HEAACWAVEINFO` fields before the `AudioSpecificConfig` inside the
/// AAC encoder's `MF_MT_USER_DATA` blob (payload type, profile, struct type,
/// reserved — 12 bytes).
const HEAAC_WAVEINFO_PREFIX: usize = 12;

/// Errors from the AAC encoder.
#[derive(Debug, thiserror::Error)]
pub enum AacError {
    /// No AAC encoder MFT (PCM → AAC) was found.
    #[error("no AAC encoder MFT found (PCM -> AAC)")]
    NoEncoder,
    /// A Media Foundation call failed.
    #[error("Media Foundation call failed: {0}")]
    Windows(#[from] windows::core::Error),
    /// `ProcessOutput` produced no sample.
    #[error("AAC encoder produced no output sample")]
    NoOutput,
    /// The encoder's output type carried no usable `AudioSpecificConfig`.
    #[error("AAC encoder gave no AudioSpecificConfig (MF_MT_USER_DATA)")]
    NoConfig,
}

/// One encoded AAC access unit (1024 samples), with its master-domain PTS.
#[derive(Debug, Clone)]
pub struct EncodedAudioPacket {
    /// Which track this belongs to (`§2.5`).
    pub stream: AudioStreamKind,
    /// Raw AAC-LC access-unit bytes (no ADTS header). `Arc<[u8]>` to match
    /// [`crate::encode::mft_h264::EncodedPacket`] so the M3 ring retains and
    /// snapshots audio packets without a bulk copy (`§3`).
    pub data: Arc<[u8]>,
    /// PTS (ticks) of the first sample, priming-compensated (`§2.6`).
    pub pts: i64,
    /// Duration in ticks (`FRAME_SAMPLES · ticks / 48_000`).
    pub duration: i64,
}

/// The synchronous MF AAC-LC encoder for one track.
pub struct AacEncoder {
    transform: IMFTransform,
    stream: AudioStreamKind,
    /// `AudioSpecificConfig` for the muxer's `esds` box.
    asc: Vec<u8>,
    /// PTS of the first input sample (set on first `encode`).
    anchor_pts: Option<i64>,
    /// Count of output access units emitted so far (including priming).
    au_index: u64,
    /// Priming samples to compensate (`§2.6`).
    priming_samples: u32,
}

impl AacEncoder {
    /// Activate and configure an AAC-LC encoder for `stream` at `bitrate_bps`.
    pub fn new(stream: AudioStreamKind, bitrate_bps: u32) -> Result<Self, AacError> {
        let transform = activate_aac_encoder()?;
        let asc = configure(&transform, bitrate_bps)?;

        // SAFETY: begin streaming on the configured transform.
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        Ok(Self {
            transform,
            stream,
            asc,
            anchor_pts: None,
            au_index: 0,
            priming_samples: DELAY_SAMPLES_FALLBACK,
        })
    }

    /// The `AudioSpecificConfig` bytes for the MP4 `esds` box.
    pub fn audio_specific_config(&self) -> &[u8] {
        &self.asc
    }

    /// Encode one block of interleaved 16-bit stereo PCM stamped at `pts`,
    /// returning any completed AAC access units.
    pub fn encode(&mut self, pcm: &[i16], pts: i64) -> Result<Vec<EncodedAudioPacket>, AacError> {
        if self.anchor_pts.is_none() {
            self.anchor_pts = Some(pts);
        }
        let sample = make_pcm_sample(pcm, pts)?;
        // SAFETY: feed one input buffer; a sync MFT accepts it directly.
        unsafe { self.transform.ProcessInput(0, &sample, 0)? };
        self.pull_outputs()
    }

    /// Flush the encoder at end of stream, returning the tail access units.
    pub fn finish(&mut self) -> Result<Vec<EncodedAudioPacket>, AacError> {
        // SAFETY: signal end-of-stream then drain the tail.
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }
        self.pull_outputs()
    }

    /// Pull all currently-available output access units (until the MFT reports it
    /// needs more input), applying priming compensation and PTS assignment.
    fn pull_outputs(&mut self) -> Result<Vec<EncodedAudioPacket>, AacError> {
        let mut out = Vec::new();
        while let PullResult::Packet(bytes) = self.pull_one()? {
            if let Some(pkt) = self.stamp(bytes) {
                out.push(pkt);
            }
        }
        Ok(out)
    }

    /// Assign a priming-compensated PTS to an emitted access unit, or drop it if
    /// its entire content is priming (`§2.6`).
    fn stamp(&mut self, bytes: Vec<u8>) -> Option<EncodedAudioPacket> {
        let sample_pos = self.au_index * FRAME_SAMPLES as u64;
        self.au_index += 1;

        // Entirely inside the priming region → drop.
        if sample_pos + FRAME_SAMPLES as u64 <= self.priming_samples as u64 {
            return None;
        }
        let anchor = self.anchor_pts.unwrap_or(0);
        let pts = anchor
            + ((sample_pos as i128 - self.priming_samples as i128) * TICKS_PER_SECOND as i128
                / SAMPLE_RATE_HZ as i128) as i64;
        let duration =
            (FRAME_SAMPLES as i128 * TICKS_PER_SECOND as i128 / SAMPLE_RATE_HZ as i128) as i64;
        Some(EncodedAudioPacket {
            stream: self.stream,
            data: bytes.into(),
            pts,
            duration,
        })
    }

    /// One `ProcessOutput` call. Returns the AU bytes, or that more input is
    /// needed. The AAC MFT does not allocate output samples, so we supply one.
    fn pull_one(&self) -> Result<PullResult, AacError> {
        // SAFETY: size an output buffer to the stream info and supply it.
        let sample = unsafe {
            let info = self.transform.GetOutputStreamInfo(0)?;
            let sample = MFCreateSample()?;
            let buffer = MFCreateMemoryBuffer(info.cbSize.max(1))?;
            sample.AddBuffer(&buffer)?;
            sample
        };

        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(Some(sample)),
            ..Default::default()
        };
        let mut status = 0u32;
        // SAFETY: single output buffer; NEED_MORE_INPUT is the normal "drained"
        // signal, not a failure.
        let hr = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
        };

        // Reclaim the sample we supplied regardless of outcome.
        // SAFETY: pSample is the sample we set above.
        let sample = unsafe { std::mem::ManuallyDrop::take(&mut output.pSample) };
        // SAFETY: release any events collection the MFT attached.
        unsafe {
            let _ = std::mem::ManuallyDrop::take(&mut output.pEvents);
        }

        if let Err(e) = hr {
            if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                return Ok(PullResult::NeedMoreInput);
            }
            return Err(AacError::Windows(e));
        }

        let sample = sample.ok_or(AacError::NoOutput)?;
        // SAFETY: flatten the encoded AU bytes out of the sample buffer.
        let bytes = unsafe {
            let buffer: IMFMediaBuffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let v = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;
            v
        };
        Ok(PullResult::Packet(bytes))
    }
}

/// Outcome of a single `ProcessOutput`.
enum PullResult {
    Packet(Vec<u8>),
    NeedMoreInput,
}

/// Wrap interleaved 16-bit PCM as a timestamped MF sample.
fn make_pcm_sample(pcm: &[i16], pts: i64) -> Result<IMFSample, AacError> {
    let byte_len = std::mem::size_of_val(pcm) as u32;
    // SAFETY: create a buffer, copy the PCM bytes in, set its length + time.
    unsafe {
        let buffer = MFCreateMemoryBuffer(byte_len.max(1))?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max = 0u32;
        buffer.Lock(&mut ptr, Some(&mut max), None)?;
        std::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, ptr, byte_len as usize);
        buffer.Unlock()?;
        buffer.SetCurrentLength(byte_len)?;

        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(pts)?;
        let frames = pcm.len() / CHANNELS as usize;
        let duration = (frames as i128 * TICKS_PER_SECOND as i128 / SAMPLE_RATE_HZ as i128) as i64;
        sample.SetSampleDuration(duration)?;
        Ok(sample)
    }
}

/// Enumerate synchronous PCM→AAC encoder MFTs and activate the first.
fn activate_aac_encoder() -> Result<IMFTransform, AacError> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Audio,
        guidSubtype: MFAudioFormat_PCM,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Audio,
        guidSubtype: MFAudioFormat_AAC,
    };

    let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx allocates a CoTaskMem array of IMFActivate*; we own + free it.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_AUDIO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut arr,
            &mut count,
        )?;
    }
    if count == 0 || arr.is_null() {
        return Err(AacError::NoEncoder);
    }
    // SAFETY: `arr` has `count` initialized entries; take element 0, drop the rest.
    let transform = unsafe {
        let slice = std::slice::from_raw_parts(arr, count as usize);
        let first = slice[0].clone().ok_or(AacError::NoEncoder)?;
        for entry in slice.iter() {
            std::ptr::drop_in_place(
                entry as *const Option<IMFActivate> as *mut Option<IMFActivate>,
            );
        }
        CoTaskMemFree(Some(arr as *const c_void));
        first.ActivateObject::<IMFTransform>()?
    };
    Ok(transform)
}

/// Set the input (PCM) then output (AAC) types and return the `AudioSpecificConfig`.
fn configure(transform: &IMFTransform, bitrate_bps: u32) -> Result<Vec<u8>, AacError> {
    let block_align = CHANNELS as u32 * 2; // 16-bit stereo → 4 bytes/frame
    let pcm_bytes_per_sec = SAMPLE_RATE_HZ * block_align;

    // SAFETY: media-type construction + set on the freshly activated transform.
    unsafe {
        // Input: 48 kHz, 16-bit, stereo PCM.
        let inp = MFCreateMediaType()?;
        inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        inp.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE_HZ)?;
        inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
        inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block_align)?;
        inp.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, pcm_bytes_per_sec)?;
        transform.SetInputType(0, &inp, 0)?;

        // Output: AAC-LC, raw AUs (payload type 0), the configured bitrate.
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE_HZ)?;
        out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
        out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bitrate_bps / 8)?;
        out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // raw AAC
        out.SetUINT32(
            &MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION,
            AAC_LC_PROFILE_LEVEL,
        )?;
        transform.SetOutputType(0, &out, 0)?;

        // The AudioSpecificConfig sits after the 12-byte HEAACWAVEINFO prefix in
        // the output type's MF_MT_USER_DATA.
        extract_audio_specific_config(&out)
    }
}

/// Read `MF_MT_USER_DATA` from the negotiated output type and slice out the
/// `AudioSpecificConfig` (the bytes after the 12-byte HEAACWAVEINFO prefix).
///
/// # Safety
/// `mt` must be the encoder's set output media type.
unsafe fn extract_audio_specific_config(mt: &IMFMediaType) -> Result<Vec<u8>, AacError> {
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut size: u32 = 0;
    mt.GetAllocatedBlob(&MF_MT_USER_DATA, &mut ptr, &mut size)?;
    let blob = std::slice::from_raw_parts(ptr, size as usize).to_vec();
    CoTaskMemFree(Some(ptr as *const c_void));

    if blob.len() <= HEAAC_WAVEINFO_PREFIX {
        return Err(AacError::NoConfig);
    }
    Ok(blob[HEAAC_WAVEINFO_PREFIX..].to_vec())
}

/// Convert interleaved f32 samples (`[-1.0, 1.0]`) to interleaved 16-bit PCM.
/// Pure and unit-tested (no COM).
pub fn f32_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_to_i16_scales_and_clamps() {
        let out = f32_to_i16(&[0.0, 1.0, -1.0, 2.0, -2.0, 0.5]);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], i16::MAX); // 1.0 → 32767
        assert_eq!(out[2], -i16::MAX); // -1.0 → -32767
        assert_eq!(out[3], i16::MAX); // clamped
        assert_eq!(out[4], -i16::MAX); // clamped
        assert_eq!(out[5], (0.5f32 * i16::MAX as f32).round() as i16); // 16384
    }

    #[test]
    fn f32_to_i16_is_empty_for_empty() {
        assert!(f32_to_i16(&[]).is_empty());
    }

    #[test]
    fn asc_prefix_length_matches_heaac_layout() {
        // A defensive check on the constant the extractor relies on: the AAC
        // encoder's USER_DATA prefixes the AudioSpecificConfig with 12 bytes.
        assert_eq!(HEAAC_WAVEINFO_PREFIX, 12);
    }
}
