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

use crate::audio::wasapi_stream::AudioTrackKind;

/// Meter floor: amplitudes at or below this dBFS map to an empty bar. −60 dBFS is
/// the conventional noise floor for a compact VU meter — below it the signal is
/// inaudible for metering purposes.
pub const METER_FLOOR_DBFS: f32 = -60.0;

/// Meter **attack** time constant (seconds): how quickly the displayed bar rises toward
/// a louder signal. Short but **non-zero** — the old instant attack snapped the bar to
/// every momentary packet RMS (~100 Hz publish), which read as flicker (fluctuating tens
/// of times a second). ~90 ms integrates that into a smooth, still-responsive rise.
pub const METER_ATTACK_TAU: f32 = 0.09;

/// Meter **release** time constant (seconds): how slowly the bar falls once the signal
/// drops. Longer than the attack (the classic VU asymmetry) so a transient stays
/// readable. ~350 ms ≈ the usual VU falloff.
pub const METER_RELEASE_TAU: f32 = 0.35;

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
/// settings window reads. One slot per [`AudioTrackKind`], addressed **by kind**
/// so there is no producer/consumer index coupling — a writer publishes for its
/// own `kind`, a reader asks for the `kind` it wants to draw. Shared behind an
/// `Arc` between the engine (writers) and the UI (reader).
///
/// Grows with [`AudioTrackKind`] when Slice B (B1) generalises the stream set to
/// N tracks: this file changes with the enum, nothing else.
#[derive(Debug, Default)]
pub struct AudioLevels {
    slots: [StreamLevel; AudioTrackKind::COUNT],
}

// Compile-time guard that every current stream kind indexes within the `slots`
// array. `index()`'s exhaustive match already forces a new Slice-B variant to get
// an arm; this catches the paired mistake of bumping a variant's index past a
// `COUNT` that was not updated with it (which would otherwise panic at runtime on
// the `slots[..]` access).
const _: () = {
    assert!(AudioTrackKind::Mix.index() < AudioTrackKind::COUNT);
    assert!(AudioTrackKind::Game.index() < AudioTrackKind::COUNT);
    assert!(AudioTrackKind::VoiceChat.index() < AudioTrackKind::COUNT);
    assert!(AudioTrackKind::OtherSystem.index() < AudioTrackKind::COUNT);
    assert!(AudioTrackKind::Mic.index() < AudioTrackKind::COUNT);
};

impl AudioLevels {
    /// A fresh, all-silent level set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish `kind`'s current level (called from that stream's audio-process
    /// thread, once per captured packet).
    pub fn publish(&self, kind: AudioTrackKind, meter: StreamMeter) {
        self.slots[kind.index()].store(meter);
    }

    /// Read `kind`'s most recent level (called from the UI thread, once per frame
    /// per drawn meter).
    pub fn level(&self, kind: AudioTrackKind) -> StreamMeter {
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

/// One frame of VU ballistics: exponentially smooth `display` toward `target` with a
/// **fast attack** (rising) and a **slower release** (falling) time constant. `dt` is the
/// frame time in seconds. Pure; the UI calls it per frame per meter. Smoothing BOTH
/// directions — rather than the old instant attack — is what removes the flicker.
pub fn smooth_toward(display: f32, target: f32, dt: f32) -> f32 {
    let tau = if target >= display {
        METER_ATTACK_TAU
    } else {
        METER_RELEASE_TAU
    };
    // Exponential approach toward the target; guard a zero/negative frame time.
    let alpha = if dt > 0.0 {
        1.0 - (-dt / tau).exp()
    } else {
        0.0
    };
    display + (target - display) * alpha
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
    fn smooth_toward_rises_fast_falls_slow_and_is_bounded() {
        let dt = 0.033;
        // Rising: moves toward the target in one frame, but NOT instantly (the flicker fix).
        let up = smooth_toward(0.2, 0.9, dt);
        assert!(up > 0.2 && up < 0.9, "rose to {up}");
        // Attack is faster than release: from the same-size gap, rising moves further.
        let rise_step = smooth_toward(0.0, 1.0, dt);
        let fall_step = 1.0 - smooth_toward(1.0, 0.0, dt);
        assert!(
            rise_step > fall_step,
            "attack {rise_step} should exceed release {fall_step}"
        );
        // Converges to (never overshoots) the target over a long frame.
        assert!(close(smooth_toward(0.5, 0.4, 10.0), 0.4, 1e-3));
        // A zero frame time is a no-op.
        assert_eq!(smooth_toward(0.3, 0.9, 0.0), 0.3);
    }

    #[test]
    fn publish_then_level_roundtrips_per_kind() {
        // Every track kind, so the 5-slot array (B1) is exercised end to end. Distinct
        // values per kind catch any slot cross-talk (a bad `index()`).
        let all = [
            AudioTrackKind::Mix,
            AudioTrackKind::Game,
            AudioTrackKind::VoiceChat,
            AudioTrackKind::OtherSystem,
            AudioTrackKind::Mic,
        ];
        let levels = AudioLevels::new();
        // Fresh set reads silent for every kind.
        for k in all {
            assert_eq!(levels.level(k), StreamMeter::default());
        }
        // Publish a unique meter per kind…
        for (i, k) in all.into_iter().enumerate() {
            levels.publish(
                k,
                StreamMeter {
                    peak: 0.1 * (i as f32 + 1.0),
                    rms: 0.05 * (i as f32 + 1.0),
                },
            );
        }
        // …and read each back exactly, with no cross-talk between slots.
        for (i, k) in all.into_iter().enumerate() {
            assert_eq!(
                levels.level(k),
                StreamMeter {
                    peak: 0.1 * (i as f32 + 1.0),
                    rms: 0.05 * (i as f32 + 1.0),
                },
                "kind {:?} read back a wrong slot",
                k
            );
        }
    }

    #[test]
    fn audio_levels_is_send_and_sync() {
        // The Arc is shared between the audio threads and the UI thread.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AudioLevels>();
    }
}
