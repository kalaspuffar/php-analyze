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
//! - [`accounting`] — the process-wide `BYTES_IN_MEMORY` atomic used
//!   by the §3.2 cap-check. Slice 3 introduces the only `add` / `sub`
//!   sites; Phase 4 will add the shipper-side subtract.
//!
//! Slice 3 (`recorder-depth-and-cap-drops`) added `max_depth` gating,
//! `buffer_cap_bytes` accounting, and the per-trace `Arc<AtomicU64>`
//! drop counter. The flush-threshold and channel-handoff paths land
//! with the Phase-4 shipper.

pub(crate) mod accounting;
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
