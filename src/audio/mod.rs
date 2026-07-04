//! `audio` — the Milestone-2 audio path: WASAPI capture (loopback + mic) →
//! resample to 48 kHz → AAC, muxed as two separate tracks (`02-AV-SYNC-SPEC §2`).
//!
//! This module is being built bottom-up. The **pure-logic foundations land
//! first** because `01-PROJECT-PLAN.md §3` puts "60% of the pain" in the audio
//! clock story, and the two hardest pieces of that story are pure math the spec
//! pins to exact numbers:
//!
//! - [`gaps`] — silence-gap synthesis (`§2.3`): loopback delivers nothing while
//!   the endpoint is silent, so a naive append makes audio shorter than video
//!   and everything after a quiet moment desyncs. The synthesizer turns a
//!   timestamp gap into an exact run of silence frames.
//! - [`drift`] — the sample-rate drift controller (`§2.4`): even with QPC
//!   stamping the samples arrive at the device crystal's rate, which is off by
//!   20–200 ppm; over a 5-minute clip that is audible lip-sync error. A P-only
//!   controller feeds a micro-resample ratio that holds the residual < 2 ms.
//!
//! Both are 100% safe and exhaustively unit-tested (including the spec's edge
//! numbers, per `CLAUDE.md` testing rules) — no COM, no hardware. The capture
//! worker, resampler, AAC encoder, and device-change state machine build on top
//! of them in later tasks.

pub mod drift;
pub mod gaps;
pub mod wasapi_stream;
