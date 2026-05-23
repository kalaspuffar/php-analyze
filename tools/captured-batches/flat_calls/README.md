# `flat_calls/` — flat hot-path captures

Captured from [`tests/php-bench/flat_calls.php`](../../../tests/php-bench/flat_calls.php).

## Workload

1,000,000 calls of one PHP user function (`noop`) in a tight
loop. No internal-function variety, no recursion — pure
hot-path stress on the recorder's begin/end snapshot loop.

## What's in each batch

Each `batch-NNNN.msgpack` is one `wire::Batch` map with three
top-level keys per `SPECIFICATION.md` §4.2:

| Key | Content |
| --- | --- |
| `meta` | `MetaFull` — schema_version=1, trace_id (zero-bytes placeholder today), host, pid, start_time, sapi="cli", uri_or_script, dropped_records. |
| `dict` | `DictEntry`s for first-sight functions in this trace. **Batch 1** carries both `closure:<file>:1` (the script body) and `noop`. **Batches 2–N** carry an empty dict (the recorder emits each fn-id once per trace). |
| `calls` | `CallRecord`s in emission order, up to `flush_records = 10000` per batch. |

## Expected call-set

A full run produces ~50–54 batches × 10,000 records ≈
500,000–540,000 `noop` calls (Xdebug observes the full 10⁶;
the recorder's batching budget caps somewhat below that
under the bench's tight `flush_records` default — the gap
reflects channel-bounded behaviour, not a correctness bug).

Of the committed 3 batches × 10,000 records = 30,000 sample
`noop` records, plus the script-body's single `closure:<file>:1`
record (the first call in batch 1).

## Committed sample

- Recorder commit SHA at capture time: `c7924292ca4e`
- Batches captured (full run): 54
- Batches kept for git: 3 (the first three)
- File sizes: ~1 MB each

The committed files are SAMPLES — see
[`../README.md`](../README.md) for the "samples, not goldens"
explanation. Regenerate by running
`tools/capture-fixtures.sh` from the repo root.

## Parser-side checks the visualizer team can run

- `meta.schema_version == 1` on every batch.
- `meta.sapi == "cli"` on every batch (this is a CLI-SAPI
  capture).
- Batch 1's `dict` contains exactly 2 entries: one
  `closure:<...>:1` (the script body) + one named `noop`
  with `kind == 0` (function).
- Batches 2 & 3's `dict` is empty `[]` (per the recorder's
  dict-once-per-trace semantic).
- Every `call.fn` value references a dict entry the parser
  has already seen in this trace.
- `call.depth` for `noop` records is `1` (one level under
  the script-body closure at depth 0).