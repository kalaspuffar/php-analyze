# `php-analyze` benchmarks

`criterion`-based benchmarks measuring the recorder's hot-path
cost without going through PHP. The recorder kernel
(`begin_with_snapshots` / `end_with_snapshots`) is pure Rust by
design (`SPECIFICATION.md` §3.2 / §7.2); the benches link
against it directly via the `bench-seam` feature so anyone can
run them on any host without `php-config` or a PHP runtime.

## Run

The `bench-seam` feature is **required** — without it, each bench
file's `compile_error!` block fires with a clear message:

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

## Benches

| Bench | Workload | What it measures |
| --- | --- | --- |
| `recorder_hot_path` | One `begin_with_snapshots + end_with_snapshots` pair per criterion iteration against a pre-allocated `Trace`. | Per-call cost in nanoseconds. The trace persists across iterations (huge limits keep the flush/cap gates quiet), so the dictionary is warm and the measurement reflects steady-state hit-path cost. |
| `recorder_workload` | `10_000` tight-loop calls per criterion iteration against a fresh `Trace`. Simulates `flat_calls.php` from `SPECIFICATION.md` §9.2. | Workload-shape time: one cold dict miss + 9_999 hits per iteration. Closer to a real request's hit ratio than `recorder_hot_path`'s long-warm dictionary. |

## What this is NOT

- **A pass criterion.** This change (`bench-criterion-skeleton`)
  stands up the measurement infrastructure but doesn't pin a
  per-call ceiling or a profiled-vs-unprofiled geo-mean budget.
  Those land in the follow-up `bench-canonical-workloads` change
  (which also resolves OQ-7 — the canonical workload set —
  jointly with the operator).
- **A zero-alloc audit.** AC-RC-5 (zero heap allocations on the
  hot path) is binding-evidence work for the
  `recorder-zero-alloc-audit` follow-up, which adds an allocator-
  counting harness over the same `bench_seam` surface.
- **A PHP-driven end-to-end comparison.** The whole point of the
  "hot path is pure Rust" architecture is that the recorder
  kernel can be measured without PHP. PHP-level benchmarking
  layers on top via `bench-canonical-workloads`.

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
