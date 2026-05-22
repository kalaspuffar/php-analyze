//! Phase-4 slice-3 shipper-encoder-and-HTTP integration test.
//!
//! Drives one PHP request through the full pipeline:
//!
//! `PHP fixture → recorder → channel → shipper thread →
//!  rmp_serde::to_vec_named → ureq POST → stub-ingest → /debug/batches`
//!
//! and asserts that the wire `Batch` decoded by the stub matches what
//! the PHP fixture produced. This is the first test that exercises
//! the end-to-end pipeline; the recorder-side `recorder_observer`
//! integration tests stopped at `RSHUTDOWN`-final-flush and read the
//! `recorder-dump` file.
//!
//! ## Skip conditions
//!
//! Skips with status 0 (loud `eprintln!`) when **any** of:
//!
//! - `PHP_ANALYZE_RUN_SHIPPER` env var is not set to `1`
//! - neither `php8.3` nor `php8.4` is on `PATH`
//!
//! The skip semantic mirrors the recorder integration test: an
//! `eprintln!` + early `return` is recorded by `cargo test` as a
//! pass. CI's apt-install + env-set steps are what guarantee the
//! test actually runs on the matrix entries that should exercise it.
//!
//! ## Why a single integration-test file (not many)
//!
//! Each round-trip spins up a fresh `stub-ingest` process (port-zero
//! bound, port-discovered from stdout, killed on `Drop`). Spreading
//! one assertion per `#[test]` would multiply the spawn/kill cost
//! linearly; this test packages every assertion into one
//! `cargo test --test shipper_round_trip` invocation, with the
//! `(binary, fixture)` cross-product mediated by per-fixture helpers
//! (mirrors the recorder integration test's shape).
//!
//! Task 7.7 (1000 sends / 1 connection) is bound in this file by
//! `try_round_trip_connection_reuse` against the
//! `connection_reuse.php` fixture, using the
//! `/debug/connection_count` endpoint added in the
//! `stub-ingest-connection-counter` change.
//!
//! Task 7.8 (retry-exhaust drop counter + §5.2 step-4 `E_NOTICE`
//! line shape) is bound in this file by
//! `try_round_trip_retry_exhaust` against the
//! `retry_exhaust.php` fixture, using the `--respond-with` flag
//! added in the `stub-ingest-configurable-failure` change.
//! The same round-trip also binds AC-SH-4 ("Bearer token never
//! appears in any log output") via a sentinel-token grep over the
//! captured PHP error log.
//!
//! Tasks 7.5 (auth-header byte-equal) and 7.6 (User-Agent string)
//! are unblocked by the prior `stub-ingest-header-capture` change
//! but bound in a separate follow-up. Task 7.10 (MSHUTDOWN drain
//! within grace) remains **deferred** — it needs the
//! `stub-ingest-slow-mode` endpoint (queued in `COMMENTS.md` §2 P0).

use std::env;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::time::Duration;

use php_analyze::wire;

#[test]
fn shipper_round_trip_lands_one_batch_on_stub() {
    if env::var("PHP_ANALYZE_RUN_SHIPPER").as_deref() != Ok("1") {
        eprintln!(
            "shipper_round_trip: skipped (set PHP_ANALYZE_RUN_SHIPPER=1 to run \
             the Phase-4 slice-3 PHP integration test)"
        );
        return;
    }

    let candidates = ["php8.3", "php8.4"];
    let available: Vec<&str> = candidates
        .iter()
        .copied()
        .filter(|name| {
            Command::new(name)
                .arg("-v")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        })
        .collect();

    if available.is_empty() {
        eprintln!(
            "shipper_round_trip: skipped (no php8.3 or php8.4 found; tried: {})",
            candidates.join(", "),
        );
        return;
    }

    let cdylib = build_cdylib();

    let mut exercised: Vec<&str> = Vec::new();
    let mut skipped: Vec<&str> = Vec::new();
    for binary in &available {
        // Each fixture gets its own freshly-spawned stub: a single
        // PHP process opens one TCP connection to the stub's
        // ureq::Agent, but the second PHP process would inevitably
        // open a second. Spawning three stubs (one per fixture)
        // keeps each fixture's per-process invariants independent
        // (connection counts, headers slot, batch store).
        let primary = try_round_trip(binary, &cdylib);
        let reuse = try_round_trip_connection_reuse(binary, &cdylib);
        let retry = try_round_trip_retry_exhaust(binary, &cdylib);
        match (primary, reuse, retry) {
            (RoundTripOutcome::Passed, RoundTripOutcome::Passed, RoundTripOutcome::Passed) => {
                exercised.push(binary)
            }
            (RoundTripOutcome::SkippedModuleApi, _, _)
            | (_, RoundTripOutcome::SkippedModuleApi, _)
            | (_, _, RoundTripOutcome::SkippedModuleApi) => skipped.push(binary),
        }
    }

    if !skipped.is_empty() {
        eprintln!(
            "shipper_round_trip: skipped {} PHP binar{} due to module-API mismatch: {}",
            skipped.len(),
            if skipped.len() == 1 { "y" } else { "ies" },
            skipped.join(", "),
        );
    }

    assert!(
        !exercised.is_empty(),
        "shipper_round_trip: no PHP binary completed a round-trip; all candidates \
         skipped on module API or unavailable ({} tried: {})",
        candidates.len(),
        candidates.join(", "),
    );
}

enum RoundTripOutcome {
    Passed,
    SkippedModuleApi,
}

fn try_round_trip(php_binary: &str, cdylib: &Path) -> RoundTripOutcome {
    let token = "rt-token-1";
    let path = "/v1/ingest";

    let stub = StubProcess::spawn(token, path);
    let server_url = format!("http://127.0.0.1:{}{}", stub.port, path);

    // Build the per-run `php.ini` that points at the just-built cdylib
    // and at the freshly-spawned stub. The `server_url` is plain
    // `http://` (no TLS) — slice-3 supports plain HTTP for the stub
    // by design; AC-SH-5's TLS verification is a deferred task.
    let tmpdir = tempfile::tempdir().expect("tempdir for php.ini");
    let ini_path = tmpdir.path().join("shipper.ini");
    let ini_body = format!(
        concat!(
            "extension={cdylib}\n",
            "php_analyze.enabled        = 1\n",
            "php_analyze.server_url     = \"{url}\"\n",
            "php_analyze.auth_token     = \"{token}\"\n",
            "php_analyze.spike_observer = 0\n",
            // Tight `shutdown_grace` so the test exits promptly even
            // if the stub somehow drops the connection. The default
            // is `1500 ms`; `300 ms` is plenty for one batch on
            // loopback.
            "php_analyze.shutdown_grace = 300\n",
        ),
        cdylib = cdylib.display(),
        url = server_url,
        token = token,
    );
    std::fs::write(&ini_path, ini_body).expect("write php.ini");

    let fixture = locate_fixture("noop.php");
    let output = Command::new(php_binary)
        .arg("-n")
        .arg("-c")
        .arg(&ini_path)
        .arg(&fixture)
        .output()
        .unwrap_or_else(|e| panic!("invoke {php_binary} {fixture:?}: {e}"));

    if mentions_module_api_mismatch(&output.stdout) || mentions_module_api_mismatch(&output.stderr)
    {
        return RoundTripOutcome::SkippedModuleApi;
    }

    assert!(
        output.status.success(),
        "{php_binary} exited non-zero on noop.php (status {:?}); stderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );

    // The MSHUTDOWN drain has flushed and POSTed the batch by the
    // time PHP exits. Pull the stub's `/debug/batches` queue and
    // decode it.
    let batches = stub.fetch_batches();
    assert_eq!(
        batches.len(),
        1,
        "{php_binary} noop.php: expected exactly 1 batch on the stub, got {} ({:?})",
        batches.len(),
        batches.iter().map(|b| b.calls.len()).collect::<Vec<_>>(),
    );

    let batch = &batches[0];
    assert_eq!(
        batch.calls.len(),
        2,
        "{php_binary} noop.php: expected 2 calls (script body + noop), got {}",
        batch.calls.len(),
    );
    assert_eq!(
        batch.meta.dropped_records, 0,
        "{php_binary} noop.php: no drops on a sub-threshold workload, got {}",
        batch.meta.dropped_records,
    );
    assert_eq!(
        batch.meta.schema_version, 1,
        "{php_binary} noop.php: wire schema version is 1",
    );
    assert!(
        !batch.meta.trace_id.is_empty(),
        "{php_binary} noop.php: trace_id is populated",
    );
    let noop_dict: Vec<&wire::DictEntry> = batch.dict.iter().filter(|d| d.fqn == "noop").collect();
    assert_eq!(
        noop_dict.len(),
        1,
        "{php_binary} noop.php: noop appears in dict exactly once, got {} ({:?})",
        noop_dict.len(),
        batch.dict.iter().map(|d| &d.fqn).collect::<Vec<_>>(),
    );

    // `drop(stub)` kills the stub process so the next iteration
    // (different PHP binary) gets a fresh stub on a fresh port.
    drop(stub);
    RoundTripOutcome::Passed
}

/// AC-SH-6 binding round-trip: drive the `connection_reuse.php`
/// fixture (1000 `noop()` calls under `flush_records = 1`) through
/// one PHP process so the shipper's single `ureq::Agent` POSTs
/// ≈1000 times sequentially, then assert the stub's
/// `/debug/connection_count` reports exactly `1`. Closes task 7.7
/// of `shipper_round_trip.rs`'s deferred-test backlog.
fn try_round_trip_connection_reuse(php_binary: &str, cdylib: &Path) -> RoundTripOutcome {
    let token = "rt-token-cr";
    let path = "/v1/ingest";

    let stub = StubProcess::spawn(token, path);
    let server_url = format!("http://127.0.0.1:{}{}", stub.port, path);

    // `flush_records = 1` forces a flush on every emitted exit
    // record, so each `noop()` call in the fixture produces a
    // flushable `PendingBatch` → one POST. The shipper's
    // `RmpEncodeAndHttpPost` reuses one `ureq::Agent` for all of
    // them; AC-SH-6 says that maps to exactly one TCP connection.
    // `shutdown_grace` is bumped relative to the noop fixture
    // because the drain may need to push out a tail of in-flight
    // batches before exit.
    let tmpdir = tempfile::tempdir().expect("tempdir for php.ini");
    let ini_path = tmpdir.path().join("shipper_reuse.ini");
    let ini_body = format!(
        concat!(
            "extension={cdylib}\n",
            "php_analyze.enabled        = 1\n",
            "php_analyze.server_url     = \"{url}\"\n",
            "php_analyze.auth_token     = \"{token}\"\n",
            "php_analyze.spike_observer = 0\n",
            "php_analyze.flush_records  = 1\n",
            "php_analyze.shutdown_grace = 5000\n",
        ),
        cdylib = cdylib.display(),
        url = server_url,
        token = token,
    );
    std::fs::write(&ini_path, ini_body).expect("write shipper_reuse.ini");

    let fixture = locate_fixture("connection_reuse.php");
    let output = Command::new(php_binary)
        .arg("-n")
        .arg("-c")
        .arg(&ini_path)
        .arg(&fixture)
        .output()
        .unwrap_or_else(|e| panic!("invoke {php_binary} {fixture:?}: {e}"));

    if mentions_module_api_mismatch(&output.stdout) || mentions_module_api_mismatch(&output.stderr)
    {
        return RoundTripOutcome::SkippedModuleApi;
    }

    assert!(
        output.status.success(),
        "{php_binary} exited non-zero on connection_reuse.php (status {:?}); stderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );

    // AC-SH-6: 1 TCP connection across ≈1000 POSTs from one agent.
    let count = stub.fetch_connection_count();
    assert_eq!(
        count, 1,
        "{php_binary} connection_reuse.php: expected exactly 1 distinct TCP \
         connection over loopback keep-alive (AC-SH-6), got {count}",
    );

    // Sanity: at least one batch landed on the stub. The exact
    // count is `1000 ± a few` because the script body's exit
    // record adds one and RSHUTDOWN's final flush can split a
    // tail; the connection-count contract is `1`, not the batch
    // count.
    let batches = stub.fetch_batches();
    assert!(
        !batches.is_empty(),
        "{php_binary} connection_reuse.php: expected at least one batch on the stub, got 0",
    );

    drop(stub);
    RoundTripOutcome::Passed
}

/// AC-SH-2 / AC-SH-4 / "Exactly one `E_NOTICE` per dropped batch"
/// binding round-trip: drive the `retry_exhaust.php` fixture
/// against a `crates/stub-ingest` spawned with
/// `--respond-with 500`. The shipper makes `retry_count + 1 = 4`
/// failed POSTs, exhausts retries, and emits one `E_NOTICE` of
/// the §5.2 step-4 shape. Asserts:
///
/// 1. `/debug/batches.len() == 4` — AC-SH-2.
/// 2. The captured error-log contains exactly one `E_NOTICE`
///    matching the §5.2 step-4 drop-notice format, naming
///    `http 500` and `(attempt 4)`.
/// 3. The configured `auth_token` sentinel does NOT appear in
///    the captured error-log bytes — AC-SH-4.
///
/// Closes task 7.8 of `shipper_round_trip.rs`'s deferred-test
/// backlog.
fn try_round_trip_retry_exhaust(php_binary: &str, cdylib: &Path) -> RoundTripOutcome {
    // Sentinel token: a unique-looking string the operator
    // wouldn't pick by accident. If any prefix or suffix of this
    // string ever appears in the captured error-log bytes, the
    // AC-SH-4 assertion fails loudly. See design D-7.
    let token = "retry-exhaust-secret-do-not-leak-987654321";
    let path = "/v1/ingest";

    let stub = StubProcess::spawn_with_respond_with(token, path, 500);
    let server_url = format!("http://127.0.0.1:{}{}", stub.port, path);

    let tmpdir = tempfile::tempdir().expect("tempdir for retry-exhaust ini");
    let ini_path = tmpdir.path().join("shipper_retry_exhaust.ini");
    let error_log_path = tmpdir.path().join("php_errors.log");
    // `retry_backoff_ms = 10` keeps the retry path under 1s of
    // wall clock (sleeps of 10ms + 20ms + 40ms = 70ms, plus four
    // ~5ms loopback round-trips). `http_timeout_ms = 200` caps
    // each attempt — if the loopback ever stalls past that, the
    // resulting `DropReason::Timeout` would mismatch the
    // asserted `http 500` and the test would fail loudly.
    // `shutdown_grace = 5000` is generous so the drain completes
    // even on a slow CI runner. `error_log` + `log_errors = 1` +
    // `error_reporting = E_ALL` route PHP's `E_NOTICE` to the
    // tempfile.
    let ini_body = format!(
        concat!(
            "extension={cdylib}\n",
            "php_analyze.enabled          = 1\n",
            "php_analyze.server_url       = \"{url}\"\n",
            "php_analyze.auth_token       = \"{token}\"\n",
            "php_analyze.spike_observer   = 0\n",
            "php_analyze.retry_count      = 3\n",
            "php_analyze.retry_backoff_ms = 10\n",
            "php_analyze.http_timeout_ms  = 200\n",
            "php_analyze.shutdown_grace   = 5000\n",
            "error_log                    = \"{error_log}\"\n",
            "log_errors                   = 1\n",
            "error_reporting              = E_ALL\n",
            "display_errors               = 0\n",
        ),
        cdylib = cdylib.display(),
        url = server_url,
        token = token,
        error_log = error_log_path.display(),
    );
    std::fs::write(&ini_path, ini_body).expect("write shipper_retry_exhaust.ini");

    let fixture = locate_fixture("retry_exhaust.php");
    let output = Command::new(php_binary)
        .arg("-n")
        .arg("-c")
        .arg(&ini_path)
        .arg(&fixture)
        .output()
        .unwrap_or_else(|e| panic!("invoke {php_binary} {fixture:?}: {e}"));

    // Read the captured PHP error log up front. With
    // `log_errors = 1` + `display_errors = 0` + a file `error_log`,
    // PHP's own startup warnings (including the module-API
    // mismatch we use as a skip signal on hosts whose `php8.x`
    // doesn't match the cdylib's build ABI) go to this file,
    // NOT to stderr. The skip check therefore has to look in
    // three places.
    let error_log = std::fs::read_to_string(&error_log_path).unwrap_or_default();

    if mentions_module_api_mismatch(&output.stdout)
        || mentions_module_api_mismatch(&output.stderr)
        || mentions_module_api_mismatch(error_log.as_bytes())
    {
        return RoundTripOutcome::SkippedModuleApi;
    }

    assert!(
        output.status.success(),
        "{php_binary} exited non-zero on retry_exhaust.php (status {:?}); stderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );

    // AC-SH-2: every attempt's body lands in the store (design
    // D-3 of the stub-side change). `retry_count = 3` → 4 total
    // attempts → 4 stored bodies.
    let batches = stub.fetch_batches();
    assert_eq!(
        batches.len(),
        4,
        "{php_binary} retry_exhaust.php: expected exactly retry_count+1=4 stored bodies \
         on the stub (AC-SH-2), got {} ({:?})",
        batches.len(),
        batches.iter().map(|b| b.calls.len()).collect::<Vec<_>>(),
    );

    // AC-SH-4: the configured token MUST NOT appear anywhere in
    // the captured error-log bytes. The sentinel is unique
    // enough that a partial-match would also be a real leak.
    assert!(
        !error_log.contains(token),
        "{php_binary} retry_exhaust.php: bearer token leaked into PHP error log \
         (AC-SH-4 violation). error_log contents:\n{error_log}",
    );

    // §5.2 step-4 / "Exactly one E_NOTICE per dropped batch":
    // the captured error-log contains exactly one line matching
    // the documented drop-notice shape. Substring assertions
    // avoid coupling to PHP's surrounding
    // `[<timestamp>] PHP Notice: ... in Unknown on line 0`
    // wrapper which varies across PHP versions.
    let drop_notice_lines: Vec<&str> = error_log
        .lines()
        .filter(|line| line.contains("php-analyze: dropped"))
        .collect();
    assert_eq!(
        drop_notice_lines.len(),
        1,
        "{php_binary} retry_exhaust.php: expected exactly one drop-notice line \
         in the PHP error log, got {} ({drop_notice_lines:?}); full error_log:\n{error_log}",
        drop_notice_lines.len(),
    );
    let notice = drop_notice_lines[0];
    assert!(
        notice.contains("http 500"),
        "{php_binary} retry_exhaust.php: drop notice must name `http 500`; got {notice:?}",
    );
    assert!(
        notice.contains("(attempt 4)"),
        "{php_binary} retry_exhaust.php: drop notice must name `(attempt 4)` \
         (retry_count+1); got {notice:?}",
    );
    assert!(
        notice.contains(&server_url),
        "{php_binary} retry_exhaust.php: drop notice must name the server URL {server_url:?}; \
         got {notice:?}",
    );

    drop(stub);
    RoundTripOutcome::Passed
}

/// Spawned `stub-ingest` process. Holds the child handle and the
/// bound port. The child is killed on `Drop` so a panicking test
/// leaves no orphan process behind.
struct StubProcess {
    child: Child,
    port: u16,
}

impl StubProcess {
    fn spawn(token: &str, path: &str) -> Self {
        Self::spawn_with_args(token, path, &[])
    }

    /// Spawn the stub with a configured `--respond-with <status>`
    /// for the retry-exhaust integration test. Other args
    /// (`--auth-token`, `--bind`, `--path`) match `spawn`.
    fn spawn_with_respond_with(token: &str, path: &str, respond_with: u16) -> Self {
        let status = respond_with.to_string();
        Self::spawn_with_args(token, path, &["--respond-with", &status])
    }

    /// Shared spawn body: builds the `Command`, applies the
    /// standard `--bind 127.0.0.1:0` + `--auth-token` + `--path`
    /// args, appends caller-supplied `extra_args`, and waits for
    /// the bind protocol's `bound:` / `ready` handshake. The
    /// `extra_args` slice lets callers opt into newer CLI flags
    /// without growing this constructor's parameter list.
    fn spawn_with_args(token: &str, path: &str, extra_args: &[&str]) -> Self {
        let bin = stub_ingest_binary();
        let mut command = Command::new(&bin);
        command
            .arg("--bind")
            .arg("127.0.0.1:0")
            .arg("--auth-token")
            .arg(token)
            .arg("--path")
            .arg(path);
        for arg in extra_args {
            command.arg(arg);
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()));

        // Read the `bound:` and `ready` lines from stdout under a
        // 5-second hard cap. The stub's bind protocol guarantees the
        // two lines arrive in that order with flushes between; if
        // the stub hangs we want to fail loudly with a clear panic
        // rather than block `cargo test`.
        let stdout = child
            .stdout
            .take()
            .expect("stub-ingest stdout was requested as piped");
        let port = match handshake_with_timeout(stdout, Duration::from_secs(5)) {
            Ok(port) => port,
            Err(msg) => {
                // Kill the child eagerly so the worker thread's
                // pending `read_line` returns EOF and exits, rather
                // than waiting for `StubProcess::Drop` (which the
                // panic below would trigger but only after stack
                // unwinding settles).
                let _ = child.kill();
                let _ = child.wait();
                panic!("stub-ingest handshake: {msg}");
            }
        };
        Self { child, port }
    }

    fn fetch_batches(&self) -> Vec<wire::Batch> {
        // Build a one-shot ureq agent for `/debug/batches`. Reusing
        // the production crate's `ureq` saves a dep here.
        let url = format!("http://127.0.0.1:{}/debug/batches", self.port);
        let response = ureq::get(&url)
            .call()
            .unwrap_or_else(|e| panic!("GET {url}: {e}"));
        let mut body = response.into_body();
        let bytes = body
            .read_to_vec()
            .unwrap_or_else(|e| panic!("read /debug/batches body: {e}"));
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "decode /debug/batches JSON: {e}; body: {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    }

    /// GET `/debug/connection_count` and return the decoded
    /// `{"count": N}` value. Used by the AC-SH-6 binding test
    /// below: a single PHP process driving ≈1000 sends via the
    /// shipper's one `ureq::Agent` must result in `count == 1`.
    fn fetch_connection_count(&self) -> usize {
        let url = format!("http://127.0.0.1:{}/debug/connection_count", self.port);
        let response = ureq::get(&url)
            .call()
            .unwrap_or_else(|e| panic!("GET {url}: {e}"));
        let mut body = response.into_body();
        let bytes = body
            .read_to_vec()
            .unwrap_or_else(|e| panic!("read /debug/connection_count body: {e}"));
        #[derive(serde::Deserialize)]
        struct CountBody {
            count: usize,
        }
        let decoded: CountBody = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "decode /debug/connection_count JSON: {e}; body: {}",
                String::from_utf8_lossy(&bytes)
            )
        });
        decoded.count
    }
}

impl Drop for StubProcess {
    fn drop(&mut self) {
        // SIGTERM via `Child::kill` (which uses SIGKILL on Linux).
        // The stub has no cleanup to do; SIGKILL is fine.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read the stub's two handshake lines (`bound: <addr>` then `ready`)
/// under a wall-clock budget. Returns the parsed port on success, an
/// error message on any failure shape.
///
/// Implementation: the read happens on a worker thread; the test
/// thread waits on a `sync_channel(1).recv_timeout(timeout)`. On
/// timeout the test thread returns an error; the caller is expected
/// to `child.kill()` so the worker's pending `read_line` returns EOF
/// and the worker exits. This is the "worker + recv_timeout" idiom
/// recommended over a `std::process::exit(137)` watchdog, which
/// would tear down the entire `cargo test` process unconditionally
/// (SEH-1 round-2 review fix).
fn handshake_with_timeout(stdout: ChildStdout, timeout: Duration) -> Result<u16, String> {
    let (tx, rx) = sync_channel::<Result<u16, String>>(1);
    std::thread::spawn(move || {
        let _ = tx.send(read_handshake(stdout));
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(format!("readline timeout after {timeout:?}")),
        Err(RecvTimeoutError::Disconnected) => {
            Err("worker thread exited without sending a result".to_string())
        }
    }
}

fn read_handshake(stdout: ChildStdout) -> Result<u16, String> {
    let mut reader = BufReader::new(stdout);
    let mut bound = String::new();
    let bytes = reader
        .read_line(&mut bound)
        .map_err(|e| format!("read bound line: {e}"))?;
    if bytes == 0 {
        return Err("stub stdout closed before `bound:`".to_string());
    }
    let port = parse_bound_line(&bound)?;
    let mut ready = String::new();
    let bytes = reader
        .read_line(&mut ready)
        .map_err(|e| format!("read ready line: {e}"))?;
    if bytes == 0 {
        return Err("stub stdout closed before `ready`".to_string());
    }
    if ready.trim() != "ready" {
        return Err(format!("expected `ready`, got {:?}", ready.trim()));
    }
    Ok(port)
}

/// Parse a `bound: 127.0.0.1:NNNNN` stdout line and return the port.
/// Factored out for unit-test reach.
fn parse_bound_line(line: &str) -> Result<u16, String> {
    let trimmed = line.trim();
    let addr = trimmed
        .strip_prefix("bound: ")
        .ok_or_else(|| format!("`bound:` line malformed: {trimmed:?}"))?;
    let port = addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .ok_or_else(|| format!("bound addr has no port: {addr:?}"))?;
    Ok(port)
}

fn mentions_module_api_mismatch(buf: &[u8]) -> bool {
    let s = String::from_utf8_lossy(buf);
    s.contains("module API")
}

fn build_cdylib() -> PathBuf {
    // Build (or reuse) the cdylib that PHP will load. `cargo build`
    // is a no-op when up to date; this gives us a single, fresh
    // artifact across all PHP binaries.
    let out = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "php-analyze",
            // No `--features recorder-dump`: this slice posts to a
            // real HTTP endpoint, no dump file needed.
        ])
        .output()
        .expect("cargo build runnable from the test");
    assert!(
        out.status.success(),
        "cargo build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    target_dir().join("debug").join("libphp_analyze.so")
}

fn stub_ingest_binary() -> PathBuf {
    // Build the stub if it's not already there. Same no-op-when-up-
    // to-date semantics as the cdylib build above.
    let out = Command::new(env!("CARGO"))
        .args(["build", "-p", "stub-ingest"])
        .output()
        .expect("cargo build stub-ingest runnable from the test");
    assert!(
        out.status.success(),
        "cargo build stub-ingest failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    target_dir().join("debug").join("stub-ingest")
}

fn target_dir() -> PathBuf {
    // `CARGO_TARGET_DIR` overrides the default `target/` location;
    // when unset, fall back to the repo-root `target/`. The
    // recorder integration test uses the same heuristic.
    if let Ok(dir) = env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(std::path::Path::parent)
        .map(|p| p.join("target"))
        .expect("crate dir → crates → repo root")
}

fn locate_fixture(name: &str) -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crate dir → crates → repo root");
    let path = repo_root.join("tests").join("php-shipper").join(name);
    assert!(
        path.exists(),
        "fixture {name} not found at {} (manifest_dir: {})",
        path.display(),
        manifest_dir.display(),
    );
    path
}
