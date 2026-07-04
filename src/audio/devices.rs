//! `audio::devices` — the `§7` device-change state machine for one capture stream.
//!
//! A stream progresses RUNNING → (event) → DRAINING → REBUILDING → RUNNING. Two
//! things trigger a rebuild, per `02-AV-SYNC-SPEC.md §7`:
//!
//! 1. **Invalidation** — any WASAPI call returning an error (the classic being
//!    `AUDCLNT_E_DEVICE_INVALIDATED` on unplug) transitions **immediately** to
//!    REBUILDING (skip debounce). This is the [`AV-4`] path (unplug/replug mic).
//! 2. **Default switch** — an `IMMNotificationClient::OnDefaultDeviceChanged` for
//!    this stream's data flow (`default-follow` policy) is **debounced 250 ms**
//!    (Windows fires bursts of 3–6 events on one switch) before the rebuild.
//!
//! The gap between the last good packet and the first packet after the rebuild is
//! filled by the existing `§2.3` silence synthesizer downstream — it needs no
//! special case here, because the QPC PTS simply jumps forward by the hole and
//! [`crate::audio::resample::StreamResampler`] (which **survives** the rebuild —
//! only the WASAPI client below it is recreated) fills it. Worst-case hole
//! 250 + 500 = 750 ms of silence, zero desync, zero crash.
//!
//! ## `unsafe`
//! Confined to the [`DefaultChangeWatcher`] COM registration (CLAUDE.md: `unsafe`
//! lives in COM wrappers). The debounce state machine ([`Debouncer`]) and the
//! [`DeviceSelection`] policy are pure, safe, and unit-tested. The watcher is
//! created, used, and dropped entirely on the capture thread (MTA) — its COM
//! objects never cross a thread boundary, so it needs no `unsafe impl Send`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tracing::warn;
use windows::core::{implement, PCWSTR};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use crate::spec_constants::device::IMM_DEBOUNCE_MS;
use crate::spec_constants::units::TICKS_PER_SECOND;

use super::wasapi_stream::AudioStreamKind;

/// Which endpoint a stream binds to (`§7` device-selection policy). Desktop
/// loopback always follows the default render endpoint; the mic is configurable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceSelection {
    /// Follow the Windows default endpoint; a default switch rebuilds to chase it.
    DefaultFollow,
    /// A pinned endpoint id: rebuild only on invalidation, and if the pinned
    /// device is gone, record silence + WARNING — never silently substitute a
    /// different device (`§7`: "that is the incumbent sin").
    Pinned(String),
}

impl DeviceSelection {
    /// Derive the selection for a stream from its config string. Desktop ignores
    /// the value (always `DefaultFollow` on render); the mic honours
    /// `"default-follow"` (or `"default-*"`) vs a pinned endpoint id. `"off"` is
    /// handled upstream (the stream is not started at all), so it maps to
    /// `DefaultFollow` defensively.
    pub fn for_mic(mic_cfg: &str) -> Self {
        let v = mic_cfg.trim();
        if v.is_empty() || v == "off" || v.starts_with("default") {
            DeviceSelection::DefaultFollow
        } else {
            DeviceSelection::Pinned(v.to_string())
        }
    }
}

/// The data flow (`eRender`/`eCapture`) whose default this stream tracks.
pub fn stream_flow(kind: AudioStreamKind) -> EDataFlow {
    match kind {
        // Desktop loopback captures the default *render* endpoint, so it chases
        // the default render device; the mic chases the default capture device.
        AudioStreamKind::Desktop => eRender,
        AudioStreamKind::Mic => eCapture,
    }
}

/// The leading-edge debounce that coalesces a burst of default-change events into
/// a single rebuild, fired a fixed `§7` window (250 ms) after the **first** event.
///
/// Pure and unit-tested. The capture loop calls [`Self::signal`] whenever it
/// observes the notification flag set, and [`Self::due`] each tick; `due` returns
/// `true` exactly once per burst, once the window has elapsed.
#[derive(Debug)]
pub struct Debouncer {
    window_ticks: i64,
    /// The tick at which a pending rebuild becomes due, or `None` when idle.
    deadline: Option<i64>,
}

impl Debouncer {
    /// A debouncer with the `§7` 250 ms window.
    pub fn new() -> Self {
        Self::with_window_ms(IMM_DEBOUNCE_MS)
    }

    /// A debouncer with an explicit window (tests use small values).
    pub fn with_window_ms(window_ms: i64) -> Self {
        Self {
            window_ticks: window_ms * TICKS_PER_SECOND / 1000,
            deadline: None,
        }
    }

    /// Arm the window on the *first* event of a burst; later events within the
    /// same window are absorbed (leading-edge: the deadline is not pushed out).
    pub fn signal(&mut self, now: i64) {
        if self.deadline.is_none() {
            self.deadline = Some(now + self.window_ticks);
        }
    }

    /// Whether a rebuild is currently pending (armed but not yet due).
    pub fn pending(&self) -> bool {
        self.deadline.is_some()
    }

    /// Returns `true` (and disarms) once, when `now` reaches the armed deadline.
    pub fn due(&mut self, now: i64) -> bool {
        match self.deadline {
            Some(deadline) if now >= deadline => {
                self.deadline = None;
                true
            }
            _ => false,
        }
    }
}

impl Default for Debouncer {
    fn default() -> Self {
        Self::new()
    }
}

/// COM sink for `IMMNotificationClient`: flips a shared flag when the default
/// endpoint for `flow` changes (Console role). The capture thread owns the flag
/// and drives the [`Debouncer`]; the callback stays trivial (Windows calls it on
/// its own MTA thread, so it must not block or touch the WASAPI client).
#[implement(IMMNotificationClient)]
struct DefaultChangeClient {
    flow: EDataFlow,
    flagged: Arc<AtomicBool>,
}

#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for DefaultChangeClient_Impl {
    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        _default_device_id: &PCWSTR,
    ) -> windows::core::Result<()> {
        // Only the Console-role default for our own data flow matters (§7
        // default-follow). Multimedia/Communications roles and the other flow
        // are ignored so we don't rebuild on an unrelated switch.
        if flow == self.flow && role == eConsole {
            self.flagged.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    fn OnDeviceStateChanged(
        &self,
        _device_id: &PCWSTR,
        _new_state: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDeviceAdded(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDeviceRemoved(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _device_id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Registers a [`DefaultChangeClient`] with an `IMMDeviceEnumerator` for the
/// lifetime of the guard, unregistering on drop. Lives on the capture thread.
pub struct DefaultChangeWatcher {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
}

impl DefaultChangeWatcher {
    /// Register a default-change watcher for `kind`'s data flow, flipping
    /// `flagged` when the tracked default switches. COM must already be
    /// initialized on this thread (the capture thread's MTA guard).
    pub fn register(
        kind: AudioStreamKind,
        flagged: Arc<AtomicBool>,
    ) -> windows::core::Result<Self> {
        // SAFETY: `MMDeviceEnumerator` is the documented CLSID for the endpoint
        // enumerator; we create an in-proc instance on this MTA thread and hold
        // it for the guard's lifetime. No raw pointers escape.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let client: IMMNotificationClient = DefaultChangeClient {
            flow: stream_flow(kind),
            flagged,
        }
        .into();
        // SAFETY: registering our sink; the enumerator keeps a ref-counted
        // pointer to `client`, released by the matching unregister in `Drop`.
        unsafe { enumerator.RegisterEndpointNotificationCallback(&client)? };
        Ok(Self { enumerator, client })
    }
}

impl Drop for DefaultChangeWatcher {
    fn drop(&mut self) {
        // SAFETY: undo the registration done in `register`; both objects are the
        // same ones passed there and live on this thread. Log-and-continue on
        // failure — this runs during teardown.
        if let Err(e) = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.client)
        } {
            warn!(error = %e, "failed to unregister device-change watcher");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_selection_maps_default_and_pinned() {
        assert_eq!(
            DeviceSelection::for_mic("default-follow"),
            DeviceSelection::DefaultFollow
        );
        assert_eq!(
            DeviceSelection::for_mic("  default-communications "),
            DeviceSelection::DefaultFollow
        );
        // "off" is handled upstream; map defensively to DefaultFollow.
        assert_eq!(
            DeviceSelection::for_mic("off"),
            DeviceSelection::DefaultFollow
        );
        assert_eq!(
            DeviceSelection::for_mic("{0.0.1.00000000}.{abc}"),
            DeviceSelection::Pinned("{0.0.1.00000000}.{abc}".to_string())
        );
    }

    #[test]
    fn debouncer_fires_once_a_fixed_window_after_first_event() {
        let mut d = Debouncer::with_window_ms(250);
        let w = 250 * TICKS_PER_SECOND / 1000; // 2_500_000 ticks
        assert!(!d.pending());
        d.signal(1_000_000);
        assert!(d.pending());
        // Not yet due before the window elapses.
        assert!(!d.due(1_000_000 + w - 1));
        // Due exactly at the deadline, and it disarms (fires once).
        assert!(d.due(1_000_000 + w));
        assert!(!d.pending());
        assert!(!d.due(1_000_000 + w + 10));
    }

    #[test]
    fn debouncer_absorbs_a_burst_into_one_deadline() {
        let mut d = Debouncer::with_window_ms(250);
        let w = 250 * TICKS_PER_SECOND / 1000;
        d.signal(1_000_000);
        // A burst of further events must NOT push the deadline out (leading edge).
        d.signal(1_050_000);
        d.signal(1_100_000);
        assert!(!d.due(1_000_000 + w - 1));
        assert!(d.due(1_000_000 + w));
    }

    #[test]
    fn debouncer_rearms_after_firing() {
        let mut d = Debouncer::with_window_ms(250);
        let w = 250 * TICKS_PER_SECOND / 1000;
        d.signal(0);
        assert!(d.due(w));
        // A second, later burst arms a fresh window.
        d.signal(10 * w);
        assert!(!d.due(10 * w + w - 1));
        assert!(d.due(10 * w + w));
    }
}
