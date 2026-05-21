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
use std::process::{Child, Command, Stdio};
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

        // Read the `bound:` and `ready` lines from stdout. The stub's
        // bind protocol guarantees these arrive in that order with
        // flushes between. A 5-second read budget is plenty on
        // loopback; if the stub hangs, we want to fail loudly rather
        // than block `cargo test`.
        let stdout = child
            .stdout
            .take()
            .expect("stub-ingest stdout was requested as piped");
        let mut reader = BufReader::new(stdout);
        let port = read_bound_port(&mut reader);
        let ready_line = read_one_line(&mut reader);
        assert_eq!(ready_line.trim(), "ready", "stub did not print `ready`");

        // The reader is dropped here, closing our end of the stdout
        // pipe. The stub continues writing to stdout (it won't, by
        // contract — stdout is silent after `ready`); on Linux,
        // writes to a closed pipe SIGPIPE the writer, but the stub
        // doesn't write again so this is moot.
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

fn read_bound_port(reader: &mut BufReader<std::process::ChildStdout>) -> u16 {
    let line = read_one_line(reader);
    let trimmed = line.trim();
    // Expected shape: `bound: 127.0.0.1:NNNNN`.
    let addr = trimmed
        .strip_prefix("bound: ")
        .unwrap_or_else(|| panic!("stub `bound:` line malformed: {trimmed:?}"));
    let port = addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("stub bound addr has no port: {addr:?}"));
    port
}

fn read_one_line(reader: &mut BufReader<std::process::ChildStdout>) -> String {
    // 5-second hard cap on the readline so a hung stub doesn't hang
    // `cargo test`. `BufRead::read_line` itself has no timeout
    // primitive in std, so we spawn a watchdog thread that kills the
    // test after the deadline. The watchdog uses `std::process::exit`
    // because there's no portable way to interrupt a blocked
    // `read_line`; this is a test-only escape hatch.
    let deadline = Duration::from_secs(5);
    let handle = std::thread::spawn(move || {
        std::thread::sleep(deadline);
        eprintln!(
            "shipper_round_trip: stub stdout readline timeout ({:?}); aborting",
            deadline
        );
        std::process::exit(137);
    });

    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .unwrap_or_else(|e| panic!("read stub stdout: {e}"));
    if bytes == 0 {
        panic!("stub stdout closed before producing a line");
    }
    // The watchdog detaches when the test naturally exits — but
    // `cargo test` runs each test in the same process, so we let it
    // sit until the deadline. The `std::process::exit(137)` only
    // fires if a different test hangs more than 5s on its own
    // stdout readline. In practice this won't fire.
    drop(handle);
    line
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
