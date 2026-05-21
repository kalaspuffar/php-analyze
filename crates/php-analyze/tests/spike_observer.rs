//! Phase-0 zend_observer spike — integration test.
//!
//! Drives `tests/php-spike/run.sh` (which loads the freshly-built
//! cdylib under a real PHP CLI process, runs the three fixtures, and
//! returns the path of the captured spike log) and asserts the
//! coverage table the `observer-integration` spec requires.
//!
//! Skipped when the gating environment variable `PHP_ANALYZE_RUN_SPIKE`
//! is not set to `1`, or when `php` is not on `PATH`. Both skips are
//! announced loudly on stderr so a developer cannot silently miss the
//! spike's evidence. The CI runner can opt in by exporting
//! `PHP_ANALYZE_RUN_SPIKE=1` once PHP is installed in its image.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[test]
fn spike_observer_covers_user_internal_and_throws() {
    if env::var("PHP_ANALYZE_RUN_SPIKE").as_deref() != Ok("1") {
        eprintln!(
            "spike_observer: skipped — set PHP_ANALYZE_RUN_SPIKE=1 to run \
             the Phase-0 zend_observer evidence test"
        );
        return;
    }

    if Command::new("php")
        .arg("-v")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("spike_observer: skipped — `php -v` is not runnable on this host");
        return;
    }

    let script = locate_driver_script();
    let output = Command::new(&script)
        .output()
        .expect("invoke tests/php-spike/run.sh");

    if output.status.code() == Some(77) {
        eprintln!(
            "spike_observer: skipped by driver (exit 77; stderr: {})",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    assert!(
        output.status.success(),
        "spike driver failed (status {:?}); stderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );

    let log_path = PathBuf::from(
        String::from_utf8(output.stdout)
            .expect("driver stdout is utf-8")
            .trim(),
    );

    let log_bytes = fs::read(&log_path).expect("read captured spike log");
    assert!(
        log_bytes.len() < 64 * 1024,
        "spike log unexpectedly large ({} bytes) — fixtures may be runaway",
        log_bytes.len(),
    );
    let log = String::from_utf8(log_bytes).expect("spike log is utf-8");

    let events: Vec<Event> = log.lines().map(Event::parse).collect();
    assert!(
        !events.is_empty(),
        "spike log is empty — observer did not fire. Log path: {}",
        log_path.display(),
    );

    // --- user_calls.php -----------------------------------------------
    assert_pair(
        &events,
        |e| {
            e.kind == EventKind::Entry
                && e.fqn.starts_with("function:")
                && e.fqn.ends_with(":only_me")
        },
        |e| {
            e.kind == EventKind::Exit
                && e.fqn.starts_with("function:")
                && e.fqn.contains(":only_me")
                && e.abnormal == Some(false)
        },
        "user function only_me",
    );
    assert_pair(
        &events,
        |e| e.kind == EventKind::Entry && e.fqn == "method:C::m",
        |e| e.kind == EventKind::Exit && e.fqn == "method:C::m" && e.abnormal == Some(false),
        "method C::m",
    );
    assert_pair(
        &events,
        |e| e.kind == EventKind::Entry && e.fqn.starts_with("closure:"),
        |e| e.kind == EventKind::Exit && e.fqn.starts_with("closure:") && e.abnormal == Some(false),
        "user closure",
    );

    // --- internal_calls.php -------------------------------------------
    //
    // Phase-0 finding: PHP 8.x specialises a small set of internal
    // functions (notably `strlen`, `is_array`, `func_num_args`) into
    // Zend opcodes at compile time when the argument is constant, and
    // the observer machinery does not see opcode-specialised calls.
    // The fixture calls `strlen("hi")`; this asserts it is NOT
    // observed, which is the durable evidence for the C-5 finding in
    // `COMMENTS.md`. Phase 2 must treat the same set of specialised
    // internals as unobservable through `zend_observer` alone.
    let strlen_seen = events
        .iter()
        .any(|e| e.fqn == "internal:strlen" && e.kind == EventKind::Entry);
    assert!(
        !strlen_seen,
        "PHP unexpectedly observed `strlen` — opcode-specialisation finding broken?",
    );

    for internal in ["array_map", "json_encode", "preg_match"] {
        let want_entry = format!("internal:{internal}");
        assert_pair(
            &events,
            |e| e.kind == EventKind::Entry && e.fqn == want_entry,
            |e| e.kind == EventKind::Exit && e.fqn == want_entry && e.abnormal == Some(false),
            &format!("internal {internal}"),
        );
    }

    // --- throws.php ---------------------------------------------------
    assert_pair(
        &events,
        |e| e.kind == EventKind::Entry && e.fqn.starts_with("function:") && e.fqn.ends_with(":bad"),
        |e| {
            e.kind == EventKind::Exit
                && e.fqn.starts_with("function:")
                && e.fqn.contains(":bad")
                && e.abnormal == Some(true)
        },
        "throwing user function bad",
    );

    // Cleanup. The driver leaves the log under /tmp so the test can
    // read it; if everything passed, remove it. On failure, the
    // assertion would have aborted before this line and the log is
    // left behind for the developer to inspect.
    let _ = fs::remove_file(&log_path);
}

/// Locate `tests/php-spike/run.sh` at the workspace root. Cargo
/// sets `CARGO_MANIFEST_DIR` to the path of the crate that owns the
/// test target (here `crates/php-analyze`), so we step up two levels
/// to reach the workspace root, where the PHP fixtures live.
fn locate_driver_script() -> PathBuf {
    let manifest_dir =
        env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo at test time");
    PathBuf::from(manifest_dir)
        .join("../../tests/php-spike/run.sh")
        .canonicalize()
        .expect("workspace tests/php-spike/run.sh exists relative to crate manifest")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    Entry,
    Exit,
}

#[derive(Debug, Clone)]
struct Event {
    kind: EventKind,
    fqn: String,
    /// `Some(_)` only for exit events (the abnormal flag).
    abnormal: Option<bool>,
}

impl Event {
    fn parse(line: &str) -> Self {
        if let Some(rest) = line.strip_prefix("entry: ") {
            Event {
                kind: EventKind::Entry,
                fqn: rest.to_owned(),
                abnormal: None,
            }
        } else if let Some(rest) = line.strip_prefix("exit: ") {
            // Shape: "<fqn> (abnormal=<true|false>)"
            let (fqn, tail) = rest
                .rsplit_once(" (abnormal=")
                .unwrap_or_else(|| panic!("malformed exit line: {line:?}"));
            let abnormal = match tail.trim_end_matches(')') {
                "true" => true,
                "false" => false,
                other => panic!("unrecognised abnormal flag {other:?} in line {line:?}"),
            };
            Event {
                kind: EventKind::Exit,
                fqn: fqn.to_owned(),
                abnormal: Some(abnormal),
            }
        } else {
            panic!("unrecognised spike log line: {line:?}");
        }
    }
}

/// Assert that the events vector contains an entry-matching and an
/// exit-matching event for the same `(category, name)` pair. Both
/// matchers must hit at least once. Reports the full log on failure.
fn assert_pair<E, X>(events: &[Event], entry_matcher: E, exit_matcher: X, label: &str)
where
    E: Fn(&Event) -> bool,
    X: Fn(&Event) -> bool,
{
    let entry_hits = events.iter().filter(|e| entry_matcher(e)).count();
    let exit_hits = events.iter().filter(|e| exit_matcher(e)).count();
    assert!(
        entry_hits >= 1,
        "no entry event matched for {label}; events were: {events:#?}",
    );
    assert!(
        exit_hits >= 1,
        "no exit event matched for {label}; events were: {events:#?}",
    );
}
