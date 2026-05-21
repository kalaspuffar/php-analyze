<?php
// Slice-2 integration fixture (`recorder-call-events`):
// 10⁴ calls to one user function. The harness asserts:
//   - exactly 10_000 `C:` records in the dump
//   - exactly one `D:` entry (the function appears once in the dict)
//   - every `C:` record's `fn_id` matches the `D:` line's `fn_id`
//
// Keep the function body trivial — Phase-5 zero-alloc work will
// re-use this fixture as a microbench, and a heavyweight body
// would dwarf the begin/end overhead we want to measure.

declare(strict_types=1);

function noop(int $i): int {
    return $i;
}

for ($i = 0; $i < 10000; $i++) {
    noop($i);
}
