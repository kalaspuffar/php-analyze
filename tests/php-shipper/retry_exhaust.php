<?php
// AC-SH-2 / AC-SH-4 binding fixture (`stub-ingest-configurable-failure`):
// produce exactly one batch so the integration test can drive the
// shipper through `retry_count + 1` failed POSTs and observe the
// retry-exhaust drop-notice path end-to-end.
//
// The per-test `php.ini` configures the stub-ingest stub via
// `--respond-with 500` (so every POST returns 500), `retry_count = 3`
// (so the shipper makes 4 attempts total), and PHP's `error_log`
// directive points at a tempfile (so the shipper's `E_NOTICE`
// drop-line is captured for the integration test's assertions).
//
// The test asserts three things after PHP exits:
//   1. `/debug/batches.len() == 4` — AC-SH-2 (the stub stored the
//      same body four times, one per attempt).
//   2. The error-log file contains exactly one `E_NOTICE` matching
//      the §5.2 step-4 drop-notice format (`php-analyze: dropped
//      <N> records from trace <uuid>: <url> http 500 (attempt 4)`).
//   3. The error-log file contains zero occurrences of the
//      sentinel token configured via `php_analyze.auth_token` —
//      AC-SH-4 (token never appears in any log line).

declare(strict_types=1);

function noop(int $x): int {
    return $x;
}

noop(1);
