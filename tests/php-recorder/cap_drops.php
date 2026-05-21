<?php
// Slice-3 integration fixture (`recorder-depth-and-cap-drops`):
// Calls a trivial user function 200 times. The harness configures
// `php_analyze.buffer_cap_bytes` tightly so only the first K calls
// fit (K is computed by the test from the §3.2 estimator); the
// remaining `200 - K` calls are dropped on the cap gate and counted
// in the dump's `DROP: dropped_records` line.

declare(strict_types=1);

function noop(): void {
}

for ($i = 0; $i < 200; $i++) {
    noop();
}
