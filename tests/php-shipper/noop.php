<?php
// Phase-4 slice-3 fixture (`shipper-encoder-and-http`): the smallest
// observable PHP request that produces an end-to-end-shipped batch.
//
// The recorder observes:
//   - the script body (one closure-shaped record at file:1)
//   - the single `noop()` call
//
// = 2 `C:` records, 1 dict entry (`noop`).
//
// The default `flush_records = 10000` and `flush_bytes ~= 8 MiB`
// thresholds are far above what one noop call produces, so no
// mid-request flush fires. `RSHUTDOWN` then issues the slice-2
// final flush, which hands the `PendingBatch` to the slice-3
// `RmpEncodeAndHttpPost` `OnBatch`, which encodes via
// `rmp_serde::to_vec_named` and POSTs the result to the configured
// `php_analyze.server_url`. The integration test verifies the
// resulting wire `Batch` landed on the stub's `/debug/batches`
// queue.

declare(strict_types=1);

function noop(int $x): int {
    return $x;
}

noop(1);
