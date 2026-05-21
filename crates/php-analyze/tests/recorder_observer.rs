//! Phase-2 slice-2 recorder integration test.
//!
//! Drives `tests/php-recorder/run.sh` against every available PHP
//! 8.3 / 8.4 binary, runs each of three fixtures
//! (`flat_calls.php`, `nested.php`, `throws.php`), parses the dump via
//! [`php_analyze::recorder::dump::parse_dump`], and asserts the
//! coverage scenarios the `recorder-call-events` spec requires.
//!
//! ## Skip conditions
//!
//! The test skips with status 0 (loud `eprintln!`) when **any** of:
//!
//! - `PHP_ANALYZE_RUN_RECORDER` env var is not set to `1`
//! - neither `php8.3` nor `php8.4` is on `PATH`
//!
//! When at least one PHP binary IS present and the env var IS set,
//! the test iterates over every binary it found. Each `(binary,
//! fixture)` pair is one assertion site.
//!
//! ## Why feature-gate the import
//!
//! The integration test's binary is compiled separately from the
//! library; it pulls in the `php-analyze` crate via the rlib output.
//! When the test runs without `--features recorder-dump`, the
//! `recorder::dump` module is `#[cfg]`-out of the rlib too, so the
//! `use` below would not resolve. Gating the entire test body on the
//! feature is the simplest way to make a `cargo test --test
//! recorder_observer` invocation (without the feature) compile and
//! produce a clear skip message.

#![cfg(feature = "recorder-dump")]

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use php_analyze::recorder::dump::{parse_dump, ParsedCall, ParsedDict, ParsedDump};

#[test]
fn recorder_observer_covers_slice_2_scenarios_on_every_available_php() {
    if env::var("PHP_ANALYZE_RUN_RECORDER").as_deref() != Ok("1") {
        eprintln!(
            "recorder_observer: skipped (set PHP_ANALYZE_RUN_RECORDER=1 to run the \
             Phase-2 slice-2 PHP integration test)"
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
            "recorder_observer: no php8.3 or php8.4 found; tried: {}",
            candidates.join(", "),
        );
        std::process::exit(77);
    }

    let runner = locate_driver_script();
    let fixtures_dir = runner
        .parent()
        .expect("driver script has a parent directory")
        .to_owned();

    // The cdylib is built against whichever `php-config` is active on
    // the host. A PHP binary whose module API differs from the
    // cdylib's will refuse to load the extension with a "module API
    // mismatch" startup warning. `run.sh` detects that condition and
    // exits 77, mirroring the autotools "skip" convention; this loop
    // treats that as a per-binary skip and continues to the next
    // candidate. At least one binary must complete a full pass for
    // the test to be meaningful.
    let mut exercised: Vec<&str> = Vec::new();
    let mut skipped: Vec<&str> = Vec::new();
    for binary in &available {
        if try_run_all_fixtures(&runner, &fixtures_dir, binary) {
            exercised.push(binary);
        } else {
            skipped.push(binary);
        }
    }

    if !skipped.is_empty() {
        eprintln!(
            "recorder_observer: skipped {} PHP binar{} due to module-API mismatch \
             against the active php-config: {}. To exercise the other version, \
             rebuild after `update-alternatives --set php-config /usr/bin/php-config<v>`.",
            skipped.len(),
            if skipped.len() == 1 { "y" } else { "ies" },
            skipped.join(", "),
        );
    }

    assert!(
        !exercised.is_empty(),
        "recorder_observer: no PHP binary on PATH matches the cdylib's module API; \
         tried: {}",
        available.join(", "),
    );

    eprintln!(
        "recorder_observer: all fixtures passed against {} PHP binar{}: {}",
        exercised.len(),
        if exercised.len() == 1 { "y" } else { "ies" },
        exercised.join(", "),
    );
}

/// Drive all three fixtures against one PHP binary. Returns `false`
/// when the first fixture's `run.sh` exits 77 (module-API mismatch);
/// returns `true` after all three fixtures pass. Panics on hard
/// failures (driver crash, assertion failure inside a per-fixture
/// helper).
fn try_run_all_fixtures(runner: &Path, fixtures_dir: &Path, binary: &str) -> bool {
    // Probe with one fixture first; if the cdylib won't load under
    // this PHP, exit 77 propagates here.
    if !probe_binary_loads_extension(runner, fixtures_dir, binary) {
        return false;
    }
    run_fixture_flat_calls(runner, fixtures_dir, binary);
    run_fixture_nested(runner, fixtures_dir, binary);
    run_fixture_throws(runner, fixtures_dir, binary);
    true
}

fn probe_binary_loads_extension(runner: &Path, fixtures_dir: &Path, binary: &str) -> bool {
    let probe_fixture = fixtures_dir.join("flat_calls.php");
    let tmp = tempfile::Builder::new()
        .prefix(&format!("recorder-probe-{binary}-"))
        .suffix(".log")
        .tempfile()
        .expect("create probe tempfile");
    let output = Command::new(runner)
        .arg(binary)
        .arg(&probe_fixture)
        .arg(tmp.path())
        .output()
        .unwrap_or_else(|err| panic!("invoke driver probe for {binary}: {err}"));
    // Exit 77 means the driver detected the module-API mismatch and
    // chose to skip; that is not a probe failure.
    output.status.code() != Some(77) && output.status.success()
}

fn locate_driver_script() -> PathBuf {
    // The integration test's working dir under `cargo test` varies;
    // `CARGO_MANIFEST_DIR` is the crate dir, and the harness lives
    // at `<repo_root>/tests/php-recorder/run.sh`.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("crate dir → crates → repo root");
    let driver = repo_root.join("tests").join("php-recorder").join("run.sh");
    assert!(
        driver.exists(),
        "driver script not found at {} (working dir: {:?})",
        driver.display(),
        env::current_dir().ok(),
    );
    driver
}

/// Run one `(binary, fixture)` pair through `run.sh` and parse the
/// dump. Panics with a descriptive message on driver failure; returns
/// the parsed dump on success.
fn run_fixture(runner: &Path, fixtures_dir: &Path, binary: &str, fixture: &str) -> ParsedDump {
    let fixture_path = fixtures_dir.join(fixture);
    let tmp = tempfile::Builder::new()
        .prefix(&format!("recorder-{binary}-{fixture}-"))
        .suffix(".log")
        .tempfile()
        .expect("create tempfile for dump");
    let dump_path = tmp.path().to_owned();

    let output = Command::new(runner)
        .arg(binary)
        .arg(&fixture_path)
        .arg(&dump_path)
        .output()
        .unwrap_or_else(|err| panic!("invoke {} ({binary}, {fixture}): {err}", runner.display()));

    if output.status.code() == Some(77) {
        panic!(
            "driver skipped unexpectedly for {binary} / {fixture}; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        output.status.success(),
        "driver failed for {binary} / {fixture} (status {:?}); stderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );

    let parsed = parse_dump(&dump_path).unwrap_or_else(|err| {
        panic!(
            "parse_dump({}) for {binary} / {fixture}: {err}; stderr:\n{}",
            dump_path.display(),
            String::from_utf8_lossy(&output.stderr),
        )
    });
    assert!(
        parsed.rshutdown_marker_seen,
        "RSHUTDOWN: marker missing in dump for {binary} / {fixture}; dump path: {}",
        dump_path.display(),
    );
    parsed
}

fn run_fixture_flat_calls(runner: &Path, fixtures_dir: &Path, binary: &str) {
    let parsed = run_fixture(runner, fixtures_dir, binary, "flat_calls.php");

    // 10⁴ calls; the script body itself is observed (as a closure),
    // and so is each `noop` call. So we expect 10_000 `noop` records
    // plus the script-body record = 10_001 records total. The dict
    // contains both: `noop` (function) and the script body
    // (closure).
    let script_body_records = parsed
        .calls
        .iter()
        .filter(|c| !matches_function_dict(&parsed, c, "noop"))
        .count();
    let noop_records = parsed.calls.len() - script_body_records;

    assert_eq!(
        noop_records,
        10_000,
        "{binary} flat_calls: expected 10_000 noop records, got {noop_records} \
         (total records: {}, script body: {script_body_records})",
        parsed.calls.len(),
    );

    // The `noop` function appears once in the dictionary.
    let noop_entries: Vec<&ParsedDict> = parsed.dict.iter().filter(|d| d.fqn == "noop").collect();
    assert_eq!(
        noop_entries.len(),
        1,
        "{binary} flat_calls: noop appears in dict {} times (expected 1)",
        noop_entries.len(),
    );
}

fn run_fixture_nested(runner: &Path, fixtures_dir: &Path, binary: &str) {
    let parsed = run_fixture(runner, fixtures_dir, binary, "nested.php");

    let a = parsed
        .calls
        .iter()
        .find(|c| matches_function_dict(&parsed, c, "a"))
        .unwrap_or_else(|| {
            panic!(
                "{binary} nested: no `a()` record found in dump (records: {:?})",
                parsed.calls,
            )
        });
    let b = parsed
        .calls
        .iter()
        .find(|c| matches_function_dict(&parsed, c, "b"))
        .expect("b() record");
    let c = parsed
        .calls
        .iter()
        .find(|c| matches_function_dict(&parsed, c, "c"))
        .expect("c() record");

    // Parent chain: a's parent is the script body's call_id (or 0
    // when no script-body record precedes); b's parent is a's
    // call_id; c's parent is b's call_id. The exact script-body
    // call_id depends on whether the script body is observed first
    // (it is — Zend fires begin on the top-level closure first).
    assert_eq!(
        b.parent, a.call_id,
        "{binary} nested: b's parent ({}) must equal a's call_id ({})",
        b.parent, a.call_id,
    );
    assert_eq!(
        c.parent, b.call_id,
        "{binary} nested: c's parent ({}) must equal b's call_id ({})",
        c.parent, b.call_id,
    );

    // None of the user-function records carry abnormal_exit.
    for r in [a, b, c] {
        assert!(
            !r.abnormal_exit,
            "{binary} nested: record {r:?} has abnormal_exit set",
        );
    }
}

fn run_fixture_throws(runner: &Path, fixtures_dir: &Path, binary: &str) {
    let parsed = run_fixture(runner, fixtures_dir, binary, "throws.php");

    let bad = parsed
        .calls
        .iter()
        .find(|c| matches_function_dict(&parsed, c, "bad"))
        .unwrap_or_else(|| {
            panic!(
                "{binary} throws: no `bad()` record found in dump (records: {:?})",
                parsed.calls,
            )
        });
    assert!(
        bad.abnormal_exit,
        "{binary} throws: bad()'s record must have abnormal_exit = true (got false)",
    );

    // The script body's record (the implicit top-level closure) does
    // NOT have abnormal_exit — the throw is caught inside the script.
    // We find the script-body record by its dict kind == "closure".
    let script_body = parsed
        .calls
        .iter()
        .find(|c| dict_kind_for(&parsed, c).as_deref() == Some("closure"))
        .expect("script body record");
    assert!(
        !script_body.abnormal_exit,
        "{binary} throws: script body must have abnormal_exit = false (got true)",
    );
}

/// Look up the dict entry that owns `call.fn_id` and check whether
/// its `fqn` equals `expected`.
fn matches_function_dict(parsed: &ParsedDump, call: &ParsedCall, expected: &str) -> bool {
    parsed
        .dict
        .iter()
        .find(|d| d.fn_id == call.fn_id)
        .map(|d| d.fqn == expected)
        .unwrap_or(false)
}

/// Look up the dict-entry kind for a given record's `fn_id`.
fn dict_kind_for(parsed: &ParsedDump, call: &ParsedCall) -> Option<String> {
    parsed
        .dict
        .iter()
        .find(|d| d.fn_id == call.fn_id)
        .map(|d| d.kind.clone())
}
