//! `mux::fmp4` — hand-rolled fragmented MP4 writer (`02-AV-SYNC-SPEC.md §4`).
//!
//! The frozen-spec muxer. It writes `ftyp` + `moov` (with `mvex`/`trex` per
//! track) up front, then **one `moof` + `mdat` fragment per second** of each
//! track's content (§4.6), so a crash mid-recording still yields a file that
//! plays up to the last completed fragment. Output is `name.mp4.part` → fsync →
//! atomic rename to `name.mp4` (§4.7).
//!
//! ## Tracks (M2)
//! Track 1 is H.264 video (timescale `fps·1000`, constant sample delta 1000 —
//! strictly CFR by construction). Tracks 2+ are AAC-LC audio (timescale 48 000,
//! constant sample delta 1024): desktop first, then mic (`§2.5`). Each track
//! emits its own single-`traf` fragments; fragments interleave in the file as
//! they fill (~1 s each), and players order them per track by `baseMediaDecodeTime`.
//!
//! ## A/V alignment (M2 record path)
//! The first video packet's PTS is the common **origin**. Video sample 0 sits at
//! container time 0. Each audio track's first admitted AU is placed at
//! `round((au_pts − origin)·48000/1e7)` (its `initial_offset`), after which AUs
//! are contiguous 1024-sample units — the resampler already made the audio gap-free
//! and QPC-locked. Audio AUs that precede the origin (≤ one 21.3 ms AU of head)
//! are dropped, matching the §4.4 head-slack rule. The full §4 save-time rebasing
//! (a chosen IDR origin, trailing-audio inclusion) lands with the M3 ring/save path.
//!
//! H.264 access units arrive Annex-B; mdat needs length-prefixed (AVCC) NAL
//! units, and SPS/PPS live in `avcC`, not the samples. AAC AUs are stored raw
//! (payload type 0) with the `AudioSpecificConfig` in the `esds` box.

use std::ffi::c_void;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MPEG_SEQUENCE_HEADER,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::encode::mft_aac::EncodedAudioPacket;
use crate::encode::mft_h264::EncodedPacket;
use crate::mux::MuxError;
use crate::spec_constants::audio::aac::FRAME_SAMPLES;
use crate::spec_constants::audio::SAMPLE_RATE_HZ;
use crate::spec_constants::mux::{
    video_timescale, AUDIO_SAMPLE_DELTA, AUDIO_TIMESCALE, FRAGMENT_SECONDS, MOVIE_TIMESCALE,
    PART_SUFFIX, VIDEO_SAMPLE_DELTA,
};
use crate::spec_constants::units::TICKS_PER_SECOND;
use crate::spec_constants::PRODUCT_NAME;

/// Sample flags for a sync sample (IDR / every AAC AU): independent, not-non-sync.
const SAMPLE_FLAGS_KEY: u32 = 0x0200_0000;
/// Sample flags for a non-sync sample (P-frame).
const SAMPLE_FLAGS_NON_KEY: u32 = 0x0101_0000;

/// Track IDs. Video is always track 1; audio tracks follow (`§2.5` order).
const VIDEO_TRACK_ID: u32 = 1;
const FIRST_AUDIO_TRACK_ID: u32 = 2;

/// AAC access units per ~1 s fragment (`ceil(48000/1024) = 47`).
const AUDIO_FRAGMENT_AUS: usize = (SAMPLE_RATE_HZ as usize).div_ceil(FRAME_SAMPLES as usize);

/// Configuration for one AAC audio track.
#[derive(Debug, Clone)]
pub struct AudioTrackConfig {
    /// `AudioSpecificConfig` bytes (from [`crate::encode::mft_aac::AacEncoder::audio_specific_config`]).
    pub asc: Vec<u8>,
    /// Channel count (2 — `§2.1`).
    pub channels: u16,
    /// Sample rate (48 000 — `§2.1`).
    pub sample_rate: u32,
    /// One steady-state AAC-LC access unit of digital silence
    /// (from [`crate::encode::mft_aac::AacEncoder::silent_au`]), repeated to fill
    /// leading silence when this track's first real AU lands more than one AAC
    /// frame after the clip origin (`§4.4` / `§2.3`). Empty = no template
    /// available → the plain `§4.4` head slack (legacy behavior).
    pub silent_au: Vec<u8>,
}

/// Per-audio-track muxing state.
struct AudioTrack {
    track_id: u32,
    config: AudioTrackConfig,
    /// 48 kHz-unit alignment offset of the first AU relative to the video origin.
    initial_offset: Option<u64>,
    /// AUs buffered before the origin (first video packet) is known: `(pts, bytes)`.
    prebuffer: Vec<(i64, Vec<u8>)>,
    /// AUs accumulating in the current fragment.
    pending: Vec<Vec<u8>>,
    /// AUs flushed in prior fragments (→ `baseMediaDecodeTime`).
    total_aus: u64,
}

/// A fragmented-MP4 muxer: one H.264 video track plus zero or more AAC tracks.
pub struct Fmp4Writer {
    file: BufWriter<File>,
    part_path: PathBuf,
    final_path: PathBuf,
    /// Flush a video fragment once accumulated media duration reaches this (1 s).
    video_fragment_threshold: u32,
    /// Common origin (first video packet PTS, ticks). Audio aligns to it.
    origin: Option<i64>,
    /// Pending video fragment samples: `(avcc_bytes, is_keyframe)`.
    video_samples: Vec<(Vec<u8>, bool)>,
    video_fragment_duration: u32,
    video_total_samples: u64,
    audio: Vec<AudioTrack>,
    /// Global `moof` sequence number (1-based, unique across all tracks).
    sequence_number: u32,
}

/// Plan leading-silence fill for a track whose first admitted AU sits at `pts`,
/// relative to the clip `origin` (both master-domain ticks). Returns the number of
/// whole silent AUs to prepend and the residual alignment offset (48 kHz timescale
/// units, `< AUDIO_SAMPLE_DELTA`) so the track begins at `origin` within ≤ 1 AAC
/// frame (`§4.4`).
///
/// The real AU then lands at `offset + silent_aus·AUDIO_SAMPLE_DELTA`, which equals
/// the track's true gap from the origin — audio stays sample-accurate while the
/// head silence shrinks from the raw gap to `< 21.33 ms`. With no silence template
/// (`have_template == false`) `silent_aus == 0` and `offset` is the full gap: the
/// legacy `§4.4` head-slack behavior. Pure — unit-tested against the spec edges.
fn plan_head_fill(pts: i64, origin: i64, have_template: bool) -> (u64, u64) {
    // Truncating tick→timescale conversion (matches the pre-fill offset math).
    let gap_ticks = (pts - origin).max(0) as i128;
    let gap_units = (gap_ticks * AUDIO_TIMESCALE as i128 / TICKS_PER_SECOND as i128) as u64;
    let au = AUDIO_SAMPLE_DELTA as u64;
    if have_template && gap_units >= au {
        (gap_units / au, gap_units % au)
    } else {
        (0, gap_units)
    }
}

impl Fmp4Writer {
    /// Create an fMP4 muxer at `final_path` (writing to `…​.part`), configured from
    /// the video encoder's `output_type` (frame size, rate, SPS/PPS) and the audio
    /// track configs (desktop first, then mic — `§2.5`).
    pub fn create(
        output_type: &IMFMediaType,
        audio_tracks: &[AudioTrackConfig],
        final_path: &Path,
    ) -> Result<Self, MuxError> {
        let (width, height) = read_frame_size(output_type)?;
        let fps = read_frame_rate(output_type)?;
        let sequence_header = read_sequence_header(output_type)?;

        let nals = annexb_nals(&sequence_header);
        let sps = nals
            .iter()
            .find(|n| nal_type(n) == 7)
            .ok_or(MuxError::InvalidStream("sequence header has no SPS"))?;
        let pps = nals
            .iter()
            .find(|n| nal_type(n) == 8)
            .ok_or(MuxError::InvalidStream("sequence header has no PPS"))?;
        if sps.len() < 4 {
            return Err(MuxError::InvalidStream("SPS too short for avcC"));
        }
        let avcc = build_avcc(sps, pps);
        let timescale = video_timescale(fps);

        let audio: Vec<AudioTrack> = audio_tracks
            .iter()
            .enumerate()
            .map(|(i, cfg)| AudioTrack {
                track_id: FIRST_AUDIO_TRACK_ID + i as u32,
                config: cfg.clone(),
                initial_offset: None,
                prebuffer: Vec::new(),
                pending: Vec::new(),
                total_aus: 0,
            })
            .collect();

        let part_path = part_path_for(final_path);
        let file = File::create(&part_path)?;
        let mut file = BufWriter::new(file);
        file.write_all(&build_ftyp())?;
        file.write_all(&build_moov(&avcc, width, height, timescale, &audio))?;

        Ok(Self {
            file,
            part_path,
            final_path: final_path.to_owned(),
            video_fragment_threshold: timescale * FRAGMENT_SECONDS as u32,
            origin: None,
            video_samples: Vec::new(),
            video_fragment_duration: 0,
            video_total_samples: 0,
            audio,
            sequence_number: 0,
        })
    }

    /// Add one encoded video packet; sets the A/V origin on the first packet and
    /// flushes a fragment once ~1 s has accumulated.
    pub fn write_video_packet(&mut self, packet: &EncodedPacket) -> Result<(), MuxError> {
        let avcc = sample_to_avcc(&packet.data);
        if avcc.is_empty() {
            return Ok(()); // no VCL NALs — nothing to store
        }
        if self.origin.is_none() {
            self.origin = Some(packet.pts);
            self.admit_prebuffered_audio()?;
        }
        self.video_samples.push((avcc, packet.is_keyframe));
        self.video_fragment_duration += VIDEO_SAMPLE_DELTA;
        if self.video_fragment_duration >= self.video_fragment_threshold {
            self.flush_video_fragment()?;
        }
        Ok(())
    }

    /// Add one encoded AAC access unit to track `track_index` (0 = desktop).
    pub fn write_audio_packet(
        &mut self,
        track_index: usize,
        packet: &EncodedAudioPacket,
    ) -> Result<(), MuxError> {
        if track_index >= self.audio.len() {
            return Ok(()); // no such track configured (e.g. mic off)
        }
        match self.origin {
            // Before the origin is known, buffer with the PTS so alignment can be
            // computed once the first video packet arrives.
            None => {
                self.audio[track_index]
                    .prebuffer
                    // `data` is `Arc<[u8]>`; the mux owns AUs until a fragment
                    // flushes, so copy out of the shared buffer here (~0.32 Mbps —
                    // negligible; video already re-allocs via `sample_to_avcc`).
                    .push((packet.pts, packet.data.to_vec()));
                Ok(())
            }
            Some(origin) => self.place_audio(track_index, origin, packet.pts, packet.data.to_vec()),
        }
    }

    /// Once the origin is known, admit each track's prebuffered AUs in order.
    fn admit_prebuffered_audio(&mut self) -> Result<(), MuxError> {
        let origin = self.origin.expect("origin set before admitting audio");
        for idx in 0..self.audio.len() {
            let buffered = std::mem::take(&mut self.audio[idx].prebuffer);
            for (pts, bytes) in buffered {
                self.place_audio(idx, origin, pts, bytes)?;
            }
        }
        Ok(())
    }

    /// Place one AU into a track, dropping it if it precedes the origin, setting
    /// the alignment offset on the first admitted AU, and flushing at ~1 s.
    ///
    /// On the first admitted AU, if the track's silence template is available and
    /// the AU lands more than one AAC frame after the origin (a late-starting
    /// track — e.g. a mic on an early save), whole silent AUs are prepended so the
    /// track *begins* at the origin within ≤ 1 AAC frame (`§4.4` / `§2.3`) while the
    /// real AU still lands sample-accurately. Without a template this is the plain
    /// `§4.4` head slack (legacy behavior).
    fn place_audio(
        &mut self,
        track_index: usize,
        origin: i64,
        pts: i64,
        bytes: Vec<u8>,
    ) -> Result<(), MuxError> {
        if self.audio[track_index].initial_offset.is_none() {
            if pts < origin {
                return Ok(()); // precedes the video origin — dropped (§4.4 head slack)
            }
            let have_template = !self.audio[track_index].config.silent_au.is_empty();
            let (silent_aus, offset) = plan_head_fill(pts, origin, have_template);
            self.audio[track_index].initial_offset = Some(offset);
            for _ in 0..silent_aus {
                let silence = self.audio[track_index].config.silent_au.clone();
                self.push_au(track_index, silence)?;
            }
        }
        self.push_au(track_index, bytes)
    }

    /// Append one AU to a track's current fragment, flushing it at the ~1 s boundary.
    fn push_au(&mut self, track_index: usize, bytes: Vec<u8>) -> Result<(), MuxError> {
        self.audio[track_index].pending.push(bytes);
        if self.audio[track_index].pending.len() >= AUDIO_FRAGMENT_AUS {
            self.flush_audio_fragment(track_index)?;
        }
        Ok(())
    }

    /// Flush the accumulated video samples as one `moof`+`mdat` fragment.
    fn flush_video_fragment(&mut self) -> Result<(), MuxError> {
        if self.video_samples.is_empty() {
            return Ok(());
        }
        self.sequence_number += 1;
        let base_decode_time = self.video_total_samples * VIDEO_SAMPLE_DELTA as u64;
        let (moof, mdat) = build_fragment(
            self.sequence_number,
            VIDEO_TRACK_ID,
            base_decode_time,
            VIDEO_SAMPLE_DELTA,
            &self.video_samples,
        );
        self.write_fragment(&moof, &mdat)?;
        self.video_total_samples += self.video_samples.len() as u64;
        self.video_samples.clear();
        self.video_fragment_duration = 0;
        Ok(())
    }

    /// Flush a track's accumulated AAC AUs as one `moof`+`mdat` fragment.
    fn flush_audio_fragment(&mut self, track_index: usize) -> Result<(), MuxError> {
        let track = &mut self.audio[track_index];
        if track.pending.is_empty() {
            return Ok(());
        }
        let base_decode_time =
            track.initial_offset.unwrap_or(0) + track.total_aus * AUDIO_SAMPLE_DELTA as u64;
        let track_id = track.track_id;
        // Every AAC AU is a sync sample.
        let samples: Vec<(Vec<u8>, bool)> = track.pending.drain(..).map(|b| (b, true)).collect();
        let n = samples.len();

        self.sequence_number += 1;
        let (moof, mdat) = build_fragment(
            self.sequence_number,
            track_id,
            base_decode_time,
            AUDIO_SAMPLE_DELTA,
            &samples,
        );
        self.write_fragment(&moof, &mdat)?;
        self.audio[track_index].total_aus += n as u64;
        Ok(())
    }

    /// Write a fragment and push it out of the BufWriter so a crash leaves whole
    /// fragments on disk (crash-safety, §4.6). Not an fsync — that is `finish`.
    fn write_fragment(&mut self, moof: &[u8], mdat: &[u8]) -> Result<(), MuxError> {
        self.file.write_all(moof)?;
        self.file.write_all(mdat)?;
        self.file.flush()?;
        Ok(())
    }

    /// Flush all tracks' final fragments, fsync the `.part`, and atomically rename.
    pub fn finish(mut self) -> Result<PathBuf, MuxError> {
        self.flush_video_fragment()?;
        for idx in 0..self.audio.len() {
            // Any audio still in the prebuffer never got an origin (no video) —
            // there is nothing to align it to, so it is dropped.
            self.flush_audio_fragment(idx)?;
        }
        self.file.flush()?;
        self.file.get_ref().sync_all()?; // FlushFileBuffers (§4.7)
        let Fmp4Writer {
            file,
            part_path,
            final_path,
            ..
        } = self;
        drop(file);
        std::fs::rename(&part_path, &final_path)?;
        Ok(final_path)
    }
}

// ── Media-type reads (COM) ────────────────────────────────────────────────────

/// Read `(width, height)` from `MF_MT_FRAME_SIZE`.
fn read_frame_size(mt: &IMFMediaType) -> Result<(u32, u32), MuxError> {
    // SAFETY: attribute read on a valid media type.
    let packed = unsafe { mt.GetUINT64(&MF_MT_FRAME_SIZE)? };
    Ok(((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32))
}

/// Read the output frame rate (numerator/denominator) from `MF_MT_FRAME_RATE`.
fn read_frame_rate(mt: &IMFMediaType) -> Result<u32, MuxError> {
    // SAFETY: attribute read on a valid media type.
    let packed = unsafe { mt.GetUINT64(&MF_MT_FRAME_RATE)? };
    let num = (packed >> 32) as u32;
    let den = (packed & 0xFFFF_FFFF) as u32;
    Ok(num / den.max(1))
}

/// Read the H.264 sequence header (SPS/PPS, Annex-B) from the media type.
fn read_sequence_header(mt: &IMFMediaType) -> Result<Vec<u8>, MuxError> {
    // SAFETY: GetAllocatedBlob hands back a CoTaskMem buffer we copy then free.
    unsafe {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut size: u32 = 0;
        mt.GetAllocatedBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut ptr, &mut size)?;
        let blob = std::slice::from_raw_parts(ptr, size as usize).to_vec();
        CoTaskMemFree(Some(ptr as *const c_void));
        Ok(blob)
    }
}

// ── Annex-B / AVCC helpers (pure) ─────────────────────────────────────────────

/// The NAL unit type of a NAL payload (first byte & 0x1F).
fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1F).unwrap_or(0)
}

/// Split an Annex-B byte stream into NAL unit payloads (without start codes).
fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut marks = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            marks.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(marks.len());
    for k in 0..marks.len() {
        let start = marks[k].1;
        let mut end = if k + 1 < marks.len() {
            marks[k + 1].0
        } else {
            data.len()
        };
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            nals.push(&data[start..end]);
        }
    }
    nals
}

/// Convert one Annex-B access unit to a length-prefixed (AVCC) sample, dropping
/// SPS/PPS/AUD (types 7/8/9 — SPS/PPS live in `avcC`).
fn sample_to_avcc(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for nal in annexb_nals(data) {
        match nal_type(nal) {
            7..=9 => continue,
            _ => {
                out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                out.extend_from_slice(nal);
            }
        }
    }
    out
}

/// Build an `AVCDecoderConfigurationRecord` (the `avcC` box payload) from SPS/PPS.
fn build_avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(11 + sps.len() + pps.len());
    v.push(1); // configurationVersion
    v.push(sps[1]); // AVCProfileIndication
    v.push(sps[2]); // profile_compatibility
    v.push(sps[3]); // AVCLevelIndication
    v.push(0xFF); // reserved(6)=1 + lengthSizeMinusOne(2)=3 (4-byte lengths)
    v.push(0xE1); // reserved(3)=1 + numOfSequenceParameterSets(5)=1
    v.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    v.extend_from_slice(sps);
    v.push(1); // numOfPictureParameterSets
    v.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    v.extend_from_slice(pps);
    v
}

// ── MP4 box construction (pure) ───────────────────────────────────────────────

/// A plain box: `size(4) + type(4) + payload`.
fn mp4box(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let size = (8 + payload.len()) as u32;
    let mut v = Vec::with_capacity(size as usize);
    v.extend_from_slice(&size.to_be_bytes());
    v.extend_from_slice(typ);
    v.extend_from_slice(payload);
    v
}

/// A full box: `size + type + version(1) + flags(3) + payload`.
fn fullbox(typ: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + payload.len());
    p.push(version);
    p.extend_from_slice(&flags.to_be_bytes()[1..4]);
    p.extend_from_slice(payload);
    mp4box(typ, &p)
}

/// Concatenate box byte-vectors.
fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.iter().flatten().copied().collect()
}

/// The unity display matrix (16.16 / 2.30 fixed point).
const DISPLAY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn push_matrix(p: &mut Vec<u8>) {
    for m in DISPLAY_MATRIX {
        p.extend_from_slice(&m.to_be_bytes());
    }
}

fn build_ftyp() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"isom");
    p.extend_from_slice(&0u32.to_be_bytes());
    for brand in [b"isom", b"iso2", b"avc1", b"mp41"] {
        p.extend_from_slice(brand);
    }
    mp4box(b"ftyp", &p)
}

fn build_moov(
    avcc: &[u8],
    width: u32,
    height: u32,
    timescale: u32,
    audio: &[AudioTrack],
) -> Vec<u8> {
    let next_track_id = FIRST_AUDIO_TRACK_ID + audio.len() as u32;
    let mvhd = build_mvhd(next_track_id);
    let mut parts = vec![mvhd, build_video_trak(avcc, width, height, timescale)];
    for track in audio {
        parts.push(build_audio_trak(track));
    }
    parts.push(build_mvex(audio));
    mp4box(b"moov", &concat(&parts))
}

fn build_mvhd(next_track_id: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    p.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    p.extend_from_slice(&MOVIE_TIMESCALE.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes()); // duration (fragmented → 0)
    p.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
    p.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
    p.extend_from_slice(&0u16.to_be_bytes()); // reserved
    p.extend_from_slice(&[0u8; 8]); // reserved
    push_matrix(&mut p);
    p.extend_from_slice(&[0u8; 24]); // pre_defined
    p.extend_from_slice(&next_track_id.to_be_bytes());
    fullbox(b"mvhd", 0, 0, &p)
}

// ── Video track ──────────────────────────────────────────────────────────────

fn build_video_trak(avcc: &[u8], width: u32, height: u32, timescale: u32) -> Vec<u8> {
    let tkhd = build_tkhd(VIDEO_TRACK_ID, width, height, 0);
    let mdia = build_video_mdia(avcc, width, height, timescale);
    mp4box(b"trak", &concat(&[tkhd, mdia]))
}

/// `tkhd` shared by video (w/h in pixels, volume 0) and audio (w/h 0, volume 1.0).
fn build_tkhd(track_id: u32, width: u32, height: u32, volume: u16) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    p.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    p.extend_from_slice(&track_id.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes()); // reserved
    p.extend_from_slice(&0u32.to_be_bytes()); // duration
    p.extend_from_slice(&[0u8; 8]); // reserved
    p.extend_from_slice(&0u16.to_be_bytes()); // layer
    p.extend_from_slice(&0u16.to_be_bytes()); // alternate_group
    p.extend_from_slice(&volume.to_be_bytes()); // volume (0 video, 0x0100 audio)
    p.extend_from_slice(&0u16.to_be_bytes()); // reserved
    push_matrix(&mut p);
    p.extend_from_slice(&(width << 16).to_be_bytes()); // width 16.16
    p.extend_from_slice(&(height << 16).to_be_bytes()); // height 16.16
    fullbox(b"tkhd", 0, 0x0000_0007, &p) // enabled | in_movie | in_preview
}

fn build_video_mdia(avcc: &[u8], width: u32, height: u32, timescale: u32) -> Vec<u8> {
    let mdhd = build_mdhd(timescale);
    let hdlr = build_hdlr(b"vide");
    let minf = build_video_minf(avcc, width, height);
    mp4box(b"mdia", &concat(&[mdhd, hdlr, minf]))
}

fn build_mdhd(timescale: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    p.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    p.extend_from_slice(&timescale.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes()); // duration
    p.extend_from_slice(&0x55C4u16.to_be_bytes()); // language 'und'
    p.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
    fullbox(b"mdhd", 0, 0, &p)
}

fn build_hdlr(handler_type: &[u8; 4]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // pre_defined
    p.extend_from_slice(handler_type);
    p.extend_from_slice(&[0u8; 12]); // reserved
    p.extend_from_slice(PRODUCT_NAME.as_bytes());
    p.push(0);
    fullbox(b"hdlr", 0, 0, &p)
}

fn build_video_minf(avcc: &[u8], width: u32, height: u32) -> Vec<u8> {
    let vmhd = fullbox(b"vmhd", 0, 1, &[0u8; 8]); // flags=1; graphicsmode + opcolor
    let dinf = build_dinf();
    let stbl = build_stbl(&build_avc1(avcc, width, height));
    mp4box(b"minf", &concat(&[vmhd, dinf, stbl]))
}

fn build_dinf() -> Vec<u8> {
    let url = fullbox(b"url ", 0, 1, &[]); // self-contained
    let mut dref_p = Vec::new();
    dref_p.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    dref_p.extend_from_slice(&url);
    let dref = fullbox(b"dref", 0, 0, &dref_p);
    mp4box(b"dinf", &dref)
}

/// `stbl` with a single sample-description entry and empty stts/stsc/stsz/stco
/// (all timing lives in the fragments).
fn build_stbl(sample_entry: &[u8]) -> Vec<u8> {
    let mut stsd_p = Vec::new();
    stsd_p.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    stsd_p.extend_from_slice(sample_entry);
    let stsd = fullbox(b"stsd", 0, 0, &stsd_p);
    let stts = fullbox(b"stts", 0, 0, &0u32.to_be_bytes());
    let stsc = fullbox(b"stsc", 0, 0, &0u32.to_be_bytes());
    let mut stsz_p = Vec::new();
    stsz_p.extend_from_slice(&0u32.to_be_bytes()); // sample_size
    stsz_p.extend_from_slice(&0u32.to_be_bytes()); // sample_count
    let stsz = fullbox(b"stsz", 0, 0, &stsz_p);
    let stco = fullbox(b"stco", 0, 0, &0u32.to_be_bytes());
    mp4box(b"stbl", &concat(&[stsd, stts, stsc, stsz, stco]))
}

fn build_avc1(avcc: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&[0u8; 6]); // reserved
    p.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    p.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
    p.extend_from_slice(&0u16.to_be_bytes()); // reserved
    p.extend_from_slice(&[0u8; 12]); // pre_defined[3]
    p.extend_from_slice(&(width as u16).to_be_bytes());
    p.extend_from_slice(&(height as u16).to_be_bytes());
    p.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // horizresolution 72dpi
    p.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // vertresolution 72dpi
    p.extend_from_slice(&0u32.to_be_bytes()); // reserved
    p.extend_from_slice(&1u16.to_be_bytes()); // frame_count
    p.extend_from_slice(&[0u8; 32]); // compressorname
    p.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
    p.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined
    p.extend_from_slice(&mp4box(b"avcC", avcc));
    p.extend_from_slice(&build_colr());
    mp4box(b"avc1", &p)
}

/// `colr` (`nclx`): BT.709 primaries/transfer/matrix, limited range.
fn build_colr() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"nclx");
    p.extend_from_slice(&1u16.to_be_bytes());
    p.extend_from_slice(&1u16.to_be_bytes());
    p.extend_from_slice(&1u16.to_be_bytes());
    p.push(0x00); // limited range
    mp4box(b"colr", &p)
}

// ── Audio track ──────────────────────────────────────────────────────────────

fn build_audio_trak(track: &AudioTrack) -> Vec<u8> {
    let tkhd = build_tkhd(track.track_id, 0, 0, 0x0100); // volume 1.0, no dimensions
    let mdia = build_audio_mdia(track);
    mp4box(b"trak", &concat(&[tkhd, mdia]))
}

fn build_audio_mdia(track: &AudioTrack) -> Vec<u8> {
    let mdhd = build_mdhd(track.config.sample_rate);
    let hdlr = build_hdlr(b"soun");
    let minf = build_audio_minf(track);
    mp4box(b"mdia", &concat(&[mdhd, hdlr, minf]))
}

fn build_audio_minf(track: &AudioTrack) -> Vec<u8> {
    // smhd: sound media header (balance 0).
    let smhd = fullbox(b"smhd", 0, 0, &[0u8; 4]);
    let dinf = build_dinf();
    let stbl = build_stbl(&build_mp4a(track));
    mp4box(b"minf", &concat(&[smhd, dinf, stbl]))
}

/// `mp4a` AudioSampleEntry + `esds`.
fn build_mp4a(track: &AudioTrack) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&[0u8; 6]); // reserved
    p.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    p.extend_from_slice(&[0u8; 8]); // version(2) + revision(2) + vendor(4)
    p.extend_from_slice(&track.config.channels.to_be_bytes());
    p.extend_from_slice(&16u16.to_be_bytes()); // samplesize
    p.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
    p.extend_from_slice(&0u16.to_be_bytes()); // reserved
    p.extend_from_slice(&(track.config.sample_rate << 16).to_be_bytes()); // 16.16
    p.extend_from_slice(&build_esds(&track.config.asc));
    mp4box(b"mp4a", &p)
}

/// `esds` (ES Descriptor) carrying the AAC `AudioSpecificConfig`.
fn build_esds(asc: &[u8]) -> Vec<u8> {
    // DecoderSpecificInfo (tag 0x05) = the ASC bytes.
    let dsi = descriptor(0x05, asc);

    // DecoderConfigDescriptor (tag 0x04).
    let mut dcd = Vec::new();
    dcd.push(0x40); // objectTypeIndication: Audio ISO/IEC 14496-3 (AAC)
    dcd.push(0x15); // streamType=audio(0x05)<<2 | upstream(0)<<1 | reserved(1)
    dcd.extend_from_slice(&[0, 0, 0]); // bufferSizeDB (24-bit)
    dcd.extend_from_slice(&0u32.to_be_bytes()); // maxBitrate (0 = unspecified)
    dcd.extend_from_slice(&0u32.to_be_bytes()); // avgBitrate
    dcd.extend_from_slice(&dsi);
    let dcd = descriptor(0x04, &dcd);

    // SLConfigDescriptor (tag 0x06): predefined = 2 (MP4).
    let sl = descriptor(0x06, &[0x02]);

    // ES_Descriptor (tag 0x03): ES_ID(2)=0, flags(1)=0, then DCD + SL.
    let mut es = Vec::new();
    es.extend_from_slice(&0u16.to_be_bytes()); // ES_ID
    es.push(0x00); // flags
    es.extend_from_slice(&dcd);
    es.extend_from_slice(&sl);
    let es = descriptor(0x03, &es);

    fullbox(b"esds", 0, 0, &es)
}

/// One MPEG-4 descriptor: `tag(1) + expandable-length + payload`.
fn descriptor(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![tag];
    v.extend_from_slice(&expandable_len(payload.len()));
    v.extend_from_slice(payload);
    v
}

/// MPEG-4 expandable ("descriptor") length: base-128, 7 bits per byte, high bit
/// = continuation. Minimal encoding.
fn expandable_len(mut n: usize) -> Vec<u8> {
    let mut out = vec![(n & 0x7f) as u8];
    n >>= 7;
    while n > 0 {
        out.insert(0, ((n & 0x7f) | 0x80) as u8);
        n >>= 7;
    }
    out
}

// ── mvex / fragments ──────────────────────────────────────────────────────────

fn build_mvex(audio: &[AudioTrack]) -> Vec<u8> {
    let mut parts = vec![build_trex(VIDEO_TRACK_ID, VIDEO_SAMPLE_DELTA)];
    for track in audio {
        parts.push(build_trex(track.track_id, AUDIO_SAMPLE_DELTA));
    }
    mp4box(b"mvex", &concat(&parts))
}

fn build_trex(track_id: u32, default_sample_duration: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&track_id.to_be_bytes());
    p.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index
    p.extend_from_slice(&default_sample_duration.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes()); // default_sample_size
    p.extend_from_slice(&0u32.to_be_bytes()); // default_sample_flags
    fullbox(b"trex", 0, 0, &p)
}

/// Build one `moof`+`mdat` fragment for `track_id` from `(bytes, is_sync)` samples,
/// each with constant `sample_delta`.
fn build_fragment(
    sequence_number: u32,
    track_id: u32,
    base_decode_time: u64,
    sample_delta: u32,
    samples: &[(Vec<u8>, bool)],
) -> (Vec<u8>, Vec<u8>) {
    let mfhd = fullbox(b"mfhd", 0, 0, &sequence_number.to_be_bytes());

    // tfhd: default-base-is-moof (0x020000).
    let tfhd = fullbox(b"tfhd", 0, 0x0002_0000, &track_id.to_be_bytes());
    // tfdt v1: 64-bit baseMediaDecodeTime.
    let tfdt = fullbox(b"tfdt", 1, 0, &base_decode_time.to_be_bytes());

    // trun: data-offset + per-sample duration/size/flags present.
    let trun_flags = 0x0001 | 0x0100 | 0x0200 | 0x0400;
    let mut trun_p = Vec::new();
    trun_p.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    trun_p.extend_from_slice(&0i32.to_be_bytes()); // data_offset (patched below)
    for (data, is_sync) in samples {
        trun_p.extend_from_slice(&sample_delta.to_be_bytes());
        trun_p.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let flags = if *is_sync {
            SAMPLE_FLAGS_KEY
        } else {
            SAMPLE_FLAGS_NON_KEY
        };
        trun_p.extend_from_slice(&flags.to_be_bytes());
    }
    let trun = fullbox(b"trun", 0, trun_flags, &trun_p);

    let traf = mp4box(b"traf", &concat(&[tfhd.clone(), tfdt.clone(), trun]));
    let mut moof = mp4box(b"moof", &concat(&[mfhd.clone(), traf]));

    // Patch trun.data_offset to point at the first mdat sample byte.
    let trun_start = 8 + mfhd.len() + 8 + tfhd.len() + tfdt.len();
    let data_offset_pos = trun_start + 16; // box(8)+version/flags(4)+sample_count(4)
    let data_offset = (moof.len() + 8) as i32; // + mdat header
    moof[data_offset_pos..data_offset_pos + 4].copy_from_slice(&data_offset.to_be_bytes());

    let mut mdat_payload = Vec::new();
    for (data, _) in samples {
        mdat_payload.extend_from_slice(data);
    }
    let mdat = mp4box(b"mdat", &mdat_payload);
    (moof, mdat)
}

/// `foo.mp4` → `foo.mp4.part`.
fn part_path_for(final_path: &Path) -> PathBuf {
    let mut s = final_path.as_os_str().to_owned();
    s.push(PART_SUFFIX);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_box_size(bytes: &[u8]) -> u32 {
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    }

    /// Walk the top-level boxes of a byte buffer, returning `(type, len)` pairs.
    fn top_boxes(mut data: &[u8]) -> Vec<([u8; 4], usize)> {
        let mut out = Vec::new();
        while data.len() >= 8 {
            let size = read_box_size(data) as usize;
            let typ = [data[4], data[5], data[6], data[7]];
            if size == 0 || size > data.len() {
                break;
            }
            out.push((typ, size));
            data = &data[size..];
        }
        out
    }

    fn find_box<'a>(data: &'a [u8], typ: &[u8; 4]) -> Option<&'a [u8]> {
        let mut d = data;
        while d.len() >= 8 {
            let size = read_box_size(d) as usize;
            if size == 0 || size > d.len() {
                return None;
            }
            if &d[4..8] == typ {
                return Some(&d[..size]);
            }
            d = &d[size..];
        }
        None
    }

    fn sample_avcc() -> Vec<u8> {
        build_avcc(&[0x67, 0x42, 0xC0, 0x1F], &[0x68, 0xCE])
    }

    fn asc_48k_stereo() -> Vec<u8> {
        vec![0x11, 0x90] // AAC-LC, 48 kHz, 2ch
    }

    // ---- head-silence fill (§4.4 / §2.3) ----

    #[test]
    fn plan_head_fill_edges() {
        // No gap → no silence, zero offset (regardless of template).
        assert_eq!(plan_head_fill(0, 0, true), (0, 0));
        assert_eq!(plan_head_fill(0, 0, false), (0, 0));

        // pts before origin is clamped to a zero gap (place_audio drops these, but
        // the math must not underflow).
        assert_eq!(plan_head_fill(500, 1000, true), (0, 0));

        // Sub-AU gap (480 units < 1024): already ≤ 1 AAC frame → no fill even with a
        // template; offset is the raw gap (legacy head slack). 100_000 ticks = 480 units.
        assert_eq!(plan_head_fill(100_000, 0, true), (0, 480));

        // Exactly one AU of gap (1024 units) → one silent AU, zero residual.
        assert_eq!(plan_head_fill(213_334, 0, true), (1, 0));

        // 2.5 AU gap (2560 units) → two whole silent AUs + 512-unit residual (< 1 AU).
        assert_eq!(plan_head_fill(533_334, 0, true), (2, 512));

        // No template → never synthesizes; the full gap is the offset (legacy).
        assert_eq!(plan_head_fill(650_000, 0, false), (0, 3120));
    }

    /// Build a bare [`Fmp4Writer`] (no ftyp/moov on disk — we inspect in-memory
    /// state) with a single audio track and origin already set, for exercising
    /// [`Fmp4Writer::place_audio`] without a COM media type.
    fn writer_with_one_audio_track(silent_au: Vec<u8>, tag: &str) -> Fmp4Writer {
        let final_path = std::env::temp_dir().join(format!("clipd_test_fill_{tag}.mp4"));
        let part_path = part_path_for(&final_path);
        let file = BufWriter::new(File::create(&part_path).expect("temp part file"));
        let audio = vec![AudioTrack {
            track_id: FIRST_AUDIO_TRACK_ID,
            config: AudioTrackConfig {
                asc: asc_48k_stereo(),
                channels: 2,
                sample_rate: 48_000,
                silent_au,
            },
            initial_offset: None,
            prebuffer: Vec::new(),
            pending: Vec::new(),
            total_aus: 0,
        }];
        Fmp4Writer {
            file,
            part_path,
            final_path,
            video_fragment_threshold: 60_000,
            origin: Some(0),
            video_samples: Vec::new(),
            video_fragment_duration: 0,
            video_total_samples: 0,
            audio,
            sequence_number: 0,
        }
    }

    fn cleanup(w: &Fmp4Writer) {
        let _ = std::fs::remove_file(&w.part_path);
    }

    #[test]
    fn place_audio_prepends_silence_for_a_late_track() {
        let silence = vec![0xAAu8, 0xBB]; // dummy template
        let mut w = writer_with_one_audio_track(silence.clone(), "late");
        // First real mic AU 3.05 AU after the origin (650_000 ticks = 3120 units).
        let real = vec![0x01u8, 0x02, 0x03];
        w.place_audio(0, 0, 650_000, real.clone()).expect("place");

        let track = &w.audio[0];
        // 3 whole silent AUs prepended, real AU follows → 4 AUs in the (unflushed,
        // < 47) fragment; the track starts within 1 AU of the origin (offset 48).
        assert_eq!(track.initial_offset, Some(48));
        assert_eq!(track.pending.len(), 4);
        assert_eq!(&track.pending[0], &silence);
        assert_eq!(&track.pending[1], &silence);
        assert_eq!(&track.pending[2], &silence);
        assert_eq!(&track.pending[3], &real);
        cleanup(&w);
    }

    #[test]
    fn place_audio_without_template_keeps_legacy_head_slack() {
        let mut w = writer_with_one_audio_track(Vec::new(), "notemplate");
        let real = vec![0x01u8, 0x02, 0x03];
        w.place_audio(0, 0, 650_000, real.clone()).expect("place");

        let track = &w.audio[0];
        // No template → no silence synthesized; the raw gap is the offset (legacy).
        assert_eq!(track.initial_offset, Some(3120));
        assert_eq!(track.pending.len(), 1);
        assert_eq!(&track.pending[0], &real);
        cleanup(&w);
    }

    #[test]
    fn place_audio_drops_aus_before_origin() {
        let mut w = writer_with_one_audio_track(vec![0xAAu8], "predrop");
        w.place_audio(0, 1000, 500, vec![0x09u8]).expect("place");
        // Precedes the origin → dropped, no offset set, nothing buffered.
        assert_eq!(w.audio[0].initial_offset, None);
        assert!(w.audio[0].pending.is_empty());
        cleanup(&w);
    }

    #[test]
    fn mp4box_size_and_type() {
        let b = mp4box(b"free", &[1, 2, 3]);
        assert_eq!(read_box_size(&b), 11);
        assert_eq!(&b[4..8], b"free");
        assert_eq!(&b[8..], &[1, 2, 3]);
    }

    #[test]
    fn expandable_len_small_is_single_byte() {
        assert_eq!(expandable_len(2), vec![0x02]);
        assert_eq!(expandable_len(127), vec![0x7f]);
    }

    #[test]
    fn expandable_len_multibyte_sets_continuation() {
        // 128 → 0x81 0x00.
        assert_eq!(expandable_len(128), vec![0x81, 0x00]);
        assert_eq!(expandable_len(300), vec![0x82, 0x2c]);
    }

    #[test]
    fn esds_nests_descriptors_and_carries_asc() {
        let asc = asc_48k_stereo();
        let esds = build_esds(&asc);
        assert_eq!(&esds[4..8], b"esds");
        // The ASC bytes must appear verbatim inside the esds.
        assert!(
            esds.windows(asc.len()).any(|w| w == asc.as_slice()),
            "ASC not embedded in esds"
        );
        // tag 0x03 (ES_Descriptor) present right after the fullbox header.
        assert_eq!(esds[12], 0x03);
    }

    #[test]
    fn moov_with_two_audio_tracks_nests_and_counts_tracks() {
        let audio = vec![
            AudioTrack {
                track_id: 2,
                config: AudioTrackConfig {
                    asc: asc_48k_stereo(),
                    channels: 2,
                    sample_rate: 48_000,
                    silent_au: Vec::new(),
                },
                initial_offset: None,
                prebuffer: Vec::new(),
                pending: Vec::new(),
                total_aus: 0,
            },
            AudioTrack {
                track_id: 3,
                config: AudioTrackConfig {
                    asc: asc_48k_stereo(),
                    channels: 2,
                    sample_rate: 48_000,
                    silent_au: Vec::new(),
                },
                initial_offset: None,
                prebuffer: Vec::new(),
                pending: Vec::new(),
                total_aus: 0,
            },
        ];
        let moov = build_moov(&sample_avcc(), 1920, 1080, 60_000, &audio);
        assert_eq!(read_box_size(&moov) as usize, moov.len());
        assert_eq!(&moov[4..8], b"moov");

        // Inside moov: 1 mvhd, 3 trak (1 video + 2 audio), 1 mvex.
        let inner = &moov[8..];
        let boxes = top_boxes(inner);
        let traks = boxes.iter().filter(|(t, _)| t == b"trak").count();
        assert_eq!(traks, 3, "expected 3 traks, got {traks}");
        assert_eq!(boxes.iter().filter(|(t, _)| t == b"mvex").count(), 1);

        // mvhd next_track_ID must be 4 (video=1, audio=2,3 → next=4).
        let mvhd = find_box(inner, b"mvhd").expect("mvhd");
        let next_id = read_box_size(&mvhd[mvhd.len() - 4..]);
        assert_eq!(next_id, 4);
    }

    #[test]
    fn audio_trak_has_soun_handler_and_mp4a() {
        let track = AudioTrack {
            track_id: 2,
            config: AudioTrackConfig {
                asc: asc_48k_stereo(),
                channels: 2,
                sample_rate: 48_000,
                silent_au: Vec::new(),
            },
            initial_offset: None,
            prebuffer: Vec::new(),
            pending: Vec::new(),
            total_aus: 0,
        };
        let trak = build_audio_trak(&track);
        assert!(find_box(&trak, b"trak").is_some());
        // Handler type 'soun' appears in the hdlr; mp4a in the stsd.
        assert!(
            trak.windows(4).any(|w| w == b"soun"),
            "no soun handler in audio trak"
        );
        assert!(
            trak.windows(4).any(|w| w == b"mp4a"),
            "no mp4a sample entry in audio trak"
        );
    }

    #[test]
    fn audio_fragment_uses_1024_sample_delta_and_sync_flags() {
        let samples = vec![(vec![0xAAu8; 8], true), (vec![0xBBu8; 12], true)];
        let (moof, mdat) = build_fragment(1, 2, 0, AUDIO_SAMPLE_DELTA, &samples);
        assert_eq!(&moof[4..8], b"moof");
        assert_eq!(read_box_size(&mdat) as usize, mdat.len());
        // The AAC sample delta (1024) must appear in the trun.
        assert!(
            moof.windows(4)
                .any(|w| w == AUDIO_SAMPLE_DELTA.to_be_bytes()),
            "audio trun missing 1024 sample delta"
        );
    }

    #[test]
    fn fragment_data_offset_points_at_mdat_payload() {
        let samples = vec![(vec![0xAAu8; 10], true), (vec![0xBBu8; 20], false)];
        let (moof, mdat) = build_fragment(1, 1, 0, VIDEO_SAMPLE_DELTA, &samples);
        assert_eq!(read_box_size(&mdat), 38); // 30 payload + 8 header
        let mfhd_len = fullbox(b"mfhd", 0, 0, &1u32.to_be_bytes()).len();
        let tfhd_len = fullbox(b"tfhd", 0, 0x02_0000, &1u32.to_be_bytes()).len();
        let tfdt_len = fullbox(b"tfdt", 1, 0, &0u64.to_be_bytes()).len();
        let pos = 8 + mfhd_len + 8 + tfhd_len + tfdt_len + 16;
        let data_offset =
            i32::from_be_bytes([moof[pos], moof[pos + 1], moof[pos + 2], moof[pos + 3]]);
        assert_eq!(data_offset as usize, moof.len() + 8);
    }

    #[test]
    fn annexb_splits_3_and_4_byte_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 0xAA, //
            0, 0, 1, 0x68, 0xBB, //
            0, 0, 0, 1, 0x65, 0xCC, 0xDD,
        ];
        let nals = annexb_nals(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nal_type(nals[0]), 7);
        assert_eq!(nal_type(nals[2]), 5);
    }

    #[test]
    fn sample_to_avcc_strips_params_and_length_prefixes() {
        let data = [
            0, 0, 0, 1, 0x67, 0xAA, // SPS (dropped)
            0, 0, 0, 1, 0x68, 0xBB, // PPS (dropped)
            0, 0, 0, 1, 0x65, 0xCC, 0xDD, 0xEE, // IDR (kept)
        ];
        let avcc = sample_to_avcc(&data);
        assert_eq!(avcc, vec![0, 0, 0, 4, 0x65, 0xCC, 0xDD, 0xEE]);
    }

    #[test]
    fn avcc_record_layout() {
        let sps = [0x67, 0x42, 0xC0, 0x1F, 0x00];
        let pps = [0x68, 0xCE, 0x3C, 0x80];
        let avcc = build_avcc(&sps, &pps);
        assert_eq!(avcc[0], 1);
        assert_eq!(avcc[1], 0x42);
        assert_eq!(avcc[3], 0x1F);
        assert_eq!(avcc[4], 0xFF);
        assert_eq!(avcc[5], 0xE1);
    }
}
