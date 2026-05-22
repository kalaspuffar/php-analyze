<?php
// NFR-PERF-1 binding fixture #1 (`bench-canonical-workloads`,
// resolves OQ-7): pure user-call overhead.
//
// 10⁶ tight-loop calls to a noop user function. With
// `flush_records = 10_000` (default), the recorder produces ~100
// batches; the shipper drops them on the unreachable
// `server_url` configured by the bench, but the recorder cost we
// care about (per-call begin/end pair) is paid before the shipper
// sees the batches. The fixture's wall-time is dominated by:
//
//   - PHP interpreter startup (≈ 50-200ms).
//   - The 10⁶-iteration loop (workload).
//   - PHP shutdown (RSHUTDOWN + MSHUTDOWN, bounded by
//     `shutdown_grace_ms = 200` in the profiled run).
//
// Stresses the recorder hot path's per-call dispatch cost with
// maximum dictionary-hit ratio: the dictionary has one entry
// (`noop`) and the second call onwards is a pure hit. This is
// the "best case" for the recorder — anything slower than 2.0×
// here means the per-call cost dominates PHP's own dispatch.

declare(strict_types=1);

function noop(int $x): int {
    return $x;
}

for ($i = 0; $i < 1_000_000; $i++) {
    noop($i);
}
