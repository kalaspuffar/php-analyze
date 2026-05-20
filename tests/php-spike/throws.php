<?php

// Fixture for the Phase-0 zend_observer spike. Exception-unwind path:
// `bad()` throws; its caller (this script's top-level) catches one
// frame above. The spike's `end` handler must observe the
// abnormal_exit on `bad`'s exit line, and the original try/catch
// MUST still resolve normally (the spike's
// ExecutorGlobals::has_exception() is non-destructive — see
// spike.rs:`end`).

declare(strict_types=1);

function bad(): void {
    throw new RuntimeException("x");
}

try {
    bad();
} catch (RuntimeException $_e) {
    // Swallow; we only care that the spike saw `abnormal=true`.
}
