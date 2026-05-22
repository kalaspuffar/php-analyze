<?php
// AC-BS-4 / AC-PB-2 binding fixture (`shipper-deadline-mid-retry`):
// produce exactly one batch so the integration test can drive
// the shipper's production HTTP path against a slow upstream
// and observe the MSHUTDOWN deadline contract end-to-end.
//
// The per-test `php.ini` configures the stub-ingest stub via
// `--simulate-slow 5000` (so the stub sleeps 5s per request),
// `php_analyze.http_timeout_ms = 200`, `retry_count = 5`,
// `retry_backoff_ms = 50`, and crucially
// `php_analyze.shutdown_grace = 200` (a tight 200ms MSHUTDOWN
// budget).
//
// Expected timeline at MSHUTDOWN:
//   T0 + 0ms:    Deadline cell published to now + 200ms.
//   T0 + 0ms:    Shipper attempt 1 begins, effective timeout 200ms.
//   T0 + 200ms:  Attempt 1 times out (stub mid-sleep). Next
//                iteration's deadline_fn() reads the cell, sees
//                Some(now+0ms), returns DeadlineExceeded.
//                Batch dropped.
//   T0 + ~200ms: MSHUTDOWN returns.
//
// The test asserts the total PHP wall-clock falls under 2000ms
// (PHP startup ≈ 100-300ms plus the shutdown_grace+200ms MSHUTDOWN
// budget plus a generous CI buffer). Without the C-18 fix
// (`shipper-deadline-mid-retry`), the pre-drain shipper called
// `drained_consume(..., None, ...)` and the retry loop exhausted
// `retry_count + 1 × http_timeout_ms + cumulative backoff =
// 2750ms` regardless of the deadline cell. That regression
// previously surfaced as elapsed = 2806ms.

declare(strict_types=1);

function noop(int $x): int {
    return $x;
}

noop(1);
