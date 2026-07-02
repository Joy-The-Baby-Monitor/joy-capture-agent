//! Monotonic capture time.
//!
//! Every frame and audio chunk is stamped with a [`Timestamp`] at the moment
//! of capture, and everything downstream — A/V sync, event correlation, WebRTC
//! RTP timestamps — derives from that stamp. The design spec calls this "time
//! discipline": get it right at the source, because retrofitting timing is
//! painful.

use std::fmt;
use std::ops::{Add, Sub};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// The process-wide monotonic epoch all [`Timestamp`]s are measured against.
/// Initialized lazily on the first `Timestamp::now()` call.
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// A monotonic capture timestamp.
///
/// Represents the elapsed time since a process-wide monotonic epoch (roughly,
/// agent startup). Timestamps are:
///
/// - **Monotonic** — backed by [`Instant`], never affected by wall-clock
///   adjustments (NTP steps, DST, manual changes).
/// - **Process-local** — only meaningful, and only comparable, within a single
///   run of the agent. They are *not* wall-clock times; converting an event's
///   capture time to a calendar time (for client display or storage) is a
///   separate concern layered on top by pairing one `Timestamp` with one
///   `SystemTime` reading.
///
/// The internal representation is a [`Duration`] offset from the epoch, giving
/// nanosecond resolution and cheap `Copy` semantics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(Duration);

impl Timestamp {
    /// Captures the current monotonic time.
    ///
    /// This is the one call every capture backend makes as data leaves the
    /// hardware (or the synthesizer, in `--simulate` mode).
    pub fn now() -> Self {
        Self(EPOCH.elapsed())
    }

    /// Builds a timestamp from an explicit offset since the process epoch.
    ///
    /// Intended for sources that derive timestamps arithmetically — e.g. an
    /// audio source anchoring a stream start with [`Timestamp::now`] and then
    /// stamping each chunk as `anchor + samples_emitted / sample_rate`, which
    /// stays drift-free regardless of scheduling jitter.
    pub fn from_offset(offset: Duration) -> Self {
        Self(offset)
    }

    /// Returns this timestamp's offset from the process epoch.
    pub fn as_duration(&self) -> Duration {
        self.0
    }

    /// Returns this timestamp's offset from the process epoch in nanoseconds.
    pub fn as_nanos(&self) -> u128 {
        self.0.as_nanos()
    }

    /// Returns how much monotonic time has passed since this timestamp,
    /// saturating to zero if `self` is in the future.
    pub fn elapsed(&self) -> Duration {
        Timestamp::now().saturating_since(*self)
    }

    /// Returns the duration from `earlier` to `self`, or `None` if `earlier`
    /// is actually later than `self`.
    pub fn checked_since(&self, earlier: Timestamp) -> Option<Duration> {
        self.0.checked_sub(earlier.0)
    }

    /// Returns the duration from `earlier` to `self`, saturating to zero if
    /// `earlier` is actually later than `self`.
    pub fn saturating_since(&self, earlier: Timestamp) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
}

impl Add<Duration> for Timestamp {
    type Output = Timestamp;

    fn add(self, rhs: Duration) -> Timestamp {
        Timestamp(self.0 + rhs)
    }
}

impl Sub<Timestamp> for Timestamp {
    type Output = Duration;

    /// Saturating difference: `later - earlier`. Yields zero rather than
    /// panicking if the operands are reversed.
    fn sub(self, rhs: Timestamp) -> Duration {
        self.saturating_since(rhs)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.6}s", self.0.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_monotonic() {
        let a = Timestamp::now();
        let b = Timestamp::now();
        let c = Timestamp::now();
        assert!(a <= b && b <= c);
    }

    #[test]
    fn arithmetic_round_trips() {
        let base = Timestamp::from_offset(Duration::from_millis(100));
        let later = base + Duration::from_millis(50);
        assert_eq!(later - base, Duration::from_millis(50));
        assert_eq!(later.checked_since(base), Some(Duration::from_millis(50)));
        assert_eq!(base.checked_since(later), None);
        assert_eq!(base - later, Duration::ZERO);
    }

    #[test]
    fn derived_timestamps_are_exact() {
        let anchor = Timestamp::from_offset(Duration::ZERO);
        let rate = 48_000u64;
        let chunk = 960u64;
        let ts = anchor + Duration::from_nanos(chunk * 1_000_000_000 / rate);
        assert_eq!(ts.as_duration(), Duration::from_millis(20));
    }
}
