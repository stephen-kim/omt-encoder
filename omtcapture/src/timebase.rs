/// System monotonic timestamp in 100ns units (1 second = 10_000_000).
///
/// Uses raw CLOCK_MONOTONIC so timestamps match OBS's os_gettime_ns() / 100.
/// Both audio and video must use the same timebase for A/V sync.
#[cfg(target_os = "linux")]
pub fn monotonic_100ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as i64 * 10_000_000) + (ts.tv_nsec as i64 / 100)
}

#[cfg(not(target_os = "linux"))]
pub fn monotonic_100ns() -> i64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    (start.elapsed().as_nanos() / 100) as i64
}
