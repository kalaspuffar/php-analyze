//! MessagePack encoder for `PendingBatch` values.
//!
//! `shipper::encode::encode_batch` is the only call site that
//! translates the recorder-side `PendingBatch` into the wire-side
//! [`wire::Batch`] and serialises it via `rmp_serde::to_vec_named`. It
//! is a pure free function on the shipper thread; the `run_loop`'s
//! consume step calls it once per drained batch (see slice's
//! `design.md` §D-1).
//!
//! ## Live drop-counter read (AD-9)
//!
//! `meta.dropped_records` is stamped from
//! `batch.drop_counter.load(Ordering::Acquire)` **at encode time**, not
//! at flush time. The counter is an `Arc<AtomicU64>` shared with the
//! source `Trace`, so any drops that occurred between
//! `Trace::flush_into_pending_batch` and the encode (channel-full
//! drops on subsequent flushes, buffer-cap drops on subsequent
//! `begin` calls, retry-exhaust drops on prior batches from the same
//! trace) all surface on the next batch's wire `meta.dropped_records`.
//! This is the documented clone-not-snapshot semantic of `AD-9`
//! (`SPECIFICATION.md` §4.1.6).
//!
//! ## Field-name shortenings
//!
//! The recorder's in-memory `CallRecord` uses `_ns` / `_bytes`
//! suffixes (`t_in_ns`, `mem_in_bytes`); the wire form drops those
//! suffixes (`t_in`, `mem_in`). The mapping lives here, at this exact
//! boundary, so a reader of `wire::CallRecord`'s `#[serde(rename)]`
//! attributes plus this conversion function sees the entire
//! recorder→wire field-name story in two adjacent places.

// The encoder is wired into `run_loop`'s consume step by task 6.x of
// the same OpenSpec change. Until that lands in the same branch, every
// symbol here is reachable only from this module's own `#[cfg(test)]`
// block; the `#[allow(dead_code)]` below opens the gate for `cargo
// clippy -- -D warnings` to pass on the intermediate commit and is
// removed by the §6 rewiring.
#![allow(dead_code)]

use std::sync::atomic::Ordering;

use crate::recorder::types::{self as rec, PendingBatch};
use crate::wire;

/// Errors that the encoder can surface to its caller. Only the
/// serialisation arm is fallible today; the conversion arm is
/// infallible by construction (every field of the wire `Batch` has a
/// total mapping from the recorder side). If the conversion ever
/// grows a fallible step (e.g. a UUID v7 parse), it lands as a new
/// variant here.
#[derive(Debug)]
pub(crate) enum EncodeError {
    /// `rmp_serde::to_vec_named` returned an error. Wraps the
    /// upstream error to preserve `Display` for the `E_NOTICE` line.
    Serialisation(rmp_serde::encode::Error),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serialisation(err) => write!(f, "rmp_serde::to_vec_named failed: {err}"),
        }
    }
}

impl std::error::Error for EncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Serialisation(err) => Some(err),
        }
    }
}

/// Convert a `PendingBatch` to a `wire::Batch` and serialise it via
/// `rmp_serde::to_vec_named`. The returned `Vec<u8>` is what the
/// shipper's HTTP step uploads as the POST body.
///
/// Exactly one atomic `Acquire` read of `batch.drop_counter` happens
/// per call — see the module docstring for the AD-9 rationale.
///
/// This function does NOT panic. Encode failures return
/// `EncodeError::Serialisation`; the caller logs one `E_NOTICE` and
/// drops the batch (no retry — the same input would fail again).
pub(crate) fn encode_batch(batch: &PendingBatch) -> Result<Vec<u8>, EncodeError> {
    let wire_batch = batch_to_wire(batch);
    rmp_serde::to_vec_named(&wire_batch).map_err(EncodeError::Serialisation)
}

/// Pure conversion `PendingBatch` → [`wire::Batch`]. Factored out of
/// [`encode_batch`] so a unit test can assert the shape of the wire
/// value without paying the MessagePack round-trip.
fn batch_to_wire(batch: &PendingBatch) -> wire::Batch {
    let dropped_records = batch.drop_counter.load(Ordering::Acquire);
    wire::Batch {
        meta: meta_partial_to_wire(&batch.meta_partial, dropped_records),
        dict: batch.dict.iter().map(dict_entry_to_wire).collect(),
        calls: batch.calls.iter().map(call_record_to_wire).collect(),
    }
}

/// Stamp `meta.dropped_records` and render the `[u8; 16]` `trace_id`
/// as a 36-character hyphenated UUID string. The wire schema (§4.2.1)
/// says `trace_id` is a string; the recorder side keeps it as raw
/// bytes for allocation-free in-memory handling. This is the single
/// site that pays the `to_string` cost.
fn meta_partial_to_wire(meta: &rec::MetaPartial, dropped_records: u64) -> wire::MetaFull {
    wire::MetaFull {
        schema_version: meta.schema_version,
        trace_id: uuid::Uuid::from_bytes(meta.trace_id).to_string(),
        host: meta.host.to_string(),
        pid: meta.pid,
        start_time: meta.start_time_realtime_ns,
        sapi: meta.sapi.to_string(),
        uri_or_script: meta.uri_or_script.to_string(),
        dropped_records,
    }
}

/// Recorder-side `DictEntry` → wire-side `DictEntry`. The two structs
/// are nominally identical (same field names, same types) except for
/// the `kind: FunctionKind` field which is two distinct enums; the
/// match below maps between them.
fn dict_entry_to_wire(entry: &rec::DictEntry) -> wire::DictEntry {
    wire::DictEntry {
        fn_id: entry.fn_id,
        fqn: entry.fqn.clone(),
        file: entry.file.clone(),
        line: entry.line,
        kind: function_kind_to_wire(entry.kind),
    }
}

/// Recorder-side `CallRecord` → wire-side `CallRecord`. Applies the
/// §4.2.3 field-name shortenings (`t_in_ns → t_in`, `mem_in_bytes →
/// mem_in`, …) at this exact site; the wire struct's
/// `#[serde(rename = "fn")]` annotation handles `fn_id → fn`.
fn call_record_to_wire(call: &rec::CallRecord) -> wire::CallRecord {
    wire::CallRecord {
        call_id: call.call_id,
        parent: call.parent,
        fn_id: call.fn_id,
        depth: call.depth,
        t_in: call.t_in_ns,
        t_out: call.t_out_ns,
        cpu_u: call.cpu_u_ns,
        cpu_s: call.cpu_s_ns,
        mem_in: call.mem_in_bytes,
        mem_out: call.mem_out_bytes,
        abnormal_exit: call.abnormal_exit,
    }
}

/// Translate the recorder-side `FunctionKind` to the wire-side
/// equivalent. The two enums are structurally identical but live in
/// different modules (the recorder layer should not depend on the
/// wire layer); the match below is the boundary.
fn function_kind_to_wire(kind: rec::FunctionKind) -> wire::FunctionKind {
    match kind {
        rec::FunctionKind::Function => wire::FunctionKind::Function,
        rec::FunctionKind::Method => wire::FunctionKind::Method,
        rec::FunctionKind::Closure => wire::FunctionKind::Closure,
        rec::FunctionKind::Internal => wire::FunctionKind::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::types::{
        CallRecord as RecCallRecord, DictEntry as RecDictEntry, MetaPartial, PendingBatch,
    };
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    /// Build a `PendingBatch` with `dropped` already stamped on the
    /// shared counter, one dict entry, and `calls_len` call records.
    /// Returns the batch and a `clone` of the counter so a test can
    /// re-bump it post-construction to observe the live-read semantic.
    fn fixture_batch(dropped: u64, calls_len: usize) -> (PendingBatch, Arc<AtomicU64>) {
        let drop_counter = Arc::new(AtomicU64::new(dropped));
        let meta_partial = MetaPartial {
            schema_version: 1,
            trace_id: [
                0x01, 0x8f, 0x32, 0xa1, 0xb2, 0xc3, 0x70, 0x00, 0x80, 0x12, 0x34, 0x56, 0x78, 0x9a,
                0xbc, 0xde,
            ],
            host: Arc::from("test-host"),
            pid: 4242,
            start_time_realtime_ns: 1_700_000_000_000_000_000,
            sapi: Arc::from("cli"),
            uri_or_script: Arc::from("/path/to/script.php"),
        };
        let dict = vec![RecDictEntry {
            fn_id: 1,
            fqn: "noop".to_owned(),
            file: "/path/to/script.php".to_owned(),
            line: 7,
            kind: rec::FunctionKind::Function,
        }];
        let calls = (0..calls_len)
            .map(|i| RecCallRecord {
                call_id: (i as u64) + 1,
                parent: 0,
                fn_id: 1,
                depth: 0,
                t_in_ns: 1_000_000 + i as i64,
                t_out_ns: 2_000_000 + i as i64,
                cpu_u_ns: 100,
                cpu_s_ns: 50,
                mem_in_bytes: 1024,
                mem_out_bytes: 1024,
                abnormal_exit: false,
            })
            .collect();
        let batch = PendingBatch {
            meta_partial,
            dict,
            calls,
            size_estimate: 1024,
            drop_counter: Arc::clone(&drop_counter),
        };
        (batch, drop_counter)
    }

    #[test]
    fn encode_batch_round_trips_through_rmp_serde() {
        let (batch, _counter) = fixture_batch(0, 2);
        let bytes = encode_batch(&batch).expect("encode succeeds for a well-formed batch");
        let decoded: wire::Batch =
            rmp_serde::from_slice(&bytes).expect("encoded bytes decode as a wire::Batch");
        assert_eq!(decoded.meta.schema_version, 1);
        assert_eq!(decoded.dict.len(), 1);
        assert_eq!(decoded.calls.len(), 2);
        assert_eq!(decoded.meta.dropped_records, 0);
        assert_eq!(decoded.meta.pid, 4242);
    }

    #[test]
    fn encode_batch_stamps_meta_dropped_records_from_live_counter() {
        // The counter reads `3` at encode time → wire.meta.dropped_records = 3.
        let (batch, _counter) = fixture_batch(3, 1);
        let bytes = encode_batch(&batch).unwrap();
        let decoded: wire::Batch = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(
            decoded.meta.dropped_records, 3,
            "encoder must read the live drop_counter at encode time (AD-9)",
        );
    }

    #[test]
    fn encode_batch_observes_post_flush_counter_bumps_at_encode_time() {
        // Flush produces a batch whose drop_counter Arc is shared with
        // the source trace. A subsequent bump on either Arc surfaces
        // here at encode time — that's the clone-not-snapshot promise.
        let (batch, counter) = fixture_batch(0, 1);
        // Simulate a post-flush drop on the same trace (e.g. a
        // channel-full bump from a later flush).
        counter.fetch_add(7, Ordering::Release);
        let bytes = encode_batch(&batch).unwrap();
        let decoded: wire::Batch = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(
            decoded.meta.dropped_records, 7,
            "encoder must observe post-flush bumps via the shared Arc",
        );
    }

    #[test]
    fn encode_batch_applies_wire_field_name_shortenings() {
        let (batch, _) = fixture_batch(0, 1);
        let bytes = encode_batch(&batch).unwrap();
        // Parse as a structureless rmpv::Value so we can inspect the
        // on-wire map keys directly. `rmpv::decode::read_value` is the
        // direct-from-bytes path (`rmpv::Value: Deserialize` is not
        // implemented).
        let value = rmpv::decode::read_value(&mut bytes.as_slice())
            .expect("rmpv decodes the produced bytes");
        let map = value.as_map().expect("top-level value is a map");

        // Pull out the `calls` array and confirm the first entry's
        // map keys are the §4.2.3 wire names, not the recorder's
        // _ns / _bytes suffixes.
        let calls_value = map
            .iter()
            .find_map(|(k, v)| (k.as_str()? == "calls").then_some(v))
            .expect("`calls` key present on the wire");
        let calls_array = calls_value.as_array().unwrap();
        let first_call = calls_array[0].as_map().unwrap();
        let keys: Vec<&str> = first_call.iter().filter_map(|(k, _)| k.as_str()).collect();
        for expected in [
            "call_id",
            "parent",
            "fn",
            "depth",
            "t_in",
            "t_out",
            "cpu_u",
            "cpu_s",
            "mem_in",
            "mem_out",
            "abnormal_exit",
        ] {
            assert!(
                keys.contains(&expected),
                "wire CallRecord must carry the `{expected}` key — got {keys:?}",
            );
        }
        for forbidden in ["fn_id", "t_in_ns", "mem_in_bytes", "cpu_u_ns"] {
            assert!(
                !keys.contains(&forbidden),
                "wire CallRecord must NOT carry the in-memory name `{forbidden}` — got {keys:?}",
            );
        }
    }

    #[test]
    fn encode_batch_handles_empty_dict_and_calls() {
        let (mut batch, _) = fixture_batch(0, 0);
        batch.dict.clear();
        let bytes = encode_batch(&batch).expect("empty batch still encodes");
        let decoded: wire::Batch = rmp_serde::from_slice(&bytes).unwrap();
        assert!(decoded.dict.is_empty());
        assert!(decoded.calls.is_empty());
        assert_eq!(decoded.meta.dropped_records, 0);
    }

    #[test]
    fn encode_batch_renders_trace_id_as_hyphenated_uuid_string() {
        let (batch, _) = fixture_batch(0, 0);
        // The fixture's trace_id bytes correspond to a known
        // UUID v7 prefix; the canonical hyphenated render is
        // 018f32a1-b2c3-7000-8012-3456789abcde (8-4-4-4-12 hex).
        let bytes = encode_batch(&batch).unwrap();
        let decoded: wire::Batch = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(
            decoded.meta.trace_id, "018f32a1-b2c3-7000-8012-3456789abcde",
            "encoder must render trace_id as the 36-char hyphenated UUID form",
        );
    }

    #[test]
    fn encode_batch_preserves_dict_and_call_field_values() {
        // Every recorder-side field maps to the matching wire-side
        // field with the correct rename / kind translation.
        let (batch, _) = fixture_batch(0, 2);
        let bytes = encode_batch(&batch).unwrap();
        let decoded: wire::Batch = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(decoded.dict[0].fn_id, 1);
        assert_eq!(decoded.dict[0].fqn, "noop");
        assert_eq!(decoded.dict[0].file, "/path/to/script.php");
        assert_eq!(decoded.dict[0].line, 7);
        assert_eq!(decoded.dict[0].kind, wire::FunctionKind::Function);
        assert_eq!(decoded.calls[0].call_id, 1);
        assert_eq!(decoded.calls[0].fn_id, 1);
        assert_eq!(decoded.calls[0].t_in, 1_000_000);
        assert_eq!(decoded.calls[0].mem_in, 1024);
        assert!(!decoded.calls[0].abnormal_exit);
        assert_eq!(decoded.calls[1].call_id, 2);
    }

    #[test]
    fn function_kind_translation_is_total() {
        // Every recorder variant maps to the matching wire variant.
        assert_eq!(
            function_kind_to_wire(rec::FunctionKind::Function),
            wire::FunctionKind::Function
        );
        assert_eq!(
            function_kind_to_wire(rec::FunctionKind::Method),
            wire::FunctionKind::Method
        );
        assert_eq!(
            function_kind_to_wire(rec::FunctionKind::Closure),
            wire::FunctionKind::Closure
        );
        assert_eq!(
            function_kind_to_wire(rec::FunctionKind::Internal),
            wire::FunctionKind::Internal
        );
    }

    #[test]
    fn batch_to_wire_meta_carries_meta_partial_fields_verbatim() {
        // The infallible meta conversion: host/sapi/uri Arc<str>
        // become owned String; pid/start_time copy through; trace_id
        // gets the UUID render. No values are dropped.
        let (batch, _) = fixture_batch(0, 0);
        let wire_batch = batch_to_wire(&batch);
        assert_eq!(wire_batch.meta.host, "test-host");
        assert_eq!(wire_batch.meta.sapi, "cli");
        assert_eq!(wire_batch.meta.uri_or_script, "/path/to/script.php");
        assert_eq!(wire_batch.meta.pid, 4242);
        assert_eq!(wire_batch.meta.start_time, 1_700_000_000_000_000_000);
    }
}
