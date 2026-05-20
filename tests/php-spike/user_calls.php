<?php

// Fixture for the Phase-0 zend_observer spike. Exercises the three
// user-call categories the spike's FQN composer must distinguish:
//
//   - a top-level user function   -> function:<file>:<line>:only_me
//   - a method on a user class    -> method:C::m
//   - a user-defined closure      -> closure:<file>:<line>
//
// Called exactly once each so the integration test can assert
// "exactly one entry, exactly one exit" per category.

declare(strict_types=1);

function only_me(): void {}

class C {
    public function m(): void {}
}

$closure = function (): void {};

only_me();
(new C())->m();
$closure();
