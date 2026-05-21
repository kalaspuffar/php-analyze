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
//! Tasks 7.5 (auth-header byte-equal), 7.6 (User-Agent string), 7.7
//! (1000 sends / 1 connection), 7.8 (retry-exhaust drop counter
//! visible on the next batch), 7.9 (token never leaked to error
//! log), and 7.10 (MSHUTDOWN drain within grace) are **deferred to a
//! follow-up OpenSpec change**. They require either new
//! `stub-ingest` debug endpoints (header capture, connection
//! counter, simulated-slow mode) or PHP error-log capture
//! infrastructure that does not yet exist. This file covers tasks
//! 7.1–7.4 only — the end-to-end happy-path verification.

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
        match try_round_trip(binary, &cdylib) {
            RoundTripOutcome::Passed => exercised.push(binary),
            RoundTripOutcome::SkippedModuleApi => skipped.push(binary),
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

/// Spawned `stub-ingest` process. Holds the child handle and the
/// bound port. The child is killed on `Drop` so a panicking test
/// leaves no orphan process behind.
struct StubProcess {
    child: Child,
    port: u16,
}

impl StubProcess {
    fn spawn(token: &str, path: &str) -> Self {
        let bin = stub_ingest_binary();
        let mut child = Command::new(&bin)
            .arg("--bind")
            .arg("127.0.0.1:0")
            .arg("--auth-token")
            .arg(token)
            .arg("--path")
            .arg(path)
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
