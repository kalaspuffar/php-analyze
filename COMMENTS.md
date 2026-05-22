# COMMENTS

This file accumulates clarifications, review notes, and out-of-scope
discoveries that supplement `SPECIFICATION.md`. If a statement here
conflicts with `SPECIFICATION.md`, this file is the more recent
clarification.

The structure of this file is:

1. **Open blockers** — operational constraints that affect every change.
2. **Path to v1.0** — the prioritized work list to close
   `SPECIFICATION.md` §1.3 acceptance criteria for the extension MVP.
3. **Forward-looking design decisions** — load-bearing architectural
   notes that subsequent work must respect.
4. **Repository hygiene** — workspace conventions that affect future
   refactors.
5. **Phase 5 anchor** — design note for the zero-alloc audit harness
   referenced from multiple Phase-5 follow-ups.

---

## 1. Open blockers

### B-2 — `git push` blocked on remote auth

**Status**: blocks remote-push of every branch this host creates.

**Cause**: this build host has no SSH key registered with
`git@github.com:kalaspuffar/php-analyze.git`. `git push -u origin
<branch>` fails with `Permission denied (publickey)`.

**To unblock**: push from a workstation that has push credentials.
Any new feature branch created on this build host needs the user's
workstation to push before the PR can be opened.

---

## 2. Path to v1.0 — prioritized work list

The extension has cleared Phase 0 (spike), Phase 1 (scaffold/config),
Phase 2 (recorder), Phase 3 (wire + stub ingest), and Phase 4 (shipper
substrate + encoder + HTTP + MSHUTDOWN deadline). The remaining work
to satisfy `SPECIFICATION.md` §1.3 acceptance criteria 1–8 is grouped
below by priority. Each entry is a queued OpenSpec change unless
otherwise marked.

The **ingestion server** and **visualization layer** are separate
deliverables that live downstream of this extension; they are
intentionally out of scope for this prioritized list and are tracked
in their own future repositories. Acceptance for this extension's
v1.0 is bounded by the eight criteria in `SPECIFICATION.md` §1.3.

### P0 — Slice-3 integration test gaps (must close before v1.0)

These AC scenarios from `SPECIFICATION.md` §3.3 / §9.2 are unbacked by
integration tests. Each was deferred from slice 3 because it needs a
new feature on the `stub-ingest` binary; the shipper code itself is
in place.

- **`stub-ingest-header-capture`** — add `/debug/last_request_headers`
  so the deferred integration tests for the `Authorization` header
  (matches configured token) and the `User-Agent` (includes crate
  version) can land. Pairs with `SPECIFICATION.md` §5.2.
- **`stub-ingest-connection-counter`** — add
  `/debug/connection_count` plus a per-PHP-request fixture loop.
  Binding evidence for **AC-SH-6** (1000 sends → one TCP connection).
- **`stub-ingest-configurable-failure`** — add `--respond-with <status>`
  plus `tests/php-shipper/retry_exhaust.php`. Binding evidence for
  **AC-SH-2** (always-500 → drop after `retry_count + 1` attempts)
  and the §5.2 step-4 `E_NOTICE` line shape, including **AC-SH-4**
  (token never appears in any log).
- **`stub-ingest-slow-mode`** (closed): the stub-side
  `--simulate-slow` flag and the AC-SH-3 stub-side binding
  shipped in this change. The matching PHP-level integration
  test for AC-BS-4 / AC-PB-2 surfaced a spec/code parity gap
  (C-18) and was deferred to `shipper-deadline-mid-retry`,
  which landed both the production fix and the PHP-level test.
- **`shipper-deadline-pass-integration-test`** (closed): the
  Rust-level deadline-pass binding via `SlowRecordingOnBatch`
  shipped during `shipper-deadline-at-recv-loop-head`. The
  production-path binding via `mshutdown_drain.php` shipped
  during `shipper-deadline-mid-retry`.

### P0 — Spec compliance gaps

- **`notice-on-master-switch-off`** — emit `E_NOTICE` on
  `MasterSwitchOff` so operators of disabled extensions see a single
  log line. One line in `bootstrap`.

(`startup-catch-unwind` and `spawn-failure-recovery` were closed by
the `bootstrap-startup-panic-safety` change — wraps the `MINIT` body
in `catch_unwind` and converts shipper-thread spawn failure into
state cleanup + one `E_WARNING` + silent-disable. See the archived
OpenSpec change for the spec deltas, design rationale, and the test
surface that pins both contracts.)

The Open Questions from `bootstrap-startup-panic-safety`'s
`design.md` are queued in the P2 band below:
`minfo-catch-unwind` (Q-1) and
`shipper-spawn-failure-in-disable-reason` (Q-2).

### P1 — Phase 5: hot-path tuning and benchmarks (§10 Phase 5)

Required to validate **NFR-PERF-1** (geo-mean wall-time overhead
≤ 2.0× vs. unprofiled) and **AC-RC-5** (zero heap allocations on the
hot path). This is the largest unaddressed phase by effort.

- **`bench-criterion-skeleton`** (closed): scaffolded `criterion`,
  added the no-PHP per-call micro-bench (`recorder_hot_path`,
  ~178 ns observed) and the 10⁴-call workload-shape bench
  (`recorder_workload`).
- **`bench-canonical-workloads`** (closed): resolved **OQ-7** with
  three self-contained workloads (`flat_calls.php`, `json_batch.php`,
  `recursive_walk.php`) and added the `workload_overhead` bench
  that asserts the ≤ 2.0× geo-mean budget. **First observed run
  reports a geo-mean of 8.89×** (`flat_calls` 30.61×, `json_batch`
  2.11×, `recursive_walk` 10.89×) — the bench correctly fails the
  assertion against the current hot path. See
  `recorder-hot-path-tuning` below for the follow-up.
- **`recorder-hot-path-tuning`** (new, P0-priority within P1) — bring
  the `workload_overhead` geo-mean from 8.89× down to ≤ 2.0×. The
  per-call profile points at the recorder kernel's overhead beyond
  the in-process micro-bench's ~178 ns/call: with `flat_calls` at
  ~2.2µs added per noop() call, the gap is the Zend-observer
  trampoline + downstream `Trace::push_record` + memory traffic
  the micro-bench can't measure. Likely paths: (a) batch the
  observer-side captures into a single struct on the stack instead
  of separate `EntrySnapshots` / `ExitSnapshots` reads;
  (b) drop the `Arc<str>` clones in the categorise hit path (the
  `FunctionKey::Function { file, function, ... }` construction
  per call); (c) interning + arena allocation for the dict-miss
  path. Should run after / alongside `recorder-zero-alloc-audit`
  since the allocator-counting harness will point at the specific
  allocations driving the cost.
- **`recorder-zero-alloc-audit`** — the audit harness that pins
  **AC-RC-5**. See §5 of this file ("Phase 5 anchor") for the design
  note. Includes the `// NOTE for Phase 5` markers near the remaining
  `to_owned()` allocations in `begin_with_snapshots` (dict-miss path);
  may require an arena or intern table for those allocations to clear
  the zero-alloc bar.

### P1 — Phase 6: hardening, docs, packaging (§10 Phase 6)

These close the remaining `SPECIFICATION.md` §1.3 acceptance criteria.

- **`token-leak-grep-test`** — CI-level grep over all
  integration-test logs for the configured token; must find zero hits.
  Pairs with `stub-ingest-configurable-failure` which exercises the
  `E_NOTICE` drop-line path (the natural place a leak would surface).
  Closes **AC-SH-4** as a CI gate.
- **`tls-system-ca-integration-test`** — **AC-SH-5** binding evidence:
  stub server with a self-signed cert → connection MUST fail; with a
  system-trusted cert (test CA injected for the test) → connection
  MUST succeed.
- **`cargo-audit-in-ci`** — wire `cargo audit` into
  `.github/workflows/ci.yml`, warning-only initially per §9.6.
- **`lock-readme-directive-table`** — parsing test that walks the
  README directive table and matches against `DIRECTIVES`'s defaults
  / ranges; covers the spike directives' drift guard. Required for
  §1.3 #8 (README documents every directive).
- **`source-distribution-tarball`** — recipe for the
  source-distribution deliverable per §7.3 and **REQ R-8**. PECL
  packaging is best-effort (SHOULD-not-MUST per R-8); revisit if cost
  is reasonable.
- **`fpm-integration-test`** — actual PHP-FPM integration test
  (`fpm_repeated.php` from §9.2): 100 requests on one FPM worker,
  assert no per-request RSS growth and each trace has a fresh
  `trace_id`. Currently every recorder/shipper integration test runs
  under PHP CLI. Closes **AC-BS-3** and **AC-PB-1** as binding
  evidence rather than design-only mitigation.

### P2 — Quality / hygiene follow-ups (non-blocking)

Cosmetic and code-clarity cleanups carried forward from prior review
rounds. None are spec gates; none block v1.0. Group-land when the
file they touch is open for another reason.

#### Configuration / bootstrap

- `cleanup-config-error-alias` — delete the unused
  `pub type ConfigError = ConfigWarning;` alias (R-6 of Phase 1
  review).
- `phpinfo-header-uses-underscore` — rename so the `phpinfo()`
  section header reads `php_analyze` not `php-analyze` (R-9).
- `single-source-trim` — collapse the defensive double-trim between
  bootstrap and config (R-11).
- `minfo-catch-unwind` — wrap `bootstrap::minfo`'s body in
  `catch_unwind` to match the contract that
  `bootstrap-startup-panic-safety` established for the other four
  lifecycle hooks. `minfo` runs only in response to operator-driven
  `phpinfo()` / `php --ri`, so a panic there is operator-visible
  but does not abort a running PHP process — lower priority, but
  consistent treatment is desirable. (Q-1 of
  `bootstrap-startup-panic-safety`'s `design.md`.)
- `shipper-spawn-failure-in-disable-reason` — surface
  `SHIPPER_SPAWN_FAILED` via a new `DisableReason::ShipperSpawnFailed`
  so `phpinfo()` / `php --ri` reflects the failed state alongside
  the E_WARNING in the error log. Requires widening `Config` from
  "set once at MINIT, immutable" to "set at MINIT, narrowable at
  RINIT", which is invasive. Deferred unless operators ask for the
  MINFO surface. (Q-2 of
  `bootstrap-startup-panic-safety`'s `design.md`.)

#### Phase-0 spike tidy-up

- `spike-graceful-degrade-on-missing-config` — promote
  `inactive_sink()` to `SpikeObserver::inactive()` and replace
  `build_spike_observer`'s `expect` on `Config::global()` with a
  `let-else` returning the inactive observer (S-4).
- `spike-tidy-fqn-and-deadcode` — `fqn` unreachable `unwrap_or`,
  `with_sink` doc clarification, `LocalFcallInfo::empty()`
  null-`func` notice (S-5, S-12, S-14).
- `spike-tighten-integration-assertions` — tighten `assert_pair`
  to `entry_hits == 1 && exit_hits == 1` per fixture and assert
  `array_map` callback fires 3× (S-10).
- `spike-portable-run-sh` — replace `python3` JSON parse with
  shell-only `${CARGO_TARGET_DIR:-…}` (S-11).
- `spike-log-path-validate-absolute` — reject non-absolute
  `spike_log_path` with `ConfigWarning::SpikeLogPathNotAbsolute`
  (S-8).
- `spike-doc-cleanup` — `// NOTE for Phase 5` near the per-call
  allocations, soften / cite the `should_observe` caching claim
  (S-9, S-13).

#### Recorder follow-ups

- `recorder-clock-ordering` — flip CPU/wall capture order inside the
  snapshot constructors so the CPU window strictly nests inside the
  wall window (RO-7).
- `recorder-cache-hostname` — cache `gethostname` once in an
  `OnceLock<Arc<str>>` populated at MINIT instead of running the
  syscall every RINIT (RO-8).
- `recorder-portable-c-char` — use `libc::c_char` instead of
  hard-coded `i8` for the hostname buffer cast (RO-9).
- `recorder-bootobserver-disabled-doc` — inline comment explaining
  why `BootObserver::Disabled`'s empty `begin`/`end` arms are
  reachable only on the first-per-function fire (RO-10).
- `recorder-driver-build-once` — move
  `cargo build --features recorder-dump` out of the per-fixture
  `run.sh` invocation; build once before the fixture loop (RO-11).
- `recorder-dump-loud-failure-in-tests` — replace `eprintln!` in
  `recorder::dump::write_trace_if_path_set` with `panic!` under
  `cfg(test)` so silent dump-file write failures surface (RO-12).
- `recorder-style-cleanups` — name-consistency of
  `_execute_data`/`_retval` underscore-prefixed params; swap `{:?}`
  for `{}` on `PathBuf` in dump error message (RO-13, RO-14).
- `accounting-saturating-sub` — switch `accounting::sub` to
  `fetch_update(|cur| Some(cur.saturating_sub(bytes)))` so a future
  double-sub becomes a saturated no-op (DCR-3). Worth landing before
  any new sub site is added.
- `recorder-test-lock-hygiene` — add "no `account_guard()` needed:
  bills zero" comments to the four named tests, or short-circuit
  `accounting::sub` when `bytes == 0` (DCR-4).
- `recorder-dump-test-lock` — per-module `DUMP_PATH_TEST_LOCK:
  Mutex<()>` so the four `dump::tests` that mutate the
  `PHP_ANALYZE_DUMP_PATH` env var don't race (DCR-5).
- `recorder-test-helper-no-leak` — reshape `cat_for` into
  `with_cat(name, |cat| { ... })` so the boxed `RawCallSite` lives on
  the test's stack frame instead of leaking (DCR-6).
- `flush-predicate-trigger-method-placement` — move
  `flush_predicate_trigger` to `Trace::flush_predicate_trigger`
  (PF-9). Cosmetic.

#### Stub-ingest tidy

- `stub-ingest-spawn-timeout` — wrap `bound:` / `ready` consumption
  in a background thread + `mpsc::sync_channel(1).recv_timeout(10s)`
  (WSI-5).
- `stub-ingest-strip-prefix-bearer` — collapse `validate_bearer`'s
  `starts_with` + slice into a `let-else strip_prefix` (WSI-6).
- `stub-ingest-lazy-method-name` — inline or lazy-wrap
  `request_method_name(&request)` so the happy paths don't allocate
  (WSI-7).
- `stub-ingest-compact-json` — switch `handle_debug_batches` to
  `to_vec` (or record the pretty-print decision in `design.md`)
  (WSI-8).
- `stub-ingest-dispatch-borrow` — split `dispatch()` so `path` and
  `method` are owned only on the 405/404 paths (WSI-10).

#### Shipper tidy

- `shipper-exit-as-enum` — model the exit as
  `enum ShipperExit { Drained { batches }, DeadlinePassed { drained,
  abandoned } }` so the implicit `drain_completed ⇔ abandoned == 0`
  invariant becomes unrepresentable.
- `shipper-thread-name-fits-task-comm` — rename the shipper thread
  to fit `TASK_COMM_LEN = 16` (e.g. `pa-shipper`); currently
  truncated to `php-analyze-shi` in `top -H` / `ps -L`.
- `shipper-substrate-tidy` — group test-surface accessors under a
  `test_support` submodule or a single `TestProbe` struct; factor
  repeated `unwrap_or_else(|e| e.into_inner())`; reword `run_loop`
  doc's forward-pointing prose.
- `shipper-collapse-drain-phases` — collapse the pre-drain /
  post-drain split into a single deadline-aware loop body now that
  every code path has an `Option<Instant>` deadline available
  (Q-1 of `shipper-deadline-at-recv-loop-head`'s `design.md`).
- `shipper-drop-counter-attribution` — split the source trace's
  `drop_counter` into `retry_dropped` / `encode_dropped` /
  `deadline_dropped` (or surface a new
  `meta.drop_reasons: BTreeMap<...>`) so a downstream operator
  inspecting `meta.dropped_records` can attribute a spike to the
  right cause. The slice-3 implementation folds every
  `OnBatchOutcome::Dropped` path into the single existing counter
  (a deliberate SEH-5 deviation, documented in code); OBJ-5 ("no
  silent loss") still holds, but the attribution is lost. Requires
  a wire-format bump if surfaced via `meta.*`.

---

## 3. Forward-looking design decisions

These are load-bearing architectural notes that subsequent work must
respect. They either capture a spec deviation (where the
implementation diverges from the prose) or document a decision the
spec leaves underspecified.

### C-5 — Zend observer viability and PHP 8.4 coverage

Output of the `spike-zend-observer` change. The `observer` feature on
`ext-php-rs = "=0.15.13"` activates the public `FcallObserver` /
`FcallInfo` / `ModuleBuilder::fcall_observer` surface. No raw FFI is
needed; `Config::global()` is populated before upstream
`observer_startup()` runs because our user `startup` shim is the
`module_startup_func`, invoked first by the `#[php_module]` macro's
auto-generated `ext_php_rs_startup`.

**Coverage table (PHP 8.4.21 cli):**

| Category | Fixture | Observed `entry:`? | Observed `exit:`? | `abnormal_exit` correct? |
| --- | --- | --- | --- | --- |
| Top-level user function | `only_me()` in `user_calls.php` | yes (`function:<file>:15:only_me`) | yes | yes (false on normal return) |
| User method | `(new C)->m()` in `user_calls.php` | yes (`method:C::m`) | yes | yes (false) |
| User closure | `$closure()` in `user_calls.php` | yes (`closure:<file>:21`) | yes | yes (false) |
| Internal — `array_map` | `internal_calls.php` | yes (`internal:array_map`) | yes | yes (false) |
| Internal — `json_encode` | `internal_calls.php` | yes (`internal:json_encode`) | yes | yes (false) |
| Internal — `preg_match` | `internal_calls.php` | yes (`internal:preg_match`) | yes | yes (false) |
| Internal — `strlen("hi")` | `internal_calls.php` | **no** (PHP 8.x opcode-specialises constant-arg `strlen` away) | no | n/a |
| Internal — `__construct` of `RuntimeException` | `throws.php` | yes (`internal:__construct`) | yes | yes (false; exception is set AFTER the constructor returns) |
| Throwing user function | `bad()` in `throws.php` | yes (`function:<file>:13:bad`) | yes | yes (**true** on unwind) |
| Top-level script body | every fixture | yes, as `closure:<file>:1` | yes | yes (false; or true if uncaught top-level throw — not exercised) |

Three further structural findings worth carrying forward:

1. The `array_map` callback (an arrow function) fires its
   `closure:` pair **once per element** — three times for
   `[1, 2, 3]`. This is exactly the per-call coverage the
   Recorder needs; no special handling required for higher-order
   internals.
2. The top-level script body is reported as `closure:<file>:1`.
   This is the natural place for any `RINIT`-allocation to anchor
   if an "entry to the request" marker is ever needed.
3. `RuntimeException`'s constructor is observed as
   `internal:__construct`; the `abnormal=false` reading on its
   exit confirms the order — Zend writes `EG(exception)` only
   *after* the constructor returns, so a peek at `has_exception()`
   inside the constructor's `end` handler correctly reads `false`.
   (The `bad()` function's own exit then reads `true`.)

The `strlen` opcode-specialisation finding is recorded in the spec
scenario `PHP-specialised internals are NOT observed` so the
Recorder inherits the known limitation cleanly.

### C-7 — PHP 8.3 verification outcome

Closes the C-5 follow-up "Recorder MUST include 8.3 verification".
Slice 2 (`recorder-observer-hooks-and-trace-lifecycle`) adds an
integration test (`crates/php-analyze/tests/recorder_observer.rs`)
and a shell harness (`tests/php-recorder/run.sh`) that iterate every
`php8.3` / `php8.4` binary on `PATH`, build the cdylib with
`--features recorder-dump`, and assert per-fixture contents.

**Host coverage:**

| PHP version | Host outcome | Notes |
| --- | --- | --- |
| PHP 8.4.21 | **passed** | `flat_calls.php` (10⁴ records, 1 dict entry for `noop`), `nested.php` (a→b→c parent chain), and `throws.php` (`bad()` record carries `abnormal_exit=true`, script body's record carries `false`) all green. |
| PHP 8.3.x | **skipped on this host** | The local `update-alternatives` points at `/usr/bin/php-config8.4`, so the cdylib's module API (20240924) cannot load under PHP 8.3 (module API 20230831). The harness's `run.sh` detects this via the PHP startup warning and exits 77; the Rust test surfaces the per-binary skip with a clear stderr message. |

**CI coverage** closes the 8.3 gap: `.github/workflows/ci.yml` runs
the same harness once per matrix entry, with
`update-alternatives --set php-config /usr/bin/php-config${{ matrix.php }}`
ensuring each entry builds the cdylib for the corresponding PHP
version.

**R-2 verdict:** updated from "Closed for PHP 8.4; partially closed
for PHP 8.3 (pending verification)" to **"Closed for PHP 8.3 and PHP
8.4"**. The matching `SPECIFICATION.md` §11 R-2 status cell is
amended.

### C-8 — Exception unwind reads `ExecutorGlobals::has_exception()`, not an `end` parameter

`SPECIFICATION.md` §3.2 lists `EG(exception)` under "Interfaces
consumed" — correct at intent. The implementation reads
`ExecutorGlobals::has_exception()` (the ext-php-rs wrapper, a
one-liner that null-checks `EG(exception)`). This is the same
pattern the spike already validated in C-5's `throws.php` row.

An early proposal claimed `ext_php_rs = 0.15.13`'s `FcallObserver::end`
had an `abnormal: bool` parameter and that the recorder would read it
directly. That was wrong: the real trait signature is
`fn end(&self, execute_data: &ExecuteData, retval: Option<&Zval>)` —
no `abnormal` parameter. Slice-2 spec
(`specs/recorder-call-events/spec.md`) and design D-7 were amended
in-flight to reflect the actual API; this note records the deviation
so the spec/design archive reads coherent against the implementation.

**Evidence:** the C-5 coverage table proves
`ExecutorGlobals::has_exception()` reads `true` exactly when the
calling frame is unwinding via an exception. No further verification
is needed beyond the integration test's `throws.php` fixture.

### Architectural note — `FcallInfo<'a>` → `RawCallSite<'a>` indirection

The RO-4 fix forced a change to the categorise input type:
`ext_php_rs::zend::FcallInfo<'a>` carries `Option<&'a str>` fields,
leaving nowhere to store a lossy-decoded `String` with the right
lifetime. The recorder now owns a `RawCallSite<'a>` with
`Option<Cow<'a, str>>` fields plus an `execute_data_addr: usize` (for
RO-5's tiebreaker). The trait signatures still take `&FcallInfo` per
upstream's contract — this is the boundary at which our owned analogue
meets the upstream borrowed one. If upstream ever widens
`FcallInfo`'s string fields, `RawCallSite` can collapse back into a
thin adapter; until then, the local type is the correctness substrate.

### C-17 — Process-global drain-deadline cell, publish-before-send ordering

**Decision**: the MSHUTDOWN drain deadline is published to a
process-global `Mutex<Option<Instant>>` cell *before* the `Drain`
message is sent on the channel and *before* the canonical `Sender` is
dropped. The shipper's `run_loop` snapshots the cell at the head of
each pre-drain iteration; once observed as `Some(_)`, the loop
transitions to the deadline-aware recv body without waiting for the
`Drain` message to surface from a saturated channel.

**Why**: with the slice-3 default `shipper_queue_depth = 8`,
`retry_count = 3`, `http_timeout_ms = 2000`, a saturated channel
against a black-holed upstream would cost up to `8 × 4 × 2s = 64s`
before the `Drain` message reaches the front of the queue — far past
the AC-BS-4 / AC-PB-2 budget of `shutdown_grace_ms + 200ms`. The
deadline-aware retry orchestrator already does the right thing once
it has a deadline; the cell-publish-before-send ordering is what
makes the deadline visible to the orchestrator early enough to bound
in-flight work.

**Alternatives considered and rejected**:

- `OnceLock<Instant>` for the cell — cannot be cleared between tests;
  the existing `reset_for_test` pattern needs clearable state.
- `AtomicI64`-encoded-as-`Instant` for lock-free reads — adds a new
  "process-start anchor" abstraction and a new error mode for one
  uncontended mutex acquire per `recv` until first publish.
- A separate "control" channel selected via
  `crossbeam_channel::select!` — two channels, two clones at
  MSHUTDOWN, two state slots, same wake-up latency as recv_deadline.
- Extending `ShipperMessage` with a `Wake` variant — does not solve
  the underlying problem (the `Drain` message itself is the wake-up
  signal we cannot deliver in time).

**Resolution**: implemented in OpenSpec change
`shipper-deadline-at-recv-loop-head` (branch
`feat/shipper-deadline-at-recv-loop-head`). Spec parity:
`SPECIFICATION.md` §3.3 "observe a global deadline
`now + shutdown_grace_ms`" and AC-BS-4 / AC-PB-2 wording unchanged;
this change makes the existing wording self-consistent under per-batch
work without amending the spec text.

### C-18 — Pre-drain `drained_consume` passes `None` deadline; deadline cell is only observed *between* batches

**Surface**: `crates/php-analyze/src/shipper/mod.rs:316`. The
shipper's `run_loop` reads `drain_deadline_snapshot()` at the
head of each pre-drain iteration. Once observed `Some(_)`, the
loop transitions to `run_drain_phase` which passes the deadline
to every per-batch `drained_consume`. But while the shipper is
mid-`drained_consume` on a batch picked up in the pre-drain
phase, the call site is:

```rust
match rx.recv() {
    Ok(ShipperMessage::Batch(batch)) => {
        drained_consume(&batch, &mut on_batch, None, &mut batches_drained);
        //                                    ^^^^ pre-drain phase
```

The deadline is `None` for the entire `run_with_retry` loop on
that batch. If MSHUTDOWN publishes the deadline cell after the
shipper has dequeued the batch but before its retry loop
completes, the retry loop exhausts the full
`(retry_count + 1) × http_timeout_ms + cumulative_backoff`
budget against the slow upstream regardless of the deadline.

**Surfaced by**: attempting to land an `mshutdown_drain.php`
PHP-level integration test for AC-BS-4 / AC-PB-2 under
`stub-ingest-slow-mode`. With `php_analyze.shutdown_grace = 200`,
`http_timeout_ms = 200`, `retry_count = 5`, `retry_backoff_ms = 50`
against `--simulate-slow 5000`, observed PHP wall-clock = 2806ms
— within 50ms of the no-deadline-budget timeline of
`6 × 200ms + (50+100+200+400+800)ms = 2750ms`. Expected ≤ 700ms
if the deadline cell were honored mid-batch.

**Spec parity**: `openspec/specs/shipper/spec.md` lines around
the MSHUTDOWN-deadline requirement claim the contract holds
*"even when … every in-flight batch is mid-`run_with_retry`
against a black-holed upstream."* That clause is **spec-only,
not bound by any current test**: the in-crate
`SlowRecordingOnBatch` test pre-seeds the channel before
spawning the shipper (so the deadline cell is observed
between-batches, not mid-batch), and uses a 100ms-per-call
`OnBatch` impl that doesn't go through `run_with_retry` at all.
The PHP-level test under `stub-ingest-slow-mode` was the first
attempted binding and surfaced the gap.

**Fix sketch** (for `shipper-deadline-mid-retry`):
`drained_consume` (and `encode_and_handle` and
`run_with_retry`) accept a `deadline: Option<Instant>` already.
The pre-drain `run_loop` arm should compute the deadline from
the cell snapshot at the moment it picks up the batch and
thread it through:

```rust
Ok(ShipperMessage::Batch(batch)) => {
    let dl = drain_deadline_snapshot();
    drained_consume(&batch, &mut on_batch, dl, &mut batches_drained);
    drop(batch);
}
```

`drain_deadline_snapshot` is a cheap mutex acquire on
loopback-CPU. Snapshotting per batch (instead of per-attempt)
keeps the change minimal — the retry loop already polls `now()`
each iteration, so a deadline set after batch pickup gets
honored on the next attempt boundary. A deadline set
mid-attempt (during a `ureq` `send`) isn't honored, but
`http_timeout_ms` is the per-attempt ceiling, so the worst-case
slip is `http_timeout_ms` past the deadline — within the spec's
`shutdown_grace + 200ms` margin as long as `http_timeout_ms ≤ 200`.

**Stub-side `--simulate-slow` is the test seam** ready for
`shipper-deadline-mid-retry` to consume. The `mshutdown_drain.php`
fixture and the `try_round_trip_mshutdown_drain` helper drafted
during `stub-ingest-slow-mode` were intentionally not committed;
they will land in `shipper-deadline-mid-retry` once the
production fix is in.

**Closed by `shipper-deadline-mid-retry`** (branch
`feat/shipper-deadline-mid-retry`). The chosen mechanism was the
"closure-typed deadline" path (closure passed all the way into
`run_with_retry`, re-read per iteration). `RmpEncodeAndHttpPost::handle`
composes the caller-supplied `deadline: Option<Instant>` with a
fresh `drain_deadline_snapshot()` per call via a `sooner(a, b)`
helper (returns the `min` of two `Some` values, or whichever is
`Some`). The PHP-level binding test (`mshutdown_drain.php` +
`try_round_trip_mshutdown_drain`) lands alongside the production
fix; observed PHP wall-clock with the fix in place is ≈ 500ms
against a `< 2000ms` test budget (was 2806ms without the fix).

---

## 4. Repository hygiene

### N-1 — `.gitignore` excludes `openspec/`, `personas/`, `CLAUDE.md`

The current `.gitignore` deliberately keeps the workflow scaffolding
out of git history. That's a design choice this repo respects. If a
future change wants to track the OpenSpec deltas in git history (e.g.
to drive code review against the spec deltas), the `.gitignore` lines
`/openspec`, `/personas`, `CLAUDE.md` will need to be revisited.

---

## 5. Phase 5 anchor — AC-RC-5 zero-alloc audit harness

The `flush_into_pending_batch` accessor is zero-alloc by construction
(`mem::take` + three `Arc::clone` calls into the `MetaPartial` —
`host`, `sapi`, `uri_or_script`). The third `Arc::clone` (on
`uri_or_script`) was not zero-alloc as originally landed; the PF-1
round-1 fix lifted `Trace::uri_or_script` and
`RequestIdentity::uri_or_script` from `String` to `Arc<str>` so the
construction is now zero-alloc end-to-end.

The audit harness that pins this property — including the
`// NOTE for Phase 5` markers near the remaining dict-miss
`to_owned()` allocations in `begin_with_snapshots` — is the
`recorder-zero-alloc-audit` follow-up listed in §2 (P1 — Phase 5).
That follow-up's implementation will need either an arena/intern
table for the dict-miss path or a justification that the dict-miss
allocations are amortised across a trace and therefore acceptable
under AC-RC-5's "steady state" wording.
