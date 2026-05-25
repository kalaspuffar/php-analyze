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
records, plus the script-body closure's single drained
record in the final batch (see below).

The recorder's MSHUTDOWN drain (`SPECIFICATION.md` §3.2)
emits the still-open script-body closure as a single
`abnormal_exit = true` `CallRecord` with `(call_id=1,
parent=0, depth=0)`. For this workload the drain batch
reliably reaches the capture sink (the steady-state
shipper-queue pressure is mild), so the committed final-
batch sample carries that drained record.

## Committed sample

- Recorder commit SHA at capture time: `8b376d2c3afa`
- Batches captured (full run): 25
- Batches kept for git: 3 (`batch-0001`, `batch-0002`, plus
  the **final** batch `batch-0025` — the highest-numbered
  batch the capture sink received, which carries the
  MSHUTDOWN-drained root record)
- File sizes: ~1 MB each (the final batch is smaller — it
  contains only the residual records plus the drained root)

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
- The final batch contains exactly one `CallRecord` with
  `call_id == 1`, `parent == 0`, `depth == 0`, and
  `abnormal_exit == true` — the MSHUTDOWN-drained
  script-body closure root. Every other record's `parent`
  field resolves either to a `call_id` in the same trace
  or to that root.
- `call.cpu_u` / `call.cpu_s` values are `0` on a build of
  the recorder running under `php_analyze.cpu_snapshot_mode
  = per-call` (the default — but those values can be near
  the clock-resolution floor for short recursive calls).
  See `COMMENTS.md` C-19 for the rationale.