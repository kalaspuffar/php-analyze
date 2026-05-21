<?php

// Fixture for the Phase-0 zend_observer spike. Confirms that the
// observer surface reaches internal (built-in) PHP functions — the
// core question Risk R-2 in SPECIFICATION.md §11 asks. A miss here
// retires R-2 only partially.
//
// Functions touched and what the spike observes about each:
//   - strlen        : PHP 8.x specialises strlen-with-constant-arg
//                     into a Zend opcode at compile time. The observer
//                     surface does NOT see it. Kept here on purpose:
//                     the integration test asserts the negative case,
//                     and COMMENTS.md C-5 records the finding.
//   - array_map     : Observed. The userland closure passed to it is
//                     also observed (entered/exited once per element).
//   - json_encode   : Observed (array-input internal).
//   - preg_match    : Observed (regex internal).

declare(strict_types=1);

strlen("hi");
array_map(fn ($x) => $x + 1, [1, 2, 3]);
json_encode(["a" => 1]);
preg_match("/x/", "x");
