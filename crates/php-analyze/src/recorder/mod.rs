//! Recorder: per-trace data model, function-dictionary interner, and the
//! production observer wiring.
//!
//! Module layout:
//!
//! - [`types`] — the §4.1 in-memory types from `SPECIFICATION.md`
//!   (`Trace`, `CallFrame`, `CallRecord`, `DictEntry`, `FunctionKey`,
//!   `FunctionKind`, `MetaPartial`, `PendingBatch`, `ShipperMessage`,
//!   `RequestIdentity`).
//! - [`dictionary`] — the function-dictionary interner that allocates a
//!   `fn_id` once per `FunctionKey` and stages `DictEntry` values for
//!   inclusion in the next batch.
//! - [`observer`] — the production `Recorder` (`FcallObserver` impl),
//!   the `BootObserver` dispatcher registered at `MINIT`, and the
//!   thread-local `Trace` slot with its `RINIT` / `RSHUTDOWN` entry
//!   points.
//!
//! Slice 3 (`recorder-depth-and-cap-drops`) will add `max_depth` gating,
//! `buffer_cap_bytes` accounting, and the `Arc<AtomicU64>` drop counter.

pub mod dictionary;
#[cfg(feature = "recorder-dump")]
pub mod dump;
pub mod observer;
pub mod types;

pub use dictionary::Dictionary;
pub use observer::{
    build_boot_observer, build_recorder_observer, rinit_allocate_trace, rshutdown_release_trace,
    BootObserver, Recorder,
};
pub use types::RequestIdentity;
