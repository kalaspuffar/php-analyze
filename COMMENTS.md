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

### P1 — MVP closing pass (see §6 for the reframe)

The operator's stated priorities (§6.1) reshape "what's between
us and v1.0" significantly. The full original Phase-5 / Phase-6
plan is summarised below as **Archived** (already shipped) and
**Deferred** (revisit after MVP validation). The actual closing
pass for handoff is four small changes; each is named in §6.3
and reproduced here for the task-list view:

- **`docs-mvp-reframe`** — pure docs. Captures the MVP posture
  (this section + §6) and any residual `SPECIFICATION.md`
  wording adjustment that surfaces during review.
  ~half a day, no code.
- **`fpm-integration-test`** — closes `SPECIFICATION.md` §1.3
  #2 and AC-BS-3 with a real PHP-FPM run. Start fpm against a
  test pool, hit `fpm_repeated.php` 100 times via a FastCGI
  client, assert no crash + fresh `trace_id` per request +
  bounded RSS + 100 distinct traces in stub-ingest. ~1-2 days.
- **`xdebug-spot-check`** — one-shot accuracy validation, NOT
  a CI-gated comparator. Shell script under
  `tools/xdebug-spot-check/` produces a written report of
  call-coverage overlap + per-function `(t_in, t_out)` delta
  vs. Xdebug for a chosen representative fixture. Operator
  reads the report, decides whether the data is trustworthy
  enough for MVP. ~1 day.
- **`capture-reference-batches`** — `tools/capture-fixtures.sh`
  runs each canonical workload, captures the resulting
  MessagePack batches from `stub-ingest`, dumps them under
  `tools/captured-batches/<workload>/*.msgpack`. Gives the
  `../php-tree-visualizer` repo concrete test fixtures.
  Makes the wire-format-as-handoff-contract tangible.
  ~half a day.

After those four, the repo is in a "good handoff state" per
§6.2; the visualizer team can pick up the wire-format-side
work without blocking on us.

### P1 — Archived (already shipped before the §6 reframe)

These were the Phase-5 deliverables that landed under the
pre-reframe priorities. They remain useful — the recorder is
genuinely leaner — but their prioritisation was wrong in
hindsight; see §6.1.

- **`bench-criterion-skeleton`** (closed): scaffolded `criterion`,
  added the no-PHP per-call micro-bench (`recorder_hot_path`,
  ~178 ns observed) and the 10⁴-call workload-shape bench
  (`recorder_workload`).
- **`bench-canonical-workloads`** (closed): resolved **OQ-7** with
  three self-contained workloads (`flat_calls.php`,
  `json_batch.php`, `recursive_walk.php`) and the
  `workload_overhead` bench. The bench's `≤ 2.0×` assertion
  is structurally unreachable on `flat_calls.php` (see C-19);
  the workload-set discussion is deferred under §6.4 as
  `bench-canonical-workloads-revisit`.
- **`recorder-zero-alloc-audit`** (closed): the `CountingAllocator`
  audit harness binds AC-RC-5 in steady state for both the
  bench-seam path and the production-path through
  `categorise_lazy → begin_with_snapshots_lazy →
  end_with_snapshots`. Widened by `recorder-hot-path-tuning`.
- **`recorder-hot-path-tuning`** (closed): every allocation-side
  optimisation the design promised landed.
  `categorise_lazy() → begin_with_snapshots_lazy()` is the new
  production hot path; the dict-hit branch performs **zero
  heap allocations**. `Dictionary::intern_ref()` (hashbrown
  raw-entry borrow probe) replaces the previous `contains_key`
  + `intern(key.clone(), …)` pair; `FqnSpec::render()` defers
  fqn rendering to the miss branch only. Observed
  `workload_overhead` geo-mean improved 10.28× → 9.08×
  (-12%); in-process `recorder_workload` improved -23%
  (185 ns/call → 135 ns/call).
- **`recorder-cpu-snapshot-cadence`** (closed): adds the
  `php_analyze.cpu_snapshot_mode` directive with `per-call`
  (default; spec-current) and `off` (skip per-call `getrusage`,
  emit `cpu_u_ns = cpu_s_ns = 0`). Saves ~1000 ns/call under
  `off`. Observed geo-mean drops from 9.88× → **5.11×** under
  `off` (-48%); `json_batch` falls to **1.87×**, individually
  under budget. Spec amendments to `SPECIFICATION.md` §3.2 /
  §3.5 / §11 R-11 land alongside.

### P1 — Deferred (revisit after MVP validation; see §6.4)

The following items were on the path to v1.0 before the §6
reframe. They remain useful but are not MVP-blockers under
the operator's restated priorities. Pick up after MVP
validation if/when the specific concern they address actually
matters in practice.

- **`spec-perf-budget-revision`** — re-frame NFR-PERF-1 to
  match observed reality once we know whether perf matters in
  Sam's actual workload.
- **`bench-canonical-workloads-revisit`** — replace the
  adversarial `flat_calls.php` with realistic operator
  workloads (Symfony hello-world, WordPress homepage, etc.).
- **Full Xdebug comparator harness (CI-gated)** — only if
  v1→v2 risks accuracy regression. For v1 MVP, the spot-check
  in `xdebug-spot-check` is sufficient evidence.
- **`recorder-zero-alloc-audit` dict-miss closure** — eliminate
  the remaining `to_owned()` allocations on first-sight calls
  via an arena / intern table. Cost-incidental; current audit
  binds the steady-state hit path which is the
  operator-relevant case.
- **Phase 6 hardening** — `token-leak-grep-test`,
  `tls-system-ca-integration-test`, `cargo-audit-in-ci`,
  `source-distribution-tarball`, PECL packaging. Production
  hardening; none MVP-blocking. The token-leak grep would
  pair with `stub-ingest-configurable-failure` which already
  exercises the `E_NOTICE` drop-line path; the TLS test
  closes **AC-SH-5**; cargo audit is per §9.6. Pick up if/when
  MVP validation surfaces a specific need.
- **`lock-readme-directive-table`** — parsing test that walks
  the README directive table and matches against `DIRECTIVES`'s
  defaults / ranges. Low priority; drift would surface as an
  `OutOfRange` warning at MINIT.

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

### C-19 — `NFR-PERF-1` geo-mean budget unreachable under spec-mandated per-call `getrusage`

> **Read §6 first.** This note was written under the
> pre-reframe priorities where `NFR-PERF-1`'s 2.0× geo-mean
> looked load-bearing. The operator subsequently clarified
> (§6.1) that the 2.0× number was a vibe estimate, not a
> constraint, and that MVP validation outranks the perf gate.
> The analysis below remains technically correct as a
> description of the recorder's syscall floor; its
> **prioritisation conclusions** ("close the gap with one of
> three follow-ups") are superseded by §6.4 which defers all
> three of them.

**Status**: `workload_overhead` geo-mean is **5.11×** on the
reference host after `recorder-hot-path-tuning` +
`recorder-cpu-snapshot-cadence` under `off` mode (or **9.08×**
under `per-call`). The pre-reframe budget was **≤ 2.0×**. The
gap is structural and documented here for historical reference.

**Surface**: `crates/php-analyze/src/clocks.rs::cpu_times_now_ns`
plus the two `EntrySnapshots::capture_now` / `ExitSnapshots::capture_now`
call sites in `crates/php-analyze/src/recorder/observer.rs`.

**Diagnosis**: a microbench on the reference host
(developer workstation, kernel 6.x, x86_64) reports:

| Syscall | Cost |
| --- | --- |
| `clock_gettime(CLOCK_MONOTONIC)` | ~45–56 ns (vDSO-fast) |
| `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` | ~458–526 ns (real syscall) |
| `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` | ~501 ns (real syscall) |
| `getrusage(RUSAGE_THREAD)` | ~436–503 ns (real syscall) |

The recorder snapshot per begin/end currently invokes:
1 × `clock_gettime(CLOCK_MONOTONIC)` (~50 ns), 1 ×
`getrusage(RUSAGE_THREAD)` (~500 ns), 1 ×
`zend_memory_usage(true)` (~10–30 ns). Per call (begin + end):
**~1100 ns just for syscalls**.

`flat_calls.php` unprofiled baseline on this host is ~60 ns/call.
The 2.0× budget allows ~60 ns of additional overhead per call.
The syscall floor alone is **~18×** that budget. Hitting 2.0× on
`flat_calls.php` is mathematically impossible without changing
either (a) the workload, (b) the per-call CPU snapshot strategy,
or (c) the budget interpretation.

**What the `recorder-hot-path-tuning` change DID accomplish**:

- Killed every Arc/String allocation on the production dict-hit
  branch (bound by the widened zero-alloc audit).
- Halved the dictionary-traversal count via
  `Dictionary::intern_ref()` (hashbrown raw-entry).
- Deferred method/closure `fqn` rendering to the miss path only.
- Improved `recorder_workload` (the in-process bench) by ~23%
  (185 ns → 135 ns per call).
- Improved `json_batch` to **under 2.0×** individually (2.42 →
  1.94).
- Improved geo-mean by ~12% (10.28× → 9.08×).

The remaining cost is the syscalls. Every allocation-side win
the design promised is on the table.

**Spec parity**: `SPECIFICATION.md` §3.2 explicitly requires
per-call CPU snapshots via `getrusage(RUSAGE_THREAD)`. R-11
("`getrusage` granularity is coarser than `t_in`/`t_out`
resolution → CPU times look quantized") accepts the *coarseness*
of the read (sub-microsecond calls reading `cpu_u/s_ns == 0`) but
does **not** authorise skipping the read entirely or amortising
it across calls. To honour the budget, the spec needs to grow
either:

- An "approximate CPU mode" toggle defaulting on for high-volume
  workloads; or
- A coarser-cadence CPU snapshot strategy (e.g., snapshot at
  most every `K` calls, attribute the delta proportionally to
  the calls in between, or drop the per-call CPU attribution
  entirely for sub-µs functions); or
- A revised budget that scales with the unprofiled per-call
  cost (e.g., "≤ 2.0× when per-call baseline ≥ 1 µs;
  ≤ syscall-floor when shorter").

These are all spec/requirements-level decisions, not
implementation tuning. They belong in a follow-up OpenSpec
change (`recorder-cpu-snapshot-cadence` or
`spec-perf-budget-revision`), authored after the operator
weighs the trade-off between CPU accuracy and per-call
overhead.

**Proposed follow-ups** (queued for §2 P1 once authored):

- `recorder-cpu-snapshot-cadence` (in progress on
  `feat/recorder-cpu-snapshot-cadence`) — ships the
  `php_analyze.cpu_snapshot_mode` directive with two values:
  `per-call` (default; spec-current) and `off` (skip the
  `getrusage` syscall, emit `cpu_u_ns = cpu_s_ns = 0`).
  **Observed on the reference dev host**: geo-mean **9.88×
  → 5.11×** under `off` (flat_calls 33.09× → 12.38×,
  json_batch 2.30× → **1.87× under budget**, recursive_walk
  12.65× → 5.77×). Half the recorder's overhead on this host
  was the per-call getrusage syscall — matches the C-19
  hypothesis closely. The `coarse` mode (sampled CPU
  amortisation) is deferred to a follow-up; the all-or-nothing
  trade-off was the smallest sufficient design surface for v1.
  Spec amendments to §3.2 / §3.5 / R-11 + README directive
  table land in the same change.
- `bench-canonical-workloads-revisit` — re-evaluate whether
  `flat_calls.php` (10⁶ noop calls) is a meaningful canonical
  workload given that no realistic PHP application looks like
  it. The honest answer per OQ-7's deferred resolution is that
  the canonical set should reflect the **operator's** real
  workload, not a worst-case adversarial micro-benchmark; a
  conversation with the operator is the next step.
- `spec-perf-budget-revision` — surface this gap in
  `SPECIFICATION.md` §8.1 NFR-PERF-1 and propose the revised
  budget shape.

**Why this change still ships**: every promised optimisation
landed. The geo-mean improved by 12% with no regressions and a
strengthened zero-alloc contract. The remaining gap requires
work outside this change's scope; landing the allocation
improvements now keeps `main` strictly closer to the budget and
unblocks the spec discussion with concrete numbers.

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

---

## 6. MVP handoff posture

### 6.1 The reframe (operator-stated priorities)

Two changes (`recorder-hot-path-tuning`, `recorder-cpu-snapshot-cadence`)
spent significant effort chasing `NFR-PERF-1`'s ≤ 2.0× geo-mean
budget. During the post-archive conversation the operator
clarified the actual priorities:

1. **`NFR-PERF-1`'s 2.0× was a vibe estimate, not a constraint.**
   Of the architect's offered `{2, 5, 10}` shortlist, `2` was
   picked as "the most ambitious that didn't feel silly." It
   wasn't derived from a measured operator pain point. The number
   is documented in `REQUIREMENTS.md` §93 / §284 and reproduced
   into `SPECIFICATION.md` §1.3 #3, but it carries less weight
   than the source documents make it look.

2. **Accuracy outranks speed.** If the recorder produces
   misleading reports, that's worse than slow ones. A 5× slow
   request that tells the truth is more useful than a 1.5× fast
   request that miscounts calls.

3. **MVP-validate first; tune later.** Before we know whether
   perf even matters in practice, we need to know whether the
   tool is useful at all — i.e., whether someone can point
   `php-analyze` at a real workload, look at the resulting
   trace, and form a useful opinion.

This reframe **does not invalidate** the two perf changes — the
zero-alloc audit, the hashbrown borrow-keyed probe, the
`cpu_snapshot_mode` directive are all real improvements that
make the recorder leaner. But it **does invalidate** the
prioritisation that put them ahead of the
accuracy-and-deployment-shape work the MVP actually depends on.
Self-correction recorded so the next prioritisation conversation
starts from the operator's priorities, not the spec's.

### 6.2 System architecture (where this repo ends)

The operator's design separates the data-producing extension
from a downstream visualization stack:

```
┌─────────────────────────────────────────────────────────────────┐
│                THIS REPO'S RESPONSIBILITY                       │
│  ┌───────────────────────────────────────────┐                  │
│  │  php-analyze (Rust cdylib in PHP)         │                  │
│  │  - Records calls via zend_observer        │                  │
│  │  - Buffers + batches                      │                  │
│  │  - Ships batches over HTTPS               │                  │
│  └───────────────────────────────────────────┘                  │
│                        │                                        │
│                        │  HTTP POST                             │
│                        │  application/vnd.php-analyze.v1+msgpack│
│                        │  ◀── THE HANDOFF LINE ──▶              │
└────────────────────────┼────────────────────────────────────────┘
                         │
┌────────────────────────┼────────────────────────────────────────┐
│                        ▼   ../php-tree-visualizer (separate)    │
│  ┌────────────────────┐    ┌──────────────┐    ┌─────────────┐  │
│  │ HTTP collector svc │───▶│  Ingester    │───▶│ Visualizer  │  │
│  │ Sinks to           │    │  Source maps │    │ Web app     │  │
│  │ mem/disk/S3        │    │  DB writer   │    │ Tree render │  │
│  └────────────────────┘    └──────────────┘    └─────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

The line is **the wire format + HTTP contract**
(`SPECIFICATION.md` §4.2 + §5.2). As long as this repo emits
batches that conform, the visualizer side is unblocked.
Everything downstream — durable storage, source maps, DB
schema, web UI — is `../php-tree-visualizer`'s problem.

That makes the "stable handoff state" question crisp:

> **This repo is in a good handoff state when it reliably
> produces wire-format-correct batches in both supported
> SAPIs.**

The acceptance criteria from `SPECIFICATION.md` §1.3 sort into
this framing as follows:

| §1.3 criterion | Concerns the handoff line? | MVP-blocker? |
| --- | --- | --- |
| #1 builds for 8.3 + 8.4 | yes | ✓ done |
| #2 CLI + FPM SAPIs | yes | ✗ FPM unverified — see §6.3 |
| #3a emits §4 batches | yes (the format) | ✓ done |
| #3b ≥99.5% coverage vs Xdebug | yes (data quality) | ✗ unbound — see §6.3 |
| #3c ±5% timing vs Xdebug | yes (data quality) | ✗ unbound — see §6.3 |
| #3d geo-mean ≤ 2.0× | no (downstream perf concern) | not for MVP |
| #4 network failure | yes (the HTTP contract) | ✓ done |
| #5 exception unwind | yes (data quality) | ✓ done |
| #6 depth overflow | yes (data quality) | ✓ done |
| #7 CI gates | meta | ✓ done |
| #8 README directives | yes (operator-facing) | ✓-ish — see below |

### 6.3 Closing pass (the MVP-shipping work)

Four small changes, in order. Together they leave this repo in
a clean handoff state. Each is sized to be a few days at most.

1. **`docs-mvp-reframe`** — this section + the §2 P1
   reorganisation below + a small `SPECIFICATION.md` note
   pointing readers at §6 for the MVP posture. Pure docs; no
   code. **Largely captured by this commit**; the OpenSpec
   change that follows is whatever residual SPECIFICATION.md
   wording adjustment surfaces during review.
2. **`fpm-integration-test`** — closes §1.3 #2 and AC-BS-3 with
   a real FPM run. Pattern: start php-fpm against a test pool,
   hit `fpm_repeated.php` 100 times via a FastCGI client,
   assert no crash + fresh `trace_id` per request + bounded
   RSS + 100 distinct traces received by stub-ingest.
3. **`xdebug-spot-check`** — NOT a CI-gated comparator
   harness. A one-shot shell script under
   `tools/xdebug-spot-check/` that runs `recursive_walk.php`
   (or a chosen representative fixture) under Xdebug, then
   under `php-analyze`, computes the call-set overlap and a
   per-function `(t_in, t_out)` delta histogram, writes the
   findings to `tools/xdebug-spot-check/REPORT.md`. The
   operator reads the report once and decides whether the
   data is trustworthy enough to hand to `../php-tree-visualizer`.
4. **`capture-reference-batches`** — a tiny
   `tools/capture-fixtures.sh` that runs each canonical
   workload, captures the resulting MessagePack batches from
   `stub-ingest`, dumps them under
   `tools/captured-batches/<workload>/*.msgpack`, commits the
   captures. Gives the visualizer repo concrete test
   fixtures: "here's what real batches look like, parse these
   in your tests." Makes the wire-format-as-handoff-contract
   tangible.

After those four land, the `SPECIFICATION.md` §1.3 acceptance
criteria look like:

```
1. Builds from source        ✓
2. CLI + FPM loadable        ✓ (after fpm-integration-test)
3a. Emits §4 batches         ✓
3b. ≥99.5% coverage          spot-checked (xdebug-spot-check)
3c. ±5% timing vs Xdebug     spot-checked (xdebug-spot-check)
3d. Geo-mean ≤ 2.0×          DEFERRED — not load-bearing for
                              MVP per §6.1
4. Network failure handling  ✓
5. Exception unwind          ✓
6. Depth overflow            ✓
7. CI gates                  ✓
8. README directives         ✓-ish

→ MVP shippable. Hand to ../php-tree-visualizer. Validate.
```

### 6.4 Deferred work (look at again after MVP validation)

The following changes were on the path to v1.0 before the
reframe. They remain useful but are not MVP-blockers under
the operator's restated priorities. Decide whether to pick
them up only after MVP validation tells us whether the
specific concern they address actually matters in practice.

| Change | Concern | Why deferred |
| --- | --- | --- |
| `spec-perf-budget-revision` | NFR-PERF-1 wording matches reality | Wait until MVP tells us whether perf matters in operator's actual workload. |
| `bench-canonical-workloads-revisit` | Replace adversarial `flat_calls.php` with realistic fixtures | Same. Don't pre-judge the workload set before someone has tried the tool on a real workload. |
| Full Xdebug comparator harness (CI-gated) | Continuous accuracy verification on every commit | Build only if v1→v2 risks accuracy regression. For v1, the spot-check report is sufficient evidence. |
| `recorder-zero-alloc-audit` dict-miss closure | Eliminate the remaining `to_owned()` allocations on first-sight calls | Cost-incidental; current audit binds the steady-state hit path which is the operator-relevant case. |
| Phase 6 hardening (`token-leak-grep-test`, `tls-system-ca-integration-test`, `cargo-audit-in-ci`, `source-distribution-tarball`, PECL packaging) | Production hardening | Useful, none MVP-blocking. Pick up if/when MVP validation surfaces a specific need (e.g., an operator complaint, a security review). |
| `lock-readme-directive-table` | Parsing test guards README drift against `DIRECTIVES` | Low priority. Drift would surface as an `OutOfRange` warning at MINIT and operators would catch it. |
| Quality / hygiene cleanups under §2 P2 | Cosmetic | As-encountered, when a file is open for another reason. |

### 6.5 What the visualizer team needs from us at the handoff line

For the team working in `../php-tree-visualizer` (or whoever
consumes our batches), the contract surface is small and
already mostly documented elsewhere:

- **Wire format**: `SPECIFICATION.md` §4.2. MessagePack with a
  three-key top-level map (`meta`, `dict`, `calls`).
  Schema-versioned (`meta.schema_version == 1` for v1).
- **Media type**: `application/vnd.php-analyze.v1+msgpack` (OQ-2).
- **HTTP contract**: `SPECIFICATION.md` §5.2. POST with bearer
  token, JSON-free; 2xx = accepted, anything else = retry-or-drop.
- **Reference implementation of a collector**: `crates/stub-ingest/`.
  Read it as documentation of what a minimal compliant collector
  looks like; it is **not** a production-ready collector
  (in-memory only, no durability, no source maps, no DB).
- **Reference batches** (after `capture-reference-batches`):
  `tools/captured-batches/*/*.msgpack`. Real batches from each
  canonical workload, suitable as test fixtures for the
  visualizer's own tests.

What the visualizer team needs to decide on their side (out of
scope for this repo):
- Durability strategy (mem / disk / S3).
- Source-map storage (how do they know what file `fn_id=7`
  came from when the source has changed since the trace ran?).
- DB schema for ingester output.
- Web UI for tree rendering.
- Authentication / multi-tenant model (this repo emits a
  static bearer token; the visualizer side may want
  per-pool tokens, OIDC, etc.).

