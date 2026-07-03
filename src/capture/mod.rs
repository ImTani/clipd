//! `capture` — the screen-capture stage of the pipeline.
//!
//! Milestone 1 sub-stages:
//! - [`wgc`] — Windows Graphics Capture of a monitor into a latest-frame cell
//!   (pixels stay on the GPU, `CLAUDE.md` rule 6).
//! - [`convert`] — `ID3D11VideoProcessor` BGRA→NV12, BT.709 limited range.
//! - [`pacing`] — the CFR slot grid (`02-AV-SYNC-SPEC.md §1`), pure logic.

pub mod convert;
pub mod pacing;
pub mod wgc;
