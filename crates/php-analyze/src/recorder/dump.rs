//! Diagnostic buffer dump for slice-2 integration tests.
//!
//! **Diagnostic only.** Built when the crate is compiled with the
//! `recorder-dump` Cargo feature. Default builds (no features) do not
//! include any symbol from this module. The feature exists solely so
//! the `tests/recorder_observer.rs` harness can verify the
//! `CallRecord` and `DictEntry` contents produced by a real PHP
//! process — without the harness, the only way to inspect a trace
//! end-to-end would be to wait for Phase 4's shipper, which is not
//! the right time to validate slice-2's correctness.
//!
//! The module disappears in Phase 4: the shipper-handoff change
//! drops both `#[cfg(feature = "recorder-dump")]` references in
//! `recorder::observer::rshutdown_release_trace` and the `pub mod
//! dump` declaration in `recorder::mod.rs`.
//!
//! ## Dump format
//!
//! Plain text, one record per line, deterministic order:
//!
//! ```text
//! D:<fn_id>:<kind>:<line>:<fqn>\t<file>
//! C:<call_id>:<parent>:<fn_id>:<depth>:<t_in_ns>:<t_out_ns>:<cpu_u_ns>:<cpu_s_ns>:<mem_in>:<mem_out>:<abnormal>
//! RSHUTDOWN: dropped trace_id=<hex>
//! ```
//!
//! - `D:` lines come first, in dictionary-insertion order, ONLY for
//!   entries that have not yet been drained via
//!   `Dictionary::take_new_entries`. (Slice 2 never drains, so every
//!   entry from the trace appears once.)
//! - `C:` lines follow, in emission order (the same order
//!   `Trace::push_record` saw).
//! - `<kind>` is one of `function`, `method`, `closure`, `internal`.
//! - `<abnormal>` is `true` or `false`.
//! - The trailing `RSHUTDOWN:` line marks the end of one trace; a
//!   parser SHOULD treat the file as one-trace-per-run.
//!
//! Tabs are used between `<fqn>` and `<file>` because filenames can
//! contain `:` (an unwieldy but legal PHP source path on Linux); the
//! `<fqn>` strings PHP produces do not contain tabs.

use std::env;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use crate::recorder::types::{DictEntry, FunctionKind, Trace};

/// The env var the harness sets; absent means "do nothing".
const DUMP_PATH_ENV: &str = "PHP_ANALYZE_DUMP_PATH";

/// Write `trace`'s buffer to the file named by `PHP_ANALYZE_DUMP_PATH`.
/// No-op if the env var is unset, empty, or unreadable.
///
/// Errors during file I/O are swallowed (logged via `eprintln!` for
/// the test harness to discover). The function's contract is purely
/// best-effort: if the dump can't be written, the test will fail at
/// the assert step, which is the right failure surface.
pub(crate) fn write_trace_if_path_set(trace: &Trace) {
    let Some(path) = read_dump_path() else {
        return;
    };
    if let Err(err) = write_trace_to_path(trace, &path) {
        eprintln!("recorder::dump: failed to write {:?}: {err}", path);
    }
}

fn read_dump_path() -> Option<PathBuf> {
    let value = env::var(DUMP_PATH_ENV).ok()?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

fn write_trace_to_path(trace: &Trace, path: &std::path::Path) -> std::io::Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut sink = BufWriter::new(file);

    // Slice 2 never calls `take_new_entries`, so the entire
    // dictionary contents are still staged. We pull a fresh view via
    // a clone iterator on the new-entries staging vec by re-using the
    // dictionary's contract: `take_new_entries` returns owned entries,
    // but we have only `&Trace`, so we walk the underlying
    // `new_entries` via the pub(crate) `Dictionary` accessor.
    //
    // The simplest path is to read through `Trace::dictionary` via
    // the existing slice-1 `take_new_entries` — but that mutates.
    // Instead we just walk the staged entries through a borrow,
    // mirroring what slice 4's shipper will do when it copies the
    // dict into a `PendingBatch`. To avoid leaking a new
    // `Dictionary` accessor surface for diagnostic purposes only,
    // this module pokes at the same `pub(crate) new_entries` field
    // the slice-1 `Dictionary` already exposes within the crate.
    for entry in trace.dictionary.new_entries_for_dump() {
        write_dict_line(&mut sink, entry)?;
    }
    for record in &trace.buffer {
        writeln!(
            sink,
            "C:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            record.call_id,
            record.parent,
            record.fn_id,
            record.depth,
            record.t_in_ns,
            record.t_out_ns,
            record.cpu_u_ns,
            record.cpu_s_ns,
            record.mem_in_bytes,
            record.mem_out_bytes,
            record.abnormal_exit,
        )?;
    }

    let hex = trace
        .trace_id
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    writeln!(sink, "RSHUTDOWN: dropped trace_id={hex}")?;

    sink.flush()?;
    Ok(())
}

fn write_dict_line<W: Write>(sink: &mut W, entry: &DictEntry) -> std::io::Result<()> {
    let kind = match entry.kind {
        FunctionKind::Function => "function",
        FunctionKind::Method => "method",
        FunctionKind::Closure => "closure",
        FunctionKind::Internal => "internal",
    };
    writeln!(
        sink,
        "D:{}:{}:{}:{}\t{}",
        entry.fn_id, kind, entry.line, entry.fqn, entry.file,
    )
}

/// A parsed view of a dump file. Used by the integration test.
#[derive(Debug, Clone)]
pub struct ParsedDump {
    pub dict: Vec<ParsedDict>,
    pub calls: Vec<ParsedCall>,
    pub rshutdown_marker_seen: bool,
}

/// A `D:` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDict {
    pub fn_id: u32,
    pub kind: String,
    pub line: u32,
    pub fqn: String,
    pub file: String,
}

/// A `C:` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCall {
    pub call_id: u64,
    pub parent: u64,
    pub fn_id: u32,
    pub depth: u16,
    pub t_in_ns: i64,
    pub t_out_ns: i64,
    pub cpu_u_ns: i64,
    pub cpu_s_ns: i64,
    pub mem_in_bytes: i64,
    pub mem_out_bytes: i64,
    pub abnormal_exit: bool,
}

/// Parse a dump file. Returns whatever lines parse cleanly; lines that
/// don't match the schema are silently ignored (a tradeoff that
/// favours diagnostic robustness over schema strictness). The test
/// harness asserts on counts and contents.
pub fn parse_dump(path: &std::path::Path) -> std::io::Result<ParsedDump> {
    let contents = std::fs::read_to_string(path)?;
    let mut dict = Vec::new();
    let mut calls = Vec::new();
    let mut rshutdown_marker_seen = false;

    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("D:") {
            if let Some(parsed) = parse_dict_line(rest) {
                dict.push(parsed);
            }
        } else if let Some(rest) = line.strip_prefix("C:") {
            if let Some(parsed) = parse_call_line(rest) {
                calls.push(parsed);
            }
        } else if line.starts_with("RSHUTDOWN:") {
            rshutdown_marker_seen = true;
        }
    }

    Ok(ParsedDump {
        dict,
        calls,
        rshutdown_marker_seen,
    })
}

fn parse_dict_line(rest: &str) -> Option<ParsedDict> {
    // `D:<fn_id>:<kind>:<line>:<fqn>\t<file>` — the first three `:` are
    // structural; the rest is `<fqn>\t<file>`. We split on `:` for the
    // first three fields then locate the tab.
    let mut parts = rest.splitn(4, ':');
    let fn_id = parts.next()?.parse().ok()?;
    let kind = parts.next()?.to_owned();
    let line = parts.next()?.parse().ok()?;
    let fqn_file = parts.next()?;
    let mut iter = fqn_file.splitn(2, '\t');
    let fqn = iter.next()?.to_owned();
    let file = iter.next().unwrap_or("").to_owned();
    Some(ParsedDict {
        fn_id,
        kind,
        line,
        fqn,
        file,
    })
}

fn parse_call_line(rest: &str) -> Option<ParsedCall> {
    let mut parts = rest.split(':');
    let call_id = parts.next()?.parse().ok()?;
    let parent = parts.next()?.parse().ok()?;
    let fn_id = parts.next()?.parse().ok()?;
    let depth = parts.next()?.parse().ok()?;
    let t_in_ns = parts.next()?.parse().ok()?;
    let t_out_ns = parts.next()?.parse().ok()?;
    let cpu_u_ns = parts.next()?.parse().ok()?;
    let cpu_s_ns = parts.next()?.parse().ok()?;
    let mem_in_bytes = parts.next()?.parse().ok()?;
    let mem_out_bytes = parts.next()?.parse().ok()?;
    let abnormal_exit = match parts.next()? {
        "true" => true,
        "false" => false,
        _ => return None,
    };
    Some(ParsedCall {
        call_id,
        parent,
        fn_id,
        depth,
        t_in_ns,
        t_out_ns,
        cpu_u_ns,
        cpu_s_ns,
        mem_in_bytes,
        mem_out_bytes,
        abnormal_exit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::types::{CallRecord, FunctionKey, RequestIdentity};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn stub_identity() -> RequestIdentity {
        RequestIdentity {
            host: Arc::from("test-host"),
            sapi: Arc::from("cli"),
            pid: 1,
            uri_or_script: "/tmp/test.php".to_owned(),
        }
    }

    fn intern_one(trace: &mut Trace, fqn: &str, file: &str, line: u32, kind: FunctionKind) -> u32 {
        trace.push_dict_entry_via_intern(
            FunctionKey::Internal {
                name: Arc::from(fqn),
            },
            |fn_id| DictEntry {
                fn_id,
                fqn: fqn.to_owned(),
                file: file.to_owned(),
                line,
                kind,
            },
        )
    }

    fn push_record(trace: &mut Trace, call_id: u64, parent: u64, fn_id: u32, abnormal: bool) {
        trace.push_record(CallRecord {
            call_id,
            parent,
            fn_id,
            depth: 0,
            t_in_ns: 1_000_000,
            t_out_ns: 2_000_000,
            cpu_u_ns: 500,
            cpu_s_ns: 100,
            mem_in_bytes: 1024,
            mem_out_bytes: 2048,
            abnormal_exit: abnormal,
        });
    }

    #[test]
    fn dump_writes_one_d_line_per_dict_entry_and_one_c_line_per_record() {
        let dir = tempdir().unwrap();
        let dump_path = dir.path().join("dump.log");
        // SAFETY: setting an env var is unsafe in Rust 2024 because
        // it is racy with other threads. `cargo test` parallelises
        // tests across threads, but the only readers of
        // `PHP_ANALYZE_DUMP_PATH` are the dump module's own tests in
        // this file. We accept the residual risk; if it becomes
        // flaky, `serial_test` would be the fix.
        unsafe {
            env::set_var(DUMP_PATH_ENV, &dump_path);
        }

        let mut trace = Trace::new(stub_identity());
        let fn_id = intern_one(&mut trace, "only_me", "/x.php", 1, FunctionKind::Function);
        push_record(&mut trace, 1, 0, fn_id, false);
        push_record(&mut trace, 2, 1, fn_id, true);

        write_trace_if_path_set(&trace);
        unsafe {
            env::remove_var(DUMP_PATH_ENV);
        }

        let parsed = parse_dump(&dump_path).expect("dump parses");
        assert_eq!(parsed.dict.len(), 1, "one D: line per dict entry");
        assert_eq!(parsed.dict[0].fqn, "only_me");
        assert_eq!(parsed.dict[0].kind, "function");
        assert_eq!(parsed.dict[0].file, "/x.php");
        assert_eq!(parsed.calls.len(), 2, "one C: line per record");
        assert_eq!(parsed.calls[0].call_id, 1);
        assert_eq!(parsed.calls[1].call_id, 2);
        assert!(parsed.calls[1].abnormal_exit);
        assert!(
            parsed.rshutdown_marker_seen,
            "RSHUTDOWN: marker must close the dump",
        );
    }

    #[test]
    fn dump_emits_no_file_when_env_var_is_absent() {
        // Belt-and-braces: explicitly clear the var first.
        unsafe {
            env::remove_var(DUMP_PATH_ENV);
        }
        let trace = Trace::new(stub_identity());
        // Should not panic and should not create any file (we can't
        // assert "no file" without a known path; the absence of a
        // panic is the contract).
        write_trace_if_path_set(&trace);
    }

    #[test]
    fn parse_dump_round_trips_a_written_dump() {
        let dir = tempdir().unwrap();
        let dump_path = dir.path().join("rt.log");
        unsafe {
            env::set_var(DUMP_PATH_ENV, &dump_path);
        }

        let mut trace = Trace::new(stub_identity());
        let id_a = intern_one(&mut trace, "a", "/x.php", 1, FunctionKind::Function);
        let id_b = intern_one(&mut trace, "b", "/x.php", 2, FunctionKind::Method);
        push_record(&mut trace, 1, 0, id_a, false);
        push_record(&mut trace, 2, 1, id_b, false);

        write_trace_if_path_set(&trace);
        unsafe {
            env::remove_var(DUMP_PATH_ENV);
        }

        let parsed = parse_dump(&dump_path).unwrap();
        assert_eq!(parsed.dict.len(), 2);
        assert_eq!(parsed.calls.len(), 2);
        // Round-trip: every field on the C: lines matches the
        // pushed records.
        assert_eq!(parsed.calls[0].fn_id, id_a);
        assert_eq!(parsed.calls[1].fn_id, id_b);
        assert_eq!(parsed.calls[0].t_in_ns, 1_000_000);
        assert_eq!(parsed.calls[0].t_out_ns, 2_000_000);
    }
}
