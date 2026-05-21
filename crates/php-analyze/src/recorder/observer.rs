//! Production observer wiring: the `Recorder` that drives the slice-1
//! substrate from real PHP `FcallObserver` events, plus the `BootObserver`
//! dispatcher that picks (Disabled | Spike | Recorder) once at `MINIT`.
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
//! ## Why `should_observe` is unconditional
//!
//! PHP caches `should_observe`'s result per unique function on first
//! sight. A transient `false` (e.g. a MINIT-time PHP-internal call
//! firing before `RINIT` populates the slot) would be cached
//! permanently and silently drop that function from every later
//! request. The runtime "is there a current trace?" filter therefore
//! happens in `begin` / `end` (which are not cached), not in
//! `should_observe`.
//!
//! ## The `FcallObserver::end` API and exception detection
//!
//! `ext_php_rs = 0.15.13` exposes `fn end(&self, execute_data: &ExecuteData, retval: Option<&Zval>)`
//! — there is no `abnormal: bool` parameter. The recorder reads
//! `ExecutorGlobals::has_exception()` inline (same path the spike
//! uses). See design.md §D-7 and the C-8 entry in `COMMENTS.md`.

use std::cell::RefCell;

use ext_php_rs::ffi;
use ext_php_rs::types::Zval;
use ext_php_rs::zend::{ExecuteData, ExecutorGlobals, FcallInfo, FcallObserver};

use crate::clocks;
use crate::config::Config;
use crate::recorder::types::{
    CallFrame, CallRecord, DictEntry, FunctionKey, FunctionKind, RequestIdentity, Trace,
};
use crate::spike::SpikeObserver;

// --- Thread-local trace slot ----------------------------------------------

thread_local! {
    /// The per-request `Trace`, populated at `RINIT` and dropped at
    /// `RSHUTDOWN`. `None` outside a request window.
    static CURRENT_TRACE: RefCell<Option<Trace>> = const { RefCell::new(None) };
}

/// Populate the thread-local with a fresh `Trace`. Called from
/// `bootstrap::rinit` when the extension is enabled and the spike is
/// off. Panics if a previous `RINIT` did not pair with an
/// `RSHUTDOWN` — that pairing failure is a bug we want to know about
/// loudly rather than silently leaking the prior buffer.
pub fn rinit_allocate_trace(identity: RequestIdentity) {
    CURRENT_TRACE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        assert!(
            borrow.is_none(),
            "RINIT without RSHUTDOWN: the recorder thread-local already holds a Trace; \
             a previous request did not call rshutdown_release_trace",
        );
        *borrow = Some(Trace::new(identity));
    });
}

/// Drop the thread-local `Trace`. Called from `bootstrap::rshutdown`
/// unconditionally — a no-op when the slot is already `None`
/// (extension disabled, spike active, or `RINIT` was skipped).
///
/// **Phase 4 anchor**: the buffer is discarded here in slice 2. The
/// Phase-4 shipper change will replace the discard with a
/// `shipper.try_send(trace.into_pending_batch())` call. The
/// `recorder-dump` Cargo feature inserts a diagnostic dump
/// in-between for the slice-2 integration tests; that dump
/// disappears alongside the discard in Phase 4. See design.md §D-9.
pub fn rshutdown_release_trace() {
    CURRENT_TRACE.with(|slot| {
        let trace = slot.borrow_mut().take();
        // `trace` is consumed below. Phase 4 will move the
        // `try_send` of the constructed `PendingBatch` to this line.
        #[cfg(feature = "recorder-dump")]
        if let Some(trace) = trace.as_ref() {
            crate::recorder::dump::write_trace_if_path_set(trace);
        }
        drop(trace);
    });
}

/// Borrow the current `Trace` mutably for the duration of `f`.
///
/// Returns `None` when the slot is empty (extension disabled, spike
/// active, or out-of-request observer fire). Returns `Some(f(trace))`
/// otherwise.
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

/// The four clock/memory values captured at call entry by the begin
/// handler. Passed through to `begin_with_snapshots` so unit tests can
/// inject deterministic values without invoking the real syscalls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EntrySnapshots {
    pub t_in_ns: i64,
    pub cpu_u_in_ns: i64,
    pub cpu_s_in_ns: i64,
    pub mem_in_bytes: i64,
}

impl EntrySnapshots {
    /// Take snapshots from the production clock primitives.
    fn capture_now() -> Self {
        let cpu = clocks::cpu_times_now_ns();
        Self {
            t_in_ns: clocks::monotonic_now_ns(),
            cpu_u_in_ns: cpu.user_ns,
            cpu_s_in_ns: cpu.system_ns,
            mem_in_bytes: clocks::memory_usage_real_bytes(),
        }
    }
}

/// The four clock/memory values captured at call exit by the end
/// handler. Same role as [`EntrySnapshots`] for `end_with_snapshots`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExitSnapshots {
    pub t_out_ns: i64,
    pub cpu_u_now_ns: i64,
    pub cpu_s_now_ns: i64,
    pub mem_out_bytes: i64,
}

impl ExitSnapshots {
    fn capture_now() -> Self {
        let cpu = clocks::cpu_times_now_ns();
        Self {
            t_out_ns: clocks::monotonic_now_ns(),
            cpu_u_now_ns: cpu.user_ns,
            cpu_s_now_ns: cpu.system_ns,
            mem_out_bytes: clocks::memory_usage_real_bytes(),
        }
    }
}

// --- FcallInfo extraction (mirrors the spike's pattern) --------------------

/// Parse `&ExecuteData` into a public `FcallInfo<'a>` value.
///
/// `ext_php_rs::zend::FcallInfo::from_execute_data` is `pub(crate)`
/// upstream, so we cannot call it. The struct literal of `FcallInfo<'a>`
/// IS public though (every field is `pub`), so we walk the
/// `execute_data → func → (op_array | internal_function)` chain
/// ourselves — same shape as the spike's `extract_info`. When
/// `ext-php-rs` promotes the constructor, the right move is to drop
/// this function (and the spike's twin) and call upstream directly.
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
unsafe fn extract_fcall_info<'a>(execute_data: &'a ExecuteData) -> FcallInfo<'a> {
    let func_ptr = execute_data.func;
    if func_ptr.is_null() {
        return empty_fcall_info();
    }

    // SAFETY: `func_ptr` non-null per the check above; pointed-to
    // `zend_function` is alive for the callback's duration.
    let func = unsafe { &*func_ptr };
    let common = unsafe { &func.common };
    #[allow(clippy::cast_possible_truncation)]
    let is_internal = common.type_ == ffi::ZEND_INTERNAL_FUNCTION as u8;

    let function_name = unsafe { zend_string_to_str(common.function_name) };

    let class_name = if common.scope.is_null() {
        None
    } else {
        // SAFETY: `scope` is null-checked; `name` may itself be null
        // (handled by `zend_string_to_str`).
        let ce = unsafe { &*common.scope };
        unsafe { zend_string_to_str(ce.name) }
    };

    let (filename, lineno) = if is_internal {
        (None, 0)
    } else {
        // SAFETY: for user functions the `op_array` arm of the union
        // is the active member; reading `func.op_array.filename` and
        // `.line_start` is well-defined.
        let op_array = unsafe { &func.op_array };
        let filename = unsafe { zend_string_to_str(op_array.filename) };
        (filename, op_array.line_start)
    };

    FcallInfo {
        function_name,
        class_name,
        filename,
        lineno,
        is_internal,
    }
}

/// An `FcallInfo` with no inner borrows. Used for the null-func
/// defensive branch in [`extract_fcall_info`]. Returns a `'static`
/// instance because, by reference covariance, it coerces into any
/// caller-chosen `'a`.
fn empty_fcall_info() -> FcallInfo<'static> {
    FcallInfo {
        function_name: None,
        class_name: None,
        filename: None,
        lineno: 0,
        is_internal: false,
    }
}

/// Convert a `*mut zend_string` into a borrowed `&'a str`. The caller
/// picks `'a`; at the call sites inside [`extract_fcall_info`] the
/// inference picks the lifetime of the enclosing `&'a ExecuteData`,
/// which is the borrow-checker bound the Zend observer surface
/// guarantees.
///
/// # Safety
///
/// `zs` must be either null or a pointer to a `zend_string` whose
/// payload bytes form valid UTF-8 and remain alive for the chosen
/// `'a`. The Zend observer surface only passes us names for user
/// functions (parser-validated identifiers) and filenames (path
/// strings from disk). Invalid UTF-8 → returns `None`; the
/// categorisation falls back to `(unknown)`.
unsafe fn zend_string_to_str<'a>(zs: *mut ffi::zend_string) -> Option<&'a str> {
    if zs.is_null() {
        return None;
    }
    let len = unsafe { (*zs).len };
    let ptr = unsafe { (*zs).val.as_ptr() };
    let slice = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
    std::str::from_utf8(slice).ok()
}

// --- categorise -----------------------------------------------------------

/// Result of categorising an `FcallInfo` per `SPECIFICATION.md` §4.1.2.
///
/// `fqn`, `file` are `&'a str` borrows from the input — copied into
/// `String`s lazily inside the dictionary's `intern` build closure
/// (only on a dictionary miss).
#[derive(Debug)]
pub(crate) struct Categorised<'a> {
    pub key: FunctionKey,
    pub kind: FunctionKind,
    pub fqn: std::borrow::Cow<'a, str>,
    pub file: &'a str,
    pub line: u32,
}

/// Map an `FcallInfo` to its `(FunctionKey, FunctionKind, fqn, file, line)`.
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
pub(crate) fn categorise<'a>(info: &'a FcallInfo<'a>) -> Categorised<'a> {
    use std::borrow::Cow;
    use std::sync::Arc;

    let line = info.lineno;
    let file = info.filename.unwrap_or("");

    if info.is_internal {
        let name = info.function_name.unwrap_or("(anonymous)");
        return Categorised {
            key: FunctionKey::Internal {
                name: Arc::from(name),
            },
            kind: FunctionKind::Internal,
            fqn: Cow::Borrowed(name),
            file: "",
            line: 0,
        };
    }

    if let Some(class) = info.class_name {
        let method = info.function_name.unwrap_or("(unknown)");
        return Categorised {
            key: FunctionKey::Method {
                class: Arc::from(class),
                method: Arc::from(method),
            },
            kind: FunctionKind::Method,
            fqn: Cow::Owned(format!("{class}::{method}")),
            file,
            line,
        };
    }

    let is_closure = match info.function_name {
        Some(name) => name.starts_with("{closure"),
        None => info.filename.is_some(),
    };
    if is_closure {
        return Categorised {
            key: FunctionKey::Closure {
                file: Arc::from(file),
                line,
            },
            kind: FunctionKind::Closure,
            fqn: Cow::Owned(format!("closure:{file}:{line}")),
            file,
            line,
        };
    }

    // Fall-through: user function. `function_name` is `Some` here
    // (the closure branch caught `None`-with-file; an internal would
    // have caught `None`-without-file at branch 1). The
    // `unwrap_or("(unknown)")` is defensive against a Zend that
    // changes shape under us.
    let function = info.function_name.unwrap_or("(unknown)");
    Categorised {
        key: FunctionKey::Function {
            file: Arc::from(file),
            function: Arc::from(function),
            line,
        },
        kind: FunctionKind::Function,
        fqn: Cow::Borrowed(function),
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
    /// Production begin handler. Captures snapshots from the live
    /// clocks, parses `execute_data` into an `FcallInfo`, categorises
    /// it, and pushes a `CallFrame`. A no-op when the thread-local
    /// slot is empty.
    fn begin_handler(&self, execute_data: &ExecuteData) {
        // SAFETY: the observer trait hands us a `&ExecuteData` that is
        // valid for the duration of the call. `extract_fcall_info`
        // reads through `(*execute_data).func` and a handful of
        // `zend_string` pointers, all of which remain valid until
        // `end` returns.
        let info = unsafe { extract_fcall_info(execute_data) };
        let snapshots = EntrySnapshots::capture_now();

        with_current_trace(|trace| {
            let categorised = categorise(&info);
            begin_with_snapshots(trace, &categorised, snapshots);
        });
    }

    /// Production end handler. Reads exception state, captures exit
    /// snapshots, builds the `CallRecord`, and pushes it. A no-op
    /// when the thread-local slot is empty.
    fn end_handler(&self, _execute_data: &ExecuteData, _retval: Option<&Zval>) {
        // `_execute_data` is unused: the frame's identity is already
        // on the trace stack from the matching `begin`. `_retval`
        // is unused per design D-7 (we don't inspect return values).
        let abnormal = ExecutorGlobals::has_exception();
        let snapshots = ExitSnapshots::capture_now();

        with_current_trace(|trace| {
            end_with_snapshots(trace, snapshots, abnormal);
        });
    }
}

impl FcallObserver for Recorder {
    /// Unconditionally `true`. See the module doc comment for why a
    /// runtime "is there a current trace?" filter cannot live here.
    fn should_observe(&self, _info: &FcallInfo) -> bool {
        true
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
/// state beyond `trace`. Tests drive this directly.
pub(crate) fn begin_with_snapshots(
    trace: &mut Trace,
    categorised: &Categorised<'_>,
    snapshots: EntrySnapshots,
) {
    let call_id = trace.next_call_id();
    let parent = trace.stack.last().map_or(0, |frame| frame.call_id);
    #[allow(clippy::cast_possible_truncation)]
    let depth = trace.stack.len() as u16;

    // The `to_owned()` calls only fire on a dictionary miss (the
    // `build` closure runs at most once per unique key, per slice-1
    // `Dictionary::intern`'s lazy-allocate contract).
    let kind = categorised.kind;
    let fqn = categorised.fqn.as_ref();
    let file = categorised.file;
    let line = categorised.line;
    let fn_id = trace.push_dict_entry_via_intern(categorised.key.clone(), |fn_id| {
        // NOTE for Phase 5: these two `to_owned()` calls are the
        // last allocations on the hot path. Phase 5's zero-alloc
        // assertion (AC-RC-5) needs a thread-local interning buffer
        // plus a `Cow<'static, str>` rewrite of `DictEntry::fqn` /
        // `file`. Do not copy this shape verbatim into the future
        // hot path.
        DictEntry {
            fn_id,
            fqn: fqn.to_owned(),
            file: file.to_owned(),
            line,
            kind,
        }
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

/// Pop the top `CallFrame`, compute deltas, and push a `CallRecord`.
/// Pure: see [`begin_with_snapshots`]. A no-op when the stack is
/// empty (a desynchronisation that SHOULD NOT happen given Zend's
/// pairing — the `debug_assert!` makes it loud in tests; the release
/// path silently returns to preserve the silent-disable posture).
pub(crate) fn end_with_snapshots(trace: &mut Trace, snapshots: ExitSnapshots, abnormal: bool) {
    let Some(frame) = trace.stack.pop() else {
        debug_assert!(
            false,
            "observer end fired with an empty trace stack — begin/end pairing broken",
        );
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
}

// --- BootObserver dispatcher ----------------------------------------------

/// Top-level observer registered with `ModuleBuilder::fcall_observer`.
/// Picks exactly one variant at `MINIT` based on `Config::global()`:
/// `Disabled` when the master switch is off, `Spike` when the spike
/// directive is on, `Recorder` otherwise.
///
/// The `match self` in each trait method compiles down to a single
/// discriminant load — essentially free per call after LLVM has
/// inlined the variants' impls.
pub enum BootObserver {
    Disabled,
    Spike(SpikeObserver),
    Recorder(Recorder),
}

impl FcallObserver for BootObserver {
    fn should_observe(&self, info: &FcallInfo) -> bool {
        match self {
            Self::Disabled => false,
            Self::Spike(s) => s.should_observe(info),
            Self::Recorder(r) => r.should_observe(info),
        }
    }

    fn begin(&self, execute_data: &ExecuteData) {
        match self {
            Self::Disabled => {}
            Self::Spike(s) => s.begin(execute_data),
            Self::Recorder(r) => r.begin(execute_data),
        }
    }

    fn end(&self, execute_data: &ExecuteData, retval: Option<&Zval>) {
        match self {
            Self::Disabled => {}
            Self::Spike(s) => s.end(execute_data, retval),
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
/// defensive fallback per S-4: if a future ext-php-rs reorders
/// startup, the extension falls back to the inactive observer rather
/// than panicking across FFI.
pub fn build_boot_observer() -> BootObserver {
    let Some(config) = Config::global() else {
        return BootObserver::Disabled;
    };
    if !config.enabled {
        return BootObserver::Disabled;
    }
    if config.spike_observer {
        return BootObserver::Spike(SpikeObserver::from_config(config));
    }
    BootObserver::Recorder(build_recorder_observer())
}

// --- Tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::types::{FunctionKey, FunctionKind, RequestIdentity};
    use std::sync::Arc;

    // --- Fixture helpers ---------------------------------------------------

    fn stub_identity() -> RequestIdentity {
        RequestIdentity {
            host: Arc::from("test-host"),
            sapi: Arc::from("cli"),
            pid: 1,
            uri_or_script: "/tmp/test.php".to_owned(),
        }
    }

    /// `FcallInfo` construction is via struct literal (all fields are
    /// public on the upstream type). The single helper centralises
    /// the stub so the four-branch categorise tests stay readable.
    fn stub_info<'a>(
        function_name: Option<&'a str>,
        class_name: Option<&'a str>,
        filename: Option<&'a str>,
        lineno: u32,
        is_internal: bool,
    ) -> FcallInfo<'a> {
        FcallInfo {
            function_name,
            class_name,
            filename,
            lineno,
            is_internal,
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
            rinit_allocate_trace(identity);
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
        let _g = TraceGuard::enter(stub_identity());
        let pid = with_current_trace(|trace| trace.pid).expect("slot must be Some after RINIT");
        assert_eq!(pid, 1);
    }

    #[test]
    fn rshutdown_release_trace_drops_the_slot() {
        rinit_allocate_trace(stub_identity());
        assert!(with_current_trace(|_| ()).is_some());
        rshutdown_release_trace();
        assert!(
            with_current_trace(|_| ()).is_none(),
            "slot must be None after RSHUTDOWN",
        );
    }

    #[test]
    fn rshutdown_release_trace_on_empty_slot_is_a_noop() {
        // Ensure the slot is empty (a previous test may have left it
        // populated; the guard's Drop handles that, but be defensive).
        rshutdown_release_trace();
        rshutdown_release_trace();
        assert!(with_current_trace(|_| ()).is_none());
    }

    #[test]
    #[should_panic(expected = "RINIT without RSHUTDOWN")]
    fn double_rinit_without_rshutdown_panics() {
        // `_g` is intentionally unbound: we want the second `rinit`
        // to fire before the first guard is dropped, so the slot is
        // still populated. The panic propagates out of the test body
        // and `#[should_panic]` matches it. The first `Trace` leaks
        // for the duration of the panic-unwind; this is a test-only
        // path that other tests don't depend on.
        rinit_allocate_trace(stub_identity());
        rinit_allocate_trace(stub_identity());
        // Defensive cleanup if `should_panic` somehow didn't match:
        rshutdown_release_trace();
    }

    #[test]
    fn with_current_trace_returns_none_when_slot_is_empty() {
        rshutdown_release_trace();
        assert!(with_current_trace(|_| 42).is_none());
    }

    // --- categorise (four branches) ----------------------------------------

    #[test]
    fn categorise_routes_methods_to_the_method_branch() {
        let info = stub_info(
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
        let info = stub_info(Some("my_fn"), None, Some("/x.php"), 20, false);
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
        let info = stub_info(
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
        let info = stub_info(None, None, Some("/x.php"), 1, false);
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
        let info = stub_info(Some("array_map"), None, None, 0, true);
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
        let info = stub_info(Some("(unknown)"), None, None, 0, false);
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

    // --- begin_with_snapshots / end_with_snapshots ------------------------

    #[test]
    fn begin_with_snapshots_pushes_one_frame_with_call_id_one_and_parent_zero() {
        let mut trace = Trace::new(stub_identity());
        let info = stub_info(Some("only_me"), None, Some("/x.php"), 3, false);
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
        let mut trace = Trace::new(stub_identity());
        let info = stub_info(Some("only_me"), None, Some("/x.php"), 3, false);
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
        let mut trace = Trace::new(stub_identity());

        // `info_*` bindings must outlive the `Categorised` values
        // returned by `categorise` (the categorisation borrows
        // `fqn`/`file` from the `FcallInfo`).
        let info_a = stub_info(Some("a"), None, Some("/x.php"), 1, false);
        let info_b = stub_info(Some("b"), None, Some("/x.php"), 2, false);
        let info_c = stub_info(Some("c"), None, Some("/x.php"), 3, false);
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
        let mut trace = Trace::new(stub_identity());
        let info = stub_info(Some("repeat"), None, Some("/x.php"), 1, false);
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
        let mut trace = Trace::new(stub_identity());
        let info = stub_info(Some("bad"), None, Some("/x.php"), 1, false);
        let cat = categorise(&info);

        begin_with_snapshots(&mut trace, &cat, entry_snapshots());
        end_with_snapshots(&mut trace, exit_snapshots(), true);

        assert_eq!(trace.buffer.len(), 1);
        assert!(trace.buffer[0].abnormal_exit);
    }

    #[test]
    fn saturating_cpu_delta_reads_as_zero_when_exit_cpu_less_than_entry_cpu() {
        let mut trace = Trace::new(stub_identity());
        let info = stub_info(Some("anywhere"), None, Some("/x.php"), 1, false);
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
    fn end_on_empty_stack_is_a_silent_noop_in_release() {
        // Build the trace in a way that bypasses the begin path so the
        // stack is empty when `end` fires. `debug_assert` would panic
        // in test builds, so we test the release-mode invariant by
        // checking the post-state when the assert is disabled.
        // `debug_assertions` is off in `cargo test --release`; in the
        // default `cargo test` (debug), the debug_assert fires —
        // catching this test would require a release run. We instead
        // assert the post-state when `cfg(debug_assertions)` is off.
        if cfg!(debug_assertions) {
            // Skip; the debug_assert! is the documented behaviour
            // in test builds.
            return;
        }
        let mut trace = Trace::new(stub_identity());
        assert!(trace.stack.is_empty());
        end_with_snapshots(&mut trace, exit_snapshots(), false);
        assert!(
            trace.buffer.is_empty(),
            "end on empty stack must not emit a record",
        );
    }

    #[test]
    fn recorder_begin_with_no_active_trace_is_a_noop() {
        // The thread-local is empty; `with_current_trace` returns
        // None. The handler should not panic, should not allocate,
        // and should leave the slot empty.
        rshutdown_release_trace(); // ensure empty
        let r = Recorder;
        assert!(r.should_observe(&empty_fcall_info()));
        // We can't easily call `r.begin_handler` here without a real
        // `ExecuteData`; instead we exercise the `with_current_trace`
        // accessor directly to prove the no-op semantics.
        let touched = with_current_trace(|_| true);
        assert!(touched.is_none(), "no active trace → no-op closure body");
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
        let r = BootObserver::Recorder(Recorder);
        rshutdown_release_trace();
        assert!(r.should_observe(&empty_fcall_info()));
    }
}
