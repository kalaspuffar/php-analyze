//! Allocator-counting audit binding `SPECIFICATION.md` §3.2 AC-RC-5:
//!
//! > Hot path (begin + end handlers) performs **zero heap
//! > allocations** in the steady state (call stack and buffer
//! > pre-grown after warmup). Verified via an allocator-counting
//! > harness.
//!
//! ## Steady-state semantics
//!
//! "Steady state" means: after warmup, with the trace's buffer +
//! stack pre-grown to capacity ≥ N and the function dictionary
//! pre-warmed with one cold-miss call per unique function, the
//! per-call cost MUST NOT include any allocator op. The audit
//! pins this verbatim by counting global allocations across an
//! `N`-call inner loop and asserting the delta is **exactly
//! zero**.
//!
//! ## `#[global_allocator]` scope
//!
//! The `CountingAllocator` declared below replaces Rust's default
//! `System` allocator **for this test binary only**. Cargo
//! compiles each `tests/*.rs` to a separate binary, so the
//! production cdylib, the unit tests inside `src/`, and the
//! other integration tests (`shipper_round_trip.rs`,
//! `spike_observer.rs`, `recorder_observer.rs`) all use Rust's
//! default `System` allocator unchanged.
//!
//! ## Why not `bench_seam` / `bench-seam` feature?
//!
//! The bench-seam re-export module is the documented surface for
//! microbench code. This is a regression *test*, not a
//! microbench, so it imports the production hot-path entry
//! points directly via `php_analyze::recorder::observer::*` and
//! `php_analyze::recorder::types::*`. All the items it needs
//! are already `pub` (promoted from `pub(crate)` in
//! `bench-criterion-skeleton`'s D-1 deviation), so no feature
//! flag is required.

use std::alloc::{GlobalAlloc, Layout, System};
use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use php_analyze::recorder::observer::{
    begin_with_snapshots, end_with_snapshots, Categorised, EntrySnapshots, ExitSnapshots,
};
use php_analyze::recorder::types::{FunctionKey, FunctionKind, Trace, TraceLimits};
use php_analyze::recorder::RequestIdentity;

// --- Counting global allocator ----------------------------------------------

/// Stdlib-only `GlobalAlloc` wrapper that increments three
/// `AtomicUsize` counters on every allocator op and forwards to
/// `std::alloc::System`. Scoped to this test binary only (see the
/// `#[global_allocator]` static below).
struct CountingAllocator;

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static DEALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static REALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded verbatim to System; the contract on
        // the caller is unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded verbatim to System.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded verbatim to System.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        REALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded verbatim to System.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: CountingAllocator = CountingAllocator;

/// Snapshot the three counters in one call. Returns
/// `(alloc, realloc, dealloc)` — same order the failure
/// message renders.
fn snapshot_counters() -> (usize, usize, usize) {
    (
        ALLOC_COUNT.load(Ordering::Relaxed),
        REALLOC_COUNT.load(Ordering::Relaxed),
        DEALLOC_COUNT.load(Ordering::Relaxed),
    )
}

// --- Hot-path setup helpers --------------------------------------------------

/// Construct a `Trace` whose limits are large enough to prevent
/// any flush or cap-drop firing during the audit. Mirrors
/// `crates/php-analyze/benches/recorder_hot_path.rs::make_trace`.
fn make_trace() -> Trace {
    let identity = RequestIdentity {
        host: Arc::from("audit-host"),
        sapi: Arc::from("cli"),
        pid: 0,
        uri_or_script: Arc::from("/test/recorder_zero_alloc.rs"),
    };
    let limits = TraceLimits {
        flush_records: usize::MAX,
        flush_bytes: usize::MAX,
        buffer_cap_bytes: usize::MAX,
        max_depth: 1024,
    };
    Trace::new(identity, limits)
}

/// A fixed `Categorised` value representing a `noop`-shaped user
/// function. Constructed once outside the timed region; the inner
/// loop borrows it.
fn make_categorised() -> Categorised<'static> {
    Categorised {
        key: FunctionKey::Function {
            file: Arc::from("/test/fixture.php"),
            function: Arc::from("noop"),
            line: 1,
        },
        kind: FunctionKind::Function,
        fqn: Cow::Borrowed("noop"),
        file: Cow::Borrowed("/test/fixture.php"),
        line: 1,
    }
}

const ENTRY_SNAPSHOTS: EntrySnapshots = EntrySnapshots {
    t_in_ns: 0,
    cpu_u_in_ns: 0,
    cpu_s_in_ns: 0,
    mem_in_bytes: 0,
};

const EXIT_SNAPSHOTS: ExitSnapshots = ExitSnapshots {
    t_out_ns: 0,
    cpu_u_now_ns: 0,
    cpu_s_now_ns: 0,
    mem_out_bytes: 0,
};

const WARMUP_SIZE: usize = 1_000;
const MEASUREMENT_SIZE: usize = 10_000;
const MAX_DEPTH: usize = 1024;

// --- Positive test: steady-state hot path is zero-alloc ---------------------

#[test]
fn recorder_hot_path_is_zero_alloc_in_steady_state() {
    let mut trace = make_trace();
    let categorised = make_categorised();

    // Pre-warm phase 1 — dictionary cold miss. One call through
    // the cold-miss path interns the FunctionKey and stages a
    // DictEntry with `to_owned()` String fields. This pre-warm
    // call's allocations are *expected* and excluded from the
    // assertion via the snapshot-after pattern.
    begin_with_snapshots(&mut trace, &categorised, ENTRY_SNAPSHOTS);
    end_with_snapshots(&mut trace, EXIT_SNAPSHOTS, false);

    // Pre-grow AFTER the cold-miss pre-warm so
    // `Vec::reserve(WARMUP_SIZE + MEASUREMENT_SIZE)` grows
    // capacity to `len() + WARMUP_SIZE + MEASUREMENT_SIZE` =
    // 1 + 11_000 = 11_001. Plenty of room for both phases below
    // without any realloc.
    trace.pregrow_for_audit(WARMUP_SIZE + MEASUREMENT_SIZE, MAX_DEPTH);

    // Pre-warm phase 2 — `WARMUP_SIZE` hit-path iterations to
    // absorb any *one-shot* lazy initialisation that fires on
    // first hit-path use (e.g., a thread-local cell's `OnceCell`
    // backing storage, a panic-format machinery init, a
    // platform-dependent allocator-side cache warmup). The CI
    // host this test runs on observed `alloc_delta=4,
    // realloc_delta=2, dealloc_delta=9` across a 10_000-call
    // window *without* this phase; the local build host saw
    // `0/0/0`. The small constant counts (not per-call) point at
    // one-shot lazy init, so a 1_000-iter warmup window is the
    // cleanest way to absorb it without weakening the strict
    // `== 0` assertion below. Per `SPECIFICATION.md` AC-RC-5's
    // "in the steady state … after warmup" wording, this is
    // exactly what "warmup" means.
    for _ in 0..WARMUP_SIZE {
        begin_with_snapshots(&mut trace, &categorised, ENTRY_SNAPSHOTS);
        end_with_snapshots(&mut trace, EXIT_SNAPSHOTS, false);
    }

    // Snapshot the counters AFTER both pre-warm phases. The
    // deltas below measure only the steady-state region — every
    // legitimate one-shot init has had a chance to fire.
    let (alloc_before, realloc_before, dealloc_before) = snapshot_counters();

    // Measurement region: MEASUREMENT_SIZE additional calls.
    // After WARMUP_SIZE + MEASUREMENT_SIZE = 11_000 total inner
    // pushes (plus pre-warm phase 1's single push), the buffer
    // is at len = 11_001 with capacity ≥ 11_001 — no realloc.
    for _ in 0..MEASUREMENT_SIZE {
        begin_with_snapshots(&mut trace, &categorised, ENTRY_SNAPSHOTS);
        end_with_snapshots(&mut trace, EXIT_SNAPSHOTS, false);
    }

    let (alloc_after, realloc_after, dealloc_after) = snapshot_counters();
    let alloc_delta = alloc_after - alloc_before;
    let realloc_delta = realloc_after - realloc_before;
    let dealloc_delta = dealloc_after - dealloc_before;

    // AC-RC-5 bound: exactly zero new allocations across the
    // steady-state region (after `WARMUP_SIZE` hit-path warmup
    // iterations). `dealloc_delta` is included in the panic
    // message for diagnostic completeness but not asserted —
    // the recorder holds the `Trace` alive across the whole
    // region, so nothing inside it should drop.
    assert!(
        alloc_delta == 0 && realloc_delta == 0,
        "AC-RC-5 violated: steady-state hot path allocated AFTER warmup. \
         alloc_delta={alloc_delta}, realloc_delta={realloc_delta}, \
         dealloc_delta={dealloc_delta} across {MEASUREMENT_SIZE} measurement \
         calls (after {WARMUP_SIZE} warmup-phase-2 calls). \
         Expected: 0 allocs, 0 reallocs. Likely culprits: hidden \
         allocation in snapshot capture, FunctionKey Arc<str> clone \
         on the hit path, HashMap rehash during what should be a hit, \
         or a new `Vec::new()` / `Box::new()` on the per-call path. \
         See COMMENTS.md C-18-adjacent: counters are precise per-op \
         counts, so the alloc count names the operation count.",
    );
}

// --- Negative-control test: cold miss DOES allocate -------------------------

#[test]
fn recorder_hot_path_first_miss_does_allocate() {
    let mut trace = make_trace();
    // Same pre-grow as the positive test. Note: pre-grow itself
    // allocates (the `reserve` calls fire), but those happen
    // *before* the counter snapshot below.
    trace.pregrow_for_audit(WARMUP_SIZE + MEASUREMENT_SIZE, MAX_DEPTH);

    let categorised = make_categorised();

    // Skip the pre-warm — the dictionary is empty. Snapshot
    // counters now, then run exactly ONE call. The single call
    // is a dict-miss and legitimately allocates (it interns the
    // FunctionKey + stages a DictEntry).
    let (alloc_before, _, _) = snapshot_counters();

    begin_with_snapshots(&mut trace, &categorised, ENTRY_SNAPSHOTS);
    end_with_snapshots(&mut trace, EXIT_SNAPSHOTS, false);

    let (alloc_after, _, _) = snapshot_counters();
    let alloc_delta = alloc_after - alloc_before;

    // The negative control: this test exists to confirm the
    // counter wiring is sensitive. If both this test and the
    // positive test pass, the counter sees the cold-miss
    // allocations (so it's working) AND the steady-state path
    // doesn't trigger any (so AC-RC-5 holds). If this test
    // fails (alloc_delta == 0), the counter is broken and the
    // positive test's pass is meaningless.
    assert!(
        alloc_delta >= 1,
        "negative-control failed: a dict-miss `(begin, end)` pair \
         should allocate (intern the FunctionKey, stage a DictEntry \
         with `to_owned()` String fields), but alloc_delta={alloc_delta}. \
         The counter wiring is broken — the positive test's pass \
         would be meaningless. Investigate before trusting the \
         AC-RC-5 binding.",
    );
}
