<?php
// NFR-PERF-1 binding fixture #3 (`bench-canonical-workloads`,
// resolves OQ-7): recursive call patterns + frame-stack growth.
//
// Build a balanced binary tree of `2^10 = 1024` leaf nodes via
// recursive `make_tree(depth, key)`, then walk it via recursive
// `walk_tree(node)` summing each node's `key`. Inner loop runs
// the build + walk `40` times so PHP startup is a small fraction
// of total runtime (each pass takes ~10ms unprofiled; 40 passes
// gives ~400-500ms total).
//
// Stresses:
//
//   - Recursive call dispatch + frame-stack growth/cleanup. Real
//     frameworks (Symfony's container resolution, WordPress's
//     `apply_filters` cascade, Laravel's middleware pipeline)
//     have this exact shape — recursive call chains 5-15 deep
//     through user code.
//   - Per-call object/array allocation in PHP. Each `make_tree`
//     node is a fresh `['key' => …, 'left' => …, 'right' => …]`
//     array; the recorder's `begin_with_snapshots` runs against
//     these PHP-level allocations.
//   - Dictionary churn: two user functions (`make_tree`, `walk_tree`),
//     hit consistently. Different from `flat_calls.php`'s single
//     entry, but still small enough that the dict-miss path runs
//     only twice per process.

declare(strict_types=1);

function make_tree(int $depth, int $key): array {
    if ($depth === 0) {
        return ['key' => $key, 'left' => null, 'right' => null];
    }
    return [
        'key' => $key,
        'left' => make_tree($depth - 1, $key * 2),
        'right' => make_tree($depth - 1, $key * 2 + 1),
    ];
}

function walk_tree(?array $node): int {
    if ($node === null) {
        return 0;
    }
    return $node['key'] + walk_tree($node['left']) + walk_tree($node['right']);
}

$total = 0;
for ($i = 0; $i < 40; $i++) {
    $tree = make_tree(10, 1);
    $total += walk_tree($tree);
}
