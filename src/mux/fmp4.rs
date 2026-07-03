//! `mux::fmp4` — hand-rolled fragmented MP4 writer (`02-AV-SYNC-SPEC.md §4`).
//!
//! The frozen-spec muxer that replaces the F1 Sink Writer. It writes
//! `ftyp` + `moov` (with `mvex`/`trex`) up front, then **one `moof` + `mdat`
//! fragment per second** of content (§4.6), so a crash mid-recording still
//! yields a file that plays up to the last completed fragment — the exact
//! "pressed the button, got nothing" failure the product exists to kill. Output
//! is `name.mp4.part` → fsync → atomic rename to `name.mp4` (§4.7).
//!
//! Timing is exact: video track timescale = `fps·1000`, every sample's duration
//! is the constant `VIDEO_SAMPLE_DELTA` (1000), so the track is strictly CFR by
//! construction (the pacing grid already emits exactly `fps` samples/second).
//! No PTS→timescale rounding is needed — a fragment's `baseMediaDecodeTime` is
//! simply `samples_so_far · sample_delta`.
//!
//! H.264 access units arrive Annex-B (start codes); mdat needs length-prefixed
//! (AVCC) NAL units, and SPS/PPS live in the `avcC` box, not the samples — both
//! handled by [`sample_to_avcc`] / [`build_avcc`].

use std::ffi::c_void;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MPEG_SEQUENCE_HEADER,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::encode::mft_h264::EncodedPacket;
use crate::mux::MuxError;
use crate::spec_constants::mux::{
    video_timescale, FRAGMENT_SECONDS, MOVIE_TIMESCALE, PART_SUFFIX, VIDEO_SAMPLE_DELTA,
};
use crate::spec_constants::PRODUCT_NAME;

/// Sample flags for a sync sample (IDR): `sample_depends_on = 2` (independent),
/// `sample_is_non_sync_sample = 0`.
const SAMPLE_FLAGS_KEY: u32 = 0x0200_0000;
/// Sample flags for a non-sync sample: `sample_depends_on = 1`,
/// `sample_is_non_sync_sample = 1`.
const SAMPLE_FLAGS_NON_KEY: u32 = 0x0101_0000;

/// A fragmented-MP4 muxer writing one H.264 video track.
pub struct Fmp4Writer {
    file: BufWriter<File>,
    part_path: PathBuf,
    final_path: PathBuf,
    /// Flush a fragment once accumulated media duration reaches this (1 s).
    fragment_threshold: u32,
    /// Pending fragment samples: `(avcc_bytes, is_keyframe)`.
    samples: Vec<(Vec<u8>, bool)>,
    /// Accumulated media duration of the pending fragment.
    fragment_duration: u32,
    /// Total samples written in prior fragments (→ `baseMediaDecodeTime`).
    total_samples: u64,
    /// `moof` sequence number (1-based).
    sequence_number: u32,
}

impl Fmp4Writer {
    /// Create an fMP4 muxer at `final_path` (writing to `…​.part`), configured
    /// from the encoder's `output_type` (frame size, rate, and SPS/PPS).
    pub fn create(output_type: &IMFMediaType, final_path: &Path) -> Result<Self, MuxError> {
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
        let part_path = part_path_for(final_path);
        let file = File::create(&part_path)?;
        let mut file = BufWriter::new(file);
        file.write_all(&build_ftyp())?;
        file.write_all(&build_moov(&avcc, width, height, timescale))?;

        Ok(Self {
            file,
            part_path,
            final_path: final_path.to_owned(),
            fragment_threshold: timescale * FRAGMENT_SECONDS as u32,
            samples: Vec::new(),
            fragment_duration: 0,
            total_samples: 0,
            sequence_number: 0,
        })
    }

    /// Add one encoded packet; flushes a fragment once ~1 s has accumulated.
    pub fn write_packet(&mut self, packet: &EncodedPacket) -> Result<(), MuxError> {
        let avcc = sample_to_avcc(&packet.data);
        if avcc.is_empty() {
            return Ok(()); // no VCL NALs — nothing to store
        }
        self.samples.push((avcc, packet.is_keyframe));
        self.fragment_duration += VIDEO_SAMPLE_DELTA;
        if self.fragment_duration >= self.fragment_threshold {
            self.flush_fragment()?;
        }
        Ok(())
    }

    /// Flush the accumulated samples as one `moof`+`mdat` fragment.
    fn flush_fragment(&mut self) -> Result<(), MuxError> {
        if self.samples.is_empty() {
            return Ok(());
        }
        self.sequence_number += 1;
        let base_decode_time = self.total_samples * VIDEO_SAMPLE_DELTA as u64;
        let (moof, mdat) = build_fragment(self.sequence_number, base_decode_time, &self.samples);
        self.file.write_all(&moof)?;
        self.file.write_all(&mdat)?;
        // Push each completed fragment out of the BufWriter to the OS so a process
        // crash leaves whole fragments on disk (crash-safety, §4.6). Not an fsync —
        // power-loss durability is the final fsync in `finish`.
        self.file.flush()?;
        self.total_samples += self.samples.len() as u64;
        self.samples.clear();
        self.fragment_duration = 0;
        Ok(())
    }

    /// Flush the final fragment, fsync the `.part`, and atomically rename.
    pub fn finish(mut self) -> Result<PathBuf, MuxError> {
        self.flush_fragment()?;
        self.file.flush()?;
        self.file.get_ref().sync_all()?; // FlushFileBuffers (spec §4.7)
        let Fmp4Writer {
            file,
            part_path,
            final_path,
            ..
        } = self;
        drop(file); // close the handle before the rename
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

/// The NAL unit type (`nal_unit_type`) of a NAL payload (first byte & 0x1F).
fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1F).unwrap_or(0)
}

/// Split an Annex-B byte stream into NAL unit payloads (without start codes).
/// Handles both 3- and 4-byte start codes.
fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    // Positions of each start code: (start-code index, payload start).
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
        // Trim trailing zeros that belong to the next (4-byte) start code.
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
    p.extend_from_slice(b"isom"); // major_brand
    p.extend_from_slice(&0u32.to_be_bytes()); // minor_version
    for brand in [b"isom", b"iso2", b"avc1", b"mp41"] {
        p.extend_from_slice(brand);
    }
    mp4box(b"ftyp", &p)
}

fn build_moov(avcc: &[u8], width: u32, height: u32, timescale: u32) -> Vec<u8> {
    let mvhd = build_mvhd();
    let trak = build_trak(avcc, width, height, timescale);
    let mvex = build_mvex();
    mp4box(b"moov", &concat(&[mvhd, trak, mvex]))
}

fn build_mvhd() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    p.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    p.extend_from_slice(&MOVIE_TIMESCALE.to_be_bytes()); // timescale
    p.extend_from_slice(&0u32.to_be_bytes()); // duration (fragmented → 0)
    p.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
    p.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
    p.extend_from_slice(&0u16.to_be_bytes()); // reserved
    p.extend_from_slice(&[0u8; 8]); // reserved
    push_matrix(&mut p);
    p.extend_from_slice(&[0u8; 24]); // pre_defined
    p.extend_from_slice(&2u32.to_be_bytes()); // next_track_ID
    fullbox(b"mvhd", 0, 0, &p)
}

fn build_trak(avcc: &[u8], width: u32, height: u32, timescale: u32) -> Vec<u8> {
    let tkhd = build_tkhd(width, height);
    let mdia = build_mdia(avcc, width, height, timescale);
    mp4box(b"trak", &concat(&[tkhd, mdia]))
}

fn build_tkhd(width: u32, height: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    p.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    p.extend_from_slice(&1u32.to_be_bytes()); // track_ID
    p.extend_from_slice(&0u32.to_be_bytes()); // reserved
    p.extend_from_slice(&0u32.to_be_bytes()); // duration
    p.extend_from_slice(&[0u8; 8]); // reserved
    p.extend_from_slice(&0u16.to_be_bytes()); // layer
    p.extend_from_slice(&0u16.to_be_bytes()); // alternate_group
    p.extend_from_slice(&0u16.to_be_bytes()); // volume (0 for video)
    p.extend_from_slice(&0u16.to_be_bytes()); // reserved
    push_matrix(&mut p);
    p.extend_from_slice(&(width << 16).to_be_bytes()); // width 16.16
    p.extend_from_slice(&(height << 16).to_be_bytes()); // height 16.16
                                                        // flags: enabled | in_movie | in_preview
    fullbox(b"tkhd", 0, 0x0000_0007, &p)
}

fn build_mdia(avcc: &[u8], width: u32, height: u32, timescale: u32) -> Vec<u8> {
    let mdhd = build_mdhd(timescale);
    let hdlr = build_hdlr();
    let minf = build_minf(avcc, width, height);
    mp4box(b"mdia", &concat(&[mdhd, hdlr, minf]))
}

fn build_mdhd(timescale: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    p.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    p.extend_from_slice(&timescale.to_be_bytes()); // timescale (fps*1000)
    p.extend_from_slice(&0u32.to_be_bytes()); // duration
    p.extend_from_slice(&0x55C4u16.to_be_bytes()); // language 'und'
    p.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
    fullbox(b"mdhd", 0, 0, &p)
}

fn build_hdlr() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // pre_defined
    p.extend_from_slice(b"vide"); // handler_type
    p.extend_from_slice(&[0u8; 12]); // reserved
    p.extend_from_slice(PRODUCT_NAME.as_bytes()); // name (from the one-constant name)
    p.push(0); // NUL-terminated
    fullbox(b"hdlr", 0, 0, &p)
}

fn build_minf(avcc: &[u8], width: u32, height: u32) -> Vec<u8> {
    let vmhd = fullbox(b"vmhd", 0, 1, &[0u8; 8]); // flags=1; graphicsmode + opcolor
    let dinf = build_dinf();
    let stbl = build_stbl(avcc, width, height);
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

fn build_stbl(avcc: &[u8], width: u32, height: u32) -> Vec<u8> {
    let stsd = build_stsd(avcc, width, height);
    let stts = fullbox(b"stts", 0, 0, &0u32.to_be_bytes()); // 0 entries
    let stsc = fullbox(b"stsc", 0, 0, &0u32.to_be_bytes()); // 0 entries
    let mut stsz_p = Vec::new();
    stsz_p.extend_from_slice(&0u32.to_be_bytes()); // sample_size
    stsz_p.extend_from_slice(&0u32.to_be_bytes()); // sample_count
    let stsz = fullbox(b"stsz", 0, 0, &stsz_p);
    let stco = fullbox(b"stco", 0, 0, &0u32.to_be_bytes()); // 0 entries
    mp4box(b"stbl", &concat(&[stsd, stts, stsc, stsz, stco]))
}

fn build_stsd(avcc: &[u8], width: u32, height: u32) -> Vec<u8> {
    let avc1 = build_avc1(avcc, width, height);
    let mut p = Vec::new();
    p.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    p.extend_from_slice(&avc1);
    fullbox(b"stsd", 0, 0, &p)
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
    p.extend_from_slice(&build_colr()); // BT.709 limited signalling
    mp4box(b"avc1", &p)
}

/// `colr` box (`nclx`): BT.709 primaries/transfer/matrix, limited range.
fn build_colr() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"nclx");
    p.extend_from_slice(&1u16.to_be_bytes()); // colour_primaries = BT.709
    p.extend_from_slice(&1u16.to_be_bytes()); // transfer_characteristics = BT.709
    p.extend_from_slice(&1u16.to_be_bytes()); // matrix_coefficients = BT.709
    p.push(0x00); // full_range_flag = 0 (limited), reserved bits 0
    mp4box(b"colr", &p)
}

fn build_mvex() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&1u32.to_be_bytes()); // track_ID
    p.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index
    p.extend_from_slice(&VIDEO_SAMPLE_DELTA.to_be_bytes()); // default_sample_duration
    p.extend_from_slice(&0u32.to_be_bytes()); // default_sample_size
    p.extend_from_slice(&0u32.to_be_bytes()); // default_sample_flags
    let trex = fullbox(b"trex", 0, 0, &p);
    mp4box(b"mvex", &trex)
}

/// Build one `moof`+`mdat` fragment from `(avcc_bytes, is_keyframe)` samples.
fn build_fragment(
    sequence_number: u32,
    base_decode_time: u64,
    samples: &[(Vec<u8>, bool)],
) -> (Vec<u8>, Vec<u8>) {
    let mfhd = fullbox(b"mfhd", 0, 0, &sequence_number.to_be_bytes());

    // tfhd: default-base-is-moof (0x020000); rely on trex defaults otherwise.
    let tfhd = fullbox(b"tfhd", 0, 0x0002_0000, &1u32.to_be_bytes()); // track_ID

    // tfdt v1: 64-bit baseMediaDecodeTime.
    let tfdt = fullbox(b"tfdt", 1, 0, &base_decode_time.to_be_bytes());

    // trun: data-offset + per-sample duration/size/flags present.
    let trun_flags = 0x0001 | 0x0100 | 0x0200 | 0x0400;
    let mut trun_p = Vec::new();
    trun_p.extend_from_slice(&(samples.len() as u32).to_be_bytes()); // sample_count
    trun_p.extend_from_slice(&0i32.to_be_bytes()); // data_offset (patched below)
    for (data, is_key) in samples {
        trun_p.extend_from_slice(&VIDEO_SAMPLE_DELTA.to_be_bytes());
        trun_p.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let flags = if *is_key {
            SAMPLE_FLAGS_KEY
        } else {
            SAMPLE_FLAGS_NON_KEY
        };
        trun_p.extend_from_slice(&flags.to_be_bytes());
    }
    let trun = fullbox(b"trun", 0, trun_flags, &trun_p);

    let traf = mp4box(b"traf", &concat(&[tfhd.clone(), tfdt.clone(), trun]));
    let mut moof = mp4box(b"moof", &concat(&[mfhd.clone(), traf]));

    // Patch trun.data_offset to point at the first byte of mdat sample data
    // (relative to the moof start, per default-base-is-moof).
    let trun_start = 8 + mfhd.len() + 8 + tfhd.len() + tfdt.len();
    let data_offset_pos = trun_start + 16; // box(8) + version/flags(4) + sample_count(4)
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

    #[test]
    fn mp4box_size_and_type() {
        let b = mp4box(b"free", &[1, 2, 3]);
        assert_eq!(read_box_size(&b), 11);
        assert_eq!(&b[4..8], b"free");
        assert_eq!(&b[8..], &[1, 2, 3]);
    }

    #[test]
    fn fullbox_has_version_and_flags() {
        let b = fullbox(b"test", 1, 0x0007_0001, &[]);
        // size(4)+type(4)+version(1)+flags(3) = 12
        assert_eq!(read_box_size(&b), 12);
        assert_eq!(b[8], 1); // version
        assert_eq!(&b[9..12], &[0x07, 0x00, 0x01]); // flags (low 3 bytes)
    }

    #[test]
    fn annexb_splits_3_and_4_byte_start_codes() {
        // 4-byte SC, SPS(7); 3-byte SC, PPS(8); 4-byte SC, IDR(5).
        let data = [
            0, 0, 0, 1, 0x67, 0xAA, //
            0, 0, 1, 0x68, 0xBB, //
            0, 0, 0, 1, 0x65, 0xCC, 0xDD,
        ];
        let nals = annexb_nals(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nal_type(nals[0]), 7);
        assert_eq!(nals[0], &[0x67, 0xAA]);
        assert_eq!(nal_type(nals[1]), 8);
        assert_eq!(nals[1], &[0x68, 0xBB]);
        assert_eq!(nal_type(nals[2]), 5);
        assert_eq!(nals[2], &[0x65, 0xCC, 0xDD]);
    }

    #[test]
    fn sample_to_avcc_strips_params_and_length_prefixes() {
        // SPS + PPS + IDR → only the IDR remains, 4-byte length-prefixed.
        let data = [
            0, 0, 0, 1, 0x67, 0xAA, // SPS (dropped)
            0, 0, 0, 1, 0x68, 0xBB, // PPS (dropped)
            0, 0, 0, 1, 0x65, 0xCC, 0xDD, 0xEE, // IDR (kept)
        ];
        let avcc = sample_to_avcc(&data);
        // length(4) = 4 + the 4 IDR bytes.
        assert_eq!(avcc, vec![0, 0, 0, 4, 0x65, 0xCC, 0xDD, 0xEE]);
    }

    #[test]
    fn avcc_record_layout() {
        let sps = [0x67, 0x42, 0xC0, 0x1F, 0x00]; // profile 0x42, level 0x1F
        let pps = [0x68, 0xCE, 0x3C, 0x80];
        let avcc = build_avcc(&sps, &pps);
        assert_eq!(avcc[0], 1); // configurationVersion
        assert_eq!(avcc[1], 0x42); // profile
        assert_eq!(avcc[3], 0x1F); // level
        assert_eq!(avcc[4], 0xFF); // lengthSizeMinusOne
        assert_eq!(avcc[5], 0xE1); // numSPS = 1
        assert_eq!(&avcc[6..8], &(sps.len() as u16).to_be_bytes());
    }

    #[test]
    fn fragment_data_offset_points_at_mdat_payload() {
        let samples = vec![(vec![0xAAu8; 10], true), (vec![0xBBu8; 20], false)];
        let (moof, mdat) = build_fragment(1, 0, &samples);
        // mdat payload = 30 bytes → mdat box = 38.
        assert_eq!(read_box_size(&mdat), 38);
        assert_eq!(&moof[4..8], b"moof");
        // Locate trun.data_offset the same way the writer patches it.
        let mfhd_len = fullbox(b"mfhd", 0, 0, &1u32.to_be_bytes()).len();
        let tfhd_len = fullbox(b"tfhd", 0, 0x02_0000, &1u32.to_be_bytes()).len();
        let tfdt_len = fullbox(b"tfdt", 1, 0, &0u64.to_be_bytes()).len();
        let pos = 8 + mfhd_len + 8 + tfhd_len + tfdt_len + 16;
        let data_offset =
            i32::from_be_bytes([moof[pos], moof[pos + 1], moof[pos + 2], moof[pos + 3]]);
        assert_eq!(data_offset as usize, moof.len() + 8);
    }

    #[test]
    fn moov_boxes_nest_to_declared_sizes() {
        let avcc = build_avcc(&[0x67, 0x42, 0xC0, 0x1F], &[0x68, 0xCE]);
        let moov = build_moov(&avcc, 1920, 1080, 60_000);
        assert_eq!(read_box_size(&moov) as usize, moov.len());
        assert_eq!(&moov[4..8], b"moov");
    }
}
