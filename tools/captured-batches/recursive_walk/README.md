# `recursive_walk/` — recursive call dispatch + frame-stack growth

Captured from [`tests/php-bench/recursive_walk.php`](../../../tests/php-bench/recursive_walk.php).

## Workload

40 iterations of:

1. `make_tree(10, 1)` — build a balanced binary tree of `2^10
   = 1024` leaves via recursive descent.
2. `walk_tree($tree)` — recursively sum every node's `key`.

The recursive call pattern stresses frame-stack growth +
cleanup. Real PHP frameworks (Symfony's container resolution,
WordPress's `apply_filters` cascade, Laravel's middleware
pipeline) have the same shape: chains 5–15 levels deep
through user-defined code. The captured records' `call.depth`
values directly reflect that stack depth.

## What's in each batch

Same shape as the other workloads. The dict for batch 1
carries **three** entries:

| `fqn` | `kind` | Notes |
| --- | --- | --- |
| `closure:<file>:1` | `2` (closure) | The script body. |
| `make_tree` | `0` (function) | Recursive tree builder. |
| `walk_tree` | `0` (function) | Recursive tree summer. |

Batches 2 and 3 carry an empty `dict`. Each of the three
committed batches packs `flush_records = 10000` records.

## Expected call-set

A full run produces ~25 batches × 10,000 records ≈ 245,000
calls — distributed as roughly 1/3 `make_tree` + 2/3
`walk_tree` (the walk visits every node including null
children, doubling its sample size relative to the build).

Of the committed 3 batches × 10,000 records = 30,000 sample
records, plus the script-body closure's single record in
batch 1.

## Committed sample

- Recorder commit SHA at capture time: `c7924292ca4e`
- Batches captured (full run): 25
- Batches kept for git: 3 (the first three)
- File sizes: ~1 MB each

Regenerate via `tools/capture-fixtures.sh` from the repo
root. See [`../README.md`](../README.md) for the
samples-not-goldens disclaimer.

## Parser-side checks the visualizer team can run

- Batch 1's `dict` contains exactly 3 entries; two have
  `kind == 0` (user function) and one has `kind == 2`
  (closure).
- `call.depth` values for `make_tree` records range up to
  `~11` (script body at 0 → first `make_tree(10, ...)` at 1
  → nine recursive levels). The depth distribution is the
  primary signal: frequencies should taper as depth grows.
- The `(call_id, parent)` graph reconstructs the recursive
  call tree. The visualizer's tree-rendering tests can use
  this fixture to verify their parent-pointer traversal.
- `call.cpu_u` / `call.cpu_s` values are `0` on a build of
  the recorder running under `php_analyze.cpu_snapshot_mode
  = per-call` (the default — but those values can be near
  the clock-resolution floor for short recursive calls).
  See `COMMENTS.md` C-19 for the rationale.