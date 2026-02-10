use std::sync::OnceLock;
use std::time::Instant;

/// Monotonic timestamp in 100ns units (1 second = 10_000_000).
///
/// OMT expects timestamps in 100ns units, and receivers may use them for A/V sync and buffering.
/// Use a single monotonic timebase for both audio and video to avoid large offsets/drift.
pub fn monotonic_100ns() -> i64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    (start.elapsed().as_nanos() / 100) as i64
}

