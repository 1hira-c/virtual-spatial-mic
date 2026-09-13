#[cfg(windows)]
pub fn ticks() -> i64 {
    let mut v = 0;
    unsafe {
        windows::Win32::System::Performance::QueryPerformanceCounter(&mut v)
            .expect("Windows performance counter");
    }
    v
}
#[cfg(windows)]
pub fn frequency() -> i64 {
    static FREQ: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut v = 0;
        unsafe {
            windows::Win32::System::Performance::QueryPerformanceFrequency(&mut v)
                .expect("Windows performance frequency");
        }
        v
    })
}
#[cfg(windows)]
pub fn now_ns() -> i64 {
    ((ticks() as i128 * 1_000_000_000) / frequency() as i128) as i64
}
#[cfg(not(windows))]
pub fn now_ns() -> i64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_nanos() as i64
}
#[cfg(not(windows))]
pub fn ticks() -> i64 {
    now_ns()
}
#[cfg(not(windows))]
pub fn frequency() -> i64 {
    1_000_000_000
}
