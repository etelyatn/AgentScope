//! Work counters for the deterministic budget tests in `tests/perf_budget.rs`.
//!
//! With the `perf-counters` feature (enabled for the test build only) each hook adds
//! to thread-local totals, so concurrently running tests never see each other's work.
//! Without the feature every hook is an empty function and the counters do not exist.

/// Totals recorded on the current thread since the last [`reset`].
#[cfg(feature = "perf-counters")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Source bytes consumed or read from provider files.
    pub bytes_read: u64,
    /// JSON records handed to the parser.
    pub records_parsed: u64,
    /// Bytes of preview text allocated for display.
    pub preview_bytes: u64,
    /// Largest single preview allocated.
    pub max_preview_bytes: u64,
}

#[cfg(feature = "perf-counters")]
thread_local! {
    static COUNTERS: std::cell::Cell<Counters> = const { std::cell::Cell::new(Counters {
        bytes_read: 0,
        records_parsed: 0,
        preview_bytes: 0,
        max_preview_bytes: 0,
    }) };
}

#[cfg(feature = "perf-counters")]
fn update(f: impl FnOnce(&mut Counters)) {
    COUNTERS.with(|cell| {
        let mut counters = cell.get();
        f(&mut counters);
        cell.set(counters);
    });
}

/// Zeroes this thread's counters.
#[cfg(feature = "perf-counters")]
pub fn reset() {
    COUNTERS.with(|cell| cell.set(Counters::default()));
}

/// This thread's counters.
#[cfg(feature = "perf-counters")]
pub fn snapshot() -> Counters {
    COUNTERS.with(std::cell::Cell::get)
}

#[cfg(feature = "perf-counters")]
pub(crate) fn bytes_read(bytes: usize) {
    update(|c| c.bytes_read += bytes as u64);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
pub(crate) fn bytes_read(_bytes: usize) {}

#[cfg(feature = "perf-counters")]
pub(crate) fn record_parsed() {
    update(|c| c.records_parsed += 1);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
pub(crate) fn record_parsed() {}

#[cfg(feature = "perf-counters")]
pub(crate) fn preview_allocated(bytes: usize) {
    update(|c| {
        c.preview_bytes += bytes as u64;
        c.max_preview_bytes = c.max_preview_bytes.max(bytes as u64);
    });
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
pub(crate) fn preview_allocated(_bytes: usize) {}
