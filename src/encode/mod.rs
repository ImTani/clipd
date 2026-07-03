//! `encode` — the hardware video-encode stage of the pipeline.
//!
//! Milestone 1: [`mft_h264`] — the async Media Foundation H.264 encoder MFT with
//! CQP rate control (spec §6.1). Audio (AAC) encode arrives in Milestone 2.

pub mod mft_h264;
