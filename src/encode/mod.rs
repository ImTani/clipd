//! `encode` — the hardware video-encode stage of the pipeline.
//!
//! Milestone 1: [`mft_h264`] — the async Media Foundation H.264 encoder MFT with
//! CQP rate control (spec §6.1). Milestone 2: [`mft_aac`] — the synchronous AAC-LC
//! audio encoder MFT (spec §2.6).

pub mod mft_aac;
pub mod mft_h264;
