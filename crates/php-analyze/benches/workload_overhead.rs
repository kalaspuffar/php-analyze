//! PHP workload-overhead benchmark binding NFR-PERF-1.
//!
//! Times each of three self-contained PHP workloads
//! (`tests/php-bench/*.php`) unprofiled vs. profiled, computes
//! the geo-mean ratio across workloads, and asserts the
//! `≤ 2.0×` budget mandated by `SPECIFICATION.md` §3 KPI / §8.1
//! NFR-PERF-1 / OBJ-2. Resolves OQ-7.
//!
//! ## Skip conditions
//!
//! Exits `0` with a loud `eprintln!` skip message when **any** of:
//!
//! - `PHP_ANALYZE_RUN_BENCH` env var is not set to `1` (default
//!   `cargo bench` paths skip this bench so they don't require
//!   PHP installed).
//! - Neither `php8.3` nor `php8.4` is on `PATH`.
//!
//! Same skip semantic as `tests/shipper_round_trip.rs`'s
//! `PHP_ANALYZE_RUN_SHIPPER` gate.
//!
//! ## Pass-criterion escape hatch
//!
//! `PHP_ANALYZE_BENCH_NO_ASSERT=1` runs the full measurement +
//! prints the markdown summary but does NOT exit non-zero on a
//! `geo-mean > 2.0` value. Used by developers iterating on the
//! hot path locally.
//!
//! Run:
//!
//! ```sh
//! PHP_ANALYZE_RUN_BENCH=1 cargo bench -p php-analyze --bench workload_overhead
//! ```

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// One of the three canonical workloads resolving OQ-7.
struct Workload {
    /// Short identifier used in the report's leftmost column and
    /// as the per-bench filter key.
    name: &'static str,
    /// Filename under `tests/php-bench/`.
    fixture: &'static str,
}

const WORKLOADS: &[Workload] = &[
    Workload {
        name: "flat_calls",
        fixture: "flat_calls.php",
    },
    Workload {
        name: "json_batch",
        fixture: "json_batch.php",
    },
    Workload {
        name: "recursive_walk",
        fixture: "recursive_walk.php",
    },
];

/// Samples per (workload, mode). M=5 is enough for a stable
/// median against PHP-subprocess variance dominated by startup
/// latency. Per-bench runtime: `3 workloads × 2 modes × 5
/// samples × ~1s/sample ≈ 30s`.
const SAMPLES: usize = 5;

/// NFR-PERF-1 pass criterion: geo-mean ≤ 2.0×.
const GEOMEAN_BUDGET: f64 = 2.0;

fn main() {
    if env::var("PHP_ANALYZE_RUN_BENCH").as_deref() != Ok("1") {
        eprintln!(
            "workload_overhead: skipped (set PHP_ANALYZE_RUN_BENCH=1 to run \
             the NFR-PERF-1 / OQ-7 workload-overhead bench against installed PHP)"
        );
        return;
    }

    let candidates = ["php8.4", "php8.3"];
    let php_binary = candidates.iter().copied().find(|name| {
        Command::new(name)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    });
    let Some(php_binary) = php_binary else {
        eprintln!(
            "workload_overhead: skipped (no PHP binary on PATH; tried: {})",
            candidates.join(", "),
        );
        return;
    };
    eprintln!("workload_overhead: using {php_binary}");

    let cdylib = build_release_cdylib();
    eprintln!("workload_overhead: cdylib at {}", cdylib.display());

    let tmpdir = tempfile::tempdir().expect("tempdir for profiled ini");
    let profiled_ini = make_profiled_ini(&cdylib, tmpdir.path());

    // Per-workload (unprofiled_median, profiled_median, ratio).
    let mut report_rows: Vec<(String, Duration, Duration, f64)> = Vec::new();

    for workload in WORKLOADS {
        let fixture = locate_bench_fixture(workload.fixture);
        eprintln!(
            "workload_overhead: timing {} ({} samples × 2 modes)",
            workload.name, SAMPLES,
        );

        let unprofiled: Vec<Duration> = (0..SAMPLES)
            .map(|_| time_one_run(php_binary, None, &fixture))
            .collect();
        let profiled: Vec<Duration> = (0..SAMPLES)
            .map(|_| time_one_run(php_binary, Some(&profiled_ini), &fixture))
            .collect();

        let unprofiled_median = median(&unprofiled);
        let profiled_median = median(&profiled);
        let ratio = profiled_median.as_secs_f64() / unprofiled_median.as_secs_f64();
        report_rows.push((
            workload.name.to_owned(),
            unprofiled_median,
            profiled_median,
            ratio,
        ));
    }

    // Geo-mean across the three ratios. Log-arithmetic form for
    // numerical stability.
    let ratios: Vec<f64> = report_rows.iter().map(|(_, _, _, r)| *r).collect();
    let geomean = (ratios.iter().map(|r| r.ln()).sum::<f64>() / ratios.len() as f64).exp();

    print_markdown_report(&report_rows, geomean);

    if env::var("PHP_ANALYZE_BENCH_NO_ASSERT").as_deref() == Ok("1") {
        eprintln!(
            "workload_overhead: assertion skipped (PHP_ANALYZE_BENCH_NO_ASSERT=1). \
             Observed geo-mean: {geomean:.2}× (budget: {GEOMEAN_BUDGET:.2}×)",
        );
        return;
    }

    assert!(
        geomean <= GEOMEAN_BUDGET,
        "NFR-PERF-1 violated: geo-mean wall-time overhead {geomean:.2}× exceeds the \
         {GEOMEAN_BUDGET:.2}× budget. See the markdown table above for per-workload \
         ratios. Set PHP_ANALYZE_BENCH_NO_ASSERT=1 to run measurements without the \
         assertion while iterating on the hot path."
    );
    eprintln!(
        "workload_overhead: NFR-PERF-1 satisfied (geo-mean {geomean:.2}× <= {GEOMEAN_BUDGET:.2}×)",
    );
}

/// Build (or reuse) the production-cdylib via `cargo build
/// --release -p php-analyze`. Returns the absolute path to the
/// resulting `libphp_analyze.so`.
fn build_release_cdylib() -> PathBuf {
    let out = Command::new(env!("CARGO"))
        .args(["build", "--release", "-p", "php-analyze"])
        .output()
        .expect("cargo build --release runnable from the bench");
    assert!(
        out.status.success(),
        "cargo build --release -p php-analyze failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    target_dir().join("release").join("libphp_analyze.so")
}

/// Compute the `target/` directory. Mirrors
/// `shipper_round_trip.rs`'s heuristic: honour `CARGO_TARGET_DIR`
/// if set, else fall back to the repo-root `target/`.
fn target_dir() -> PathBuf {
    if let Ok(dir) = env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .map(|p| p.join("target"))
        .expect("crate dir → crates → repo root")
}

/// Locate a fixture under `tests/php-bench/`. Asserts the file
/// exists with a clear panic message if not.
fn locate_bench_fixture(name: &str) -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("crate dir → crates → repo root");
    let path = repo_root.join("tests").join("php-bench").join(name);
    assert!(
        path.exists(),
        "fixture {name} not found at {}",
        path.display(),
    );
    path
}

/// Write a per-run `php.ini` to `<tmpdir>/profiled.ini`. Loads
/// the cdylib, enables the extension, and configures a synthetic
/// **unreachable** `server_url` (port 1 is reserved) so no batch
/// actually ships. `shutdown_grace_ms = 200` + `http_timeout_ms =
/// 200` keep MSHUTDOWN bounded — relies on the C-18 fix
/// (`shipper-deadline-mid-retry`) for the per-iteration deadline
/// re-read that makes the bound hold even on an unreachable
/// upstream.
fn make_profiled_ini(cdylib: &Path, tmpdir: &Path) -> PathBuf {
    let ini_path = tmpdir.join("profiled.ini");
    // Optional CPU-snapshot mode override
    // (recorder-cpu-snapshot-cadence). `PHP_ANALYZE_BENCH_CPU_MODE`
    // accepts the same vocabulary as the directive itself:
    // `per-call` (default; current spec) or `off` (skip
    // `getrusage`). Anything unrecognised is passed verbatim; the
    // extension's parser will log one warning and fall back. Used
    // by operators to quantify the `off` mode's saving on their
    // own host.
    let cpu_mode_line = match std::env::var("PHP_ANALYZE_BENCH_CPU_MODE").ok().as_deref() {
        Some(value) if !value.is_empty() => {
            format!("php_analyze.cpu_snapshot_mode = \"{value}\"\n")
        }
        _ => String::new(),
    };
    let ini_body = format!(
        concat!(
            "extension={cdylib}\n",
            "php_analyze.enabled           = 1\n",
            "php_analyze.server_url        = \"http://127.0.0.1:1/sink\"\n",
            "php_analyze.auth_token        = \"bench-token\"\n",
            "php_analyze.spike_observer    = 0\n",
            "php_analyze.shutdown_grace_ms = 200\n",
            "php_analyze.http_timeout_ms   = 200\n",
            "{cpu_mode_line}"
        ),
        cdylib = cdylib.display(),
        cpu_mode_line = cpu_mode_line,
    );
    std::fs::write(&ini_path, ini_body).expect("write profiled.ini");
    ini_path
}

/// Time one PHP invocation. `ini_arg = None` means `php -n`
/// (unprofiled — no extensions, no ini). `Some(path)` means
/// `php -n -c <path>` (profiled — explicit per-test ini).
fn time_one_run(php_binary: &str, ini_arg: Option<&Path>, fixture: &Path) -> Duration {
    let mut cmd = Command::new(php_binary);
    cmd.arg("-n");
    if let Some(ini) = ini_arg {
        cmd.arg("-c").arg(ini);
    }
    cmd.arg(fixture);

    let start = Instant::now();
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("invoke {php_binary} {fixture:?}: {e}"));
    let elapsed = start.elapsed();
    assert!(
        output.status.success(),
        "{php_binary} {fixture:?} (ini={ini_arg:?}) exited non-zero (status {:?}); stderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    elapsed
}

/// Median of a `Vec<Duration>`. For M=5 there's an exact middle
/// element after sorting; this function assumes non-empty input
/// and an odd sample count, which `SAMPLES = 5` satisfies.
fn median(samples: &[Duration]) -> Duration {
    assert!(!samples.is_empty(), "median() requires non-empty input");
    let mut sorted = samples.to_vec();
    sorted.sort();
    sorted[sorted.len() / 2]
}

/// Print the markdown report to stdout. Format:
///
/// ```text
/// | Workload | Unprofiled | Profiled | Ratio |
/// | --- | --- | --- | --- |
/// | flat_calls | 432ms | 781ms | 1.81× |
/// | json_batch | 198ms | 290ms | 1.46× |
/// | recursive_walk | 405ms | 689ms | 1.70× |
/// | **geo-mean** | | | **1.66×** |
/// ```
fn print_markdown_report(rows: &[(String, Duration, Duration, f64)], geomean: f64) {
    println!();
    println!("| Workload | Unprofiled (median) | Profiled (median) | Ratio |");
    println!("| --- | --- | --- | --- |");
    for (name, unprofiled, profiled, ratio) in rows {
        println!(
            "| `{name}` | {} | {} | {ratio:.2}× |",
            format_duration(*unprofiled),
            format_duration(*profiled),
        );
    }
    println!("| **geo-mean** | | | **{geomean:.2}×** |");
    println!();
}

/// Format a `Duration` as `XXXms` (rounded to nearest ms). PHP
/// subprocess timings are typically 100ms-2s; millisecond
/// resolution is the natural granularity.
fn format_duration(d: Duration) -> String {
    format!("{}ms", d.as_millis())
}
