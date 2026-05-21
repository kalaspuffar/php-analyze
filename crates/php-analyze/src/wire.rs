//! `wire` — serde-derived types for the `SPECIFICATION.md` §4.2
//! MessagePack batch schema.
//!
//! This module is the **single source of truth** for the on-wire shape
//! of a `Batch` (and its `MetaFull` / `DictEntry` / `CallRecord` /
//! `FunctionKind` parts) that the Phase-4 shipper will encode and that
//! the `stub-ingest` crate decodes. Field names, types, and the
//! `FunctionKind` integer mapping are frozen at v1; future v2 schema
//! bumps modify these types in place.
//!
//! ### v1-frozen wire decisions
//!
//! - **Field names match §4.2 exactly.** Where a Rust keyword
//!   collides with a wire key (`fn`), the Rust field uses a different
//!   identifier and `#[serde(rename = "fn")]` maps it.
//! - **`FunctionKind` is encoded as a small int (0..=3) per §4.2.2**,
//!   not as a tagged string. See [`FunctionKind`].
//! - **`trace_id` is `String` on the wire**, not [`uuid::Uuid`]. This
//!   decouples the wire schema from the (Phase-4) `uuid` crate version
//!   — see `design.md` D-3 of the `wire-types-and-stub-ingest` change.
//! - **Forward compatibility**: unknown extra keys on decode are
//!   **silently ignored** (serde's default — no
//!   `#[serde(deny_unknown_fields)]` on any wire type). The parallel
//!   `MetaFullStrict` test type in the `tests` submodule is the
//!   regression boundary that pins this choice in place.
//!
//! ### Out of scope in this slice
//!
//! Phase 4 (`Shipper + transport`) owns:
//!
//! - `From<&recorder::types::Trace> for Batch` (and per-record
//!   conversions for `CallRecord` / `DictEntry`). The intentional
//!   absence of any such impl in this module is the slice-3 boundary;
//!   see the spec scenario *"No `From<_> for wire::Batch` impls exist
//!   in this slice"*.
//! - Stamping `MetaFull.dropped_records` from `Trace::drop_counter`.
//!   The wire **field** exists here; the wire **value** is whatever
//!   the test (or, in Phase 4, the shipper) writes into it.
//! - `MEDIA_TYPE` / `SCHEMA_VERSION` as HTTP `Content-Type` header and
//!   stamped `MetaFull.schema_version`. The constants live here; the
//!   `Content-Type` plumbing belongs to the shipper.

use serde::{Deserialize, Serialize};

/// The HTTP `Content-Type` header value used for v1 `Batch` payloads.
///
/// Frozen at v1 per `SPECIFICATION.md` §1.4 OQ-2. Future incompatible
/// schema changes ship under `application/vnd.php-analyze.v2+msgpack`
/// etc.; this constant moves to `v2` when (and only when) v2 lands.
pub const MEDIA_TYPE: &str = "application/vnd.php-analyze.v1+msgpack";

/// The `meta.schema_version` value the Phase-4 shipper stamps onto
/// every encoded batch. Decoders SHOULD NOT reject unknown future
/// versions on type alone — they SHOULD apply the schema's
/// forward-compat rule and only fail when a structural mismatch
/// prevents decode.
pub const SCHEMA_VERSION: u8 = 1;

// SECURITY/COMPAT: never add `#[serde(deny_unknown_fields)]` to any
// wire type defined in this module — `SPECIFICATION.md` §4.2 mandates
// that decoders ignore unknown extra keys ("unknown extra keys in any
// map MUST be ignored by the server"). The parallel
// `MetaFullStrict` test type below is the regression boundary that
// pins this rule in place.

/// `SPECIFICATION.md` §4.2.2 freezes the `kind` field encoding as a
/// small int: `0=function`, `1=method`, `2=closure`, `3=internal`.
/// Using serde's default `#[derive]` representation would emit a
/// tagged string instead, silently breaking the §4.2.2 contract — so
/// we route the serde representation through `u8` explicitly via
/// `serde(into / try_from = "u8")`. The `TryFrom` returns a
/// `&'static str` error so a hostile peer sending `kind = 99`
/// produces a clean decode error instead of a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "u8", try_from = "u8")]
#[repr(u8)]
pub enum FunctionKind {
    Function = 0,
    Method = 1,
    Closure = 2,
    Internal = 3,
}

impl From<FunctionKind> for u8 {
    fn from(kind: FunctionKind) -> Self {
        // The discriminant assignment above is the source of truth;
        // this match keeps the mapping visible at the call site.
        match kind {
            FunctionKind::Function => 0,
            FunctionKind::Method => 1,
            FunctionKind::Closure => 2,
            FunctionKind::Internal => 3,
        }
    }
}

impl TryFrom<u8> for FunctionKind {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(FunctionKind::Function),
            1 => Ok(FunctionKind::Method),
            2 => Ok(FunctionKind::Closure),
            3 => Ok(FunctionKind::Internal),
            _ => Err("invalid FunctionKind discriminant"),
        }
    }
}

/// `meta` map on the wire — `SPECIFICATION.md` §4.2.1.
///
/// All eight fields are present unconditionally; none is
/// `#[serde(rename = "…")]`-renamed (the wire names happen to be
/// valid Rust identifiers).
///
/// `trace_id` is a 36-character UUID rendering on the wire; the wire
/// type does not enforce the format, by design (D-3 of the change's
/// design.md). `dropped_records` is the cumulative drop count at send
/// time — Phase 4 stamps it from `Trace::drop_counter`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaFull {
    pub schema_version: u8,
    pub trace_id: String,
    pub host: String,
    pub pid: u32,
    pub start_time: i64,
    pub sapi: String,
    pub uri_or_script: String,
    pub dropped_records: u64,
}

/// `dict` array entry on the wire — `SPECIFICATION.md` §4.2.2.
///
/// For internal functions, `file` SHALL be `""` and `line` SHALL be
/// `0` per §4.2.2; the type accepts these values on decode without
/// any special-casing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictEntry {
    pub fn_id: u32,
    pub fqn: String,
    pub file: String,
    pub line: u32,
    pub kind: FunctionKind,
}

/// `calls` array entry on the wire — `SPECIFICATION.md` §4.2.3.
///
/// The Rust field `fn_id` is `#[serde(rename = "fn")]`-mapped to the
/// wire key `"fn"` (Rust reserves `fn` as a keyword). The remaining
/// field names are byte-identical to the §4.2.3 wire keys.
///
/// Note the `_ns` / `_bytes` suffixes used by
/// [`crate::recorder::types::CallRecord`] do **not** appear here —
/// Phase 4's `From<&recorder::types::CallRecord>` conversion maps
/// `t_in_ns → t_in`, `mem_in_bytes → mem_in`, etc. The two types are
/// deliberately separate (design D-4); see the module-level doc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallRecord {
    pub call_id: u64,
    pub parent: u64,
    #[serde(rename = "fn")]
    pub fn_id: u32,
    pub depth: u16,
    pub t_in: i64,
    pub t_out: i64,
    pub cpu_u: i64,
    pub cpu_s: i64,
    pub mem_in: i64,
    pub mem_out: i64,
    pub abnormal_exit: bool,
}

/// Top-level wire batch — `SPECIFICATION.md` §4.2.
///
/// Encoded by `rmp_serde::to_vec_named` (named-map representation, so
/// field names appear as MessagePack string keys); decoded by
/// `rmp_serde::from_slice`. Round-trips losslessly through
/// `PartialEq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
    pub meta: MetaFull,
    pub dict: Vec<DictEntry>,
    pub calls: Vec<CallRecord>,
}

#[cfg(test)]
mod tests {
    //! Tests are organised top-down: constants, then `FunctionKind`,
    //! then each struct, then the cross-cutting round-trip suite, then
    //! the forward-compat boundary.

    use super::*;

    // ---- constants -------------------------------------------------

    #[test]
    fn media_type_matches_oq_2_string_exactly() {
        assert_eq!(MEDIA_TYPE, "application/vnd.php-analyze.v1+msgpack");
    }

    #[test]
    fn schema_version_is_one() {
        assert_eq!(SCHEMA_VERSION, 1u8);
    }

    // ---- FunctionKind ---------------------------------------------

    #[test]
    fn function_kind_from_u8_round_trips_each_variant() {
        for kind in [
            FunctionKind::Function,
            FunctionKind::Method,
            FunctionKind::Closure,
            FunctionKind::Internal,
        ] {
            let byte: u8 = kind.into();
            let recovered =
                FunctionKind::try_from(byte).expect("each variant must round-trip through u8");
            assert_eq!(recovered, kind);
        }
    }

    #[test]
    fn function_kind_try_from_rejects_out_of_range_byte() {
        let err = FunctionKind::try_from(99u8).expect_err("99 is not a valid discriminant");
        assert!(
            err.contains("FunctionKind"),
            "error message should mention FunctionKind, got: {err}"
        );
    }

    /// Tiny wrapper so we can encode a single `FunctionKind` via the
    /// named-map encoder (MessagePack named-named-map roots need to be
    /// maps, not bare ints — this is `rmp_serde::to_vec_named`'s
    /// contract).
    #[derive(Debug, Serialize, Deserialize)]
    struct KindWrapper {
        k: FunctionKind,
    }

    #[test]
    fn function_kind_method_encodes_as_integer_one_via_rmp_serde() {
        let bytes = rmp_serde::to_vec_named(&KindWrapper {
            k: FunctionKind::Method,
        })
        .expect("rmp_serde must accept the wrapper");

        // Decode through `rmpv::Value` to read the `k` field as an
        // integer without re-using the same `FunctionKind`
        // deserialiser we are trying to test.
        let value = rmpv::decode::read_value(&mut bytes.as_slice())
            .expect("rmpv must accept the encoded wrapper");
        let map = value.as_map().expect("KindWrapper encodes to a map");
        let (_, k) = map
            .iter()
            .find(|(k, _)| k.as_str() == Some("k"))
            .expect("encoded map carries the `k` field");
        assert_eq!(
            k.as_i64(),
            Some(1),
            "FunctionKind::Method must encode as integer 1; got {k:?}"
        );
    }

    // ---- MetaFull --------------------------------------------------

    fn sample_meta_full() -> MetaFull {
        MetaFull {
            schema_version: SCHEMA_VERSION,
            trace_id: "0190b5e7-1c2d-7000-8000-000000000001".to_owned(),
            host: "test-host".to_owned(),
            pid: 12345,
            start_time: 1_716_304_800_000_000_000,
            sapi: "cli".to_owned(),
            uri_or_script: "/tmp/script.php".to_owned(),
            dropped_records: 7,
        }
    }

    #[test]
    fn meta_full_round_trips_with_all_fields_populated() {
        let original = sample_meta_full();
        let bytes = rmp_serde::to_vec_named(&original).expect("encode succeeds");
        let decoded: MetaFull = rmp_serde::from_slice(&bytes).expect("decode succeeds");
        assert_eq!(decoded, original);
    }

    #[test]
    fn meta_full_encoded_bytes_contain_all_eight_wire_keys() {
        let bytes = rmp_serde::to_vec_named(&sample_meta_full()).expect("encode succeeds");
        for key in [
            "schema_version",
            "trace_id",
            "host",
            "pid",
            "start_time",
            "sapi",
            "uri_or_script",
            "dropped_records",
        ] {
            assert!(
                contains_msgpack_str_key(&bytes, key),
                "encoded MetaFull must contain wire key {key:?}",
            );
        }
    }

    // ---- DictEntry -------------------------------------------------

    #[test]
    fn dict_entry_user_method_round_trips() {
        let original = DictEntry {
            fn_id: 7,
            fqn: "App\\Service::run".to_owned(),
            file: "/srv/app/src/Service.php".to_owned(),
            line: 42,
            kind: FunctionKind::Method,
        };
        let bytes = rmp_serde::to_vec_named(&original).expect("encode succeeds");
        let decoded: DictEntry = rmp_serde::from_slice(&bytes).expect("decode succeeds");
        assert_eq!(decoded, original);
    }

    #[test]
    fn dict_entry_internal_round_trips_with_empty_file_and_zero_line() {
        let original = DictEntry {
            fn_id: 1,
            fqn: "array_map".to_owned(),
            file: String::new(),
            line: 0,
            kind: FunctionKind::Internal,
        };
        let bytes = rmp_serde::to_vec_named(&original).expect("encode succeeds");
        let decoded: DictEntry = rmp_serde::from_slice(&bytes).expect("decode succeeds");
        assert_eq!(decoded, original);
    }

    // ---- CallRecord ------------------------------------------------

    fn sample_call_record() -> CallRecord {
        CallRecord {
            call_id: 1,
            parent: 0,
            fn_id: 42,
            depth: 3,
            t_in: 1_000_000,
            t_out: 2_000_000,
            cpu_u: 500,
            cpu_s: 100,
            mem_in: 4096,
            mem_out: 8192,
            abnormal_exit: false,
        }
    }

    #[test]
    fn call_record_round_trips_and_encoded_bytes_contain_literal_fn_key() {
        let original = sample_call_record();
        let bytes = rmp_serde::to_vec_named(&original).expect("encode succeeds");

        // `"fn"` MUST appear as a MessagePack string key; `"fn_id"`
        // MUST NOT — the rename is the slice-3 wire contract.
        assert!(
            contains_msgpack_str_key(&bytes, "fn"),
            "encoded CallRecord must contain the wire key \"fn\""
        );
        assert!(
            !contains_msgpack_str_key(&bytes, "fn_id"),
            "encoded CallRecord must NOT carry the Rust field name \"fn_id\""
        );

        let decoded: CallRecord = rmp_serde::from_slice(&bytes).expect("decode succeeds");
        assert_eq!(decoded, original);
    }

    #[test]
    fn call_record_abnormal_exit_true_round_trips() {
        let mut original = sample_call_record();
        original.abnormal_exit = true;
        let bytes = rmp_serde::to_vec_named(&original).expect("encode succeeds");
        let decoded: CallRecord = rmp_serde::from_slice(&bytes).expect("decode succeeds");
        assert!(decoded.abnormal_exit);
        assert_eq!(decoded, original);
    }

    #[test]
    fn call_record_with_zero_durations_round_trips() {
        // Sub-tick calls: the recorder emits these when t_in == t_out
        // and getrusage's microsecond granularity yields cpu_u == 0.
        let original = CallRecord {
            call_id: 99,
            parent: 1,
            fn_id: 7,
            depth: 5,
            t_in: 500,
            t_out: 500,
            cpu_u: 0,
            cpu_s: 0,
            mem_in: 1024,
            mem_out: 1024,
            abnormal_exit: false,
        };
        let bytes = rmp_serde::to_vec_named(&original).expect("encode succeeds");
        let decoded: CallRecord = rmp_serde::from_slice(&bytes).expect("decode succeeds");
        assert_eq!(decoded, original);
    }

    // ---- Batch -----------------------------------------------------

    fn sample_batch_minimum() -> Batch {
        Batch {
            meta: sample_meta_full(),
            dict: vec![DictEntry {
                fn_id: 1,
                fqn: "main".to_owned(),
                file: "/tmp/script.php".to_owned(),
                line: 1,
                kind: FunctionKind::Function,
            }],
            calls: vec![sample_call_record()],
        }
    }

    #[test]
    fn batch_round_trips_minimum_shape() {
        let original = sample_batch_minimum();
        let bytes = rmp_serde::to_vec_named(&original).expect("encode succeeds");
        let decoded: Batch = rmp_serde::from_slice(&bytes).expect("decode succeeds");
        assert_eq!(decoded, original);
    }

    #[test]
    fn batch_round_trips_with_empty_dict_and_empty_calls() {
        let original = Batch {
            meta: sample_meta_full(),
            dict: vec![],
            calls: vec![],
        };
        let bytes = rmp_serde::to_vec_named(&original).expect("encode succeeds");
        let decoded: Batch = rmp_serde::from_slice(&bytes).expect("decode succeeds");
        assert!(decoded.dict.is_empty());
        assert!(decoded.calls.is_empty());
        assert_eq!(decoded.meta, original.meta);
    }

    fn sample_batch_realistic() -> Batch {
        let dict = vec![
            DictEntry {
                fn_id: 1,
                fqn: "do_work".to_owned(),
                file: "/srv/app/src/lib.php".to_owned(),
                line: 12,
                kind: FunctionKind::Function,
            },
            DictEntry {
                fn_id: 2,
                fqn: "App\\Service::run".to_owned(),
                file: "/srv/app/src/Service.php".to_owned(),
                line: 42,
                kind: FunctionKind::Method,
            },
            DictEntry {
                fn_id: 3,
                fqn: "{closure}".to_owned(),
                file: "/srv/app/src/handler.php".to_owned(),
                line: 7,
                kind: FunctionKind::Closure,
            },
            DictEntry {
                fn_id: 4,
                fqn: "array_map".to_owned(),
                file: String::new(),
                line: 0,
                kind: FunctionKind::Internal,
            },
        ];
        let mut calls = Vec::with_capacity(6);
        for i in 0..5 {
            calls.push(CallRecord {
                call_id: i + 1,
                parent: i,
                fn_id: ((i % 4) + 1) as u32,
                depth: i as u16,
                t_in: 1_000_000 + (i as i64) * 100,
                t_out: 1_000_500 + (i as i64) * 100,
                cpu_u: 50,
                cpu_s: 10,
                mem_in: 4096,
                mem_out: 4096 + (i as i64) * 16,
                abnormal_exit: false,
            });
        }
        calls.push(CallRecord {
            call_id: 6,
            parent: 5,
            fn_id: 2,
            depth: 5,
            t_in: 2_000_000,
            t_out: 2_001_000,
            cpu_u: 100,
            cpu_s: 20,
            mem_in: 8192,
            mem_out: 8192,
            abnormal_exit: true,
        });

        Batch {
            meta: sample_meta_full(),
            dict,
            calls,
        }
    }

    #[test]
    fn batch_round_trips_realistic_shape() {
        let original = sample_batch_realistic();
        let bytes = rmp_serde::to_vec_named(&original).expect("encode succeeds");
        let decoded: Batch = rmp_serde::from_slice(&bytes).expect("decode succeeds");
        assert_eq!(decoded, original);
    }

    /// Spec scenario `Scenario: A realistic-shape round-trip
    /// succeeds` under the *wire.rs SHALL be production-side
    /// encode-only* requirement. Distinct from
    /// `batch_round_trips_realistic_shape` so the spec-to-test
    /// mapping is 1:1.
    #[test]
    fn realistic_shape_round_trip_succeeds() {
        let original = sample_batch_realistic();
        let bytes = rmp_serde::to_vec_named(&original).expect("encode succeeds");
        let decoded: Batch = rmp_serde::from_slice(&bytes).expect("decode succeeds");
        assert_eq!(decoded, original);

        // Sanity-check the fixture: at least one of each FunctionKind
        // SHOULD appear in `dict`, and at least one
        // `abnormal_exit = true` SHOULD appear in `calls`.
        let mut seen_kinds = std::collections::HashSet::new();
        for e in &decoded.dict {
            seen_kinds.insert(e.kind);
        }
        assert!(seen_kinds.contains(&FunctionKind::Function));
        assert!(seen_kinds.contains(&FunctionKind::Method));
        assert!(seen_kinds.contains(&FunctionKind::Closure));
        assert!(seen_kinds.contains(&FunctionKind::Internal));
        assert!(decoded.calls.iter().any(|c| c.abnormal_exit));
    }

    #[test]
    fn dict_entry_kind_99_decode_fails_cleanly() {
        // Hand-build a `DictEntry`-shaped map with `kind = 99` via
        // rmpv, so we exercise the `TryFrom<u8>` rejection path
        // without panicking.
        let map = rmpv::Value::Map(vec![
            ("fn_id".into(), rmpv::Value::from(7u32)),
            ("fqn".into(), rmpv::Value::from("foo")),
            ("file".into(), rmpv::Value::from("")),
            ("line".into(), rmpv::Value::from(0u32)),
            ("kind".into(), rmpv::Value::from(99u8)),
        ]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &map).expect("rmpv encode");

        let err =
            rmp_serde::from_slice::<DictEntry>(&bytes).expect_err("decode must fail for kind=99");
        let msg = format!("{err}");
        assert!(
            msg.contains("FunctionKind") || msg.contains("invalid"),
            "decode error must mention the invalid FunctionKind; got: {msg}",
        );
    }

    // ---- Forward-compat boundary ----------------------------------

    /// Parallel-shape `MetaFull` with `#[serde(deny_unknown_fields)]`.
    /// The pair of tests below asserts that:
    /// 1. `MetaFull` accepts an unknown-field input (the §4.2 rule).
    /// 2. `MetaFullStrict` *rejects* the same input — proving that
    ///    our forward-compat tolerance is a deliberate consequence
    ///    of not setting `deny_unknown_fields`, not an accident of
    ///    `rmp-serde`'s decoder.
    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct MetaFullStrict {
        schema_version: u8,
        trace_id: String,
        host: String,
        pid: u32,
        start_time: i64,
        sapi: String,
        uri_or_script: String,
        dropped_records: u64,
    }

    fn meta_full_with_extra_field_bytes() -> Vec<u8> {
        // Hand-build the named-map encoding of `MetaFull` + one
        // extra `"future_field": 42` key. Using `rmpv` keeps the
        // test legible without ceding the encoding to the very
        // serde derive we are testing.
        let m = sample_meta_full();
        let value = rmpv::Value::Map(vec![
            ("schema_version".into(), rmpv::Value::from(m.schema_version)),
            ("trace_id".into(), rmpv::Value::from(m.trace_id.as_str())),
            ("host".into(), rmpv::Value::from(m.host.as_str())),
            ("pid".into(), rmpv::Value::from(m.pid)),
            ("start_time".into(), rmpv::Value::from(m.start_time)),
            ("sapi".into(), rmpv::Value::from(m.sapi.as_str())),
            (
                "uri_or_script".into(),
                rmpv::Value::from(m.uri_or_script.as_str()),
            ),
            (
                "dropped_records".into(),
                rmpv::Value::from(m.dropped_records),
            ),
            ("future_field".into(), rmpv::Value::from(42u32)),
        ]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &value).expect("rmpv encode");
        bytes
    }

    #[test]
    fn meta_full_decodes_cleanly_with_unknown_extra_field() {
        let bytes = meta_full_with_extra_field_bytes();
        let decoded: MetaFull = rmp_serde::from_slice(&bytes)
            .expect("forward-compat: unknown extra fields are silently ignored");
        assert_eq!(decoded, sample_meta_full());
    }

    #[test]
    fn meta_full_strict_rejects_the_same_unknown_field_bytes() {
        let bytes = meta_full_with_extra_field_bytes();
        let result: Result<MetaFullStrict, _> = rmp_serde::from_slice(&bytes);
        assert!(
            result.is_err(),
            "regression boundary: MetaFullStrict with deny_unknown_fields \
             must reject the same bytes that MetaFull accepts",
        );
    }

    // ---- helpers ---------------------------------------------------

    /// Search `bytes` for the MessagePack `fixstr` / `str8` /
    /// `str16` / `str32` representation of `key`. This is a
    /// minimum-viable scanner that handles the lengths a wire field
    /// name can plausibly take (≤ 255 bytes, so `fixstr` and `str8`
    /// suffice; we keep the larger branches for completeness and
    /// future-proofing).
    fn contains_msgpack_str_key(bytes: &[u8], key: &str) -> bool {
        let len = key.len();
        let key = key.as_bytes();

        // fixstr: 0xa0..=0xbf with the low 5 bits as length.
        if len <= 0x1f {
            let header = 0xa0u8 | (len as u8);
            if find_subsequence(bytes, &[&[header], key].concat()).is_some() {
                return true;
            }
        }
        // str 8: 0xd9 <len:u8> <bytes>
        if len <= u8::MAX as usize
            && find_subsequence(bytes, &[&[0xd9u8, len as u8], key].concat()).is_some()
        {
            return true;
        }
        // str 16: 0xda <len:u16-be> <bytes>
        if len <= u16::MAX as usize {
            let mut needle = vec![0xdau8];
            needle.extend_from_slice(&(len as u16).to_be_bytes());
            needle.extend_from_slice(key);
            if find_subsequence(bytes, &needle).is_some() {
                return true;
            }
        }
        // str 32: 0xdb <len:u32-be> <bytes>
        let mut needle = vec![0xdbu8];
        needle.extend_from_slice(&(len as u32).to_be_bytes());
        needle.extend_from_slice(key);
        find_subsequence(bytes, &needle).is_some()
    }

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || needle.len() > haystack.len() {
            return None;
        }
        haystack.windows(needle.len()).position(|w| w == needle)
    }
}
