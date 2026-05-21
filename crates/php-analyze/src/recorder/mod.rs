//! Recorder substrate: in-memory data model and function-dictionary interner.
//!
//! This module is the pure-Rust foundation the Phase-2 observer wiring sits
//! on top of. It contains:
//!
//! - [`types`] — the §4.1 in-memory types from `SPECIFICATION.md`
//!   (`Trace`, `CallFrame`, `CallRecord`, `DictEntry`, `FunctionKey`,
//!   `FunctionKind`, `MetaPartial`, `PendingBatch`, `ShipperMessage`).
//! - [`dictionary`] — the function-dictionary interner that allocates a
//!   `fn_id` once per `FunctionKey` and stages `DictEntry` values for
//!   inclusion in the next batch.
//!
//! No `FcallObserver`, no `Trace` allocation at `RINIT`, no depth or buffer-
//! cap enforcement, and no `Arc<AtomicU64>` drop counter are wired in this
//! module yet. Those land in the two follow-up Phase-2 slices
//! (`recorder-observer-hooks-and-trace-lifecycle`,
//! `recorder-depth-and-cap-drops`).

pub mod dictionary;
pub mod types;

pub use dictionary::Dictionary;
