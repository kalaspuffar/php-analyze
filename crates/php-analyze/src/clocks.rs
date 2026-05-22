//! Clock and memory snapshot primitives for the recorder hot path.
//!
//! Per `SPECIFICATION.md` §3.2 (clock-sources table) and OQ-8:
//!
//! - [`monotonic_now_ns`]    backs `CallFrame::t_in_ns` / `CallRecord::t_in_ns` /
//!   `t_out_ns`. `CLOCK_MONOTONIC` is monotonic within a process, so subtracting
//!   two reads never yields a negative duration even if the wall clock is
//!   stepped backwards mid-trace.
//! - [`cpu_times_now_ns`]    backs `CallRecord::cpu_u_ns` / `cpu_s_ns` (via
//!   per-call deltas). `getrusage(RUSAGE_THREAD)` returns `timeval`s whose
//!   resolution is typically microseconds; sub-microsecond calls may read `0`
//!   on either component. R-11 in `SPECIFICATION.md` §11 accepts this.
//!   The `RUSAGE_THREAD` (Linux 2.6.26+) choice — rather than `RUSAGE_SELF`
//!   — scopes the CPU accounting to the calling thread, so the Phase-4
//!   shipper thread's CPU does not bleed into the request thread's per-call
//!   deltas. See `SPECIFICATION.md` §3.2 for the spec amendment.
//! - [`realtime_now_ns`]     backs `Trace::start_time_realtime_ns` and the
//!   corresponding `MetaPartial` field. **Anchor only** — never participates
//!   in subtraction; the recorder uses [`monotonic_now_ns`] for durations.
//! - [`memory_usage_real_bytes`] backs `CallFrame::mem_in_bytes` /
//!   `CallRecord::mem_in_bytes` / `mem_out_bytes`. Wraps Zend
//!   `zend_memory_usage(true)`.
//!
//! ## libc vs `ext_php_rs::ffi`
//!
//! `libc` is the source for POSIX symbols. `ext_php_rs::ffi` only re-exports
//! Zend symbols; it carries no `clock_gettime` or `getrusage` binding. `libc`
//! is already in the workspace's transitive graph as a build-dep of
//! `ext-php-rs-bindgen`, so listing it directly costs zero new crates.
//!
//! ## Test build vs production build
//!
//! [`memory_usage_real_bytes`] is the only function in this module that
//! touches Zend FFI. Its body is gated by `cfg`: production builds call the
//! real `zend_memory_usage`; `cargo test` builds return `0` so the test binary
//! links without `libphp.so`. The end-to-end behaviour of the real symbol is
//! exercised in Phase-2 slice 2's PHP fixture (a deliberate-allocation script
//! with an asserted `mem_out - mem_in` delta on the resulting `CallRecord`).

/// Per-process user and system CPU consumption, in nanoseconds.
///
/// Returned by [`cpu_times_now_ns`]. The granularity is set by the host
/// kernel's `getrusage` resolution — typically microseconds on Linux x86_64.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuTimes {
    /// Total user-space CPU time used by the calling process.
    pub user_ns: i64,
    /// Total kernel-space CPU time used by the calling process.
    pub system_ns: i64,
}

/// Monotonic nanoseconds since an arbitrary, process-local epoch.
///
/// Backed by `clock_gettime(CLOCK_MONOTONIC, …)`. The returned value is
/// guaranteed not to decrease across successive calls within the same process,
/// which is the property the recorder relies on for `t_out - t_in`
/// subtraction (AC-RC-6 in `SPECIFICATION.md` §3.2). The conversion from
/// `timespec` to `i64` is `tv_sec * 1_000_000_000 + tv_nsec`; both inputs are
/// safe in `i64` for any conceivable trace duration.
pub fn monotonic_now_ns() -> i64 {
    clock_gettime_ns(libc::CLOCK_MONOTONIC)
}

/// Wall-clock nanoseconds since the UNIX epoch.
///
/// Backed by `clock_gettime(CLOCK_REALTIME, …)`. Used **once per trace** to
/// populate `Trace::start_time_realtime_ns` and the corresponding wire field
/// (`SPECIFICATION.md` §1.4 OQ-8). The recorder never subtracts two
/// `realtime_now_ns` reads — duration arithmetic uses [`monotonic_now_ns`].
pub fn realtime_now_ns() -> i64 {
    clock_gettime_ns(libc::CLOCK_REALTIME)
}

/// Per-thread CPU consumption (user + system), in nanoseconds.
///
/// Backed by `getrusage(RUSAGE_THREAD, …)`. Each `timeval` field
/// (`ru_utime`, `ru_stime`) is converted to nanoseconds as
/// `tv_sec * 1_000_000_000 + tv_usec * 1_000`.
///
/// ## Why `RUSAGE_THREAD` and not `RUSAGE_SELF`
///
/// `RUSAGE_SELF` sums CPU across every thread of the process. Once the
/// Phase-4 shipper thread exists, that would mean a `cpu_u/s_ns` delta
/// computed around a PHP call includes whatever CPU the shipper consumed
/// during the same interval — inflating per-call CPU readings under load.
/// `RUSAGE_THREAD` (Linux 2.6.26+) accounts only the calling thread; the
/// recorder runs on the PHP request thread, which is the only thread whose
/// CPU is meaningful for `(t_in, t_out)`-scoped CPU measurement. The spec
/// was amended in lockstep (`SPECIFICATION.md` §3.2 / §7.4).
///
/// **Granularity caveat** (R-11 in `SPECIFICATION.md` §11): `getrusage`
/// typically reports microseconds, so a `CallRecord` whose entire body runs
/// in under a microsecond will see `cpu_u_ns == 0` and/or `cpu_s_ns == 0`.
/// This is acceptable for staging-level profiling per the spec.
pub fn cpu_times_now_ns() -> CpuTimes {
    // Safety: `getrusage` writes into `usage`; `RUSAGE_THREAD` is a valid
    // `who` argument on Linux 2.6.26+ (kernel ≥ 4.4 is required per §7.4,
    // so this is unconditionally available on the supported target).
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut usage) };
    // Hard-assert rather than debug-assert: a non-zero return (EINVAL on
    // an unknown `who` constant, EFAULT on a bad pointer) is impossible on
    // the supported target with a valid stack pointer, but if it ever
    // happens we want a loud panic, not a zero-filled `rusage` that
    // silently becomes a bogus `CpuTimes` written into a `CallRecord`.
    assert!(
        rc == 0,
        "getrusage(RUSAGE_THREAD) is infallible on Linux x86_64 for the \
         supported `who` constant with a valid pointer; got rc={rc}"
    );

    CpuTimes {
        user_ns: timeval_to_ns(usage.ru_utime),
        system_ns: timeval_to_ns(usage.ru_stime),
    }
}

/// PHP "real" memory usage in bytes (including allocator overhead).
///
/// Production builds (`cfg(not(test))`) call `zend_memory_usage(true)` and
/// cast the `size_t` result to `i64`. Test builds (`cfg(test)`) return `0`
/// unconditionally so the test binary links without a live PHP runtime —
/// the production path is exercised end-to-end by Phase-2 slice 2's PHP
/// fixture.
///
/// The cast to `i64` is safe in practice: a single PHP process consuming
/// more than 2^63 bytes (≈ 9.2 EiB) would have other problems.
///
/// ## Why an `extern "C"` block here
///
/// `ext-php-rs = "=0.15.13"`'s bindgen allowlist does not include
/// `zend_memory_usage`, so we declare it ourselves against the PHP-side
/// signature in `Zend/zend_alloc.h`:
///
/// ```c
/// ZEND_API size_t zend_memory_usage(bool real_usage);
/// ```
///
/// `size_t` maps to `usize`; C99 `_Bool` maps to Rust `bool` on all
/// platforms the project supports (Linux x86_64). The symbol is provided
/// by the PHP runtime at extension-load time; the `cdylib` does not link
/// against `libphp.so`, but the loader resolves the symbol when PHP loads
/// our `.so`.
#[cfg(not(test))]
pub fn memory_usage_real_bytes() -> i64 {
    // `unsafe extern "C"` (rather than the older bare `extern "C"`) is
    // the form Rust 2024 requires; it is accepted on edition 2021 as
    // well, so adopting it now keeps the future edition upgrade silent.
    unsafe extern "C" {
        fn zend_memory_usage(real_usage: bool) -> usize;
    }
    // Safety: `zend_memory_usage` is a leaf Zend function with no
    // preconditions beyond the Zend runtime being initialised — which it
    // is, because we are running inside a PHP-loaded extension callback.
    unsafe { zend_memory_usage(true) as i64 }
}

/// Test-build stub for [`memory_usage_real_bytes`].
///
/// Returns `0` so `cargo test` can run without `libphp.so` on the test host.
/// The signature matches the production version exactly; downstream code
/// sees no difference at compile time.
#[cfg(test)]
pub fn memory_usage_real_bytes() -> i64 {
    0
}

// --- internal helpers ---

/// One-shot `clock_gettime` wrapper shared by [`monotonic_now_ns`] and
/// [`realtime_now_ns`]. Pulling the dispatch out keeps both public
/// functions to a single line.
fn clock_gettime_ns(clock_id: libc::clockid_t) -> i64 {
    // Safety: `ts` is uninitialised but `clock_gettime` writes both fields
    // before returning; passing a valid `clockid_t` cannot fault. We still
    // assert the return code in debug to surface unexpected EINVAL loudly.
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::clock_gettime(clock_id, &mut ts) };
    // Hard-assert rather than debug-assert: a non-zero return (EINVAL on
    // an unknown `clockid_t`, EFAULT on a bad pointer) cannot happen for
    // the two clock IDs this module passes in (`CLOCK_MONOTONIC`,
    // `CLOCK_REALTIME`) on the supported target. If it ever does, we
    // want to panic loudly rather than silently emit a zero-filled
    // timestamp that downstream code reads as 1970-01-01.
    assert!(
        rc == 0,
        "clock_gettime with a built-in clock_id is infallible; got rc={rc}"
    );

    // `tv_sec` is `time_t` (i64 on Linux x86_64); `tv_nsec` is `c_long`
    // (i64 on the same target). The product `tv_sec * 1_000_000_000` for
    // any realistic timestamp fits in i64 with billions of years of
    // headroom.
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

/// Convert a POSIX `timeval` (seconds + microseconds) into nanoseconds.
/// Used by [`cpu_times_now_ns`] for both the user and system fields.
///
/// On Linux x86_64 (the only supported target — `SPECIFICATION.md` §7.4)
/// `tv_sec` (`time_t`) and `tv_usec` (`suseconds_t`) are both `i64`, so no
/// explicit casts are needed. Clippy enforces the absence of `as i64`
/// no-ops via the default `unnecessary_cast` lint.
fn timeval_to_ns(tv: libc::timeval) -> i64 {
    tv.tv_sec * 1_000_000_000 + tv.tv_usec * 1_000
}

/// All four observer-relevant clock/memory values in one batch. Used
/// by [`RawSnapshot::capture_now`] so the recorder hot path passes
/// through a single inlinable entry point per begin/end boundary
/// (recorder-hot-path-tuning D-3). The syscall pattern is unchanged
/// versus calling the individual `*_now_ns` helpers — same one
/// `CLOCK_MONOTONIC` read, same one `getrusage` read, same one
/// `zend_memory_usage` call.
///
/// The struct exists for shape parity with the begin/end snapshot
/// types in `recorder::observer`; the helper assembles values from
/// the same syscall pattern documented on those helpers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawSnapshot {
    pub t_ns: i64,
    pub cpu_u_ns: i64,
    pub cpu_s_ns: i64,
    pub mem_bytes: i64,
}

/// Combined snapshot helper. Equivalent to calling
/// [`monotonic_now_ns`] + [`cpu_times_now_ns`] +
/// [`memory_usage_real_bytes`] in sequence; provided as a single
/// `#[inline]` function so the optimiser can fold the boundary and so
/// the recorder hot path has a single named seam to reason about.
#[inline]
pub(crate) fn snapshot_now() -> RawSnapshot {
    let t_ns = monotonic_now_ns();
    let cpu = cpu_times_now_ns();
    let mem_bytes = memory_usage_real_bytes();
    RawSnapshot {
        t_ns,
        cpu_u_ns: cpu.user_ns,
        cpu_s_ns: cpu.system_ns,
        mem_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    /// UNIX timestamp for 2020-01-01 00:00:00 UTC, in nanoseconds.
    /// Used as a lower bound on [`realtime_now_ns`] to catch wildly wrong
    /// unit-conversion bugs.
    const YEAR_2020_NS: i64 = 1_577_836_800_000_000_000;

    #[test]
    fn monotonic_now_ns_is_non_decreasing_across_a_two_millisecond_sleep() {
        let a = monotonic_now_ns();
        sleep(Duration::from_millis(2));
        let b = monotonic_now_ns();

        assert!(b >= a, "monotonic clock must not decrease: a={a}, b={b}");

        // A 2 ms sleep must elapse at least 1 ms in clock time (the kernel
        // is allowed to round down). A return in nanoseconds will sit
        // above 1 ms; a return in microseconds (×1000 too small) or
        // milliseconds (×1_000_000 too small) will not. There is
        // deliberately **no** upper bound here: a paged-out or `nice`d CI
        // runner can pause a thread for hundreds of milliseconds without
        // that being a unit-conversion bug, and `b >= a` already proves
        // monotonicity. The 1 ms lower bound is the only assertion that
        // earns its place.
        let delta = b - a;
        assert!(
            delta >= 1_000_000,
            "monotonic delta {delta}ns is below 1 ms — likely a \
             unit-conversion bug (µs or ms instead of ns)"
        );
    }

    #[test]
    fn cpu_times_now_ns_returns_non_negative_components_after_a_busy_loop() {
        // Burn a little CPU so `user_ns` has at least a few microseconds
        // to report. We don't assert that the value is non-zero (a
        // sufficiently fast host plus getrusage's µs granularity can
        // legitimately round it to 0); we only assert non-negativity.
        let mut acc: u64 = 0;
        for i in 0..1_000_000_u64 {
            acc = acc.wrapping_add(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        }
        // Defeat dead-code elimination on `acc` so the loop above is not
        // optimised away under `--release`.
        std::hint::black_box(acc);

        let times = cpu_times_now_ns();
        assert!(
            times.user_ns >= 0,
            "user_ns must be non-negative: {}",
            times.user_ns
        );
        assert!(
            times.system_ns >= 0,
            "system_ns must be non-negative: {}",
            times.system_ns
        );
    }

    #[test]
    fn realtime_now_ns_is_after_year_2020() {
        let now = realtime_now_ns();
        assert!(
            now > YEAR_2020_NS,
            "realtime_now_ns returned {now}, which is before 2020 — \
             likely a unit-conversion bug (µs or ms instead of ns)"
        );
        assert!(now < i64::MAX, "realtime_now_ns must fit in i64");
    }

    #[test]
    fn memory_usage_real_bytes_is_callable_from_a_pure_rust_unit_test() {
        // Test build returns 0; the assertion confirms the symbol is
        // reachable without panic.
        let value = memory_usage_real_bytes();
        assert_eq!(value, 0, "test-build stub must return 0");
    }

    #[test]
    fn memory_usage_real_bytes_returns_an_i64() {
        // Compile-time signature contract: the binding must be `i64`.
        // The let-binding annotation is what enforces the contract; the
        // body of this test is intentionally minimal.
        let _value: i64 = memory_usage_real_bytes();
    }

    #[test]
    fn snapshot_now_returns_consistent_components_across_two_reads() {
        // The combined snapshot helper must agree with the individual
        // `*_now_ns` helpers on what they would have read separately
        // — modulo the strict ordering between the wall-clock and CPU
        // reads (which can advance between calls). The bound is the
        // weakest one all four components share: non-negative,
        // monotonically non-decreasing across two reads, and within
        // a sane upper bound (`i64::MAX`).
        let a = snapshot_now();
        std::thread::sleep(Duration::from_micros(500));
        let b = snapshot_now();

        // Wall clock must not regress.
        assert!(b.t_ns >= a.t_ns, "monotonic regression: a={a:?}, b={b:?}");
        // CPU times are monotonically non-decreasing per the kernel's
        // accounting contract.
        assert!(b.cpu_u_ns >= a.cpu_u_ns, "user CPU regressed");
        assert!(b.cpu_s_ns >= a.cpu_s_ns, "system CPU regressed");
        // Memory: the test-build stub returns 0 unconditionally;
        // production reads PHP's real-usage counter. Both paths are
        // non-negative.
        assert!(b.mem_bytes >= 0);
    }
}
