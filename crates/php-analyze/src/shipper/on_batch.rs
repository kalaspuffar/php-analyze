//! The `OnBatch` trait — the production-vs-test seam.
//!
//! `shipper::run_loop` consumes `PendingBatch` values, encodes them
//! via [`crate::shipper::encode::encode_batch`], and hands the
//! resulting `Vec<u8>` to an `OnBatch` implementation. Production
//! wires [`crate::shipper::http::RmpEncodeAndHttpPost`]
//! (`ureq::Agent` + retry/backoff per `SPECIFICATION.md` §5.2); tests
//! wire [`RecordingOnBatch`] (an append-only `Vec` plus a scripted
//! `Vec<OnBatchOutcome>` to feed back to the loop).
//!
//! The trait stays `pub(crate)` so the seam never leaks to operators —
//! AC-OQ-2 requires the cdylib to expose no symbols beyond the
//! Zend-facing entry points (handled by `lib.rs::get_module`).

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::Arc;
use std::time::Instant;

/// Outcome of one [`OnBatch::handle`] call.
///
/// `Sent` is the only success arm; `Dropped` carries the reason + the
/// number of total attempts (including the initial one). Callers
/// (i.e. `run_loop`) use the outcome to bump `ShipperExit`'s
/// `batches_drained` counter and to confirm the budget round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OnBatchOutcome {
    Sent,
    Dropped { reason: DropReason, attempts: u32 },
}

/// Why a batch was dropped. The `Display` impl matches the §5.2
/// `<status_or_error>` token verbatim so the `E_NOTICE` line uses
/// these values directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DropReason {
    /// Non-2xx HTTP response after `retry_count + 1` attempts.
    HttpStatus(u16),
    /// `ureq::Error::Timeout(_)` on the final attempt.
    Timeout,
    /// `ureq::Error::ConnectionFailed` on the final attempt
    /// (kernel-level `ECONNREFUSED`, broken DNS, etc.).
    ConnectRefused,
    /// `ureq::Error::Tls(_)` — TLS handshake failure on the final
    /// attempt.
    TlsError,
    /// `ureq::Error::Io(_)` or any other transport-shaped error not
    /// matched by the variants above.
    Transport,
    /// `shipper::encode::encode_batch` returned an error. No retry
    /// is attempted — the same input would fail again.
    EncodeFailed,
    /// The post-`Drain` deadline elapsed before this batch's next
    /// attempt could start, or before its next backoff could
    /// complete.
    DeadlineExceeded,
}

impl std::fmt::Display for DropReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Spec wording (§5.2): `http <N>` for HTTP responses.
            DropReason::HttpStatus(code) => write!(f, "http {code}"),
            DropReason::Timeout => f.write_str("timeout"),
            DropReason::ConnectRefused => f.write_str("connect_refused"),
            DropReason::TlsError => f.write_str("tls_error"),
            DropReason::Transport => f.write_str("transport"),
            DropReason::EncodeFailed => f.write_str("encode_failed"),
            DropReason::DeadlineExceeded => f.write_str("deadline_exceeded"),
        }
    }
}

/// The contract every batch-handler implements. `Send` is required so
/// the shipper thread can own the implementation by value.
///
/// `deadline` is `None` during the pre-`Drain` phase (the shipper is
/// in steady-state recv); `Some(deadline)` during the post-`Drain`
/// phase, so the implementation can cut retries short per design
/// D-7 of the `shipper-encoder-and-http` change.
pub(crate) trait OnBatch: Send {
    fn handle(
        &mut self,
        encoded: &[u8],
        trace_id: [u8; 16],
        records_in_batch: usize,
        deadline: Option<Instant>,
    ) -> OnBatchOutcome;

    /// The destination URL this handler POSTs to, for inclusion in
    /// the `SPECIFICATION.md` §5.2 `E_NOTICE` drop line. The default
    /// returns `None` so test fakes (e.g. [`RecordingOnBatch`]) do
    /// not need to invent a URL; the production
    /// [`crate::shipper::http::RmpEncodeAndHttpPost`] impl overrides
    /// it to expose `&self.server_url.as_str()`. The bearer token
    /// MUST NOT be exposed through this accessor (AC-SH-4); URLs
    /// have no slot for the token, so the invariant holds by
    /// construction.
    fn server_url(&self) -> Option<&str> {
        None
    }
}

/// What the recording fake captured for one `handle` call. Tests pull
/// `recorded` out and assert by index.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordedBatch {
    pub(crate) encoded: Vec<u8>,
    pub(crate) trace_id: [u8; 16],
    pub(crate) records_in_batch: usize,
    pub(crate) deadline_was_some: bool,
}

/// Test-only [`OnBatch`] that:
///
/// - appends every call's args to `recorded`,
/// - pops the next outcome from `script` (front-to-back) and returns
///   it,
/// - bumps `drop_counter_for_drops` by `records_in_batch` on every
///   `Dropped` outcome so the shipper-loop tests can assert the
///   "shared `Arc<AtomicU64>` is bumped per dropped batch" contract
///   without the recorder being involved.
///
/// Visibility is `pub(crate)` so the run_loop tests in the parent
/// `shipper` module can reach it.
#[cfg(test)]
pub(crate) struct RecordingOnBatch {
    pub(crate) recorded: Vec<RecordedBatch>,
    pub(crate) script: Vec<OnBatchOutcome>,
    /// Drop counter the recording fake bumps on `Dropped` outcomes.
    /// Tests pass an `Arc::clone` of the trace's counter so the
    /// "retry-exhaust bumps the shared counter" assertion works the
    /// same as production. `None` skips the bump.
    pub(crate) drop_counter_for_drops: Option<Arc<AtomicU64>>,
}

#[cfg(test)]
impl RecordingOnBatch {
    pub(crate) fn new(script: Vec<OnBatchOutcome>) -> Self {
        Self {
            recorded: Vec::new(),
            script,
            drop_counter_for_drops: None,
        }
    }

    /// Variant constructor that wires the drop-counter Arc. Used by
    /// the retry-exhaust scenario tests.
    pub(crate) fn with_drop_counter(
        script: Vec<OnBatchOutcome>,
        drop_counter: Arc<AtomicU64>,
    ) -> Self {
        Self {
            recorded: Vec::new(),
            script,
            drop_counter_for_drops: Some(drop_counter),
        }
    }
}

#[cfg(test)]
impl OnBatch for RecordingOnBatch {
    fn handle(
        &mut self,
        encoded: &[u8],
        trace_id: [u8; 16],
        records_in_batch: usize,
        deadline: Option<Instant>,
    ) -> OnBatchOutcome {
        self.recorded.push(RecordedBatch {
            encoded: encoded.to_vec(),
            trace_id,
            records_in_batch,
            deadline_was_some: deadline.is_some(),
        });
        // If the script ran out of scripted outcomes, default to
        // `Sent` — a missing script entry is interpreted as "the
        // happy path" rather than a panic, so tests that don't care
        // about the outcome stay readable.
        let outcome = if self.script.is_empty() {
            OnBatchOutcome::Sent
        } else {
            self.script.remove(0)
        };
        if matches!(outcome, OnBatchOutcome::Dropped { .. }) {
            if let Some(counter) = &self.drop_counter_for_drops {
                counter.fetch_add(records_in_batch as u64, Ordering::Release);
            }
        }
        outcome
    }
}

/// Test-only [`OnBatch`] that captures the exact `deadline: Option<Instant>`
/// argument passed to each `handle` call. Sibling of
/// [`RecordingOnBatch`]; differs only in that `RecordedBatch` stores a
/// boolean while this fake captures the full `Instant` so a test can
/// assert "the deadline plumbed to `handle` equals the deadline I
/// published to `DRAIN_DEADLINE`".
///
/// Always returns `OnBatchOutcome::Sent`. Tests that care about the
/// `Dropped` path reach for [`RecordingOnBatch`] with a scripted
/// outcome instead.
#[cfg(test)]
pub(crate) struct DeadlineRecordingOnBatch {
    /// The full sequence of `deadline` values seen by `handle`, in
    /// call order. Wrapped in `Arc<Mutex<_>>` so the test thread can
    /// read it after the shipper thread joins.
    pub(crate) seen_deadlines: Arc<std::sync::Mutex<Vec<Option<Instant>>>>,
}

#[cfg(test)]
impl DeadlineRecordingOnBatch {
    pub(crate) fn new() -> Self {
        Self {
            seen_deadlines: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Convenience: clone the inner `Arc` for the test thread to
    /// inspect after `JoinHandle::join`. The shipper thread owns the
    /// original.
    pub(crate) fn shared_handle(&self) -> Arc<std::sync::Mutex<Vec<Option<Instant>>>> {
        Arc::clone(&self.seen_deadlines)
    }
}

#[cfg(test)]
impl OnBatch for DeadlineRecordingOnBatch {
    fn handle(
        &mut self,
        _encoded: &[u8],
        _trace_id: [u8; 16],
        _records_in_batch: usize,
        deadline: Option<Instant>,
    ) -> OnBatchOutcome {
        self.seen_deadlines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(deadline);
        OnBatchOutcome::Sent
    }
}

/// Test-only [`OnBatch`] that sleeps a configurable `Duration` per
/// `handle` call before returning `Sent`. Models a slow upstream
/// (e.g. a black-holed HTTPS server) without standing up the
/// production HTTP path. Used by the AC-BS-4 / AC-PB-2 binding tests
/// to put real wall-clock time inside the per-batch consume step so
/// the recv-loop-head cell-read has work to short-circuit.
///
/// **Deadline honouring**: if `handle` is called with `Some(deadline)`
/// and `Instant::now() >= deadline`, the fake returns
/// `OnBatchOutcome::Dropped { reason: DeadlineExceeded, attempts: 0 }`
/// **without sleeping**. If the deadline would lapse mid-sleep, the
/// fake clamps the sleep to `deadline - now` and then returns
/// `DeadlineExceeded`. This mirrors what the production
/// `RmpEncodeAndHttpPost` does via `run_with_retry`: every attempt
/// honours the deadline before starting, and a backoff that would
/// extend past the deadline collapses the remaining retries.
#[cfg(test)]
pub(crate) struct SlowRecordingOnBatch {
    pub(crate) sleep_per_call: std::time::Duration,
    /// Number of `handle` calls this fake has serviced (Sent or
    /// Dropped). Tests assert "the shipper called handle N times"
    /// against this.
    pub(crate) calls: Arc<AtomicU64>,
}

#[cfg(test)]
impl SlowRecordingOnBatch {
    pub(crate) fn new(sleep_per_call: std::time::Duration) -> Self {
        Self {
            sleep_per_call,
            calls: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn calls_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.calls)
    }
}

#[cfg(test)]
impl OnBatch for SlowRecordingOnBatch {
    fn handle(
        &mut self,
        _encoded: &[u8],
        _trace_id: [u8; 16],
        _records_in_batch: usize,
        deadline: Option<Instant>,
    ) -> OnBatchOutcome {
        self.calls.fetch_add(1, Ordering::Release);
        if let Some(d) = deadline {
            let now = Instant::now();
            if now >= d {
                return OnBatchOutcome::Dropped {
                    reason: DropReason::DeadlineExceeded,
                    attempts: 0,
                };
            }
            let remaining = d - now;
            if remaining < self.sleep_per_call {
                // Clamp: sleep what we have, then report
                // DeadlineExceeded so the outer loop's deadline-pass
                // arm sees a faithful "we couldn't complete in time"
                // signal.
                std::thread::sleep(remaining);
                return OnBatchOutcome::Dropped {
                    reason: DropReason::DeadlineExceeded,
                    attempts: 1,
                };
            }
        }
        std::thread::sleep(self.sleep_per_call);
        OnBatchOutcome::Sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_reason_display_matches_spec_5_2_status_or_error_tokens() {
        // The exact wording in `SPECIFICATION.md` §5.2 step 4:
        // `<status_or_error>` is `http <N>` / `timeout` / `connect_refused`
        // / `tls_error` / `transport`. Plus our two local additions
        // (`encode_failed`, `deadline_exceeded`) for the §3.3 paths the
        // spec doesn't name explicitly.
        assert_eq!(DropReason::HttpStatus(500).to_string(), "http 500");
        assert_eq!(DropReason::HttpStatus(401).to_string(), "http 401");
        assert_eq!(DropReason::Timeout.to_string(), "timeout");
        assert_eq!(DropReason::ConnectRefused.to_string(), "connect_refused");
        assert_eq!(DropReason::TlsError.to_string(), "tls_error");
        assert_eq!(DropReason::Transport.to_string(), "transport");
        assert_eq!(DropReason::EncodeFailed.to_string(), "encode_failed");
        assert_eq!(
            DropReason::DeadlineExceeded.to_string(),
            "deadline_exceeded",
        );
    }

    #[test]
    fn recording_on_batch_returns_sent_when_script_is_empty() {
        let mut fake = RecordingOnBatch::new(Vec::new());
        let outcome = fake.handle(&[1, 2, 3], [0u8; 16], 4, None);
        assert_eq!(outcome, OnBatchOutcome::Sent);
        assert_eq!(fake.recorded.len(), 1);
        assert_eq!(fake.recorded[0].records_in_batch, 4);
        assert_eq!(fake.recorded[0].encoded, vec![1, 2, 3]);
    }

    #[test]
    fn recording_on_batch_pops_script_entries_front_to_back() {
        let mut fake = RecordingOnBatch::new(vec![
            OnBatchOutcome::Sent,
            OnBatchOutcome::Dropped {
                reason: DropReason::HttpStatus(500),
                attempts: 4,
            },
            OnBatchOutcome::Sent,
        ]);
        let a = fake.handle(&[1], [0u8; 16], 1, None);
        let b = fake.handle(&[2], [0u8; 16], 1, None);
        let c = fake.handle(&[3], [0u8; 16], 1, None);
        assert_eq!(a, OnBatchOutcome::Sent);
        assert!(matches!(b, OnBatchOutcome::Dropped { .. }));
        assert_eq!(c, OnBatchOutcome::Sent);
    }

    #[test]
    fn recording_on_batch_with_drop_counter_bumps_on_dropped_outcomes() {
        let counter = Arc::new(AtomicU64::new(0));
        let mut fake = RecordingOnBatch::with_drop_counter(
            vec![OnBatchOutcome::Dropped {
                reason: DropReason::Timeout,
                attempts: 3,
            }],
            Arc::clone(&counter),
        );
        fake.handle(&[1, 2, 3], [0u8; 16], 7, None);
        assert_eq!(
            counter.load(Ordering::Acquire),
            7,
            "drop counter bumped by records_in_batch on a Dropped outcome",
        );
    }

    #[test]
    fn recording_on_batch_does_not_bump_on_sent_outcomes() {
        let counter = Arc::new(AtomicU64::new(0));
        let mut fake = RecordingOnBatch::with_drop_counter(
            vec![OnBatchOutcome::Sent, OnBatchOutcome::Sent],
            Arc::clone(&counter),
        );
        fake.handle(&[1], [0u8; 16], 3, None);
        fake.handle(&[2], [0u8; 16], 5, None);
        assert_eq!(
            counter.load(Ordering::Acquire),
            0,
            "Sent outcomes never bump the drop counter",
        );
    }

    #[test]
    fn recording_on_batch_captures_whether_deadline_was_passed() {
        let mut fake = RecordingOnBatch::new(Vec::new());
        fake.handle(&[1], [0u8; 16], 1, None);
        fake.handle(&[2], [0u8; 16], 1, Some(Instant::now()));
        assert!(!fake.recorded[0].deadline_was_some);
        assert!(fake.recorded[1].deadline_was_some);
    }
}
