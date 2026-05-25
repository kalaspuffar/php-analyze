<?php
// P-0 integration fixture (`skip-functions-directive`):
// proves the default skip list filters `strlen` / `count` out of the
// trace while preserving observation of a user-defined function
// (`my_skip_fn_target`).
//
// The harness asserts:
//   - exactly ONE `C:` record naming `my_skip_fn_target` in the dump
//     (no `strlen` records, no `count` records — those are on the
//     curated default skip list and PHP cached `should_observe = false`
//     after the first sight).
//   - exactly ONE `D:` entry for `my_skip_fn_target` (the dict only
//     ever sees functions whose `begin` actually fires).
//
// The fixture's PHP file is also implicitly observed as a top-level
// `closure:` per `COMMENTS.md` C-5; that record is on the
// "non-skipped" side of the filter and is expected to appear in the
// dump alongside the user function's record.

declare(strict_types=1);

function my_skip_fn_target(int $i): int {
    return $i;
}

// These three should be filtered by the default skip list.
strlen("a");
strlen("b");
strlen("c");
count([]);
count([1, 2, 3]);

// This one should be observed.
my_skip_fn_target(42);
