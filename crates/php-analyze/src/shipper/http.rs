//! Production `OnBatch` impl: encode + HTTP POST + retry/backoff.
//!
//! `SPECIFICATION.md` §5.2 is the binding spec; the
//! `shipper-encoder-and-http` OpenSpec change's `design.md` §D-4 and
//! §D-7 are the implementation notes (open-loop backoff, no jitter,
//! deadline-aware retry budget).
//!
//! ## Three layers
//!
//! 1. **`AttemptOutcome`** — a per-attempt verdict (`Ok(())` on 2xx,
//!    `Err(DropReason)` on any failure). Used by the retry orchestrator
//!    to decide "should we sleep + retry, or give up?".
//! 2. **`run_with_retry`** — the pure-Rust retry orchestrator. Takes
//!    a `FnMut() -> AttemptOutcome`, a retry budget, a backoff base,
//!    and an optional deadline. Returns an `OnBatchOutcome`. Unit-
//!    tested directly with hand-written attempt closures so the retry
//!    arithmetic is verified without any actual network.
//! 3. **`RmpEncodeAndHttpPost`** — the production wiring. Owns the
//!    `ureq::Agent`, the URL, the token, and the retry config; its
//!    `OnBatch::handle` builds the per-attempt closure that calls
//!    `agent.post(...).send(...)` and translates `ureq::Error` to
//!    `DropReason`.
//!
//! ## Token handling
//!
//! The `auth_token` is held as `SecretString`. The only call to
//! `ExposeSecret::expose_secret()` is inside the per-attempt closure
//! at the `Authorization: Bearer …` header set. The token plaintext
//! does NOT appear in any `Display`, `Debug`, `Error::source`, or log
//! line — AC-SH-4 is enforced by construction.

use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use secrecy::{ExposeSecret, SecretString};
use url::Url;

use super::on_batch::{DropReason, OnBatch, OnBatchOutcome};
use super::PendingBatch;
use crate::wire;

/// Per-attempt verdict consumed by [`run_with_retry`].
///
/// Distinct from [`OnBatchOutcome`] because the orchestrator needs to
/// know "this attempt's failure shape" *before* it commits to "the
/// whole batch is dropped". A retry that ultimately succeeds turns a
/// sequence of `Err` attempts into a single `OnBatchOutcome::Sent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AttemptOutcome {
    Sent,
    Failed(DropReason),
}

/// `SPECIFICATION.md` §5.2: open-loop exponential backoff. Sleep
/// `retry_backoff_ms × 2^attempt` between attempt `attempt` and
/// attempt `attempt + 1`. `attempt = 0` is the sleep after the first
/// failure; `attempt = 1` is the sleep before the second retry; etc.
pub(super) fn backoff_duration(retry_backoff_ms: u32, attempt: u32) -> Duration {
    // `1u32 << attempt` may overflow `u32` for unrealistically large
    // `attempt` values (`retry_count > 31`). `saturating_mul` keeps the
    // arithmetic deterministic at the extreme; in practice
    // `retry_count` is bounded by directive validation (`<= 8`).
    let factor = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    Duration::from_millis(u64::from(retry_backoff_ms).saturating_mul(u64::from(factor)))
}

/// The pure retry orchestrator. Calls `attempt()` repeatedly:
///
/// - First call is attempt 0 (the initial POST).
/// - On `AttemptOutcome::Sent`, returns `OnBatchOutcome::Sent` with
///   the total attempt count.
/// - On `AttemptOutcome::Failed(reason)`:
///   - If this was the final attempt (`attempt == retry_count`),
///     return `OnBatchOutcome::Dropped { reason, attempts: attempt + 1 }`.
///   - Otherwise sleep `backoff_duration(retry_backoff_ms, attempt)`
///     (via `sleep_fn` — passed in so tests can replace it with a
///     no-op) and continue.
/// - If `deadline` is `Some(d)` and the loop is about to sleep past
///   `d`, OR is about to start an attempt at `t >= d`, return
///   `OnBatchOutcome::Dropped { reason: DeadlineExceeded, attempts }`.
///
/// Pure with respect to the network: the only impure inputs are
/// `attempt` and `sleep_fn`. Tests pass deterministic closures.
pub(super) fn run_with_retry(
    retry_count: u32,
    retry_backoff_ms: u32,
    deadline: Option<Instant>,
    now: impl Fn() -> Instant,
    mut attempt: impl FnMut(u32) -> AttemptOutcome,
    mut sleep_fn: impl FnMut(Duration),
) -> OnBatchOutcome {
    for attempt_idx in 0..=retry_count {
        // Deadline check before launching an attempt. The slice-1
        // deadline-pass arm already drains the residual queue;
        // here we surface a per-batch DeadlineExceeded so the
        // E_NOTICE line is correct.
        if let Some(d) = deadline {
            if now() >= d {
                return OnBatchOutcome::Dropped {
                    reason: DropReason::DeadlineExceeded,
                    attempts: attempt_idx,
                };
            }
        }
        match attempt(attempt_idx) {
            AttemptOutcome::Sent => {
                return OnBatchOutcome::Sent;
            }
            AttemptOutcome::Failed(reason) => {
                if attempt_idx == retry_count {
                    // Final attempt failed. No more retries.
                    return OnBatchOutcome::Dropped {
                        reason,
                        attempts: attempt_idx + 1,
                    };
                }
                let sleep = backoff_duration(retry_backoff_ms, attempt_idx);
                // Deadline check before sleeping. A backoff that
                // would extend past the deadline collapses the
                // remaining retries.
                if let Some(d) = deadline {
                    if now() + sleep >= d {
                        return OnBatchOutcome::Dropped {
                            reason: DropReason::DeadlineExceeded,
                            attempts: attempt_idx + 1,
                        };
                    }
                }
                sleep_fn(sleep);
            }
        }
    }
    // `for attempt_idx in 0..=retry_count` covers every legal
    // `attempt_idx`, and every iteration of the inner `match`
    // returns. The compiler still requires a tail expression because
    // it cannot prove the loop exits early; this `unreachable!`
    // turns the invariant into a runtime panic if a future refactor
    // breaks it.
    unreachable!("run_with_retry's inner match returns on every branch")
}

/// Production [`OnBatch`] impl. Configured at construction with a
/// reused `ureq::Agent`, the destination URL, the bearer token, and
/// the §5.2 retry parameters.
pub(crate) struct RmpEncodeAndHttpPost {
    agent: ureq::Agent,
    server_url: Url,
    auth_token: SecretString,
    retry_count: u32,
    retry_backoff_ms: u32,
    user_agent: String,
}

impl RmpEncodeAndHttpPost {
    /// Build the production HTTP poster. The `ureq::Agent` is built
    /// once and reused across every POST (per design D-6 → AC-SH-6:
    /// 1000 sends, 1 TCP connection).
    pub(crate) fn new(
        server_url: Url,
        auth_token: SecretString,
        retry_count: u32,
        retry_backoff: Duration,
        http_timeout: Duration,
    ) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(http_timeout))
            // `http_status_as_error` defaults to `true` — non-2xx
            // responses surface as `Error::StatusCode(N)`, which we
            // translate to `DropReason::HttpStatus(N)`. We do not
            // depend on response bodies (§5.2 explicitly disclaims).
            .build();
        let agent = ureq::Agent::new_with_config(config);
        // `retry_backoff` is bounded by directive validation to
        // `≤ 60_000 ms` (well within `u32`), so the `try_into` is a
        // total conversion in practice. The `debug_assert` encodes
        // that invariant: if future validation work raises the
        // bound without auditing this site, a misconfigured operator
        // would otherwise silently get a 49-day backoff after the
        // `u32::MAX` clamp.
        let backoff_millis = retry_backoff.as_millis();
        debug_assert!(
            backoff_millis <= u128::from(u32::MAX),
            "retry_backoff_ms exceeds u32::MAX — directive validation should have caught this",
        );
        let retry_backoff_ms: u32 = backoff_millis.try_into().unwrap_or(u32::MAX);
        Self {
            agent,
            server_url,
            auth_token,
            retry_count,
            retry_backoff_ms,
            user_agent: format!("php-analyze/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

impl OnBatch for RmpEncodeAndHttpPost {
    fn handle(
        &mut self,
        encoded: &[u8],
        _trace_id: [u8; 16],
        _records_in_batch: usize,
        deadline: Option<Instant>,
    ) -> OnBatchOutcome {
        // Bearer string is built fresh per `handle` call. The token
        // plaintext lives only in this local for the duration of
        // the `set("Authorization", ...)` call inside `attempt`. No
        // logging, no error path captures it.
        let bearer = format!("Bearer {}", self.auth_token.expose_secret());
        // `url`, `user_agent`, and `agent` are borrowed by the
        // closure rather than cloned (SEH-6 round-2 fix): the closure
        // only outlives the `handle` call below, so the borrows of
        // `&self.*` are sound for the closure's entire lifetime and
        // we save two `String` allocations + one `Arc` clone per
        // batch.
        let url = self.server_url.as_str();
        let user_agent = self.user_agent.as_str();
        let agent = &self.agent;

        let attempt = |_| -> AttemptOutcome {
            match agent
                .post(url)
                .header("Authorization", bearer.as_str())
                .header("Content-Type", wire::MEDIA_TYPE)
                .header("User-Agent", user_agent)
                .send(encoded)
            {
                Ok(_) => AttemptOutcome::Sent,
                Err(err) => AttemptOutcome::Failed(map_ureq_error(&err)),
            }
        };

        run_with_retry(
            self.retry_count,
            self.retry_backoff_ms,
            deadline,
            Instant::now,
            attempt,
            thread::sleep,
        )
    }

    /// `SPECIFICATION.md` §5.2 step 4 drop-notice URL token. Returns
    /// the configured `server_url`'s string form so the drop notice
    /// emitted on the PHP-thread side reads `... <url> <status_or_error> ...`
    /// for operator triage. The bearer token is held in
    /// `self.auth_token` and is NOT exposed through this accessor —
    /// the `Url` type has no slot for it (AC-SH-4 enforced by
    /// construction).
    fn server_url(&self) -> Option<&str> {
        Some(self.server_url.as_str())
    }
}

/// Translate a `ureq::Error` to one of the §5.2 `<status_or_error>`
/// shapes the `E_NOTICE` line can use directly.
///
/// `ureq::Error` exposes a fixed set of variants. We collapse the
/// `Io(_)` and other miscellaneous transport-shaped errors to
/// `Transport`; the operator-actionable variants (Timeout,
/// ConnectionFailed, Tls, StatusCode) each get their own
/// [`DropReason`].
fn map_ureq_error(err: &ureq::Error) -> DropReason {
    match err {
        ureq::Error::StatusCode(code) => DropReason::HttpStatus(*code),
        ureq::Error::Timeout(_) => DropReason::Timeout,
        ureq::Error::ConnectionFailed => DropReason::ConnectRefused,
        ureq::Error::Tls(_) => DropReason::TlsError,
        // `Io`, `BodyExceedsLimit`, `BadUri`, …: everything else is
        // a generic transport-shaped failure. The bearer token
        // never appears in `err`'s Debug output because we never
        // attach it to the `ureq::Error` chain.
        _ => DropReason::Transport,
    }
}

/// Encode a freshly-flushed `PendingBatch` and pass the bytes to the
/// `on_batch` step. Used by `run_loop` per design D-3. Returns the
/// `OnBatchOutcome` directly so the caller can update its counters.
///
/// On encode failure: emits no log line here (the caller is expected
/// to log one `E_NOTICE` per dropped batch); returns
/// `OnBatchOutcome::Dropped { reason: EncodeFailed, attempts: 0 }`
/// per the spec's "no retry — same input would fail again" rule.
pub(crate) fn encode_and_handle(
    batch: &PendingBatch,
    on_batch: &mut dyn OnBatch,
    deadline: Option<Instant>,
) -> OnBatchOutcome {
    match super::encode::encode_batch(batch) {
        Ok(encoded) => on_batch.handle(
            &encoded,
            batch.meta_partial.trace_id,
            batch.calls.len(),
            deadline,
        ),
        Err(_) => OnBatchOutcome::Dropped {
            reason: DropReason::EncodeFailed,
            attempts: 0,
        },
    }
}

/// Bump the source trace's drop counter by `records_in_batch` on
/// **every** `OnBatchOutcome::Dropped` outcome (retry-exhaust,
/// encode-failure, deadline-exceeded). The counter is the
/// `Arc<AtomicU64>` carried on the `PendingBatch` per AD-9; bumping
/// it ensures the next batch from the same trace surfaces this drop
/// in its `meta.dropped_records` (closing the R-13 contract for
/// HTTP-side drops the same way the recorder closes it for
/// channel-full and buffer-cap drops).
///
/// **Spec deviation (round-2 review SEH-5):** `SPECIFICATION.md`
/// §5.2 step 3 names retry-exhaust as the bump trigger; this impl
/// also bumps on `EncodeFailed` (no retry is applied — the same
/// input would fail again) and `DeadlineExceeded` (the deadline
/// elapsed before any attempt could complete). The records are
/// genuinely lost in both cases, so the bump preserves the
/// "no silent loss" invariant (OBJ-5); the deviation is to fold
/// every drop path into a single counter rather than split into
/// `encode_dropped` / `deadline_dropped` counters that would need
/// new `meta.*` fields. Recorded in `COMMENTS.md` SEH-5; a follow-up
/// `shipper-drop-counter-attribution` change can split these into
/// separate counters if downstream operators need the distinction.
pub(crate) fn bump_drop_counter_on_drop(batch: &PendingBatch, outcome: &OnBatchOutcome) {
    if matches!(outcome, OnBatchOutcome::Dropped { .. }) {
        batch
            .drop_counter
            .fetch_add(batch.calls.len() as u64, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    // ---- backoff arithmetic -----------------------------------------

    #[test]
    fn backoff_duration_is_open_loop_exponential() {
        // `retry_backoff_ms = 200`: sleeps are 200ms, 400ms, 800ms.
        assert_eq!(backoff_duration(200, 0), Duration::from_millis(200));
        assert_eq!(backoff_duration(200, 1), Duration::from_millis(400));
        assert_eq!(backoff_duration(200, 2), Duration::from_millis(800));
        assert_eq!(backoff_duration(200, 3), Duration::from_millis(1_600));
    }

    #[test]
    fn backoff_duration_saturates_on_unrealistic_attempt_counts() {
        // `1u32 << 32` overflows; we saturate to u32::MAX without
        // panicking. The resulting Duration is huge but well-formed.
        let d = backoff_duration(200, 64);
        assert!(d >= Duration::from_secs(60));
    }

    // ---- run_with_retry orchestrator --------------------------------

    /// Helper: a `Cell<u32>`-backed "attempt counter" so tests can
    /// scripted the per-attempt outcome without lifetime contortions.
    struct Script {
        outcomes: Vec<AttemptOutcome>,
        idx: Cell<usize>,
    }

    impl Script {
        fn new(outcomes: Vec<AttemptOutcome>) -> Self {
            Self {
                outcomes,
                idx: Cell::new(0),
            }
        }
        fn next(&self, _idx: u32) -> AttemptOutcome {
            let i = self.idx.get();
            self.idx.set(i + 1);
            self.outcomes[i]
        }
    }

    #[test]
    fn run_with_retry_returns_sent_on_first_attempt_success() {
        let script = Script::new(vec![AttemptOutcome::Sent]);
        let sleeps = Cell::new(0u32);
        let outcome = run_with_retry(
            3,
            200,
            None,
            Instant::now,
            |i| script.next(i),
            |_| sleeps.set(sleeps.get() + 1),
        );
        assert_eq!(outcome, OnBatchOutcome::Sent);
        assert_eq!(sleeps.get(), 0, "no sleeps on first-attempt success");
        assert_eq!(script.idx.get(), 1, "exactly one attempt was made");
    }

    #[test]
    fn run_with_retry_retries_on_failure_then_succeeds() {
        // Three failures then one success. With retry_count = 3 we have
        // 4 total attempts available — the 4th (index 3) succeeds.
        let script = Script::new(vec![
            AttemptOutcome::Failed(DropReason::HttpStatus(503)),
            AttemptOutcome::Failed(DropReason::HttpStatus(503)),
            AttemptOutcome::Failed(DropReason::HttpStatus(503)),
            AttemptOutcome::Sent,
        ]);
        let sleeps = Cell::new(0u32);
        let outcome = run_with_retry(
            3,
            50,
            None,
            Instant::now,
            |i| script.next(i),
            |_| sleeps.set(sleeps.get() + 1),
        );
        assert_eq!(outcome, OnBatchOutcome::Sent);
        assert_eq!(sleeps.get(), 3, "three sleeps between four attempts");
    }

    #[test]
    fn run_with_retry_exhausts_after_retry_count_plus_one_attempts() {
        // Always-500: should exhaust at `retry_count + 1 = 4` attempts.
        let script = Script::new(vec![AttemptOutcome::Failed(DropReason::HttpStatus(500)); 4]);
        let outcome = run_with_retry(
            3,
            10,
            None,
            Instant::now,
            |i| script.next(i),
            |_| {}, // no-sleep fake
        );
        assert_eq!(
            outcome,
            OnBatchOutcome::Dropped {
                reason: DropReason::HttpStatus(500),
                attempts: 4,
            },
        );
        assert_eq!(script.idx.get(), 4, "exactly retry_count + 1 = 4 attempts");
    }

    #[test]
    fn run_with_retry_carries_the_final_drop_reason_not_an_earlier_one() {
        // First attempt: 503. Second: timeout. The final outcome
        // carries the LAST reason (timeout), not the first.
        let script = Script::new(vec![
            AttemptOutcome::Failed(DropReason::HttpStatus(503)),
            AttemptOutcome::Failed(DropReason::Timeout),
        ]);
        let outcome = run_with_retry(1, 10, None, Instant::now, |i| script.next(i), |_| {});
        assert_eq!(
            outcome,
            OnBatchOutcome::Dropped {
                reason: DropReason::Timeout,
                attempts: 2,
            },
        );
    }

    #[test]
    fn run_with_retry_deadline_passed_before_first_attempt() {
        // Deadline already in the past at orchestrator entry. No
        // attempt is made at all.
        let now = Instant::now();
        let deadline = now - Duration::from_millis(1);
        let script = Script::new(vec![AttemptOutcome::Sent]);
        let outcome = run_with_retry(
            3,
            200,
            Some(deadline),
            move || now,
            |i| script.next(i),
            |_| {},
        );
        assert_eq!(
            outcome,
            OnBatchOutcome::Dropped {
                reason: DropReason::DeadlineExceeded,
                attempts: 0,
            },
        );
    }

    #[test]
    fn run_with_retry_deadline_passes_mid_loop_collapses_remaining_retries() {
        // The deadline is just past `now`. Attempt 0 fails; the
        // would-be backoff sleep extends past the deadline. Drop
        // with DeadlineExceeded.
        let now = Cell::new(Instant::now());
        let deadline = now.get() + Duration::from_millis(10);
        let script = Script::new(vec![
            AttemptOutcome::Failed(DropReason::HttpStatus(503)),
            AttemptOutcome::Sent, // would-be retry that never runs
        ]);
        // Advance the clock to just past the deadline during the
        // sleep check, so the orchestrator sees the deadline as
        // expired before sleeping.
        let now_clone = now.clone();
        let outcome = run_with_retry(
            3,
            200, // first sleep is 200ms >> 10ms deadline window
            Some(deadline),
            move || now_clone.get(),
            |i| {
                let o = script.next(i);
                // Bump the clock between attempt 0 and the deadline
                // check that follows, simulating wall-clock advance.
                now.set(deadline + Duration::from_millis(1));
                o
            },
            |_| panic!("should not sleep when the next sleep would exceed deadline"),
        );
        assert_eq!(
            outcome,
            OnBatchOutcome::Dropped {
                reason: DropReason::DeadlineExceeded,
                attempts: 1,
            },
        );
    }

    // ---- map_ureq_error ---------------------------------------------

    #[test]
    fn map_ureq_error_status_code_maps_to_http_status() {
        let err = ureq::Error::StatusCode(401);
        assert_eq!(map_ureq_error(&err), DropReason::HttpStatus(401));
        let err = ureq::Error::StatusCode(500);
        assert_eq!(map_ureq_error(&err), DropReason::HttpStatus(500));
    }

    #[test]
    fn map_ureq_error_connection_failed_maps_to_connect_refused() {
        let err = ureq::Error::ConnectionFailed;
        assert_eq!(map_ureq_error(&err), DropReason::ConnectRefused);
    }

    #[test]
    fn map_ureq_error_tls_maps_to_tls_error() {
        let err = ureq::Error::Tls("self-signed");
        assert_eq!(map_ureq_error(&err), DropReason::TlsError);
    }

    // ---- bump_drop_counter_on_drop ----------------------------------

    #[test]
    fn bump_drop_counter_on_drop_bumps_only_on_dropped_outcomes() {
        use std::sync::atomic::AtomicU64;
        use std::sync::Arc;
        // Build the smallest possible PendingBatch for this assertion.
        let drop_counter = Arc::new(AtomicU64::new(0));
        let batch = PendingBatch {
            meta_partial: crate::recorder::types::MetaPartial {
                schema_version: 1,
                trace_id: [0u8; 16],
                host: Arc::from("h"),
                pid: 1,
                start_time_realtime_ns: 0,
                sapi: Arc::from("cli"),
                uri_or_script: Arc::from("/x"),
            },
            dict: Vec::new(),
            calls: (0..5)
                .map(|i| crate::recorder::types::CallRecord {
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
                .collect(),
            size_estimate: 0,
            drop_counter: Arc::clone(&drop_counter),
        };

        bump_drop_counter_on_drop(&batch, &OnBatchOutcome::Sent);
        assert_eq!(drop_counter.load(Ordering::Acquire), 0, "Sent: no bump");

        bump_drop_counter_on_drop(
            &batch,
            &OnBatchOutcome::Dropped {
                reason: DropReason::HttpStatus(500),
                attempts: 4,
            },
        );
        assert_eq!(
            drop_counter.load(Ordering::Acquire),
            5,
            "Dropped: bumps by calls.len()",
        );
    }
}
