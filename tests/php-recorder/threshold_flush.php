<?php
// Phase-4 slice 2 fixture (`recorder-flushes-into-shipper`):
// 10_000 calls to one user function (`noop`). The recorder also
// observes the script body itself as a closure, so the dump ends
// up with 10_001 `C:` records total (10_000 noop + 1 script body).
// Run with `php_analyze.flush_records = 5000` and a very large
// `php_analyze.flush_bytes` so only the records-trigger fires
// during the request. The harness asserts the dump contains:
//   - exactly 3 `F:` lines
//   - first two have `trigger=records record_count=5000`
//     (records 1..=5000 and 5001..=10000, all from `noop`)
//   - third has `trigger=rshutdown record_count=1` — the script
//     body's own `end` record is the residual at RSHUTDOWN
//   - total `C:` records = 10_001 (10_000 noop + 1 script body)
//
// The trigger cadence is the slice-2 contract: every
// `flush_records`-th record produces a records-trigger; whatever
// remains in the buffer at RSHUTDOWN produces the final
// rshutdown-trigger (if any).

declare(strict_types=1);

function noop(int $i): int {
    return $i;
}

for ($i = 0; $i < 10000; $i++) {
    noop($i);
}
