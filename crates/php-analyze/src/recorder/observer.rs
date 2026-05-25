//! Production observer wiring: the `Recorder` that drives the slice-1
//! substrate from real PHP `FcallObserver` events, plus the `BootObserver`
//! dispatcher that picks (Disabled | Recorder) once at `MINIT`.
//!
//! The per-request `Trace` lives in a `thread_local!` slot owned by this
//! module. `bootstrap::rinit` populates it via [`rinit_allocate_trace`];
//! `bootstrap::rshutdown` clears it via [`rshutdown_release_trace`]. The
//! recorder's begin/end handlers reach the trace through
//! [`with_current_trace`], a single accessor that maps a closure over
//! the borrow.
//!
//! ## Why a thread-local
//!
//! The PHP request thread is the only thread the observer fires on
//! (slice 2's scope — no shipper thread until Phase 4, no slice-3
//! atomics). A thread-local cleanly models "this state belongs to
//! whichever thread the request is running on" and removes any need
//! for a `Mutex`. The `RefCell` borrow is always uncontended because
//! observer callbacks are not re-entrant on a single thread (Zend
//! never invokes `begin` on the same thread while the same observer's
//! `end` is in flight). See design.md §D-2.
//!
//! ## `should_observe` filter semantics
//!
//! PHP caches `should_observe`'s result per unique function on first
//! sight. A *transient* `false` (e.g. a MINIT-time PHP-internal call
//! firing before `RINIT` populates the slot) would be cached
//! permanently and silently drop that function from every later
//! request — so any per-request state filter MUST live in `begin` /
//! `end` (which are not cached), never in `should_observe`.
//!
//! A *static* filter is safe and beneficial: when the answer is a
//! deterministic function of the function's name (and class scope),
//! the cached value stays correct across every subsequent request,
//! and skipped functions cost **zero per call** for the lifetime of
//! the process. The `Recorder::should_observe` impl uses this for
//! `Config::skip_functions` and `Config::skip_internal` — see
//! `REVIEW.md` finding P-0 and the `skip-functions-directive`
//! change.
//!
//! ## The `FcallObserver::end` API and exception detection
//!
//! `ext_php_rs = 0.15.13` exposes `fn end(&self, execute_data: &ExecuteData, retval: Option<&Zval>)`
//! — there is no `abnormal: bool` parameter. The recorder reads
//! `ExecutorGlobals::has_exception()` inline. See design.md §D-7 and
//! the C-8 entry in `COMMENTS.md`.

use std::cell::RefCell;

use ext_php_rs::ffi;
use ext_php_rs::types::Zval;
use ext_php_rs::zend::{ExecuteData, ExecutorGlobals, FcallInfo, FcallObserver};

use crate::clocks;
use crate::config::Config;
use crate::recorder::accounting;
use crate::recorder::flush;
use crate::recorder::types::{
    CallFrame, CallRecord, DictEntry, FunctionKey, FunctionKeyRef, FunctionKind, PendingBatch,
    RequestIdentity, Trace, TraceLimits, CALL_RECORD_FIXED_BYTES, DICT_ENTRY_FIXED_BYTES,
};

// --- Thread-local trace slot ----------------------------------------------

thread_local! {
    /// The per-request `Trace`, populated at `RINIT` and dropped at
    /// `RSHUTDOWN`. `None` outside a request window.
    static CURRENT_TRACE: RefCell<Option<Trace>> = const { RefCell::new(None) };
}

/// Populate the thread-local with a fresh `Trace`. Called from
/// `bootstrap::rinit` when the extension is enabled.
///
/// **Posture (RO-1).** A previous request that ran `RINIT` without a
/// matching `RSHUTDOWN` is a bug we want to know about — but
/// panicking here would propagate across the `extern "C"` FFI
/// boundary and abort the PHP process, violating
/// `SPECIFICATION.md` §8.3 NFR-REL-1 ("never crash the PHP process")
/// and AD-4 (silent-disable posture). The compromise is a
/// `debug_assert!` (so tests and debug builds still catch the
/// pairing bug loudly) plus an explicit release-mode recovery that
/// drops the stale `Trace` before installing the fresh one. The
/// stale buffer is lost — slice 3's `dropped_records` counter will
/// be the operator-visible signal once that lands.
pub fn rinit_allocate_trace(identity: RequestIdentity, limits: TraceLimits) {
    CURRENT_TRACE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        debug_assert!(
            borrow.is_none(),
            "RINIT without RSHUTDOWN: the recorder thread-local already holds a Trace; \
             a previous request did not call rshutdown_release_trace",
        );
        // Release-path recovery: drop the stale Trace silently and
        // replace it with a fresh one. The assignment itself drops
        // the previous Option contents via `Drop` — and since
        // slice 3's `rshutdown_release_trace` is the only sanctioned
        // subtract from `accounting::BYTES_IN_MEMORY`, a release-path
        // overwrite here would leak the stale trace's contribution to
        // the budget. We `take()` and process the stale trace through
        // the same drain path before installing the fresh one so the
        // atomic stays accurate.
        if let Some(stale) = borrow.take() {
            crate::recorder::accounting::sub(stale.buffer_estimated_bytes);
            drop(stale);
        }
        *borrow = Some(Trace::new(identity, limits));
    });
}

/// Drop the thread-local `Trace`. Called from `bootstrap::rshutdown`
/// unconditionally — a no-op when the slot is already `None`
/// (extension disabled or `RINIT` was skipped).
///
/// ## Phase-4 slice 2: `RSHUTDOWN` final flush
///
/// Per `SPECIFICATION.md` §3.2 / REQ F-BF-3, a non-empty buffer at
/// request end SHALL be handed to the shipper as a final batch
/// before the trace is dropped. An **empty** buffer is NOT flushed
/// — the spec is explicit, and an empty `Batch(_)` would force the
/// future encoder to either produce an empty `calls` array or
/// special-case the absence.
///
/// Order of operations:
///
/// 1. Take the trace out of the thread-local slot.
/// 2. If the trace has any buffered records, build a `PendingBatch`
///    via `Trace::flush_into_pending_batch` (which already resets
///    `buffer_estimated_bytes = 0` and clears the two `Vec`s) and
///    hand it to `recorder::flush::try_send_batch`. The
///    `recorder-dump` `F: trigger=rshutdown` line is emitted in
///    this branch so a fixture can assert the cadence.
/// 3. Subtract whatever `buffer_estimated_bytes` remains on the
///    post-flush trace. In the flushed case this is zero (a `sub(0)`
///    no-op); in the empty-buffer case it is also zero in the
///    common path (`push_record` and `push_dict_entry_via_intern`
///    bill the atomic synchronously with the field). The subtract
///    is kept for symmetry with slice 3's `DCR-1` ordering rule
///    ("subtract before sink so the budget stays exact even if a
///    downstream hand-off panics") — the `try_send_batch` call is
///    above it, but `try_send_batch` itself is panic-safe by
///    construction (the `Mutex` poison guards in `clone_canonical_sender`
///    swallow panics), so the ordering is purely a "what if a
///    future addition panics here" defence.
/// 4. Run the `recorder-dump` whole-trace dumper (gated behind the
///    feature flag). This is the same site slice 3 used; only the
///    `F:` line shape grew.
///
/// **Channel-full at RSHUTDOWN**: the final batch goes through
/// `try_send_batch` like every other batch. If the channel is full,
/// the drop counter bumps by `batch.calls.len()` and the bytes are
/// subtracted — exactly the same R-13 distinction as a mid-request
/// flush. The trace's `drop_counter` is the `Arc` clone on the
/// batch, so the bump is visible through the trace's `Arc` until
/// the trace itself drops at the end of this function.
pub fn rshutdown_release_trace() {
    CURRENT_TRACE.with(|slot| {
        let trace = slot.borrow_mut().take();
        let Some(mut trace) = trace else {
            return;
        };

        // Drain any still-open CallFrames into abnormal-exit
        // CallRecords before the final flush. PHP's script body is
        // observed as a closure that begins at script start; its
        // matching `end`-fcall callback never fires before MSHUTDOWN,
        // so without this drain the root frame's CallRecord never
        // reaches the wire and downstream collectors that enforce
        // referential integrity on `parent` would file every other
        // call as an orphan. See
        // `openspec/specs/recorder-call-events/spec.md`
        // ("`rshutdown_release_trace` SHALL drain still-open
        // CallFrames into abnormal-exit CallRecords before the final
        // flush") for the contract.
        drain_open_frames_into_buffer(&mut trace);

        // Phase-4 slice 2: hand the non-empty buffer to the shipper
        // before the trace is dropped.
        if !trace.buffer.is_empty() {
            let batch = trace.flush_into_pending_batch();
            emit_flush_dump_line(&trace, &batch, FlushTrigger::Rshutdown);
            flush::try_send_batch(batch);
        }

        // Slice-3 invariant: subtract this trace's residual
        // contribution. In the flushed case, `flush_into_pending_batch`
        // already reset `buffer_estimated_bytes` to zero, so this is a
        // `sub(0)` no-op. In the empty-buffer case, the field was zero
        // anyway. The unconditional call keeps the slice-3 wording
        // intact and survives a future change that puts non-billed
        // bytes onto the running estimate.
        accounting::sub(trace.buffer_estimated_bytes);

        #[cfg(feature = "recorder-dump")]
        crate::recorder::dump::write_trace_if_path_set(&trace);
        drop(trace);
    });
}

/// Drain every still-open `CallFrame` on `trace.stack` into the
/// trace's buffer as abnormal-exit `CallRecord`s.
///
/// Ordering invariant — **top-first**: we pop from the top of the
/// stack and emit immediately. The drained record sequence is
/// therefore `(depth=N, depth=N-1, …, depth=0)`, matching the
/// recorder's existing end-of-call emission order (a child's record
/// always precedes its parent's). The §4.2.3 wire-format Requirement
/// on `parent` referential integrity assumes this ordering within a
/// single batch.
///
/// Snapshot invariant — **shared exit triple**: we capture one
/// `ExitSnapshots` before the loop and reuse it for every drained
/// record. The drain runs in a tight loop with no PHP code executing
/// between frames, so per-frame snapshots would only measure the
/// drain loop's own cost. The shared-snapshot approach preserves the
/// "frames ended at MSHUTDOWN" semantic and keeps the drain
/// allocation- and syscall-free per record.
///
/// Accounting invariant — every drained record flows through
/// `Trace::push_record`, so `buffer_estimated_bytes` and the
/// process-wide `BYTES_IN_MEMORY` atomic track the drained records
/// exactly as they would for naturally-ended records. The subsequent
/// `accounting::sub(trace.buffer_estimated_bytes)` in
/// `rshutdown_release_trace` therefore restores the atomic to its
/// pre-trace value after the resulting batch is flushed.
///
/// This deliberately does **not** consult `flush_records` /
/// `flush_bytes`: the spec requires a single-batch emission for the
/// drain, so we never flush mid-loop. The outer
/// `rshutdown_release_trace` runs exactly one flush after the drain
/// returns.
fn drain_open_frames_into_buffer(trace: &mut Trace) {
    if trace.stack.is_empty() {
        return;
    }
    let exit = ExitSnapshots::capture_now();
    while let Some(frame) = trace.stack.pop() {
        let record = frame.into_abnormal_call_record(
            exit.t_out_ns,
            exit.cpu_u_now_ns,
            exit.cpu_s_now_ns,
            exit.mem_out_bytes,
        );
        trace.push_record(record);
    }
}

/// Borrow the current `Trace` mutably for the duration of `f`.
///
/// Returns `None` when the slot is empty (extension disabled or
/// out-of-request observer fire). Returns `Some(f(trace))` otherwise.
///
/// The borrow is scoped to `f`'s body. Callers MUST NOT recursively
/// invoke any function that itself calls `with_current_trace` —
/// `RefCell::borrow_mut` would panic on the inner borrow. The slice-2
/// hot path never re-enters; if a future slice introduces re-entry,
/// the panic message is the bug signal.
pub(crate) fn with_current_trace<R>(f: impl FnOnce(&mut Trace) -> R) -> Option<R> {
    CURRENT_TRACE.with(|slot| slot.borrow_mut().as_mut().map(f))
}

// --- Function-call snapshots (testability adapter) -------------------------

/// `#[cfg(test)]` counter incremented once per
/// `EntrySnapshots::capture_now` and `ExitSnapshots::capture_now`
/// invocation. Lets unit tests assert "this drop path did not pay
/// for any syscall" by reading the counter delta around a drop
/// scenario (REVIEW.md §4.5 #1, gate-before-snapshot D-5).
///
/// The static and the increment site are both behind `#[cfg(test)]`,
/// so production builds carry zero overhead. Tests serialise
/// observations via the same lock pattern used by other test seams
/// in this module — call [`reset_snapshot_capture_count_for_test`]
/// at test entry to start from zero.
#[cfg(test)]
pub(crate) static SNAPSHOT_CAPTURE_COUNT_FOR_TEST: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Test-only read accessor for [`SNAPSHOT_CAPTURE_COUNT_FOR_TEST`].
#[cfg(test)]
pub(crate) fn snapshot_capture_count_for_test() -> usize {
    SNAPSHOT_CAPTURE_COUNT_FOR_TEST.load(std::sync::atomic::Ordering::Relaxed)
}

/// Test-only reset for [`SNAPSHOT_CAPTURE_COUNT_FOR_TEST`]. Call at
/// the start of a test that wants to assert a specific counter delta.
#[cfg(test)]
pub(crate) fn reset_snapshot_capture_count_for_test() {
    SNAPSHOT_CAPTURE_COUNT_FOR_TEST.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// The four clock/memory values captured at call entry by the begin
/// handler. Passed through to `begin_with_snapshots` so unit tests can
/// inject deterministic values without invoking the real syscalls.
/// `pub` (not `pub(crate)`) so `lib.rs::bench_seam` can re-export
/// it for the criterion benches under `crates/php-analyze/benches/`.
/// External Rust consumers have no reason to construct or consume
/// this — it exists as a `pub` item only to satisfy Rust's
/// visibility rules for the bench-seam `pub use`. See
/// `bench-criterion-skeleton`'s `design.md` D-1 (revised).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntrySnapshots {
    pub t_in_ns: i64,
    pub cpu_u_in_ns: i64,
    pub cpu_s_in_ns: i64,
    pub mem_in_bytes: i64,
}

impl EntrySnapshots {
    /// Take snapshots from the production clock primitives. Routes
    /// through [`clocks::snapshot_now`] so the recorder hot path has
    /// one inlinable boundary per begin/end (recorder-hot-path-tuning
    /// D-3). The CPU read is conditional on the
    /// `cpu_snapshot_mode` directive (recorder-cpu-snapshot-cadence):
    /// `PerCall` (default) invokes `getrusage(RUSAGE_THREAD)`; `Off`
    /// skips the syscall and returns zero CPU fields.
    #[inline]
    fn capture_now() -> Self {
        #[cfg(test)]
        SNAPSHOT_CAPTURE_COUNT_FOR_TEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let raw = clocks::snapshot_now(current_cpu_snapshot_mode());
        Self {
            t_in_ns: raw.t_ns,
            cpu_u_in_ns: raw.cpu_u_ns,
            cpu_s_in_ns: raw.cpu_s_ns,
            mem_in_bytes: raw.mem_bytes,
        }
    }
}

/// The four clock/memory values captured at call exit by the end
/// handler. Same role as [`EntrySnapshots`] for `end_with_snapshots`.
/// `pub` for the same bench-seam reason as [`EntrySnapshots`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitSnapshots {
    pub t_out_ns: i64,
    pub cpu_u_now_ns: i64,
    pub cpu_s_now_ns: i64,
    pub mem_out_bytes: i64,
}

impl ExitSnapshots {
    /// Take snapshots from the production clock primitives. Mirror of
    /// [`EntrySnapshots::capture_now`]; same conditional CPU read.
    #[inline]
    fn capture_now() -> Self {
        #[cfg(test)]
        SNAPSHOT_CAPTURE_COUNT_FOR_TEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let raw = clocks::snapshot_now(current_cpu_snapshot_mode());
        Self {
            t_out_ns: raw.t_ns,
            cpu_u_now_ns: raw.cpu_u_ns,
            cpu_s_now_ns: raw.cpu_s_ns,
            mem_out_bytes: raw.mem_bytes,
        }
    }
}

/// Resolve the active `cpu_snapshot_mode` for the current process.
///
/// Reads `Config::global().cpu_snapshot_mode` (the frozen-at-MINIT
/// value) and returns its `Copy` mode. Before `MINIT` runs — or in
/// the `cargo test` build where production paths are exercised in
/// isolation — the global is uninitialised; this helper falls back
/// to [`CpuSnapshotMode::PerCall`] so unit tests preserve the
/// spec-current behaviour without depending on global state.
///
/// The branch on `Config::global()`'s `Option` is single-load against
/// a `OnceLock`; the steady-state cost (post-MINIT) is one memory
/// load. The mode itself is `Copy`, so the return is a value-copy.
///
/// **Test seam**: under `cfg(test)`, [`set_cpu_snapshot_mode_for_test`]
/// can publish a mode that this helper observes before consulting
/// `Config::global()`. The override is process-wide; tests that use
/// it MUST [`clear_cpu_snapshot_mode_for_test`] on teardown.
#[inline]
fn current_cpu_snapshot_mode() -> crate::config::CpuSnapshotMode {
    #[cfg(test)]
    if let Some(mode) = cpu_snapshot_mode_test_override() {
        return mode;
    }
    crate::config::Config::global()
        .map(|c| c.cpu_snapshot_mode)
        .unwrap_or(crate::config::CpuSnapshotMode::PerCall)
}

/// Test-only override slot for [`current_cpu_snapshot_mode`]. Encoded
/// as an `AtomicU8` because `AtomicEnum` is not in stdlib:
/// - `0xff` = no override (default; helper consults `Config::global`).
/// - `0` = `CpuSnapshotMode::PerCall`.
/// - `1` = `CpuSnapshotMode::Off`.
///
/// The encoding mirrors the natural source order of the variants;
/// tests reset the slot to `0xff` on teardown so cross-test
/// contamination is impossible.
#[cfg(test)]
static CPU_SNAPSHOT_MODE_TEST_OVERRIDE: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0xff);

#[cfg(test)]
const CPU_SNAPSHOT_MODE_TEST_OVERRIDE_UNSET: u8 = 0xff;

/// Serialises every test that touches the
/// [`CPU_SNAPSHOT_MODE_TEST_OVERRIDE`] slot. Cargo test runs tests
/// in parallel within a binary; without this lock, a test setting
/// `Off` and another setting `PerCall` race against
/// [`current_cpu_snapshot_mode`]'s read, producing flaky failures.
/// CI observed `cpu_u_in_ns = 73000` in an `Off`-expected test
/// because a parallel `PerCall` test cleared the override
/// mid-call. The lock is held for the lifetime of any
/// [`CpuSnapshotModeTestGuard`] or [`lock_cpu_snapshot_mode_for_test`]
/// returned `MutexGuard`.
#[cfg(test)]
static CPU_SNAPSHOT_MODE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn cpu_snapshot_mode_test_override() -> Option<crate::config::CpuSnapshotMode> {
    use std::sync::atomic::Ordering;
    match CPU_SNAPSHOT_MODE_TEST_OVERRIDE.load(Ordering::Relaxed) {
        CPU_SNAPSHOT_MODE_TEST_OVERRIDE_UNSET => None,
        0 => Some(crate::config::CpuSnapshotMode::PerCall),
        1 => Some(crate::config::CpuSnapshotMode::Off),
        // Any other value is a test-side bug — the encoding above is
        // total.
        other => panic!("CPU_SNAPSHOT_MODE_TEST_OVERRIDE has bogus value {other}"),
    }
}

/// Publish a test override for [`current_cpu_snapshot_mode`]. Must be
/// paired with [`clear_cpu_snapshot_mode_for_test`] in the test's
/// teardown so the slot does not leak into other tests' assertions.
/// Callers MUST already hold the [`CPU_SNAPSHOT_MODE_TEST_LOCK`];
/// the [`CpuSnapshotModeTestGuard`] RAII helper enforces this.
#[cfg(test)]
fn set_cpu_snapshot_mode_for_test(mode: crate::config::CpuSnapshotMode) {
    use std::sync::atomic::Ordering;
    let encoded = match mode {
        crate::config::CpuSnapshotMode::PerCall => 0,
        crate::config::CpuSnapshotMode::Off => 1,
    };
    CPU_SNAPSHOT_MODE_TEST_OVERRIDE.store(encoded, Ordering::Relaxed);
}

/// Clear the test override; subsequent reads of
/// [`current_cpu_snapshot_mode`] fall back to the
/// `Config::global()`-or-default path. Idempotent. Callers MUST
/// hold [`CPU_SNAPSHOT_MODE_TEST_LOCK`].
#[cfg(test)]
fn clear_cpu_snapshot_mode_for_test() {
    use std::sync::atomic::Ordering;
    CPU_SNAPSHOT_MODE_TEST_OVERRIDE.store(CPU_SNAPSHOT_MODE_TEST_OVERRIDE_UNSET, Ordering::Relaxed);
}

/// Acquire [`CPU_SNAPSHOT_MODE_TEST_LOCK`] without publishing an
/// override. Used by tests that need to observe the no-override
/// state (i.e., that [`current_cpu_snapshot_mode`] falls back to
/// `Config::global()` or the default) while still serialising
/// against tests that DO publish an override.
#[cfg(test)]
fn lock_cpu_snapshot_mode_for_test() -> std::sync::MutexGuard<'static, ()> {
    CPU_SNAPSHOT_MODE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// RAII guard that publishes a mode override on construction and
/// clears it on drop. **Holds [`CPU_SNAPSHOT_MODE_TEST_LOCK`] for
/// its entire lifetime** so two tests using the override cannot
/// interleave their `set` and `read` operations — the lock is the
/// only thing that makes the override safe to use under cargo
/// test's default parallel execution.
///
/// The held `MutexGuard<'static, ()>` is released by `Drop`, which
/// also clears the override slot. Test panics still unwind through
/// `Drop`, so the slot and the lock both end the test in a clean
/// state even on failure.
#[cfg(test)]
struct CpuSnapshotModeTestGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl CpuSnapshotModeTestGuard {
    fn new(mode: crate::config::CpuSnapshotMode) -> Self {
        // `unwrap_or_else(|e| e.into_inner())` recovers from a
        // poisoned lock — a previous test panicked while holding it.
        // The override slot might still hold the stale value; the
        // `set_cpu_snapshot_mode_for_test` call below overwrites it
        // unconditionally so the slot is consistent before any read.
        let lock = lock_cpu_snapshot_mode_for_test();
        set_cpu_snapshot_mode_for_test(mode);
        Self { _lock: lock }
    }
}

#[cfg(test)]
impl Drop for CpuSnapshotModeTestGuard {
    fn drop(&mut self) {
        clear_cpu_snapshot_mode_for_test();
        // `_lock` drops here, releasing the mutex for the next test.
    }
}

// --- Call-site extraction --------------------------------------------------

/// A call-site as extracted from `&ExecuteData`, with `Cow<'a, str>` for
/// every string field so non-UTF-8 payloads (file paths in particular,
/// see RO-4) round-trip as lossy `String`s rather than vanishing.
///
/// This is the recorder-owned analogue of `ext_php_rs::zend::FcallInfo<'a>`.
/// We can't use `FcallInfo<'a>` directly because its string fields are
/// `Option<&'a str>` — there's nowhere to put a lossy-decoded `String`
/// with the right lifetime. The `Cow` form sidesteps the problem at
/// the source: a UTF-8 payload stays a zero-copy borrow; a malformed
/// payload becomes an owned `String` with U+FFFD substituted for the
/// invalid bytes. The recorder's hot path is unchanged in the common
/// (UTF-8) case.
/// `pub` for the same bench-seam reason as [`EntrySnapshots`].
#[derive(Clone, Debug)]
pub struct RawCallSite<'a> {
    pub function_name: Option<std::borrow::Cow<'a, str>>,
    pub class_name: Option<std::borrow::Cow<'a, str>>,
    pub filename: Option<std::borrow::Cow<'a, str>>,
    pub lineno: u32,
    pub is_internal: bool,
    /// Raw `execute_data` pointer captured as `usize`. Used **only**
    /// as a call-site tiebreaker in the unknown-function fallback
    /// (RO-5) so two distinct unnamed call sites do not collapse to
    /// one dictionary entry. The pointer is never dereferenced from
    /// here, so storing it as `usize` is sound regardless of the
    /// pointer's provenance lifetime.
    pub execute_data_addr: usize,
}

impl RawCallSite<'static> {
    /// A `RawCallSite` with no inner borrows. Used for the null-func
    /// defensive branch in [`extract_call_site`] (and reachable from
    /// tests).
    fn empty() -> Self {
        Self {
            function_name: None,
            class_name: None,
            filename: None,
            lineno: 0,
            is_internal: false,
            execute_data_addr: 0,
        }
    }
}

/// Parse `&ExecuteData` into a [`RawCallSite<'a>`].
///
/// `ext_php_rs::zend::FcallInfo::from_execute_data` is `pub(crate)`
/// upstream, so we cannot call it. When `ext-php-rs` promotes the
/// constructor, the right move is to drop this function and adapt the
/// upstream value into `RawCallSite`.
///
/// # Safety
///
/// `execute_data` must be a valid `&ExecuteData` such that
/// `(*execute_data).func` is either null or points at a live
/// `zend_function`, and any `zend_string` pointers reached through
/// `func.common.{function_name,scope->name}` and `func.op_array.filename`
/// are either null or valid for the duration of the call. All of those
/// invariants are upheld by the Zend observer machinery for the
/// duration of a `begin`/`end` callback.
unsafe fn extract_call_site<'a>(execute_data: &'a ExecuteData) -> RawCallSite<'a> {
    let execute_data_addr = std::ptr::from_ref(execute_data) as usize;
    let func_ptr = execute_data.func;
    if func_ptr.is_null() {
        let mut empty = RawCallSite::empty();
        empty.execute_data_addr = execute_data_addr;
        return empty;
    }

    // SAFETY: `func_ptr` non-null per the check above; pointed-to
    // `zend_function` is alive for the callback's duration.
    let func = unsafe { &*func_ptr };
    let common = unsafe { &func.common };
    #[allow(clippy::cast_possible_truncation)]
    let is_internal = common.type_ == ffi::ZEND_INTERNAL_FUNCTION as u8;

    let function_name = unsafe { zend_string_to_cow(common.function_name) };

    let class_name = if common.scope.is_null() {
        None
    } else {
        // SAFETY: `scope` is null-checked; `name` may itself be null
        // (handled by `zend_string_to_cow`).
        let ce = unsafe { &*common.scope };
        unsafe { zend_string_to_cow(ce.name) }
    };

    let (filename, lineno) = if is_internal {
        (None, 0)
    } else {
        // SAFETY: for user functions the `op_array` arm of the union
        // is the active member; reading `func.op_array.filename` and
        // `.line_start` is well-defined.
        let op_array = unsafe { &func.op_array };
        let filename = unsafe { zend_string_to_cow(op_array.filename) };
        (filename, op_array.line_start)
    };

    RawCallSite {
        function_name,
        class_name,
        filename,
        lineno,
        is_internal,
        execute_data_addr,
    }
}

/// Convert a `*mut zend_string` into a borrowed UTF-8 view, lossily
/// decoding non-UTF-8 bytes via [`String::from_utf8_lossy`].
///
/// The common case — function/method names, which are parser-validated
/// PHP identifiers — returns `Cow::Borrowed(&'a str)` with zero
/// allocation. The rare non-UTF-8 case — most often a file path on a
/// filesystem with non-UTF-8 names — returns `Cow::Owned(String)`
/// with U+FFFD substituted for each invalid byte (RO-4). A previous
/// version of this helper silently dropped non-UTF-8 names; that
/// caused (a) distinct files to collapse to the same empty-file
/// `FunctionKey` and (b) the closure-vs-function precedence rule to
/// misroute, both of which the wire format would have been unable to
/// see.
///
/// # Safety
///
/// `zs` must be either null or a pointer to a `zend_string` whose
/// payload bytes remain alive for the chosen `'a`. The Zend observer
/// surface upholds that invariant for the duration of the
/// `begin`/`end` callback.
unsafe fn zend_string_to_cow<'a>(zs: *mut ffi::zend_string) -> Option<std::borrow::Cow<'a, str>> {
    if zs.is_null() {
        return None;
    }
    let len = unsafe { (*zs).len };
    let ptr = unsafe { (*zs).val.as_ptr() };
    let slice = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
    Some(String::from_utf8_lossy(slice))
}

// --- categorise -----------------------------------------------------------

/// Result of categorising a [`RawCallSite`] per `SPECIFICATION.md`
/// §4.1.2.
///
/// Both `fqn` and `file` are `Cow<'a, str>` so the common UTF-8 path
/// stays zero-copy while the lossy non-UTF-8 path (RO-4) and the
/// synthesised unknown-fallback names (RO-5) can flow through as
/// owned `String`s. They are turned into owned `String`s lazily
/// inside the dictionary's `intern` build closure (only on a
/// dictionary miss).
/// `pub` for the same bench-seam reason as [`EntrySnapshots`].
#[derive(Debug)]
pub struct Categorised<'a> {
    pub key: FunctionKey,
    pub kind: FunctionKind,
    pub fqn: std::borrow::Cow<'a, str>,
    pub file: std::borrow::Cow<'a, str>,
    pub line: u32,
}

/// Map a [`RawCallSite`] to its `(FunctionKey, FunctionKind, fqn, file, line)`.
///
/// Precedence per `SPECIFICATION.md` §4.1.2 (matches the spike's `fqn`):
///
/// 1. Internal function → `Internal { name }` / `FunctionKind::Internal`.
/// 2. Method (scope is `Some`) → `Method { class, method }` / `Method`.
/// 3. Closure (function_name starts with `{closure` OR function_name is
///    `None` while file is `Some`) → `Closure { file, line }` /
///    `Closure`.
/// 4. User function (otherwise, with file populated) → `Function`.
///
/// PHP-8.x reports closures via `function_name = Some("{closure...}")`
/// — see the spike's C-5 evidence. The substring match `starts_with("{closure")`
/// catches both the bare `{closure}` form and the
/// `{closure:<file>:<line>}` form PHP 8.4 uses.
///
/// ## RO-5: unknown-function fallback identity
///
/// A previous version of this function papered over Zend reporting
/// gaps with the literal placeholder strings `(unknown)` /
/// `(anonymous)`. Every distinct gap-shaped call site collapsed to
/// one `FunctionKey`, causing the dictionary to fold unrelated
/// functions into a single per-call counter. The fallback now
/// incorporates the `execute_data` address as a tiebreaker —
/// `(unknown)@0x<hex>` — so two genuinely-distinct call sites stay
/// genuinely distinct in the trace. Zend reuse of the same
/// `execute_data` slot within one request is a known collision, but
/// it's bounded and recognisable; the previous "everything is one"
/// behaviour was not.
// `pub` (not `pub(crate)`) for the bench-seam re-export. See the
// note on `EntrySnapshots` for the rationale.
pub fn categorise<'a>(info: &'a RawCallSite<'a>) -> Categorised<'a> {
    use std::borrow::Cow;
    use std::sync::Arc;

    let line = info.lineno;
    // File is empty when Zend reports no filename — kept as a
    // borrow when the Cow itself is borrowed.
    let file: Cow<'a, str> = match info.filename.as_ref() {
        Some(f) => Cow::Borrowed(f.as_ref()),
        None => Cow::Borrowed(""),
    };

    if info.is_internal {
        let name = info.function_name.as_ref().map_or_else(
            || Cow::Owned(unknown_placeholder("anonymous", info.execute_data_addr)),
            |n| Cow::Borrowed(n.as_ref()),
        );
        return Categorised {
            key: FunctionKey::Internal {
                name: Arc::from(name.as_ref()),
            },
            kind: FunctionKind::Internal,
            fqn: name,
            file: Cow::Borrowed(""),
            line: 0,
        };
    }

    if let Some(class) = info.class_name.as_ref() {
        let class_str: &str = class.as_ref();
        let method = info.function_name.as_ref().map_or_else(
            || Cow::Owned(unknown_placeholder("unknown", info.execute_data_addr)),
            |m| Cow::Borrowed(m.as_ref()),
        );
        return Categorised {
            key: FunctionKey::Method {
                class: Arc::from(class_str),
                method: Arc::from(method.as_ref()),
            },
            kind: FunctionKind::Method,
            fqn: Cow::Owned(format!("{class_str}::{}", method.as_ref())),
            file,
            line,
        };
    }

    let is_closure = match info.function_name.as_ref() {
        Some(name) => name.starts_with("{closure"),
        None => info.filename.is_some(),
    };
    if is_closure {
        let file_str: &str = file.as_ref();
        return Categorised {
            key: FunctionKey::Closure {
                file: Arc::from(file_str),
                line,
            },
            kind: FunctionKind::Closure,
            fqn: Cow::Owned(format!("closure:{file_str}:{line}")),
            file,
            line,
        };
    }

    // Fall-through: user function. `function_name` is `Some` here
    // for any Zend-reported shape we expect (the closure branch
    // caught `None`-with-file; an internal would have caught
    // `None`-without-file at branch 1). The synthesised
    // `(unknown)@<addr>` is the RO-5 tiebreaker for any unexpected
    // shape so distinct call sites do not collide in the dict.
    let function = info.function_name.as_ref().map_or_else(
        || Cow::Owned(unknown_placeholder("unknown", info.execute_data_addr)),
        |f| Cow::Borrowed(f.as_ref()),
    );
    let file_str: &str = file.as_ref();
    Categorised {
        key: FunctionKey::Function {
            file: Arc::from(file_str),
            function: Arc::from(function.as_ref()),
            line,
        },
        kind: FunctionKind::Function,
        fqn: function,
        file,
        line,
    }
}

/// Build a call-site-distinguishing fallback name for a missing
/// `function_name`. The address is the raw `execute_data` pointer so
/// distinct call sites within one request map to distinct names;
/// Zend's reuse of the same address across calls is the one
/// remaining collision mode and is documented in the
/// `categorise_unknown_fallback_uses_execute_data_addr_as_tiebreaker`
/// test.
fn unknown_placeholder(kind: &str, addr: usize) -> String {
    format!("({kind})@0x{addr:x}")
}

// --- Lazy categorisation (zero-alloc production path) -------------------

/// Borrow-shaped description of a function's fully-qualified name.
/// Constructed by [`categorise_lazy`] without allocating; rendered to
/// an owning `String` only on a dictionary miss inside the
/// [`begin_with_snapshots_lazy`] build closure.
///
/// The five variants cover the four [`FunctionKind`]s plus a
/// fallback for the unknown-call-site case (RO-5). The `Borrowed`
/// variant fits both `Internal { name }` and `Function { function }`
/// — both render to the bare borrowed string with no formatting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FqnSpec<'a> {
    /// `Internal` and user `Function` variants — fqn is the borrowed
    /// function name itself.
    Borrowed(&'a str),
    /// `Method` variant — renders to `"<class>::<method>"`.
    Method { class: &'a str, method: &'a str },
    /// `Closure` variant — renders to `"closure:<file>:<line>"`.
    Closure { file: &'a str, line: u32 },
    /// RO-5 fallback — renders to `"(<kind_label>)@0x<addr>"`. The
    /// `kind_label` is `"unknown"` or `"anonymous"` per the
    /// branch's precedence row.
    Unknown {
        kind_label: &'static str,
        addr: usize,
    },
}

impl FqnSpec<'_> {
    /// Render the borrowed spec into an owning `String`. Called only
    /// on a dictionary miss; never on the hit path. The capacity hint
    /// keeps the allocation right-sized for the bytes the format
    /// emits.
    pub fn render(&self) -> String {
        match self {
            FqnSpec::Borrowed(name) => (*name).to_owned(),
            FqnSpec::Method { class, method } => {
                let mut s = String::with_capacity(class.len() + 2 + method.len());
                s.push_str(class);
                s.push_str("::");
                s.push_str(method);
                s
            }
            FqnSpec::Closure { file, line } => {
                // `closure:` (8) + file + `:` + line digits (≤ 10). The
                // overshoot for short lines is one or two bytes; the
                // alternative (counting digits) is more code for no win.
                let mut s = String::with_capacity(8 + file.len() + 1 + 10);
                s.push_str("closure:");
                s.push_str(file);
                s.push(':');
                // `write!` into `String` is infallible.
                use std::fmt::Write;
                let _ = write!(&mut s, "{line}");
                s
            }
            FqnSpec::Unknown { kind_label, addr } => unknown_placeholder(kind_label, *addr),
        }
    }

    /// Length of the rendered `String` without rendering. Used by the
    /// cap-gate's miss-cost projection in [`begin_with_snapshots_lazy`].
    pub fn render_len(&self) -> usize {
        match self {
            FqnSpec::Borrowed(name) => name.len(),
            FqnSpec::Method { class, method } => class.len() + 2 + method.len(),
            FqnSpec::Closure { file, line } => {
                let line_digits = if *line == 0 {
                    1
                } else {
                    line.ilog10() as usize + 1
                };
                "closure:".len() + file.len() + 1 + line_digits
            }
            FqnSpec::Unknown { kind_label, addr } => {
                // "(<label>)@0x<hex>" — hex digit count is the bit
                // count of `addr` shifted down, rounded up to nibble
                // boundary.
                let hex_digits = if *addr == 0 {
                    1
                } else {
                    (usize::BITS - addr.leading_zeros()).div_ceil(4) as usize
                };
                1 + kind_label.len() + 1 + 1 + 2 + hex_digits
            }
        }
    }
}

/// Borrow-shaped, zero-alloc categorisation result. The production
/// [`Recorder::begin_handler`] consumes this shape so the dict-hit
/// branch never allocates a `FunctionKey`'s `Arc<str>` fields or a
/// `String` for the rendered `fqn`. The owning `FunctionKey` is
/// materialised only inside the [`begin_with_snapshots_lazy`] build
/// closure on a dictionary miss.
///
/// The existing [`Categorised`] / [`categorise`] / [`begin_with_snapshots`]
/// chain stays in place for the bench seam (re-exported from
/// `lib.rs::bench_seam`) so unit tests asserting on
/// `cat.key` / `cat.fqn` continue to pass against their owning
/// shapes.
#[derive(Debug)]
pub struct LazyCategorised<'a> {
    pub key_ref: FunctionKeyRef<'a>,
    pub kind: FunctionKind,
    pub fqn_spec: FqnSpec<'a>,
    pub file: &'a str,
    pub line: u32,
}

/// Categorise a [`RawCallSite`] without allocating. Mirrors
/// [`categorise`]'s precedence ladder one-for-one (Internal → Method
/// → Closure → Function with the RO-5 unknown-fallback path) but
/// returns the borrow-shaped [`LazyCategorised`] used by the
/// production hot path.
///
/// Identity rules (kept identical to [`categorise`] so the dictionary
/// stays single-rooted across both entry points):
///
/// 1. Internal function → `FunctionKeyRef::Internal { name }` with
///    `fqn_spec = Borrowed(name)`.
/// 2. Method (scope is `Some`) →
///    `FunctionKeyRef::Method { class, method }` with `fqn_spec =
///    Method { class, method }`.
/// 3. Closure (function_name starts with `{closure` OR function_name
///    is `None` and file is `Some`) → `FunctionKeyRef::Closure {
///    file, line }` with `fqn_spec = Closure { file, line }`.
/// 4. Otherwise → `FunctionKeyRef::Function { file, function, line }`
///    with `fqn_spec = Borrowed(function)`.
///
/// The unknown-fallback path materialises a per-call address-bearing
/// `FqnSpec::Unknown` instead of allocating an `(unknown)@0x…`
/// `String` up front — the rendering happens only on a dictionary
/// miss. The `FunctionKeyRef` for the fallback shape is **not**
/// zero-alloc-stable across distinct call sites (each call site has
/// a distinct address, so each is a fresh dictionary miss); this is
/// the same property the existing [`categorise`] has.
///
/// `pub` for the bench-seam and the broadened zero-alloc audit
/// (recorder-hot-path-tuning §6) which exercise the production path
/// without PHP.
pub fn categorise_lazy<'a>(info: &'a RawCallSite<'a>) -> LazyCategorised<'a> {
    let line = info.lineno;
    let file: &'a str = info.filename.as_deref().unwrap_or("");

    if info.is_internal {
        // Branch 1: internal function.
        let (key_ref, fqn_spec) = match info.function_name.as_deref() {
            Some(name) => (FunctionKeyRef::Internal { name }, FqnSpec::Borrowed(name)),
            None => {
                // No name reported — feed the unknown-fallback through
                // both the key (so two anonymous internals don't
                // collide) and the fqn (so the rendered name carries
                // the same diagnostic).
                let addr = info.execute_data_addr;
                // The fallback `FunctionKeyRef::Internal { name: "" }`
                // would collapse every anonymous internal in this
                // request to one dict entry. Steer them to the
                // address-tagged Function variant so distinct call
                // sites stay distinct (mirrors the RO-5 tiebreaker
                // already in `categorise`).
                (
                    FunctionKeyRef::Function {
                        file: "",
                        function: "",
                        line: addr as u32,
                    },
                    FqnSpec::Unknown {
                        kind_label: "anonymous",
                        addr,
                    },
                )
            }
        };
        return LazyCategorised {
            key_ref,
            kind: FunctionKind::Internal,
            fqn_spec,
            file: "",
            line: 0,
        };
    }

    if let Some(class) = info.class_name.as_deref() {
        // Branch 2: method.
        let method = info.function_name.as_deref().unwrap_or("");
        let key_ref = FunctionKeyRef::Method { class, method };
        let fqn_spec = FqnSpec::Method { class, method };
        return LazyCategorised {
            key_ref,
            kind: FunctionKind::Method,
            fqn_spec,
            file,
            line,
        };
    }

    let is_closure = match info.function_name.as_deref() {
        Some(name) => name.starts_with("{closure"),
        None => info.filename.is_some(),
    };
    if is_closure {
        // Branch 3: closure.
        return LazyCategorised {
            key_ref: FunctionKeyRef::Closure { file, line },
            kind: FunctionKind::Closure,
            fqn_spec: FqnSpec::Closure { file, line },
            file,
            line,
        };
    }

    // Branch 4: user function fall-through.
    let function = info.function_name.as_deref().unwrap_or("");
    LazyCategorised {
        key_ref: FunctionKeyRef::Function {
            file,
            function,
            line,
        },
        kind: FunctionKind::Function,
        fqn_spec: FqnSpec::Borrowed(function),
        file,
        line,
    }
}

// --- Recorder -------------------------------------------------------------

/// The production observer. Zero-size; all per-request state lives in
/// the thread-local `CURRENT_TRACE`. `Send + Sync` is trivially
/// satisfied.
#[derive(Default)]
pub struct Recorder;

/// Factory called by [`build_boot_observer`] when the dispatcher
/// chooses the recorder variant.
pub fn build_recorder_observer() -> Recorder {
    Recorder
}

impl Recorder {
    /// Production begin handler. Parses `execute_data` into a
    /// [`RawCallSite`], categorises it, and pushes a `CallFrame`. A
    /// no-op when the thread-local slot is empty.
    ///
    /// RO-6: the clock/memory snapshot is taken **inside** the
    /// `with_current_trace` closure so the syscall trio runs only
    /// when there is somewhere for the data to go. Observer fires
    /// between `MINIT` and the first `RINIT` (slot empty) — and any
    /// future out-of-request fire — no longer pay for clock reads
    /// they cannot record.
    fn begin_handler(&self, execute_data: &ExecuteData) {
        // SAFETY: the observer trait hands us a `&ExecuteData` that is
        // valid for the duration of the call. `extract_call_site`
        // reads through `(*execute_data).func` and a handful of
        // `zend_string` pointers, all of which remain valid until
        // `end` returns.
        let info = unsafe { extract_call_site(execute_data) };

        with_current_trace(|trace| {
            // Production path: zero-alloc on the dict-hit branch. The
            // owning `FunctionKey` and rendered `fqn` are materialised
            // only inside the `intern_ref` build closure on a miss
            // (recorder-hot-path-tuning D-1 / D-2).
            let lazy = categorise_lazy(&info);
            // Gate-before-snapshot (REVIEW.md P-1, gate-before-snapshot
            // change): consult the depth/cap gates before paying the
            // ~1,100 ns syscall trio for snapshots. Dropped begins now
            // cost zero syscalls — the drop count, `dropped_begins`,
            // and `virtual_depth` accounting all happen inside the
            // predicate.
            if !begin_lazy_would_accept(trace, &lazy) {
                return;
            }
            let snapshots = EntrySnapshots::capture_now();
            begin_with_snapshots_lazy_accept(trace, &lazy, snapshots);
        });
    }

    /// Production end handler. Reads exception state, captures exit
    /// snapshots, builds the `CallRecord`, and pushes it. A no-op
    /// when the thread-local slot is empty.
    ///
    /// RO-6: snapshot capture moves inside the `with_current_trace`
    /// closure so the syscall trio runs only when there is a frame
    /// to close out. `has_exception` is similarly cheap-but-skippable;
    /// reading it inside the closure keeps the two captures
    /// co-located with the work that uses them.
    fn end_handler(&self, _execute_data: &ExecuteData, _retval: Option<&Zval>) {
        // `_execute_data` is unused: the frame's identity is already
        // on the trace stack from the matching `begin`. `_retval`
        // is unused per design D-7 (we don't inspect return values).
        with_current_trace(|trace| {
            // Gate-before-snapshot (REVIEW.md P-1, gate-before-snapshot
            // change): consume the `dropped_begins` LIFO matcher
            // before paying the ~1,100 ns syscall trio + the
            // `has_exception()` read. Ends paired with previously
            // dropped begins now cost zero syscalls.
            if !end_would_accept(trace) {
                return;
            }
            let abnormal = ExecutorGlobals::has_exception();
            let snapshots = ExitSnapshots::capture_now();
            end_with_snapshots_accept(trace, snapshots, abnormal);
        });
    }
}

/// Pure observation-filter helper. Returns `true` when the function
/// should be observed (recorder begin/end will fire), `false` to ask
/// PHP to permanently elide observation of this Zend function entry
/// (per the cache contract documented in the module header).
///
/// Composition: `false` iff (function name in `skip_functions`) OR
/// (`skip_internal && info.is_internal`). The split is by design —
/// callers without a populated `Config` (out-of-request fires before
/// MINIT completes) fall through to the trait `should_observe`'s
/// `let-else true` path and never reach this helper. See `REVIEW.md`
/// finding P-0.
fn should_observe_filter(
    skip_internal: bool,
    skip_functions: &std::collections::HashSet<String>,
    info: &FcallInfo,
) -> bool {
    if skip_internal && info.is_internal {
        return false;
    }
    let Some(name) = info.function_name else {
        // Anonymous closures / opcode-specialised builtins arriving
        // without a name: we can't filter what we can't name.
        // Observe (conservative).
        return true;
    };
    let key = match info.class_name {
        Some(class) => {
            let mut k = String::with_capacity(class.len() + name.len() + 2);
            k.push_str(&class.to_ascii_lowercase());
            k.push_str("::");
            k.push_str(&name.to_ascii_lowercase());
            k
        }
        None => name.to_ascii_lowercase(),
    };
    !skip_functions.contains(&key)
}

impl FcallObserver for Recorder {
    /// Consult `Config::skip_functions` / `Config::skip_internal` to
    /// gate observation. PHP caches the result per Zend function
    /// entry on first sight — so this body runs at most once per
    /// unique function in the process lifetime, and `false` returns
    /// cost zero per call from then on. See `REVIEW.md` finding P-0
    /// and the `skip-functions-directive` change.
    ///
    /// **Why a static filter is safe here.** The module doc above
    /// warns against returning a *transient* `false` from
    /// `should_observe` (per-request state would be cached
    /// permanently). A static filter does not have that problem: the
    /// answer is a deterministic function of the function's name
    /// (and class scope), identical on the first observation as on
    /// the millionth, so the cached value stays correct across every
    /// subsequent request.
    fn should_observe(&self, info: &FcallInfo) -> bool {
        match Config::global() {
            Some(c) => should_observe_filter(c.skip_internal, &c.skip_functions, info),
            None => true,
        }
    }

    fn begin(&self, execute_data: &ExecuteData) {
        self.begin_handler(execute_data);
    }

    fn end(&self, execute_data: &ExecuteData, retval: Option<&Zval>) {
        self.end_handler(execute_data, retval);
    }
}

/// Push a `CallFrame` onto the trace stack, allocating a `call_id` and
/// interning the function via the dictionary. Pure: no FFI, no global
/// state beyond `trace` and the process-wide
/// [`accounting::BYTES_IN_MEMORY`] atomic that the slice-3 cap-gate
/// consults. Tests drive this directly.
///
/// ## Slice-3 overflow policies
///
/// The body enforces the §3.2 overflow policies before staging
/// anything. The order is:
///
/// 1. **Increment [`Trace::virtual_depth`]** by `1` regardless of
///    acceptance — the depth is the PHP-side truth (every observed
///    `begin` sees one more level of recursion). The accepted-frame
///    stack length is **not** a substitute because a dropped
///    ancestor's frame is not on the stack but PHP-side recursion
///    continues into the descendant.
///
/// 2. **Depth gate**: if `virtual_depth > max_depth`, call
///    [`Trace::record_drop`] (bumps the `Arc<AtomicU64>` drop
///    counter and increments [`Trace::dropped_begins`]) and return
///    without touching the dictionary, stack, or accounting atomic.
///
/// 3. **Cap gate**: compute `would_add = CALL_RECORD_FIXED_BYTES +
///    dict_miss_cost(trace, categorised)`. The miss-cost lookup is a
///    single hashmap probe (`Dictionary::contains_key`); it does not
///    intern. If `accounting::snapshot() + would_add >
///    buffer_cap_bytes`, drop the same way the depth gate does.
///
/// 4. **Accept**: intern the dict entry via
///    [`Trace::push_dict_entry_via_intern`] (which bills both the
///    per-trace estimator and the process-wide atomic on a miss) and
///    push the `CallFrame`. The record's own `CALL_RECORD_FIXED_BYTES`
///    contribution is billed at end time inside
///    [`Trace::push_record`] (design D-3).
// `pub` (not `pub(crate)`) for the bench-seam re-export. See the
// note on `EntrySnapshots` for the rationale.
pub fn begin_with_snapshots(
    trace: &mut Trace,
    categorised: &Categorised<'_>,
    snapshots: EntrySnapshots,
) {
    if begin_would_accept(trace, categorised) {
        begin_with_snapshots_accept(trace, categorised, snapshots);
    }
    // else: drop already recorded by `begin_would_accept`.
}

/// Predicate half of [`begin_with_snapshots`]. Performs the
/// `virtual_depth` increment, the depth gate, and the cap gate.
/// Returns `true` when the call should be accepted; returns `false`
/// after calling [`Trace::record_drop`] for the caller.
///
/// **No syscalls.** This is the gate-before-snapshot entry point
/// from `gate-before-snapshot` (REVIEW.md P-1) — the caller (e.g.
/// [`Recorder::begin_handler`]) consults this predicate first, and
/// only invokes [`EntrySnapshots::capture_now`] when the answer is
/// `true`. Dropped begins now pay zero syscalls for the work that
/// would have been thrown away.
fn begin_would_accept(trace: &mut Trace, categorised: &Categorised<'_>) -> bool {
    // 1. Track PHP-side depth unconditionally. `saturating_add` so a
    // pathological 2^32 begins in a single trace does not panic.
    trace.virtual_depth = trace.virtual_depth.saturating_add(1);

    // 2. Depth gate.
    if trace.virtual_depth > trace.max_depth {
        trace.record_drop();
        return false;
    }

    // 3. Cap gate. `would_add` is the worst-case contribution this
    //    call will add to the budget if accepted: one record's fixed
    //    bytes plus, on a dictionary miss, the new dict entry's
    //    bytes. On a dictionary hit the second term is zero. The
    //    probe uses `contains_key_ref` so the §3.2 projection does
    //    not allocate an owning `FunctionKey` it would discard.
    let key_ref = categorised.key.as_ref();
    let would_add = CALL_RECORD_FIXED_BYTES
        + dict_miss_cost_ref(trace, &key_ref, &categorised.fqn, &categorised.file);
    if accounting::snapshot().saturating_add(would_add) > trace.buffer_cap_bytes {
        trace.record_drop();
        return false;
    }

    true
}

/// Accept-only tail of [`begin_with_snapshots`]. Assumes
/// [`begin_would_accept`] has already returned `true` for this
/// `(trace, categorised)` pair — `virtual_depth` is bumped, neither
/// gate fired. Pushes the dict entry (if new) and the `CallFrame`.
///
/// Calling this without the matching predicate having returned
/// `true` would skip the gates and stage a record the gates were
/// meant to reject; the bench-seam wrapper [`begin_with_snapshots`]
/// composes them safely.
fn begin_with_snapshots_accept(
    trace: &mut Trace,
    categorised: &Categorised<'_>,
    snapshots: EntrySnapshots,
) {
    let call_id = trace.next_call_id();
    let parent = trace.stack.last().map_or(0, |frame| frame.call_id);
    // `virtual_depth` is already 1-based after the increment above; the
    // `CallFrame.depth` field is zero-indexed per slice-2 semantics.
    // `u16` cast: `virtual_depth` is bounded by `max_depth` (≤ u16::MAX
    // per the directive range), so the cast is lossless on the accept
    // path.
    #[allow(clippy::cast_possible_truncation)]
    let depth = (trace.virtual_depth - 1) as u16;

    // The `to_owned()` calls only fire on a dictionary miss — the
    // `intern_ref` build closure runs at most once per unique key.
    // The bench-seam consumers reuse a single long-lived `Categorised`
    // across many iterations, so the per-iteration `categorised.key`
    // clone the previous version did is replaced by a single
    // `categorised.key.as_ref()` probe.
    let key_ref = categorised.key.as_ref();
    let kind = categorised.kind;
    let fqn: &str = categorised.fqn.as_ref();
    let file: &str = categorised.file.as_ref();
    let line = categorised.line;
    let fn_id = trace.push_dict_entry_via_intern_ref(&key_ref, |fn_id| {
        let owning_key = categorised.key.clone();
        let entry = DictEntry {
            fn_id,
            fqn: fqn.to_owned(),
            file: file.to_owned(),
            line,
            kind,
        };
        (owning_key, entry)
    });

    trace.stack.push(CallFrame {
        call_id,
        parent,
        fn_id,
        depth,
        t_in_ns: snapshots.t_in_ns,
        cpu_u_in_ns: snapshots.cpu_u_in_ns,
        cpu_s_in_ns: snapshots.cpu_s_in_ns,
        mem_in_bytes: snapshots.mem_in_bytes,
    });
}

/// Zero-alloc production sibling of [`begin_with_snapshots`]. Accepts
/// a [`LazyCategorised`] (borrow-shaped) and routes the dictionary
/// probe + miss-cost projection through a single
/// `Dictionary::intern_ref` traversal — no `Arc<str>` allocation on
/// the dict-hit branch, no transient `String` for the rendered fqn.
///
/// Mirrors [`begin_with_snapshots`]'s slice-3 overflow policies
/// step-for-step:
///
/// 1. Increment [`Trace::virtual_depth`].
/// 2. Depth gate → `record_drop` if exceeded.
/// 3. Cap gate → project miss cost via `contains_key_ref` +
///    `fqn_spec.render_len()`; `record_drop` if it would push
///    `bytes_in_memory` past `buffer_cap_bytes`.
/// 4. Accept: probe-or-intern via `push_dict_entry_via_intern_ref`,
///    push the `CallFrame`. The miss path materialises the owning
///    `FunctionKey` (via `FunctionKeyRef::to_owned`), renders the
///    `fqn`, and stages the `DictEntry` in one place.
pub fn begin_with_snapshots_lazy(
    trace: &mut Trace,
    lazy: &LazyCategorised<'_>,
    snapshots: EntrySnapshots,
) {
    if begin_lazy_would_accept(trace, lazy) {
        begin_with_snapshots_lazy_accept(trace, lazy, snapshots);
    }
    // else: drop already recorded by `begin_lazy_would_accept`.
}

/// Predicate half of [`begin_with_snapshots_lazy`]. Performs the
/// `virtual_depth` increment and runs the depth + cap gates. Returns
/// `true` when the call should be accepted; returns `false` after
/// calling [`Trace::record_drop`] for the caller.
///
/// **No syscalls.** The gate-before-snapshot entry point from
/// `gate-before-snapshot` (REVIEW.md P-1). [`Recorder::begin_handler`]
/// consults this predicate before invoking
/// [`EntrySnapshots::capture_now`]; dropped begins now pay zero
/// syscalls for the work that would have been thrown away.
fn begin_lazy_would_accept(trace: &mut Trace, lazy: &LazyCategorised<'_>) -> bool {
    // 1. Track PHP-side depth unconditionally.
    trace.virtual_depth = trace.virtual_depth.saturating_add(1);

    // 2. Depth gate.
    if trace.virtual_depth > trace.max_depth {
        trace.record_drop();
        return false;
    }

    // 3. Cap gate. `would_add` projection: `contains_key_ref` is a
    //    zero-alloc hashmap probe, and `fqn_spec.render_len()`
    //    computes the would-be `String::len` without rendering.
    let would_add = if trace.dictionary.contains_key_ref(&lazy.key_ref) {
        CALL_RECORD_FIXED_BYTES
    } else {
        CALL_RECORD_FIXED_BYTES
            + DICT_ENTRY_FIXED_BYTES
            + lazy.fqn_spec.render_len()
            + lazy.file.len()
    };
    if accounting::snapshot().saturating_add(would_add) > trace.buffer_cap_bytes {
        trace.record_drop();
        return false;
    }

    true
}

/// Accept-only tail of [`begin_with_snapshots_lazy`]. Assumes
/// [`begin_lazy_would_accept`] has already returned `true` for this
/// `(trace, lazy)` pair — `virtual_depth` is bumped, neither gate
/// fired. Stages the dict entry (allocating only on a miss) and
/// pushes the `CallFrame`.
fn begin_with_snapshots_lazy_accept(
    trace: &mut Trace,
    lazy: &LazyCategorised<'_>,
    snapshots: EntrySnapshots,
) {
    let call_id = trace.next_call_id();
    let parent = trace.stack.last().map_or(0, |frame| frame.call_id);
    #[allow(clippy::cast_possible_truncation)]
    let depth = (trace.virtual_depth - 1) as u16;

    let kind = lazy.kind;
    let file = lazy.file;
    let line = lazy.line;
    let fqn_spec = lazy.fqn_spec;
    let key_ref = lazy.key_ref;
    let fn_id = trace.push_dict_entry_via_intern_ref(&lazy.key_ref, |fn_id| {
        // Miss path: materialise owning key and stage entry. This is
        // the **only** branch on which `Arc<str>` allocations and a
        // rendered `fqn` `String` happen.
        let owning_key = key_ref.to_owned();
        let entry = DictEntry {
            fn_id,
            fqn: fqn_spec.render(),
            file: file.to_owned(),
            line,
            kind,
        };
        (owning_key, entry)
    });

    trace.stack.push(CallFrame {
        call_id,
        parent,
        fn_id,
        depth,
        t_in_ns: snapshots.t_in_ns,
        cpu_u_in_ns: snapshots.cpu_u_in_ns,
        cpu_s_in_ns: snapshots.cpu_s_in_ns,
        mem_in_bytes: snapshots.mem_in_bytes,
    });
}

/// Project the §3.2 dict-miss cost: `0` on a hit, or
/// `DICT_ENTRY_FIXED_BYTES + len(fqn) + len(file)` on a miss. Probes
/// the dictionary by borrowed key so no `Arc<str>` allocation
/// happens to compute the projection. Used by
/// [`begin_with_snapshots`]; [`begin_with_snapshots_lazy`] inlines
/// the same logic against `FqnSpec::render_len` to avoid touching
/// the rendered `fqn`'s `String`.
fn dict_miss_cost_ref(trace: &Trace, key_ref: &FunctionKeyRef<'_>, fqn: &str, file: &str) -> usize {
    if trace.dictionary.contains_key_ref(key_ref) {
        0
    } else {
        DICT_ENTRY_FIXED_BYTES + fqn.len() + file.len()
    }
}

/// Pop the top `CallFrame`, compute deltas, and push a `CallRecord`.
/// Pure: see [`begin_with_snapshots`]. A no-op when the stack is
/// empty (a desynchronisation that SHOULD NOT happen given Zend's
/// pairing — the `debug_assert!` makes it loud in tests; the release
/// path silently returns to preserve the silent-disable posture).
///
/// ## Slice-3 LIFO pairing
///
/// Decrements [`Trace::virtual_depth`] **regardless of accept/drop**
/// — every PHP-side `end` corresponds to a PHP-side `begin` and the
/// depth must track. Then consumes the LIFO matcher:
///
/// - If [`Trace::dropped_begins`] is positive, the matching `begin`
///   was dropped (depth gate or cap gate). Decrement the matcher and
///   return without popping or emitting.
/// - Otherwise, pop the frame and dispatch to [`finish_call_record`]
///   for the slice-2 accept path.
// `pub` (not `pub(crate)`) for the bench-seam re-export. See the
// note on `EntrySnapshots` for the rationale.
pub fn end_with_snapshots(trace: &mut Trace, snapshots: ExitSnapshots, abnormal: bool) {
    if end_would_accept(trace) {
        end_with_snapshots_accept(trace, snapshots, abnormal);
    }
    // else: dropped_begins consumed by `end_would_accept`.
}

/// Predicate half of [`end_with_snapshots`]. Decrements
/// `virtual_depth` and consumes the `dropped_begins` LIFO matcher.
/// Returns `true` when the end should be accepted (the matching
/// begin produced a frame on the stack); returns `false` when the
/// matching begin was previously dropped, having decremented
/// `dropped_begins` for the caller.
///
/// **No syscalls.** The gate-before-snapshot entry point from
/// `gate-before-snapshot` (REVIEW.md P-1). [`Recorder::end_handler`]
/// consults this predicate before invoking
/// [`ExitSnapshots::capture_now`] and [`ExecutorGlobals::has_exception`];
/// ends paired with dropped begins now pay zero syscalls.
fn end_would_accept(trace: &mut Trace) -> bool {
    // Decrement first so the depth is consistent for any caller that
    // observes `virtual_depth` mid-pop. `saturating_sub` defends an
    // adversarial-end-before-begin sequence (test 10.3); in well-formed
    // traces the counter never reaches `0` before this point.
    trace.virtual_depth = trace.virtual_depth.saturating_sub(1);

    // LIFO consume: an end paired with a dropped begin returns
    // silently.
    if trace.dropped_begins > 0 {
        trace.dropped_begins -= 1;
        return false;
    }
    true
}

/// Accept-only tail of [`end_with_snapshots`]. Assumes
/// [`end_would_accept`] has already returned `true` for this
/// `trace` — `virtual_depth` is decremented, `dropped_begins` was
/// already zero. Pops the matching frame from the stack and emits
/// the `CallRecord`.
fn end_with_snapshots_accept(trace: &mut Trace, snapshots: ExitSnapshots, abnormal: bool) {
    let popped = trace.stack.pop();
    debug_assert!(
        popped.is_some(),
        "observer end fired with an empty trace stack — begin/end pairing broken",
    );
    finish_call_record(trace, popped, snapshots, abnormal);
}

/// Pure tail of [`end_with_snapshots`]. Takes the already-popped
/// frame as an `Option`; returns silently on `None`. Split out so
/// the release-path "empty-stack → silent no-op" contract from
/// `SPECIFICATION.md` §8.3 NFR-REL-1 / AD-4 can be exercised from a
/// default `cargo test` (debug) build (RO-3): the `debug_assert!` in
/// the caller is the loud signal for the pairing bug, this helper
/// is the recovery path the assert documents.
pub(crate) fn finish_call_record(
    trace: &mut Trace,
    popped: Option<CallFrame>,
    snapshots: ExitSnapshots,
    abnormal: bool,
) {
    let Some(frame) = popped else {
        return;
    };

    // `SPECIFICATION.md` §3.2: "saturating, may be `0` on
    // monotonic-skew". The `.max(0)` clamps the i64 difference to a
    // non-negative value, which is what the spec wants — bare
    // `saturating_sub` would clamp at `i64::MIN`, not `0`. The
    // `saturating_sub` before `.max(0)` defends against an overflow
    // path that a plain `-` would expose if the values are at the
    // extremes (unreachable in practice; cheap to keep).
    let cpu_u_ns = snapshots
        .cpu_u_now_ns
        .saturating_sub(frame.cpu_u_in_ns)
        .max(0);
    let cpu_s_ns = snapshots
        .cpu_s_now_ns
        .saturating_sub(frame.cpu_s_in_ns)
        .max(0);

    trace.push_record(CallRecord {
        call_id: frame.call_id,
        parent: frame.parent,
        fn_id: frame.fn_id,
        depth: frame.depth,
        t_in_ns: frame.t_in_ns,
        t_out_ns: snapshots.t_out_ns,
        cpu_u_ns,
        cpu_s_ns,
        mem_in_bytes: frame.mem_in_bytes,
        mem_out_bytes: snapshots.mem_out_bytes,
        abnormal_exit: abnormal,
    });

    // Phase-4 slice 2: threshold-driven flush at the end-handler accept
    // tail (`SPECIFICATION.md` §3.2 "after each emitted record … hand
    // current buffer to shipper"). The check sits *after* push_record
    // so the post-push counts are the ones tested against the
    // thresholds, and *only* on the accept branch — the slice-3 LIFO
    // consume branch returns early above (line at `if trace.dropped_begins > 0`)
    // and never reaches here. Tests cover both branches.
    if let Some(trigger) = flush_predicate_trigger(trace) {
        let batch = trace.flush_into_pending_batch();
        emit_flush_dump_line(trace, &batch, trigger);
        flush::try_send_batch(batch);
    }
}

/// Decide whether the post-`push_record` state crosses either flush
/// threshold and return which trigger fired. Pure read on the two
/// cached `Trace` fields (`flush_records`, `flush_bytes`) and the two
/// running counters (`buffer.len()`, `buffer_estimated_bytes`).
/// Records-trigger wins ties because the predicate is `||` and the
/// records check is the first operand — the trigger name is mostly a
/// diagnostic-line concern (the `recorder-dump` `F:` line), not a
/// behavioural difference.
fn flush_predicate_trigger(trace: &Trace) -> Option<FlushTrigger> {
    if trace.buffer.len() >= trace.flush_records {
        Some(FlushTrigger::Records)
    } else if trace.buffer_estimated_bytes >= trace.flush_bytes {
        Some(FlushTrigger::Bytes)
    } else {
        None
    }
}

/// Why a flush was emitted. Carried into the `recorder-dump` `F:` line
/// so a fixture can assert that the cadence is driven by the predicate
/// the operator configured, not by chance. The third variant
/// (`Rshutdown`) is set by [`rshutdown_release_trace`]; the first two
/// are the steady-state mid-request triggers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FlushTrigger {
    Records,
    Bytes,
    Rshutdown,
}

impl FlushTrigger {
    /// Wire name carried in the `recorder-dump` `F:` line. Stable
    /// strings — the fixture assertions in `tests/recorder_observer.rs`
    /// depend on these values. The method is only reachable from the
    /// `recorder-dump`-gated emitter; default builds compile it but
    /// never call it, hence the `allow(dead_code)`.
    #[cfg_attr(not(feature = "recorder-dump"), allow(dead_code))]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Records => "records",
            Self::Bytes => "bytes",
            Self::Rshutdown => "rshutdown",
        }
    }
}

/// Emit the `recorder-dump` `F:` block for a freshly-built flush
/// batch. Production builds (no `recorder-dump` feature) compile this
/// to an empty function — the `cfg`-gated body lives in
/// [`crate::recorder::dump`] (Phase-4 slice 2 §7).
#[cfg(feature = "recorder-dump")]
fn emit_flush_dump_line(trace: &Trace, batch: &PendingBatch, trigger: FlushTrigger) {
    crate::recorder::dump::record_flush(
        trace,
        batch,
        trigger,
        batch.calls.len(),
        batch.size_estimate,
    );
}

#[cfg(not(feature = "recorder-dump"))]
fn emit_flush_dump_line(_trace: &Trace, _batch: &PendingBatch, _trigger: FlushTrigger) {}

// --- BootObserver dispatcher ----------------------------------------------

/// Top-level observer registered with `ModuleBuilder::fcall_observer`.
/// Picks exactly one variant at `MINIT` based on `Config::global()`:
/// `Disabled` when the master switch is off (or `Config::global()`
/// hasn't populated yet), `Recorder` when the extension is enabled.
///
/// The `match self` in each trait method compiles down to a single
/// discriminant load — essentially free per call after LLVM has
/// inlined the variants' impls.
pub enum BootObserver {
    Disabled,
    Recorder(Recorder),
}

impl FcallObserver for BootObserver {
    fn should_observe(&self, info: &FcallInfo) -> bool {
        match self {
            Self::Disabled => false,
            Self::Recorder(r) => r.should_observe(info),
        }
    }

    fn begin(&self, execute_data: &ExecuteData) {
        match self {
            Self::Disabled => {}
            Self::Recorder(r) => r.begin(execute_data),
        }
    }

    fn end(&self, execute_data: &ExecuteData, retval: Option<&Zval>) {
        match self {
            Self::Disabled => {}
            Self::Recorder(r) => r.end(execute_data, retval),
        }
    }
}

/// Build the dispatcher from the resolved config. Called once at
/// `MINIT` by `lib.rs::get_module`'s `.fcall_observer(...)` chain.
///
/// `Config::global()` is `Some` at this point (the macro-expansion
/// order documented in `COMMENTS.md` C-5 makes our user `startup`
/// shim run before the observer factory). The `let-else` is the
/// defensive fallback: if a future ext-php-rs reorders startup, the
/// extension falls back to the inactive observer rather than
/// panicking across FFI.
pub fn build_boot_observer() -> BootObserver {
    let Some(config) = Config::global() else {
        return BootObserver::Disabled;
    };
    if !config.enabled {
        return BootObserver::Disabled;
    }
    BootObserver::Recorder(build_recorder_observer())
}

// --- Tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::types::{FunctionKey, FunctionKind, RequestIdentity};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    // --- Fixture helpers ---------------------------------------------------

    fn stub_identity() -> RequestIdentity {
        RequestIdentity {
            host: Arc::from("test-host"),
            sapi: Arc::from("cli"),
            pid: 1,
            uri_or_script: Arc::from("/tmp/test.php"),
        }
    }

    /// Slice-3 [`TraceLimits`] preset matching the directive-table
    /// defaults — uncapped for slice-2-style tests that don't care
    /// about the gates.
    fn permissive_limits() -> TraceLimits {
        TraceLimits {
            max_depth: 1024,
            buffer_cap_bytes: 64 * 1024 * 1024,
            // Phase-4 slice 2: slice-3 observer tests never cross the
            // flush thresholds. The flush-cadence tests below build
            // their own `TraceLimits` with explicit values.
            flush_records: usize::MAX,
            flush_bytes: usize::MAX,
        }
    }

    /// `Trace::new` shorthand for tests that want the slice-2 baseline
    /// behaviour (huge depth, huge cap). Tests that exercise the
    /// gates build a `TraceLimits` explicitly.
    fn fresh_trace() -> Trace {
        Trace::new(stub_identity(), permissive_limits())
    }

    /// Acquire the slice-3 accounting test-lock. Tests that touch the
    /// process-wide [`accounting::BYTES_IN_MEMORY`] atomic (either
    /// directly or via `push_record` / `push_dict_entry_via_intern`)
    /// hold this guard for their entire body.
    fn account_guard() -> std::sync::MutexGuard<'static, ()> {
        accounting::acquire_test_lock()
    }

    /// Build a [`RawCallSite`] from string literals. Centralises the
    /// boilerplate so the four-branch categorise tests stay
    /// readable. The `execute_data_addr` field is set to a stable
    /// per-test value so collision tests can drive it directly when
    /// they care, and `0` is fine when they don't.
    fn stub_site<'a>(
        function_name: Option<&'a str>,
        class_name: Option<&'a str>,
        filename: Option<&'a str>,
        lineno: u32,
        is_internal: bool,
    ) -> RawCallSite<'a> {
        RawCallSite {
            function_name: function_name.map(std::borrow::Cow::Borrowed),
            class_name: class_name.map(std::borrow::Cow::Borrowed),
            filename: filename.map(std::borrow::Cow::Borrowed),
            lineno,
            is_internal,
            execute_data_addr: 0,
        }
    }

    /// An empty `FcallInfo` for `should_observe` smoke tests. The
    /// observer trait still takes the upstream type, so we keep one
    /// constructor close to the tests that need it.
    fn empty_fcall_info() -> FcallInfo<'static> {
        FcallInfo {
            function_name: None,
            class_name: None,
            filename: None,
            lineno: 0,
            is_internal: false,
        }
    }

    fn entry_snapshots() -> EntrySnapshots {
        EntrySnapshots {
            t_in_ns: 1_000_000,
            cpu_u_in_ns: 500,
            cpu_s_in_ns: 100,
            mem_in_bytes: 1_024,
        }
    }

    fn exit_snapshots() -> ExitSnapshots {
        ExitSnapshots {
            t_out_ns: 2_000_000,
            cpu_u_now_ns: 1_500,
            cpu_s_now_ns: 300,
            mem_out_bytes: 2_048,
        }
    }

    /// Helper that owns the slot reset between each test that
    /// touches `CURRENT_TRACE`. The cell is thread-local, and tests
    /// run on the same thread when invoked sequentially — but
    /// `cargo test` parallelises across threads, so each test that
    /// enters the slot must also exit it. Using a guard struct
    /// makes the unwind path (panic in a test body) reset the slot
    /// too.
    struct TraceGuard;

    impl TraceGuard {
        fn enter(identity: RequestIdentity) -> Self {
            rinit_allocate_trace(identity, permissive_limits());
            Self
        }

        /// Slice-3 variant: enter with explicit limits when the test
        /// is exercising the depth or cap gate.
        #[allow(dead_code)]
        fn enter_with_limits(identity: RequestIdentity, limits: TraceLimits) -> Self {
            rinit_allocate_trace(identity, limits);
            Self
        }
    }

    impl Drop for TraceGuard {
        fn drop(&mut self) {
            rshutdown_release_trace();
        }
    }

    // --- Thread-local lifecycle -------------------------------------------

    #[test]
    fn rinit_allocate_trace_populates_the_slot() {
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let _g = TraceGuard::enter(stub_identity());
        let pid = with_current_trace(|trace| trace.pid).expect("slot must be Some after RINIT");
        assert_eq!(pid, 1);
    }

    #[test]
    fn rshutdown_release_trace_drops_the_slot() {
        let _account_guard = account_guard();
        accounting::reset_for_test();
        rinit_allocate_trace(stub_identity(), permissive_limits());
        assert!(with_current_trace(|_| ()).is_some());
        rshutdown_release_trace();
        assert!(
            with_current_trace(|_| ()).is_none(),
            "slot must be None after RSHUTDOWN",
        );
    }

    #[test]
    fn rshutdown_release_trace_on_empty_slot_is_a_noop() {
        let _account_guard = account_guard();
        accounting::reset_for_test();
        // Ensure the slot is empty (a previous test may have left it
        // populated; the guard's Drop handles that, but be defensive).
        rshutdown_release_trace();
        rshutdown_release_trace();
        assert!(with_current_trace(|_| ()).is_none());
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "RINIT without RSHUTDOWN")]
    fn double_rinit_without_rshutdown_panics_in_debug_builds() {
        let _account_guard = account_guard();
        accounting::reset_for_test();
        // Debug-only invariant: the pairing failure surfaces as a
        // `debug_assert!` panic so test runs and developer rebuilds
        // catch it loudly. Release builds take the silent-recovery
        // path covered by the next test (RO-1).
        rinit_allocate_trace(stub_identity(), permissive_limits());
        rinit_allocate_trace(stub_identity(), permissive_limits());
        // Defensive cleanup if `should_panic` somehow didn't match:
        rshutdown_release_trace();
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn double_rinit_without_rshutdown_replaces_the_stale_trace_in_release_builds() {
        let _account_guard = account_guard();
        accounting::reset_for_test();
        // Release-path RO-1 invariant: a `RINIT` on top of a
        // populated slot does NOT panic across the FFI boundary;
        // instead the stale `Trace` is dropped and a fresh one
        // takes its place. The first `Trace` carries `pid = 1`; the
        // second carries `pid = 2`. After the double-rinit the slot
        // must hold the second.
        let first = RequestIdentity {
            pid: 1,
            ..stub_identity()
        };
        let second = RequestIdentity {
            pid: 2,
            ..stub_identity()
        };
        rinit_allocate_trace(first, permissive_limits());
        rinit_allocate_trace(second, permissive_limits());
        let pid = with_current_trace(|trace| trace.pid)
            .expect("slot holds the recovery Trace after double rinit");
        assert_eq!(pid, 2, "release path must replace the stale Trace");
        rshutdown_release_trace();
    }

    #[test]
    fn with_current_trace_returns_none_when_slot_is_empty() {
        let _account_guard = account_guard();
        accounting::reset_for_test();
        rshutdown_release_trace();
        assert!(with_current_trace(|_| 42).is_none());
    }

    // --- categorise (four branches) ----------------------------------------

    #[test]
    fn categorise_routes_methods_to_the_method_branch() {
        let info = stub_site(
            Some("greet"),
            Some("Greeter"),
            Some("/srv/app.php"),
            7,
            false,
        );
        let cat = categorise(&info);
        assert_eq!(cat.kind, FunctionKind::Method);
        assert_eq!(
            cat.key,
            FunctionKey::Method {
                class: Arc::from("Greeter"),
                method: Arc::from("greet"),
            }
        );
        assert_eq!(cat.fqn.as_ref(), "Greeter::greet");
        assert_eq!(cat.file, "/srv/app.php");
        assert_eq!(cat.line, 7);
    }

    #[test]
    fn categorise_routes_user_functions_to_the_function_branch() {
        let info = stub_site(Some("my_fn"), None, Some("/x.php"), 20, false);
        let cat = categorise(&info);
        assert_eq!(cat.kind, FunctionKind::Function);
        assert_eq!(
            cat.key,
            FunctionKey::Function {
                file: Arc::from("/x.php"),
                function: Arc::from("my_fn"),
                line: 20,
            }
        );
        assert_eq!(cat.fqn.as_ref(), "my_fn");
        assert_eq!(cat.file, "/x.php");
        assert_eq!(cat.line, 20);
    }

    #[test]
    fn categorise_routes_closures_via_function_name_prefix() {
        let info = stub_site(
            Some("{closure:/srv/app.php:42}"),
            None,
            Some("/srv/app.php"),
            42,
            false,
        );
        let cat = categorise(&info);
        assert_eq!(cat.kind, FunctionKind::Closure);
        assert_eq!(
            cat.key,
            FunctionKey::Closure {
                file: Arc::from("/srv/app.php"),
                line: 42,
            }
        );
        assert_eq!(cat.fqn.as_ref(), "closure:/srv/app.php:42");
    }

    #[test]
    fn categorise_routes_closures_when_function_name_is_absent() {
        // PHP-8.x sometimes reports the closure entry with
        // `function_name = None` and `filename = Some(...)`. The spike
        // handles this branch; the recorder must match.
        let info = stub_site(None, None, Some("/x.php"), 1, false);
        let cat = categorise(&info);
        assert_eq!(cat.kind, FunctionKind::Closure);
        assert_eq!(
            cat.key,
            FunctionKey::Closure {
                file: Arc::from("/x.php"),
                line: 1,
            }
        );
        assert_eq!(cat.fqn.as_ref(), "closure:/x.php:1");
    }

    #[test]
    fn categorise_routes_internals_to_the_internal_branch() {
        let info = stub_site(Some("array_map"), None, None, 0, true);
        let cat = categorise(&info);
        assert_eq!(cat.kind, FunctionKind::Internal);
        assert_eq!(
            cat.key,
            FunctionKey::Internal {
                name: Arc::from("array_map"),
            }
        );
        assert_eq!(cat.fqn.as_ref(), "array_map");
        // File/line are blanked for internals — they have no source
        // location.
        assert_eq!(cat.file, "");
        assert_eq!(cat.line, 0);
    }

    #[test]
    fn categorise_handles_missing_line_and_missing_file_gracefully() {
        // Defensive: if Zend ever surfaces a user function without
        // `file`, the categorisation falls through to the closure
        // branch (per the substring rules). That's the same shape
        // the spike documents in C-5.
        let info = stub_site(Some("(unknown)"), None, None, 0, false);
        let cat = categorise(&info);
        // No file means we end up in the function branch (the
        // closure branch requires file=Some when function_name is
        // None, and a closure name pattern when function_name is
        // Some). This particular shape (Some("(unknown)") with no
        // file) hits the function fall-through.
        assert_eq!(cat.kind, FunctionKind::Function);
        assert_eq!(cat.file, "");
        assert_eq!(cat.line, 0);
    }

    // --- RO-4: lossy UTF-8 decode and RO-5: call-site tiebreaker ----------

    #[test]
    fn zend_string_to_cow_replaces_invalid_utf8_bytes_with_replacement_char() {
        // Build a fake `zend_string` with a payload of `[0xFF, 0xFF]`
        // (never valid UTF-8) and assert the helper returns
        // `Cow::Owned(...)` containing the U+FFFD replacement
        // character, rather than `None` (which would silently drop
        // the field). The previous helper, `zend_string_to_str`,
        // returned `None` here; that caused per-call collisions
        // documented in the RO-4 review note.
        let payload = b"\xFF\xFF";
        let bytes = make_zend_string(payload);
        let zs = bytes.as_ptr() as *mut ffi::zend_string;

        // SAFETY: the buffer outlives the call to `zend_string_to_cow`;
        // the layout matches `zend_string` for `len + val` (the
        // refcount/h/flags prefix is zeroed and not read by the
        // helper).
        let cow =
            unsafe { zend_string_to_cow::<'_>(zs) }.expect("non-null pointer must produce Some(_)");
        assert!(
            cow.contains('\u{FFFD}'),
            "lossy decode must substitute U+FFFD for invalid bytes; got {cow:?}",
        );
        // The decoded form is owned, not borrowed (lossy decoding
        // always allocates).
        assert!(
            matches!(cow, std::borrow::Cow::Owned(_)),
            "non-UTF-8 input must produce Cow::Owned; got {cow:?}",
        );
    }

    #[test]
    fn zend_string_to_cow_returns_a_zero_copy_borrow_for_valid_utf8() {
        // The common case (parser-validated PHP identifiers and
        // UTF-8 paths) must stay zero-copy — the hot path budget
        // depends on it.
        let payload = b"my_fn";
        let bytes = make_zend_string(payload);
        let zs = bytes.as_ptr() as *mut ffi::zend_string;
        let cow = unsafe { zend_string_to_cow::<'_>(zs) }.expect("non-null");
        assert_eq!(cow, "my_fn");
        assert!(
            matches!(cow, std::borrow::Cow::Borrowed(_)),
            "valid UTF-8 must be borrowed, not allocated; got {cow:?}",
        );
    }

    #[test]
    fn categorise_unknown_fallback_uses_execute_data_addr_as_tiebreaker() {
        // RO-5: two distinct unknown-shaped call sites must NOT
        // collapse to the same FunctionKey. Before the fix, every
        // unknown collapsed to the literal `(unknown)` /
        // `(anonymous)` placeholder; the dictionary then folded
        // unrelated call sites into a single per-call counter.
        let mut a = stub_site(None, None, None, 0, false);
        a.execute_data_addr = 0x1000;
        let mut b = stub_site(None, None, None, 0, false);
        b.execute_data_addr = 0x2000;

        // Both should hit the function fall-through (None
        // function_name with no file → not closure → not method →
        // not internal). The synthesised names must differ.
        let ca = categorise(&a);
        let cb = categorise(&b);
        assert_ne!(
            ca.fqn, cb.fqn,
            "distinct call sites must produce distinct fqns; got {} == {}",
            ca.fqn, cb.fqn,
        );
        assert_ne!(
            ca.key, cb.key,
            "distinct call sites must produce distinct keys",
        );
        // The synthesised name includes the address marker so
        // operators recognise the fallback.
        assert!(
            ca.fqn.contains("@0x"),
            "fallback name must surface the address tiebreaker; got {}",
            ca.fqn,
        );
    }

    #[test]
    fn categorise_internal_with_no_name_uses_execute_data_addr_tiebreaker() {
        // Same RO-5 invariant on the internal-function branch.
        // Internal calls with `function_name = None` are rare (Zend
        // normally fills the name), but the tiebreaker must hold
        // for any path that goes through the fallback.
        let mut a = stub_site(None, None, None, 0, true);
        a.execute_data_addr = 0x1000;
        let mut b = stub_site(None, None, None, 0, true);
        b.execute_data_addr = 0x2000;
        let ca = categorise(&a);
        let cb = categorise(&b);
        assert_ne!(ca.key, cb.key);
        assert!(ca.fqn.contains("(anonymous)@0x"));
    }

    /// Build a heap-allocated `zend_string`-shaped byte buffer with
    /// the given payload. The layout matches `ffi::zend_string`'s
    /// declaration: `gc + h + len + val[len + 1]` with `val` placed
    /// at the correct offset for the `*(zs).val.as_ptr()` reads.
    ///
    /// Used only by the lossy-decode tests above. Keeping it inside
    /// `mod tests` avoids any chance of the helper leaking into
    /// production binaries.
    fn make_zend_string(payload: &[u8]) -> Vec<u8> {
        use std::mem::{align_of, size_of};

        let zs_size = size_of::<ffi::zend_string>();
        let zs_align = align_of::<ffi::zend_string>();
        // `val` is a flexible array `[c_char; 1]` at the tail of
        // `zend_string`; the layout already includes one byte. We
        // need `payload.len()` plus the NUL terminator past the
        // declared `[c_char; 1]` slot, so the trailing extension is
        // `payload.len()` extra bytes (the `+1` for NUL is offset
        // by the declared one-byte slot).
        let extra = payload.len();
        let total = zs_size + extra;
        // Allocate with the right alignment; `Vec<u8>` does not
        // guarantee `zend_string` alignment, but the system allocator
        // typically returns 16-byte alignment for any allocation and
        // `zend_string`'s alignment requirement is `align_of::<usize>()`
        // (8 on x86_64). We assert just in case.
        let mut buf = vec![0u8; total];
        assert!(
            (buf.as_ptr() as usize) % zs_align == 0,
            "Vec<u8>'s default alignment must satisfy zend_string's; \
             rerun with `Box::into_raw(vec![..].into_boxed_slice())` if this trips",
        );
        // SAFETY: `buf` is sized as `zend_string` + extra. The
        // initial zeroing covers `gc`, `h`, and the leading
        // refcount/flags bits the helper does not read.
        unsafe {
            let zs_ptr = buf.as_mut_ptr().cast::<ffi::zend_string>();
            (*zs_ptr).len = payload.len();
            // `val` is the flexible-array tail; write into the byte
            // offset immediately after the declared one-byte slot.
            let val_ptr = std::ptr::addr_of_mut!((*zs_ptr).val) as *mut u8;
            std::ptr::copy_nonoverlapping(payload.as_ptr(), val_ptr, payload.len());
        }
        buf
    }

    // --- begin_with_snapshots / end_with_snapshots ------------------------

    #[test]
    fn begin_with_snapshots_pushes_one_frame_with_call_id_one_and_parent_zero() {
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let mut trace = fresh_trace();
        let info = stub_site(Some("only_me"), None, Some("/x.php"), 3, false);
        let cat = categorise(&info);
        begin_with_snapshots(&mut trace, &cat, entry_snapshots());

        assert_eq!(trace.stack.len(), 1);
        let frame = trace.stack[0];
        assert_eq!(frame.call_id, 1);
        assert_eq!(frame.parent, 0);
        assert_eq!(frame.depth, 0);
        assert_eq!(frame.t_in_ns, 1_000_000);
        // The dictionary staged exactly one entry for the new function.
        let entries = trace.dictionary.take_new_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].fqn, "only_me");
        assert_eq!(frame.fn_id, entries[0].fn_id);
    }

    #[test]
    fn begin_then_end_emits_one_callrecord_with_matching_fields() {
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let mut trace = fresh_trace();
        let info = stub_site(Some("only_me"), None, Some("/x.php"), 3, false);
        let cat = categorise(&info);

        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        end_with_snapshots(&mut trace, exit_snapshots(), false);

        assert!(
            trace.stack.is_empty(),
            "end must pop the frame matched by begin",
        );
        assert_eq!(trace.buffer.len(), 1, "exactly one record emitted");
        let r = &trace.buffer[0];
        assert_eq!(r.call_id, 1);
        assert_eq!(r.parent, 0);
        assert_eq!(r.depth, 0);
        assert_eq!(r.t_in_ns, 1_000_000);
        assert_eq!(r.t_out_ns, 2_000_000);
        assert_eq!(r.cpu_u_ns, 1_000); // 1_500 − 500
        assert_eq!(r.cpu_s_ns, 200); // 300 − 100
        assert_eq!(r.mem_in_bytes, 1_024);
        assert_eq!(r.mem_out_bytes, 2_048);
        assert!(!r.abnormal_exit);
    }

    #[test]
    fn nested_calls_produce_chained_parent_pointers() {
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let mut trace = fresh_trace();

        // `info_*` bindings must outlive the `Categorised` values
        // returned by `categorise` (the categorisation borrows
        // `fqn`/`file` from the `RawCallSite`).
        let info_a = stub_site(Some("a"), None, Some("/x.php"), 1, false);
        let info_b = stub_site(Some("b"), None, Some("/x.php"), 2, false);
        let info_c = stub_site(Some("c"), None, Some("/x.php"), 3, false);
        let a = categorise(&info_a);
        let b = categorise(&info_b);
        let c = categorise(&info_c);

        begin_with_snapshots(&mut trace, &a, entry_snapshots());
        begin_with_snapshots(&mut trace, &b, entry_snapshots());
        begin_with_snapshots(&mut trace, &c, entry_snapshots());

        // Stack: [a (call_id 1, parent 0), b (2, parent 1), c (3, parent 2)]
        assert_eq!(trace.stack.len(), 3);
        assert_eq!(trace.stack[2].call_id, 3);
        assert_eq!(trace.stack[2].parent, 2);
        assert_eq!(trace.stack[2].depth, 2);

        // Pop in reverse (LIFO): c, then b, then a.
        end_with_snapshots(&mut trace, exit_snapshots(), false);
        end_with_snapshots(&mut trace, exit_snapshots(), false);
        end_with_snapshots(&mut trace, exit_snapshots(), false);

        assert!(trace.stack.is_empty());
        assert_eq!(trace.buffer.len(), 3);
        let pairs: Vec<(u64, u64)> = trace.buffer.iter().map(|r| (r.call_id, r.parent)).collect();
        // Emission order is end-handler order: c first (innermost),
        // then b, then a.
        assert_eq!(pairs, vec![(3, 2), (2, 1), (1, 0)]);
    }

    #[test]
    fn dict_miss_allocates_once_dict_hit_allocates_zero_strings() {
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let mut trace = fresh_trace();
        let info = stub_site(Some("repeat"), None, Some("/x.php"), 1, false);
        let cat = categorise(&info);

        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        let first_entries = trace.dictionary.take_new_entries();
        assert_eq!(
            first_entries.len(),
            1,
            "first miss stages exactly one entry"
        );

        // Pop the first frame so the second begin's parent is 0 again
        // (and the buffer accounting stays clean).
        end_with_snapshots(&mut trace, exit_snapshots(), false);

        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        let second_entries = trace.dictionary.take_new_entries();
        assert!(
            second_entries.is_empty(),
            "hit must not stage a new dictionary entry; got {second_entries:?}",
        );
    }

    #[test]
    fn end_with_abnormal_true_writes_abnormal_exit_true() {
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let mut trace = fresh_trace();
        let info = stub_site(Some("bad"), None, Some("/x.php"), 1, false);
        let cat = categorise(&info);

        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        end_with_snapshots(&mut trace, exit_snapshots(), true);

        assert_eq!(trace.buffer.len(), 1);
        assert!(trace.buffer[0].abnormal_exit);
    }

    #[test]
    fn saturating_cpu_delta_reads_as_zero_when_exit_cpu_less_than_entry_cpu() {
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let mut trace = fresh_trace();
        let info = stub_site(Some("anywhere"), None, Some("/x.php"), 1, false);
        let cat = categorise(&info);

        // Entry CPU times are higher than the exit's — `saturating_sub`
        // must read as 0, never a negative number. This models the
        // thread-migration scenario described in spec D-7.
        let high_entry = EntrySnapshots {
            t_in_ns: 1_000,
            cpu_u_in_ns: 10_000,
            cpu_s_in_ns: 5_000,
            mem_in_bytes: 0,
        };
        let low_exit = ExitSnapshots {
            t_out_ns: 2_000,
            cpu_u_now_ns: 1_000,
            cpu_s_now_ns: 500,
            mem_out_bytes: 0,
        };

        begin_with_snapshots(&mut trace, &cat, high_entry);
        end_with_snapshots(&mut trace, low_exit, false);

        let r = &trace.buffer[0];
        assert_eq!(r.cpu_u_ns, 0, "saturating_sub clamps to 0");
        assert_eq!(r.cpu_s_ns, 0, "saturating_sub clamps to 0");
    }

    #[test]
    fn finish_call_record_with_no_frame_is_a_silent_noop() {
        // RO-3: the release-path "empty-stack → silent no-op"
        // contract is exercised through `finish_call_record(None)`
        // so a default `cargo test` (debug-assertions on) actually
        // runs it, instead of vacuously returning past a
        // `cfg!(debug_assertions)` early-return. The
        // `end_with_snapshots` caller's `debug_assert!` is the loud
        // signal for the pairing bug in test/dev builds; this
        // helper is the recovery path the assert documents, and
        // both should be tested.
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let mut trace = fresh_trace();
        finish_call_record(&mut trace, None, exit_snapshots(), false);
        assert!(
            trace.buffer.is_empty(),
            "no popped frame must not emit a record",
        );
        assert!(trace.stack.is_empty(), "stack must remain empty");
    }

    #[test]
    fn finish_call_record_with_a_frame_emits_a_record_with_the_frame_fields() {
        // Companion to the silent-noop test: prove `finish_call_record`
        // does the real work when handed `Some(frame)`. Together the
        // two tests pin both arms of the helper without depending on
        // `cfg(debug_assertions)`.
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let mut trace = fresh_trace();
        let frame = CallFrame {
            call_id: 7,
            parent: 0,
            fn_id: 3,
            depth: 0,
            t_in_ns: 1_000_000,
            cpu_u_in_ns: 500,
            cpu_s_in_ns: 100,
            mem_in_bytes: 1_024,
        };
        finish_call_record(&mut trace, Some(frame), exit_snapshots(), true);
        assert_eq!(trace.buffer.len(), 1);
        let r = &trace.buffer[0];
        assert_eq!(r.call_id, 7);
        assert_eq!(r.fn_id, 3);
        assert_eq!(r.cpu_u_ns, 1_000);
        assert_eq!(r.cpu_s_ns, 200);
        assert!(r.abnormal_exit);
    }

    #[test]
    fn recorder_begin_with_no_active_trace_is_a_noop() {
        // The thread-local is empty; `with_current_trace` returns
        // None. The handler should not panic, should not allocate,
        // and should leave the slot empty.
        //
        // RO-6 follow-up: the snapshot trio (monotonic clock, CPU
        // times, memory-real) is now captured **inside** the
        // `with_current_trace` closure, so a slot-empty fire pays
        // for the `RefCell::borrow_mut` and the
        // `Option::as_mut().map(_)` only — no `clock_gettime`, no
        // `getrusage`, no `zend_memory_usage`. We do not have a
        // direct way to assert "no syscall" without a mock clock,
        // so this test pins the structural smoke: the closure body
        // is never entered when the slot is empty.
        let _account_guard = account_guard();
        accounting::reset_for_test();
        rshutdown_release_trace(); // ensure empty
        let r = Recorder;
        assert!(r.should_observe(&empty_fcall_info()));
        let touched = with_current_trace(|_| true);
        assert!(touched.is_none(), "no active trace → no-op closure body");
    }

    // --- Slice-3 depth gate (max_depth) -----------------------------------

    /// Build a `Categorised<'static>` from a stub `RawCallSite` so the
    /// slice-3 gate tests stay readable. The returned `Categorised`
    /// borrows from the leaked `RawCallSite` to satisfy the `'a`
    /// lifetime; the leak is fine for a test.
    fn cat_for(name: &'static str) -> Categorised<'static> {
        let site = Box::leak(Box::new(stub_site(
            Some(name),
            None,
            Some("/x.php"),
            1,
            false,
        )));
        categorise(site)
    }

    /// Build a `Trace` with a tight depth ceiling and a comfortable
    /// budget. Slice-3 depth-gate tests use this.
    fn trace_with_max_depth(max_depth: u32) -> Trace {
        Trace::new(
            stub_identity(),
            TraceLimits {
                max_depth,
                buffer_cap_bytes: 64 * 1024 * 1024,
                flush_records: usize::MAX,
                flush_bytes: usize::MAX,
            },
        )
    }

    /// Build a `Trace` with a comfortable depth but a tight byte budget.
    /// Slice-3 cap-gate tests use this.
    fn trace_with_cap(buffer_cap_bytes: usize) -> Trace {
        Trace::new(
            stub_identity(),
            TraceLimits {
                max_depth: 1024,
                buffer_cap_bytes,
                flush_records: usize::MAX,
                flush_bytes: usize::MAX,
            },
        )
    }

    #[test]
    fn begin_at_exactly_max_depth_is_accepted() {
        let _guard = account_guard();
        accounting::reset_for_test();
        let mut trace = trace_with_max_depth(5);
        let cat = cat_for("ok");

        for _ in 0..5 {
            begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        }

        assert_eq!(trace.stack.len(), 5, "five accepted frames");
        assert_eq!(trace.virtual_depth, 5);
        assert_eq!(trace.dropped_begins, 0);
        assert_eq!(trace.drop_counter.load(Ordering::Acquire), 0);
    }

    #[test]
    fn begin_at_max_depth_plus_one_is_dropped_and_bumps_counter() {
        let _guard = account_guard();
        accounting::reset_for_test();
        let mut trace = trace_with_max_depth(5);
        let cat = cat_for("recurse");

        for _ in 0..6 {
            begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        }

        assert_eq!(trace.stack.len(), 5, "sixth begin must not push");
        assert_eq!(trace.virtual_depth, 6, "virtual depth tracks PHP-side");
        assert_eq!(trace.dropped_begins, 1);
        assert_eq!(trace.drop_counter.load(Ordering::Acquire), 1);
    }

    #[test]
    fn dropped_begin_does_not_touch_bytes_in_memory() {
        let _guard = account_guard();
        accounting::reset_for_test();
        let mut trace = trace_with_max_depth(1);
        let cat = cat_for("over");

        // First begin is at depth 1 (accepted).
        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        let snapshot_after_accept = accounting::snapshot();
        assert!(
            snapshot_after_accept > 0,
            "the accepted begin must bill the dict-miss bytes",
        );

        // Second begin is at depth 2 (dropped). The atomic must not
        // change.
        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        assert_eq!(
            accounting::snapshot(),
            snapshot_after_accept,
            "depth-dropped begin must not touch the atomic",
        );
    }

    #[test]
    fn dropped_begin_does_not_intern_a_dict_entry() {
        let _guard = account_guard();
        accounting::reset_for_test();
        let mut trace = trace_with_max_depth(1);
        let cat_first = cat_for("first");
        let cat_second = cat_for("second_dropped");

        begin_with_snapshots(&mut trace, &cat_first, entry_snapshots());
        let first_dict_len = trace.dictionary.take_new_entries().len();
        assert_eq!(first_dict_len, 1, "first begin staged one entry");

        // Second begin is dropped on depth. Even though `second_dropped`
        // is a fresh function name, the dictionary must NOT learn about
        // it.
        begin_with_snapshots(&mut trace, &cat_second, entry_snapshots());
        let second_dict_len = trace.dictionary.take_new_entries().len();
        assert_eq!(
            second_dict_len, 0,
            "dropped begin must not stage a dict entry",
        );
    }

    #[test]
    fn dropped_begin_does_not_push_a_call_frame() {
        let _guard = account_guard();
        accounting::reset_for_test();
        let mut trace = trace_with_max_depth(2);
        let cat = cat_for("any");

        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        let frames_after_two = trace.stack.len();

        // Third begin is past max_depth.
        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        assert_eq!(
            trace.stack.len(),
            frames_after_two,
            "dropped begin must leave the stack unchanged",
        );
    }

    // --- Slice-3 cap gate (buffer_cap_bytes) -------------------------------

    #[test]
    fn accept_below_cap_bills_atomic_by_dict_miss_cost_at_begin() {
        let _guard = account_guard();
        accounting::reset_for_test();
        // Huge cap so the gate never trips; we are pinning the
        // billing-split contract for the accept path.
        let mut trace = trace_with_cap(1_000_000);
        let cat = cat_for("billtest");

        // At begin time, the dict-miss cost is billed.
        begin_with_snapshots(&mut trace, &cat, entry_snapshots());

        let expected_dict_bytes =
            DICT_ENTRY_FIXED_BYTES + cat.fqn.as_ref().len() + cat.file.as_ref().len();
        assert_eq!(
            accounting::snapshot(),
            expected_dict_bytes,
            "begin must bill the dict-miss cost into the process-wide atomic",
        );
        // The record portion is not yet billed (push_record fires at
        // end time, see the next test).
    }

    #[test]
    fn accept_below_cap_bills_atomic_by_call_record_fixed_bytes_at_end() {
        let _guard = account_guard();
        accounting::reset_for_test();
        let mut trace = trace_with_cap(1_000_000);
        let cat = cat_for("billtest");

        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        let snapshot_after_begin = accounting::snapshot();

        end_with_snapshots(&mut trace, exit_snapshots(), false);

        assert_eq!(
            accounting::snapshot() - snapshot_after_begin,
            CALL_RECORD_FIXED_BYTES,
            "end must bill the CALL_RECORD_FIXED_BYTES contribution (slice-3 D-3)",
        );
    }

    #[test]
    fn begin_above_cap_is_dropped_and_bumps_counter() {
        let _guard = account_guard();
        accounting::reset_for_test();
        // Tight cap: below the §3.2 worst-case-per-call cost
        // (CALL_RECORD_FIXED_BYTES + DICT_ENTRY_FIXED_BYTES + any
        // miss-string bytes). The first begin must drop.
        let mut trace = trace_with_cap(8);
        let cat = cat_for("over_cap");

        begin_with_snapshots(&mut trace, &cat, entry_snapshots());

        assert_eq!(trace.stack.len(), 0, "cap-dropped begin pushed nothing");
        assert_eq!(trace.dropped_begins, 1);
        assert_eq!(trace.drop_counter.load(Ordering::Acquire), 1);
        assert_eq!(accounting::snapshot(), 0, "atomic untouched on drop");
    }

    #[test]
    fn repeated_call_after_miss_drop_remains_a_miss_until_accepted() {
        let _guard = account_guard();
        accounting::reset_for_test();
        let mut trace = trace_with_cap(8);
        let cat = cat_for("never_accepted");

        // Two cap-drops. Each begin re-projects would_add with the
        // miss-cost because the previous drop did not intern.
        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        begin_with_snapshots(&mut trace, &cat, entry_snapshots());

        assert_eq!(trace.dropped_begins, 2);
        assert_eq!(trace.drop_counter.load(Ordering::Acquire), 2);
        // The dictionary must still consider this key unseen.
        assert!(
            !trace.dictionary.contains_key(&cat.key),
            "two dropped begins must not have interned the key",
        );
    }

    #[test]
    fn cap_reset_via_reset_for_test_re_accepts_previously_dropped_call() {
        let _guard = account_guard();
        accounting::reset_for_test();
        let mut trace = trace_with_cap(8);
        let cat = cat_for("recover");

        // First begin drops on cap.
        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        assert_eq!(trace.dropped_begins, 1);

        // Drain the LIFO state so the subsequent end pairings stay
        // clean for the test's intent.
        trace.dropped_begins = 0;
        trace.virtual_depth = 0;

        // Now raise the cap (test-only: rebuild a trace with a roomy
        // cap and confirm the same `cat` is accepted).
        accounting::reset_for_test();
        let mut roomy = trace_with_cap(1_000_000);
        begin_with_snapshots(&mut roomy, &cat, entry_snapshots());

        assert_eq!(
            roomy.stack.len(),
            1,
            "with a roomy cap the begin is accepted"
        );
        assert_eq!(roomy.dropped_begins, 0);
        assert_eq!(roomy.drop_counter.load(Ordering::Acquire), 0);
    }

    // --- Slice-3 end-side LIFO pairing ------------------------------------

    #[test]
    fn finish_after_depth_drop_decrements_counters_and_does_not_pop() {
        let _guard = account_guard();
        accounting::reset_for_test();
        let mut trace = trace_with_max_depth(1);
        let cat = cat_for("over");

        // First begin accepted (depth = 1), second dropped (would be 2).
        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        begin_with_snapshots(&mut trace, &cat, entry_snapshots());

        // End the dropped begin first (LIFO). The matcher must
        // consume the drop, leave the stack and buffer alone.
        end_with_snapshots(&mut trace, exit_snapshots(), false);

        assert_eq!(
            trace.stack.len(),
            1,
            "the accepted frame is still on the stack"
        );
        assert_eq!(
            trace.buffer.len(),
            0,
            "no record emitted for the dropped end"
        );
        assert_eq!(trace.dropped_begins, 0);
        assert_eq!(trace.virtual_depth, 1);
    }

    #[test]
    fn finish_after_cap_drop_decrements_counters_and_does_not_pop() {
        let _guard = account_guard();
        accounting::reset_for_test();
        let mut trace = trace_with_cap(8);
        let cat = cat_for("cap_drop");

        // Drop on cap.
        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        assert_eq!(trace.dropped_begins, 1);

        // The matching end must NOT pop and must NOT emit.
        end_with_snapshots(&mut trace, exit_snapshots(), false);

        assert_eq!(trace.stack.len(), 0);
        assert_eq!(trace.buffer.len(), 0);
        assert_eq!(trace.dropped_begins, 0);
        assert_eq!(trace.virtual_depth, 0);
    }

    #[test]
    fn lifo_pairing_accept_drop_accept_returns_two_records_in_pop_order() {
        let _guard = account_guard();
        accounting::reset_for_test();
        // Goal: drive a `begin(accept) → begin(drop) → begin(accept)`
        // sequence and confirm the three matching `end` calls pair
        // correctly in LIFO order.
        //
        // The depth gate is the cleanest trigger: set `max_depth = 2`
        // and recurse three deep. The third begin is at virtual_depth 3,
        // which is > max_depth ⇒ dropped. But that gives us
        // `accept, accept, drop` — not what we want.
        //
        // Instead, use a cap-gate scenario that drops the **second**
        // begin: size the cap so the first call's miss + record fits,
        // but the second call's *new-function* miss does not. The
        // third call reuses the first function (a dict hit, no miss
        // cost projected) so it fits.
        //
        // The two function-names must have the same length so the
        // cap-size arithmetic stays symmetric and easy to read.
        let cat_first = cat_for("first__"); // 7 chars
        let cat_droppy = cat_for("droppy_"); // 7 chars, distinct key
        let first_miss =
            DICT_ENTRY_FIXED_BYTES + cat_first.fqn.as_ref().len() + cat_first.file.as_ref().len();
        // Cap = first miss + record (room for first begin's would_add
        // and its end's record-bill, but not for a second miss).
        let cap = first_miss + CALL_RECORD_FIXED_BYTES;

        let mut trace = trace_with_cap(cap);

        // 1. First begin: accepted (dict miss billed to atomic).
        begin_with_snapshots(&mut trace, &cat_first, entry_snapshots());
        assert_eq!(trace.stack.len(), 1);
        assert_eq!(trace.dropped_begins, 0);

        // 2. Second begin (different function): cap-gate drops because
        //    `accounting::snapshot() + (CALL_RECORD_FIXED_BYTES +
        //    second_miss)` exceeds the cap.
        begin_with_snapshots(&mut trace, &cat_droppy, entry_snapshots());
        assert_eq!(trace.stack.len(), 1, "second begin dropped on cap");
        assert_eq!(trace.dropped_begins, 1);

        // 3. Third begin (same function as the first): dict hit means
        //    miss-cost = 0; would_add = CALL_RECORD_FIXED_BYTES, which
        //    still fits inside `cap`. Accepted.
        let cat_third = cat_for("first__");
        begin_with_snapshots(&mut trace, &cat_third, entry_snapshots());
        assert_eq!(trace.stack.len(), 2, "third begin accepted (dict hit)");
        assert_eq!(trace.dropped_begins, 1, "still one drop pending");
        assert_eq!(trace.virtual_depth, 3);

        // Pop in reverse: third (accept), second (LIFO drop consume),
        // first (accept).
        end_with_snapshots(&mut trace, exit_snapshots(), false);
        end_with_snapshots(&mut trace, exit_snapshots(), false);
        end_with_snapshots(&mut trace, exit_snapshots(), false);

        assert!(trace.stack.is_empty());
        assert_eq!(trace.buffer.len(), 2, "two records, one per accepted call");
        assert_eq!(trace.dropped_begins, 0);
        assert_eq!(trace.virtual_depth, 0);
        // Pop order: the third-accepted (call_id = 2) ends first, then
        // the LIFO consume (no record), then the first-accepted
        // (call_id = 1).
        assert_eq!(trace.buffer[0].call_id, 2);
        assert_eq!(trace.buffer[1].call_id, 1);
    }

    #[test]
    fn virtual_depth_returns_to_zero_after_balanced_begin_end_pairs() {
        let _guard = account_guard();
        accounting::reset_for_test();
        let mut trace = fresh_trace();
        let cat = cat_for("anywhere");

        for _ in 0..10 {
            begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        }
        for _ in 0..10 {
            end_with_snapshots(&mut trace, exit_snapshots(), false);
        }

        assert_eq!(trace.virtual_depth, 0);
        assert_eq!(trace.dropped_begins, 0);
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn virtual_depth_never_underflows_under_an_adversarial_end_then_begin_sequence() {
        // RO-1 / NFR-REL-1 defense: even if Zend ever delivered an
        // `end` event without a matching `begin` (a contract
        // violation), the recorder must not underflow `virtual_depth`
        // (an underflow wraps to `u32::MAX` and would silently
        // depth-drop the entire next request). `saturating_sub` is
        // the guard; this test pins it.
        //
        // Release-only: slice 2's `debug_assert!(popped.is_some(),
        // …)` in `end_with_snapshots` would panic on the empty stack
        // before our saturating_sub takes effect. Release builds
        // skip the debug-assert, and the saturating_sub becomes the
        // active defense. Same pattern as slice-2's RO-3
        // `double_rinit_without_rshutdown_replaces_the_stale_trace_in_release_builds`.
        let _guard = account_guard();
        accounting::reset_for_test();
        let mut trace = fresh_trace();

        // Three ends without prior begins. In release each `end`
        // saturating-subs virtual_depth and then `finish_call_record`
        // gets a `None` pop, returning silently.
        for _ in 0..3 {
            end_with_snapshots(&mut trace, exit_snapshots(), false);
        }
        assert_eq!(
            trace.virtual_depth, 0,
            "saturating_sub keeps the floor at zero"
        );
        assert_eq!(trace.dropped_begins, 0, "no drops to consume");

        // Now begin/end normally — the counter must behave as if the
        // adversarial ends never happened.
        let cat = cat_for("normal");
        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        end_with_snapshots(&mut trace, exit_snapshots(), false);
        assert_eq!(trace.virtual_depth, 0);
        assert_eq!(trace.buffer.len(), 1);
    }

    #[test]
    fn virtual_depth_returns_to_zero_after_balanced_lifo_consume_sequence() {
        // Debug-build companion to the above: drive 4 begins under a
        // depth-zero gate (all drop) followed by 4 ends (all LIFO
        // consume). The saturating_sub never goes below zero because
        // every end's decrement is matched by a prior begin's
        // increment. This pins the LIFO branch's invariant without
        // tripping the slice-2 debug_assert.
        let _guard = account_guard();
        accounting::reset_for_test();
        // max_depth = 0 ⇒ every begin trips the depth gate.
        let mut trace = trace_with_max_depth(0);
        let cat = cat_for("always_dropped");

        for _ in 0..4 {
            begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        }
        assert_eq!(trace.virtual_depth, 4);
        assert_eq!(trace.dropped_begins, 4);
        assert_eq!(trace.drop_counter.load(Ordering::Acquire), 4);

        for _ in 0..4 {
            end_with_snapshots(&mut trace, exit_snapshots(), false);
        }
        assert_eq!(trace.virtual_depth, 0);
        assert_eq!(trace.dropped_begins, 0);
        assert!(trace.buffer.is_empty(), "no records from dropped begins");
    }

    // --- Slice-3 RSHUTDOWN subtract ---------------------------------------

    #[test]
    fn rshutdown_returns_atomic_to_zero_after_balanced_trace() {
        // Phase-4 slice 2 update: `rshutdown_release_trace` now hands
        // the non-empty buffer to the shipper before subtracting. The
        // shipper-side consume subtract (Section 6) is what returns
        // the bytes to the budget; until that lands we emulate the
        // consume inline so the slice-3 invariant ("RSHUTDOWN returns
        // the atomic to zero") survives the producer-wiring change.
        let _shipper_guard = crate::shipper::acquire_test_lock();
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let rx = install_test_channel(8);

        rinit_allocate_trace(stub_identity(), permissive_limits());
        with_current_trace(|trace| {
            let cat_a = cat_for("rshut_a");
            let cat_b = cat_for("rshut_b");
            begin_with_snapshots(trace, &cat_a, entry_snapshots());
            end_with_snapshots(trace, exit_snapshots(), false);
            begin_with_snapshots(trace, &cat_b, entry_snapshots());
            end_with_snapshots(trace, exit_snapshots(), false);
            assert!(
                accounting::snapshot() > 0,
                "accepted calls must bill the atomic before rshutdown",
            );
        });

        rshutdown_release_trace();

        // Emulate the Section-6 shipper consume subtract by draining
        // every queued batch and subtracting its `size_estimate`.
        for batch in drain_queued_batches(&rx) {
            accounting::sub(batch.size_estimate);
        }

        assert_eq!(
            accounting::snapshot(),
            0,
            "rshutdown_release_trace + shipper consume must balance the atomic",
        );
        crate::shipper::reset_for_test();
    }

    #[test]
    fn rshutdown_on_empty_slot_does_not_touch_atomic() {
        let _guard = account_guard();
        accounting::reset_for_test();
        // Ensure the slot is empty.
        rshutdown_release_trace();
        // A subsequent rshutdown is the case the test cares about: it
        // must be a no-op, not an underflow.
        rshutdown_release_trace();
        assert_eq!(accounting::snapshot(), 0);
    }

    #[test]
    fn two_consecutive_request_cycles_keep_zero_balance_invariant() {
        // Phase-4 slice 2: each RSHUTDOWN now flushes via try_send;
        // we emulate the Section-6 shipper subtract by draining the
        // channel between cycles so the slice-3 invariant survives.
        let _shipper_guard = crate::shipper::acquire_test_lock();
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let rx = install_test_channel(8);

        for _ in 0..2 {
            rinit_allocate_trace(stub_identity(), permissive_limits());
            with_current_trace(|trace| {
                let cat = cat_for("repeat");
                begin_with_snapshots(trace, &cat, entry_snapshots());
                end_with_snapshots(trace, exit_snapshots(), false);
            });
            rshutdown_release_trace();
            // Emulate the shipper consume so the atomic balances.
            for batch in drain_queued_batches(&rx) {
                accounting::sub(batch.size_estimate);
            }
            assert_eq!(
                accounting::snapshot(),
                0,
                "atomic returns to zero between requests",
            );
        }
        crate::shipper::reset_for_test();
    }

    #[test]
    fn dropped_begins_returns_to_zero_after_balanced_begin_end_pairs_through_drops() {
        let _guard = account_guard();
        accounting::reset_for_test();
        let mut trace = trace_with_max_depth(3);
        let cat = cat_for("recurse");

        // 10 begins, 10 ends. Only the first 3 are accepted; the rest
        // are dropped on depth.
        for _ in 0..10 {
            begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        }
        for _ in 0..10 {
            end_with_snapshots(&mut trace, exit_snapshots(), false);
        }

        assert_eq!(trace.virtual_depth, 0);
        assert_eq!(trace.dropped_begins, 0);
        assert_eq!(trace.drop_counter.load(Ordering::Acquire), 7);
        assert!(trace.stack.is_empty());
        assert_eq!(trace.buffer.len(), 3, "only the three accepted calls emit");
    }

    // --- Phase-4 slice 2: threshold-driven flush --------------------------

    /// Build a `Trace` with the four cap / depth knobs set permissively
    /// but the two flush thresholds set explicitly. The slice-2
    /// flush-cadence tests use this rather than `permissive_limits`
    /// (which sets both flush thresholds to `usize::MAX`).
    fn trace_with_flush_thresholds(flush_records: usize, flush_bytes: usize) -> Trace {
        Trace::new(
            stub_identity(),
            TraceLimits {
                max_depth: 1024,
                buffer_cap_bytes: 64 * 1024 * 1024,
                flush_records,
                flush_bytes,
            },
        )
    }

    /// Install a Sender + Receiver pair into the shipper's canonical
    /// slot so `try_send_batch` on the recorder's accept tail has a
    /// real channel to land on. Returns the Receiver so the test
    /// asserts the cadence by counting `ShipperMessage::Batch(_)`
    /// drains.
    fn install_test_channel(
        depth: usize,
    ) -> crossbeam_channel::Receiver<crate::recorder::types::ShipperMessage> {
        crate::shipper::reset_for_test();
        let (tx, rx) = crossbeam_channel::bounded(depth);
        crate::shipper::install_test_sender(tx);
        rx
    }

    /// Drain every queued `Batch` from `rx` and return them as a vec.
    /// Used to assert the number and contents of batches a test
    /// emitted. The receiver is left in place so a follow-up
    /// `try_recv` confirms emptiness.
    fn drain_queued_batches(
        rx: &crossbeam_channel::Receiver<crate::recorder::types::ShipperMessage>,
    ) -> Vec<PendingBatch> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            match msg {
                crate::recorder::types::ShipperMessage::Batch(b) => out.push(b),
                crate::recorder::types::ShipperMessage::Drain { .. } => {
                    panic!("unexpected Drain in flush cadence test");
                }
            }
        }
        out
    }

    // --- Focused tests for `flush_predicate_trigger` ----------------------
    //
    // PF-5 (`COMMENTS.md`): the end-to-end flush tests below assert
    // `batches.len() == 1` but don't pin which branch of the predicate
    // fired. The integration fixture `run_fixture_threshold_flush` covers
    // the trigger-string contract at the dump layer, but the unit suite
    // is what runs on every `cargo test`. These three tests pin each
    // arm against hand-built `Trace` state so a future regression that
    // collapses the `Records` / `Bytes` / `None` decision is caught at
    // compile-then-run time.

    /// Bind a `Trace`'s `buffer` and `buffer_estimated_bytes` fields to
    /// known values without driving them through `push_record`. The
    /// predicate is a pure read of those two fields against the limits,
    /// so synthesised buffer contents are the right input: there is no
    /// billing through `accounting`, and the dict is left empty. Callers
    /// that mix this helper with the production flush path will leak
    /// budget bytes — these tests do not, because they only call the
    /// predicate.
    fn trace_with_buffer_state(
        flush_records: usize,
        flush_bytes: usize,
        records: usize,
        estimated_bytes: usize,
    ) -> Trace {
        let mut trace = trace_with_flush_thresholds(flush_records, flush_bytes);
        for i in 0..records {
            trace.buffer.push(crate::recorder::types::CallRecord {
                call_id: (i as u64) + 1,
                parent: 0,
                fn_id: 0,
                depth: 0,
                t_in_ns: 0,
                t_out_ns: 0,
                cpu_u_ns: 0,
                cpu_s_ns: 0,
                mem_in_bytes: 0,
                mem_out_bytes: 0,
                abnormal_exit: false,
            });
        }
        trace.buffer_estimated_bytes = estimated_bytes;
        trace
    }

    #[test]
    fn flush_predicate_trigger_returns_records_when_buffer_meets_records_threshold() {
        // 5 records ≥ flush_records=5; bytes are well below the bytes
        // threshold, so the predicate must pick `Records`.
        let trace = trace_with_buffer_state(5, 10_000, 5, 64);
        assert_eq!(
            flush_predicate_trigger(&trace),
            Some(FlushTrigger::Records),
            "records-arm must win when buffer.len() >= flush_records",
        );
    }

    #[test]
    fn flush_predicate_trigger_returns_bytes_when_only_byte_threshold_is_met() {
        // Records well below the threshold; bytes ≥ flush_bytes.
        let trace = trace_with_buffer_state(usize::MAX, 80, 1, 97);
        assert_eq!(
            flush_predicate_trigger(&trace),
            Some(FlushTrigger::Bytes),
            "bytes-arm must fire when records-arm cannot",
        );
    }

    #[test]
    fn flush_predicate_trigger_returns_none_when_neither_threshold_is_met() {
        // Both gates disarmed by being safely below the thresholds.
        let trace = trace_with_buffer_state(100, 10_000, 3, 192);
        assert_eq!(
            flush_predicate_trigger(&trace),
            None,
            "predicate must return None when neither threshold is met",
        );
    }

    #[test]
    fn flush_predicate_trigger_records_arm_wins_ties_when_both_thresholds_meet() {
        // Both gates would fire; predicate evaluates records first so
        // the diagnostic `F:` line carries `trigger=records`. The
        // behavioural outcome (a flush happens) is the same either way;
        // pinning the tie-break is a contract for `recorder-dump`.
        let trace = trace_with_buffer_state(4, 80, 4, 256);
        assert_eq!(
            flush_predicate_trigger(&trace),
            Some(FlushTrigger::Records),
            "records-arm wins ties (predicate evaluates records first)",
        );
    }

    #[test]
    fn finish_call_record_flushes_at_exactly_flush_records() {
        let _shipper_guard = crate::shipper::acquire_test_lock();
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let rx = install_test_channel(8);

        let mut trace = trace_with_flush_thresholds(4, usize::MAX);
        let cat = cat_for("ok");

        // Four balanced begin/end pairs → exactly one flush.
        for _ in 0..4 {
            begin_with_snapshots(&mut trace, &cat, entry_snapshots());
            end_with_snapshots(&mut trace, exit_snapshots(), false);
        }
        let batches = drain_queued_batches(&rx);
        assert_eq!(
            batches.len(),
            1,
            "exactly one flush at the records boundary"
        );
        assert_eq!(
            batches[0].calls.len(),
            4,
            "batch carries the four accepted records",
        );
        assert!(trace.buffer.is_empty(), "trace buffer reset after flush");

        // One more begin/end is below the next threshold; no second flush.
        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        end_with_snapshots(&mut trace, exit_snapshots(), false);
        assert!(
            drain_queued_batches(&rx).is_empty(),
            "no second flush below the next records boundary",
        );
        assert_eq!(trace.buffer.len(), 1);

        // PF-6 (`COMMENTS.md`): balance the accounting atomic by
        // mimicking the shipper's consume-side subtract for the
        // already-queued batch, then subtracting the residual record
        // bytes still owned by this trace. The next test's
        // `reset_for_test()` would mask any imbalance anyway, but the
        // explicit loop makes the production budget round-trip visible
        // and matches the slice-3 `rshutdown_release_trace_after_*`
        // test's shape.
        crate::shipper::reset_for_test();
        for batch in &batches {
            accounting::sub(batch.size_estimate);
        }
        accounting::sub(trace.buffer_estimated_bytes);
        assert_eq!(
            accounting::snapshot(),
            0,
            "balanced after fake shipper subtract + residual subtract",
        );
    }

    #[test]
    fn finish_call_record_flushes_at_first_byte_threshold_crossing() {
        let _shipper_guard = crate::shipper::acquire_test_lock();
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let rx = install_test_channel(8);

        // The §3.2 estimator: 64 bytes/record + per-dict-entry bytes
        // (24 + len(fqn) + len(file)). Our `cat_for("ok")` interns
        // `"ok"` with `file="/x.php"` (slice-3 fixture). Compute the
        // post-first-record estimate so the test threshold falls just
        // above it.
        //
        // First accept: 1 record (64) + 1 dict entry (24 + 2 + 7 = 33)
        //   = 97 bytes total. The cap-gate `would_add` is the same.
        // Threshold `flush_bytes = 80` triggers on the first record
        // (buffer_estimated_bytes >= 80).
        let mut trace = trace_with_flush_thresholds(usize::MAX, 80);
        let cat = cat_for("ok");

        // PF-5: assert the predicate would pick the `Bytes` arm at the
        // crossing point — `flush_records = usize::MAX` disarms the
        // records arm, but a future regression could flip the `||`
        // order and still produce a single batch. Synthesise a `Trace`
        // in the exact post-accept shape (1 record, 97 bytes) so the
        // arm assertion is independent of how `finish_call_record`
        // produces the buffer.
        let probe = trace_with_buffer_state(usize::MAX, 80, 1, 97);
        assert_eq!(
            flush_predicate_trigger(&probe),
            Some(FlushTrigger::Bytes),
            "the byte-threshold crossing must trigger via the Bytes arm",
        );

        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        end_with_snapshots(&mut trace, exit_snapshots(), false);

        let batches = drain_queued_batches(&rx);
        assert_eq!(batches.len(), 1, "flush triggers on the byte threshold");
        assert_eq!(batches[0].calls.len(), 1);

        // The post-flush buffer is empty and the running estimate is
        // zero, so the next accept does not cross the threshold again
        // (it's back to the post-first-accept value, which is the
        // first crossing — but the `flush_records = usize::MAX`
        // disarms that gate, and the bytes-gate fires only when
        // `>= 80`, which the second accept reaches).
        //
        // To exercise the "no spurious second flush" property we
        // raise the bytes threshold *above* what one accept produces
        // (97 bytes) and confirm the second accept does not trigger.
        let mut trace2 = trace_with_flush_thresholds(usize::MAX, 200);
        begin_with_snapshots(&mut trace2, &cat, entry_snapshots());
        end_with_snapshots(&mut trace2, exit_snapshots(), false);
        assert!(
            drain_queued_batches(&rx).is_empty(),
            "below the threshold, no flush yet",
        );
        assert_eq!(trace2.buffer.len(), 1);

        // PF-6: balance the atomic. The `batches` vec holds the
        // already-queued batch (the bytes-trigger crossing); the two
        // traces still own the post-flush residual (`trace2` has 1
        // record, 97 bytes; `trace`'s buffer is empty so its residual
        // is 0).
        crate::shipper::reset_for_test();
        for batch in &batches {
            accounting::sub(batch.size_estimate);
        }
        accounting::sub(trace.buffer_estimated_bytes + trace2.buffer_estimated_bytes);
        assert_eq!(
            accounting::snapshot(),
            0,
            "balanced after fake shipper subtract + residuals",
        );
    }

    #[test]
    fn finish_call_record_lifo_consume_branch_does_not_flush() {
        let _shipper_guard = crate::shipper::acquire_test_lock();
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let rx = install_test_channel(8);

        // Tight thresholds so a hypothetical accidental flush would
        // be obvious — but the LIFO branch returns before reaching
        // the predicate, so the channel stays empty.
        let mut trace = trace_with_flush_thresholds(1, 1);
        trace.dropped_begins = 1;
        trace.virtual_depth = 1;

        // An `end` event with `dropped_begins > 0` decrements the
        // matcher (LIFO consume branch) and returns without
        // push_record — the flush predicate is unreachable.
        end_with_snapshots(&mut trace, exit_snapshots(), false);

        assert_eq!(trace.dropped_begins, 0);
        assert!(
            drain_queued_batches(&rx).is_empty(),
            "LIFO consume must not reach the flush predicate",
        );
        crate::shipper::reset_for_test();
    }

    #[test]
    fn finish_call_record_does_not_double_flush_after_a_post_flush_reset() {
        let _shipper_guard = crate::shipper::acquire_test_lock();
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let rx = install_test_channel(8);

        // Flush at every record; verify the cadence is exactly N batches
        // for N records, not 2N (a regression that double-evaluated
        // the predicate would fire twice per record).
        let mut trace = trace_with_flush_thresholds(1, usize::MAX);
        let cat = cat_for("ok");
        for _ in 0..5 {
            begin_with_snapshots(&mut trace, &cat, entry_snapshots());
            end_with_snapshots(&mut trace, exit_snapshots(), false);
        }
        let batches = drain_queued_batches(&rx);
        assert_eq!(
            batches.len(),
            5,
            "exactly one batch per record at flush_records = 1",
        );
        for batch in &batches {
            assert_eq!(
                batch.calls.len(),
                1,
                "each batch carries exactly one record"
            );
        }
        assert!(trace.buffer.is_empty());

        // PF-6: balance the atomic. Five batches landed on the
        // channel, each carrying one record's worth of size_estimate;
        // the trace's residual buffer is empty so its
        // `buffer_estimated_bytes` is zero.
        crate::shipper::reset_for_test();
        for batch in &batches {
            accounting::sub(batch.size_estimate);
        }
        accounting::sub(trace.buffer_estimated_bytes);
        assert_eq!(
            accounting::snapshot(),
            0,
            "balanced after fake shipper subtract + empty residual",
        );
    }

    // --- Phase-4 slice 2: RSHUTDOWN final flush ---------------------------

    #[test]
    fn rshutdown_release_trace_flushes_non_empty_buffer_then_balances_accounting() {
        let _shipper_guard = crate::shipper::acquire_test_lock();
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let rx = install_test_channel(8);

        // Drive a sub-threshold workload so no mid-request flush
        // happens. `flush_records = 100` is comfortably above the 3
        // accepts below; `flush_bytes = usize::MAX` disarms the
        // bytes-gate.
        rinit_allocate_trace(
            stub_identity(),
            TraceLimits {
                max_depth: 1024,
                buffer_cap_bytes: 64 * 1024 * 1024,
                flush_records: 100,
                flush_bytes: usize::MAX,
            },
        );
        with_current_trace(|trace| {
            let cat = cat_for("ok");
            for _ in 0..3 {
                begin_with_snapshots(trace, &cat, entry_snapshots());
                end_with_snapshots(trace, exit_snapshots(), false);
            }
            assert_eq!(trace.buffer.len(), 3);
        });
        // No mid-request batches yet.
        assert!(drain_queued_batches(&rx).is_empty());

        rshutdown_release_trace();

        // Exactly one batch on the channel, carrying the three
        // records; the accounting atomic is non-zero because the
        // shipper hasn't consumed yet (the shipper-side subtract is
        // Section 6's concern).
        let batches = drain_queued_batches(&rx);
        assert_eq!(batches.len(), 1, "RSHUTDOWN flushes one final batch");
        assert_eq!(batches[0].calls.len(), 3);
        // Simulate the shipper-side consume-path subtract so the next
        // test sees a balanced atomic. Section 6 wires this for real.
        accounting::sub(batches[0].size_estimate);
        assert_eq!(accounting::snapshot(), 0);

        crate::shipper::reset_for_test();
    }

    #[test]
    fn rshutdown_release_trace_with_empty_buffer_does_not_flush() {
        let _shipper_guard = crate::shipper::acquire_test_lock();
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let rx = install_test_channel(8);

        rinit_allocate_trace(stub_identity(), permissive_limits());
        // No calls at all — buffer stays empty.
        rshutdown_release_trace();

        assert!(
            drain_queued_batches(&rx).is_empty(),
            "empty buffer must NOT produce a final batch",
        );
        assert_eq!(accounting::snapshot(), 0);

        crate::shipper::reset_for_test();
    }

    #[test]
    fn rshutdown_release_trace_after_threshold_flush_only_emits_residual() {
        let _shipper_guard = crate::shipper::acquire_test_lock();
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let rx = install_test_channel(8);

        rinit_allocate_trace(
            stub_identity(),
            TraceLimits {
                max_depth: 1024,
                buffer_cap_bytes: 64 * 1024 * 1024,
                flush_records: 4,
                flush_bytes: usize::MAX,
            },
        );
        with_current_trace(|trace| {
            let cat = cat_for("ok");
            // 6 calls @ flush_records = 4 → one mid-request batch of 4,
            // 2 records left in the buffer.
            for _ in 0..6 {
                begin_with_snapshots(trace, &cat, entry_snapshots());
                end_with_snapshots(trace, exit_snapshots(), false);
            }
        });

        // The mid-request batch has already landed; drain it so the
        // RSHUTDOWN check is clean.
        let mid = drain_queued_batches(&rx);
        assert_eq!(mid.len(), 1, "one mid-request flush");
        assert_eq!(mid[0].calls.len(), 4);

        rshutdown_release_trace();

        let residual = drain_queued_batches(&rx);
        assert_eq!(residual.len(), 1, "exactly one residual batch");
        assert_eq!(residual[0].calls.len(), 2, "two records carried over");

        // The shipper's consume subtract is Section 6; emulate it
        // here so the atomic ends at zero.
        accounting::sub(mid[0].size_estimate);
        accounting::sub(residual[0].size_estimate);
        assert_eq!(accounting::snapshot(), 0);

        crate::shipper::reset_for_test();
    }

    // --- MSHUTDOWN drain of unclosed CallFrames --------------------------

    #[test]
    fn rshutdown_drains_a_single_unclosed_root_frame_as_abnormal_exit() {
        // PHP's script-body closure begins at script start; its
        // matching end-fcall never fires before MSHUTDOWN. Without
        // the drain, no CallRecord with call_id=1 would ever ship.
        let _shipper_guard = crate::shipper::acquire_test_lock();
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let rx = install_test_channel(8);

        rinit_allocate_trace(stub_identity(), permissive_limits());
        with_current_trace(|trace| {
            let cat = cat_for("script_main_closure");
            // One begin, NO matching end_with_snapshots — the frame
            // sits on `Trace.stack` waiting for the drain.
            begin_with_snapshots(trace, &cat, entry_snapshots());
            assert_eq!(trace.stack.len(), 1);
            assert!(
                trace.buffer.is_empty(),
                "no records emitted yet; the begin only pushes a frame",
            );
        });

        rshutdown_release_trace();

        let batches = drain_queued_batches(&rx);
        assert_eq!(
            batches.len(),
            1,
            "the drain must produce exactly one final batch covering the drained root",
        );
        assert_eq!(batches[0].calls.len(), 1);
        let root = &batches[0].calls[0];
        assert_eq!(root.call_id, 1);
        assert_eq!(root.parent, 0);
        assert_eq!(root.depth, 0);
        assert!(
            root.abnormal_exit,
            "a frame drained at MSHUTDOWN ended abnormally by definition",
        );
        assert!(
            root.t_out_ns >= root.t_in_ns,
            "t_out_ns came from the drain snapshot; must be >= the captured t_in_ns",
        );

        // The shipper-side consume-subtract is Section 6; emulate it
        // so the atomic ends balanced.
        accounting::sub(batches[0].size_estimate);
        assert_eq!(accounting::snapshot(), 0);
        crate::shipper::reset_for_test();
    }

    #[test]
    fn rshutdown_drains_two_nested_unclosed_frames_top_first_with_shared_exit_snapshot() {
        let _shipper_guard = crate::shipper::acquire_test_lock();
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let rx = install_test_channel(8);

        rinit_allocate_trace(stub_identity(), permissive_limits());
        with_current_trace(|trace| {
            let outer = cat_for("outer");
            let inner = cat_for("inner");
            // Two unmatched begins → both frames on the stack.
            begin_with_snapshots(trace, &outer, entry_snapshots());
            begin_with_snapshots(trace, &inner, entry_snapshots());
            assert_eq!(trace.stack.len(), 2);
        });

        rshutdown_release_trace();

        let batches = drain_queued_batches(&rx);
        assert_eq!(batches.len(), 1);
        let calls = &batches[0].calls;
        assert_eq!(calls.len(), 2);
        // Top-first order: the inner (call_id=2, parent=1, depth=1)
        // frame is popped and emitted first; the outer
        // (call_id=1, parent=0, depth=0) frame follows.
        assert_eq!(
            (calls[0].call_id, calls[0].parent, calls[0].depth),
            (2, 1, 1)
        );
        assert_eq!(
            (calls[1].call_id, calls[1].parent, calls[1].depth),
            (1, 0, 0)
        );
        assert!(calls[0].abnormal_exit && calls[1].abnormal_exit);
        // Shared exit snapshot — both records get byte-equal exit
        // fields. The CPU/mem fields may legitimately differ (each
        // is `snapshot - frame.cpu_*_in_ns`), so we assert only on
        // `t_out_ns` and `mem_out_bytes`, which are taken directly
        // from the shared snapshot.
        assert_eq!(calls[0].t_out_ns, calls[1].t_out_ns);
        assert_eq!(calls[0].mem_out_bytes, calls[1].mem_out_bytes);

        accounting::sub(batches[0].size_estimate);
        assert_eq!(accounting::snapshot(), 0);
        crate::shipper::reset_for_test();
    }

    #[test]
    fn rshutdown_drain_is_a_noop_for_a_balanced_trace() {
        let _shipper_guard = crate::shipper::acquire_test_lock();
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let rx = install_test_channel(8);

        rinit_allocate_trace(stub_identity(), permissive_limits());
        with_current_trace(|trace| {
            let cat = cat_for("balanced");
            begin_with_snapshots(trace, &cat, entry_snapshots());
            end_with_snapshots(trace, exit_snapshots(), false);
            assert!(
                trace.stack.is_empty(),
                "balanced begin/end leaves the stack empty; drain must be a no-op",
            );
            assert_eq!(trace.buffer.len(), 1);
        });

        rshutdown_release_trace();

        let batches = drain_queued_batches(&rx);
        assert_eq!(
            batches.len(),
            1,
            "the existing flush branch sends the one buffered record",
        );
        assert_eq!(
            batches[0].calls.len(),
            1,
            "no extra drained records — the stack was already empty",
        );
        assert!(
            !batches[0].calls[0].abnormal_exit,
            "the balanced record ended normally; the drain did not synthesize anything",
        );

        accounting::sub(batches[0].size_estimate);
        assert_eq!(accounting::snapshot(), 0);
        crate::shipper::reset_for_test();
    }

    #[test]
    fn rshutdown_drain_on_empty_slot_sends_nothing_and_leaves_atomic_at_zero() {
        let _shipper_guard = crate::shipper::acquire_test_lock();
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let rx = install_test_channel(8);

        // No rinit_allocate_trace — the slot is empty.
        rshutdown_release_trace();
        rshutdown_release_trace();

        assert!(
            drain_queued_batches(&rx).is_empty(),
            "no trace means nothing to drain and nothing to flush",
        );
        assert_eq!(accounting::snapshot(), 0);
        crate::shipper::reset_for_test();
    }

    // --- BootObserver dispatcher ------------------------------------------

    #[test]
    fn boot_observer_disabled_should_observe_returns_false() {
        let b = BootObserver::Disabled;
        assert!(!b.should_observe(&empty_fcall_info()));
    }

    #[test]
    fn boot_observer_disabled_begin_and_end_do_not_panic() {
        // We cannot construct a real `ExecuteData` outside PHP, so the
        // explicit assertion is "the dispatcher's `Disabled` arms
        // compile to a no-op match arm with no body". The match-arm
        // shape is the contract; this is a smoke test that the enum
        // variant exists and the trait impl wires it through.
        let b = BootObserver::Disabled;
        assert!(matches!(b, BootObserver::Disabled));
    }

    #[test]
    fn boot_observer_recorder_should_observe_returns_true_unconditionally() {
        // Construct a `Recorder` directly (zero-size, no FFI). Its
        // `should_observe` is true regardless of the slot state — the
        // caching contract documented at the top of this module.
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let r = BootObserver::Recorder(Recorder);
        rshutdown_release_trace();
        assert!(r.should_observe(&empty_fcall_info()));
    }

    // --- categorise_lazy parity with categorise ---------------------------
    //
    // `categorise_lazy` is the zero-alloc sibling of `categorise` used
    // by the production hot path. These tests assert it routes every
    // shape the existing `categorise` tests covered to the same
    // `FunctionKind`, the same rendered fqn, the same file/line, and a
    // `FunctionKeyRef` that round-trips to the same owning `FunctionKey`
    // that `categorise` produces. The property is what lets the
    // borrow-keyed dictionary probe and the owning-key probe agree on
    // identity (recorder-hot-path-tuning §3).

    #[test]
    fn categorise_lazy_routes_methods_to_the_method_branch_with_matching_components() {
        let info = stub_site(
            Some("greet"),
            Some("Greeter"),
            Some("/srv/app.php"),
            7,
            false,
        );
        let owned = categorise(&info);
        let lazy = categorise_lazy(&info);
        assert_eq!(lazy.kind, owned.kind);
        assert_eq!(lazy.kind, FunctionKind::Method);
        assert!(matches!(
            lazy.key_ref,
            FunctionKeyRef::Method {
                class: "Greeter",
                method: "greet"
            }
        ));
        assert_eq!(lazy.fqn_spec.render(), "Greeter::greet");
        assert_eq!(lazy.fqn_spec.render(), owned.fqn.as_ref());
        assert_eq!(lazy.file, "/srv/app.php");
        assert_eq!(lazy.line, 7);
        // Round-trip identity: the lazy key materialises to the same
        // owning `FunctionKey` that `categorise` emits.
        assert_eq!(lazy.key_ref.to_owned(), owned.key);
    }

    #[test]
    fn categorise_lazy_routes_user_functions_to_the_function_branch_with_matching_components() {
        let info = stub_site(Some("my_fn"), None, Some("/x.php"), 20, false);
        let owned = categorise(&info);
        let lazy = categorise_lazy(&info);
        assert_eq!(lazy.kind, FunctionKind::Function);
        assert!(matches!(
            lazy.key_ref,
            FunctionKeyRef::Function {
                file: "/x.php",
                function: "my_fn",
                line: 20
            }
        ));
        assert_eq!(lazy.fqn_spec.render(), "my_fn");
        assert_eq!(lazy.fqn_spec.render(), owned.fqn.as_ref());
        assert_eq!(lazy.key_ref.to_owned(), owned.key);
    }

    #[test]
    fn categorise_lazy_routes_closures_via_function_name_prefix() {
        let info = stub_site(
            Some("{closure:/srv/app.php:42}"),
            None,
            Some("/srv/app.php"),
            42,
            false,
        );
        let owned = categorise(&info);
        let lazy = categorise_lazy(&info);
        assert_eq!(lazy.kind, FunctionKind::Closure);
        assert!(matches!(
            lazy.key_ref,
            FunctionKeyRef::Closure {
                file: "/srv/app.php",
                line: 42
            }
        ));
        assert_eq!(lazy.fqn_spec.render(), "closure:/srv/app.php:42");
        assert_eq!(lazy.fqn_spec.render(), owned.fqn.as_ref());
        assert_eq!(lazy.key_ref.to_owned(), owned.key);
    }

    #[test]
    fn categorise_lazy_routes_closures_when_function_name_is_absent() {
        let info = stub_site(None, None, Some("/x.php"), 1, false);
        let lazy = categorise_lazy(&info);
        assert_eq!(lazy.kind, FunctionKind::Closure);
        assert!(matches!(
            lazy.key_ref,
            FunctionKeyRef::Closure {
                file: "/x.php",
                line: 1
            }
        ));
        assert_eq!(lazy.fqn_spec.render(), "closure:/x.php:1");
    }

    #[test]
    fn categorise_lazy_routes_internals_to_the_internal_branch() {
        let info = stub_site(Some("array_map"), None, None, 0, true);
        let owned = categorise(&info);
        let lazy = categorise_lazy(&info);
        assert_eq!(lazy.kind, FunctionKind::Internal);
        assert!(matches!(
            lazy.key_ref,
            FunctionKeyRef::Internal { name: "array_map" }
        ));
        assert_eq!(lazy.fqn_spec.render(), "array_map");
        assert_eq!(lazy.fqn_spec.render(), owned.fqn.as_ref());
        assert_eq!(lazy.file, "");
        assert_eq!(lazy.line, 0);
        assert_eq!(lazy.key_ref.to_owned(), owned.key);
    }

    #[test]
    fn fqn_spec_render_len_matches_actual_string_length_across_every_variant() {
        let cases = [
            FqnSpec::Borrowed("noop"),
            FqnSpec::Method {
                class: "Ns\\Cls",
                method: "doThing",
            },
            FqnSpec::Closure {
                file: "/srv/app.php",
                line: 42,
            },
            FqnSpec::Closure {
                file: "/x.php",
                line: 0,
            },
            FqnSpec::Closure {
                file: "/x.php",
                line: 999_999,
            },
            FqnSpec::Unknown {
                kind_label: "anonymous",
                addr: 0xdead_beef,
            },
            FqnSpec::Unknown {
                kind_label: "unknown",
                addr: 0,
            },
        ];
        for spec in &cases {
            let rendered = spec.render();
            assert_eq!(
                spec.render_len(),
                rendered.len(),
                "render_len mismatch for {spec:?} (rendered `{rendered}`)",
            );
        }
    }

    #[test]
    fn begin_with_snapshots_lazy_matches_begin_with_snapshots_on_a_simple_workload() {
        // Two equivalent calls — one through the bench-seam (owning)
        // path, one through the lazy (borrow) path — must produce the
        // same `(call_id, parent, fn_id, depth)` and the same
        // dictionary state.
        let _account_guard = account_guard();
        accounting::reset_for_test();

        let info = stub_site(Some("noop"), None, Some("/x.php"), 1, false);
        let owned = categorise(&info);
        let lazy = categorise_lazy(&info);

        let mut trace_a = Trace::new(stub_identity(), permissive_limits());
        let mut trace_b = Trace::new(stub_identity(), permissive_limits());

        begin_with_snapshots(&mut trace_a, &owned, entry_snapshots());
        begin_with_snapshots_lazy(&mut trace_b, &lazy, entry_snapshots());

        assert_eq!(trace_a.stack.len(), 1);
        assert_eq!(trace_b.stack.len(), 1);
        let frame_a = trace_a.stack.last().unwrap();
        let frame_b = trace_b.stack.last().unwrap();
        assert_eq!(frame_a.call_id, frame_b.call_id);
        assert_eq!(frame_a.parent, frame_b.parent);
        assert_eq!(frame_a.fn_id, frame_b.fn_id);
        assert_eq!(frame_a.depth, frame_b.depth);

        // Both dictionaries staged the same `DictEntry`.
        let staged_a = trace_a.dictionary.take_new_entries();
        let staged_b = trace_b.dictionary.take_new_entries();
        assert_eq!(staged_a, staged_b);
    }

    // --- cpu_snapshot_mode integration (recorder-cpu-snapshot-cadence) ----
    //
    // These tests exercise `EntrySnapshots::capture_now()` /
    // `ExitSnapshots::capture_now()` against each mode via the test
    // override seam (`CpuSnapshotModeTestGuard`). They prove the
    // mode is honoured uniformly across begin and end, not just at
    // the lower-level `clocks::snapshot_now` boundary.

    #[test]
    fn entry_snapshots_capture_now_under_off_mode_returns_zero_cpu_fields() {
        let _guard = CpuSnapshotModeTestGuard::new(crate::config::CpuSnapshotMode::Off);

        // Burn some CPU first so a `PerCall` snapshot at the same
        // point would observe non-zero — proves the zeroing is
        // mode-driven, not coincidence.
        let mut sum: u64 = 0;
        for i in 0..50_000_u64 {
            sum = sum.wrapping_add(i.wrapping_mul(31));
        }
        std::hint::black_box(sum);

        let snap = EntrySnapshots::capture_now();
        assert_eq!(snap.cpu_u_in_ns, 0, "Off mode must force cpu_u_in_ns = 0");
        assert_eq!(snap.cpu_s_in_ns, 0, "Off mode must force cpu_s_in_ns = 0");
        // Wall clock and memory still populate.
        assert!(snap.t_in_ns >= 0);
        assert!(snap.mem_in_bytes >= 0);
    }

    #[test]
    fn exit_snapshots_capture_now_under_off_mode_returns_zero_cpu_fields() {
        let _guard = CpuSnapshotModeTestGuard::new(crate::config::CpuSnapshotMode::Off);
        let mut sum: u64 = 0;
        for i in 0..50_000_u64 {
            sum = sum.wrapping_add(i.wrapping_mul(31));
        }
        std::hint::black_box(sum);

        let snap = ExitSnapshots::capture_now();
        assert_eq!(snap.cpu_u_now_ns, 0);
        assert_eq!(snap.cpu_s_now_ns, 0);
        assert!(snap.t_out_ns >= 0);
        assert!(snap.mem_out_bytes >= 0);
    }

    #[test]
    fn entry_snapshots_capture_now_under_per_call_returns_non_negative_cpu_fields() {
        let _guard = CpuSnapshotModeTestGuard::new(crate::config::CpuSnapshotMode::PerCall);
        let mut sum: u64 = 0;
        for i in 0..50_000_u64 {
            sum = sum.wrapping_add(i.wrapping_mul(31));
        }
        std::hint::black_box(sum);

        let snap = EntrySnapshots::capture_now();
        // PerCall returns non-negative values; the actual reading can
        // legitimately be 0 for very short busy loops (R-11 microsecond
        // granularity), but never negative.
        assert!(snap.cpu_u_in_ns >= 0, "PerCall: {snap:?}");
        assert!(snap.cpu_s_in_ns >= 0, "PerCall: {snap:?}");
    }

    #[test]
    fn recorder_begin_end_pair_under_off_mode_emits_zero_cpu_call_record() {
        // End-to-end: drive a begin/end pair through the lazy production
        // path against a real Trace and assert the emitted CallRecord
        // has cpu_u_ns / cpu_s_ns == 0. Mirrors the production observer
        // chain (Recorder::begin_handler → categorise_lazy →
        // begin_with_snapshots_lazy → end_with_snapshots) without
        // standing up a PHP runtime.
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let _mode_guard = CpuSnapshotModeTestGuard::new(crate::config::CpuSnapshotMode::Off);

        let mut trace = Trace::new(stub_identity(), permissive_limits());
        let info = stub_site(Some("noop"), None, Some("/x.php"), 1, false);
        let lazy = categorise_lazy(&info);

        // Burn CPU between begin and end so a PerCall trace would
        // record non-zero CPU; under Off mode we expect zero.
        let entry = EntrySnapshots::capture_now();
        begin_with_snapshots_lazy(&mut trace, &lazy, entry);
        let mut sum: u64 = 0;
        for i in 0..50_000_u64 {
            sum = sum.wrapping_add(i.wrapping_mul(31));
        }
        std::hint::black_box(sum);
        let exit = ExitSnapshots::capture_now();
        end_with_snapshots(&mut trace, exit, false);

        // The CallRecord should land in the buffer.
        assert_eq!(trace.buffer.len(), 1);
        let record = &trace.buffer[0];
        assert_eq!(
            record.cpu_u_ns, 0,
            "Off mode: CallRecord must carry cpu_u_ns = 0; got {record:?}"
        );
        assert_eq!(
            record.cpu_s_ns, 0,
            "Off mode: CallRecord must carry cpu_s_ns = 0; got {record:?}"
        );
        // Wall and memory fields still populate normally.
        assert!(record.t_in_ns >= 0);
        assert!(record.t_out_ns >= record.t_in_ns);
    }

    #[test]
    fn recorder_begin_end_pair_under_per_call_mode_emits_non_negative_cpu_call_record() {
        let _account_guard = account_guard();
        accounting::reset_for_test();
        let _mode_guard = CpuSnapshotModeTestGuard::new(crate::config::CpuSnapshotMode::PerCall);

        let mut trace = Trace::new(stub_identity(), permissive_limits());
        let info = stub_site(Some("noop"), None, Some("/x.php"), 1, false);
        let lazy = categorise_lazy(&info);

        let entry = EntrySnapshots::capture_now();
        begin_with_snapshots_lazy(&mut trace, &lazy, entry);
        let mut sum: u64 = 0;
        for i in 0..50_000_u64 {
            sum = sum.wrapping_add(i.wrapping_mul(31));
        }
        std::hint::black_box(sum);
        let exit = ExitSnapshots::capture_now();
        end_with_snapshots(&mut trace, exit, false);

        assert_eq!(trace.buffer.len(), 1);
        let record = &trace.buffer[0];
        // PerCall returns non-negative; the actual value depends on
        // kernel scheduling and getrusage granularity.
        assert!(record.cpu_u_ns >= 0, "PerCall: {record:?}");
        assert!(record.cpu_s_ns >= 0, "PerCall: {record:?}");
    }

    #[test]
    fn current_cpu_snapshot_mode_defaults_to_per_call_when_config_global_is_unset() {
        // The helper falls back to PerCall when Config::global is
        // None (test builds where MINIT has not run). Confirms the
        // backward-compatible default.
        //
        // Serialises against tests that publish an override — without
        // the lock, a parallel `CpuSnapshotModeTestGuard::new(Off)`
        // could win the race between our `clear` and our `read`,
        // returning `Off` and failing the assertion.
        let _audit_lock = lock_cpu_snapshot_mode_for_test();
        clear_cpu_snapshot_mode_for_test();
        assert_eq!(
            current_cpu_snapshot_mode(),
            crate::config::CpuSnapshotMode::PerCall
        );
    }

    #[test]
    fn current_cpu_snapshot_mode_test_override_round_trips_both_variants() {
        // Sanity check on the test seam itself — PerCall and Off
        // round-trip through the override. Drop-side cleanup is
        // exercised by directly calling `set` / `clear` while
        // holding the audit lock for the whole test — using nested
        // `CpuSnapshotModeTestGuard` scopes would briefly release
        // the lock between scopes and let a parallel test win the
        // race, so we hold the lock once across all four state
        // transitions.
        let _audit_lock = lock_cpu_snapshot_mode_for_test();

        set_cpu_snapshot_mode_for_test(crate::config::CpuSnapshotMode::Off);
        assert_eq!(
            current_cpu_snapshot_mode(),
            crate::config::CpuSnapshotMode::Off
        );

        clear_cpu_snapshot_mode_for_test();
        // After clear the override is unset; helper falls back to
        // default (PerCall in tests, since Config::global is None).
        assert_eq!(
            current_cpu_snapshot_mode(),
            crate::config::CpuSnapshotMode::PerCall
        );

        set_cpu_snapshot_mode_for_test(crate::config::CpuSnapshotMode::PerCall);
        assert_eq!(
            current_cpu_snapshot_mode(),
            crate::config::CpuSnapshotMode::PerCall
        );

        clear_cpu_snapshot_mode_for_test();
        assert_eq!(
            current_cpu_snapshot_mode(),
            crate::config::CpuSnapshotMode::PerCall
        );
    }

    // --- should_observe filter (skip-functions-directive / REVIEW.md P-0) -

    fn fcall_info_named(
        function_name: Option<&'static str>,
        class_name: Option<&'static str>,
        is_internal: bool,
    ) -> FcallInfo<'static> {
        FcallInfo {
            function_name,
            class_name,
            filename: None,
            lineno: 0,
            is_internal,
        }
    }

    fn skip_set(entries: &[&str]) -> std::collections::HashSet<String> {
        entries.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn should_observe_filter_returns_false_for_skipped_free_function() {
        let set = skip_set(&["strlen"]);
        let info = fcall_info_named(Some("strlen"), None, true);
        assert!(!should_observe_filter(false, &set, &info));
    }

    #[test]
    fn should_observe_filter_returns_true_for_unfiltered_free_function() {
        let set = skip_set(&["strlen"]);
        let info = fcall_info_named(Some("my_user_function"), None, false);
        assert!(should_observe_filter(false, &set, &info));
    }

    #[test]
    fn should_observe_filter_uses_class_method_form_for_methods() {
        // Method `Foo::bar` matches the `foo::bar` entry...
        let set = skip_set(&["foo::bar"]);
        let info = fcall_info_named(Some("bar"), Some("Foo"), false);
        assert!(!should_observe_filter(false, &set, &info));
    }

    #[test]
    fn should_observe_filter_free_function_entry_does_not_match_method() {
        // ...but a bare `bar` entry MUST NOT match the same method
        // (free-function and method names live in different
        // namespaces per design.md D-5).
        let set = skip_set(&["bar"]);
        let info = fcall_info_named(Some("bar"), Some("Foo"), false);
        assert!(should_observe_filter(false, &set, &info));
    }

    #[test]
    fn should_observe_filter_lowercases_both_sides_for_case_insensitive_match() {
        let set = skip_set(&["strlen"]);
        let info = fcall_info_named(Some("StRlEn"), None, true);
        assert!(!should_observe_filter(false, &set, &info));
    }

    #[test]
    fn should_observe_filter_lowercases_class_part_too() {
        let set = skip_set(&["foo::bar"]);
        let info = fcall_info_named(Some("BAR"), Some("FOO"), false);
        assert!(!should_observe_filter(false, &set, &info));
    }

    #[test]
    fn should_observe_filter_skip_internal_returns_false_for_any_internal_call() {
        // Empty skip_functions set; skip_internal = true. Every
        // internal call is skipped regardless of name.
        let set = skip_set(&[]);
        let info = fcall_info_named(Some("some_internal_not_on_list"), None, true);
        assert!(!should_observe_filter(true, &set, &info));
    }

    #[test]
    fn should_observe_filter_skip_internal_does_not_affect_user_calls() {
        let set = skip_set(&[]);
        let info = fcall_info_named(Some("my_user_function"), None, false);
        assert!(should_observe_filter(true, &set, &info));
    }

    #[test]
    fn should_observe_filter_returns_true_when_no_function_name_is_available() {
        // Anonymous closure or opcode-specialised builtin: we can't
        // filter what we can't name. Observe (conservative).
        let set = skip_set(&["strlen"]);
        let info = fcall_info_named(None, None, false);
        assert!(should_observe_filter(false, &set, &info));
    }

    #[test]
    fn should_observe_filter_or_composition_skip_functions_takes_precedence() {
        // Both filters would skip; result is still `false`. Sanity
        // check that the OR composition doesn't accidentally invert.
        let set = skip_set(&["strlen"]);
        let info = fcall_info_named(Some("strlen"), None, true);
        assert!(!should_observe_filter(true, &set, &info));
    }

    // --- Gate-before-snapshot (REVIEW.md P-1 / gate-before-snapshot) ------
    //
    // These tests pin "dropped begin/end events SHALL NOT invoke clock or
    // memory syscalls". The `SNAPSHOT_CAPTURE_COUNT_FOR_TEST` counter is
    // incremented inside `EntrySnapshots::capture_now` and
    // `ExitSnapshots::capture_now` under `#[cfg(test)]` (production builds
    // are unaffected — both the static and the increment site are
    // `cfg(test)`-gated). Tests serialise observations via `account_guard`
    // so the counter delta is reliable under `cargo test`'s parallel
    // runner.

    #[test]
    fn begin_dropped_by_depth_gate_captures_zero_snapshots() {
        let _guard = account_guard();
        accounting::reset_for_test();
        reset_snapshot_capture_count_for_test();
        let mut trace = trace_with_max_depth(5);
        let cat = cat_for("recurse");

        // Fill the trace to exactly `max_depth` via the production
        // owning-key path. These five begins each capture a snapshot
        // (the counter increments by 5).
        for _ in 0..5 {
            let snap = EntrySnapshots::capture_now();
            begin_with_snapshots(&mut trace, &cat, snap);
        }
        let counter_at_max = snapshot_capture_count_for_test();
        assert_eq!(
            counter_at_max, 5,
            "five accepted begins should have captured exactly five snapshots"
        );

        // The sixth begin is the one this test is about: it must be
        // dropped by the depth gate without paying for a snapshot.
        // We exercise the **predicate** directly — the handler
        // composition is what `Recorder::begin_handler` does
        // internally; the predicate is the load-bearing surface for
        // "no syscall on drop".
        let accepted = begin_would_accept(&mut trace, &cat);
        assert!(!accepted, "depth gate must reject the sixth begin");

        // The counter MUST NOT have advanced. This is the headline
        // P-1 assertion.
        assert_eq!(
            snapshot_capture_count_for_test(),
            counter_at_max,
            "depth-gated drop must capture zero snapshots"
        );

        // Drop semantics are preserved verbatim.
        assert_eq!(trace.virtual_depth, 6, "depth tracks PHP regardless");
        assert_eq!(trace.dropped_begins, 1);
        assert_eq!(trace.drop_counter.load(Ordering::Acquire), 1);
    }

    #[test]
    fn begin_dropped_by_cap_gate_captures_zero_snapshots() {
        let _guard = account_guard();
        accounting::reset_for_test();
        reset_snapshot_capture_count_for_test();
        // Cap is tight enough that the very first begin's `would_add`
        // projection breaches it.
        let mut trace = trace_with_cap(1);
        let cat = cat_for("blocked");

        let accepted = begin_would_accept(&mut trace, &cat);
        assert!(!accepted, "cap gate must reject the begin");

        assert_eq!(
            snapshot_capture_count_for_test(),
            0,
            "cap-gated drop must capture zero snapshots"
        );
        assert_eq!(trace.dropped_begins, 1);
        assert_eq!(trace.drop_counter.load(Ordering::Acquire), 1);
    }

    #[test]
    fn end_paired_with_dropped_begin_captures_zero_snapshots() {
        let _guard = account_guard();
        accounting::reset_for_test();
        reset_snapshot_capture_count_for_test();
        // Construct a trace where one begin has already been dropped
        // (the `dropped_begins > 0` LIFO precondition the end path
        // peeks).
        let mut trace = trace_with_max_depth(1024);
        trace.virtual_depth = 1;
        trace.dropped_begins = 1;

        let accepted = end_would_accept(&mut trace);
        assert!(!accepted, "end paired with dropped begin must not accept");

        assert_eq!(
            snapshot_capture_count_for_test(),
            0,
            "end paired with dropped begin must capture zero snapshots"
        );
        assert_eq!(
            trace.dropped_begins, 0,
            "LIFO matcher must consume the dropped_begins counter"
        );
        assert_eq!(
            trace.virtual_depth, 0,
            "virtual_depth must decrement regardless of accept/drop"
        );
    }

    #[test]
    fn accepted_begin_end_pair_captures_exactly_two_snapshots() {
        let _guard = account_guard();
        accounting::reset_for_test();
        reset_snapshot_capture_count_for_test();
        let mut trace = trace_with_max_depth(1024);
        let cat = cat_for("happy");

        // Begin: predicate accepts → snapshot captured → accept tail
        // stages the frame.
        let begin_ok = begin_would_accept(&mut trace, &cat);
        assert!(begin_ok);
        let begin_snap = EntrySnapshots::capture_now();
        begin_with_snapshots_accept(&mut trace, &cat, begin_snap);

        // End: predicate accepts (no dropped_begins to consume) →
        // snapshot captured → accept tail emits the record.
        let end_ok = end_would_accept(&mut trace);
        assert!(end_ok);
        let end_snap = ExitSnapshots::capture_now();
        end_with_snapshots_accept(&mut trace, end_snap, false);

        assert_eq!(
            snapshot_capture_count_for_test(),
            2,
            "accepted begin+end pair must capture exactly two snapshots"
        );
        assert_eq!(trace.dropped_begins, 0);
        assert_eq!(trace.drop_counter.load(Ordering::Acquire), 0);
        assert_eq!(
            trace.buffer.len(),
            1,
            "accepted pair must emit one CallRecord"
        );
    }
}
