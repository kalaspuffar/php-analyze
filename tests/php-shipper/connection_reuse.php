<?php
// AC-SH-6 binding fixture (`stub-ingest-connection-counter`): drive
// many sequential POSTs through the shipper's single `ureq::Agent`
// so the integration test can assert that
// `/debug/connection_count` reports `1` despite 1000 sends.
//
// The per-test `php.ini` sets `php_analyze.flush_records = 1`, so
// every emitted exit record trips the recorder's flush threshold
// and produces a `PendingBatch`; the shipper turns each into one
// POST. Calling `noop()` 1000 times therefore exercises ≈1000
// distinct POSTs through the same `ureq::Agent`, which (by
// `ureq`'s default HTTP/1.1 keep-alive) reuses one TCP connection
// for the entire run.
//
// Batch count is approximately `1000` (give or take the script
// body's own exit record and the `RSHUTDOWN` final-flush
// bookkeeping); the integration test asserts on the *connection
// count*, not the batch count, so the small wobble is fine.

declare(strict_types=1);

function noop(int $x): int {
    return $x;
}

for ($i = 0; $i < 1000; $i++) {
    noop($i);
}
