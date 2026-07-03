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

pub mod sinkwriter;

/// The encoder's output media type, wrapped so it can be handed once from the
/// encode thread to the mux thread.
pub struct SendMediaType(pub IMFMediaType);

// SAFETY: `IMFMediaType` is an MTA-agile Media Foundation object; a
// `SendMediaType` is transferred exactly once (encode thread → mux thread) by
// ownership over a bounded channel, never aliased across threads. Both threads
// run in the multithreaded apartment (see `crate::com`).
unsafe impl Send for SendMediaType {}
