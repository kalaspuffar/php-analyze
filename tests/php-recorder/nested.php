<?php
// Slice-2 integration fixture (`recorder-call-events`):
// Three user functions arranged in a strict parent-child chain.
// The harness asserts:
//   - exactly three `C:` records (one per function)
//   - exactly three `D:` entries (each function appears once)
//   - `(call_id, parent)` pairs reconstruct the (3,2), (2,1), (1,0)
//     chain in emission order (end-handler order: c first, b, a last)

declare(strict_types=1);

function c(): void {
    // leaf
}

function b(): void {
    c();
}

function a(): void {
    b();
}

a();
