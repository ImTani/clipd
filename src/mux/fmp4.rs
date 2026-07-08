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
//!
//! ## Hybrid-`moov` finalize (B5, amended `§4`)
//! The frozen `§4` structure — `ftyp` + fragmented `moov` + `moof`/`mdat`
//! fragments — is crash-safe but leaves non-fragment-aware readers (Explorer's
//! duration/thumbnail, some editors, old WMP seeking) squinting at a
//! duration-0 movie. On a clean [`finish`](Fmp4Writer::finish) we do the
//! OBS-Hybrid **soft remux**: a **finalized (progressive) `moov`** with complete
//! per-track sample tables (`stts`/`stsz`/`stsc`/`co64`/`stss`) is **appended at
//! the end of the file**, and a 16-byte `free` placeholder written right after
//! `ftyp` is overwritten in place with an `mdat` header whose 64-bit size spans
//! everything up to that trailing `moov` — swallowing the original fragmented
//! `moov` + every `moof`/`mdat` into one opaque Media Data box. The result reads
//! as a plain progressive MP4 (`ftyp` · giant `mdat` · `moov`); the trailing
//! `moov`'s chunk offsets point at the untouched sample bytes in place (the
//! placeholder is 16 bytes both before and after, so nothing shifts). Two small
//! writes, no media copy. A crash **before** finalize leaves a valid fragmented
//! MP4 (the `free` box is simply skipped) — `§4.6` intent preserved. Atomicity is
//! still `.part` → fsync → rename (`§4.7`).
//!
//! An audio track that received **zero** AUs all clip (an unbound per-app track —
//! no VC/game ever ran) is **dropped** from the finalized `moov` rather than
//! emitted as a zero-sample track (D-B5): a finalized clip carries only tracks
//! with content. The head-slack offset (`initial_offset`, ≤ 1 AAC frame) is
//! carried into the progressive timeline by an empty **edit list** (`elst`) so the
//! finalized file's A/V alignment is byte-for-byte the fragmented file's.

use std::ffi::c_void;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
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

/// Bytes of a plain (32-bit-size) box header — `size(4) + type(4)`; the `mdat`
/// header preceding every fragment's payload.
const MDAT_HEADER_LEN: u64 = 8;

/// Sample flags for a sync sample (IDR / every AAC AU): independent, not-non-sync.
const SAMPLE_FLAGS_KEY: u32 = 0x0200_0000;
/// Sample flags for a non-sync sample (P-frame).
const SAMPLE_FLAGS_NON_KEY: u32 = 0x0101_0000;

/// Track IDs. Video is always track 1; audio tracks follow (`§2.5` order).
const VIDEO_TRACK_ID: u32 = 1;
const FIRST_AUDIO_TRACK_ID: u32 = 2;

/// The `free` placeholder written between `ftyp` and the fragmented `moov`, in the
/// 64-bit-largesize box form (`size32 == 1` → real size in the next 8 bytes). On
/// [`finish`](Fmp4Writer::finish) its type is patched `free` → `mdat` and its
/// largesize widened to swallow the whole fragment stream (hybrid finalize). 16
/// bytes both before and after, so patching shifts no sample bytes.
const PLACEHOLDER_LEN: u64 = 16;

/// AAC access units per ~1 s fragment (`ceil(48000/1024) = 47`).
const AUDIO_FRAGMENT_AUS: usize = (SAMPLE_RATE_HZ as usize).div_ceil(FRAME_SAMPLES as usize);

/// Cap on synthesized leading-silence AUs ([`plan_head_fill`], `§4.4` fill). ~2 s of
/// audio — comfortably beyond any real device-startup latency (tens of ms; the `§7`
/// rebuild budget is 750 ms) yet a hard bound: a track that legitimately starts many
/// seconds after the origin (a device held exclusively for a long time) degrades to an
/// implicit offset for the excess instead of bursting thousands of cloned AUs +
/// fragment flushes onto the mux thread. The target case (mic ~30–60 ms late) is far
/// under the cap, so it is unaffected.
const MAX_HEAD_SILENCE_AUS: u64 = (2 * SAMPLE_RATE_HZ as u64).div_ceil(FRAME_SAMPLES as u64);

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

/// Sample metadata accumulated across a track's fragments so the finalized
/// (progressive) `moov` can be built at [`finish`](Fmp4Writer::finish) time
/// without re-reading the file. Grows one entry per sample / per fragment.
#[derive(Default)]
struct TrackIndex {
    /// Byte size of every sample, in order (→ `stsz`).
    sizes: Vec<u32>,
    /// One `(absolute file offset of the first sample byte, sample count)` per
    /// flushed fragment — each fragment's `mdat` payload is one MP4 "chunk"
    /// (→ `stsc` / `co64`).
    chunks: Vec<(u64, u32)>,
    /// Per-sample sync flag (→ `stss`). Audio is all-sync (no `stss` emitted);
    /// video carries IDR positions.
    sync: Vec<bool>,
}

impl TrackIndex {
    /// Record one flushed fragment: its `mdat`-payload file offset plus each
    /// sample's `(size, is_sync)`.
    fn push_chunk(&mut self, payload_offset: u64, samples: &[(Vec<u8>, bool)]) {
        self.chunks.push((payload_offset, samples.len() as u32));
        for (bytes, sync) in samples {
            self.sizes.push(bytes.len() as u32);
            self.sync.push(*sync);
        }
    }

    fn sample_count(&self) -> u64 {
        self.sizes.len() as u64
    }
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
    /// Sample tables for the finalized `moov`.
    index: TrackIndex,
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
    // ── Hybrid-finalize state (B5) ──
    /// Running absolute write position in the `.part` file (chunk-offset source
    /// of truth; every byte written advances it).
    file_pos: u64,
    /// File offset of the `free` placeholder (== `ftyp` length); patched to a
    /// giant `mdat` header on finalize.
    placeholder_offset: u64,
    /// Video sample tables for the finalized `moov`.
    video_index: TrackIndex,
    /// Stored video config for rebuilding the finalized `moov` (consumed once in
    /// `create` for the fragmented `moov`, needed again at `finish`).
    avcc: Vec<u8>,
    width: u32,
    height: u32,
    video_timescale: u32,
}

/// Plan leading-silence fill for a track whose first admitted AU sits at `pts`,
/// relative to the clip `origin` (both master-domain ticks). Returns the number of
/// whole silent AUs to prepend and the residual alignment offset (48 kHz timescale
/// units, `< AUDIO_SAMPLE_DELTA`) so the track begins at `origin` within ≤ 1 AAC
/// frame (`§4.4`).
///
/// The real AU always lands at `offset + silent_aus·AUDIO_SAMPLE_DELTA == gap_units`,
/// so audio stays sample-accurate; the synthesized run shrinks the head silence from
/// the raw gap toward `< 21.33 ms`. With no silence template (`have_template == false`)
/// `silent_aus == 0` and `offset` is the full gap: the legacy `§4.4` head-slack
/// behavior. The run is capped at [`MAX_HEAD_SILENCE_AUS`]; any excess stays in
/// `offset` (a genuinely-very-late track keeps an implicit head offset rather than
/// bursting thousands of AUs). Pure — unit-tested against the spec edges.
fn plan_head_fill(pts: i64, origin: i64, have_template: bool) -> (u64, u64) {
    // Truncating tick→timescale conversion (matches the pre-fill offset math).
    let gap_ticks = (pts - origin).max(0) as i128;
    let gap_units = (gap_ticks * AUDIO_TIMESCALE as i128 / TICKS_PER_SECOND as i128) as u64;
    let au = AUDIO_SAMPLE_DELTA as u64;
    if have_template && gap_units >= au {
        let silent_aus = (gap_units / au).min(MAX_HEAD_SILENCE_AUS);
        (silent_aus, gap_units - silent_aus * au)
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

        Self::create_from_parts(avcc, width, height, fps, audio_tracks, final_path)
    }

    /// The COM-free construction core (shared by [`create`](Self::create) and the
    /// finalize tests): assemble the tracks and write `ftyp` · `free` placeholder ·
    /// fragmented `moov` to the `.part` file, given an already-extracted `avcC`.
    fn create_from_parts(
        avcc: Vec<u8>,
        width: u32,
        height: u32,
        fps: u32,
        audio_tracks: &[AudioTrackConfig],
        final_path: &Path,
    ) -> Result<Self, MuxError> {
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
                index: TrackIndex::default(),
            })
            .collect();

        let part_path = part_path_for(final_path);
        let file = File::create(&part_path)?;
        let mut file = BufWriter::new(file);

        // `ftyp` · `free` placeholder (patched to a giant `mdat` at finalize) ·
        // fragmented `moov`. The placeholder sits right after `ftyp` so its
        // patched `mdat` spans the whole fragment stream.
        let ftyp = build_ftyp();
        let placeholder_offset = ftyp.len() as u64;
        let placeholder = build_placeholder_box();
        let moov = build_moov(&avcc, width, height, timescale, &audio);
        file.write_all(&ftyp)?;
        file.write_all(&placeholder)?;
        file.write_all(&moov)?;
        let file_pos = ftyp.len() as u64 + placeholder.len() as u64 + moov.len() as u64;

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
            file_pos,
            placeholder_offset,
            video_index: TrackIndex::default(),
            avcc,
            width,
            height,
            video_timescale: timescale,
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
        // The `mdat` payload (first sample byte) starts after this fragment's
        // `moof` and the 8-byte `mdat` header — its absolute offset is one chunk.
        let payload_offset = self.file_pos + moof.len() as u64 + MDAT_HEADER_LEN;
        self.video_index
            .push_chunk(payload_offset, &self.video_samples);
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
        let payload_offset = self.file_pos + moof.len() as u64 + MDAT_HEADER_LEN;
        self.audio[track_index]
            .index
            .push_chunk(payload_offset, &samples);
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
        self.file_pos += moof.len() as u64 + mdat.len() as u64;
        Ok(())
    }

    /// Flush all tracks' final fragments, append the finalized (progressive)
    /// `moov` and convert the head placeholder to a giant `mdat` (hybrid
    /// finalize), fsync the `.part`, and atomically rename.
    pub fn finish(mut self) -> Result<PathBuf, MuxError> {
        // 1. Flush every track's trailing (partial) fragment (§4.6 order).
        self.flush_video_fragment()?;
        for idx in 0..self.audio.len() {
            // Any audio still in the prebuffer never got an origin (no video) —
            // there is nothing to align it to, so it is dropped.
            self.flush_audio_fragment(idx)?;
        }

        // 2. Hybrid finalize (amended §4): append a progressive `moov` at EOF,
        //    then overwrite the head `free` placeholder with an `mdat` header
        //    whose 64-bit size swallows the entire fragment stream up to that
        //    `moov`. A degenerate clip with no video has no timeline to index —
        //    leave it a valid fragmented file (the `free` box is simply skipped by
        //    any reader) and warn, rather than emit a track-less `moov`.
        if self.video_index.sample_count() > 0 {
            let moov_start = self.file_pos;
            let final_moov = self.build_final_moov();
            self.file.write_all(&final_moov)?;
            self.file_pos += final_moov.len() as u64;

            let span = moov_start - self.placeholder_offset;
            let header = giant_mdat_header(span);
            self.file.seek(SeekFrom::Start(self.placeholder_offset))?;
            self.file.write_all(&header)?;
        } else {
            tracing::warn!(
                target: "mux",
                "no video samples — finalized as a bare fragmented MP4 (no progressive moov)"
            );
        }

        // 3. Atomicity (§4.7): flush → FlushFileBuffers → atomic rename.
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

    /// Build the finalized (progressive) `moov`: real per-track sample tables and
    /// durations, video first, then each audio track that received ≥ 1 AU. Audio
    /// tracks with **zero** AUs all clip (an unbound per-app track) are dropped
    /// (D-B5). No `mvex` — this is a non-fragmented movie box.
    fn build_final_moov(&self) -> Vec<u8> {
        let video_media_dur = self.video_index.sample_count() * VIDEO_SAMPLE_DELTA as u64;
        let mut max_dur = to_movie_ts(video_media_dur, self.video_timescale);
        let mut max_track_id = VIDEO_TRACK_ID;

        let mut traks = vec![self.build_final_video_trak()];
        for track in &self.audio {
            let n = track.index.sample_count();
            if n == 0 {
                continue; // unbound all-session per-app track → drop (D-B5)
            }
            let media_dur_movie =
                to_movie_ts(n * AUDIO_SAMPLE_DELTA as u64, track.config.sample_rate);
            let delay_movie =
                to_movie_ts(track.initial_offset.unwrap_or(0), track.config.sample_rate);
            max_dur = max_dur.max(delay_movie + media_dur_movie);
            max_track_id = max_track_id.max(track.track_id);
            traks.push(build_final_audio_trak(track));
        }

        // `next_track_id` is the highest SURVIVING track id + 1. This can never
        // collide with a dropped track's id: track ids are assigned densely as
        // `FIRST_AUDIO_TRACK_ID + index`, so every id (dropped or kept) is
        // ≤ the max configured id, and `max_track_id` here is the max over the
        // kept ids — a dropped id is strictly < `max_track_id + 1`. Keep track-id
        // assignment dense if this drop logic is ever refactored.
        let mvhd = build_mvhd(max_track_id + 1, max_dur);
        let mut parts = vec![mvhd];
        parts.extend(traks);
        mp4box(b"moov", &concat(&parts))
    }

    /// The finalized video `trak`: full sample tables + real durations, no `elst`
    /// (video sample 0 sits at time 0 — the clip origin, §4.3).
    fn build_final_video_trak(&self) -> Vec<u8> {
        let media_dur = self.video_index.sample_count() * VIDEO_SAMPLE_DELTA as u64;
        let dur_movie = to_movie_ts(media_dur, self.video_timescale);
        let tkhd = build_tkhd(VIDEO_TRACK_ID, self.width, self.height, 0, dur_movie);
        let mdhd = build_mdhd(self.video_timescale, media_dur as u32);
        let hdlr = build_hdlr(b"vide");
        let vmhd = fullbox(b"vmhd", 0, 1, &[0u8; 8]);
        let dinf = build_dinf();
        let stbl = build_stbl_full(
            &build_avc1(&self.avcc, self.width, self.height),
            &self.video_index,
            VIDEO_SAMPLE_DELTA,
        );
        let minf = mp4box(b"minf", &concat(&[vmhd, dinf, stbl]));
        let mdia = mp4box(b"mdia", &concat(&[mdhd, hdlr, minf]));
        mp4box(b"trak", &concat(&[tkhd, mdia]))
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

/// The 16-byte `free` placeholder (64-bit-largesize form) written after `ftyp`.
/// Layout: `size32 = 1` · `'free'` · `largesize = 16`. Patched in place to a giant
/// `mdat` header on [`finish`](Fmp4Writer::finish) ([`giant_mdat_header`]).
fn build_placeholder_box() -> Vec<u8> {
    let mut v = Vec::with_capacity(PLACEHOLDER_LEN as usize);
    v.extend_from_slice(&1u32.to_be_bytes()); // size32 == 1 → read largesize
    v.extend_from_slice(b"free");
    v.extend_from_slice(&PLACEHOLDER_LEN.to_be_bytes()); // largesize (empty free box)
    v
}

/// The 16-byte header that overwrites the placeholder at finalize: `size32 = 1` ·
/// `'mdat'` · `largesize = span` (the whole fragment stream up to the trailing
/// `moov`). Same length as the placeholder, so no sample byte shifts.
fn giant_mdat_header(span: u64) -> [u8; PLACEHOLDER_LEN as usize] {
    let mut h = [0u8; PLACEHOLDER_LEN as usize];
    h[0..4].copy_from_slice(&1u32.to_be_bytes());
    h[4..8].copy_from_slice(b"mdat");
    h[8..16].copy_from_slice(&span.to_be_bytes());
    h
}

fn build_moov(
    avcc: &[u8],
    width: u32,
    height: u32,
    timescale: u32,
    audio: &[AudioTrack],
) -> Vec<u8> {
    let next_track_id = FIRST_AUDIO_TRACK_ID + audio.len() as u32;
    let mvhd = build_mvhd(next_track_id, 0); // fragmented → duration 0
    let mut parts = vec![mvhd, build_video_trak(avcc, width, height, timescale)];
    for track in audio {
        parts.push(build_audio_trak(track));
    }
    parts.push(build_mvex(audio));
    mp4box(b"moov", &concat(&parts))
}

fn build_mvhd(next_track_id: u32, duration: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    p.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    p.extend_from_slice(&MOVIE_TIMESCALE.to_be_bytes());
    p.extend_from_slice(&duration.to_be_bytes()); // 0 while fragmented; real on finalize
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
    let tkhd = build_tkhd(VIDEO_TRACK_ID, width, height, 0, 0);
    let mdia = build_video_mdia(avcc, width, height, timescale);
    mp4box(b"trak", &concat(&[tkhd, mdia]))
}

/// `tkhd` shared by video (w/h in pixels, volume 0) and audio (w/h 0, volume 1.0).
/// `duration` is in the movie timescale (0 while fragmented; the track's total
/// presentation duration — including any `elst` head delay — on finalize).
fn build_tkhd(track_id: u32, width: u32, height: u32, volume: u16, duration: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    p.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    p.extend_from_slice(&track_id.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes()); // reserved
    p.extend_from_slice(&duration.to_be_bytes());
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
    let mdhd = build_mdhd(timescale, 0);
    let hdlr = build_hdlr(b"vide");
    let minf = build_video_minf(avcc, width, height);
    mp4box(b"mdia", &concat(&[mdhd, hdlr, minf]))
}

fn build_mdhd(timescale: u32, duration: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    p.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    p.extend_from_slice(&timescale.to_be_bytes());
    p.extend_from_slice(&duration.to_be_bytes()); // 0 while fragmented; real on finalize
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
    let tkhd = build_tkhd(track.track_id, 0, 0, 0x0100, 0); // volume 1.0, no dimensions
    let mdia = build_audio_mdia(track);
    mp4box(b"trak", &concat(&[tkhd, mdia]))
}

fn build_audio_mdia(track: &AudioTrack) -> Vec<u8> {
    let mdhd = build_mdhd(track.config.sample_rate, 0);
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

// ── Finalized (progressive) sample tables ─────────────────────────────────────

/// Convert a media-timescale duration to the movie timescale (1000), round-half-up.
fn to_movie_ts(media_dur: u64, media_timescale: u32) -> u32 {
    let ts = media_timescale.max(1) as u64;
    ((media_dur * MOVIE_TIMESCALE as u64 + ts / 2) / ts) as u32
}

/// The finalized audio `trak`: full sample tables + real durations, plus an empty
/// **edit list** (`edts`/`elst`) that re-inserts the ≤ 1-AAC-frame head offset the
/// fragmented file carried in `baseMediaDecodeTime`, so the progressive timeline's
/// A/V alignment matches it exactly (§4.4).
fn build_final_audio_trak(track: &AudioTrack) -> Vec<u8> {
    let media_dur = track.index.sample_count() * AUDIO_SAMPLE_DELTA as u64;
    let media_dur_movie = to_movie_ts(media_dur, track.config.sample_rate);
    let delay_movie = to_movie_ts(track.initial_offset.unwrap_or(0), track.config.sample_rate);

    let tkhd = build_tkhd(track.track_id, 0, 0, 0x0100, delay_movie + media_dur_movie);
    let mdhd = build_mdhd(track.config.sample_rate, media_dur as u32);
    let hdlr = build_hdlr(b"soun");
    let smhd = fullbox(b"smhd", 0, 0, &[0u8; 4]);
    let dinf = build_dinf();
    let stbl = build_stbl_full(&build_mp4a(track), &track.index, AUDIO_SAMPLE_DELTA);
    let minf = mp4box(b"minf", &concat(&[smhd, dinf, stbl]));
    let mdia = mp4box(b"mdia", &concat(&[mdhd, hdlr, minf]));

    let mut parts = vec![tkhd];
    if delay_movie > 0 {
        parts.push(build_edts(delay_movie, media_dur_movie));
    }
    parts.push(mdia);
    mp4box(b"trak", &concat(&parts))
}

/// A populated `stbl`: `stsd` (one entry) · `stts` · optional `stss` · `stsc` ·
/// `stsz` · `co64`. `co64` (64-bit chunk offsets) is used unconditionally so
/// long/high-bitrate clips past 4 GiB stay valid.
fn build_stbl_full(sample_entry: &[u8], index: &TrackIndex, sample_delta: u32) -> Vec<u8> {
    let mut stsd_p = Vec::new();
    stsd_p.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    stsd_p.extend_from_slice(sample_entry);
    let stsd = fullbox(b"stsd", 0, 0, &stsd_p);

    let mut parts = vec![stsd, build_stts(index.sample_count(), sample_delta)];
    if let Some(stss) = build_stss(&index.sync) {
        parts.push(stss);
    }
    parts.push(build_stsc(&index.chunks));
    parts.push(build_stsz(&index.sizes));
    parts.push(build_co64(&index.chunks));
    mp4box(b"stbl", &concat(&parts))
}

/// `stts`: one run — every sample shares `sample_delta` (video 1000, audio 1024).
fn build_stts(sample_count: u64, sample_delta: u32) -> Vec<u8> {
    let mut p = Vec::new();
    if sample_count == 0 {
        p.extend_from_slice(&0u32.to_be_bytes()); // entry_count
    } else {
        p.extend_from_slice(&1u32.to_be_bytes()); // entry_count
        p.extend_from_slice(&(sample_count as u32).to_be_bytes());
        p.extend_from_slice(&sample_delta.to_be_bytes());
    }
    fullbox(b"stts", 0, 0, &p)
}

/// `stsz`: per-sample sizes (`sample_size = 0` → the table is authoritative).
fn build_stsz(sizes: &[u32]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // sample_size 0 → per-sample table
    p.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
    for s in sizes {
        p.extend_from_slice(&s.to_be_bytes());
    }
    fullbox(b"stsz", 0, 0, &p)
}

/// `stsc`: run-length-compressed samples-per-chunk. Each fragment is one chunk;
/// consecutive fragments with the same AU count collapse to a single entry.
fn build_stsc(chunks: &[(u64, u32)]) -> Vec<u8> {
    let mut entries: Vec<(u32, u32)> = Vec::new(); // (first_chunk 1-based, samples_per_chunk)
    for (i, (_, count)) in chunks.iter().enumerate() {
        if entries.last().map(|e| e.1) == Some(*count) {
            continue;
        }
        entries.push(((i + 1) as u32, *count));
    }
    let mut p = Vec::new();
    p.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (first_chunk, spc) in entries {
        p.extend_from_slice(&first_chunk.to_be_bytes());
        p.extend_from_slice(&spc.to_be_bytes());
        p.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index
    }
    fullbox(b"stsc", 0, 0, &p)
}

/// `co64`: one 64-bit absolute file offset per chunk (the fragment's first
/// sample byte, which the giant `mdat` will contain unmoved).
fn build_co64(chunks: &[(u64, u32)]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&(chunks.len() as u32).to_be_bytes());
    for (offset, _) in chunks {
        p.extend_from_slice(&offset.to_be_bytes());
    }
    fullbox(b"co64", 0, 0, &p)
}

/// `stss`: 1-based sync-sample numbers. Omitted entirely when **every** sample is
/// a sync sample (audio, and any all-keyframe video) — the ISO default is
/// "all samples are sync", so no box is the correct, smaller encoding.
fn build_stss(sync: &[bool]) -> Option<Vec<u8>> {
    if sync.iter().all(|&s| s) {
        return None;
    }
    let keys: Vec<u32> = sync
        .iter()
        .enumerate()
        .filter(|(_, &s)| s)
        .map(|(i, _)| (i + 1) as u32)
        .collect();
    let mut p = Vec::new();
    p.extend_from_slice(&(keys.len() as u32).to_be_bytes());
    for k in keys {
        p.extend_from_slice(&k.to_be_bytes());
    }
    Some(fullbox(b"stss", 0, 0, &p))
}

/// `edts`/`elst` with an empty edit: `delay_movie` (movie ts) of blank at the
/// track head, then the media (`media_dur_movie`) from `media_time = 0` — the
/// progressive equivalent of the fragmented head `baseMediaDecodeTime`.
fn build_edts(delay_movie: u32, media_dur_movie: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&2u32.to_be_bytes()); // entry_count
                                              // Empty edit (media_time -1): inserts `delay_movie` of emptiness at the head.
    p.extend_from_slice(&delay_movie.to_be_bytes());
    p.extend_from_slice(&(-1i32).to_be_bytes());
    p.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // media_rate 1.0 (16.16)
                                                        // Media edit: play the whole media from its start.
    p.extend_from_slice(&media_dur_movie.to_be_bytes());
    p.extend_from_slice(&0i32.to_be_bytes()); // media_time 0
    p.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    let elst = fullbox(b"elst", 0, 0, &p);
    mp4box(b"edts", &elst)
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

        // Pathological gap (10 s = 480_000 units) → the silence run is capped; the
        // excess stays as an implicit offset, and the real AU still lands at the true
        // gap (invariant `offset + silent_aus·1024 == gap_units`).
        let (k, off) = plan_head_fill(100_000_000, 0, true);
        assert_eq!(k, MAX_HEAD_SILENCE_AUS);
        assert!(off >= AUDIO_SAMPLE_DELTA as u64); // excess kept implicit
        assert_eq!(k * AUDIO_SAMPLE_DELTA as u64 + off, 480_000);
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
            index: TrackIndex::default(),
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
            file_pos: 0,
            placeholder_offset: 0,
            video_index: TrackIndex::default(),
            avcc: sample_avcc(),
            width: 1920,
            height: 1080,
            video_timescale: 60_000,
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
                index: TrackIndex::default(),
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
                index: TrackIndex::default(),
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
            index: TrackIndex::default(),
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

    // ---- finalized (progressive) sample tables (B5) ----

    fn read_u32(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    /// Recursively find the first box of `typ`, descending the known container
    /// boxes (moov/trak/mdia/minf/stbl/edts).
    fn find_box_deep<'a>(data: &'a [u8], typ: &[u8; 4]) -> Option<&'a [u8]> {
        let mut d = data;
        while d.len() >= 8 {
            let size = read_box_size(d) as usize;
            if size < 8 || size > d.len() {
                return None;
            }
            if &d[4..8] == typ {
                return Some(&d[..size]);
            }
            if matches!(
                &d[4..8],
                b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"edts"
            ) {
                if let Some(found) = find_box_deep(&d[8..size], typ) {
                    return Some(found);
                }
            }
            d = &d[size..];
        }
        None
    }

    #[test]
    fn to_movie_ts_rounds_half_up() {
        // 1024 audio units (48 kHz) → 1024/48 = 21.33 ms → 21 (movie ts 1000).
        assert_eq!(to_movie_ts(1024, 48_000), 21);
        // 3000 video units at ts 60000 → 50 ms → 50.
        assert_eq!(to_movie_ts(3000, 60_000), 50);
        // Exact: 48000 units / 48000 = 1000 ms.
        assert_eq!(to_movie_ts(48_000, 48_000), 1000);
        assert_eq!(to_movie_ts(0, 48_000), 0);
    }

    #[test]
    fn stts_is_one_run_of_the_delta() {
        let stts = build_stts(5, AUDIO_SAMPLE_DELTA);
        assert_eq!(&stts[4..8], b"stts");
        let p = &stts[12..]; // after box(8)+ver/flags(4)
        assert_eq!(read_u32(p, 0), 1); // entry_count
        assert_eq!(read_u32(p, 4), 5); // sample_count
        assert_eq!(read_u32(p, 8), AUDIO_SAMPLE_DELTA); // sample_delta
                                                        // Zero samples → zero entries.
        let empty = build_stts(0, AUDIO_SAMPLE_DELTA);
        assert_eq!(read_u32(&empty[12..], 0), 0);
    }

    #[test]
    fn stsz_lists_every_sample_size() {
        let stsz = build_stsz(&[10, 20, 30]);
        let p = &stsz[12..];
        assert_eq!(read_u32(p, 0), 0); // sample_size 0 → table authoritative
        assert_eq!(read_u32(p, 4), 3); // sample_count
        assert_eq!(read_u32(p, 8), 10);
        assert_eq!(read_u32(p, 12), 20);
        assert_eq!(read_u32(p, 16), 30);
    }

    #[test]
    fn stsc_run_length_collapses_equal_chunks() {
        // Two chunks of 47, one of 12 → two entries (run of 47, then 12 @ chunk 3).
        let chunks = [(100u64, 47u32), (200, 47), (300, 12)];
        let stsc = build_stsc(&chunks);
        let p = &stsc[12..];
        assert_eq!(read_u32(p, 0), 2); // entry_count
        assert_eq!(read_u32(p, 4), 1); // first_chunk
        assert_eq!(read_u32(p, 8), 47); // samples_per_chunk
        assert_eq!(read_u32(p, 12), 1); // sample_description_index
        assert_eq!(read_u32(p, 16), 3); // first_chunk of the second run
        assert_eq!(read_u32(p, 20), 12);
    }

    #[test]
    fn co64_carries_64bit_offsets() {
        let chunks = [(0x1_0000_0000u64, 1u32), (0x1_0000_0040, 1)];
        let co64 = build_co64(&chunks);
        assert_eq!(&co64[4..8], b"co64");
        let p = &co64[12..];
        assert_eq!(read_u32(p, 0), 2); // entry_count
        assert_eq!(
            u64::from_be_bytes(p[4..12].try_into().unwrap()),
            0x1_0000_0000
        );
        assert_eq!(
            u64::from_be_bytes(p[12..20].try_into().unwrap()),
            0x1_0000_0040
        );
    }

    #[test]
    fn stss_omitted_when_all_sync_and_lists_keyframes_otherwise() {
        assert!(build_stss(&[true, true, true]).is_none());
        let stss = build_stss(&[true, false, false, true]).expect("stss");
        let p = &stss[12..];
        assert_eq!(read_u32(p, 0), 2); // two sync samples
        assert_eq!(read_u32(p, 4), 1); // sample 1 (1-based)
        assert_eq!(read_u32(p, 8), 4); // sample 4
    }

    #[test]
    fn edts_has_empty_edit_then_media_edit() {
        let edts = build_edts(9, 1000);
        assert_eq!(&edts[4..8], b"edts");
        let elst = find_box_deep(&edts, b"elst").expect("elst");
        let p = &elst[12..];
        assert_eq!(read_u32(p, 0), 2); // entry_count
                                       // Empty edit: segment_duration 9, media_time -1.
        assert_eq!(read_u32(p, 4), 9);
        assert_eq!(read_u32(p, 8), 0xFFFF_FFFF); // media_time -1
                                                 // Media edit: duration 1000, media_time 0.
        assert_eq!(read_u32(p, 16), 1000);
        assert_eq!(read_u32(p, 20), 0);
    }

    #[test]
    fn placeholder_and_giant_mdat_header_are_16_bytes() {
        let ph = build_placeholder_box();
        assert_eq!(ph.len(), PLACEHOLDER_LEN as usize);
        assert_eq!(read_u32(&ph, 0), 1); // size32 == 1 → largesize form
        assert_eq!(&ph[4..8], b"free");
        assert_eq!(
            u64::from_be_bytes(ph[8..16].try_into().unwrap()),
            PLACEHOLDER_LEN
        );

        let h = giant_mdat_header(0xDEAD_BEEF);
        assert_eq!(read_u32(&h, 0), 1);
        assert_eq!(&h[4..8], b"mdat");
        assert_eq!(
            u64::from_be_bytes(h[8..16].try_into().unwrap()),
            0xDEAD_BEEF
        );
    }

    /// A 64-bit-largesize-aware top-level box header.
    struct BoxHdr {
        offset: u64,
        size: u64,
        typ: [u8; 4],
    }

    fn top_boxes_largesize(mut data: &[u8]) -> Vec<BoxHdr> {
        let mut out = Vec::new();
        let mut pos = 0u64;
        while data.len() >= 8 {
            let size32 = read_box_size(data);
            let typ = [data[4], data[5], data[6], data[7]];
            let size = if size32 == 1 {
                if data.len() < 16 {
                    break;
                }
                u64::from_be_bytes(data[8..16].try_into().unwrap())
            } else {
                size32 as u64
            };
            if size < 8 || size as usize > data.len() {
                break;
            }
            out.push(BoxHdr {
                offset: pos,
                size,
                typ,
            });
            data = &data[size as usize..];
            pos += size;
        }
        out
    }

    fn cfg(sample_rate: u32) -> AudioTrackConfig {
        AudioTrackConfig {
            asc: asc_48k_stereo(),
            channels: 2,
            sample_rate,
            silent_au: vec![0x07u8, 0x08, 0x09, 0x0A], // non-empty template
        }
    }

    fn vpacket(pts: i64, byte: u8, keyframe: bool) -> EncodedPacket {
        // Annex-B VCL NAL: IDR (type 5) or non-IDR slice (type 1).
        let nal_type = if keyframe { 0x65u8 } else { 0x41 };
        EncodedPacket {
            data: std::sync::Arc::from([0u8, 0, 0, 1, nal_type, byte].as_slice()),
            pts,
            duration: 166_667,
            is_keyframe: keyframe,
            epoch_id: 0,
        }
    }

    fn apacket(pts: i64, fill: u8) -> EncodedAudioPacket {
        EncodedAudioPacket {
            stream: crate::audio::wasapi_stream::AudioTrackKind::Mix,
            data: std::sync::Arc::from([fill; 6].as_slice()),
            pts,
            duration: 213_333,
        }
    }

    /// End-to-end hybrid finalize: drive `create_from_parts` → packets → `finish`,
    /// then assert the finalized file is a progressive MP4 (`ftyp` · giant `mdat` ·
    /// `moov`) whose `moov` indexes the untouched sample bytes, drops the empty
    /// audio track (D-B5), and carries an `elst` for the offset audio head.
    #[test]
    fn finalize_produces_progressive_moov_over_giant_mdat() {
        let final_path = std::env::temp_dir().join("clipd_test_hybrid_finalize.mp4");
        let _ = std::fs::remove_file(&final_path);
        // Two audio tracks; only track 0 is fed (track 1 stays empty → dropped).
        let audio = vec![cfg(48_000), cfg(48_000)];
        let mut w =
            Fmp4Writer::create_from_parts(sample_avcc(), 1920, 1080, 60, &audio, &final_path)
                .expect("create");

        // Video: IDR + 2 P-frames. Origin = first video PTS (0).
        w.write_video_packet(&vpacket(0, 0x11, true)).unwrap();
        w.write_video_packet(&vpacket(166_667, 0x22, false))
            .unwrap();
        w.write_video_packet(&vpacket(333_334, 0x33, false))
            .unwrap();

        // Audio track 0 starts 300_000 ticks after origin → head fill:
        // 1440 units = 1 silent AU + 416-unit residual offset → an elst is emitted.
        for i in 0..5u64 {
            w.write_audio_packet(0, &apacket(300_000 + i as i64 * 213_333, 0xA0 + i as u8))
                .unwrap();
        }

        let path = w.finish().expect("finish");
        let bytes = std::fs::read(&path).expect("read finalized");

        // Top-level layout: exactly ftyp, giant mdat, moov (no stray moof/free).
        let boxes = top_boxes_largesize(&bytes);
        let types: Vec<[u8; 4]> = boxes.iter().map(|b| b.typ).collect();
        assert_eq!(
            types,
            vec![*b"ftyp", *b"mdat", *b"moov"],
            "finalized top-level layout must be ftyp/mdat/moov"
        );

        // The giant mdat reaches exactly to the moov and uses the largesize form.
        let mdat = &boxes[1];
        let moov = &boxes[2];
        assert_eq!(
            mdat.offset + mdat.size,
            moov.offset,
            "mdat must span up to moov"
        );
        assert_eq!(
            read_u32(&bytes[mdat.offset as usize..], 0),
            1,
            "mdat must be largesize"
        );

        let moov_bytes = &bytes[moov.offset as usize..(moov.offset + moov.size) as usize];

        // Two traks (video + 1 audio); the empty track 1 is dropped; no mvex.
        let inner = top_boxes(&moov_bytes[8..]);
        assert_eq!(
            inner.iter().filter(|(t, _)| t == b"trak").count(),
            2,
            "expected video + 1 audio trak (empty track dropped)"
        );
        assert_eq!(inner.iter().filter(|(t, _)| t == b"mvex").count(), 0);

        // Progressive tables present; no legacy 32-bit stco.
        assert!(find_box_deep(moov_bytes, b"co64").is_some(), "co64 missing");
        assert!(
            find_box_deep(moov_bytes, b"stss").is_some(),
            "video stss missing"
        );
        assert!(
            find_box_deep(moov_bytes, b"elst").is_some(),
            "audio elst missing"
        );

        // The first co64 chunk offset (video, flushed first) points at the real
        // first video sample: AVCC of the IDR = [len=2][0x65,0x11].
        let co64 = find_box_deep(moov_bytes, b"co64").unwrap();
        let first_off = u64::from_be_bytes(co64[16..24].try_into().unwrap()) as usize;
        assert_eq!(
            &bytes[first_off..first_off + 6],
            &[0, 0, 0, 2, 0x65, 0x11],
            "co64[0] must point at the first video sample bytes inside the mdat"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// A clip with no video finalizes to a valid (bare) fragmented file rather than
    /// a track-less progressive moov — the placeholder stays a `free` box.
    #[test]
    fn finalize_without_video_stays_fragmented() {
        let final_path = std::env::temp_dir().join("clipd_test_hybrid_novideo.mp4");
        let _ = std::fs::remove_file(&final_path);
        let mut w = Fmp4Writer::create_from_parts(
            sample_avcc(),
            1920,
            1080,
            60,
            &[cfg(48_000)],
            &final_path,
        )
        .expect("create");
        // No video → audio prebuffers and is dropped at finish (no origin).
        w.write_audio_packet(0, &apacket(0, 0x55)).unwrap();
        let path = w.finish().expect("finish");
        let bytes = std::fs::read(&path).expect("read");

        // The head placeholder is still a `free` box; there is exactly one moov
        // (the fragmented one) and no appended progressive moov.
        let boxes = top_boxes_largesize(&bytes);
        assert!(
            boxes.iter().any(|b| &b.typ == b"free"),
            "placeholder must stay free"
        );
        assert_eq!(boxes.iter().filter(|b| &b.typ == b"moov").count(), 1);
        let _ = std::fs::remove_file(&path);
    }
}
