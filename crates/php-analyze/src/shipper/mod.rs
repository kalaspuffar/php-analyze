//! Shipper — Phase 4 substrate (slice 1).
//!
//! `SPECIFICATION.md` §3.3 (Shipper) and §3.4 (Process-wide bootstrap
//! & shutdown) are split across multiple OpenSpec changes:
//!
//! - **Slice 1 (this module)**: channel, lazy thread spawn, drain
//!   protocol. The shipper drains `ShipperMessage::Batch(_)` values
//!   from the channel and *drops them silently* — no encoding, no
//!   HTTP, no retries. The thread, the channel, and the
//!   `MSHUTDOWN`-bounded drain are all here.
//! - **Slice 2 (future)**: MessagePack encoding via `rmp_serde` on
//!   the shipper thread; single-attempt POST via `ureq`; stamping of
//!   `meta.dropped_records`; the `Recorder` side's
//!   `RSHUTDOWN`-final-flush and threshold-driven flushes.
//! - **Slice 3 (future)**: retry / backoff on transient HTTP
//!   failures; the `E_NOTICE` log line on drop-on-retry-exhaust; the
//!   `channel-full vs. buffer-cap` drop distinction (R-13).
//!
//! Slice 1's substrate is deliberately small and testable:
//! - [`run_loop`] is a pure-Rust state machine that takes a
//!   [`crossbeam_channel::Receiver`] and returns a [`ShipperExit`].
//!   Unit-tested directly with hand-constructed channels.
//! - [`install_channel_at_minit`], [`spawn_if_needed_at_rinit`], and
//!   [`drain_and_join_at_mshutdown`] are the lifecycle entry points
//!   the `bootstrap` layer calls. They mediate four process-global
//!   slots (sender, receiver, spawn-once flag, join handle) under a
//!   per-test mutex for serialisation.
//!
//! ## Design deviation: `Mutex<Option<Sender>>` vs. `OnceLock<Sender>`
//!
//! The OpenSpec change's `tasks.md` §4.1 sketched `OnceLock<Sender<…>>`
//! for the canonical-Sender slot. `OnceLock` cannot be cleared, so
//! [`drain_and_join_at_mshutdown`] would have no way to drop the
//! Sender at shutdown — leaving the channel open and making the
//! shipper loop block on `recv_deadline` until the deadline expired
//! even on a clean shutdown of an empty channel. The implementation
//! uses [`std::sync::Mutex`]`<`[`Option`]`<`[`Sender`]`>>` instead:
//! `install_channel_at_minit` enforces the "set once" semantic by
//! checking `is_some()` before populating, and
//! `drain_and_join_at_mshutdown` takes the Sender out and explicitly
//! drops it. The "process-global Sender" spec wording is preserved;
//! only the container type differs. Recorded in `COMMENTS.md` C-13.

mod encode;
mod http;
mod on_batch;

use crate::recorder::types::PendingBatch;
use on_batch::{OnBatch, OnBatchOutcome};

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvError, RecvTimeoutError, Sender};

use crate::recorder::types::ShipperMessage;

/// Outcome of one full run of [`run_loop`].
///
/// Named fields rather than a tuple so that slices 2 / 3 can grow new
/// counters (encode failures, retry exhausts, etc.) without breaking
/// match exhaustiveness at the callers.
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub(crate) struct ShipperExit {
    /// Number of `ShipperMessage::Batch(_)` values consumed during
    /// the entire run (both pre-drain and drain phases). Slice 1
    /// drops each batch on the floor; the count is the only signal
    /// of "did the loop see the work".
    pub(crate) batches_drained: u64,
    /// `true` when the loop exited because the channel closed,
    /// `false` when the loop exited because the drain deadline
    /// elapsed with messages still pending or pollable.
    pub(crate) drain_completed: bool,
    /// Lower-bound estimate of how many `Batch` messages were still
    /// queued at the moment the deadline lapsed. Read from
    /// [`crossbeam_channel::Receiver::len`] at the timeout, so the
    /// number reflects what was in the channel queue at deadline
    /// time; it does not include a `Drain` message that might still
    /// be in flight (the loop has already consumed `Drain` before
    /// reaching the deadline branch).
    pub(crate) batches_abandoned_at_deadline: u64,
}

/// Outcome of [`drain_and_join_at_mshutdown`].
///
/// Slice 1 never reads the inner [`ShipperExit`] in production — it
/// is preserved for slice 2's encoder, which will surface
/// `batches_abandoned_at_deadline` to an `E_NOTICE` log line on the
/// drop-at-shutdown path. The `#[allow(dead_code)]` on `Clean`'s
/// payload below documents this: the field is read by tests (via the
/// derived `Debug`) and will be read in production by slice 2.
#[derive(Debug)]
pub(crate) enum JoinOutcome {
    /// The thread joined cleanly with the given exit summary.
    Clean(#[allow(dead_code, reason = "slice 2 will read the exit summary")] ShipperExit),
    /// The thread panicked during its loop. Slice 2 will surface
    /// this as an `E_NOTICE`; slice 1 silently discards.
    Panicked,
    /// Either the channel was never installed (disabled extension)
    /// or the thread was never spawned (enabled extension that
    /// crashed before its first `RINIT`). Returned without joining
    /// or panicking.
    NotInstalled,
}

// --- Process-global state -------------------------------------------------

/// Canonical Sender slot.
///
/// `Mutex<Option<Sender>>` rather than `OnceLock<Sender>` so that
/// [`drain_and_join_at_mshutdown`] can `take` the Sender and drop it,
/// closing the channel and unblocking the shipper loop. See the
/// module doc's "Design deviation" section.
static SENDER_SLOT: Mutex<Option<Sender<ShipperMessage>>> = Mutex::new(None);

/// Receiver slot. Populated at MINIT alongside the Sender; taken out
/// by the first spawn that wins the [`SHIPPER_SPAWNED`] CAS.
static RECEIVER_SLOT: Mutex<Option<Receiver<ShipperMessage>>> = Mutex::new(None);

/// One-shot spawn guard. Set to `true` by the winner of the
/// [`compare_exchange`](AtomicBool::compare_exchange) in
/// [`spawn_if_needed_at_rinit`]. Reset back to `false` only if the
/// winner discovers the receiver slot is empty (the install step
/// was skipped) so a later, properly-installed RINIT can still
/// spawn. The reset window cannot leak in production because MINIT
/// runs before any RINIT — but it shows up in tests and the
/// invariant is small.
static SHIPPER_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Stashed JoinHandle so [`drain_and_join_at_mshutdown`] can join the
/// thread. Taken out at shutdown.
static SHIPPER_HANDLE: Mutex<Option<JoinHandle<ShipperExit>>> = Mutex::new(None);

/// MSHUTDOWN drain deadline, published by
/// [`drain_and_join_at_mshutdown`] **before** the `Drain` message is
/// sent on the channel. The shipper's pre-drain loop reads this cell
/// at the head of each iteration; once observed as `Some(_)`, the
/// loop transitions to the deadline-aware recv body (the same body
/// the post-`Drain` phase uses), bounding each in-flight batch's
/// `run_with_retry` budget by the published deadline.
///
/// The cell goes `None → Some(_)` exactly once per OS process.
/// Production code never writes `None`; only test-only `reset_for_test`
/// and the sibling `set_drain_deadline_for_test` / `clear_drain_deadline_for_test`
/// helpers reach in to mutate it.
///
/// The publish-before-send ordering is what makes AC-BS-4 / AC-PB-2
/// hold under slice-3 per-batch work (encode + POST + retry): even if
/// the channel is saturated and the `Drain` message itself is stuck
/// behind a pile of pre-`Drain` batches, the shipper's next loop
/// iteration reads this cell and transitions to the deadline-aware
/// path without waiting for the `Drain`.
static DRAIN_DEADLINE: Mutex<Option<Instant>> = Mutex::new(None);

/// Read the MSHUTDOWN drain deadline cell. Returns `None` until
/// [`publish_drain_deadline`] has been called. Used by [`run_loop`]'s
/// pre-drain phase to detect MSHUTDOWN without waiting for the
/// `Drain` message in the channel.
pub(crate) fn drain_deadline_snapshot() -> Option<Instant> {
    *DRAIN_DEADLINE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Publish the MSHUTDOWN drain deadline. Called exactly once per
/// process by [`drain_and_join_at_mshutdown`] immediately before the
/// `Drain` message is sent and the canonical `Sender` is dropped.
///
/// In production the cell is `None` on entry; the `debug_assert!`
/// catches a future refactor that double-publishes. Tests that need
/// to model a republish (e.g. cell-cleared-between-MSHUTDOWNs) reach
/// for [`set_drain_deadline_for_test`] instead, which has no such
/// assertion.
pub(crate) fn publish_drain_deadline(deadline: Instant) {
    let mut slot = DRAIN_DEADLINE.lock().unwrap_or_else(|e| e.into_inner());
    debug_assert!(
        slot.is_none(),
        "DRAIN_DEADLINE is monotonic None → Some; production code publishes at most once per process"
    );
    *slot = Some(deadline);
}

/// `SPECIFICATION.md` §5.2 step 4 drop-notice queue.
///
/// The shipper thread produces "one `E_NOTICE` per dropped batch"
/// lines, but the shipper thread is a background OS thread: calling
/// `ext_php_rs::error::php_error` (which is the canonical
/// `E_NOTICE` emit path used by `bootstrap::report_warning` for
/// `E_WARNING`) from a non-PHP thread is undefined behaviour —
/// `zend_error_va_list` reads TSRM / `EG(...)` globals that are
/// only bound on the PHP-thread side. The queue here decouples the
/// two: the shipper formats the spec line and pushes it onto the
/// `Mutex<VecDeque>`; the next PHP-thread `RSHUTDOWN` (and the
/// process-final `MSHUTDOWN`) drain it and emit each line via
/// `php_error(E_NOTICE, ...)`.
///
/// Drains during `MSHUTDOWN` happen **after** the shipper join
/// returns, so any notices pushed by the deadline-or-cleanup drain
/// path are visible too.
static DROP_NOTICE_QUEUE: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// Producer side: the shipper thread pushes a formatted drop line.
/// Visibility is `pub(crate)` so [`drained_consume`] can call it
/// from inside [`run_loop`].
pub(crate) fn push_drop_notice(notice: String) {
    let mut q = DROP_NOTICE_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    q.push_back(notice);
}

/// Consumer side: the bootstrap layer drains the queue on each
/// `RSHUTDOWN` and once more during `MSHUTDOWN` after the shipper
/// join. Returns the queued lines in push order; the caller is
/// expected to feed each through `php_error(E_NOTICE, &line)`.
pub(crate) fn drain_drop_notices() -> Vec<String> {
    let mut q = DROP_NOTICE_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    q.drain(..).collect()
}

// --- Pure run_loop --------------------------------------------------------

/// Drain the channel until a clean termination condition.
///
/// State machine:
///
/// 1. **Pre-drain phase**: block on [`Receiver::recv`]. Each
///    [`ShipperMessage::Batch`] is counted, its `size_estimate` is
///    subtracted from [`crate::recorder::accounting::BYTES_IN_MEMORY`]
///    (the Phase-4 slice-2 second leg of the budget round-trip:
///    the producer billed the bytes on accept, the consumer
///    returns them on consume), and the batch is dropped. A
///    [`ShipperMessage::Drain`] transitions to the drain phase. An
///    `Err(RecvError)` (the channel closed without a `Drain`) is a
///    **clean close pre-drain** — return with
///    `drain_completed: true` and `batches_abandoned_at_deadline:
///    0`.
/// 2. **Drain phase**: block on [`Receiver::recv_deadline`]. Each
///    `Batch` is counted **and** its `size_estimate` is subtracted
///    from the budget atomic exactly as in the pre-drain phase. A
///    second `Drain` is tolerated (ignored). Termination conditions:
///    - **Clean close post-drain**: `Err(RecvTimeoutError::Disconnected)` —
///      return with `drain_completed: true`,
///      `batches_abandoned_at_deadline: 0`.
///    - **Deadline pass**: `Err(RecvTimeoutError::Timeout)` — drain
///      the residual queue via `try_recv` so abandoned batches do
///      not leak their `size_estimate` across `pm.max_requests`
///      worker recycles. Each abandoned batch contributes to
///      `batches_abandoned_at_deadline`. Return with
///      `drain_completed: false`.
///
/// Slice-3 (Phase-4 `shipper-encoder-and-http`) wires the consume
/// step to an [`OnBatch`] implementation. Each received batch is
/// encoded (via [`http::encode_and_handle`]) and handed to
/// `on_batch.handle`, which performs the HTTP POST + retry/backoff
/// for production wiring or simply records the bytes for tests.
/// The `accounting::sub` location moves from "on receive" to "after
/// encode" per slice-3 design D-3 — the encoded bytes are
/// short-lived and not budgeted, so the budget is released once
/// encoding completes. A `Dropped` outcome bumps the source trace's
/// `drop_counter` (closing the §11 R-13 contract for HTTP-side drops
/// the same way the recorder closes it for channel-full and
/// buffer-cap drops); only a `Sent` outcome contributes to
/// `batches_drained`. Encode failures (`OnBatchOutcome::Dropped {
/// reason: EncodeFailed, .. }`) bump the drop counter the same way.
pub(crate) fn run_loop(rx: Receiver<ShipperMessage>, mut on_batch: impl OnBatch) -> ShipperExit {
    let mut batches_drained: u64 = 0;
    // Snapshot of the [`DRAIN_DEADLINE`] cell. `None` means no MSHUTDOWN
    // deadline is in effect yet; the loop re-reads the cell at the top
    // of each iteration. Once observed as `Some(_)`, the loop
    // transitions to `run_drain_phase` and never re-reads the cell
    // (the value is monotonic `None → Some` by construction).
    let mut local_deadline: Option<Instant> = None;
    loop {
        if local_deadline.is_none() {
            local_deadline = drain_deadline_snapshot();
        }
        if let Some(d) = local_deadline {
            return run_drain_phase(&rx, &mut on_batch, d, batches_drained);
        }
        match rx.recv() {
            Ok(ShipperMessage::Batch(batch)) => {
                drained_consume(&batch, &mut on_batch, None, &mut batches_drained);
                drop(batch);
            }
            Ok(ShipperMessage::Drain { deadline }) => {
                return run_drain_phase(&rx, &mut on_batch, deadline, batches_drained);
            }
            Err(RecvError) => {
                return ShipperExit {
                    batches_drained,
                    drain_completed: true,
                    batches_abandoned_at_deadline: 0,
                };
            }
        }
    }
}

/// The shared deadline-aware recv body, entered when either the
/// [`DRAIN_DEADLINE`] cell is observed `Some(_)` or a
/// `ShipperMessage::Drain { deadline }` is popped from the channel.
///
/// `recv_deadline(deadline)` blocks until either a message arrives or
/// the deadline lapses. Each consumed `Batch` is passed to
/// `drained_consume(.., Some(deadline), ..)` so the per-batch
/// `run_with_retry` budget is bounded by the same deadline. A second
/// `Drain { .. }` message in the channel is tolerated (ignored after
/// a `debug_assert_eq!` that its deadline agrees with the entry
/// deadline; this is the "both signals delivered, both agree by
/// construction" case).
///
/// Termination:
///
/// - `Err(RecvTimeoutError::Disconnected)`: the channel closed
///   cleanly. Return `drain_completed: true`,
///   `batches_abandoned_at_deadline: 0`.
/// - `Err(RecvTimeoutError::Timeout)`: the deadline passed. Drain
///   the residual queue via `try_recv` so abandoned batches return
///   their `size_estimate` to the budget, count each abandoned
///   batch, and return `drain_completed: false`.
fn run_drain_phase(
    rx: &Receiver<ShipperMessage>,
    on_batch: &mut impl OnBatch,
    deadline: Instant,
    mut batches_drained: u64,
) -> ShipperExit {
    loop {
        match rx.recv_deadline(deadline) {
            Ok(ShipperMessage::Batch(batch)) => {
                drained_consume(&batch, on_batch, Some(deadline), &mut batches_drained);
                drop(batch);
            }
            // A second Drain is structurally unreachable today (only
            // `drain_and_join_at_mshutdown` sends Drain, and only
            // once), but tolerating it costs nothing and removes a
            // future-bug class. When both signals are live, the
            // deadlines agree by construction — both are computed
            // from the same local in `drain_and_join_at_mshutdown`.
            Ok(ShipperMessage::Drain {
                deadline: drain_msg_deadline,
            }) => {
                debug_assert_eq!(
                    deadline, drain_msg_deadline,
                    "cell-published and Drain-message deadlines must agree",
                );
            }
            Err(RecvTimeoutError::Timeout) => {
                // Drain the residual queue so abandoned batches
                // return their bytes to the budget. We do NOT
                // encode-and-POST abandoned batches — the deadline
                // has passed; we just balance accounting and count
                // them.
                let mut abandoned: u64 = 0;
                loop {
                    match rx.try_recv() {
                        Ok(ShipperMessage::Batch(batch)) => {
                            crate::recorder::accounting::sub(batch.size_estimate);
                            abandoned += 1;
                            drop(batch);
                        }
                        Ok(ShipperMessage::Drain { .. }) => {}
                        Err(_) => break,
                    }
                }
                return ShipperExit {
                    batches_drained,
                    drain_completed: false,
                    batches_abandoned_at_deadline: abandoned,
                };
            }
            Err(RecvTimeoutError::Disconnected) => {
                return ShipperExit {
                    batches_drained,
                    drain_completed: true,
                    batches_abandoned_at_deadline: 0,
                };
            }
        }
    }
}

/// Per-batch consume step: encode, hand to `on_batch`, bump the
/// `drop_counter` on `Dropped`, release the budget, queue the
/// `SPECIFICATION.md` §5.2 step-4 drop-notice line, and advance the
/// `batches_drained` counter on `Sent`.
///
/// Factored out of `run_loop` so the pre-drain and post-drain
/// branches share one expression and the slice-3 design D-3 ordering
/// rule lives in exactly one place. The canonical order is
/// **encode → bump drop counter → accounting::sub → format/queue
/// notice → count**: bump the drop counter first so the cross-thread
/// `Arc<AtomicU64>` invariant (a future batch from the same trace
/// surfaces this drop in its `meta.dropped_records`) is established
/// before the byte budget is released; release the budget next so a
/// hypothetical encode-panic does not double-subtract; format and
/// queue the notice last because it can be deferred without
/// affecting accounting correctness.
fn drained_consume(
    batch: &PendingBatch,
    on_batch: &mut impl OnBatch,
    deadline: Option<Instant>,
    batches_drained: &mut u64,
) {
    let outcome = http::encode_and_handle(batch, on_batch, deadline);
    http::bump_drop_counter_on_drop(batch, &outcome);
    crate::recorder::accounting::sub(batch.size_estimate);
    if let OnBatchOutcome::Dropped { reason, attempts } = &outcome {
        let notice = format_drop_notice(
            batch,
            on_batch.server_url().unwrap_or(""),
            *reason,
            *attempts,
        );
        push_drop_notice(notice);
    }
    if matches!(outcome, OnBatchOutcome::Sent) {
        *batches_drained += 1;
    }
}

/// Render the `SPECIFICATION.md` §5.2 step-4 drop line:
///
/// > `php-analyze: dropped <N> records from trace <uuid>: <url> <status_or_error> (attempt <K>)`
///
/// `<uuid>` uses the same 36-char hyphenated render as
/// [`crate::shipper::encode::meta_partial_to_wire`] so the trace ID
/// rendered on the wire matches the one operators see in the error
/// log. `<status_or_error>` is the [`on_batch::DropReason`]
/// `Display` token (`http 401`, `timeout`, `tls_error`, …); the
/// bearer token plaintext never appears in this string because
/// `DropReason` does not carry it (AC-SH-4 enforced by
/// construction).
fn format_drop_notice(
    batch: &PendingBatch,
    server_url: &str,
    reason: on_batch::DropReason,
    attempts: u32,
) -> String {
    let trace_id = uuid::Uuid::from_bytes(batch.meta_partial.trace_id);
    format!(
        "php-analyze: dropped {} records from trace {}: {} {} (attempt {})",
        batch.calls.len(),
        trace_id,
        server_url,
        reason,
        attempts,
    )
}

// --- Lifecycle entry points ------------------------------------------------

/// Install the shipper channel into the process-global slots. Called
/// from `MINIT` when `Config::enabled` is `true`. The first call
/// wins; subsequent calls in the same process are a no-op
/// (idempotent so a misconfigured PHP that runs MINIT twice does not
/// orphan a receiver).
pub(crate) fn install_channel_at_minit(depth: usize) {
    let mut sender_slot = SENDER_SLOT.lock().unwrap_or_else(|e| e.into_inner());
    if sender_slot.is_some() {
        return;
    }
    let (tx, rx) = bounded(depth);
    *sender_slot = Some(tx);
    let mut receiver_slot = RECEIVER_SLOT.lock().unwrap_or_else(|e| e.into_inner());
    *receiver_slot = Some(rx);
}

/// Production entry point. Spawn the shipper thread on the first
/// `RINIT` per process with the real [`http::RmpEncodeAndHttpPost`]
/// `OnBatch` implementation built from the supplied `Config`.
///
/// The bootstrap layer calls this once per process; it is idempotent
/// via the [`SHIPPER_SPAWNED`] CAS guard.
pub(crate) fn spawn_if_needed_at_rinit(config: &crate::config::Config) {
    let server_url = config
        .server_url
        .clone()
        .expect("Config::server_url is Some when Config::enabled is true");
    let retry_count = u32::from(config.retry_count);
    let retry_backoff = config.retry_backoff;
    let http_timeout = config.http_timeout;
    let auth_token = config.auth_token.clone();
    spawn_with_on_batch_factory(move || {
        http::RmpEncodeAndHttpPost::new(
            server_url,
            auth_token,
            retry_count,
            retry_backoff,
            http_timeout,
        )
    });
}

/// Shared spawn machinery — does the CAS, takes the receiver, and
/// spawns [`run_loop`] with the [`OnBatch`] produced by
/// `make_on_batch`. The factory is invoked at most once per
/// successful CAS; if the CAS loses or the receiver slot is empty,
/// `make_on_batch` is never called.
///
/// Visibility is `pub(crate)` so tests can plumb a
/// [`on_batch::RecordingOnBatch`] without touching `Config::global()`.
pub(crate) fn spawn_with_on_batch_factory<O: OnBatch + Send + 'static>(
    make_on_batch: impl FnOnce() -> O,
) {
    // Success ordering: `Acquire`. Pairs with the install step's
    // mutex release, establishing a happens-before edge with the
    // subsequent receiver take. Failure ordering: `Relaxed`, since
    // the loser does no further reads.
    if SHIPPER_SPAWNED
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let rx = {
        let mut slot = RECEIVER_SLOT.lock().unwrap_or_else(|e| e.into_inner());
        slot.take()
    };
    let Some(rx) = rx else {
        // No channel installed. Revert the CAS so a later RINIT
        // (after a properly-installed channel) can still spawn.
        // `Relaxed` is sufficient — no associated state to publish.
        SHIPPER_SPAWNED.store(false, Ordering::Relaxed);
        return;
    };
    let on_batch = make_on_batch();
    let handle = thread::Builder::new()
        .name("php-analyze-shipper".to_owned())
        .spawn(move || run_loop(rx, on_batch))
        .expect("OS thread spawn for the shipper failed");
    let mut handle_slot = SHIPPER_HANDLE.lock().unwrap_or_else(|e| e.into_inner());
    *handle_slot = Some(handle);
}

/// Test-only shim that mirrors slice 1's parameterless
/// `spawn_if_needed_at_rinit` shape. Spawns with an always-`Sent`
/// [`on_batch::RecordingOnBatch`] — the slice-1 "drain silently"
/// behaviour is preserved exactly under the new generic
/// [`run_loop`] signature.
#[cfg(test)]
pub(crate) fn spawn_if_needed_at_rinit_for_test() {
    spawn_with_on_batch_factory(|| on_batch::RecordingOnBatch::new(Vec::new()));
}

/// Send `Drain { deadline: now + grace }`, drop the canonical
/// Sender, and join the shipper thread. The `+ 200 ms` slack
/// referenced by `SPECIFICATION.md` AC-BS-4 / AC-PB-2 is the budget
/// for the channel-close + `recv_deadline`-loop-exit + `JoinHandle::join`
/// overhead.
///
/// ## Two-signal protocol for the MSHUTDOWN deadline
///
/// 1. **Cell publish (primary)**: write `Some(deadline)` to the
///    process-global [`DRAIN_DEADLINE`] cell. The shipper's pre-drain
///    loop snapshots this cell at the head of each iteration; once
///    observed as `Some(_)`, the loop transitions to the
///    deadline-aware recv body without waiting for the `Drain`
///    message.
/// 2. **`Drain` message (secondary)**: send `ShipperMessage::Drain { deadline }`
///    via `send_timeout(grace)`. Carries the same `deadline` value as
///    the cell; serves as a redundant signal for code paths that
///    bypass this function (tests pushing `Drain` directly into the
///    channel).
///
/// The publish-before-send ordering matters: under a saturated
/// channel with slow per-batch work (slice 3's encode + POST + retry
/// can take `(retry_count + 1) × http_timeout_ms` per batch), the
/// `send_timeout` may run out before the `Drain` reaches the front
/// of the queue. The cell publish gives the shipper an out-of-band
/// signal that the recv-loop head observes regardless of channel
/// state. The mutex's release-on-drop semantics establish a
/// happens-before edge with the shipper's next snapshot read.
///
/// A panicking shipper thread is turned into
/// [`JoinOutcome::Panicked`]; the function itself does not panic, so
/// a downstream slice's encoder / HTTP error cannot escape across
/// the FFI boundary into PHP.
pub(crate) fn drain_and_join_at_mshutdown(grace: Duration) -> JoinOutcome {
    let sender = {
        let mut slot = SENDER_SLOT.lock().unwrap_or_else(|e| e.into_inner());
        slot.take()
    };
    let Some(sender) = sender else {
        return JoinOutcome::NotInstalled;
    };
    let deadline = Instant::now() + grace;
    // Publish the deadline **before** sending the `Drain` message
    // (and before dropping the canonical `Sender`). The shipper's
    // next pre-drain loop iteration reads the cell and transitions
    // to the deadline-aware path even if the `Drain` is stuck behind
    // a saturated queue — this is what makes AC-BS-4 / AC-PB-2 hold
    // under slice-3's per-batch encode + POST + retry work.
    publish_drain_deadline(deadline);
    // `send_timeout(grace)` (not `try_send`) so the Drain message
    // also reaches the shipper when the channel is momentarily full.
    // The cell publish above is the primary signal; the Drain is a
    // redundant secondary so test code paths that bypass this
    // function still observe a deadline.
    let _ = sender.send_timeout(ShipperMessage::Drain { deadline }, grace);
    // Dropping the canonical Sender (and any clones held by future
    // slice-2 producers, all of which are already gone at MSHUTDOWN
    // because RSHUTDOWN dropped them) closes the channel; the
    // shipper loop sees `Disconnected` and exits cleanly.
    drop(sender);
    let handle = {
        let mut slot = SHIPPER_HANDLE.lock().unwrap_or_else(|e| e.into_inner());
        slot.take()
    };
    let Some(handle) = handle else {
        return JoinOutcome::NotInstalled;
    };
    match handle.join() {
        Ok(exit) => JoinOutcome::Clean(exit),
        Err(_) => JoinOutcome::Panicked,
    }
}

// --- Test surface ---------------------------------------------------------

#[cfg(test)]
pub(crate) fn acquire_test_lock() -> std::sync::MutexGuard<'static, ()> {
    tests::lock()
}

/// Test-only: clear every process-global slot so the next test
/// starts from a known empty state. Production code never resets
/// any of these slots (the process exits at MSHUTDOWN).
#[cfg(test)]
pub(crate) fn reset_for_test() {
    *SENDER_SLOT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *RECEIVER_SLOT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *SHIPPER_HANDLE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *DRAIN_DEADLINE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    SHIPPER_SPAWNED.store(false, Ordering::SeqCst);
    DROP_NOTICE_QUEUE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// Test-only: write the [`DRAIN_DEADLINE`] cell directly, bypassing
/// the `debug_assert!(slot.is_none())` in [`publish_drain_deadline`].
/// Tests use this seam to model "deadline published, but `Drain`
/// message never sent" — the binding shape of the
/// `run_loop_with_cell_publish_and_no_drain_message_*` scenarios.
#[cfg(test)]
pub(crate) fn set_drain_deadline_for_test(deadline: Instant) {
    *DRAIN_DEADLINE.lock().unwrap_or_else(|e| e.into_inner()) = Some(deadline);
}

/// Test-only: clear the [`DRAIN_DEADLINE`] cell back to `None` without
/// touching the other process-global slots. Sibling of
/// [`set_drain_deadline_for_test`]; [`reset_for_test`] is the broader
/// hammer.
#[cfg(test)]
pub(crate) fn clear_drain_deadline_for_test() {
    *DRAIN_DEADLINE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Test-only: peek the canonical Sender's `is_some()` state without
/// holding the lock past return. Used by the disabled-config tests
/// in this crate's `bootstrap` module to assert the silent-disable
/// posture doesn't install a channel.
#[cfg(test)]
pub(crate) fn sender_is_installed() -> bool {
    SENDER_SLOT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

/// Test-only: peek the JoinHandle slot's `is_some()` state.
#[cfg(test)]
pub(crate) fn handle_is_installed() -> bool {
    SHIPPER_HANDLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

/// Test-only: peek the `SHIPPER_SPAWNED` flag.
#[cfg(test)]
pub(crate) fn spawned_flag() -> bool {
    SHIPPER_SPAWNED.load(Ordering::Relaxed)
}

/// Test-only: peek the receiver slot's `is_some()` state.
#[cfg(test)]
pub(crate) fn receiver_is_installed() -> bool {
    RECEIVER_SLOT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

/// Test-only: install a hand-built `Sender` into the canonical
/// [`SENDER_SLOT`] so a test can wire up a channel without standing
/// up an actual shipper thread. Used by `recorder::flush::tests` to
/// drive `try_send_batch`'s Sender-present arms against a
/// test-controlled `Receiver`.
///
/// The slot is replaced atomically under the same mutex production
/// code uses, so a parallel test cannot observe a half-installed
/// state.
#[cfg(test)]
pub(crate) fn install_test_sender(tx: Sender<ShipperMessage>) {
    *SENDER_SLOT.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
}

/// Test-only: clone the canonical Sender so a test can push synthetic
/// `Batch` / `Drain` messages through the same channel the shipper
/// thread is reading. Returns `None` if no channel is installed.
#[cfg(test)]
pub(crate) fn clone_sender_for_test() -> Option<Sender<ShipperMessage>> {
    SENDER_SLOT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .cloned()
}

/// Production accessor used by [`recorder::flush::try_send_batch`] to
/// reach the canonical Sender for a single `try_send` of one
/// `PendingBatch`. Returns `None` when no channel has been installed
/// (silent-disable or never-installed); the caller treats that as the
/// "no-Sender" arm.
///
/// The clone is cheap (`crossbeam_channel::Sender` is internally an
/// `Arc` over the shared queue) so the per-flush cost is one mutex
/// acquire + one atomic increment; the slice-1 deviation
/// (`Mutex<Option<Sender>>` rather than `OnceLock<Sender>`) keeps the
/// shutdown-time drop simple at the cost of this single mutex per
/// flush, which is bounded by `flush_records`-worth of work between
/// flushes.
///
/// Mutex poisoning is treated as benign per the slice-1 pattern: a
/// poisoned mutex still holds the canonical Sender, and the slot is
/// only ever written under that same mutex, so reading through the
/// poison guard is sound.
///
/// [`recorder::flush::try_send_batch`]: crate::recorder::flush::try_send_batch
pub(crate) fn clone_canonical_sender() -> Option<Sender<ShipperMessage>> {
    SENDER_SLOT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .cloned()
}

/// Test-only: install a panicking thread's `JoinHandle` plus a stub
/// channel so [`drain_and_join_at_mshutdown`] exercises its
/// `Err(_)`-arm classification without depending on the real
/// [`run_loop`]. The panicking thread returns `!` which coerces to
/// `ShipperExit`; the `Err(_)` from `JoinHandle::join` is what
/// `drain_and_join_at_mshutdown` should map to
/// [`JoinOutcome::Panicked`].
#[cfg(test)]
pub(crate) fn install_panicking_handle_for_test() {
    let (tx, _rx) = bounded::<ShipperMessage>(1);
    *SENDER_SLOT.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    let handle = thread::Builder::new()
        .name("php-analyze-shipper-panic-test".to_owned())
        .spawn(|| -> ShipperExit {
            // Panic immediately, before any recv. The locally-bound
            // `_rx` drops when `install_panicking_handle_for_test`
            // returns; the panicking thread itself never touches the
            // receiver.
            panic!("intentional panic for drain_and_join_at_mshutdown test");
        })
        .expect("OS thread spawn for the panic-injection test failed");
    // Give the spawned thread a moment to actually panic before the
    // test calls `drain_and_join_at_mshutdown` and joins it. Not
    // strictly required (join() is happy with a still-running
    // panicking thread), but tightens the test's intent.
    while !handle.is_finished() {
        std::thread::yield_now();
    }
    *SHIPPER_HANDLE.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialises every test in this module that touches the
    /// process-global slots ([`SENDER_SLOT`], [`RECEIVER_SLOT`],
    /// [`SHIPPER_SPAWNED`], [`SHIPPER_HANDLE`]). The accounting
    /// module follows the same pattern at `recorder::accounting::tests::TEST_LOCK`.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    pub(super) fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Build a placeholder `PendingBatch` for tests that just need
    /// *something* of variant `Batch` to push through the channel.
    /// Slice 1 never reads any field of a `PendingBatch` beyond the
    /// discriminant; Phase-4 slice 2 reads `size_estimate` on the
    /// consume path to subtract from `accounting::BYTES_IN_MEMORY`, so
    /// the variant carries a real (though tunable) value below. The
    /// fresh `Arc<AtomicU64>` matches the slice-2 wire-shape requirement
    /// and is otherwise inert.
    fn dummy_batch() -> ShipperMessage {
        dummy_batch_with_size(0)
    }

    /// Sibling of [`dummy_batch`] that lets a test seed the
    /// `size_estimate` so the post-consume `accounting::snapshot()` can
    /// be asserted deterministically (Phase-4 slice 2 §6.5–6.8).
    fn dummy_batch_with_size(size_estimate: usize) -> ShipperMessage {
        use crate::recorder::types::{MetaPartial, PendingBatch};
        use std::sync::atomic::AtomicU64;
        use std::sync::Arc;
        let meta_partial = MetaPartial {
            schema_version: 1,
            trace_id: [0u8; 16],
            host: Arc::from("test-host"),
            pid: std::process::id(),
            start_time_realtime_ns: 0,
            sapi: Arc::from("cli"),
            uri_or_script: Arc::from("/test"),
        };
        ShipperMessage::Batch(PendingBatch {
            meta_partial,
            dict: Vec::new(),
            calls: Vec::new(),
            size_estimate,
            drop_counter: Arc::new(AtomicU64::new(0)),
        })
    }

    // --- run_loop -------------------------------------------------------

    #[test]
    fn run_loop_drains_three_batches_and_exits_cleanly_on_channel_close() {
        let (tx, rx) = bounded::<ShipperMessage>(8);
        let handle =
            thread::spawn(move || run_loop(rx, on_batch::RecordingOnBatch::new(Vec::new())));
        for _ in 0..3 {
            tx.send(dummy_batch()).expect("send batch");
        }
        drop(tx);
        let exit = handle.join().expect("shipper joined cleanly");
        assert_eq!(
            exit,
            ShipperExit {
                batches_drained: 3,
                drain_completed: true,
                batches_abandoned_at_deadline: 0,
            }
        );
    }

    #[test]
    fn run_loop_with_drain_future_deadline_finishes_queued_batches() {
        let (tx, rx) = bounded::<ShipperMessage>(8);
        let handle =
            thread::spawn(move || run_loop(rx, on_batch::RecordingOnBatch::new(Vec::new())));
        let start = Instant::now();
        tx.send(dummy_batch()).unwrap();
        tx.send(dummy_batch()).unwrap();
        tx.send(ShipperMessage::Drain {
            deadline: Instant::now() + Duration::from_secs(5),
        })
        .unwrap();
        tx.send(dummy_batch()).unwrap();
        drop(tx);
        let exit = handle.join().expect("shipper joined cleanly");
        let elapsed = start.elapsed();
        assert_eq!(
            exit,
            ShipperExit {
                batches_drained: 3,
                drain_completed: true,
                batches_abandoned_at_deadline: 0,
            }
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "exit should not wait for the 5s deadline; took {elapsed:?}"
        );
    }

    #[test]
    fn run_loop_with_drain_past_deadline_abandons_queued_batches() {
        let (tx, rx) = bounded::<ShipperMessage>(128);
        for _ in 0..100 {
            tx.send(dummy_batch()).unwrap();
        }
        tx.send(ShipperMessage::Drain {
            // 1ms ago; the loop's recv_deadline returns Timeout
            // immediately, with the 100 batches still in the queue.
            deadline: Instant::now() - Duration::from_millis(1),
        })
        .unwrap();
        // Keep the Sender alive so the channel does NOT close — that
        // way the exit must come from the deadline branch, not the
        // Disconnected branch.
        let start = Instant::now();
        let handle =
            thread::spawn(move || run_loop(rx, on_batch::RecordingOnBatch::new(Vec::new())));
        let exit = handle.join().expect("shipper joined cleanly");
        let elapsed = start.elapsed();
        assert!(!exit.drain_completed, "deadline-pass exit, got {exit:?}");
        assert_eq!(
            exit.batches_drained + exit.batches_abandoned_at_deadline,
            100,
            "every batch is accounted for: {exit:?}"
        );
        assert!(
            elapsed < Duration::from_millis(200),
            "deadline exit should be prompt; took {elapsed:?}"
        );
        // Now drop the sender so the test doesn't keep the channel
        // alive past the function.
        drop(tx);
    }

    #[test]
    fn run_loop_exits_cleanly_on_channel_close_without_a_drain() {
        let (tx, rx) = bounded::<ShipperMessage>(8);
        let handle =
            thread::spawn(move || run_loop(rx, on_batch::RecordingOnBatch::new(Vec::new())));
        tx.send(dummy_batch()).unwrap();
        drop(tx);
        let exit = handle.join().expect("shipper joined cleanly");
        assert!(exit.drain_completed);
        assert_eq!(exit.batches_drained, 1);
        assert_eq!(exit.batches_abandoned_at_deadline, 0);
    }

    #[test]
    fn run_loop_with_empty_channel_and_immediate_close_returns_zero_counts() {
        let (tx, rx) = bounded::<ShipperMessage>(8);
        let handle =
            thread::spawn(move || run_loop(rx, on_batch::RecordingOnBatch::new(Vec::new())));
        drop(tx);
        let exit = handle.join().expect("shipper joined cleanly");
        assert_eq!(
            exit,
            ShipperExit {
                batches_drained: 0,
                drain_completed: true,
                batches_abandoned_at_deadline: 0,
            }
        );
    }

    // --- Phase-4 slice 2: consume-path accounting subtract --------------

    #[test]
    fn run_loop_pre_drain_subtracts_size_estimate_for_each_consumed_batch() {
        let _account_guard = crate::recorder::accounting::acquire_test_lock();
        crate::recorder::accounting::reset_for_test();

        // Seed the budget with the sum of three batches' size_estimates.
        // Each batch carries `100` bytes; after the shipper consumes
        // all three, the snapshot must return to zero.
        crate::recorder::accounting::add(300);

        let (tx, rx) = bounded::<ShipperMessage>(8);
        let handle =
            thread::spawn(move || run_loop(rx, on_batch::RecordingOnBatch::new(Vec::new())));
        for _ in 0..3 {
            tx.send(dummy_batch_with_size(100)).unwrap();
        }
        drop(tx);
        let exit = handle.join().expect("shipper joined cleanly");

        assert_eq!(exit.batches_drained, 3);
        assert!(exit.drain_completed);
        assert_eq!(
            crate::recorder::accounting::snapshot(),
            0,
            "every consumed batch's size_estimate is returned to the budget",
        );
    }

    #[test]
    fn run_loop_drain_phase_subtracts_size_estimate_for_future_deadline() {
        let _account_guard = crate::recorder::accounting::acquire_test_lock();
        crate::recorder::accounting::reset_for_test();
        crate::recorder::accounting::add(500);

        let (tx, rx) = bounded::<ShipperMessage>(8);
        let handle =
            thread::spawn(move || run_loop(rx, on_batch::RecordingOnBatch::new(Vec::new())));
        // Two pre-drain batches, then Drain with a comfortable
        // deadline, then a third batch the drain-phase will consume.
        tx.send(dummy_batch_with_size(100)).unwrap();
        tx.send(dummy_batch_with_size(100)).unwrap();
        tx.send(ShipperMessage::Drain {
            deadline: Instant::now() + Duration::from_secs(5),
        })
        .unwrap();
        tx.send(dummy_batch_with_size(300)).unwrap();
        drop(tx);
        let exit = handle.join().expect("shipper joined cleanly");

        assert_eq!(exit.batches_drained, 3);
        assert!(exit.drain_completed);
        assert_eq!(
            crate::recorder::accounting::snapshot(),
            0,
            "both pre-drain and drain-phase consumes subtract from the budget",
        );
    }

    #[test]
    fn run_loop_deadline_pass_subtracts_size_estimate_for_abandoned_batches() {
        let _account_guard = crate::recorder::accounting::acquire_test_lock();
        crate::recorder::accounting::reset_for_test();
        crate::recorder::accounting::add(1_000);

        let (tx, rx) = bounded::<ShipperMessage>(16);
        // Send 10 batches @ 100 bytes each (total 1000) BEFORE
        // spawning the shipper so the queue is saturated when the
        // loop starts; then send a past-deadline Drain so the
        // shipper exits via the deadline-pass arm. The arm's
        // try_recv drain must subtract every abandoned batch.
        for _ in 0..10 {
            tx.send(dummy_batch_with_size(100)).unwrap();
        }
        tx.send(ShipperMessage::Drain {
            deadline: Instant::now()
                .checked_sub(Duration::from_millis(1))
                .unwrap_or_else(Instant::now),
        })
        .unwrap();
        let handle =
            thread::spawn(move || run_loop(rx, on_batch::RecordingOnBatch::new(Vec::new())));
        let exit = handle.join().expect("shipper joined cleanly");

        assert!(!exit.drain_completed, "deadline-pass exit");
        assert_eq!(
            exit.batches_drained + exit.batches_abandoned_at_deadline,
            10,
            "every batch is accounted for: {exit:?}",
        );
        assert_eq!(
            crate::recorder::accounting::snapshot(),
            0,
            "deadline-pass arm must drain residual batches and subtract their bytes",
        );
        drop(tx);
    }

    #[test]
    fn run_loop_deadline_pass_with_no_residual_returns_zero_abandoned() {
        let _account_guard = crate::recorder::accounting::acquire_test_lock();
        crate::recorder::accounting::reset_for_test();

        let (tx, rx) = bounded::<ShipperMessage>(8);
        tx.send(ShipperMessage::Drain {
            deadline: Instant::now()
                .checked_sub(Duration::from_millis(1))
                .unwrap_or_else(Instant::now),
        })
        .unwrap();
        let start = Instant::now();
        let handle =
            thread::spawn(move || run_loop(rx, on_batch::RecordingOnBatch::new(Vec::new())));
        let exit = handle.join().expect("shipper joined cleanly");
        let elapsed = start.elapsed();

        assert!(!exit.drain_completed);
        assert_eq!(exit.batches_drained, 0);
        assert_eq!(exit.batches_abandoned_at_deadline, 0);
        assert!(
            elapsed < Duration::from_millis(100),
            "deadline-pass with no residual must return promptly; took {elapsed:?}",
        );
        assert_eq!(crate::recorder::accounting::snapshot(), 0);
        drop(tx);
    }

    // --- install_channel_at_minit --------------------------------------

    #[test]
    fn install_channel_at_minit_populates_sender_and_receiver_slot() {
        let _guard = lock();
        reset_for_test();
        assert!(!sender_is_installed());
        assert!(!receiver_is_installed());
        install_channel_at_minit(8);
        assert!(sender_is_installed());
        assert!(receiver_is_installed());
        let tx = clone_sender_for_test().expect("sender installed");
        assert_eq!(tx.capacity(), Some(8));
        reset_for_test();
    }

    #[test]
    fn install_channel_at_minit_is_idempotent_in_the_same_process() {
        let _guard = lock();
        reset_for_test();
        install_channel_at_minit(8);
        let first = clone_sender_for_test().expect("first sender");
        // Second call must NOT replace the first sender — otherwise
        // any clone the recorder is holding would point at a
        // detached, never-read channel.
        install_channel_at_minit(99);
        let second = clone_sender_for_test().expect("second sender");
        // Both clones are of the same underlying channel: `same_channel`
        // returns true iff they share the same internal handle.
        assert!(first.same_channel(&second));
        // The capacity of the underlying channel is the first call's
        // value, not the second call's.
        assert_eq!(second.capacity(), Some(8));
        reset_for_test();
    }

    // --- spawn_if_needed_at_rinit --------------------------------------

    #[test]
    fn spawn_if_needed_at_rinit_spawns_exactly_one_thread_on_first_call() {
        let _guard = lock();
        reset_for_test();
        install_channel_at_minit(8);
        assert!(!spawned_flag());
        assert!(!handle_is_installed());
        spawn_if_needed_at_rinit_for_test();
        assert!(spawned_flag());
        assert!(handle_is_installed());
        // Clean up by draining + joining so the test doesn't leak a
        // thread to the next test.
        let _ = drain_and_join_at_mshutdown(Duration::from_millis(100));
        reset_for_test();
    }

    #[test]
    fn spawn_if_needed_at_rinit_is_a_noop_on_subsequent_calls() {
        let _guard = lock();
        reset_for_test();
        install_channel_at_minit(8);
        spawn_if_needed_at_rinit_for_test();
        // Second call must not double-spawn.
        spawn_if_needed_at_rinit_for_test();
        // The CAS guard is what enforces this — but we also assert
        // the receiver slot stays empty (the second call must not
        // somehow take it again).
        assert!(spawned_flag());
        assert!(handle_is_installed());
        assert!(!receiver_is_installed());
        let _ = drain_and_join_at_mshutdown(Duration::from_millis(100));
        reset_for_test();
    }

    #[test]
    fn spawn_if_needed_at_rinit_is_a_noop_when_no_channel_is_installed() {
        let _guard = lock();
        reset_for_test();
        spawn_if_needed_at_rinit_for_test();
        assert!(!spawned_flag(), "no channel → no spawn → CAS reverted");
        assert!(!handle_is_installed());
        assert!(!sender_is_installed());
        // A later, properly-installed RINIT should still be able to
        // spawn — this is what the revert is for.
        install_channel_at_minit(8);
        spawn_if_needed_at_rinit_for_test();
        assert!(spawned_flag());
        assert!(handle_is_installed());
        let _ = drain_and_join_at_mshutdown(Duration::from_millis(100));
        reset_for_test();
    }

    #[test]
    fn concurrent_spawn_calls_race_to_a_single_thread() {
        use std::sync::{Arc, Barrier};
        let _guard = lock();
        reset_for_test();
        install_channel_at_minit(8);
        let barrier = Arc::new(Barrier::new(3));
        let mut joiners = Vec::with_capacity(3);
        for _ in 0..3 {
            let b = Arc::clone(&barrier);
            joiners.push(thread::spawn(move || {
                b.wait();
                spawn_if_needed_at_rinit_for_test();
            }));
        }
        for j in joiners {
            j.join().expect("test thread joined");
        }
        // Exactly one shipper thread was spawned even though three
        // call sites raced. The other two saw the CAS already won
        // and returned without touching the receiver slot.
        assert!(spawned_flag());
        assert!(handle_is_installed());
        assert!(
            !receiver_is_installed(),
            "exactly one thread took the receiver"
        );
        let outcome = drain_and_join_at_mshutdown(Duration::from_millis(100));
        assert!(matches!(outcome, JoinOutcome::Clean(_)));
        reset_for_test();
    }

    // --- drain_and_join_at_mshutdown -----------------------------------

    #[test]
    fn drain_and_join_at_mshutdown_with_an_empty_channel_returns_clean_in_milliseconds() {
        let _guard = lock();
        reset_for_test();
        install_channel_at_minit(8);
        spawn_if_needed_at_rinit_for_test();
        let start = Instant::now();
        let outcome = drain_and_join_at_mshutdown(Duration::from_secs(5));
        let elapsed = start.elapsed();
        match outcome {
            JoinOutcome::Clean(exit) => {
                assert!(exit.drain_completed, "exit on channel close, not deadline");
            }
            other => panic!("expected Clean, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_millis(100),
            "empty-channel drain must not wait for the deadline; took {elapsed:?}"
        );
        reset_for_test();
    }

    #[test]
    fn drain_and_join_at_mshutdown_respects_the_grace_deadline_under_a_backlog() {
        let _guard = lock();
        reset_for_test();
        install_channel_at_minit(2048);
        spawn_if_needed_at_rinit_for_test();
        // Push 1000 batches before the drain. The shipper will burn
        // through them very quickly (no I/O); the deadline-vs-close
        // race is essentially "whichever happens first". The point
        // of the test is the total wall-time bound.
        let tx = clone_sender_for_test().expect("sender installed");
        for _ in 0..1000 {
            tx.send(dummy_batch()).unwrap();
        }
        drop(tx);
        let start = Instant::now();
        let outcome = drain_and_join_at_mshutdown(Duration::from_millis(50));
        let elapsed = start.elapsed();
        assert!(
            matches!(outcome, JoinOutcome::Clean(_)),
            "shutdown under backlog returns Clean; got {outcome:?}"
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "shutdown bounded by grace + 200ms slack; took {elapsed:?}"
        );
        reset_for_test();
    }

    #[test]
    fn drain_and_join_at_mshutdown_is_a_noop_when_no_channel_was_installed() {
        let _guard = lock();
        reset_for_test();
        let outcome = drain_and_join_at_mshutdown(Duration::from_secs(1));
        assert!(matches!(outcome, JoinOutcome::NotInstalled));
        reset_for_test();
    }

    #[test]
    fn drain_and_join_at_mshutdown_turns_a_panicking_shipper_thread_into_a_clean_panicked_outcome()
    {
        let _guard = lock();
        reset_for_test();
        install_panicking_handle_for_test();
        let outcome = drain_and_join_at_mshutdown(Duration::from_millis(100));
        assert!(
            matches!(outcome, JoinOutcome::Panicked),
            "panicking shipper → Panicked; got {outcome:?}"
        );
        reset_for_test();
    }

    // --- §5.2 step-4 drop-notice queue ---------------------------------

    fn dummy_batch_with_calls(call_count: usize) -> PendingBatch {
        use crate::recorder::types::{CallRecord, MetaPartial, PendingBatch};
        use std::sync::atomic::AtomicU64;
        use std::sync::Arc;
        let calls: Vec<CallRecord> = (0..call_count)
            .map(|i| CallRecord {
                call_id: i as u64 + 1,
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
        let trace_id: [u8; 16] = [
            0x01, 0x91, 0xff, 0xff, 0x00, 0x00, 0x70, 0x00, 0x80, 0x00, 0xde, 0xad, 0xbe, 0xef,
            0xca, 0xfe,
        ];
        PendingBatch {
            meta_partial: MetaPartial {
                schema_version: 1,
                trace_id,
                host: Arc::from("h"),
                pid: 1,
                start_time_realtime_ns: 0,
                sapi: Arc::from("cli"),
                uri_or_script: Arc::from("/x"),
            },
            dict: Vec::new(),
            calls,
            size_estimate: 0,
            drop_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    #[test]
    fn format_drop_notice_matches_spec_5_2_step_4_wording() {
        let batch = dummy_batch_with_calls(7);
        let line = format_drop_notice(
            &batch,
            "http://127.0.0.1:8080/v1/ingest",
            on_batch::DropReason::HttpStatus(401),
            4,
        );
        // Spec wording: `php-analyze: dropped <N> records from trace
        // <uuid>: <url> <status_or_error> (attempt <K>)`.
        assert_eq!(
            line,
            "php-analyze: dropped 7 records from trace \
             0191ffff-0000-7000-8000-deadbeefcafe: \
             http://127.0.0.1:8080/v1/ingest http 401 (attempt 4)",
        );
    }

    #[test]
    fn format_drop_notice_renders_each_drop_reason_token_per_5_2() {
        let batch = dummy_batch_with_calls(1);
        for (reason, expected_token) in [
            (on_batch::DropReason::Timeout, "timeout"),
            (on_batch::DropReason::ConnectRefused, "connect_refused"),
            (on_batch::DropReason::TlsError, "tls_error"),
            (on_batch::DropReason::Transport, "transport"),
            (on_batch::DropReason::EncodeFailed, "encode_failed"),
            (on_batch::DropReason::DeadlineExceeded, "deadline_exceeded"),
        ] {
            let line = format_drop_notice(&batch, "https://example.test/v1", reason, 1);
            assert!(
                line.contains(expected_token),
                "DropReason::{reason:?} should render as `{expected_token}`; got: {line}",
            );
        }
    }

    #[test]
    fn format_drop_notice_with_empty_server_url_renders_a_double_space() {
        // When `OnBatch::server_url()` returns `None`, the caller
        // passes "" — the rendered line has an empty `<url>` slot but
        // remains parseable. This is the test-fake path; production
        // never hits it because `RmpEncodeAndHttpPost::server_url`
        // returns `Some(...)`.
        let batch = dummy_batch_with_calls(1);
        let line = format_drop_notice(&batch, "", on_batch::DropReason::Timeout, 1);
        assert!(
            line.contains(":  timeout"),
            "empty server_url → consecutive spaces before status; got: {line}",
        );
    }

    #[test]
    fn push_and_drain_drop_notices_round_trips_in_push_order() {
        let _guard = lock();
        reset_for_test();
        push_drop_notice("one".to_owned());
        push_drop_notice("two".to_owned());
        push_drop_notice("three".to_owned());
        let drained = drain_drop_notices();
        assert_eq!(drained, vec!["one", "two", "three"]);
        assert!(
            drain_drop_notices().is_empty(),
            "queue is empty after the first drain",
        );
        reset_for_test();
    }

    #[test]
    fn drained_consume_pushes_a_notice_on_dropped_outcomes() {
        let _guard = lock();
        let _account_guard = crate::recorder::accounting::acquire_test_lock();
        reset_for_test();
        crate::recorder::accounting::reset_for_test();

        let batch = dummy_batch_with_calls(3);
        let mut on_batch_fake =
            on_batch::RecordingOnBatch::new(vec![on_batch::OnBatchOutcome::Dropped {
                reason: on_batch::DropReason::HttpStatus(503),
                attempts: 4,
            }]);
        let mut counter: u64 = 0;
        drained_consume(&batch, &mut on_batch_fake, None, &mut counter);

        let notices = drain_drop_notices();
        assert_eq!(notices.len(), 1, "exactly one notice queued");
        assert!(
            notices[0].starts_with("php-analyze: dropped 3 records from trace "),
            "queued line preserves spec format; got: {}",
            notices[0],
        );
        assert!(
            notices[0].contains("http 503 (attempt 4)"),
            "queued line carries the DropReason::Display token + attempts; got: {}",
            notices[0],
        );
        assert_eq!(counter, 0, "Dropped outcome does not bump batches_drained");
        reset_for_test();
    }

    #[test]
    fn drained_consume_does_not_push_a_notice_on_sent_outcomes() {
        let _guard = lock();
        let _account_guard = crate::recorder::accounting::acquire_test_lock();
        reset_for_test();
        crate::recorder::accounting::reset_for_test();

        let batch = dummy_batch_with_calls(2);
        let mut on_batch_fake =
            on_batch::RecordingOnBatch::new(vec![on_batch::OnBatchOutcome::Sent]);
        let mut counter: u64 = 0;
        drained_consume(&batch, &mut on_batch_fake, None, &mut counter);

        assert!(
            drain_drop_notices().is_empty(),
            "Sent outcome must not queue a drop notice",
        );
        assert_eq!(counter, 1, "Sent outcome bumps batches_drained");
        reset_for_test();
    }

    // --- §SEH-9 DRAIN_DEADLINE cell ------------------------------------

    #[test]
    fn drain_deadline_is_none_at_process_start() {
        let _guard = lock();
        reset_for_test();
        assert!(
            drain_deadline_snapshot().is_none(),
            "DRAIN_DEADLINE starts None and reset_for_test clears it back to None",
        );
    }

    #[test]
    fn publish_drain_deadline_transitions_cell_to_some_and_reset_clears_it() {
        let _guard = lock();
        reset_for_test();
        assert!(drain_deadline_snapshot().is_none());
        let deadline = Instant::now() + Duration::from_secs(5);
        set_drain_deadline_for_test(deadline);
        assert_eq!(
            drain_deadline_snapshot(),
            Some(deadline),
            "set_drain_deadline_for_test publishes the exact Instant",
        );
        clear_drain_deadline_for_test();
        assert!(
            drain_deadline_snapshot().is_none(),
            "clear_drain_deadline_for_test returns the cell to None",
        );
        set_drain_deadline_for_test(deadline);
        reset_for_test();
        assert!(
            drain_deadline_snapshot().is_none(),
            "reset_for_test clears the cell alongside the other slots",
        );
    }

    #[test]
    fn run_loop_with_cell_publish_and_no_drain_message_still_exits_via_deadline_pass() {
        let _guard = lock();
        let _account_guard = crate::recorder::accounting::acquire_test_lock();
        reset_for_test();
        crate::recorder::accounting::reset_for_test();
        crate::recorder::accounting::add(400);

        // Saturate the channel with 4 batches, publish a past
        // deadline to the cell, then spawn the shipper. The shipper
        // SHALL self-exit via the deadline-pass arm without ever
        // popping a `Drain` message.
        let (tx, rx) = bounded::<ShipperMessage>(16);
        for _ in 0..4 {
            tx.send(dummy_batch_with_size(100)).unwrap();
        }
        set_drain_deadline_for_test(
            Instant::now()
                .checked_sub(Duration::from_millis(1))
                .unwrap_or_else(Instant::now),
        );
        let start = Instant::now();
        let handle =
            thread::spawn(move || run_loop(rx, on_batch::RecordingOnBatch::new(Vec::new())));
        let exit = handle.join().expect("shipper joined cleanly");
        let elapsed = start.elapsed();

        assert!(
            !exit.drain_completed,
            "cell-published past deadline → deadline-pass exit, got {exit:?}",
        );
        assert_eq!(
            exit.batches_drained + exit.batches_abandoned_at_deadline,
            4,
            "every batch is accounted for: {exit:?}",
        );
        assert!(
            elapsed < Duration::from_millis(200),
            "cell-published past deadline must short-circuit promptly; took {elapsed:?}",
        );
        assert_eq!(
            crate::recorder::accounting::snapshot(),
            0,
            "accounting balanced across pre-drain consumes and deadline-pass abandons",
        );
        drop(tx);
        reset_for_test();
    }

    #[test]
    fn run_loop_with_cell_publish_passes_some_deadline_to_drained_consume() {
        let _guard = lock();
        let _account_guard = crate::recorder::accounting::acquire_test_lock();
        reset_for_test();
        crate::recorder::accounting::reset_for_test();

        // Publish the cell FIRST so the recv-loop head observes it on
        // its very first iteration; otherwise the first batch is
        // consumed under the slice-3 `None` path and we'd assert on
        // the second batch.
        let deadline = Instant::now() + Duration::from_millis(500);
        set_drain_deadline_for_test(deadline);

        let (tx, rx) = bounded::<ShipperMessage>(8);
        tx.send(dummy_batch()).unwrap();
        tx.send(dummy_batch()).unwrap();
        drop(tx);

        let recorder = on_batch::DeadlineRecordingOnBatch::new();
        let shared = recorder.shared_handle();
        let handle = thread::spawn(move || run_loop(rx, recorder));
        let _exit = handle.join().expect("shipper joined cleanly");

        let seen = shared.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(
            seen.len(),
            2,
            "two batches → two handle calls; got {} ({seen:?})",
            seen.len(),
        );
        for (i, observed) in seen.iter().enumerate() {
            assert_eq!(
                *observed,
                Some(deadline),
                "batch {i}'s deadline must match the cell value exactly; observed {observed:?}",
            );
        }
        reset_for_test();
    }

    #[test]
    fn run_loop_with_cell_publish_and_comfortable_deadline_drains_normally() {
        let _guard = lock();
        let _account_guard = crate::recorder::accounting::acquire_test_lock();
        reset_for_test();
        crate::recorder::accounting::reset_for_test();
        crate::recorder::accounting::add(800);

        // Publish a 10s deadline. With a fast `RecordingOnBatch`, the
        // loop should drain all 8 batches in milliseconds — proving
        // the cell-publish does not pessimise the happy path.
        set_drain_deadline_for_test(Instant::now() + Duration::from_secs(10));

        let (tx, rx) = bounded::<ShipperMessage>(16);
        for _ in 0..8 {
            tx.send(dummy_batch_with_size(100)).unwrap();
        }
        drop(tx);

        let start = Instant::now();
        let handle =
            thread::spawn(move || run_loop(rx, on_batch::RecordingOnBatch::new(Vec::new())));
        let exit = handle.join().expect("shipper joined cleanly");
        let elapsed = start.elapsed();

        assert!(
            exit.drain_completed,
            "comfortable deadline + Sender drop → clean close, got {exit:?}",
        );
        assert_eq!(exit.batches_drained, 8);
        assert_eq!(exit.batches_abandoned_at_deadline, 0);
        assert!(
            elapsed < Duration::from_millis(100),
            "comfortable deadline must not pessimise the happy path; took {elapsed:?}",
        );
        assert_eq!(crate::recorder::accounting::snapshot(), 0);
        reset_for_test();
    }

    #[test]
    fn drain_and_join_at_mshutdown_publishes_cell_before_send() {
        let _guard = lock();
        reset_for_test();
        install_channel_at_minit(8);
        spawn_if_needed_at_rinit_for_test();
        assert!(drain_deadline_snapshot().is_none(), "cell starts empty",);
        let outcome = drain_and_join_at_mshutdown(Duration::from_millis(50));
        assert!(
            matches!(outcome, JoinOutcome::Clean(_)),
            "clean shutdown; got {outcome:?}",
        );
        assert!(
            drain_deadline_snapshot().is_some(),
            "drain_and_join_at_mshutdown publishes the cell before it returns",
        );
        reset_for_test();
    }

    #[test]
    fn drain_and_join_at_mshutdown_with_saturated_channel_and_slow_on_batch_returns_within_grace() {
        let _guard = lock();
        let _account_guard = crate::recorder::accounting::acquire_test_lock();
        reset_for_test();
        crate::recorder::accounting::reset_for_test();
        crate::recorder::accounting::add(800);

        // Set up a channel + handle slot manually so we can wire a
        // deadline-honouring SlowRecordingOnBatch (the production
        // spawn helper would pass RmpEncodeAndHttpPost). Push 8
        // batches before the shipper starts so the queue is
        // saturated. Each handle call would sleep 100ms if it had
        // time, but the fake honours `deadline` — once now ≥ deadline
        // it returns `DropReason::DeadlineExceeded` immediately
        // (mirroring what `run_with_retry` does between attempts in
        // production). With a 200ms grace, only ~2 batches can
        // complete Sent; the remaining ~6 return DeadlineExceeded
        // and the shipper exits via the channel-close arm once the
        // queue empties.
        //
        // The AC-BS-4 / AC-PB-2 binding invariants are:
        //   1. elapsed time ≤ grace + 200ms slack.
        //   2. accounting balanced (every batch's size_estimate
        //      returned to the budget, regardless of Sent vs
        //      DeadlineExceeded).
        //   3. every batch was passed to `handle` (the `calls`
        //      counter equals 8).
        //
        // The `drained` / `abandoned` / `deadline-exceeded` split is
        // timing-dependent (depends on how fast the test host
        // services the queue) and does not bind the AC; we do NOT
        // assert on `drain_completed` or the split between
        // `batches_drained` and `batches_abandoned_at_deadline`.
        let (tx, rx) = bounded::<ShipperMessage>(16);
        for _ in 0..8 {
            tx.send(dummy_batch_with_size(100)).unwrap();
        }
        install_test_sender(tx);

        let slow = on_batch::SlowRecordingOnBatch::new(Duration::from_millis(100));
        let calls = slow.calls_handle();
        let join_handle = thread::Builder::new()
            .name("php-analyze-shipper-slow-test".to_owned())
            .spawn(move || run_loop(rx, slow))
            .expect("spawn slow shipper thread");
        *SHIPPER_HANDLE.lock().unwrap_or_else(|e| e.into_inner()) = Some(join_handle);

        let start = Instant::now();
        let outcome = drain_and_join_at_mshutdown(Duration::from_millis(200));
        let elapsed = start.elapsed();

        assert!(
            matches!(outcome, JoinOutcome::Clean(_)),
            "expected Clean, got {outcome:?}",
        );
        assert!(
            elapsed < Duration::from_millis(400),
            "AC-BS-4 / AC-PB-2 bound: grace (200ms) + 200ms slack; took {elapsed:?}",
        );
        let observed_calls = calls.load(std::sync::atomic::Ordering::Acquire);
        assert_eq!(
            observed_calls, 8,
            "every batch was passed to handle (some Sent, rest DeadlineExceeded); \
             observed {observed_calls}",
        );
        assert_eq!(
            crate::recorder::accounting::snapshot(),
            0,
            "every batch's size_estimate is returned to the budget",
        );
        reset_for_test();
    }

    #[test]
    fn cell_publish_mid_flight_collapses_in_progress_recv_loop() {
        let _guard = lock();
        let _account_guard = crate::recorder::accounting::acquire_test_lock();
        reset_for_test();
        crate::recorder::accounting::reset_for_test();
        crate::recorder::accounting::add(800);

        // 8 batches with a SlowRecordingOnBatch that sleeps 50ms per
        // call (and honours the deadline). Spawn the shipper, let it
        // start consuming, then publish a near-future deadline from
        // the test thread. The shipper SHALL pick up the cell on its
        // next recv-loop iteration and bound the remaining batches
        // to the published deadline.
        //
        // Binding invariants (same shape as the AC-BS-4 test above):
        //   1. elapsed bounded by deadline + slack.
        //   2. every batch was passed to handle (`calls` == 8).
        //   3. accounting balanced.
        // The `drained` / `abandoned` split is timing-dependent.
        let (tx, rx) = bounded::<ShipperMessage>(16);
        for _ in 0..8 {
            tx.send(dummy_batch_with_size(100)).unwrap();
        }
        let slow = on_batch::SlowRecordingOnBatch::new(Duration::from_millis(50));
        let calls = slow.calls_handle();
        let start = Instant::now();
        let join_handle = thread::spawn(move || run_loop(rx, slow));

        // Let the shipper consume one batch (~50ms), then publish a
        // 100ms deadline. The shipper's next iteration after the
        // current handle returns observes the cell.
        thread::sleep(Duration::from_millis(60));
        set_drain_deadline_for_test(Instant::now() + Duration::from_millis(100));

        let exit = join_handle.join().expect("shipper joined cleanly");
        let elapsed = start.elapsed();

        // Timing budget: ~60ms (warm-up) + 100ms (deadline) +
        // 50ms (final in-flight batch) + 200ms (test slack).
        assert!(
            elapsed < Duration::from_millis(500),
            "cell publish mid-flight bounds the loop; took {elapsed:?}, exit={exit:?}",
        );
        let observed_calls = calls.load(std::sync::atomic::Ordering::Acquire);
        assert_eq!(
            observed_calls, 8,
            "every batch was passed to handle (some Sent, rest DeadlineExceeded); \
             observed {observed_calls}",
        );
        assert_eq!(crate::recorder::accounting::snapshot(), 0);
        drop(tx);
        reset_for_test();
    }

    #[test]
    fn run_loop_with_drain_message_only_and_no_cell_publish_preserves_slice_3_behaviour() {
        let _guard = lock();
        let _account_guard = crate::recorder::accounting::acquire_test_lock();
        reset_for_test();
        crate::recorder::accounting::reset_for_test();
        crate::recorder::accounting::add(300);

        // Tests that bypass `drain_and_join_at_mshutdown` (i.e.
        // push `Drain { deadline }` directly into the channel) must
        // continue to work — slice-3 semantics are preserved on this
        // path. The cell stays `None` throughout.
        assert!(drain_deadline_snapshot().is_none());

        let (tx, rx) = bounded::<ShipperMessage>(8);
        for _ in 0..3 {
            tx.send(dummy_batch_with_size(100)).unwrap();
        }
        tx.send(ShipperMessage::Drain {
            deadline: Instant::now() + Duration::from_secs(5),
        })
        .unwrap();
        drop(tx);

        let handle =
            thread::spawn(move || run_loop(rx, on_batch::RecordingOnBatch::new(Vec::new())));
        let exit = handle.join().expect("shipper joined cleanly");

        assert!(exit.drain_completed, "clean close, got {exit:?}");
        assert_eq!(exit.batches_drained, 3);
        assert_eq!(exit.batches_abandoned_at_deadline, 0);
        assert!(
            drain_deadline_snapshot().is_none(),
            "Drain-message-only path leaves the cell empty",
        );
        assert_eq!(crate::recorder::accounting::snapshot(), 0);
        reset_for_test();
    }
}
