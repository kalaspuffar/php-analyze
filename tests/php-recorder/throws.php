<?php
// Slice-2 integration fixture (`recorder-call-events`):
// A function that throws, caught by the script body. The harness
// asserts:
//   - the `C:` record for `bad()` has `abnormal_exit = true`
//   - the script body's record (the implicit top-level closure)
//     has `abnormal_exit = false` — the throw is caught, so the
//     script body returns normally.

declare(strict_types=1);

function bad(): never {
    throw new RuntimeException('slice-2 throws fixture');
}

try {
    bad();
} catch (RuntimeException) {
    // swallow
}
