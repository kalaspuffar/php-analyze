# `json_batch/` — interleaved internal + user calls

Captured from [`tests/php-bench/json_batch.php`](../../../tests/php-bench/json_batch.php).

## Workload

100 iterations of `json_encode` + `json_decode` + `array_map`
over a small array. Tests high-fan-out internal calls
(`json_encode`, `json_decode`) interleaved with short user
calls (`row_id`, the array_map callback). Closer in shape to
real PHP applications than `flat_calls.php`'s pure hot path.

## What's in each batch

Same shape as the other workloads (`meta`, `dict`, `calls`).
The dict for batch 1 carries **four** entries:

| `fqn` | `kind` | Notes |
| --- | --- | --- |
| `closure:<file>:1` | `2` (closure) | The script body. |
| `json_encode` | `3` (internal) | PHP's built-in JSON encoder. |
| `json_decode` | `3` (internal) | PHP's built-in JSON decoder. |
| `row_id` | `0` (function) | The user-defined transformer passed to `array_map`. |

Batches 2 and 3 carry an empty `dict` (dict-once-per-trace
semantic). All three batches carry the same per-batch ceiling
of `flush_records = 10000` records.

## Expected call-set

A full run produces ~11 batches × 10,000 records ≈ 110,000
calls. Of the committed 3 batches × 10,000 records = 30,000
sample records, distributed roughly evenly across the four
functions (`json_encode`, `json_decode`, `row_id`, plus the
script-body closure's single drained record in the final
batch — see below).

The recorder's MSHUTDOWN drain (`SPECIFICATION.md` §3.2)
emits the still-open script-body closure as a single
`abnormal_exit = true` `CallRecord` with `(call_id=1,
parent=0, depth=0)`. The committed final-batch sample
(`batch-0011`) carries that drained record alongside any
residual records flushed at MSHUTDOWN.

## Committed sample

- Recorder commit SHA at capture time: `8b376d2c3afa`
- Batches captured (full run): 11
- Batches kept for git: 3 (`batch-0001`, `batch-0002`, plus
  the **final** batch `batch-0011`)
- File sizes: ~1 MB each (the final batch is much smaller —
  it contains only the residual records plus the drained
  root)

Regenerate via `tools/capture-fixtures.sh` from the repo
root. See [`../README.md`](../README.md) for the
samples-not-goldens disclaimer.

## Parser-side checks the visualizer team can run

- Batch 1's `dict` contains exactly 4 entries; the kinds are
  one `closure`, one `function`, two `internal`.
- `dict[?fqn=="json_encode"].kind == 3` (internal) — the
  parser should be able to distinguish internal vs. user
  calls by the `kind` field.
- `call.depth` for `json_encode` / `json_decode` calls is
  `1` (called directly from the script body); for `row_id`
  calls it's `2` (called via `array_map`, which is itself at
  depth 1). The depth field encodes the recorded call-stack
  position.
- `call.mem_in` / `call.mem_out` are non-zero — JSON operations
  allocate PHP-side memory; the recorder captures
  `zend_memory_usage(true)` snapshots that reflect that.
- The final batch contains exactly one `CallRecord` with
  `call_id == 1`, `parent == 0`, `depth == 0`, and
  `abnormal_exit == true` — the MSHUTDOWN-drained
  script-body closure root. Every other record's `parent`
  field resolves either to a previously-seen `call_id` or
  to that root.