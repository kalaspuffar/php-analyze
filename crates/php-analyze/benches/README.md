# `php-analyze` benchmarks

Two families of benchmarks:

- **Recorder hot-path microbenches** (`recorder_hot_path`,
  `recorder_workload`) — `criterion`-based, pure-Rust, no PHP
  required. Measure the recorder kernel's per-call cost.
- **Workload-overhead bench** (`workload_overhead`) — PHP-driven,
  no criterion. Times the production cdylib against three
  canonical PHP workloads, computes the geo-mean ratio across
  workloads, and **asserts NFR-PERF-1's ≤ 2.0× budget**.

## Run — recorder hot-path microbenches

The `bench-seam` feature is **required** — without it, each
hot-path bench file's `compile_error!` block fires with a clear
message:

```sh
cargo bench -p php-analyze --features bench-seam
```

Per-bench filter form:

```sh
cargo bench -p php-analyze --features bench-seam --bench recorder_hot_path
cargo bench -p php-analyze --features bench-seam --bench recorder_workload
```

Compile-only (CI smoke gate, if added later):

```sh
cargo bench -p php-analyze --features bench-seam --no-run
```

## Run — workload-overhead bench (PHP-driven)

The `PHP_ANALYZE_RUN_BENCH=1` env var is **required** — without
it, the bench exits 0 with a one-line skip message so default
`cargo bench` paths don't require PHP installed:

```sh
PHP_ANALYZE_RUN_BENCH=1 cargo bench -p php-analyze --bench workload_overhead
```

Disarm the assertion (for iterating on the hot path locally
without the budget blocking progress):

```sh
PHP_ANALYZE_RUN_BENCH=1 PHP_ANALYZE_BENCH_NO_ASSERT=1 \
    cargo bench -p php-analyze --bench workload_overhead
```

The bench prints a markdown table to stdout summarising
unprofiled / profiled medians and the per-workload ratio + the
geo-mean.

## Benches

| Bench | Workload | What it measures |
| --- | --- | --- |
| `recorder_hot_path` | One `begin_with_snapshots + end_with_snapshots` pair per criterion iteration against a pre-allocated `Trace`. | Per-call cost in nanoseconds. The trace persists across iterations (huge limits keep the flush/cap gates quiet), so the dictionary is warm and the measurement reflects steady-state hit-path cost. |
| `recorder_workload` | `10_000` tight-loop calls per criterion iteration against a fresh `Trace`. Simulates `flat_calls.php` from `SPECIFICATION.md` §9.2. | Workload-shape time: one cold dict miss + 9_999 hits per iteration. Closer to a real request's hit ratio than `recorder_hot_path`'s long-warm dictionary. |
| `workload_overhead` | Three canonical PHP workloads under `tests/php-bench/` (`flat_calls.php` 10⁶ user calls; `json_batch.php` 10⁵ JSON rows; `recursive_walk.php` 1024-node tree × 40 passes), each timed 5× unprofiled + 5× profiled. Resolves OQ-7. | Geo-mean wall-time ratio (profiled / unprofiled) across the three workloads. Asserts the `≤ 2.0×` NFR-PERF-1 budget unless `PHP_ANALYZE_BENCH_NO_ASSERT=1` is set. |

## `cpu_snapshot_mode` operator lever (`recorder-cpu-snapshot-cadence`)

The `php_analyze.cpu_snapshot_mode` directive lets operators trade
per-call CPU attribution for ~1000 ns of saved syscall cost per PHP
function. Two modes:

- `per-call` (default) — spec-current. Every begin/end snapshot calls
  `getrusage(RUSAGE_THREAD)`.
- `off` — skip the `getrusage` call entirely; `cpu_u_ns` and `cpu_s_ns`
  are emitted as `0` in every `CallRecord`. Saves ~1000 ns/call on
  hosts without vDSO for that syscall (most Linux x86_64).

The `workload_overhead` bench can toggle the mode via the
`PHP_ANALYZE_BENCH_CPU_MODE` env var:

```sh
# Default mode (per-call):
PHP_ANALYZE_RUN_BENCH=1 cargo bench -p php-analyze --bench workload_overhead

# Performance-optimised mode (off):
PHP_ANALYZE_RUN_BENCH=1 PHP_ANALYZE_BENCH_CPU_MODE=off \
    cargo bench -p php-analyze --bench workload_overhead
```

Observed numbers on the reference dev host (5-sample medians):

| Workload | `per-call` | `off` | Improvement |
| --- | --- | --- | --- |
| `flat_calls` | 33.09× | **12.38×** | -63% |
| `json_batch` | 2.30× | **1.87×** (under budget) | -19% |
| `recursive_walk` | 12.65× | **5.77×** | -54% |
| **geo-mean** | **9.88×** | **5.11×** | **-48%** |

**Recommendation for high-volume pools**: `off` is a credible
operator lever — roughly half the recorder's overhead on this host
disappears. The trade-off is **all `CallRecord::cpu_u_ns` /
`cpu_s_ns` read as 0**; downstream consumers that need per-call CPU
attribution must stay on `per-call`. The 2.0× NFR-PERF-1 budget is
still not reached under either mode; closing the residual gap
requires re-evaluating whether the canonical workloads
(`flat_calls.php` in particular) represent realistic PHP traffic —
see `COMMENTS.md` C-19's `bench-canonical-workloads-revisit`
follow-up.

## Most recent observed numbers (after `recorder-hot-path-tuning`)

Reference host (developer workstation, PHP 8.4.21, default
sample-of-5 medians):

| Bench | Result | Notes |
| --- | --- | --- |
| `recorder_workload` (10⁴ calls) | **≈ 1.35 ms** (~135 ns/call) | Down from 1.85 ms (~185 ns/call) before the change; `-23%`, p < 0.05. The kill-Arc-clone + single-traversal `Dictionary::intern_ref` is the load-bearing optimisation. |
| `recorder_hot_path` (1 call) | 4.6–6.3 µs (high variance) | Single-call shape is dominated by syscall + bench-harness noise; `recorder_workload` is the reliable in-process number. |
| `workload_overhead` flat_calls | **29.20×** | Was 30.56× baseline. Per-call unprofiled is ~60 ns; the syscall floor (~1000 ns per begin/end pair from two `getrusage` calls on this kernel) puts 2.0× structurally out of reach. See `COMMENTS.md` §3 C-19. |
| `workload_overhead` json_batch | **1.94×** | Was 2.42× baseline. Individually under the 2.0× budget — heavyweight workloads where PHP-side cost dwarfs the recorder are within reach. |
| `workload_overhead` recursive_walk | **13.19×** | Was 14.67× baseline. Same syscall-floor story as flat_calls. |
| `workload_overhead` **geo-mean** | **9.08×** | Was 10.28× baseline (`-12%`). Does **not** satisfy NFR-PERF-1's ≤ 2.0× budget — see `COMMENTS.md` §3 C-19 for the gap analysis and proposed follow-ups (the CPU-snapshot amortisation under R-11 is the most realistic path). |

## What this is NOT

- **A zero-alloc audit.** AC-RC-5 (zero heap allocations on the
  hot path) is binding-evidence work for the
  `recorder-zero-alloc-audit` follow-up, which adds an allocator-
  counting harness over the same `bench_seam` surface.
- **An Xdebug comparison.** REQ §15.1 #3 (≥ 99.5% call coverage
  + ±5% per-call timing vs. Xdebug) is a separate measurement;
  belongs to a future `bench-xdebug-comparison`.
- **A criterion-reported number for the workload bench.** The
  workload bench uses raw `Instant::now()` + `Duration` because
  PHP-subprocess variance is dominated by startup latency, not
  measurement precision. Criterion's nanosecond-precision
  machinery is overkill at this scale — see
  `openspec/changes/archive/<date>-bench-canonical-workloads/design.md`
  D-6.

## Why the `bench-seam` feature exists

The recorder's hot-path entry points and value types
(`begin_with_snapshots`, `end_with_snapshots`, `categorise`,
`Categorised`, `EntrySnapshots`, `ExitSnapshots`, `RawCallSite`)
are conceptually internal — no external Rust consumer needs them
in steady-state operation. They're `pub` in the source so the
bench files (which live outside the `php-analyze` crate per
Cargo's bench layout) can reach them via the
`php_analyze::bench_seam` re-export module.

The `bench-seam` feature gates the *re-export module*, not the
underlying items: when the feature is off, the items are still
`pub` but the `bench_seam` module isn't compiled, so the
"discoverable bench-only surface" is hidden from anyone not
opting in. Production cdylib builds (default features) compile
the recorder unchanged.

See `openspec/changes/archive/<date>-bench-criterion-skeleton/design.md`
D-1 for the visibility-rules trade-off (Rust's `pub use` cannot
widen `pub(crate)` to `pub`, which forced the items to be
permanently `pub`; the bench-seam concept survives in the
re-export grouping rather than in the visibility gate).
