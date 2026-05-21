//! Process-wide memory budget for the recorder hot path.
//!
//! `SPECIFICATION.md` §3.2 requires that `buffer_cap_bytes` is enforced
//! against an "atomic snapshot" of the process-wide pending bytes — not
//! just per-trace bytes — because Phase 4 will put pending batches in
//! the shipper channel where they continue to count against the budget
//! after the recorder has handed them off.
//!
//! §2.4 specifies `AtomicUsize` with `Relaxed` ordering: monotonicity
//! of the bound is sufficient, no happens-before edges are needed.
//! Two concurrent FPM workers may both load a stale value and both
//! `fetch_add` past the cap, but the overshoot is bounded by one
//! worst-case-per-call contribution per worker, which is acceptable.
//!
//! Slice 3 (`recorder-depth-and-cap-drops`) introduces this atomic
//! and the only `add` / `sub` call sites:
//! - `recorder::observer::begin_with_snapshots` calls
//!   [`snapshot`] for the cap-check and [`add`] on the accept path
//!   (dict-miss bytes, billed at begin).
//! - `recorder::types::Trace::push_record` calls [`add`] for the
//!   per-record `CALL_RECORD_FIXED_BYTES` contribution.
//! - `recorder::observer::rshutdown_release_trace` calls [`sub`] with
//!   `trace.buffer_estimated_bytes` to return the trace's contribution
//!   to the budget before the trace is dropped.
//!
//! Phase 4 will add a second `sub` site inside the shipper: after a
//! `PendingBatch` is consumed (encoded + posted or dropped on retry
//! exhaustion), the shipper subtracts the batch's `size_estimate`.
//! No other code path is expected to touch this atomic.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Process-wide pending bytes counter.
///
/// Visibility is `pub(crate)` because the atomic itself is not part
/// of the crate's public API — callers go through [`add`], [`sub`],
/// and [`snapshot`] so the ordering choice stays centralised and so
/// the documented invariant ("`Relaxed` everywhere except the test
/// reset") survives future edits.
pub(crate) static BYTES_IN_MEMORY: AtomicUsize = AtomicUsize::new(0);

/// Add `bytes` to the budget atomically.
///
/// `Relaxed` ordering per §2.4. The caller is expected to have
/// already passed the cap-check; this function does not enforce the
/// cap itself.
pub(crate) fn add(bytes: usize) {
    BYTES_IN_MEMORY.fetch_add(bytes, Ordering::Relaxed);
}

/// Subtract `bytes` from the budget atomically.
///
/// `Relaxed` ordering per §2.4. The caller is responsible for
/// ensuring `bytes` does not exceed the current value; underflow
/// would wrap and corrupt the budget. Slice 3's only callers feed
/// `trace.buffer_estimated_bytes`, which is bounded by what the
/// recorder previously `add`-ed.
pub(crate) fn sub(bytes: usize) {
    BYTES_IN_MEMORY.fetch_sub(bytes, Ordering::Relaxed);
}

/// Read the current budget snapshot.
///
/// `Relaxed` ordering: a stale read is acceptable because the only
/// consumer (the cap-check) is comparing against a soft target.
pub(crate) fn snapshot() -> usize {
    BYTES_IN_MEMORY.load(Ordering::Relaxed)
}

/// Test-only reset. `SeqCst` ordering so a `reset_for_test()` at the
/// top of a test is guaranteed to be visible to every subsequent
/// load on this thread, regardless of what other threads may have
/// staged.
///
/// Callers are tests that exercise the cap with deterministic
/// numbers; they reset the budget as their first line. Parallel
/// tests that all reset are racy by construction — slice-3 tests
/// either run single-threaded under `#[test]` or are gated behind a
/// serial-test attribute (none today; `serial_test` is not a
/// dependency).
#[cfg(test)]
pub(crate) fn reset_for_test() {
    BYTES_IN_MEMORY.store(0, Ordering::SeqCst);
}

/// Re-export the test serialisation guard for slice-3 sibling tests
/// (observer, types) that also touch [`BYTES_IN_MEMORY`].
///
/// Exposed only inside `#[cfg(test)]`; production builds do not link
/// any of this.
#[cfg(test)]
pub(crate) fn acquire_test_lock() -> std::sync::MutexGuard<'static, ()> {
    tests::lock()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialises accounting tests so a parallel `cargo test` runner
    /// can't read another test's add/sub mid-flight. The slice-3
    /// design.md "test pollution" row names this guard explicitly.
    /// Slice-3's observer tests that also touch the atomic use the
    /// same shape locally (`super::tests_lock()` is not exposed; each
    /// test module owns its own guard so the link is local and the
    /// guard cannot accidentally cover unrelated tests).
    pub(super) static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Convenience accessor: acquire the lock, poisoned-or-not.
    /// Tests treat poisoning as benign — a panicking test leaves the
    /// atomic in an undefined state, and the next test resets it.
    pub(super) fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn snapshot_returns_zero_after_reset_for_test() {
        let _guard = lock();
        reset_for_test();
        assert_eq!(snapshot(), 0);
    }

    #[test]
    fn add_and_sub_are_inverses_under_relaxed_ordering() {
        let _guard = lock();
        reset_for_test();
        add(2048);
        assert_eq!(snapshot(), 2048);
        sub(2048);
        assert_eq!(snapshot(), 0);
    }

    #[test]
    fn add_then_snapshot_returns_the_sum() {
        let _guard = lock();
        reset_for_test();
        add(100);
        add(200);
        add(300);
        // `Relaxed` provides no ordering between the three adds, but
        // monotonicity-of-bound only requires the sum to equal the
        // total added on this thread before the load — which is what
        // we observe.
        assert_eq!(snapshot(), 600);
    }
}
