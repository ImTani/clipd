//! `mux` — the container-writing stage.
//!
//! Milestone 1 ships [`sinkwriter`] (the Media Foundation Sink Writer in
//! passthrough) as the first cut. The frozen-spec fMP4 writer (crash-safe
//! `moof`/`mdat` + rebasing, `02-AV-SYNC-SPEC.md §4`) replaces it in Task F2; the
//! Sink Writer stays as the documented fallback (DECISIONS.md).
//!
//! The muxer runs on its own thread and consumes byte-based
//! [`EncodedPacket`](crate::encode::mft_h264::EncodedPacket)s off a channel, so a
//! blocking or cloud-synced disk write never stalls capture (plan pitfall 24 /
//! data-flow rule 4).

use windows::Win32::Media::MediaFoundation::IMFMediaType;

pub mod fmp4;
pub mod sinkwriter;

/// Errors shared by the muxer implementations.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    /// A Media Foundation call failed.
    #[error("Media Foundation call failed: {0}")]
    Windows(#[from] windows::core::Error),
    /// A filesystem error (create / write / fsync / rename).
    #[error("mux I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A Media Foundation attribute store creation returned no object.
    #[error("mux attribute store creation returned no object")]
    NoAttributes,
    /// The encoded stream was malformed (e.g. missing SPS/PPS for the `avcC` box).
    #[error("invalid encoded stream: {0}")]
    InvalidStream(&'static str),
}

/// The encoder's output media type, wrapped so it can be handed once from the
/// encode thread to the mux thread.
pub struct SendMediaType(pub IMFMediaType);

// SAFETY: `IMFMediaType` is an MTA-agile Media Foundation object; a
// `SendMediaType` is transferred exactly once (encode thread → mux thread) by
// ownership over a bounded channel, never aliased across threads. Both threads
// run in the multithreaded apartment (see `crate::com`).
unsafe impl Send for SendMediaType {}
