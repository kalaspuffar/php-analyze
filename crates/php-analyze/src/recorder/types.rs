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

use std::sync::Arc;
use std::time::Instant;

use crate::clocks;
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
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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
/// Mirrors `SPECIFICATION.md` §4.1.6 (`PendingBatch`) with one deliberate
/// deviation for this slice: there is no `drop_counter: Arc<AtomicU64>`
/// field. The drop counter is introduced by Phase-2 slice 3
/// (`recorder-depth-and-cap-drops`) when the depth and buffer-cap
/// enforcement paths that need to bump it land. The shipper does not
/// exist yet either (Phase 4), so the type is currently unused at
/// runtime; it ships now so the substrate is feature-complete for the
/// next slice's needs.
///
/// **Invariant**: `size_estimate` is always
/// [`estimate_batch_bytes`]`(&dict, &calls)`. Constructing a `PendingBatch`
/// directly bypasses that invariant, which is why slice-3 callers should
/// prefer the future `Trace::flush_into_batch` accessor (not yet present)
/// over the field-level constructor.
#[derive(Debug)]
pub struct PendingBatch {
    pub meta_partial: MetaPartial,
    pub dict: Vec<DictEntry>,
    pub calls: Vec<CallRecord>,
    pub size_estimate: usize,
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
/// `buffer_estimated_bytes`, `call_id_seq`) are `pub(crate)`, not `pub`.
/// External callers go through the accessor methods ([`push_record`],
/// [`push_dict_entry_via_intern`], [`next_call_id`]) so the invariant
/// can be enforced:
///
/// **Invariant**: `buffer_estimated_bytes` is always
/// [`estimate_batch_bytes`]`(&dictionary.new_entries, &buffer)`.
///
/// Slice 3 (`recorder-depth-and-cap-drops`) extends this invariant to
/// include the in-progress dictionary and to drive the `flush_bytes` /
/// `buffer_cap_bytes` thresholds. Phase-2 slice 2's observer wiring
/// touches the state only through the accessors below, so an
/// independently-evolved hot-path cannot desync the estimator from the
/// buffer contents.
///
/// [`push_record`]: Self::push_record
/// [`push_dict_entry_via_intern`]: Self::push_dict_entry_via_intern
/// [`next_call_id`]: Self::next_call_id
#[derive(Debug)]
pub struct Trace {
    pub trace_id: [u8; 16],
    pub start_time_realtime_ns: i64,
    pub host: Arc<str>,
    pub pid: u32,
    pub sapi: Arc<str>,
    pub uri_or_script: String,
    pub(crate) call_id_seq: u64,
    // Slice 2 (`recorder-observer-hooks-and-trace-lifecycle`) is the
    // first non-test reader: the begin handler pushes a `CallFrame` on
    // entry, the end handler pops on exit. Until then dead-code
    // analysis would flag the field — but removing it would force a
    // slice-2 type-shape change, which is exactly what this substrate
    // slice exists to avoid.
    #[allow(dead_code)]
    pub(crate) stack: Vec<CallFrame>,
    pub(crate) buffer: Vec<CallRecord>,
    pub(crate) dictionary: Dictionary,
    pub(crate) buffer_estimated_bytes: usize,
}

impl Trace {
    /// Construct a fresh trace at request start. The recorder calls this
    /// from `RINIT` in Phase-2 slice 2.
    ///
    /// `trace_id` is zero-initialised in this slice; UUID v7 generation
    /// arrives in Phase 4. `start_time_realtime_ns` is captured here, at
    /// construction, because the recorder needs the request anchor as
    /// early as possible and `clocks::realtime_now_ns` is cheap.
    pub fn new(host: Arc<str>, sapi: Arc<str>, pid: u32, uri_or_script: String) -> Self {
        Self {
            trace_id: [0; 16],
            start_time_realtime_ns: clocks::realtime_now_ns(),
            host,
            pid,
            sapi,
            uri_or_script,
            call_id_seq: 0,
            stack: Vec::new(),
            buffer: Vec::new(),
            dictionary: Dictionary::new(),
            buffer_estimated_bytes: 0,
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

    /// Push a completed `CallRecord` into the pending buffer and update
    /// the size-estimate by exactly the §3.2 per-record contribution.
    ///
    /// This is the only sanctioned way to grow `buffer`; going through
    /// the accessor keeps `buffer_estimated_bytes` aligned with the
    /// invariant documented on [`Trace`].
    pub fn push_record(&mut self, record: CallRecord) {
        self.buffer.push(record);
        self.buffer_estimated_bytes += CALL_RECORD_FIXED_BYTES;
    }

    /// Intern a function key and update the size-estimate by the §3.2
    /// per-dict-entry contribution on a dictionary miss. On a hit, the
    /// estimate is unchanged because no new `DictEntry` is staged.
    ///
    /// Mirrors [`Dictionary::intern`]'s lazy-allocate contract: the
    /// `build` closure runs at most once, only on a miss.
    pub fn push_dict_entry_via_intern(
        &mut self,
        key: FunctionKey,
        build: impl FnOnce(u32) -> DictEntry,
    ) -> u32 {
        let estimate = &mut self.buffer_estimated_bytes;
        self.dictionary.intern(key, |fn_id| {
            let entry = build(fn_id);
            *estimate += DICT_ENTRY_FIXED_BYTES + entry.fqn.len() + entry.file.len();
            entry
        })
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

    #[test]
    fn trace_new_produces_the_documented_initial_state() {
        let trace = Trace::new(
            Arc::from("host.example"),
            Arc::from("cli"),
            12345,
            "/path/to/script.php".to_owned(),
        );

        assert_eq!(trace.trace_id, [0u8; 16]);
        assert_eq!(trace.pid, 12345);
        assert_eq!(&*trace.host, "host.example");
        assert_eq!(&*trace.sapi, "cli");
        assert_eq!(trace.uri_or_script, "/path/to/script.php");
        assert_eq!(trace.call_id_seq, 0);
        assert!(trace.stack.is_empty(), "fresh stack must be empty");
        assert!(trace.buffer.is_empty(), "fresh buffer must be empty");
        assert_eq!(trace.buffer_estimated_bytes, 0);
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
        let mut trace = Trace::new(Arc::from("host"), Arc::from("cli"), 1, "/s.php".to_owned());
        let ids: Vec<u64> = (0..5).map(|_| trace.next_call_id()).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);
        assert_eq!(trace.call_id_seq, 5);
    }

    #[test]
    fn trace_push_record_appends_to_buffer_and_bumps_the_estimate_by_64() {
        let mut trace = Trace::new(Arc::from("host"), Arc::from("cli"), 1, "/s.php".to_owned());
        let before = trace.buffer_estimated_bytes;
        trace.push_record(sample_call_record());
        assert_eq!(trace.buffer.len(), 1, "buffer must hold the new record");
        assert_eq!(
            trace.buffer_estimated_bytes - before,
            CALL_RECORD_FIXED_BYTES,
            "estimate must grow by exactly the §3.2 per-record constant"
        );
    }

    #[test]
    fn trace_push_dict_entry_via_intern_bumps_estimate_only_on_a_miss() {
        let mut trace = Trace::new(Arc::from("host"), Arc::from("cli"), 1, "/s.php".to_owned());

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

        // Hit: estimate must not change, build closure must not run.
        let estimate_before_hit = trace.buffer_estimated_bytes;
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
}
