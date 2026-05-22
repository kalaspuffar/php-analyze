//! In-memory data model for the recorder, mirroring `SPECIFICATION.md` §4.1.
//!
//! These types are owned by the PHP request thread for the lifetime of one
//! trace. They deliberately carry **no wire-encoding derives** — the §4.2
//! wire format uses different field names (`fn` vs `fn_id`, `t_in` vs
//! `t_in_ns`, …) and a different `FunctionKind` encoding (small int vs
//! string), and the conversion belongs to the future `wire.rs` module
//! (Phase 3). Adding the derives now would force this slice to commit to
//! wire names prematurely or to decorate every field with rename
//! attributes. See design.md §D-5 for the full rationale.
//!
//! The `recorder_types_module_does_not_derive_serde_serialize` test
//! enforces this contract by searching the file's own source for the
//! wire-derive name. The constraint is checked at `cargo test` time and
//! fails loudly if any future edit pulls the wire layer up into the
//! substrate.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::clocks;
use crate::recorder::accounting;
use crate::recorder::Dictionary;

/// Categorisation of a PHP function for dictionary-key purposes.
///
/// Mirrors `SPECIFICATION.md` §4.1.5. Encoded as a small int in the §4.2
/// wire format; here it stays a Rust enum for type safety.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FunctionKind {
    Function,
    Method,
    Closure,
    Internal,
}

/// Identity of a PHP function for interning purposes (§4.1.2).
///
/// `Arc<str>` is used for the string components so the same allocation
/// can be reused across many `FunctionKey` instances during a trace
/// (one per call site) and across the dictionary's internal hashmap.
///
/// `Closure` carries only `(file, line)` per design.md §OQ-T1 default:
/// two closures with the same declaration site are the same closure for
/// profiling purposes. If a future PHP fixture reveals a case where
/// closures-at-the-same-line need to be distinguished, the variant grows
/// a pointer field in a follow-up change.
///
/// `Hash` is implemented by hand (not derived) so that the bytes written
/// to the hasher match [`FunctionKeyRef`]'s implementation byte-for-byte
/// — required for the dictionary's `hashbrown::raw_entry` borrow-keyed
/// probe (recorder-hot-path-tuning D-1). The mirror property is
/// enforced by `function_key_and_ref_hash_identically` tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FunctionKey {
    Function {
        file: Arc<str>,
        function: Arc<str>,
        line: u32,
    },
    Method {
        class: Arc<str>,
        method: Arc<str>,
    },
    Closure {
        file: Arc<str>,
        line: u32,
    },
    Internal {
        name: Arc<str>,
    },
}

/// Borrowed view of a [`FunctionKey`] whose construction does not
/// allocate. Used by the recorder's hot path so the dict-hit branch
/// can probe `Dictionary` without paying for an owning key's
/// `Arc<str>` allocations on every call.
///
/// The variants and field order mirror [`FunctionKey`] exactly so the
/// `Hash` impls write the same bytes to the hasher and the
/// `FunctionKey::matches_ref` predicate compares structurally. The
/// `to_owned` conversion materialises the `Arc<str>`s — call it only
/// on a dictionary miss, never on the hit path.
#[derive(Clone, Copy, Debug)]
pub enum FunctionKeyRef<'a> {
    Function {
        file: &'a str,
        function: &'a str,
        line: u32,
    },
    Method {
        class: &'a str,
        method: &'a str,
    },
    Closure {
        file: &'a str,
        line: u32,
    },
    Internal {
        name: &'a str,
    },
}

/// Numeric variant tag used by both `Hash` impls so the bytes are
/// stable across `FunctionKey` ↔ `FunctionKeyRef`. The values match
/// the source order of the enum variants but are bound explicitly to
/// guard against a future reorder breaking hash interop.
const FN_KEY_TAG_FUNCTION: u8 = 0;
const FN_KEY_TAG_METHOD: u8 = 1;
const FN_KEY_TAG_CLOSURE: u8 = 2;
const FN_KEY_TAG_INTERNAL: u8 = 3;

impl std::hash::Hash for FunctionKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            FunctionKey::Function {
                file,
                function,
                line,
            } => {
                state.write_u8(FN_KEY_TAG_FUNCTION);
                <str as std::hash::Hash>::hash(file, state);
                <str as std::hash::Hash>::hash(function, state);
                state.write_u32(*line);
            }
            FunctionKey::Method { class, method } => {
                state.write_u8(FN_KEY_TAG_METHOD);
                <str as std::hash::Hash>::hash(class, state);
                <str as std::hash::Hash>::hash(method, state);
            }
            FunctionKey::Closure { file, line } => {
                state.write_u8(FN_KEY_TAG_CLOSURE);
                <str as std::hash::Hash>::hash(file, state);
                state.write_u32(*line);
            }
            FunctionKey::Internal { name } => {
                state.write_u8(FN_KEY_TAG_INTERNAL);
                <str as std::hash::Hash>::hash(name, state);
            }
        }
    }
}

impl std::hash::Hash for FunctionKeyRef<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            FunctionKeyRef::Function {
                file,
                function,
                line,
            } => {
                state.write_u8(FN_KEY_TAG_FUNCTION);
                <str as std::hash::Hash>::hash(*file, state);
                <str as std::hash::Hash>::hash(*function, state);
                state.write_u32(*line);
            }
            FunctionKeyRef::Method { class, method } => {
                state.write_u8(FN_KEY_TAG_METHOD);
                <str as std::hash::Hash>::hash(*class, state);
                <str as std::hash::Hash>::hash(*method, state);
            }
            FunctionKeyRef::Closure { file, line } => {
                state.write_u8(FN_KEY_TAG_CLOSURE);
                <str as std::hash::Hash>::hash(*file, state);
                state.write_u32(*line);
            }
            FunctionKeyRef::Internal { name } => {
                state.write_u8(FN_KEY_TAG_INTERNAL);
                <str as std::hash::Hash>::hash(*name, state);
            }
        }
    }
}

impl FunctionKey {
    /// Structural equality against a borrowed view. Used by the
    /// `Dictionary` borrow-keyed probe's predicate; returns `true`
    /// when the borrowed components would round-trip to `self`.
    #[inline]
    pub fn matches_ref(&self, key_ref: &FunctionKeyRef<'_>) -> bool {
        match (self, key_ref) {
            (
                FunctionKey::Function {
                    file: f1,
                    function: fn1,
                    line: l1,
                },
                FunctionKeyRef::Function {
                    file: f2,
                    function: fn2,
                    line: l2,
                },
            ) => &**f1 == *f2 && &**fn1 == *fn2 && l1 == l2,
            (
                FunctionKey::Method {
                    class: c1,
                    method: m1,
                },
                FunctionKeyRef::Method {
                    class: c2,
                    method: m2,
                },
            ) => &**c1 == *c2 && &**m1 == *m2,
            (
                FunctionKey::Closure { file: f1, line: l1 },
                FunctionKeyRef::Closure { file: f2, line: l2 },
            ) => &**f1 == *f2 && l1 == l2,
            (FunctionKey::Internal { name: n1 }, FunctionKeyRef::Internal { name: n2 }) => {
                &**n1 == *n2
            }
            _ => false,
        }
    }

    /// Borrow the owning key as a [`FunctionKeyRef`] without
    /// allocating. The reverse of `FunctionKeyRef::to_owned`.
    #[inline]
    pub fn as_ref(&self) -> FunctionKeyRef<'_> {
        match self {
            FunctionKey::Function {
                file,
                function,
                line,
            } => FunctionKeyRef::Function {
                file,
                function,
                line: *line,
            },
            FunctionKey::Method { class, method } => FunctionKeyRef::Method { class, method },
            FunctionKey::Closure { file, line } => FunctionKeyRef::Closure { file, line: *line },
            FunctionKey::Internal { name } => FunctionKeyRef::Internal { name },
        }
    }
}

impl FunctionKeyRef<'_> {
    /// Materialise the borrowed view into an owning [`FunctionKey`].
    /// Each string field becomes a fresh `Arc<str>` — call only on a
    /// dictionary miss, never on the hit path.
    #[inline]
    pub fn to_owned(self) -> FunctionKey {
        match self {
            FunctionKeyRef::Function {
                file,
                function,
                line,
            } => FunctionKey::Function {
                file: Arc::from(file),
                function: Arc::from(function),
                line,
            },
            FunctionKeyRef::Method { class, method } => FunctionKey::Method {
                class: Arc::from(class),
                method: Arc::from(method),
            },
            FunctionKeyRef::Closure { file, line } => FunctionKey::Closure {
                file: Arc::from(file),
                line,
            },
            FunctionKeyRef::Internal { name } => FunctionKey::Internal {
                name: Arc::from(name),
            },
        }
    }
}

/// Stack-local per-call state captured at call entry, popped at call exit.
///
/// Mirrors `SPECIFICATION.md` §4.1.3 verbatim — same field names, same
/// primitive types. The recorder hot path will push one of these on the
/// `Trace::stack` at every observer-begin handler invocation and pop it
/// at the matching end-handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallFrame {
    pub call_id: u64,
    pub parent: u64,
    pub fn_id: u32,
    pub depth: u16,
    pub t_in_ns: i64,
    pub cpu_u_in_ns: i64,
    pub cpu_s_in_ns: i64,
    pub mem_in_bytes: i64,
}

/// The metric record emitted at call exit. One per observed PHP call.
///
/// Mirrors `SPECIFICATION.md` §4.1.4. Note the field names are the
/// in-memory names (`t_in_ns`, `fn_id`, …); the wire shortenings
/// (`t_in`, `fn`, …) are applied by the future `wire.rs` encoder.
///
/// Not `Copy` — eleven fields including a `bool` make pass-by-value
/// large enough that the implicit copy cost outweighs the ergonomic
/// win. Pass by `&CallRecord` or move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallRecord {
    pub call_id: u64,
    pub parent: u64,
    pub fn_id: u32,
    pub depth: u16,
    pub t_in_ns: i64,
    pub t_out_ns: i64,
    pub cpu_u_ns: i64,
    pub cpu_s_ns: i64,
    pub mem_in_bytes: i64,
    pub mem_out_bytes: i64,
    pub abnormal_exit: bool,
}

/// A dictionary entry staged for inclusion in the next batch.
///
/// Mirrors `SPECIFICATION.md` §4.1.5. The strings are owned `String`s
/// (not `Arc<str>`) because the §4.2 wire encoder will hold them for the
/// duration of one batch and ownership transfer is the cleanest model;
/// Phase 3 may revisit if profiling shows churn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DictEntry {
    pub fn_id: u32,
    pub fqn: String,
    pub file: String,
    pub line: u32,
    pub kind: FunctionKind,
}

/// Per-trace identifying metadata, separable from the call data.
///
/// Mirrors `SPECIFICATION.md` §4.1.6 (`MetaPartial`). The `dropped_records`
/// field is **not** present here — it is stamped by the shipper at send
/// time by reading the `Arc<AtomicU64>` drop counter (AD-3). The drop
/// counter itself arrives in Phase-2 slice 3.
///
/// `trace_id` is `[u8; 16]` (16-byte raw UUID) rather than the `Uuid`
/// type because the `uuid` crate is not yet a dependency — UUID v7
/// generation lands in Phase 4 alongside the shipper. The byte
/// representation matches `Uuid::as_bytes()` so the future migration is
/// a one-line swap.
#[derive(Clone, Debug)]
pub struct MetaPartial {
    pub schema_version: u8,
    pub trace_id: [u8; 16],
    pub host: Arc<str>,
    pub pid: u32,
    pub start_time_realtime_ns: i64,
    pub sapi: Arc<str>,
    pub uri_or_script: Arc<str>,
}

/// A batch handed off from the recorder to the shipper.
///
/// Mirrors `SPECIFICATION.md` §4.1.6 (`PendingBatch`). Per AD-9 the
/// `drop_counter` field is an [`Arc::clone`] of the source [`Trace`]'s
/// counter — not a snapshot. The next-slice encoder reads the **live**
/// counter at send time, after the recorder has had the chance to bump
/// it via subsequent drops in the same trace; a snapshot would freeze
/// the value at flush time and lose any drops that occurred between
/// flush and encode.
///
/// **Invariant (estimator)**: `size_estimate` is always
/// [`estimate_batch_bytes`]`(&dict, &calls)`. Constructing a
/// `PendingBatch` directly bypasses that invariant, which is why callers
/// should prefer [`Trace::flush_into_pending_batch`] over the field-level
/// constructor. The accessor populates `size_estimate` from the trace's
/// running `buffer_estimated_bytes` and `debug_assert!`s the invariant
/// before returning.
#[derive(Debug)]
pub struct PendingBatch {
    pub meta_partial: MetaPartial,
    pub dict: Vec<DictEntry>,
    pub calls: Vec<CallRecord>,
    pub size_estimate: usize,
    /// Per-trace drop counter (`SPECIFICATION.md` AD-9), shared with
    /// the source [`Trace`] via [`Arc::clone`]. The shipper reads it
    /// at encode time (Phase 4 slice 3) to stamp
    /// `meta.dropped_records`; the recorder continues to bump it on
    /// the buffer-cap, depth-gate, and channel-full drop paths in the
    /// same trace until the trace ends.
    pub drop_counter: Arc<AtomicU64>,
}

/// `SPECIFICATION.md` §3.2 size-estimate constants.
///
/// The estimate is an over-approximation tuned to bound real-memory
/// headroom for the `flush_bytes` / `buffer_cap_bytes` thresholds. Exact
/// bytes are known only after wire encoding (shipper, Phase 3+).
///
/// Both constants are `pub(crate)` so the slice-3 invariant
/// (`buffer_estimated_bytes == estimate_batch_bytes(...)` for the current
/// pending contents) can be enforced from `Trace`'s accessor methods
/// without bleeding the magic numbers across the call sites.
pub(crate) const CALL_RECORD_FIXED_BYTES: usize = 64;
pub(crate) const DICT_ENTRY_FIXED_BYTES: usize = 24;

/// Size-estimate for a batch as specified by §3.2:
/// `64 bytes/call + (24 + len(fqn) + len(file)) per dict entry`.
///
/// Free function (not a method) so the same formula is reachable from
/// `Trace::push_record` / `push_dict_entry_via_intern` for the
/// incremental case and from `PendingBatch` construction for the
/// whole-batch case, without either having to know about the other.
pub fn estimate_batch_bytes(dict: &[DictEntry], calls: &[CallRecord]) -> usize {
    let dict_bytes: usize = dict
        .iter()
        .map(|entry| DICT_ENTRY_FIXED_BYTES + entry.fqn.len() + entry.file.len())
        .sum();
    dict_bytes + calls.len() * CALL_RECORD_FIXED_BYTES
}

/// Messages from the recorder to the shipper.
///
/// Mirrors `SPECIFICATION.md` §4.1.6. The channel itself is constructed
/// in Phase 4 alongside the shipper thread; this enum ships now so the
/// substrate matches the spec shape.
#[derive(Debug)]
pub enum ShipperMessage {
    Batch(PendingBatch),
    Drain { deadline: Instant },
}

/// Per-request identity values plumbed from `bootstrap::rinit` into
/// [`Trace::new`].
///
/// Collapses what used to be four positional parameters on `Trace::new`
/// (`host`, `sapi`, `pid`, `uri_or_script`) into one struct. Two
/// `Arc<str>` arguments sandwiching a `u32` made the original signature
/// vulnerable to a silent swap at the call site; named-field
/// construction at the only non-test caller (`bootstrap::rinit`)
/// removes the class of bug. See review finding RS-8 and slice-2
/// proposal §D-4 / `recorder-call-events` spec for the rationale.
#[derive(Debug, Clone)]
pub struct RequestIdentity {
    pub host: Arc<str>,
    pub sapi: Arc<str>,
    pub pid: u32,
    /// Carried as `Arc<str>` so the eventual `MetaPartial`-construction
    /// site (`Trace::flush_into_pending_batch`) clones the `Arc` rather
    /// than allocating a fresh slice per flush. The `Arc::from(&str)`
    /// allocation happens **once per request** at
    /// `bootstrap::request_identity_from_sapi`, paid alongside the
    /// `host` / `sapi` allocations, instead of once per flush. See
    /// review finding PF-1 in `COMMENTS.md`.
    pub uri_or_script: Arc<str>,
}

/// Per-trace cap and flush thresholds, plumbed from `Config` into
/// [`Trace::new`].
///
/// Slice 3 (`recorder-depth-and-cap-drops`) introduced this struct with
/// `max_depth` and `buffer_cap_bytes`; Phase-4 slice 2
/// (`recorder-flushes-into-shipper`) added the two flush thresholds so
/// the end-handler's hot-path predicate is a pair of field loads against
/// values already in cache from the preceding `push_record` write — no
/// indirection through `Config::global()`, no `usize` cast per call.
///
/// `max_depth` widens from `Config::max_depth: u16` to `u32` so the
/// comparison against `Trace::virtual_depth: u32` happens without a
/// cast on the hot path. The widening is lossless.
#[derive(Debug, Clone, Copy)]
pub struct TraceLimits {
    pub max_depth: u32,
    pub buffer_cap_bytes: usize,
    /// Cached `Config::flush_records` — flush the buffer once it
    /// reaches this many records. `usize` to match
    /// `Vec::len()` on the comparison.
    pub flush_records: usize,
    /// Cached `Config::flush_bytes` — flush the buffer once
    /// `buffer_estimated_bytes` reaches this many bytes. `usize` to
    /// match the estimator's own type.
    pub flush_bytes: usize,
}

impl From<&crate::config::Config> for TraceLimits {
    /// Build a `TraceLimits` from the resolved [`Config`]. Centralises
    /// the `u16 → u32` widening for `max_depth` and the
    /// `flush_records` / `flush_bytes` `usize` carry-through.
    fn from(config: &crate::config::Config) -> Self {
        Self {
            max_depth: u32::from(config.max_depth),
            buffer_cap_bytes: config.buffer_cap_bytes,
            flush_records: config.flush_records,
            flush_bytes: config.flush_bytes,
        }
    }
}

/// Per-request recorder state, owned by the PHP request thread.
///
/// Mirrors `SPECIFICATION.md` §4.1.2 with two implementation choices
/// documented in design.md:
///
/// - `stack: Vec<CallFrame>` rather than `SmallVec<[CallFrame; 64]>` —
///   the inline-capacity choice is a Phase-5 hot-path tuning concern
///   (design.md §D-7).
/// - `trace_id: [u8; 16]` placeholder — UUID v7 generation arrives in
///   Phase 4 (design.md §D-7 / OQ-T3 comment trail).
///
/// Per OQ-T2 default, `host`, `sapi`, and `pid` live on `Trace` rather
/// than only on `MetaPartial`; they are cheap to carry (Arc clones) and
/// the alternative would force the recorder to plumb them in at flush
/// time, which is the kind of error that survives review.
///
/// ## Field visibility and the size-estimate invariant
///
/// The mutable state fields (`stack`, `buffer`, `dictionary`,
/// `buffer_estimated_bytes`, `call_id_seq`, `virtual_depth`,
/// `dropped_begins`) are `pub(crate)`, not `pub`. External callers
/// go through the accessor methods ([`push_record`],
/// [`push_dict_entry_via_intern`], [`next_call_id`], [`record_drop`])
/// so the invariants can be enforced:
///
/// **Invariant (estimator)**: `buffer_estimated_bytes` is always
/// [`estimate_batch_bytes`]`(&dictionary.new_entries, &buffer)`.
///
/// **Invariant (LIFO pairing)**: `virtual_depth - dropped_begins ==
/// stack.len()` after every balanced begin/end pair. Equivalently,
/// every `record_drop` is matched by exactly one
/// `dropped_begins -= 1` inside `finish_call_record`'s LIFO consume
/// branch.
///
/// **Invariant (atomic budget)**: with the `accounting`
/// reset-for-test guard held and one trace alive,
/// `accounting::snapshot()` equals `buffer_estimated_bytes` after
/// every accept or drop, and returns to zero after the matching
/// `rshutdown_release_trace`.
///
/// Slice 2's observer wiring touches the state only through the
/// accessors below; slice 3 extends the accessors with the
/// per-trace `record_drop()` helper and routes the `push_record`
/// record-byte bill through [`accounting::add`] so the process-wide
/// budget stays consistent across concurrent FPM workers.
///
/// [`push_record`]: Self::push_record
/// [`push_dict_entry_via_intern`]: Self::push_dict_entry_via_intern
/// [`next_call_id`]: Self::next_call_id
/// [`record_drop`]: Self::record_drop
#[derive(Debug)]
pub struct Trace {
    pub trace_id: [u8; 16],
    pub start_time_realtime_ns: i64,
    pub host: Arc<str>,
    pub pid: u32,
    pub sapi: Arc<str>,
    /// `Arc<str>` so [`Self::flush_into_pending_batch`] can clone the
    /// `Arc` into [`MetaPartial::uri_or_script`] without allocating.
    /// The matching allocation happens once per request at
    /// `bootstrap::request_identity_from_sapi`. See PF-1 in
    /// `COMMENTS.md` for the worked rationale.
    pub uri_or_script: Arc<str>,

    /// Per-trace `Arc<AtomicU64>` drop counter (AD-9). Phase 4 clones
    /// the `Arc` into `PendingBatch::drop_counter` so the shipper
    /// reads it at encode time without re-entering the recorder. The
    /// counter is monotonic-increase from zero and is never
    /// decremented; cross-thread reads use `Ordering::Acquire`.
    pub drop_counter: Arc<AtomicU64>,

    pub(crate) call_id_seq: u64,
    pub(crate) stack: Vec<CallFrame>,
    pub(crate) buffer: Vec<CallRecord>,
    pub(crate) dictionary: Dictionary,
    pub(crate) buffer_estimated_bytes: usize,

    /// PHP-side call depth — incremented on every observed `begin`,
    /// decremented on every observed `end`, regardless of accept /
    /// drop. The depth gate compares this against [`max_depth`].
    /// `u32` ceiling is comfortably above `Config::max_depth`'s
    /// `u16` range; overflow is not a realistic concern.
    pub(crate) virtual_depth: u32,

    /// LIFO matcher for begin/end pairing through drops. Incremented
    /// when a begin is dropped; decremented when the matching end
    /// arrives. Per-thread LIFO is guaranteed by the Zend Engine, so
    /// a single counter suffices — no per-call state is needed.
    pub(crate) dropped_begins: u32,

    /// Cached threshold from `Config::max_depth`. Widened from `u16`
    /// to `u32` so the hot-path comparison with `virtual_depth`
    /// happens without a cast. Slice-3 design D-1.
    pub(crate) max_depth: u32,

    /// Cached threshold from `Config::buffer_cap_bytes`. Slice-3
    /// design D-3 / D-4.
    pub(crate) buffer_cap_bytes: usize,

    /// Cached threshold from `Config::flush_records`. The end-handler
    /// accept tail compares `buffer.len()` against this value;
    /// crossing the threshold drives a flush via
    /// `recorder::flush::try_send_batch`. See Phase-4 slice 2 design
    /// D-3 (flush at end-handler, not begin) and the matching
    /// `recorder-call-events` requirement.
    pub(crate) flush_records: usize,

    /// Cached threshold from `Config::flush_bytes`. Sibling of
    /// `flush_records`; the predicate is an `||` so either crossing
    /// fires the flush.
    pub(crate) flush_bytes: usize,
}

impl Trace {
    /// Construct a fresh trace at request start. The recorder calls this
    /// from `RINIT` in Phase-2 slice 2.
    ///
    /// `trace_id` is zero-initialised in this slice; UUID v7 generation
    /// arrives in Phase 4. `start_time_realtime_ns` is captured here, at
    /// construction, because the recorder needs the request anchor as
    /// early as possible and `clocks::realtime_now_ns` is cheap.
    ///
    /// `limits` carries the cached `max_depth` and `buffer_cap_bytes`
    /// thresholds (slice 3 design D-1 / D-3). A fresh `Arc<AtomicU64>`
    /// drop counter is constructed here, per AD-9 — every trace gets
    /// its own counter so cross-trace contamination is impossible.
    pub fn new(identity: RequestIdentity, limits: TraceLimits) -> Self {
        let RequestIdentity {
            host,
            sapi,
            pid,
            uri_or_script,
        } = identity;
        Self {
            trace_id: [0; 16],
            start_time_realtime_ns: clocks::realtime_now_ns(),
            host,
            pid,
            sapi,
            uri_or_script,
            drop_counter: Arc::new(AtomicU64::new(0)),
            call_id_seq: 0,
            stack: Vec::new(),
            buffer: Vec::new(),
            dictionary: Dictionary::new(),
            buffer_estimated_bytes: 0,
            virtual_depth: 0,
            dropped_begins: 0,
            max_depth: limits.max_depth,
            buffer_cap_bytes: limits.buffer_cap_bytes,
            flush_records: limits.flush_records,
            flush_bytes: limits.flush_bytes,
        }
    }

    /// Allocate the next `call_id`. Call IDs are monotonic from 1 within
    /// a trace; `0` is the "no parent" sentinel per `SPECIFICATION.md`
    /// §4.1.2 and never returned by this method.
    ///
    /// `checked_add` matches the dictionary's `fn_id` overflow stance:
    /// 2^64 calls in a single trace is unreachable, but the contract
    /// should not depend on workload.
    pub fn next_call_id(&mut self) -> u64 {
        self.call_id_seq = self
            .call_id_seq
            .checked_add(1)
            .expect("call_id counter overflowed u64 — 2^64 calls in a single trace");
        self.call_id_seq
    }

    /// Push a completed `CallRecord` into the pending buffer.
    ///
    /// This is the only sanctioned way to grow `buffer`; going through
    /// the accessor keeps `buffer_estimated_bytes` aligned with the
    /// estimator invariant documented on [`Trace`].
    ///
    /// ## Billing split (slice-3 design D-3)
    ///
    /// Slice 3 routes the per-record `CALL_RECORD_FIXED_BYTES`
    /// contribution through both the per-trace estimator AND the
    /// process-wide [`accounting`] atomic. The dict-miss portion of
    /// a call's budget is billed at `begin` time (inside
    /// [`push_dict_entry_via_intern`]) so the cap-gate has the most
    /// pessimistic projection for any **single** call considered in
    /// isolation; the record portion is billed at `end` time (here).
    ///
    /// ### Known imprecision: nested-overshoot under unbalanced LIFO
    ///
    /// The cap-gate at `begin` reads `accounting::snapshot()` and
    /// projects `would_add = CALL_RECORD_FIXED_BYTES +
    /// dict_miss_cost`. Under nested calls, multiple in-flight
    /// `begin`s can all see the same pre-bump snapshot and all
    /// accept; the matching `end`s then each bill
    /// `CALL_RECORD_FIXED_BYTES` sequentially. The post-`end` budget
    /// therefore overshoots `buffer_cap_bytes` by up to
    /// `(in_flight - 1) * CALL_RECORD_FIXED_BYTES` per trace
    /// (bounded by `max_depth * CALL_RECORD_FIXED_BYTES` in the
    /// worst case).
    ///
    /// This is acceptable per `SPECIFICATION.md` §3.2 which frames
    /// `buffer_cap_bytes` as a **soft target** — the cap gates new
    /// admissions, it does not retroactively reject already-running
    /// calls. The cumulative effect across traces is zero because
    /// each trace's contribution is subtracted in
    /// [`rshutdown_release_trace`]. The alternative (bill-at-begin,
    /// per-record) was considered and rejected because it
    /// complicates the rshutdown subtract — slice 3 deliberately
    /// keeps the per-trace estimator and the process-wide atomic in
    /// lockstep, with the per-record contribution landing on both
    /// sides at the same point in time (here, at `end`). See
    /// `COMMENTS.md` DCR-2 review note for the worked example.
    pub fn push_record(&mut self, record: CallRecord) {
        self.buffer.push(record);
        self.buffer_estimated_bytes += CALL_RECORD_FIXED_BYTES;
        accounting::add(CALL_RECORD_FIXED_BYTES);
    }

    /// Intern a function key and update the size-estimate by the §3.2
    /// per-dict-entry contribution on a dictionary miss. On a hit, the
    /// estimate is unchanged because no new `DictEntry` is staged.
    ///
    /// Mirrors [`Dictionary::intern`]'s lazy-allocate contract: the
    /// `build` closure runs at most once, only on a miss.
    ///
    /// On a miss, the dict-entry portion of the §3.2 estimator is
    /// added to **both** the per-trace `buffer_estimated_bytes`
    /// **and** the process-wide [`accounting::BYTES_IN_MEMORY`]
    /// atomic so the cap-gate's projection stays consistent across
    /// concurrent FPM workers (slice-3 design D-3 / D-4).
    pub fn push_dict_entry_via_intern(
        &mut self,
        key: FunctionKey,
        build: impl FnOnce(u32) -> DictEntry,
    ) -> u32 {
        let estimate = &mut self.buffer_estimated_bytes;
        self.dictionary.intern(key, |fn_id| {
            let entry = build(fn_id);
            let contribution = DICT_ENTRY_FIXED_BYTES + entry.fqn.len() + entry.file.len();
            *estimate += contribution;
            accounting::add(contribution);
            entry
        })
    }

    /// Borrow-keyed sibling of [`push_dict_entry_via_intern`]: probes
    /// the dictionary by a borrowed [`FunctionKeyRef`] (zero
    /// `Arc<str>` allocations on the hit path) and only invokes
    /// `build` on a miss. On a miss the build closure materialises
    /// both the owning `FunctionKey` (the dictionary's key) and the
    /// staged `DictEntry` in one place — the same Arc<str> family
    /// can back both if the caller chooses.
    ///
    /// Returns the `fn_id` only; the hit/miss split is collapsed
    /// because the §3.2 cap-gate has already projected the miss cost
    /// via [`crate::recorder::Dictionary::contains_key_ref`] before
    /// calling this method.
    ///
    /// **Zero-alloc on hit**: the recorder hot path's binding test is
    /// `recorder-zero-alloc-audit` (recorder-hot-path-tuning §6).
    pub fn push_dict_entry_via_intern_ref<F>(
        &mut self,
        key_ref: &crate::recorder::types::FunctionKeyRef<'_>,
        build: F,
    ) -> u32
    where
        F: FnOnce(u32) -> (FunctionKey, DictEntry),
    {
        let estimate = &mut self.buffer_estimated_bytes;
        let (fn_id, _outcome) = self.dictionary.intern_ref(key_ref, |fn_id| {
            let (owning_key, entry) = build(fn_id);
            let contribution = DICT_ENTRY_FIXED_BYTES + entry.fqn.len() + entry.file.len();
            *estimate += contribution;
            accounting::add(contribution);
            (owning_key, entry)
        });
        fn_id
    }

    /// Pre-grow `self.buffer` and `self.stack` to the requested
    /// capacities so subsequent `push_record` / observer-driven
    /// stack pushes within those bounds are pointer-bump only
    /// (no reallocation). Used by the `recorder-zero-alloc-audit`
    /// regression test (`tests/recorder_zero_alloc.rs`) and the
    /// recorder microbenches under `crates/php-analyze/benches/`
    /// to establish the "steady state" precondition AC-RC-5
    /// requires.
    ///
    /// Production code does NOT call this — `Trace::new` starts
    /// with empty `Vec`s and they grow organically as records
    /// land. This method exists solely for the bench / audit
    /// surface; the alternative (promoting `buffer` and `stack`
    /// fields to `pub`) was rejected to keep their visibility
    /// scoped to the recorder module's internal callers.
    pub fn pregrow_for_audit(&mut self, buffer_capacity: usize, stack_capacity: usize) {
        self.buffer.reserve(buffer_capacity);
        self.stack.reserve(stack_capacity);
    }

    /// Record a dropped begin: bump the `Arc<AtomicU64>` drop counter
    /// (shared with the future shipper) and increment the LIFO
    /// `dropped_begins` matcher (consumed by `finish_call_record`).
    ///
    /// Centralises the two-step drop so the begin-gate call sites
    /// (depth gate, cap gate) stay readable. `Ordering::Relaxed` on
    /// the atomic increment is sufficient because the only
    /// cross-thread reader is Phase 4's shipper, which will use
    /// `Ordering::Acquire` on the load side and is published through
    /// the channel-send happens-before edge anyway.
    pub(crate) fn record_drop(&mut self) {
        self.drop_counter.fetch_add(1, Ordering::Relaxed);
        self.dropped_begins = self.dropped_begins.saturating_add(1);
    }

    /// Hand the current buffer and new-since-last-flush dictionary
    /// entries to a fresh [`PendingBatch`], resetting the trace's
    /// pending state in-place. Called by the Phase-4 producer paths
    /// (threshold-driven flush at the end-handler tail, `RSHUTDOWN`
    /// final flush). See design.md §D-2 / §D-6.
    ///
    /// Steady-state cost: two `Vec::new()` swaps (`mem::take`), one
    /// `Arc::clone` (atomic increment), one `MetaPartial` construct.
    /// **No allocations on this path** — the `Vec`-and-`Arc::clone`
    /// triple is constant-time; the `MetaPartial` carries three
    /// `Arc<str>` clones (`host`, `sapi`, `uri_or_script`), not owned
    /// strings. PF-1 (`COMMENTS.md`) records the round-1 fix that
    /// lifted `Trace::uri_or_script` from `String` to `Arc<str>` so
    /// the third clone is also a refcount bump rather than a fresh
    /// `Arc::from(&str)` allocation per flush.
    ///
    /// // NOTE for Phase 5 (AC-RC-5 zero-alloc audit): this is the
    /// // only non-Drop hot path that produces a `PendingBatch`. The
    /// // zero-alloc property rests on `std::mem::take` returning the
    /// // sentinel `Vec::new()` for both fields and on `Arc::clone`
    /// // being a single relaxed atomic increment. Phase 5 may want
    /// // to pre-size the post-take `Vec`s via
    /// // `Vec::with_capacity(flush_records)` to avoid the first
    /// // post-flush push's `malloc`; see design.md §D-2 for the
    /// // trade-off.
    ///
    /// The dictionary's interning map is **preserved** by
    /// [`Dictionary::take_new_entries`] — a function that fired one
    /// `CallRecord` into batch N continues to map to the same
    /// `fn_id` for any subsequent record that targets it; only the
    /// staged `new_entries` journal moves. The next batch's `dict`
    /// vec therefore lists only entries that became visible *after*
    /// this flush, matching the §4.2 incremental wire shape.
    pub(crate) fn flush_into_pending_batch(&mut self) -> PendingBatch {
        // Capture the running estimate *before* the take so the
        // returned `PendingBatch.size_estimate` matches what the
        // recorder billed to `accounting::BYTES_IN_MEMORY`. The
        // estimator invariant (§D-2) is re-checked via
        // `debug_assert_eq!` below.
        let size_estimate = self.buffer_estimated_bytes;

        let calls = std::mem::take(&mut self.buffer);
        let dict = self.dictionary.take_new_entries();
        self.buffer_estimated_bytes = 0;

        let meta_partial = MetaPartial {
            schema_version: 1,
            trace_id: self.trace_id,
            host: Arc::clone(&self.host),
            pid: self.pid,
            start_time_realtime_ns: self.start_time_realtime_ns,
            sapi: Arc::clone(&self.sapi),
            uri_or_script: Arc::clone(&self.uri_or_script),
        };

        let batch = PendingBatch {
            meta_partial,
            dict,
            calls,
            size_estimate,
            drop_counter: Arc::clone(&self.drop_counter),
        };

        // Estimator invariant: the running `buffer_estimated_bytes`
        // matches what the §3.2 formula would compute from the moved
        // contents. Held in `debug_assert_eq!` so release builds skip
        // the recomputation but tests catch any drift between the
        // incremental bookkeeping in `push_record` /
        // `push_dict_entry_via_intern` and the whole-batch formula.
        debug_assert_eq!(
            batch.size_estimate,
            estimate_batch_bytes(&batch.dict, &batch.calls),
            "estimator drift: buffer_estimated_bytes diverged from estimate_batch_bytes",
        );

        batch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `CallFrame` with arbitrary but legal values for use in
    /// tests that only need to bind field types.
    fn sample_call_frame() -> CallFrame {
        CallFrame {
            call_id: 1,
            parent: 0,
            fn_id: 1,
            depth: 1,
            t_in_ns: 1_000_000,
            cpu_u_in_ns: 500,
            cpu_s_in_ns: 100,
            mem_in_bytes: 1024,
        }
    }

    fn sample_call_record() -> CallRecord {
        CallRecord {
            call_id: 1,
            parent: 0,
            fn_id: 1,
            depth: 1,
            t_in_ns: 1_000_000,
            t_out_ns: 2_000_000,
            cpu_u_ns: 500,
            cpu_s_ns: 100,
            mem_in_bytes: 1024,
            mem_out_bytes: 2048,
            abnormal_exit: false,
        }
    }

    #[test]
    fn call_frame_carries_every_field_named_in_spec_4_1_3_with_the_named_types() {
        let frame = sample_call_frame();
        let _: u64 = frame.call_id;
        let _: u64 = frame.parent;
        let _: u32 = frame.fn_id;
        let _: u16 = frame.depth;
        let _: i64 = frame.t_in_ns;
        let _: i64 = frame.cpu_u_in_ns;
        let _: i64 = frame.cpu_s_in_ns;
        let _: i64 = frame.mem_in_bytes;
    }

    #[test]
    fn call_record_carries_every_field_named_in_spec_4_1_4_with_the_named_types() {
        let record = sample_call_record();
        let _: u64 = record.call_id;
        let _: u64 = record.parent;
        let _: u32 = record.fn_id;
        let _: u16 = record.depth;
        let _: i64 = record.t_in_ns;
        let _: i64 = record.t_out_ns;
        let _: i64 = record.cpu_u_ns;
        let _: i64 = record.cpu_s_ns;
        let _: i64 = record.mem_in_bytes;
        let _: i64 = record.mem_out_bytes;
        let _: bool = record.abnormal_exit;
    }

    #[test]
    fn function_kind_has_exactly_four_variants_matching_spec_4_1_5() {
        // An exhaustive match without a wildcard arm fails to compile if
        // a new variant is added without updating this test, and also
        // catches a renamed variant.
        let kinds = [
            FunctionKind::Function,
            FunctionKind::Method,
            FunctionKind::Closure,
            FunctionKind::Internal,
        ];
        for kind in kinds {
            let _label = match kind {
                FunctionKind::Function => "function",
                FunctionKind::Method => "method",
                FunctionKind::Closure => "closure",
                FunctionKind::Internal => "internal",
            };
        }
    }

    /// Build a `RequestIdentity` from string literals. Centralises the
    /// boilerplate so tests stay readable; slice-2-and-later callers
    /// fill in `Trace::new`'s first argument.
    fn sample_identity(host: &str, sapi: &str, pid: u32, uri_or_script: &str) -> RequestIdentity {
        RequestIdentity {
            host: Arc::from(host),
            sapi: Arc::from(sapi),
            pid,
            uri_or_script: Arc::from(uri_or_script),
        }
    }

    /// Slice-3 [`TraceLimits`] preset matching the directive-table
    /// defaults from `config.rs` — a tall depth ceiling and a 64-MiB
    /// budget — so tests that don't care about the gates inherit the
    /// "uncapped" behaviour the slice-2 tests had before slice 3.
    pub(super) fn permissive_limits() -> TraceLimits {
        TraceLimits {
            max_depth: 1024,
            buffer_cap_bytes: 64 * 1024 * 1024,
            // Phase-4 slice 2: thresholds large enough that the slice-3
            // tests, which predate the flush predicate, never cross
            // either of them. Tests that exercise the flush cadence
            // build their own `TraceLimits` with smaller values.
            flush_records: usize::MAX,
            flush_bytes: usize::MAX,
        }
    }

    /// Build a `Trace` with permissive limits — the slice-3 shorthand
    /// that keeps the slice-2 test bodies short while still exercising
    /// the new `Trace::new` signature.
    pub(super) fn trace_with(identity: RequestIdentity) -> Trace {
        Trace::new(identity, permissive_limits())
    }

    /// Acquire the slice-3 accounting test-lock for the duration of a
    /// test that touches the process-wide budget. Callers also call
    /// `accounting::reset_for_test()` after acquiring.
    pub(super) fn account_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::recorder::accounting::acquire_test_lock()
    }

    #[test]
    fn trace_new_produces_the_documented_initial_state() {
        let _guard = account_guard();
        accounting::reset_for_test();

        let trace = trace_with(sample_identity(
            "host.example",
            "cli",
            12345,
            "/path/to/script.php",
        ));

        assert_eq!(trace.trace_id, [0u8; 16]);
        assert_eq!(trace.pid, 12345);
        assert_eq!(&*trace.host, "host.example");
        assert_eq!(&*trace.sapi, "cli");
        assert_eq!(&*trace.uri_or_script, "/path/to/script.php");
        assert_eq!(trace.call_id_seq, 0);
        assert!(trace.stack.is_empty(), "fresh stack must be empty");
        assert!(trace.buffer.is_empty(), "fresh buffer must be empty");
        assert_eq!(trace.buffer_estimated_bytes, 0);
        assert_eq!(trace.virtual_depth, 0);
        assert_eq!(trace.dropped_begins, 0);
        assert!(
            trace.start_time_realtime_ns > 0,
            "start_time_realtime_ns must be populated from CLOCK_REALTIME"
        );

        // The dictionary is fresh: interning any first key returns 1.
        let mut dict = trace.dictionary;
        let id = dict.intern(
            FunctionKey::Internal {
                name: Arc::from("strlen"),
            },
            |fn_id| DictEntry {
                fn_id,
                fqn: "internal:strlen".to_owned(),
                file: String::new(),
                line: 0,
                kind: FunctionKind::Internal,
            },
        );
        assert_eq!(id, 1, "a fresh dictionary must start fn_ids at 1");
    }

    #[test]
    fn trace_next_call_id_is_monotonic_from_one() {
        let mut trace = trace_with(sample_identity("host", "cli", 1, "/s.php"));
        let ids: Vec<u64> = (0..5).map(|_| trace.next_call_id()).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);
        assert_eq!(trace.call_id_seq, 5);
    }

    #[test]
    fn trace_push_record_appends_to_buffer_and_bumps_the_estimate_by_64() {
        let _guard = account_guard();
        accounting::reset_for_test();

        let mut trace = trace_with(sample_identity("host", "cli", 1, "/s.php"));
        let before = trace.buffer_estimated_bytes;
        trace.push_record(sample_call_record());
        assert_eq!(trace.buffer.len(), 1, "buffer must hold the new record");
        assert_eq!(
            trace.buffer_estimated_bytes - before,
            CALL_RECORD_FIXED_BYTES,
            "estimate must grow by exactly the §3.2 per-record constant"
        );
        assert_eq!(
            accounting::snapshot(),
            CALL_RECORD_FIXED_BYTES,
            "push_record must also bill the process-wide atomic (slice-3 D-3)",
        );
    }

    #[test]
    fn trace_push_dict_entry_via_intern_bumps_estimate_only_on_a_miss() {
        let _guard = account_guard();
        accounting::reset_for_test();

        let mut trace = trace_with(sample_identity("host", "cli", 1, "/s.php"));

        let key = FunctionKey::Internal {
            name: Arc::from("strlen"),
        };

        // Miss: estimate grows by `24 + len("internal:strlen") + len("")`.
        let estimate_before_miss = trace.buffer_estimated_bytes;
        let fqn = "internal:strlen".to_owned();
        let file = String::new();
        let expected_dict_contribution = DICT_ENTRY_FIXED_BYTES + fqn.len() + file.len();
        let first = trace.push_dict_entry_via_intern(key.clone(), |fn_id| DictEntry {
            fn_id,
            fqn: fqn.clone(),
            file: file.clone(),
            line: 0,
            kind: FunctionKind::Internal,
        });
        assert_eq!(first, 1, "first miss assigns fn_id 1");
        assert_eq!(
            trace.buffer_estimated_bytes - estimate_before_miss,
            expected_dict_contribution,
            "miss must grow estimate by the §3.2 per-dict-entry formula"
        );
        assert_eq!(
            accounting::snapshot(),
            expected_dict_contribution,
            "miss must also bill the process-wide atomic (slice-3 D-4)",
        );

        // Hit: estimate must not change, build closure must not run.
        let estimate_before_hit = trace.buffer_estimated_bytes;
        let snapshot_before_hit = accounting::snapshot();
        let mut build_ran = false;
        let second = trace.push_dict_entry_via_intern(key, |fn_id| {
            build_ran = true;
            DictEntry {
                fn_id,
                fqn: "should-not-build".to_owned(),
                file: String::new(),
                line: 0,
                kind: FunctionKind::Internal,
            }
        });
        assert_eq!(second, first, "hit returns the existing fn_id");
        assert!(!build_ran, "build closure must not run on a hit");
        assert_eq!(
            trace.buffer_estimated_bytes, estimate_before_hit,
            "hit must leave the estimate unchanged"
        );
        assert_eq!(
            accounting::snapshot(),
            snapshot_before_hit,
            "hit must leave the process-wide atomic unchanged",
        );
    }

    #[test]
    fn estimate_batch_bytes_matches_the_spec_3_2_formula() {
        let calls = vec![sample_call_record(), sample_call_record()];
        let dict = vec![
            DictEntry {
                fn_id: 1,
                fqn: "ns\\foo".to_owned(),
                file: "/a.php".to_owned(),
                line: 1,
                kind: FunctionKind::Function,
            },
            DictEntry {
                fn_id: 2,
                fqn: "C::m".to_owned(),
                file: String::new(),
                line: 0,
                kind: FunctionKind::Method,
            },
        ];

        let expected = 2 * CALL_RECORD_FIXED_BYTES
            + (DICT_ENTRY_FIXED_BYTES + "ns\\foo".len() + "/a.php".len())
            + (DICT_ENTRY_FIXED_BYTES + "C::m".len());
        assert_eq!(estimate_batch_bytes(&dict, &calls), expected);

        // Empty inputs collapse to zero — the §3.2 formula has no
        // constant offset beyond what each entry contributes.
        assert_eq!(estimate_batch_bytes(&[], &[]), 0);
    }

    #[test]
    fn request_identity_round_trips_through_trace_new() {
        // Slice-2 contract: every `RequestIdentity` field surfaces on
        // the returned `Trace` unchanged. The test deliberately uses
        // values distinct from the slice-1 baseline so a copy-paste
        // regression that hard-coded the old defaults would be caught.
        let identity = RequestIdentity {
            host: Arc::from("worker-42.prod"),
            sapi: Arc::from("fpm-fcgi"),
            pid: 4242,
            uri_or_script: Arc::from("/srv/app/index.php?route=/api/v1/users"),
        };
        let trace = Trace::new(identity.clone(), permissive_limits());

        assert_eq!(&*trace.host, &*identity.host);
        assert_eq!(&*trace.sapi, &*identity.sapi);
        assert_eq!(trace.pid, identity.pid);
        assert_eq!(&*trace.uri_or_script, &*identity.uri_or_script);
    }

    // --- Slice-3 tests ------------------------------------------------

    #[test]
    fn trace_new_initialises_drop_counter_to_zero_and_arc_is_unique_per_call() {
        let trace_a = trace_with(sample_identity("h", "cli", 1, "/a.php"));
        assert_eq!(
            trace_a.drop_counter.load(Ordering::Acquire),
            0,
            "fresh drop counter must read zero",
        );

        let trace_b = trace_with(sample_identity("h", "cli", 1, "/b.php"));

        // Mutate `trace_a`'s counter; `trace_b`'s must stay zero.
        trace_a.drop_counter.fetch_add(7, Ordering::Relaxed);
        assert_eq!(trace_a.drop_counter.load(Ordering::Acquire), 7);
        assert_eq!(
            trace_b.drop_counter.load(Ordering::Acquire),
            0,
            "second trace's counter must be independent (AD-9)",
        );

        // The Arc itself is distinct, not a clone of the same allocation.
        assert!(
            !Arc::ptr_eq(&trace_a.drop_counter, &trace_b.drop_counter),
            "each Trace::new must allocate a fresh Arc<AtomicU64>",
        );
    }

    #[test]
    fn trace_new_initialises_virtual_depth_and_dropped_begins_to_zero() {
        let trace = trace_with(sample_identity("h", "cli", 1, "/a.php"));
        assert_eq!(trace.virtual_depth, 0);
        assert_eq!(trace.dropped_begins, 0);
    }

    #[test]
    fn trace_new_caches_max_depth_and_buffer_cap_bytes_from_request_limits() {
        let limits = TraceLimits {
            max_depth: 42,
            buffer_cap_bytes: 99,
            flush_records: usize::MAX,
            flush_bytes: usize::MAX,
        };
        let trace = Trace::new(sample_identity("h", "cli", 1, "/a.php"), limits);
        assert_eq!(trace.max_depth, 42);
        assert_eq!(trace.buffer_cap_bytes, 99);
    }

    #[test]
    fn trace_new_caches_flush_thresholds_from_request_limits() {
        let limits = TraceLimits {
            max_depth: 1024,
            buffer_cap_bytes: 64 * 1024 * 1024,
            flush_records: 5000,
            flush_bytes: 524_288,
        };
        let trace = Trace::new(sample_identity("h", "cli", 1, "/a.php"), limits);
        assert_eq!(trace.flush_records, 5000);
        assert_eq!(trace.flush_bytes, 524_288);
    }

    #[test]
    fn trace_limits_from_config_carries_flush_records_and_flush_bytes() {
        // Build a `Config` with non-default flush thresholds via the
        // public `from_ini_values` path so the test exercises the same
        // resolution code the bootstrap layer runs at MINIT.
        let raw = crate::config::RawIni {
            enabled: Some(true),
            server_url: Some("https://ingest.example.com/v1/ingest".to_owned()),
            auth_token: Some("inline-token".to_owned()),
            flush_records: Some(5000),
            flush_bytes: Some(524_288),
            ..crate::config::RawIni::default()
        };
        let (config, _warnings) = crate::config::Config::from_ini_values(&raw);
        assert!(config.enabled, "the test config must be enabled");
        assert_eq!(config.flush_records, 5000);
        assert_eq!(config.flush_bytes, 524_288);

        let limits = TraceLimits::from(&config);
        assert_eq!(limits.flush_records, 5000);
        assert_eq!(limits.flush_bytes, 524_288);
        // u16 → u32 widening — verified once so the `From` impl is
        // covered end-to-end for max_depth too.
        assert_eq!(limits.max_depth, u32::from(config.max_depth));
        assert_eq!(limits.buffer_cap_bytes, config.buffer_cap_bytes);
    }

    #[test]
    fn record_drop_bumps_both_counter_and_dropped_begins() {
        let mut trace = trace_with(sample_identity("h", "cli", 1, "/a.php"));
        let counter_before = trace.drop_counter.load(Ordering::Acquire);
        let lifo_before = trace.dropped_begins;

        trace.record_drop();

        assert_eq!(
            trace.drop_counter.load(Ordering::Acquire),
            counter_before + 1,
            "Arc<AtomicU64> drop counter must bump by 1",
        );
        assert_eq!(
            trace.dropped_begins,
            lifo_before + 1,
            "LIFO matcher must bump by 1",
        );

        // Three more drops should yield exactly three more increments
        // on both counters — confirming the centralisation invariant.
        trace.record_drop();
        trace.record_drop();
        trace.record_drop();
        assert_eq!(
            trace.drop_counter.load(Ordering::Acquire),
            counter_before + 4
        );
        assert_eq!(trace.dropped_begins, lifo_before + 4);
    }

    #[test]
    fn two_consecutive_traces_have_independent_arc_drop_counters() {
        // Sanity check on AD-9: even after the previous trace bumps
        // its counter heavily, the next allocation must start fresh.
        let trace_one = trace_with(sample_identity("h", "cli", 1, "/a.php"));
        for _ in 0..50 {
            trace_one.drop_counter.fetch_add(1, Ordering::Relaxed);
        }
        assert_eq!(trace_one.drop_counter.load(Ordering::Acquire), 50);

        let trace_two = trace_with(sample_identity("h", "cli", 1, "/a.php"));
        assert_eq!(
            trace_two.drop_counter.load(Ordering::Acquire),
            0,
            "AD-9: per-trace Arc<AtomicU64> must not inherit previous trace's count",
        );
        assert!(
            !Arc::ptr_eq(&trace_one.drop_counter, &trace_two.drop_counter),
            "the Arc itself must be a fresh allocation, not a clone",
        );
    }

    // --- Phase-4 slice 2: flush_into_pending_batch -------------------------

    /// Stage a buffer + dictionary state on a fresh trace by interning
    /// `dict_entries` distinct functions and pushing `record_count`
    /// `CallRecord`s through the `Trace`-side accessors so the
    /// estimator invariant holds throughout. Returns the trace; the
    /// caller MUST hold an `account_guard()` because the helper
    /// touches the process-wide budget.
    fn trace_with_staged_buffer(record_count: usize, dict_entries: usize) -> Trace {
        let mut trace = trace_with(sample_identity("h", "cli", 1, "/x.php"));
        for i in 0..dict_entries {
            let name = format!("fn_{i}");
            let key = FunctionKey::Internal {
                name: Arc::from(name.as_str()),
            };
            let fqn_owned = format!("internal:{name}");
            trace.push_dict_entry_via_intern(key, |fn_id| DictEntry {
                fn_id,
                fqn: fqn_owned,
                file: String::new(),
                line: 0,
                kind: FunctionKind::Internal,
            });
        }
        for i in 0..record_count {
            trace.push_record(CallRecord {
                call_id: (i as u64) + 1,
                ..sample_call_record()
            });
        }
        trace
    }

    #[test]
    fn flush_into_pending_batch_moves_buffer_and_dict_new_entries() {
        let _guard = account_guard();
        accounting::reset_for_test();

        let mut trace = trace_with_staged_buffer(3, 2);
        let pre_buffer_bytes = trace.buffer_estimated_bytes;
        assert!(
            pre_buffer_bytes > 0,
            "test fixture must produce a non-zero estimate"
        );

        let batch = trace.flush_into_pending_batch();

        assert_eq!(batch.calls.len(), 3, "calls move into the batch");
        assert_eq!(batch.dict.len(), 2, "dict-new-entries move into the batch");
        assert!(trace.buffer.is_empty(), "trace buffer is reset");
        assert_eq!(
            trace.buffer_estimated_bytes, 0,
            "trace buffer_estimated_bytes is reset",
        );

        // The dictionary's interning map is preserved across the flush:
        // re-interning the same key returns the existing `fn_id` and
        // stages no fresh `DictEntry`.
        let key = FunctionKey::Internal {
            name: Arc::from("fn_0"),
        };
        assert!(
            trace.dictionary.contains_key(&key),
            "interning map MUST survive the flush",
        );
    }

    #[test]
    fn flush_into_pending_batch_resets_buffer_estimated_bytes() {
        let _guard = account_guard();
        accounting::reset_for_test();

        let mut trace = trace_with_staged_buffer(5, 1);
        let pre_estimate = trace.buffer_estimated_bytes;
        let batch = trace.flush_into_pending_batch();

        assert_eq!(trace.buffer_estimated_bytes, 0);
        assert_eq!(
            batch.size_estimate, pre_estimate,
            "size_estimate equals the pre-take estimator value",
        );
        // The §3.2 whole-batch formula matches the incremental
        // accumulator — also asserted by `flush_into_pending_batch`'s
        // own `debug_assert_eq!`, but kept here as a release-build
        // check.
        assert_eq!(
            batch.size_estimate,
            estimate_batch_bytes(&batch.dict, &batch.calls),
        );
    }

    #[test]
    fn flush_into_pending_batch_preserves_the_dictionary_map_across_flushes() {
        let _guard = account_guard();
        accounting::reset_for_test();

        let mut trace = trace_with_staged_buffer(1, 1);
        let first = trace.flush_into_pending_batch();
        assert_eq!(first.dict.len(), 1, "first flush carries the dict entry");

        // Re-intern the same key: should NOT add a new entry to the
        // trace's `dictionary.new_entries`. We confirm by inspecting
        // the next flush, which would otherwise carry a duplicate.
        let key = FunctionKey::Internal {
            name: Arc::from("fn_0"),
        };
        let fqn_owned = "internal:fn_0".to_owned();
        let _fn_id = trace.push_dict_entry_via_intern(key, |fn_id| DictEntry {
            fn_id,
            fqn: fqn_owned,
            file: String::new(),
            line: 0,
            kind: FunctionKind::Internal,
        });
        trace.push_record(sample_call_record());

        let second = trace.flush_into_pending_batch();
        assert!(
            second.dict.is_empty(),
            "second flush carries no dict entries — the function was already interned",
        );
        assert_eq!(second.calls.len(), 1, "the one record moves through");
    }

    #[test]
    fn pending_batch_drop_counter_is_arc_clone_of_trace_counter() {
        let _guard = account_guard();
        accounting::reset_for_test();

        let mut trace = trace_with(sample_identity("h", "cli", 1, "/x.php"));
        // Stage one record so the flush produces a non-empty batch
        // (a zero-record flush is exercised separately).
        trace.push_record(sample_call_record());

        let batch = trace.flush_into_pending_batch();

        // The Arc is the same allocation.
        assert!(
            Arc::ptr_eq(&batch.drop_counter, &trace.drop_counter),
            "PendingBatch::drop_counter must be Arc::clone of trace.drop_counter, not a snapshot",
        );

        // A bump through the trace is visible through the batch's
        // clone — the slice-2 design promise that the encoder reads
        // the live counter, not a snapshot.
        trace.drop_counter.fetch_add(7, Ordering::Release);
        assert_eq!(batch.drop_counter.load(Ordering::Acquire), 7);

        // And a bump through the batch's clone is visible through
        // the trace — symmetry of the Arc.
        batch.drop_counter.fetch_add(2, Ordering::Release);
        assert_eq!(trace.drop_counter.load(Ordering::Acquire), 9);
    }

    #[test]
    fn two_flushes_from_the_same_trace_share_the_drop_counter_arc() {
        let _guard = account_guard();
        accounting::reset_for_test();

        let mut trace = trace_with(sample_identity("h", "cli", 1, "/x.php"));
        trace.push_record(sample_call_record());
        let batch_a = trace.flush_into_pending_batch();
        trace.push_record(sample_call_record());
        let batch_b = trace.flush_into_pending_batch();

        assert!(
            Arc::ptr_eq(&batch_a.drop_counter, &batch_b.drop_counter),
            "both batches from the same trace must share the Arc",
        );
        assert!(
            Arc::ptr_eq(&batch_a.drop_counter, &trace.drop_counter),
            "and share with the source trace",
        );
    }

    #[test]
    fn recorder_types_module_does_not_derive_serde_serialize() {
        // The substrate slice deliberately defers `serde` to Phase 3.
        // This test reads its own source file and asserts the absence
        // of the wire-derive name — a derive, an impl, or even a future
        // contributor's `use serde::...`. A grep-style test is the
        // simplest way to enforce a negative architectural constraint at
        // `cargo test` time.
        //
        // The search string is built at runtime so this test's own
        // source does not satisfy its own grep. The message uses the
        // bracketed form `[serde-derive-name]` for the same reason.
        let needle = format!("{}{}", "Seri", "alize");
        let source = include_str!("types.rs");
        assert!(
            !source.contains(&needle),
            "recorder::types must not mention the wire-derive name \
             `[serde-derive-name]` — wire encoding belongs to Phase 3's \
             wire.rs (design.md §D-5)"
        );
    }

    // --- FunctionKey ↔ FunctionKeyRef interop -----------------------------

    /// Hash a single value with `FxBuildHasher` and return the finished
    /// u64. Used to prove `FunctionKey` and `FunctionKeyRef` produce
    /// identical hashes — required for the `Dictionary` borrow-keyed
    /// probe to find an owning entry inserted under the same identity.
    fn fx_hash_one<T: std::hash::Hash>(value: &T) -> u64 {
        use std::hash::BuildHasher;
        rustc_hash::FxBuildHasher.hash_one(value)
    }

    #[test]
    fn function_key_and_ref_hash_identically_for_the_function_variant() {
        let owned = FunctionKey::Function {
            file: Arc::from("/a/b.php"),
            function: Arc::from("noop"),
            line: 42,
        };
        let borrowed = FunctionKeyRef::Function {
            file: "/a/b.php",
            function: "noop",
            line: 42,
        };
        assert_eq!(fx_hash_one(&owned), fx_hash_one(&borrowed));
    }

    #[test]
    fn function_key_and_ref_hash_identically_for_the_method_variant() {
        let owned = FunctionKey::Method {
            class: Arc::from("Ns\\Cls"),
            method: Arc::from("doThing"),
        };
        let borrowed = FunctionKeyRef::Method {
            class: "Ns\\Cls",
            method: "doThing",
        };
        assert_eq!(fx_hash_one(&owned), fx_hash_one(&borrowed));
    }

    #[test]
    fn function_key_and_ref_hash_identically_for_the_closure_variant() {
        let owned = FunctionKey::Closure {
            file: Arc::from("/a/b.php"),
            line: 17,
        };
        let borrowed = FunctionKeyRef::Closure {
            file: "/a/b.php",
            line: 17,
        };
        assert_eq!(fx_hash_one(&owned), fx_hash_one(&borrowed));
    }

    #[test]
    fn function_key_and_ref_hash_identically_for_the_internal_variant() {
        let owned = FunctionKey::Internal {
            name: Arc::from("strlen"),
        };
        let borrowed = FunctionKeyRef::Internal { name: "strlen" };
        assert_eq!(fx_hash_one(&owned), fx_hash_one(&borrowed));
    }

    #[test]
    fn function_key_matches_ref_returns_true_for_structurally_equal_views() {
        let cases: &[(FunctionKey, FunctionKeyRef<'_>)] = &[
            (
                FunctionKey::Function {
                    file: Arc::from("/x.php"),
                    function: Arc::from("f"),
                    line: 1,
                },
                FunctionKeyRef::Function {
                    file: "/x.php",
                    function: "f",
                    line: 1,
                },
            ),
            (
                FunctionKey::Method {
                    class: Arc::from("C"),
                    method: Arc::from("m"),
                },
                FunctionKeyRef::Method {
                    class: "C",
                    method: "m",
                },
            ),
            (
                FunctionKey::Closure {
                    file: Arc::from("/x.php"),
                    line: 5,
                },
                FunctionKeyRef::Closure {
                    file: "/x.php",
                    line: 5,
                },
            ),
            (
                FunctionKey::Internal {
                    name: Arc::from("array_map"),
                },
                FunctionKeyRef::Internal { name: "array_map" },
            ),
        ];
        for (owned, borrowed) in cases {
            assert!(
                owned.matches_ref(borrowed),
                "expected {owned:?} to match {borrowed:?}"
            );
        }
    }

    #[test]
    fn function_key_matches_ref_returns_false_for_cross_variant_or_mismatched_components() {
        let owned = FunctionKey::Function {
            file: Arc::from("/x.php"),
            function: Arc::from("f"),
            line: 1,
        };
        // Cross-variant.
        assert!(!owned.matches_ref(&FunctionKeyRef::Internal { name: "f" }));
        // Same variant, different line.
        assert!(!owned.matches_ref(&FunctionKeyRef::Function {
            file: "/x.php",
            function: "f",
            line: 2,
        }));
        // Same variant, different function name.
        assert!(!owned.matches_ref(&FunctionKeyRef::Function {
            file: "/x.php",
            function: "g",
            line: 1,
        }));
    }

    #[test]
    fn function_key_ref_round_trips_through_to_owned_and_back() {
        let cases = [
            FunctionKey::Function {
                file: Arc::from("/a.php"),
                function: Arc::from("f"),
                line: 1,
            },
            FunctionKey::Method {
                class: Arc::from("C"),
                method: Arc::from("m"),
            },
            FunctionKey::Closure {
                file: Arc::from("/a.php"),
                line: 5,
            },
            FunctionKey::Internal {
                name: Arc::from("array_map"),
            },
        ];
        for original in &cases {
            let borrowed = original.as_ref();
            let round_tripped = borrowed.to_owned();
            assert_eq!(*original, round_tripped);
            assert!(original.matches_ref(&borrowed));
            assert_eq!(fx_hash_one(original), fx_hash_one(&borrowed));
        }
    }
}
