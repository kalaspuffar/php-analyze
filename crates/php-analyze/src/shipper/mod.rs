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

// --- Pure run_loop --------------------------------------------------------

/// Drain the channel until a clean termination condition.
///
/// State machine:
///
/// 1. **Pre-drain phase**: block on [`Receiver::recv`]. Each
///    [`ShipperMessage::Batch`] is counted and dropped. A
///    [`ShipperMessage::Drain`] transitions to the drain phase. An
///    `Err(RecvError)` (the channel closed without a `Drain`) is a
///    **clean close pre-drain** — return with
///    `drain_completed: true` and `batches_abandoned_at_deadline:
///    0`.
/// 2. **Drain phase**: block on [`Receiver::recv_deadline`]. Each
///    `Batch` is counted. A second `Drain` is tolerated (ignored).
///    Termination conditions:
///    - **Clean close post-drain**: `Err(RecvTimeoutError::Disconnected)` —
///      return with `drain_completed: true`,
///      `batches_abandoned_at_deadline: 0`.
///    - **Deadline pass**: `Err(RecvTimeoutError::Timeout)` —
///      return with `drain_completed: false`,
///      `batches_abandoned_at_deadline: rx.len() as u64` (the
///      messages still queued at the moment of timeout).
///
/// Slice 2+ will grow a fourth termination condition via an
/// `on_batch` callback that returns an error (the encoder or HTTP
/// client failing). Slice 1 does not have one.
pub(crate) fn run_loop(rx: Receiver<ShipperMessage>) -> ShipperExit {
    let mut batches_drained: u64 = 0;
    let deadline: Instant = loop {
        match rx.recv() {
            Ok(ShipperMessage::Batch(_)) => batches_drained += 1,
            Ok(ShipperMessage::Drain { deadline }) => break deadline,
            Err(RecvError) => {
                return ShipperExit {
                    batches_drained,
                    drain_completed: true,
                    batches_abandoned_at_deadline: 0,
                };
            }
        }
    };
    loop {
        match rx.recv_deadline(deadline) {
            Ok(ShipperMessage::Batch(_)) => batches_drained += 1,
            // A second Drain is structurally unreachable today (only
            // `drain_and_join_at_mshutdown` sends Drain, and only
            // once), but tolerating it costs nothing and removes a
            // future-bug class.
            Ok(ShipperMessage::Drain { .. }) => {}
            Err(RecvTimeoutError::Timeout) => {
                let abandoned = u64::try_from(rx.len()).unwrap_or(u64::MAX);
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

/// Spawn the shipper thread on the first `RINIT` per process. Guarded
/// by an [`AtomicBool::compare_exchange`] so concurrent `RINIT`s race
/// to a single spawn. The winner takes the receiver, spawns
/// [`run_loop`], and stashes the [`JoinHandle`] in
/// [`SHIPPER_HANDLE`]. If no channel was installed (disabled
/// extension or a programming error), the winner reverts the spawn
/// flag so a later, correctly-installed RINIT can still spawn.
pub(crate) fn spawn_if_needed_at_rinit() {
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
    let handle = thread::Builder::new()
        .name("php-analyze-shipper".to_owned())
        .spawn(move || run_loop(rx))
        .expect("OS thread spawn for the shipper failed");
    let mut handle_slot = SHIPPER_HANDLE.lock().unwrap_or_else(|e| e.into_inner());
    *handle_slot = Some(handle);
}

/// Send `Drain { deadline: now + grace }`, drop the canonical
/// Sender, and join the shipper thread. The `+ 200 ms` slack
/// referenced by `SPECIFICATION.md` AC-BS-4 / AC-PB-2 is the budget
/// for the channel-close + `recv_deadline`-loop-exit + `JoinHandle::join`
/// overhead; slice 1 measurably stays well under it.
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
    // `send_timeout(grace)` (not `try_send`) so the Drain message —
    // and the deadline it carries — still reaches the shipper when
    // the channel is momentarily full. `try_send` would silently
    // skip the Drain on a saturated queue, and the shipper would
    // then drain the entire backlog *without* ever seeing the
    // deadline; in slice 1 that's harmless (each batch is a few
    // instructions) but slice 2's encode + POST + retry per batch
    // can blow past `shutdown_grace_ms` in the worst case. If
    // `send_timeout` itself runs out the grace, the `drop(sender)`
    // below closes the channel and the shipper exits via
    // `Disconnected` — the deadline is effectively zero by then.
    //
    // TODO(slice-2): once `run_loop` performs real per-batch work,
    // even a successfully-delivered Drain is not enough on its own.
    // A full queue plus slow work blows past `grace` while the
    // shipper is still chewing through pre-Drain batches. Slice 2
    // must expose the deadline to the recv-loop head — e.g. a
    // sibling `OnceLock<Instant>` or `AtomicI64`-as-`Instant` — so
    // the loop can self-exit even before the Drain message
    // surfaces. See `COMMENTS.md` round-1 review finding R-1 for
    // the full rationale.
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
    SHIPPER_SPAWNED.store(false, Ordering::SeqCst);
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
    /// Slice 1 never reads any field of a `PendingBatch`, so the
    /// content is irrelevant; we just need the discriminant.
    fn dummy_batch() -> ShipperMessage {
        use crate::recorder::types::{MetaPartial, PendingBatch};
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
            size_estimate: 0,
        })
    }

    // --- run_loop -------------------------------------------------------

    #[test]
    fn run_loop_drains_three_batches_and_exits_cleanly_on_channel_close() {
        let (tx, rx) = bounded::<ShipperMessage>(8);
        let handle = thread::spawn(move || run_loop(rx));
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
        let handle = thread::spawn(move || run_loop(rx));
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
        let handle = thread::spawn(move || run_loop(rx));
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
        let handle = thread::spawn(move || run_loop(rx));
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
        let handle = thread::spawn(move || run_loop(rx));
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
        spawn_if_needed_at_rinit();
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
        spawn_if_needed_at_rinit();
        // Second call must not double-spawn.
        spawn_if_needed_at_rinit();
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
        spawn_if_needed_at_rinit();
        assert!(!spawned_flag(), "no channel → no spawn → CAS reverted");
        assert!(!handle_is_installed());
        assert!(!sender_is_installed());
        // A later, properly-installed RINIT should still be able to
        // spawn — this is what the revert is for.
        install_channel_at_minit(8);
        spawn_if_needed_at_rinit();
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
                spawn_if_needed_at_rinit();
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
        spawn_if_needed_at_rinit();
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
        spawn_if_needed_at_rinit();
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
}
