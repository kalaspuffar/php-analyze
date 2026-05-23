<?php
// fpm-integration-test fixture: the smallest deterministic PHP body
// that produces a stable wire `Batch` shape per FastCGI request.
//
// Binds (via `crates/php-analyze/tests/fpm_repeated_requests.rs`):
//   - SPECIFICATION.md §1.3 #2 — extension loadable in the
//     `fpm-fcgi` SAPI
//   - SPECIFICATION.md §3.1 AC-BS-3 — 100 repeated requests on
//     one FPM worker do not leak RSS (MVP-closing 100-request
//     scale per COMMENTS.md §6.3; spec's 10⁴ scale deferred per
//     COMMENTS.md §6.4)
//   - SPECIFICATION.md §3.4 AC-PB-1 — every FPM worker owns
//     exactly one shipper thread after first RINIT (bound via
//     the `pm.max_children = 8` helper in the wrapping test)
//
// Per-request emission:
//   - One closure-shaped record for the script body (file:1).
//   - One `function` record for `noop()`.
//   - One `internal` record for `strlen("ok")`.
// = 3 calls, 2 dict entries (`noop`, `strlen`) per request.
//
// The integration test asserts only on `meta.trace_id` cardinality
// (100 distinct values over 100 requests) and on the per-worker
// RSS / thread invariants — not on the exact dict/call shape — so
// any small drift in this fixture stays internally consistent.

declare(strict_types=1);

function noop(int $x): int {
    return $x;
}

$len = strlen("ok");
noop($len);

echo "ok";
