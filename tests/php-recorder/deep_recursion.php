<?php
// Slice-3 integration fixture (`recorder-depth-and-cap-drops`):
// A single user function that recurses 2000 times. With the test
// harness setting `php_analyze.max_depth = 100`, the recorder must
// accept the first 100 begins and drop the remaining 1900 on the
// depth gate. The dump's `DROP: dropped_records` line carries the
// 1900 count (plus the top-level script-body frame's 1 record if
// Zend reports it; see C-5 / harness comment).

declare(strict_types=1);

function recurse(int $n): int {
    if ($n <= 0) {
        return 0;
    }
    return recurse($n - 1) + 1;
}

recurse(2000);
