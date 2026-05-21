<?php
// Phase-4 slice 2 fixture (`recorder-flushes-into-shipper`):
// the minimal observable PHP request. The script body is itself
// observed as a closure (see `COMMENTS.md` C-5 — the spike's
// coverage table shows PHP reports the top-level body as
// `closure:<file>:1`), so even an "empty" file produces exactly
// one `C:` record. With the default flush thresholds, no
// mid-request flush can fire — the buffer holds one record at
// `RSHUTDOWN`, and the slice-2 `RSHUTDOWN`-final flush hand-off
// produces exactly one `F:` line with `trigger=rshutdown
// record_count=1`. This pins the spec scenario "no mid-request
// flushes for a sub-threshold workload; `RSHUTDOWN` still flushes
// the residual" (PF-7's spec-parity follow-up in `COMMENTS.md`).

declare(strict_types=1);
