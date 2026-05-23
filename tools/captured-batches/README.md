# `tools/captured-batches/` — reference MessagePack batches

This directory contains real `wire::Batch` byte sequences
captured from `php-analyze` runs against the canonical PHP
workloads. The downstream visualizer team (`../php-tree-visualizer`
and any other consumer) uses these as **parser test
fixtures**: feed them into their MessagePack decoder, walk
the decoded structure, assert on the shape.

## Contract

The captured `batch-NNNN.msgpack` files conform to the wire
format described in `SPECIFICATION.md` §4.2 (schema v1, media
type `application/vnd.php-analyze.v1+msgpack`). Each file is
**one HTTP POST body** the recorder emitted during a single
PHP run — byte-identical to what hit the wire, with no
`rmp_serde` round-trip on the capture path.

The reference collector implementation lives in
`crates/stub-ingest/`. Reading that source documents what a
minimal compliant ingest server looks like; the captured
files document what a parser will encounter on the wire.

## Directory structure

| Subdirectory | Source workload | Call shape |
| --- | --- | --- |
| [`flat_calls/`](./flat_calls/) | [`tests/php-bench/flat_calls.php`](../../tests/php-bench/flat_calls.php) | One user function (`noop`), called 10⁶ times in a tight loop. Tests the hot-path under maximal flush pressure. |
| [`json_batch/`](./json_batch/) | [`tests/php-bench/json_batch.php`](../../tests/php-bench/json_batch.php) | 100 iterations of `json_encode` + `json_decode` + `array_map`. Tests high-fan-out internal calls and short user functions interleaved. |
| [`recursive_walk/`](./recursive_walk/) | [`tests/php-bench/recursive_walk.php`](../../tests/php-bench/recursive_walk.php) | 40 iterations of a balanced-tree build (`make_tree`) + walk (`walk_tree`) at depth 10. Tests recursive call dispatch and frame-stack growth. |

Each subdirectory contains a `README.md` describing that
workload's call shape in more detail plus its own
`batch-NNNN.msgpack` files (zero-padded 4-digit monotonic
counter starting at `0001`, in arrival order).

## Samples, not goldens

These files **change run-to-run**. Sources of variation:

- `meta.start_time` — wall-clock nanoseconds at trace
  allocation (`CLOCK_REALTIME`).
- `meta.pid`, `meta.host` — per-process metadata.
- `meta.trace_id` — currently `[0; 16]` (a documented
  placeholder per `recorder::types::Trace::new`; UUID v7
  generation is a deferred Phase-4 follow-up). When that
  lands, the trace ID rolls per request.
- Per-call `t_in` / `t_out` — `CLOCK_MONOTONIC` nanoseconds
  from process start; values shift per run.

**Do not write `diff`-against-bytes tests** against these
files. The intended pattern is:

1. Decode via your MessagePack library.
2. Assert on structure: `meta.schema_version == 1`, every
   `call.fn` resolves to a dict entry, etc.
3. Spot-check the dict's `fqn` values for each workload's
   known function names (e.g. `recursive_walk`'s dict
   includes `make_tree` and `walk_tree`).

The per-workload `README.md` carries the recorder commit SHA
that produced the committed snapshot. Re-run
`tools/capture-fixtures.sh` from the repo root to refresh.

## Regenerating

```sh
./tools/capture-fixtures.sh
```

Builds the cdylib + stub-ingest, then for each workload
spawns `stub-ingest --capture-dir tools/captured-batches/<workload>/`
and runs the corresponding fixture under `php-analyze`. The
existing `batch-*.msgpack` files in each subdirectory are
deleted before regeneration (per `capture-reference-batches`'
design D-2 / Q-2), so a recapture that produces fewer batches
doesn't leak stale files.

The script is operator-driven — `cargo test` and CI do not
invoke it. Regenerate when:

- The recorder's behaviour changes in a way that should
  reshape the captures (a hot-path tuning, a directive
  default change, etc.).
- The wire format schema bumps (then expect a new `v2/`
  subdirectory; the existing `v1/` files stay in place as
  historical reference).
- You want to refresh the committed snapshot for any reason.

## Where this fits in the MVP

This is the fourth and final MVP-closing item named in
`COMMENTS.md` §6.3. After this directory lands, the handoff
to `../php-tree-visualizer` is complete:

- ✓ Spec documents the format (`SPECIFICATION.md` §4.2).
- ✓ Reference collector implementation lives in
  `crates/stub-ingest/`.
- ✓ Concrete wire bytes for parser tests live here.
