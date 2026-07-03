//! `capture` — the screen-capture stage of the pipeline.
//!
//! Milestone 1 wires the first two sub-stages:
//! - [`wgc`] — Windows Graphics Capture of a monitor into a latest-frame cell
//!   (pixels stay on the GPU, `CLAUDE.md` rule 6).
//!
//! Later M1 tasks add `convert` (`ID3D11VideoProcessor` BGRA→NV12) and `pacing`
//! (the CFR slot grid, `02-AV-SYNC-SPEC.md §1`).

pub mod wgc;
