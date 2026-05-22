<?php
// NFR-PERF-1 binding fixture #2 (`bench-canonical-workloads`,
// resolves OQ-7): mixed user-call + PHP-internal-call workload.
//
// Build 10⁵ synthetic rows, json_encode → json_decode round-trip,
// iterate the decoded array via a user wrapper function reading
// each row's `id` field. The mix exercises:
//
//   - User-call dispatch in the build loop + the iterate loop.
//   - PHP-internal calls (`json_encode`, `json_decode`) — the
//     recorder observes these per the C-5 / spike-zend-observer
//     finding, with the well-known `strlen` opcode-specialisation
//     caveat (PHP 8.x specialises `strlen("literal")` away;
//     `json_encode` / `json_decode` are not specialised, so they
//     do fire begin/end observer events).
//   - Memory pressure from the encoded string + decoded array.
//
// Real-world frameworks (PSR-7 message handling, ORM hydrate)
// have this exact shape: user wrapper around internal serde
// boundary. Stresses the recorder's categorise() fast-path that
// distinguishes user fns from internals.

declare(strict_types=1);

function row_id(array $row): int {
    return (int) $row['id'];
}

$rows = [];
for ($i = 0; $i < 100_000; $i++) {
    $rows[] = ['id' => $i, 'name' => "row-{$i}", 'tag' => 'bench'];
}

$encoded = json_encode($rows);
$decoded = json_decode($encoded, true);

$sum = 0;
foreach ($decoded as $row) {
    $sum += row_id($row);
}
