//! `mux::sinkwriter` — Media Foundation Sink Writer in passthrough (M1 first cut).
//!
//! Cannibalized from Milestone-0 spike #4. Configures the Sink Writer's input
//! type == output type == the encoder's negotiated H.264 type, so it muxes our
//! pre-encoded access units into MP4 **without re-encoding**, honouring our grid
//! timestamps. Output is written to `name.mp4.part`, finalized, fsync'd, then
//! atomically renamed to `name.mp4` (`02-AV-SYNC-SPEC.md §4.7`).
//!
//! Container type is forced with `MF_TRANSCODE_CONTAINERTYPE = MPEG4` so the
//! writer does not depend on the `.part` file extension to pick the container.
//!
//! Crash-safety caveat: the Sink Writer writes `moov` only at `Finalize()`, so a
//! crash mid-recording leaves an unplayable `.part`. This is the knowingly
//! temporary M1 first cut — the fMP4 writer (Task F2) provides the frozen-spec
//! crash-safe fragmentation.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSinkWriter, MFCreateAttributes, MFCreateMemoryBuffer, MFCreateSample,
    MFCreateSinkWriterFromURL, MFSampleExtension_CleanPoint, MFTranscodeContainerType_MPEG4,
    MF_SINK_WRITER_DISABLE_THROTTLING, MF_TRANSCODE_CONTAINERTYPE,
};

use crate::encode::mft_h264::EncodedPacket;
use crate::spec_constants::mux::PART_SUFFIX;

/// Errors from muxing.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    /// A Media Foundation call failed.
    #[error("Media Foundation sink-writer call failed: {0}")]
    Windows(#[from] windows::core::Error),
    /// A filesystem error (create / fsync / rename).
    #[error("mux I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// `MFCreateAttributes` returned no object.
    #[error("sink-writer attribute store creation returned no object")]
    NoAttributes,
}

/// A Sink-Writer MP4 muxer writing one H.264 video stream.
pub struct SinkWriterMux {
    writer: IMFSinkWriter,
    stream_index: u32,
    part_path: PathBuf,
    final_path: PathBuf,
}

impl SinkWriterMux {
    /// Create a passthrough MP4 muxer at `final_path` (writing to `…​.part`),
    /// configured from the encoder's `output_type`.
    pub fn create(output_type: &IMFMediaType, final_path: &Path) -> Result<Self, MuxError> {
        let part_path = part_path_for(final_path);
        let url = wide(&part_path);

        // SAFETY: standard Sink Writer setup; input type == output type selects
        // passthrough (no encoder MFT is inserted).
        let (writer, stream_index) = unsafe {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 2)?;
            let attrs = attrs.ok_or(MuxError::NoAttributes)?;
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;

            let writer = MFCreateSinkWriterFromURL(PCWSTR(url.as_ptr()), None, &attrs)?;
            let stream_index = writer.AddStream(output_type)?;
            writer.SetInputMediaType(stream_index, output_type, None)?;
            writer.BeginWriting()?;
            (writer, stream_index)
        };

        Ok(Self {
            writer,
            stream_index,
            part_path,
            final_path: final_path.to_owned(),
        })
    }

    /// Mux one encoded packet (reconstructs an MF sample from its bytes and grid
    /// timestamp; passthrough — no re-encode).
    pub fn write_packet(&self, packet: &EncodedPacket) -> Result<(), MuxError> {
        // SAFETY: build a memory-backed sample carrying the encoded bytes + grid
        // PTS/duration + keyframe flag, then hand it to the passthrough stream.
        unsafe {
            let len = packet.data.len() as u32;
            let buffer = MFCreateMemoryBuffer(len.max(1))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(packet.data.as_ptr(), ptr, packet.data.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(len)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(packet.pts)?;
            sample.SetSampleDuration(packet.duration)?;
            if packet.is_keyframe {
                sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
            }
            self.writer.WriteSample(self.stream_index, &sample)?;
        }
        Ok(())
    }

    /// Finalize the MP4, fsync the `.part`, and atomically rename to the final
    /// path. Consumes the muxer. Returns the final path.
    pub fn finish(self) -> Result<PathBuf, MuxError> {
        // SAFETY: flush and close the container (writes the `moov`).
        unsafe { self.writer.Finalize()? };
        // fsync the completed file before the rename (spec §4.7 durability).
        {
            let file = OpenOptions::new().write(true).open(&self.part_path)?;
            file.sync_all()?;
        }
        std::fs::rename(&self.part_path, &self.final_path)?;
        Ok(self.final_path)
    }
}

/// `foo.mp4` → `foo.mp4.part`.
fn part_path_for(final_path: &Path) -> PathBuf {
    let mut s = final_path.as_os_str().to_owned();
    s.push(PART_SUFFIX);
    PathBuf::from(s)
}

/// A NUL-terminated wide string for a Win32/MF URL parameter.
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
