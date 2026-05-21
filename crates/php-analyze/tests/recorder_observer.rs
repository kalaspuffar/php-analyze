//! Phase-2 slice-2 + slice-3 recorder integration test.
//!
//! Drives `tests/php-recorder/run.sh` against every available PHP
//! 8.3 / 8.4 binary, runs the slice-2 fixtures (`flat_calls.php`,
//! `nested.php`, `throws.php`) plus the slice-3 fixtures
//! (`deep_recursion.php`, `cap_drops.php`), parses the dump via
//! [`php_analyze::recorder::dump::parse_dump`], and asserts the
//! coverage scenarios the `recorder-call-events` spec requires.
//!
//! Slice-3 additions:
//! - Every slice-2 fixture's dump must contain `DROP: dropped_records=0`
//!   (regression guard: cap/depth-gates must not fire on the default
//!   directive values).
//! - `deep_recursion.php` runs with `php_analyze.max_depth=100`. The
//!   fixture recurses 2000 times so 1900 begins are depth-dropped.
//! - `cap_drops.php` runs with a tight `php_analyze.buffer_cap_bytes`
//!   so the cap-gate fires for some subset of the 200 noop calls. The
//!   harness asserts `noop_records + dropped_records == 200` and both
//!   sides are positive — the exact split depends on the fixture's
//!   absolute path length (the script-body and noop dict entries
//!   contribute path-length-dependent bytes to the budget). See the
//!   `cap_drops` body and C-10 in `COMMENTS.md` for the derivation.
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
        // RO-2: `cargo test` treats any non-zero exit as a test
        // failure (and `std::process::exit` terminates the whole
        // test binary, taking out result reporting). The skip
        // semantics we actually want — "PHP not installed, leave
        // the rest of the suite alone" — is an `eprintln!` plus
        // an early return, which `cargo test` records as a pass.
        // CI's apt-install step is what guarantees PHP is present
        // on the matrix entries that set `PHP_ANALYZE_RUN_RECORDER=1`.
        eprintln!(
            "recorder_observer: skipped (no php8.3 or php8.4 found; tried: {})",
            candidates.join(", "),
        );
        return;
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
    // Slice-3 fixtures:
    run_fixture_deep_recursion(runner, fixtures_dir, binary);
    run_fixture_cap_drops(runner, fixtures_dir, binary);
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
        // No INI overrides for the probe — use the harness defaults.
        .output()
        .unwrap_or_else(|err| panic!("invoke driver probe for {binary}: {err}"));
    // Exit 77 means the driver detected the module-API mismatch and
    // chose to skip; that is not a probe failure.
    output.status.code() != Some(77) && output.status.success()
}

/// Slice-3 regression assertion: every fixture run with default
/// directives must end with a zero drop count. Slice-3 fixtures that
/// deliberately trip the gates pass a different expectation through
/// the `expected_drops` argument.
fn assert_dropped_records(parsed: &ParsedDump, expected: u64, binary: &str, fixture: &str) {
    assert_eq!(
        parsed.dropped_records,
        Some(expected),
        "{binary} {fixture}: dropped_records mismatch — expected {expected}, parser saw {:?}",
        parsed.dropped_records,
    );
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
///
/// `ini_overrides` forwards `--ini KEY=VAL` arguments to `run.sh`,
/// which passes them to PHP as `-d KEY=VAL`. Slice-3 fixtures use
/// this to set `php_analyze.max_depth` / `buffer_cap_bytes` without
/// affecting other fixtures.
fn run_fixture(
    runner: &Path,
    fixtures_dir: &Path,
    binary: &str,
    fixture: &str,
    ini_overrides: &[(&str, String)],
) -> ParsedDump {
    let fixture_path = fixtures_dir.join(fixture);
    let tmp = tempfile::Builder::new()
        .prefix(&format!("recorder-{binary}-{fixture}-"))
        .suffix(".log")
        .tempfile()
        .expect("create tempfile for dump");
    let dump_path = tmp.path().to_owned();

    let mut cmd = Command::new(runner);
    cmd.arg(binary).arg(&fixture_path).arg(&dump_path);
    for (key, value) in ini_overrides {
        cmd.arg("--ini").arg(format!("{key}={value}"));
    }
    let output = cmd
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
    let parsed = run_fixture(runner, fixtures_dir, binary, "flat_calls.php", &[]);
    assert_dropped_records(&parsed, 0, binary, "flat_calls.php");

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
    let parsed = run_fixture(runner, fixtures_dir, binary, "nested.php", &[]);
    assert_dropped_records(&parsed, 0, binary, "nested.php");

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
    let parsed = run_fixture(runner, fixtures_dir, binary, "throws.php", &[]);
    assert_dropped_records(&parsed, 0, binary, "throws.php");

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

// --- Slice-3 fixtures -------------------------------------------------

/// Drive `deep_recursion.php` with `php_analyze.max_depth = 100`. The
/// fixture recurses 2000 times, so the recorder accepts the first 100
/// begins and depth-drops the remaining 1900. The script-body's
/// closure is observed in addition; its frame is accepted (depth 1)
/// and recorded.
fn run_fixture_deep_recursion(runner: &Path, fixtures_dir: &Path, binary: &str) {
    let parsed = run_fixture(
        runner,
        fixtures_dir,
        binary,
        "deep_recursion.php",
        &[("php_analyze.max_depth", "100".to_owned())],
    );

    let recurse_records: Vec<&ParsedCall> = parsed
        .calls
        .iter()
        .filter(|c| matches_function_dict(&parsed, c, "recurse"))
        .collect();

    // Budget for the depth gate:
    // - The script-body closure is observed at `virtual_depth = 1`
    //   and accepted (1 record).
    // - `recurse(2000)` enters at virtual_depth = 2 (accept), and
    //   the chain continues through `recurse(1903)` at depth 99 and
    //   `recurse(1902)` at depth 100 — all accepted. So the recurse
    //   accepts span depths 2..=100 inclusive = 99 records.
    // - `recurse(1901)` is at virtual_depth = 101 ⇒ depth-drop. The
    //   remaining 1901 recursive calls (down to `recurse(0)`) also
    //   drop. Total drops: 1902.
    // - `recurse(N)` is called with N = 2000, 1999, …, 0 inclusive ⇒
    //   2001 begins total. 99 accepted + 1902 dropped = 2001. ✓
    assert_eq!(
        recurse_records.len(),
        99,
        "{binary} deep_recursion: expected exactly 99 recurse records \
         (max_depth = 100 minus the script-body's depth-1 frame), got {} \
         (total records: {}, dump dropped_records: {:?})",
        recurse_records.len(),
        parsed.calls.len(),
        parsed.dropped_records,
    );

    // The recurse function is interned exactly once.
    let recurse_dict: Vec<&ParsedDict> =
        parsed.dict.iter().filter(|d| d.fqn == "recurse").collect();
    assert_eq!(
        recurse_dict.len(),
        1,
        "{binary} deep_recursion: recurse appears in dict {} times (expected 1)",
        recurse_dict.len(),
    );

    assert_dropped_records(&parsed, 1902, binary, "deep_recursion.php");
}

/// Drive `cap_drops.php` with a tight `php_analyze.buffer_cap_bytes`.
/// The fixture calls `noop()` 200 times. The cap is sized at runtime
/// from the fixture's absolute path length (the script-body and
/// `noop` dict entries contribute path-length bytes; a hard-coded cap
/// would behave differently across CI hosts).
///
/// ## Assertions
///
/// - `noop_records + dropped_records == 200` (every call accounted
///   for, either as an emitted record or as a drop).
/// - `noop_records > 0` (cap permits at least one accept; otherwise
///   the gate is too tight and the test is meaningless).
/// - `dropped_records > 0` (cap rejects at least one begin; the gate
///   is exercised).
///
/// C-10 in `COMMENTS.md` records why this test does not assert
/// "exactly K" as the spec's idealisation suggested: the script-body's
/// dict entry pulls in the absolute fixture path, whose length is not
/// predictable across CI hosts. The slice-3 spec's intent — "the
/// cap-gate fires and the counter is accurate" — is fully exercised
/// by the three assertions above.
fn run_fixture_cap_drops(runner: &Path, fixtures_dir: &Path, binary: &str) {
    // Cap = 1024 bytes. Per `cap_drops.php`'s budgeting in
    // `COMMENTS.md` C-10, this admits 4..10 noop accepts on typical
    // CI path lengths (60–200 chars) and rejects the rest.
    //
    // Config range-clamping (`config.rs::RANGE_BUFFER_CAP_BYTES`)
    // enforces `buffer_cap_bytes >= flush_bytes`, and `flush_bytes`'s
    // default is 1 MiB. We override both so the cap actually lands
    // at 1024 rather than getting clamped up. The 1024 min on
    // `flush_bytes` matches its own directive range.
    let cap = 1024_usize;
    let flush_bytes = 1024_usize;
    let parsed = run_fixture(
        runner,
        fixtures_dir,
        binary,
        "cap_drops.php",
        &[
            ("php_analyze.flush_bytes", flush_bytes.to_string()),
            ("php_analyze.buffer_cap_bytes", cap.to_string()),
        ],
    );

    let noop_records: Vec<&ParsedCall> = parsed
        .calls
        .iter()
        .filter(|c| matches_function_dict(&parsed, c, "noop"))
        .collect();
    let accepts = u64::try_from(noop_records.len()).expect("noop_records fits in u64");
    let drops = parsed
        .dropped_records
        .expect("slice-3 DROP: line must be present; missing line indicates the writer regressed");

    assert_eq!(
        accepts + drops,
        200,
        "{binary} cap_drops: noop_accepts ({accepts}) + drops ({drops}) must equal 200 \
         (cap was {cap}, total records {}, dict {})",
        parsed.calls.len(),
        parsed.dict.len(),
    );
    assert!(
        accepts > 0,
        "{binary} cap_drops: expected at least one noop record under cap = {cap}; \
         the gate is too tight for this host's path length to exercise the test",
    );
    assert!(
        drops > 0,
        "{binary} cap_drops: expected at least one drop under cap = {cap}; \
         the gate did not fire (path length too short?)",
    );

    // The noop function appears in the dict exactly once (its first
    // begin missed; subsequent begins either dropped or hit).
    let noop_dict: Vec<&ParsedDict> = parsed.dict.iter().filter(|d| d.fqn == "noop").collect();
    assert_eq!(
        noop_dict.len(),
        1,
        "{binary} cap_drops: noop appears in dict {} times (expected 1)",
        noop_dict.len(),
    );
}
