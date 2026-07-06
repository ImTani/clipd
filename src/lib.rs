//! `clipd` — a single-binary, native Windows replay-buffer clipper.
//!
//! Continuous capture (monitor or focused window) → hardware encode → in-memory
//! compressed ring buffer → hotkey saves the last N seconds as fMP4. See
//! `CLAUDE.md` and the `clipper-devpack/devpack/` docs for the normative spec.
//!
//! This crate is split library + binary so the pure-logic modules (clock,
//! config, and the spec constants) are unit-testable without the engine or any
//! hardware. The binary ([`main`](../clipd/index.html)) is a thin shell over
//! these modules until the capture/encode/audio/mux threads land in later
//! milestones.

pub mod audio;
pub mod autostart;
pub mod capture;
pub mod clock;
pub mod com;
pub mod config;
pub mod encode;
pub mod engine;
pub mod gpu;
pub mod hotkey;
pub mod logging;
pub mod mux;
pub mod ring;
pub mod save;
pub mod spec_constants;
pub mod ui;
pub mod watchdog;
