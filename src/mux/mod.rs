//! `mux` — the container-writing stage.
//!
//! The engine muxes with [`fmp4`] — the frozen-spec §4 crash-safe fragmented-MP4
//! writer (`moof`/`mdat` per second + atomic `.part`→fsync→rename). [`sinkwriter`]
//! (the Media Foundation Sink Writer in passthrough) was the Task-F1 first cut and
//! stays as the documented, still-compiled fallback (DECISIONS.md).
//!
//! The muxer runs on its own thread and consumes byte-based
//! [`EncodedPacket`](crate::encode::mft_h264::EncodedPacket)s off a channel, so a
//! blocking or cloud-synced disk write does not block the encode loop directly
//! (plan pitfall 24 / data-flow rule 4). NOTE: M1 has no ring buffer, so a
//! *sustained* stall still back-pressures capture within a few frames (channel
//! depth); the full decoupling arrives with the M3 ring.

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
