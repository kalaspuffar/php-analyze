//! Recorder → Shipper flush helper.
//!
//! This module owns the producer side of the Phase-4 channel. It is the
//! sole call site that calls [`Sender::try_send`] for
//! [`ShipperMessage::Batch`] values and the sole site that distinguishes
//! a **channel-full** drop from a **no-Sender** drop (`SPECIFICATION.md`
//! §11 R-13). The threshold-driven and `RSHUTDOWN`-final flush sites in
//! `recorder::observer` call into [`try_send_batch`]; they never reach
//! the canonical Sender directly.
//!
//! References:
//!
//! - `SPECIFICATION.md` §3.2 — "after each emitted record … hand current
//!   buffer to shipper", and the §3.2 buffer-cap-drop sibling.
//! - `SPECIFICATION.md` §5.3 — "Producer (Recorder): `try_send` — if
//!   `Err(Full)`, drop the batch newest-first, bump `drop_counter` by
//!   `batch.calls.len()`. Never `send` (blocking)."
//! - `SPECIFICATION.md` §11 R-13 — both the buffer-cap drop and the
//!   channel-full drop bump the same `Arc<AtomicU64>` drop counter; the
//!   distinction is recorded at the source for a future `E_NOTICE`.
//! - `openspec/changes/recorder-flushes-into-shipper/design.md` §D-1
//!   (module exists), §D-4 (channel-full ordering), §D-7 (no-Sender arm).
//!
//! The module is `pub(crate)` and exposes a single free function. There
//! is no production state held here — `SENDER_SLOT` lives in the
//! [`crate::shipper`] module and is accessed exclusively through
//! [`crate::shipper::clone_canonical_sender`].

use std::sync::atomic::Ordering;

use crossbeam_channel::TrySendError;

use crate::recorder::accounting;
use crate::recorder::types::{PendingBatch, ShipperMessage};
use crate::shipper;

/// Hand a freshly-flushed [`PendingBatch`] to the shipper. The four arms
/// are listed in design.md §D-4 / §D-7; the spec wording is reproduced
/// here so a future reader can audit the implementation against the
/// requirement without leaving the file.
///
/// 1. **No-Sender arm** (silent-disable / never-installed): subtract
///    `batch.size_estimate` from [`accounting::BYTES_IN_MEMORY`], drop
///    the batch, return. The drop counter SHALL NOT be bumped — the
///    batch's records were never visible to any future encoder, and
///    bumping the counter would attribute the drop to a batch that
///    will never exist. A `debug_assert!` fires in tests so a future
///    regression that wires the observer without a Sender surfaces at
///    `cargo test` time. The slot is checked *here* (not in the
///    end-handler) so the predicate site stays a pure field-load.
/// 2. **Sender-present, `Ok(())`**: the channel now owns the bytes.
///    The producer SHALL NOT subtract from
///    [`accounting::BYTES_IN_MEMORY`]; the shipper's consume-path
///    subtract is what closes the budget loop (slice-2 §6 task).
/// 3. **Sender-present, `Err(TrySendError::Full(batch))`** — the
///    channel-full / R-13 arm:
///    a. `accounting::sub(batch.size_estimate)` — bytes never reached
///    the shipper.
///    b. `batch.drop_counter.fetch_add(batch.calls.len() as u64,
///    Ordering::Release)` — surface the drop in a future batch's
///    `meta.dropped_records` (encoder reads `Acquire`).
///    c. Drop `batch`.
/// 4. **Sender-present, `Err(TrySendError::Disconnected(_))`**: the
///    shipper exited mid-request (only possible after `MSHUTDOWN`
///    started draining; not a steady-state condition). Subtract, drop,
///    no counter bump — the batch never reaches an encoder.
pub(crate) fn try_send_batch(batch: PendingBatch) {
    let Some(sender) = shipper::clone_canonical_sender() else {
        // No-Sender arm. See design D-7.
        debug_assert!(
            false,
            "try_send_batch called with no Sender installed; bootstrap layer should have prevented this — extension was enabled, observer ran, but shipper::clone_canonical_sender() returned None"
        );
        accounting::sub(batch.size_estimate);
        return;
    };

    match sender.try_send(ShipperMessage::Batch(batch)) {
        Ok(()) => {
            // Bytes now belong to the channel. The shipper's
            // consume-path subtract is the second leg of the budget
            // round-trip (slice-2 §6, slice-2 `shipper` capability).
        }
        Err(TrySendError::Full(ShipperMessage::Batch(batch))) => {
            // Channel-full arm. Order matters (design D-4): subtract
            // first so the atomic invariant holds continuously, then
            // bump the counter, then let the batch drop.
            accounting::sub(batch.size_estimate);
            // Saturating cast: `batch.calls.len()` is `usize` and
            // bounded by the recorder's `flush_records` (max 10⁹) so
            // a u64 widen is lossless. `as u64` rather than `try_into`
            // because the bound is enforced by directive validation —
            // a panic here would surprise an operator who tweaked
            // `flush_records` for a tight test.
            batch
                .drop_counter
                .fetch_add(batch.calls.len() as u64, Ordering::Release);
            drop(batch);
        }
        Err(TrySendError::Disconnected(ShipperMessage::Batch(batch))) => {
            // Disconnected arm. No future encoder will see this batch
            // — the shipper is gone — so the counter bump would
            // surface a drop nobody can read. We still subtract so
            // the budget atomic stays balanced when the next request
            // recycles the FPM worker.
            accounting::sub(batch.size_estimate);
            drop(batch);
        }
        Err(TrySendError::Full(other)) | Err(TrySendError::Disconnected(other)) => {
            // try_send only ever returns the message variant we sent,
            // i.e. `Batch(_)`. This arm is reachable only if a future
            // refactor sends a different variant through this helper.
            unreachable!(
                "try_send_batch sent a Batch but try_send returned a non-Batch message: {other:?}",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::types::{MetaPartial, PendingBatch};
    use crossbeam_channel::bounded;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// Build a fresh `PendingBatch` for a try_send_batch test.
    ///
    /// `calls_len` controls the channel-full drop-counter bump
    /// (`drop_counter += calls.len()`); `size_estimate` controls the
    /// `accounting` delta. The drop counter Arc is fresh so tests can
    /// independently assert pre/post values.
    fn fixture_batch(calls_len: usize, size_estimate: usize) -> (PendingBatch, Arc<AtomicU64>) {
        use crate::recorder::types::CallRecord;
        let drop_counter = Arc::new(AtomicU64::new(0));
        let calls = (0..calls_len)
            .map(|i| CallRecord {
                call_id: (i as u64) + 1,
                parent: 0,
                fn_id: 1,
                depth: 0,
                t_in_ns: 0,
                t_out_ns: 0,
                cpu_u_ns: 0,
                cpu_s_ns: 0,
                mem_in_bytes: 0,
                mem_out_bytes: 0,
                abnormal_exit: false,
            })
            .collect();
        let meta_partial = MetaPartial {
            schema_version: 1,
            trace_id: [0u8; 16],
            host: Arc::from("test-host"),
            pid: std::process::id(),
            start_time_realtime_ns: 0,
            sapi: Arc::from("cli"),
            uri_or_script: Arc::from("/test"),
        };
        let batch = PendingBatch {
            meta_partial,
            dict: Vec::new(),
            calls,
            size_estimate,
            drop_counter: Arc::clone(&drop_counter),
        };
        (batch, drop_counter)
    }

    /// Install a Sender + Receiver pair into the shipper's canonical
    /// slot so a `try_send_batch` call has a real channel to land on.
    /// Returns the receiver half so the test can assert what arrived.
    /// The slice-1 `acquire_test_lock` guard serialises across tests.
    fn install_test_channel(depth: usize) -> crossbeam_channel::Receiver<ShipperMessage> {
        // Use the slice-1 test helpers: `reset_for_test` clears every
        // shipper slot, then we install our own `Sender` directly into
        // SENDER_SLOT via the shipper's test-only seam.
        shipper::reset_for_test();
        let (tx, rx) = bounded::<ShipperMessage>(depth);
        shipper::install_test_sender(tx);
        rx
    }

    #[test]
    fn try_send_batch_ok_path_does_not_touch_accounting() {
        let _shipper_guard = shipper::acquire_test_lock();
        let _account_guard = accounting::acquire_test_lock();
        accounting::reset_for_test();
        let rx = install_test_channel(8);

        // Seed the budget to a known non-zero so a wayward
        // `accounting::sub` would corrupt it visibly.
        accounting::add(1024);

        let (batch, drop_counter) = fixture_batch(3, 1024);
        try_send_batch(batch);

        assert_eq!(
            accounting::snapshot(),
            1024,
            "Ok arm must NOT subtract — the shipper-side consume will",
        );
        assert_eq!(
            drop_counter.load(Ordering::Acquire),
            0,
            "Ok arm must NOT bump the drop counter",
        );

        // Exactly one Batch is now waiting on the channel.
        let consumed = rx.try_recv().expect("Batch should be queued");
        match consumed {
            ShipperMessage::Batch(b) => assert_eq!(b.calls.len(), 3),
            other => panic!("unexpected message variant: {other:?}"),
        }
        shipper::reset_for_test();
    }

    #[test]
    fn try_send_batch_full_arm_bumps_drop_counter_by_calls_len() {
        let _shipper_guard = shipper::acquire_test_lock();
        let _account_guard = accounting::acquire_test_lock();
        accounting::reset_for_test();
        let _rx = install_test_channel(1);

        // Saturate the bounded(1) channel with a sentinel.
        let (sentinel, _sentinel_counter) = fixture_batch(1, 0);
        shipper::clone_canonical_sender()
            .expect("Sender installed")
            .try_send(ShipperMessage::Batch(sentinel))
            .expect("first send fills the channel");

        // Seed the budget to a known value the channel-full subtract
        // will then remove. The bump-by-len assertion is independent.
        accounting::add(512);
        let (batch, drop_counter) = fixture_batch(17, 512);
        try_send_batch(batch);

        assert_eq!(
            accounting::snapshot(),
            0,
            "channel-full arm must subtract batch.size_estimate from accounting",
        );
        assert_eq!(
            drop_counter.load(Ordering::Acquire),
            17,
            "channel-full arm must bump drop_counter by batch.calls.len()",
        );
        shipper::reset_for_test();
    }

    #[test]
    fn try_send_batch_disconnected_arm_subtracts_without_counter_bump() {
        let _shipper_guard = shipper::acquire_test_lock();
        let _account_guard = accounting::acquire_test_lock();
        accounting::reset_for_test();
        let rx = install_test_channel(8);
        // Drop the receiver to break the channel; the SENDER_SLOT still
        // holds the canonical Sender, so try_send_batch reaches the
        // Disconnected arm rather than the No-Sender arm.
        drop(rx);

        accounting::add(2048);
        let (batch, drop_counter) = fixture_batch(11, 2048);
        try_send_batch(batch);

        assert_eq!(
            accounting::snapshot(),
            0,
            "Disconnected arm must subtract batch.size_estimate",
        );
        assert_eq!(
            drop_counter.load(Ordering::Acquire),
            0,
            "Disconnected arm must NOT bump drop_counter — no encoder will read it",
        );
        shipper::reset_for_test();
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn try_send_batch_no_sender_arm_subtracts_without_counter_bump() {
        let _shipper_guard = shipper::acquire_test_lock();
        let _account_guard = accounting::acquire_test_lock();
        accounting::reset_for_test();
        shipper::reset_for_test(); // ensure SENDER_SLOT is empty
        assert!(!shipper::sender_is_installed(), "precondition: no Sender");

        accounting::add(4096);
        let (batch, drop_counter) = fixture_batch(5, 4096);
        try_send_batch(batch);

        assert_eq!(
            accounting::snapshot(),
            0,
            "No-Sender arm must subtract batch.size_estimate",
        );
        assert_eq!(
            drop_counter.load(Ordering::Acquire),
            0,
            "No-Sender arm must NOT bump drop_counter",
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "try_send_batch called with no Sender installed")]
    fn try_send_batch_no_sender_arm_debug_asserts() {
        let _shipper_guard = shipper::acquire_test_lock();
        let _account_guard = accounting::acquire_test_lock();
        accounting::reset_for_test();
        shipper::reset_for_test(); // ensure SENDER_SLOT is empty

        let (batch, _drop_counter) = fixture_batch(1, 256);
        try_send_batch(batch);
    }
}
