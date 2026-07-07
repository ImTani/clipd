//! `audio::levels` — lock-free per-stream audio level publishing for the VU
//! meters (M7 Slice A / A3).
//!
//! ## Satellite law direction (`CLAUDE.md` "UI rules")
//! The engine's audio-process threads PUBLISH a level here; the settings window
//! READS it. Direction is strictly engine → UI: this type lives in the engine
//! (`audio`) module and the `ui` module only holds a clone of the `Arc` and
//! reads it. No UI code is referenced from here, and the engine runs fully whether
//! or not the window ever opens.
//!
//! ## Why lock-free (atomics, not a channel or a mutex)
//! A level is a *latest-value* signal, not a stream of events: a meter only ever
//! wants the most recent value and tolerates one that is a frame stale. An
//! [`AtomicU32`] per scalar (an `f32` stored as its bit pattern) lets the audio
//! thread store without ever blocking and the UI thread load without ever
//! blocking — and it deliberately does NOT route through `ShellSignal` (the
//! tray's single, state-only consumer). [`Ordering::Relaxed`] suffices: `peak`
//! and `rms` are independent scalars with no cross-field invariant, and there is
//! no other memory whose visibility they gate.
//!
//! ## Pure math
//! The amplitude → dBFS → bar-fraction mapping and the meter decay are pure
//! functions, unit-tested here with the boundary numbers (silence, full scale,
//! −6 dB at half amplitude, floor clamp) like the other logic modules.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::audio::wasapi_stream::AudioStreamKind;

/// Meter floor: amplitudes at or below this dBFS map to an empty bar. −60 dBFS is
/// the conventional noise floor for a compact VU meter — below it the signal is
/// inaudible for metering purposes.
pub const METER_FLOOR_DBFS: f32 = -60.0;

/// Meter release rate, in bar-fraction per second: how fast the *displayed* bar
/// falls once the signal drops (the rise is instant — see [`release_toward`]).
/// 0.7/s ≈ 1.4 s full→empty, the usual VU falloff: fast enough to feel live, slow
/// enough to read a transient.
pub const METER_RELEASE_PER_SEC: f32 = 0.7;

/// A snapshot of one stream's level, as published by the audio thread and read by
/// the UI. Linear amplitude, `0.0..` (peak may momentarily exceed `1.0` on a
/// loopback that overshoots full scale; the dBFS mapping clamps the bar).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StreamMeter {
    /// Peak (max `|sample|`) over the last published packet.
    pub peak: f32,
    /// RMS (root-mean-square) over the last published packet.
    pub rms: f32,
}

/// One stream's most-recently published level, lock-free. The two `f32`s are held
/// as their `u32` bit patterns; see the module docs for why `Relaxed` is enough.
#[derive(Debug, Default)]
struct StreamLevel {
    peak: AtomicU32,
    rms: AtomicU32,
}

impl StreamLevel {
    fn store(&self, meter: StreamMeter) {
        self.peak.store(meter.peak.to_bits(), Ordering::Relaxed);
        self.rms.store(meter.rms.to_bits(), Ordering::Relaxed);
    }

    fn load(&self) -> StreamMeter {
        StreamMeter {
            peak: f32::from_bits(self.peak.load(Ordering::Relaxed)),
            rms: f32::from_bits(self.rms.load(Ordering::Relaxed)),
        }
    }
}

/// Lock-free per-stream audio levels. The audio-process threads publish; the
/// settings window reads. One slot per [`AudioStreamKind`], addressed **by kind**
/// so there is no producer/consumer index coupling — a writer publishes for its
/// own `kind`, a reader asks for the `kind` it wants to draw. Shared behind an
/// `Arc` between the engine (writers) and the UI (reader).
///
/// Grows with [`AudioStreamKind`] when Slice B (B1) generalises the stream set to
/// N tracks: this file changes with the enum, nothing else.
#[derive(Debug, Default)]
pub struct AudioLevels {
    slots: [StreamLevel; AudioStreamKind::COUNT],
}

// Compile-time guard that every current stream kind indexes within the `slots`
// array. `index()`'s exhaustive match already forces a new Slice-B variant to get
// an arm; this catches the paired mistake of bumping a variant's index past a
// `COUNT` that was not updated with it (which would otherwise panic at runtime on
// the `slots[..]` access).
const _: () = {
    assert!(AudioStreamKind::Desktop.index() < AudioStreamKind::COUNT);
    assert!(AudioStreamKind::Mic.index() < AudioStreamKind::COUNT);
};

impl AudioLevels {
    /// A fresh, all-silent level set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish `kind`'s current level (called from that stream's audio-process
    /// thread, once per captured packet).
    pub fn publish(&self, kind: AudioStreamKind, meter: StreamMeter) {
        self.slots[kind.index()].store(meter);
    }

    /// Read `kind`'s most recent level (called from the UI thread, once per frame
    /// per drawn meter).
    pub fn level(&self, kind: AudioStreamKind) -> StreamMeter {
        self.slots[kind.index()].load()
    }
}

/// Peak (max `|sample|`) and RMS over an interleaved `f32` buffer of any channel
/// count. Returns silence for an empty buffer. Called on the captured packet
/// *before* resampling — resampling does not meaningfully change amplitude, and
/// metering here needs no extra copy of the samples. The RMS accumulator is `f64`
/// so summing ~1 k samples per packet stays numerically clean.
pub fn peak_rms(samples: &[f32]) -> StreamMeter {
    if samples.is_empty() {
        return StreamMeter::default();
    }
    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f64;
    for &s in samples {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
        sum_sq += (s as f64) * (s as f64);
    }
    let rms = (sum_sq / samples.len() as f64).sqrt() as f32;
    StreamMeter { peak, rms }
}

/// Convert a linear amplitude (`0.0..`) to dBFS. Zero or negative maps to
/// [`METER_FLOOR_DBFS`]; tiny positive values that would fall below the floor are
/// clamped up to it. `20·log10(amplitude)`.
pub fn linear_to_dbfs(amplitude: f32) -> f32 {
    if amplitude <= 0.0 {
        return METER_FLOOR_DBFS;
    }
    (20.0 * amplitude.log10()).max(METER_FLOOR_DBFS)
}

/// Map a dBFS value to a `0.0..=1.0` bar fraction across
/// `[METER_FLOOR_DBFS, 0 dBFS]`. At/above 0 dBFS clamps to `1.0`; at/below the
/// floor clamps to `0.0`.
pub fn dbfs_to_fraction(dbfs: f32) -> f32 {
    ((dbfs - METER_FLOOR_DBFS) / -METER_FLOOR_DBFS).clamp(0.0, 1.0)
}

/// Linear amplitude → `0.0..=1.0` meter bar fraction (dBFS-scaled). The
/// composition the UI actually draws with.
pub fn linear_to_fraction(amplitude: f32) -> f32 {
    dbfs_to_fraction(linear_to_dbfs(amplitude))
}

/// One frame of VU decay: the displayed bar snaps up to `target` instantly
/// (attack) but falls toward it at [`METER_RELEASE_PER_SEC`] (release). `dt` is
/// the frame time in seconds. Pure; the UI calls it per frame per meter.
pub fn release_toward(display: f32, target: f32, dt: f32) -> f32 {
    if target >= display {
        target
    } else {
        (display - METER_RELEASE_PER_SEC * dt).max(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two `f32`s are "close" within a small absolute tolerance.
    fn close(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn peak_rms_of_silence_is_zero() {
        assert_eq!(peak_rms(&[0.0; 128]), StreamMeter::default());
    }

    #[test]
    fn peak_rms_of_empty_is_zero() {
        assert_eq!(peak_rms(&[]), StreamMeter::default());
    }

    #[test]
    fn peak_rms_full_scale_square() {
        // Alternating ±1.0: peak 1.0, RMS 1.0.
        let s: Vec<f32> = (0..1000)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let m = peak_rms(&s);
        assert!(close(m.peak, 1.0, 1e-6), "peak {}", m.peak);
        assert!(close(m.rms, 1.0, 1e-6), "rms {}", m.rms);
    }

    #[test]
    fn peak_rms_half_amplitude_pulse_train() {
        // [1, 0, 1, 0, …] at ±0 gives peak 1.0, RMS sqrt(1/2) ≈ 0.7071.
        let s: Vec<f32> = (0..1000)
            .map(|i| if i % 2 == 0 { 1.0 } else { 0.0 })
            .collect();
        let m = peak_rms(&s);
        assert!(close(m.peak, 1.0, 1e-6), "peak {}", m.peak);
        assert!(
            close(m.rms, std::f32::consts::FRAC_1_SQRT_2, 1e-3),
            "rms {}",
            m.rms
        );
    }

    #[test]
    fn peak_tracks_the_largest_magnitude() {
        // A lone negative spike sets the peak even though most samples are small.
        let m = peak_rms(&[0.1, -0.9, 0.2, 0.05]);
        assert!(close(m.peak, 0.9, 1e-6), "peak {}", m.peak);
    }

    #[test]
    fn dbfs_boundaries() {
        assert!(
            close(linear_to_dbfs(1.0), 0.0, 1e-4),
            "full scale is 0 dBFS"
        );
        assert!(
            close(linear_to_dbfs(0.5), -6.0206, 1e-3),
            "half amplitude ≈ −6 dBFS"
        );
        assert_eq!(
            linear_to_dbfs(0.0),
            METER_FLOOR_DBFS,
            "silence clamps to floor"
        );
        assert_eq!(
            linear_to_dbfs(1e-9),
            METER_FLOOR_DBFS,
            "a value far below the floor clamps to it"
        );
    }

    #[test]
    fn fraction_boundaries() {
        assert!(
            close(dbfs_to_fraction(0.0), 1.0, 1e-6),
            "0 dBFS fills the bar"
        );
        assert!(
            close(dbfs_to_fraction(METER_FLOOR_DBFS), 0.0, 1e-6),
            "floor empties it"
        );
        assert!(
            close(dbfs_to_fraction(METER_FLOOR_DBFS / 2.0), 0.5, 1e-6),
            "half the floor is a half bar"
        );
        assert_eq!(
            dbfs_to_fraction(6.0),
            1.0,
            "above full scale clamps to full"
        );
        assert_eq!(
            dbfs_to_fraction(-120.0),
            0.0,
            "below the floor clamps to empty"
        );
    }

    #[test]
    fn linear_to_fraction_endpoints() {
        assert!(close(linear_to_fraction(1.0), 1.0, 1e-6));
        assert_eq!(linear_to_fraction(0.0), 0.0);
    }

    #[test]
    fn release_snaps_up_and_decays_down() {
        // Rising target: instant.
        assert_eq!(release_toward(0.2, 0.9, 0.033), 0.9);
        // Falling: decays by rate·dt, not past the target.
        let dt = 0.1;
        let after = release_toward(1.0, 0.0, dt);
        assert!(
            close(after, 1.0 - METER_RELEASE_PER_SEC * dt, 1e-6),
            "decayed to {after}"
        );
        // A long dt clamps to the target rather than overshooting below it.
        assert_eq!(release_toward(0.5, 0.4, 10.0), 0.4);
    }

    #[test]
    fn publish_then_level_roundtrips_per_kind() {
        let levels = AudioLevels::new();
        // Fresh set reads silent.
        assert_eq!(
            levels.level(AudioStreamKind::Desktop),
            StreamMeter::default()
        );
        assert_eq!(levels.level(AudioStreamKind::Mic), StreamMeter::default());

        let d = StreamMeter {
            peak: 0.8,
            rms: 0.4,
        };
        let m = StreamMeter {
            peak: 0.3,
            rms: 0.1,
        };
        levels.publish(AudioStreamKind::Desktop, d);
        levels.publish(AudioStreamKind::Mic, m);
        // Each kind reads back its own value — no slot cross-talk.
        assert_eq!(levels.level(AudioStreamKind::Desktop), d);
        assert_eq!(levels.level(AudioStreamKind::Mic), m);
    }

    #[test]
    fn audio_levels_is_send_and_sync() {
        // The Arc is shared between the audio threads and the UI thread.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AudioLevels>();
    }
}
