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
#[derive(Debug)]
pub struct PendingBatch {
    pub meta_partial: MetaPartial,
    pub dict: Vec<DictEntry>,
    pub calls: Vec<CallRecord>,
    pub size_estimate: usize,
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
#[derive(Debug)]
pub struct Trace {
    pub trace_id: [u8; 16],
    pub start_time_realtime_ns: i64,
    pub host: Arc<str>,
    pub pid: u32,
    pub sapi: Arc<str>,
    pub uri_or_script: String,
    pub call_id_seq: u64,
    pub stack: Vec<CallFrame>,
    pub buffer: Vec<CallRecord>,
    pub dictionary: Dictionary,
    pub buffer_estimated_bytes: usize,
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
