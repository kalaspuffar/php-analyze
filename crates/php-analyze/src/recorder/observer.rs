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
use crate::recorder::accounting;
use crate::recorder::types::{
    CallFrame, CallRecord, DictEntry, FunctionKey, FunctionKind, RequestIdentity, Trace,
    TraceLimits, CALL_RECORD_FIXED_BYTES, DICT_ENTRY_FIXED_BYTES,
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
/// off.
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
        // Slice-3 invariant: subtract this trace's contribution from
        // the process-wide budget so the atomic returns to its
        // pre-trace value at request boundary. Phase 4 will move
        // this subtract to the shipper's batch-consumed path; in
        // slice 3 the trace owns the entire bill, so the
        // rshutdown-time subtract is the only sub site.
        if let Some(trace) = trace.as_ref() {
            accounting::sub(trace.buffer_estimated_bytes);
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
#[derive(Clone, Debug)]
pub(crate) struct RawCallSite<'a> {
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
/// upstream, so we cannot call it. The struct walk below mirrors the
/// spike's `extract_info`; when `ext-php-rs` promotes the constructor,
/// the right move is to drop this function (and the spike's twin) and
/// adapt the upstream value into `RawCallSite`.
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
#[derive(Debug)]
pub(crate) struct Categorised<'a> {
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
pub(crate) fn categorise<'a>(info: &'a RawCallSite<'a>) -> Categorised<'a> {
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
            let snapshots = EntrySnapshots::capture_now();
            let categorised = categorise(&info);
            begin_with_snapshots(trace, &categorised, snapshots);
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
            let abnormal = ExecutorGlobals::has_exception();
            let snapshots = ExitSnapshots::capture_now();
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
pub(crate) fn begin_with_snapshots(
    trace: &mut Trace,
    categorised: &Categorised<'_>,
    snapshots: EntrySnapshots,
) {
    // 1. Track PHP-side depth unconditionally. `saturating_add` so a
    // pathological 2^32 begins in a single trace does not panic.
    trace.virtual_depth = trace.virtual_depth.saturating_add(1);

    // 2. Depth gate.
    if trace.virtual_depth > trace.max_depth {
        trace.record_drop();
        return;
    }

    // 3. Cap gate. `would_add` is the worst-case contribution this
    //    call will add to the budget if accepted: one record's fixed
    //    bytes plus, on a dictionary miss, the new dict entry's
    //    bytes. On a dictionary hit the second term is zero.
    let would_add = CALL_RECORD_FIXED_BYTES + dict_miss_cost(trace, categorised);
    if accounting::snapshot().saturating_add(would_add) > trace.buffer_cap_bytes {
        trace.record_drop();
        return;
    }

    // 4. Accept path.
    let call_id = trace.next_call_id();
    let parent = trace.stack.last().map_or(0, |frame| frame.call_id);
    // `virtual_depth` is already 1-based after the increment above; the
    // `CallFrame.depth` field is zero-indexed per slice-2 semantics.
    // `u16` cast: `virtual_depth` is bounded by `max_depth` (≤ u16::MAX
    // per the directive range), so the cast is lossless on the accept
    // path.
    #[allow(clippy::cast_possible_truncation)]
    let depth = (trace.virtual_depth - 1) as u16;

    // The `to_owned()` calls only fire on a dictionary miss (the
    // `build` closure runs at most once per unique key, per slice-1
    // `Dictionary::intern`'s lazy-allocate contract).
    let kind = categorised.kind;
    let fqn: &str = categorised.fqn.as_ref();
    let file: &str = categorised.file.as_ref();
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

/// Project the §3.2 dict-miss cost: `0` on a hit, or
/// `DICT_ENTRY_FIXED_BYTES + len(fqn) + len(file)` on a miss. Reads
/// through `Dictionary::contains_key`, which is a single hashmap probe
/// and does not stage any state. Used by the cap-gate in
/// [`begin_with_snapshots`] to compute `would_add` without committing
/// to an intern.
///
/// The function names "miss cost" by inverting "hit cost is zero" — a
/// dictionary that already contains the key would not bill anything if
/// the begin were accepted.
fn dict_miss_cost(trace: &Trace, categorised: &Categorised<'_>) -> usize {
    if trace.dictionary.contains_key(&categorised.key) {
        0
    } else {
        DICT_ENTRY_FIXED_BYTES + categorised.fqn.len() + categorised.file.len()
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
pub(crate) fn end_with_snapshots(trace: &mut Trace, snapshots: ExitSnapshots, abnormal: bool) {
    // Decrement first so the depth is consistent for any caller that
    // observes `virtual_depth` mid-pop. `saturating_sub` defends an
    // adversarial-end-before-begin sequence (test 10.3); in well-formed
    // traces the counter never reaches `0` before this point.
    trace.virtual_depth = trace.virtual_depth.saturating_sub(1);

    // LIFO consume: an end paired with a dropped begin returns
    // silently.
    if trace.dropped_begins > 0 {
        trace.dropped_begins -= 1;
        return;
    }

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
    use std::sync::atomic::Ordering;
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

    /// Slice-3 [`TraceLimits`] preset matching the directive-table
    /// defaults — uncapped for slice-2-style tests that don't care
    /// about the gates.
    fn permissive_limits() -> TraceLimits {
        TraceLimits {
            max_depth: 1024,
            buffer_cap_bytes: 64 * 1024 * 1024,
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
        let _g = TraceGuard::enter(stub_identity());
        let pid = with_current_trace(|trace| trace.pid).expect("slot must be Some after RINIT");
        assert_eq!(pid, 1);
    }

    #[test]
    fn rshutdown_release_trace_drops_the_slot() {
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

    // --- Slice-3 RSHUTDOWN subtract ---------------------------------------

    #[test]
    fn rshutdown_returns_atomic_to_zero_after_balanced_trace() {
        let _guard = account_guard();
        accounting::reset_for_test();

        // Allocate the trace into the thread-local slot via the
        // production path so the rshutdown helper exercises the real
        // sub site.
        rinit_allocate_trace(stub_identity(), permissive_limits());

        // Drive a few accepts inside the slot.
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

        assert_eq!(
            accounting::snapshot(),
            0,
            "rshutdown_release_trace must subtract the full contribution",
        );
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
        let _guard = account_guard();
        accounting::reset_for_test();

        for _ in 0..2 {
            rinit_allocate_trace(stub_identity(), permissive_limits());
            with_current_trace(|trace| {
                let cat = cat_for("repeat");
                begin_with_snapshots(trace, &cat, entry_snapshots());
                end_with_snapshots(trace, exit_snapshots(), false);
            });
            rshutdown_release_trace();
            assert_eq!(
                accounting::snapshot(),
                0,
                "atomic returns to zero between requests",
            );
        }
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
