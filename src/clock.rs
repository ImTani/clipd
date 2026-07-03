//! The single master clock domain: QueryPerformanceCounter → ticks.
//!
//! `02-AV-SYNC-SPEC.md §0`: *"There is exactly one clock domain in the entire
//! program. No component may keep time by counting samples or frames."* Raw QPC
//! converted to ticks (100 ns units) IS that domain.
//!
//! Apartment / threading: the QPC syscalls here are apartment-agnostic and
//! thread-safe; no `CoInitialize` is required to call them. `unsafe` in this
//! module is confined to the two FFI calls that read the performance counter and
//! its frequency (the syscall boundary). The conversion math and the
//! monotonicity guard are 100% safe and unit-tested.

use crate::spec_constants::units::TICKS_PER_SECOND;

/// Errors from establishing the clock. `QueryPerformanceCounter` itself cannot
/// fail on Windows XP+ (per Microsoft docs), so only frequency acquisition is
/// fallible in practice.
#[derive(Debug, thiserror::Error)]
pub enum ClockError {
    /// `QueryPerformanceFrequency` failed or reported a non-positive frequency.
    #[error("QueryPerformanceFrequency failed or returned a non-positive value")]
    QueryFrequencyFailed,
}

/// Convert a raw QPC value to ticks (100 ns units) given the QPC frequency.
///
/// `02-AV-SYNC-SPEC.md §0`:
///
/// > read QueryPerformanceFrequency once and build the conversion
/// > `ticks = qpc * 10_000_000 / qpf` using 128-bit intermediate math
/// > (`i128` mul-then-div) to avoid overflow.
///
/// The `i128` intermediate is mandatory: at the common 10 MHz QPC rate,
/// `qpc * 10_000_000` overflows `i64` after ~2.9 hours of uptime.
///
/// This function is pure and total: it is the exhaustively-tested core of the
/// clock. `qpf` must be positive (guaranteed by [`Clock::new`]); a zero `qpf`
/// would panic on divide-by-zero, which is why construction validates it.
#[inline]
pub const fn qpc_to_ticks(qpc: i64, qpf: i64) -> i64 {
    ((qpc as i128 * TICKS_PER_SECOND as i128) / qpf as i128) as i64
}

/// The master clock: caches the (immutable) QPC frequency and converts raw
/// counter reads into ticks. `qpf` never changes for the life of the process
/// (`§0`), so it is read once at construction.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    qpf: i64,
}

impl Clock {
    /// Construct from an explicit frequency. Used by tests and by
    /// [`Clock::from_system`]. Returns [`ClockError::QueryFrequencyFailed`] for
    /// a non-positive frequency so the divisor in [`qpc_to_ticks`] is always
    /// valid.
    pub fn new(qpf: i64) -> Result<Self, ClockError> {
        if qpf <= 0 {
            return Err(ClockError::QueryFrequencyFailed);
        }
        Ok(Self { qpf })
    }

    /// Read `QueryPerformanceFrequency` once and build the clock. `§0`.
    pub fn from_system() -> Result<Self, ClockError> {
        Self::new(query_performance_frequency()?)
    }

    /// The cached QPC frequency (Hz).
    #[inline]
    pub fn qpf(&self) -> i64 {
        self.qpf
    }

    /// Convert a raw QPC reading to ticks in the master domain.
    #[inline]
    pub fn ticks_from_qpc(&self, qpc: i64) -> i64 {
        qpc_to_ticks(qpc, self.qpf)
    }

    /// Read the counter now and return the current master-domain tick. `§0`.
    #[inline]
    pub fn now_ticks(&self) -> i64 {
        self.ticks_from_qpc(query_performance_counter())
    }
}

/// The monotonicity guard from `02-AV-SYNC-SPEC.md §0`:
///
/// > any producer emitting a packet with `pts <= previous_pts` of the same
/// > stream MUST bump it to `previous_pts + 1` tick and increment a
/// > `ts_violation` counter.
///
/// One guard instance per producer/stream. It is a diagnostic canary — the
/// [`violations`](Self::violations) count staying at 0 is the expected steady
/// state; the bump keeps the PTS sequence strictly increasing regardless.
#[derive(Debug, Default, Clone)]
pub struct MonotonicGuard {
    last: Option<i64>,
    violations: u64,
}

impl MonotonicGuard {
    /// A fresh guard with no prior packet and a zero violation count.
    pub fn new() -> Self {
        Self::default()
    }

    /// Admit a candidate PTS, returning the PTS to actually use. If `pts` is
    /// not strictly greater than the previous admitted PTS, it is bumped to
    /// `previous + 1` and the violation counter is incremented.
    pub fn admit(&mut self, pts: i64) -> i64 {
        let out = match self.last {
            Some(prev) if pts <= prev => {
                self.violations += 1;
                prev + 1
            }
            _ => pts,
        };
        self.last = Some(out);
        out
    }

    /// The number of monotonicity violations observed. Logged every
    /// [`crate::spec_constants::monotonicity::TS_VIOLATION_LOG_INTERVAL_SECONDS`]
    /// if nonzero, by the watchdog.
    #[inline]
    pub fn violations(&self) -> u64 {
        self.violations
    }

    /// The most recently admitted PTS, if any.
    #[inline]
    pub fn last(&self) -> Option<i64> {
        self.last
    }
}

/// Read `QueryPerformanceFrequency`. `§0`: cache the result; it never changes.
fn query_performance_frequency() -> Result<i64, ClockError> {
    let mut freq: i64 = 0;
    // SAFETY: `freq` is a valid, aligned, writable `i64`. The Win32 API writes
    // the counter frequency into it and returns Ok on success. No aliasing: we
    // hold the only reference for the duration of the call.
    unsafe {
        windows::Win32::System::Performance::QueryPerformanceFrequency(&mut freq)
            .map_err(|_| ClockError::QueryFrequencyFailed)?;
    }
    if freq <= 0 {
        return Err(ClockError::QueryFrequencyFailed);
    }
    Ok(freq)
}

/// Read `QueryPerformanceCounter`. Per Microsoft docs this cannot fail on
/// Windows XP or later; if the (documented-impossible) failure occurs we return
/// the last-known-good behavior by yielding 0, which the monotonicity guard on
/// each stream will then correct into a strictly increasing sequence.
fn query_performance_counter() -> i64 {
    let mut count: i64 = 0;
    // SAFETY: `count` is a valid, aligned, writable `i64` held exclusively for
    // the call. The Win32 API writes the current counter value into it.
    unsafe {
        let _ = windows::Win32::System::Performance::QueryPerformanceCounter(&mut count);
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_mhz_qpc_maps_one_to_one() {
        // §0: at the typical 10 MHz QPC rate, 1 QPC unit = 100 ns = 1 tick.
        let qpf = 10_000_000;
        assert_eq!(qpc_to_ticks(0, qpf), 0);
        assert_eq!(qpc_to_ticks(1, qpf), 1);
        assert_eq!(qpc_to_ticks(10_000_000, qpf), 10_000_000); // 1 s
    }

    #[test]
    fn three_mhz_qpc_scales_correctly() {
        // A 3 MHz counter: one second of counts (3_000_000) is 10_000_000 ticks.
        let qpf = 3_000_000;
        assert_eq!(qpc_to_ticks(3_000_000, qpf), 10_000_000);
        // Non-exact division floors: 7 counts at 3 MHz = 70_000_000/3_000_000
        // = 23.33 → 23 (floored).
        assert_eq!(
            qpc_to_ticks(7, qpf),
            (7i128 * 10_000_000 / 3_000_000) as i64
        );
        assert_eq!(qpc_to_ticks(7, qpf), 23);
    }

    #[test]
    fn i128_intermediate_prevents_overflow() {
        // §0: qpc * 10_000_000 overflows i64 well within realistic uptimes.
        // At 10 MHz, ~2.9 h of counts already overflows the naive i64 product.
        let qpf = 10_000_000;
        let qpc = 3_000_000_000_000i64; // ~83 h of counts; product is ~3e19 > i64::MAX
                                        // Naive i64 product would overflow; the i128 path yields the true value.
        assert_eq!(qpc_to_ticks(qpc, qpf), 3_000_000_000_000);

        // Even larger, near a value whose *tick* result stays in range.
        let qpc = 9_000_000_000_000i64;
        assert_eq!(qpc_to_ticks(qpc, qpf), 9_000_000_000_000);
    }

    #[test]
    fn clock_rejects_nonpositive_frequency() {
        assert!(Clock::new(0).is_err());
        assert!(Clock::new(-1).is_err());
        assert!(Clock::new(1).is_ok());
    }

    #[test]
    fn clock_converts_via_cached_frequency() {
        let c = Clock::new(10_000_000).unwrap();
        assert_eq!(c.qpf(), 10_000_000);
        assert_eq!(c.ticks_from_qpc(5_000_000), 5_000_000);
    }

    #[test]
    fn guard_passes_strictly_increasing_untouched() {
        let mut g = MonotonicGuard::new();
        assert_eq!(g.admit(100), 100);
        assert_eq!(g.admit(101), 101);
        assert_eq!(g.admit(1_000), 1_000);
        assert_eq!(g.violations(), 0);
    }

    #[test]
    fn guard_bumps_equal_pts() {
        // The `<=` boundary from §0: an equal PTS is a violation.
        let mut g = MonotonicGuard::new();
        assert_eq!(g.admit(100), 100);
        assert_eq!(g.admit(100), 101);
        assert_eq!(g.violations(), 1);
        assert_eq!(g.last(), Some(101));
    }

    #[test]
    fn guard_bumps_decreasing_pts_and_chains() {
        let mut g = MonotonicGuard::new();
        assert_eq!(g.admit(500), 500);
        assert_eq!(g.admit(490), 501); // bumped to prev+1
        assert_eq!(g.admit(495), 502); // still behind the bumped value → bump
        assert_eq!(g.admit(600), 600); // finally ahead → passes
        assert_eq!(g.violations(), 2);
    }

    #[test]
    fn guard_first_admit_never_violates() {
        let mut g = MonotonicGuard::new();
        assert_eq!(g.admit(-42), -42);
        assert_eq!(g.violations(), 0);
    }

    #[test]
    fn system_clock_is_monotonic_nondecreasing() {
        // Runs on any Windows host incl. the CI runner (no GPU needed). QPC is
        // monotonic, so successive reads never go backwards.
        let c = Clock::from_system().expect("QPF available on Windows");
        let a = c.now_ticks();
        let b = c.now_ticks();
        assert!(b >= a, "QPC went backwards: {a} -> {b}");
    }
}
