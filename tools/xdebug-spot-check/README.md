# `tools/xdebug-spot-check/` — operator-driven accuracy report

One-shot validation that `php-analyze`'s recorder produces
plausible numbers compared to Xdebug, for one chosen PHP
fixture. The output is a Markdown report at
[`REPORT.md`](./REPORT.md) that the operator reads to judge
whether the wire format the recorder produces is trustworthy
enough to hand to the `../php-tree-visualizer` repo.

This is **not** a CI gate. There is no automated pass/fail. The
report carries numbers; the operator decides what they mean.
The deferred CI-gated comparator (see `COMMENTS.md` §6.4) is a
separate, larger workstream.

This tool binds `SPECIFICATION.md` §1.3 #3b (≥ 99.5 % call
coverage vs. Xdebug) and #3c (per-call timing within ±5 % of
Xdebug) at MVP-closing scope per `COMMENTS.md` §6.3 #3. The
underlying OpenSpec change is `xdebug-spot-check`.

## What the tool does

[`run.sh`](./run.sh) orchestrates a single comparison run:

1. Checks prerequisites and bails fast with an apt-actionable
   error message if any are missing.
2. Builds `libphp_analyze.so` and `stub-ingest` (cargo
   no-ops on the second invocation).
3. Runs the fixture once under Xdebug's trace mode
   (`xdebug.mode = trace`, `xdebug.trace_format = 1`), capturing
   `last-run/trace.xt`.
4. Runs the same fixture once under `php-analyze` against a
   freshly-spawned `stub-ingest` on a loopback port, captures
   the resulting MessagePack batches as JSON via
   `/debug/batches` into `last-run/batches.json`.
5. Hands both to [`compare.py`](./compare.py), which produces a
   fresh [`REPORT.md`](./REPORT.md) with two sections:
   - **Call coverage** — total counts, per-function counts,
     overlap percentages.
   - **Per-call timing delta** — bucketed histogram + p50/p95/p99
     of `|analyze - xdebug| / xdebug`.

## Host requirements

| Requirement | Debian-family install command |
| --- | --- |
| PHP 8.3 or PHP 8.4 (matching the freshly-built cdylib's module API) | `apt install php8.4` |
| The matching Xdebug 3.x package | `apt install php8.4-xdebug` |
| Python 3 with `msgpack` | `apt install python3-msgpack` *(or `pip install --user msgpack`)* |
| Rust toolchain to build the cdylib | already required for the rest of this repo |

The host's `update-alternatives --config php-config` MUST point
at the same PHP version the cdylib will be loaded into. The
cdylib is built against whatever `php-config` resolves to at
`cargo build` time. If they mismatch, `run.sh` bails with a
clear error.

## How to run

From the repo root:

```sh
# Default: tests/php-bench/recursive_walk.php
./tools/xdebug-spot-check/run.sh

# Alternate fixture
./tools/xdebug-spot-check/run.sh path/to/your-fixture.php
```

The script writes its working artefacts under
`tools/xdebug-spot-check/last-run/` (gitignored) and overwrites
`tools/xdebug-spot-check/REPORT.md` on every successful run.
The operator commits whichever revision of `REPORT.md` they
consider the current MVP-validation evidence.

## How to read the report

Open `REPORT.md` and look at:

1. **Header** — the cdylib's git SHA + the host's PHP/Xdebug
   versions + the timestamp tell you what was measured. If the
   git SHA shows `dirty-working-tree`, the report came from an
   uncommitted local build.
2. **Call coverage** — does each function the operator cares
   about show roughly equal counts in both columns?
   `recursive_walk.php`'s reference shape is ~82 thousand
   `make_tree` calls and ~123 thousand `walk_tree` calls per
   run. Significant skew (>5 %) is the signal to investigate.
3. **Per-call timing delta** — the histogram shows how many
   per-function-position-matched calls fall into each bucket.
   The p95 row is the operator-relevant summary: "how bad is
   the worst typical case?"

## Known limitations

- **Xdebug's own per-call overhead inflates the reference
  durations.** Xdebug is itself a per-call instrumented
  profiler; its `t_out − t_in` includes its own instrumentation
  cost. The right way to read the timing-delta histogram is as
  "how consistently does the recorder's timing shape track
  Xdebug's?", not "is the recorder accurate to the millisecond?".
- **PHP-specialised internals are invisible to the Zend
  observer.** Per `COMMENTS.md` C-5, opcode-specialised
  constant-arg internals like `strlen("hi")` are observed by
  Xdebug but not by `php-analyze`. The coverage table flags any
  function name that appears in Xdebug-only as a footnote; the
  operator should not interpret these as recorder bugs.
- **Position-wise pairing is order-dependent.** For
  deterministic fixtures (`recursive_walk.php`, `flat_calls.php`,
  `json_batch.php`) this is fine: the Nth call of `make_tree`
  on either side corresponds. For non-deterministic fixtures
  (anything involving timing, randomness, or external IO),
  the timing-delta histogram is meaningless. The report's
  preamble flags this if it can detect the call-set isn't
  identical on both sides.
- **The committed `REPORT.md` ages.** A future recorder change
  could move the numbers without anyone noticing. The committed
  copy is dated; re-run the tool whenever something the
  recorder touches lands.

## Where this fits in the MVP roadmap

This tool is the third of four MVP-closing items named in
`COMMENTS.md` §6.3:

1. ✓ `docs-mvp-reframe`
2. ✓ `fpm-integration-test`
3. ▶ `xdebug-spot-check` (you are here)
4. `capture-reference-batches` — fixtures for the visualizer
   team's tests
