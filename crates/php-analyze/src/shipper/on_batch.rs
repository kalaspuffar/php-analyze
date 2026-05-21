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
