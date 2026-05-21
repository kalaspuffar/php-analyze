# COMMENTS

This file accumulates clarifications, review notes, and out-of-scope
discoveries that supplement `SPECIFICATION.md`. If a statement here
conflicts with `SPECIFICATION.md`, this file is the more recent
clarification.

## Open blockers

### B-2 — `git push` blocked on remote auth

**Status**: blocks remote-push of every branch this host creates.

**Cause**: this build host has no SSH key registered with
`git@github.com:kalaspuffar/php-analyze.git`. `git push -u origin
<branch>` fails with `Permission denied (publickey)`.

**To unblock**: push from a workstation that has push credentials.
Any new feature branch created on this build host needs the user's
workstation to push before the PR can be opened.

## Forward-looking design decisions

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
2. The top-level script body is reported as
   `closure:<file>:1`. This is the natural place for any
   `RINIT`-allocation to anchor if an "entry to the request"
   marker is ever needed.
3. `RuntimeException`'s constructor is observed as
   `internal:__construct`; the `abnormal=false` reading on its
   exit confirms the order — Zend writes `EG(exception)` only
   *after* the constructor returns, so a peek at
   `has_exception()` inside the constructor's `end` handler
   correctly reads `false`. (The `bad()` function's own exit then
   reads `true`.)

The `strlen` opcode-specialisation finding is recorded in the
spec scenario `PHP-specialised internals are NOT observed` so the
Recorder inherits the known limitation cleanly.

### C-7 — PHP 8.3 verification outcome

Closes the C-5 follow-up "Recorder MUST include 8.3
verification". Slice 2 (`recorder-observer-hooks-and-trace-lifecycle`)
adds an integration test (`crates/php-analyze/tests/recorder_observer.rs`)
and a shell harness (`tests/php-recorder/run.sh`) that iterate every
`php8.3` / `php8.4` binary on `PATH`, build the cdylib with
`--features recorder-dump`, and assert per-fixture contents.

**Host coverage:**

| PHP version | Host outcome | Notes |
| --- | --- | --- |
| PHP 8.4.21 | **passed** | Both `flat_calls.php` (10⁴ records, 1 dict entry for `noop`), `nested.php` (a→b→c parent chain), and `throws.php` (`bad()` record carries `abnormal_exit=true`, script body's record carries `false`) all green. |
| PHP 8.3.x | **skipped on this host** | The local `update-alternatives` points at `/usr/bin/php-config8.4`, so the cdylib's module API (20240924) cannot load under PHP 8.3 (module API 20230831). The harness's `run.sh` detects this via the PHP startup warning and exits 77; the Rust test surfaces the per-binary skip with a clear stderr message. |

**CI coverage** closes the 8.3 gap: `.github/workflows/ci.yml` runs the
same harness once per matrix entry, with `update-alternatives --set
php-config /usr/bin/php-config${{ matrix.php }}` ensuring each entry
builds the cdylib for the corresponding PHP version. The slice-2 PR's
CI run is the binding evidence — the 8.3 job and the 8.4 job both
execute the same three fixtures against their matching PHP runtime.

**R-2 verdict:** updated from "Closed for PHP 8.4; partially closed
for PHP 8.3 (pending verification)" to **"Closed for PHP 8.3 and PHP
8.4"**. The matching `SPECIFICATION.md` §11 R-2 status cell is
amended in the same change.

### C-17 — Process-global drain-deadline cell, publish-before-send ordering

**Decision**: the MSHUTDOWN drain deadline is published to a
process-global `Mutex<Option<Instant>>` cell *before* the `Drain`
message is sent on the channel and *before* the canonical `Sender` is
dropped. The shipper's `run_loop` snapshots the cell at the head of
each pre-drain iteration; once observed as `Some(_)`, the loop
transitions to the deadline-aware recv body without waiting for the
`Drain` message to surface from a saturated channel.

**Why**: slice 3 (`shipper-encoder-and-http`) wired real per-batch
work (MessagePack encode + HTTPS POST + up to `retry_count + 1`
attempts with exponential backoff). With the slice-3 default
`shipper_queue_depth = 8`, `retry_count = 3`, `http_timeout_ms = 2000`,
a saturated channel against a black-holed upstream would cost up to
`8 × 4 × 2s = 64s` before the `Drain` message reaches the front of the
queue — far past the AC-BS-4 / AC-PB-2 budget of
`shutdown_grace_ms + 200ms`. The slice-3 deadline-aware retry
orchestrator already does the right thing once it has a deadline; the
cell-publish-before-send ordering is what makes the deadline visible
to the orchestrator early enough to bound the in-flight work.

**Alternatives considered and rejected**:

- `OnceLock<Instant>` for the cell — cannot be cleared between
  tests; the existing `reset_for_test` pattern needs clearable state.
- `AtomicI64`-encoded-as-`Instant` for lock-free reads — adds a new
  "process-start anchor" abstraction and a new error mode for one
  uncontended mutex acquire per `recv` until first publish.
- A separate "control" channel selected via `crossbeam_channel::select!` —
  two channels, two clones at MSHUTDOWN, two state slots, same wake-up
  latency as recv_deadline.
- Extending `ShipperMessage` with a `Wake` variant — does not solve
  the underlying problem (the `Drain` message itself is the wake-up
  signal we cannot deliver in time).

**Resolution**: implemented in OpenSpec change
`shipper-deadline-at-recv-loop-head` (branch
`feat/shipper-deadline-at-recv-loop-head`). Closes SEH-9. Spec
parity: `SPECIFICATION.md` §3.3 step "observe a global deadline
`now + shutdown_grace_ms`" and AC-BS-4 / AC-PB-2 wording unchanged;
this change makes the existing wording self-consistent under
slice-3 per-batch work without amending the spec text.

### C-8 — Exception unwind reads `ExecutorGlobals::has_exception()`, not an `end` parameter

`SPECIFICATION.md` §3.2 lists `EG(exception)` under "Interfaces
consumed" — correct at intent. The implementation reads
`ExecutorGlobals::has_exception()` (the ext-php-rs wrapper, a
one-liner that null-checks `EG(exception)`). This is the same pattern
the spike already validated in C-5's `throws.php` coverage row.

The proposal originally claimed `ext_php_rs = 0.15.13`'s
`FcallObserver::end` had an `abnormal: bool` parameter and that the
recorder would read it directly. That was wrong: the real trait
signature is `fn end(&self, execute_data: &ExecuteData, retval:
Option<&Zval>)` — no `abnormal` parameter. Slice-2 spec
(`specs/recorder-call-events/spec.md`) and design D-7 were amended
in-flight to reflect the actual API; this note records the deviation
so the spec/design archive reads coherent against the implementation.

**Evidence:** the same C-5 coverage table proves
`ExecutorGlobals::has_exception()` reads `true` exactly when the
calling frame is unwinding via an exception. No further verification
is needed beyond the integration test's `throws.php` fixture (which
the harness exercises against every available PHP version).

### Architectural note — `FcallInfo<'a>` → `RawCallSite<'a>` indirection

The RO-4 fix forced a change to the categorise input type:
`ext_php_rs::zend::FcallInfo<'a>` carries `Option<&'a str>`
fields, leaving nowhere to store a lossy-decoded `String`
with the right lifetime. The recorder now owns a
`RawCallSite<'a>` with `Option<Cow<'a, str>>` fields plus an
`execute_data_addr: usize` (for RO-5's tiebreaker). The trait
signatures still take `&FcallInfo` per upstream's contract —
this is the boundary at which our owned analogue meets the
upstream borrowed one. If upstream ever widens
`FcallInfo`'s string fields, `RawCallSite` can collapse back
into a thin adapter; until then, the local type is the
correctness substrate.

## Repository hygiene

### N-1 — `.gitignore` excludes `openspec/`, `personas/`, `CLAUDE.md`

The current `.gitignore` deliberately keeps the workflow scaffolding
out of git history. That's a design choice this change respects. If a
future change wants to track the OpenSpec deltas in git history (e.g.
to drive code review against the spec deltas), the `.gitignore` lines
`/openspec`, `/personas`, `CLAUDE.md` will need to be revisited.

## Deferred follow-up changes

Each entry is a queued OpenSpec change that has NOT yet been created.
Grouped by phase / capability; the parenthetical names the source
review finding.

### Phase 1 — bootstrap & config cleanup

- `cleanup-config-error-alias` — delete the unused `pub type ConfigError = ConfigWarning;` alias (R-6).
- `lock-readme-directive-table` — add a parsing test that walks the README directive table and matches against `DIRECTIVES`'s defaults; also covers the spike directives' drift guard (R-7, S-7).
- `phpinfo-header-uses-underscore` — one-line rename so the section header reads `php_analyze` not `php-analyze` (R-9).
- `single-source-trim` — collapse the defensive double-trim between bootstrap and config (R-11).
- `notice-on-master-switch-off` — emit `E_NOTICE` on `MasterSwitchOff` so disabled-extension operators see a log line (R-12).

(R-10 `mshutdown-respects-silent-disable` was closed as a side benefit of `shipper-thread-and-channel`; no follow-up needed.)

### Phase 0 — spike tidy-up

- `spike-graceful-degrade-on-missing-config` — promote `inactive_sink()` to a public `SpikeObserver::inactive()` sibling and replace `build_spike_observer`'s `expect` on `Config::global()` with a `let-else` returning the inactive observer (S-4).
- `spike-tidy-fqn-and-deadcode` — `fqn` unreachable `unwrap_or`, `with_sink` doc clarification, `LocalFcallInfo::empty()` null-`func` notice (S-5, S-12, S-14).
- `spike-tighten-integration-assertions` — tighten `assert_pair` to `entry_hits == 1 && exit_hits == 1` per fixture and assert `array_map` callback fires 3× (S-10).
- `spike-portable-run-sh` — replace `python3` JSON parse with shell-only `${CARGO_TARGET_DIR:-…}` (S-11).
- `spike-log-path-validate-absolute` — reject non-absolute `spike_log_path` with a `ConfigWarning::SpikeLogPathNotAbsolute` (S-8).
- `spike-doc-cleanup` — `// NOTE for Phase 5` near the per-call allocations, soften / cite the `should_observe` caching claim (S-9, S-13).

### Phase 2 — recorder follow-ups

- `recorder-clock-ordering` — flip CPU/wall capture order inside the snapshot constructors so the cpu window strictly nests inside the wall window (RO-7).
- `recorder-cache-hostname` — cache `gethostname` once in an `OnceLock<Arc<str>>` populated at MINIT instead of running the syscall every RINIT (RO-8).
- `recorder-portable-c-char` — use `libc::c_char` instead of hard-coded `i8` for the hostname buffer cast (RO-9).
- `recorder-bootobserver-disabled-doc` — inline comment explaining why `BootObserver::Disabled`'s empty `begin`/`end` arms are reachable only on first-per-function fire (RO-10).
- `recorder-driver-build-once` — move `cargo build --features recorder-dump` out of the per-fixture `run.sh` invocation; let the Rust integration test build once before the fixture loop (RO-11).
- `recorder-dump-loud-failure-in-tests` — replace `eprintln!` in `recorder::dump::write_trace_if_path_set` with `panic!` under `cfg(test)` so silent dump-file write failures surface (RO-12).
- `recorder-style-cleanups` — name-consistency of `_execute_data`/`_retval` underscore-prefixed params; swap `{:?}` for `{}` on `PathBuf` in dump error message (RO-13, RO-14).

### Phase 2 — recorder substrate (slice-3 carry-forwards)

- `accounting-saturating-sub` — switch `accounting::sub` to `fetch_update(|cur| Some(cur.saturating_sub(bytes)))` so a future double-sub becomes a saturated no-op instead of a corrupting wrap; worth landing before Phase 4 adds the second sub site (DCR-3).
- `recorder-test-lock-hygiene` — either add "no `account_guard()` needed: bills zero" comments to the four named tests, or short-circuit `accounting::sub` when `bytes == 0` (DCR-4).
- `recorder-dump-test-lock` — add a per-module `DUMP_PATH_TEST_LOCK: Mutex<()>` so the four `dump::tests` that mutate `PHP_ANALYZE_DUMP_PATH` env var don't race (DCR-5).
- `recorder-test-helper-no-leak` — reshape `cat_for` into a `with_cat(name, |cat| { ... })` closure so the boxed `RawCallSite` lives on the test's stack frame instead of leaking (DCR-6).

### Phase 3 — wire format & stub ingest tidy

- `stub-ingest-spawn-timeout` — wrap `bound:` / `ready` consumption in a background thread that ships lines through a `mpsc::sync_channel(1)` with a `recv_timeout(10s)` cap (WSI-5).
- `stub-ingest-strip-prefix-bearer` — collapse `validate_bearer`'s `starts_with` + slice into a `let-else strip_prefix` (WSI-6).
- `stub-ingest-lazy-method-name` — inline or lazy-wrap `request_method_name(&request)` so the happy paths don't allocate (WSI-7).
- `stub-ingest-compact-json` — switch `handle_debug_batches` to `to_vec` (or record the pretty-print decision in `design.md`) (WSI-8).
- `stub-ingest-dispatch-borrow` — split `dispatch()` so `path` and `method` are owned only on the 405/404 paths; pairs naturally with `stub-ingest-lazy-method-name` (WSI-10).

### Phase 4 — shipper substrate carry-forwards

- `shipper-exit-as-enum` — model the exit as `enum ShipperExit { Drained { batches }, DeadlinePassed { drained, abandoned } }` so the implicit `drain_completed ⇔ abandoned == 0` invariant becomes unrepresentable (R-2).
- `startup-catch-unwind` — factor `bootstrap::startup`'s post-warning body into `startup_body()` and wrap it in `catch_unwind` to match `mshutdown`/`rinit`/`rshutdown`; should land before any panic-producing code (encoder / HTTP-client config) lands in `startup` (R-4).
- `spawn-failure-recovery` — either clear `SENDER_SLOT` / reset `SHIPPER_SPAWNED` on the spawn-failure path before re-panicking, or replace `.expect` with a `Result`-returning helper that bubbles up to a silent-disable + one `E_WARNING`. Must land before any producer wires real work into the channel (R-5).
- `shipper-deadline-pass-integration-test` — inject an `on_batch` callback that sleeps `100 ms` with `grace = 200 ms` and assert `JoinOutcome::Clean(ShipperExit::DeadlinePassed { .. })` with non-zero `abandoned`; must land with the encoder slice (R-6).
- `shipper-thread-name-fits-task-comm` — rename thread to fit `TASK_COMM_LEN = 16` (e.g. `pa-shipper`, `phpz-shipper`, `php-an-shipper`); currently truncated to `php-analyze-shi` in `top -H` / `ps -L` (R-7).
- `shipper-substrate-tidy` — group test-surface accessors under a `test_support` submodule or a single `TestProbe` struct; factor repeated `unwrap_or_else(|e| e.into_inner())`; reword `run_loop` doc's forward-pointing prose.

### Phase 4 — slice-2 producer-wiring tidy

- `flush-predicate-trigger-method-placement` — move `flush_predicate_trigger` to `Trace::flush_predicate_trigger` when the encoder slice provides a natural home for `FlushTrigger` next to `Trace` (PF-9). Cosmetic; no spec impact.

### Phase 4 — slice-3 follow-ups carried out of the round-2 review

- `shipper-collapse-drain-phases` — with the cell-publish path in place (SEH-9 / C-17), every code path through `run_loop` that processes a batch now has an `Option<Instant>` deadline available. The two-phase pre-drain/post-drain split is leftover from slice 1's silent-drain semantic. Collapsing them into a single deadline-aware loop body (taking the cell's `Option<Instant>` directly rather than gating on it) would simplify the state machine and remove the `local_deadline` snapshot dance. Open Question Q-1 from `shipper-deadline-at-recv-loop-head`'s `design.md`; deferred so the SEH-9 change stays small and the refactor is reviewable in isolation.
- `stub-ingest-header-capture` — add a `/debug/last_request_headers` endpoint on `stub-ingest` so the deferred §7.5 (`shipper_round_trip_authorization_header_matches_configured_token`) and §7.6 (`shipper_round_trip_user_agent_includes_crate_version`) integration tests can land (SEH-10).
- `stub-ingest-connection-counter` — add a `/debug/connection_count` endpoint plus a per-PHP-request fixture loop so the deferred §7.7 (`shipper_round_trip_keep_alive_serves_1000_requests_with_one_connection`) test can land. AC-SH-6 binding evidence (SEH-10).
- `stub-ingest-configurable-failure` — add a `--respond-with <status>` flag to `stub-ingest` plus a `tests/php-shipper/retry_exhaust.php` fixture so the deferred §7.8 (`shipper_round_trip_500_exhausts_retries_and_bumps_drop_counter`) and §7.9 (`shipper_round_trip_token_not_leaked_to_php_error_log`) integration tests can land. The §7.9 test also binds the SEH-2 `E_NOTICE` emit path end-to-end (SEH-10).
- `stub-ingest-slow-mode` — add a `--simulate-slow` flag to `stub-ingest` so the deferred §7.10 (`shipper_round_trip_mshutdown_drains_within_grace`) test can land. AC-SH-3 binding evidence (SEH-10).
- `shipper-drop-counter-attribution` — split the source trace's `drop_counter` into `retry_dropped` / `encode_dropped` / `deadline_dropped` (or surface a new `meta.drop_reasons: BTreeMap<...>`) so a downstream operator inspecting `meta.dropped_records` can attribute a spike to the right cause. The slice-3 implementation folds every `OnBatchOutcome::Dropped` path into the single existing counter (SEH-5 deviation); the records are genuinely lost on every path, so OBJ-5 ("no silent loss") still holds, but the attribution is lost.

### Phase 4 — slice-3 encoder-and-HTTP review findings

Review of branch `feat/shipper-encoder-and-http` (commits `4a67e43`,
`cff3a54`, `5b393ec`, `a6c3732`) against `SPECIFICATION.md` §3.3,
§5.2, and §6.3. Lib tests pass (221 / 221); the issues below are
correctness and spec-compliance gaps that should land before the
slice is treated as closed.

#### Critical — must fix before merge

- `SEH-1` **`shipper_round_trip` watchdog kills the test process unconditionally.**
  - **File:** `crates/php-analyze/tests/shipper_round_trip.rs:308-339`
  - **Severity:** Critical.
  - Each `read_one_line` call (twice per `StubProcess::spawn`, plus
    once per binary in the matrix) spawns a thread that
    `std::thread::sleep(Duration::from_secs(5))` then
    `std::process::exit(137)`. The `drop(handle)` at the end of
    `read_one_line` only detaches the `JoinHandle`; it does **not**
    cancel the spawned thread. After 5 wall-clock seconds the
    watchdog fires and tears down the entire `cargo test` process —
    even if the test has already passed, even if every readline
    returned in milliseconds. On a slow CI runner, a slow PHP
    extension load, or any sequence of binaries that pushes the test
    past 5 s, the binary exits with status `137` instead of `0`. The
    in-code comment "In practice this won't fire" relies on the
    test's happy path completing faster than 5 s — a wall-clock
    invariant the CI matrix has no way to enforce.
  - **Suggestion:** replace the spawned watchdog with the standard
    "blocking read on a worker thread + `mpsc::sync_channel(1).recv_timeout(5s)`
    on the test thread" idiom. The worker reads exactly one line and
    sends it through the channel; on `recv_timeout` `Disconnected` /
    `Timeout` the test panics with a clear message and the
    `StubProcess::Drop` impl kills the stub. No `std::process::exit`,
    no detached threads, no wall-clock dependency.

#### Major — spec compliance / correctness

- `SEH-2` **No `E_NOTICE` is emitted on drop, despite §5.2 step 4.**
  - **File:** `crates/php-analyze/src/shipper/{http.rs,mod.rs,on_batch.rs}`.
  - `SPECIFICATION.md` §5.2 step 4: *"Log one `E_NOTICE` per dropped
    batch: `php-analyze: dropped <N> records from trace <uuid>: <url>
    <status_or_error> (attempt <K>)`."* The slice adds
    `DropReason::Display` impls matching the `<status_or_error>`
    tokens verbatim, threads `trace_id: [u8; 16]` through
    `OnBatch::handle`, and surfaces `attempts: u32` on
    `OnBatchOutcome::Dropped` — i.e. every input the spec line needs
    is in place — but no call site ever writes the line. The new
    production `_trace_id` parameter on `RmpEncodeAndHttpPost::handle`
    is dead code (underscore-prefixed), and `drained_consume` discards
    the outcome reason / attempts on the `Dropped` arm.
  - This is the binding AC for both R-13 (drop visibility) and §11's
    operator-observability story; it is also the only signal a
    misconfigured operator gets that their auth token is wrong (401)
    or their endpoint is dead (connect-refused).
  - **Suggestion:** in `drained_consume`'s `Dropped` arm, format the
    spec line using `DropReason::Display`, the rendered UUID (the
    same `Uuid::from_bytes(...).to_string()` already used in
    `encode::meta_partial_to_wire`), and the `server_url` (plumbed
    via the `OnBatch` impl, e.g. a new `&self.server_url` accessor or
    a separate `OnBatch::log_drop` callback). Use
    `ext_php_rs::error::php_error` at `ErrorType::Notice` — the same
    integration `bootstrap::report_warning` uses for `E_WARNING`. The
    spec line must NOT include `auth_token` (AC-SH-4); this is
    already enforced by construction because the bearer string never
    leaves `RmpEncodeAndHttpPost::handle`'s local scope.

- `SEH-3` **`drained_consume`'s doc comment contradicts the implementation.**
  - **File:** `crates/php-analyze/src/shipper/mod.rs:251-265`
  - The doc claims the design-D-3 ordering is
    *"encode → accounting::sub → bump drop counter → count"* but the
    code is *"encode → bump drop counter → accounting::sub → count"*.
    Both orderings are correctness-preserving (the drop-counter bump
    is only visible to *future* batches from the same trace via the
    `Arc<AtomicU64>` shared with the source `Trace`), so this is a
    documentation defect rather than a runtime bug — but the doc is
    the only place the ordering invariant is recorded outside
    OpenSpec, and it is wrong.
  - **Suggestion:** decide which order is canonical and align both.
    "bump → sub" matches the natural reading "settle the drop
    accounting first, then release the byte budget" and is also
    cheaper-to-reason-about (the byte budget release is the
    happens-after side of the cross-thread invariant); update the
    comment to match. If the OpenSpec change's `design.md` D-3
    instead intends "sub → bump", swap the two lines in
    `drained_consume`.

- `SEH-4` **Dead computation in `run_with_retry`'s deadline check.**
  - **File:** `crates/php-analyze/src/shipper/http.rs:124-127`
  - ```rust
    if let Some(d) = deadline {
        let wakeup = now().saturating_duration_since(Instant::now()) + sleep;
        let _ = wakeup; // not strictly needed but documents intent
        if now() + sleep >= d {
    ```
  - `now().saturating_duration_since(Instant::now())` is always zero
    (or epsilon) by construction — `now()` is the captured clock
    closure, and on the production path `now == Instant::now`. The
    `_ = wakeup` discard confirms the value is unused. The
    surrounding `if now() + sleep >= d` does the actual work; the
    three lines above it should be deleted. Bonus: the inline
    comment "not strictly needed but documents intent" admits the
    code is dead.
  - **Suggestion:** delete the `let wakeup = …; let _ = wakeup;` two
    lines.

- `SEH-5` **`bump_drop_counter_on_drop` ignores `DropReason`; encode-failures and deadline-exceeded drops bump identically to retry-exhaust.**
  - **File:** `crates/php-analyze/src/shipper/http.rs:290-296`
  - `SPECIFICATION.md` §5.2 names retry-exhaust as the only path that
    adds `records_in_batch` to `drop_counter`. The implementation
    also bumps on `EncodeFailed` (attempts = 0, no retry policy
    applied) and `DeadlineExceeded` (could be attempts = 0 if the
    deadline was already past at orchestrator entry). Bumping on
    those paths is defensible — the records *are* lost — but the
    spec wording does not authorise it, and a downstream operator
    inspecting `meta.dropped_records` will not know whether a spike
    is retry exhaustion or an encoder regression.
  - **Suggestion:** either (a) record the bump-on-encode/deadline as
    a deliberate slice-3 deviation in this file's design log and
    amend `SPECIFICATION.md` §5.2 to match, or (b) split the
    accounting so encode-failures and deadline-exceeded drops bump a
    separate `encode_dropped` / `deadline_dropped` counter that
    surfaces in `meta` (would also resolve the operator-attribution
    ambiguity).

#### Minor — quality / hygiene

- `SEH-6` `RmpEncodeAndHttpPost::handle` allocates a fresh `String`
  for `url` and `user_agent` per call
  (`crates/php-analyze/src/shipper/http.rs:206-208`) and clones the
  `ureq::Agent`. None of the three needs to be owned by the closure
  — `&self.server_url`, `&self.user_agent`, `&self.agent` are all
  borrowable for the closure's lifetime. The clone is cheap
  (`Agent` is `Arc`-backed) but the two `String` allocations are
  per-batch overhead with no purpose. **Suggestion:** let the
  closure borrow; only the `bearer` needs a local because of the
  `expose_secret()` boundary.

- `SEH-7` `RmpEncodeAndHttpPost::new` silently clamps
  `retry_backoff.as_millis()` to `u32::MAX` via
  `try_into().unwrap_or(u32::MAX)`
  (`crates/php-analyze/src/shipper/http.rs:181`). Today
  `retry_backoff` is bounded by directive validation to `≤ 60_000`
  ms, so the clamp never fires; if the validation bound is ever
  raised without auditing this call site, a misconfigured operator
  will get a 49-day backoff instead of the warning they deserve.
  **Suggestion:** debug-assert the input fits in `u32` (or store
  the source `Duration` and compute the per-attempt sleep in
  `Duration` arithmetic).

- `SEH-8` `last_reason` in `run_with_retry` is initialised to
  `DropReason::Transport` and only consumed by the loop's
  structurally-unreachable fallthrough
  (`crates/php-analyze/src/shipper/http.rs:93,141-144`). The
  fallthrough cannot be reached because the inner `match` returns
  on every branch and the `for ..= retry_count` covers every
  possible `attempt_idx`. **Suggestion:** replace the fallthrough
  with `unreachable!("run_with_retry's inner match returns on every branch")`
  and drop `last_reason`; the compiler will then prove the
  invariant.

- `SEH-9` The `TODO(slice-2)` comment in
  `drain_and_join_at_mshutdown`
  (`crates/php-analyze/src/shipper/mod.rs:394-402`) refers to a
  forward-looking risk that slice 3 has now materialised — each
  batch carries encode + POST + retry work, exactly the case the
  TODO warned about. The deadline-aware recv-loop head is still
  not implemented. Slice 4 should pick it up before any
  longer-than-trivial backoff is exercised under MSHUTDOWN; the
  current `send_timeout(Drain, grace)` + `recv_deadline` shape is
  one-batch-deep and a saturated channel plus slow retries can
  still blow past `shutdown_grace_ms`. **Suggestion:** track as a
  follow-up OpenSpec change `shipper-deadline-at-recv-loop-head`
  and link to this finding.

- `SEH-10` The slice-3 commit message marks AC-SH-3 (black-holed
  timeout), AC-SH-5 (rustls + native CA), AC-SH-6 (1000 sends, 1
  TCP connection), plus tasks 7.5–7.10 as "deferred to integration
  tests / follow-up OpenSpec changes". That is acceptable for a
  slice, but the deferral is currently recorded only in the commit
  body. **Suggestion:** add an entry to the
  `## Deferred follow-up changes` section above naming the four
  follow-ups by spec ID (`stub-ingest-header-capture`,
  `stub-ingest-connection-counter`,
  `stub-ingest-configurable-failure`, `stub-ingest-slow-mode`) so
  the deferral is visible to anyone reading `COMMENTS.md` rather
  than `git log`.

- `SEH-11` `RmpEncodeAndHttpPost::handle`'s `attempt` closure binds
  `_attempt_idx: u32` but never references it
  (`crates/php-analyze/src/shipper/http.rs:210`). Once `SEH-2` is
  fixed, the E_NOTICE line wants `(attempt <K>)`; the attempts
  count is already on `OnBatchOutcome::Dropped`, so the underscore
  is fine, but the parameter could be dropped from the closure's
  signature (`|_| -> AttemptOutcome`) to reduce visual noise.

#### Round-2 fix status (recorded on branch `feat/shipper-encoder-and-http`)

Per CLAUDE.md / C-9 / C-11 / C-12 / C-14 / PF-* precedent: blocking
findings land on this same branch as additional commits under the
existing `shipper-encoder-and-http` OpenSpec change. Non-blocking
items that need new infrastructure are queued in the
`## Deferred follow-up changes` list above.

- **SEH-1 (critical) — fixed.** `tests/shipper_round_trip.rs`'s
  `read_one_line` watchdog was replaced with the standard "worker
  thread + `mpsc::sync_channel(1).recv_timeout(5s)`" idiom. The
  worker reads the two handshake lines on a stoppable thread; on
  timeout the test thread panics with a clear message and the
  `StubProcess::Drop` impl kills the stub. No `std::process::exit`,
  no detached threads, no wall-clock dependency on the test as a
  whole.
- **SEH-2 (major) — fixed.** Added `OnBatch::server_url(&self) ->
  Option<&str>` with a `None` default for test fakes;
  `RmpEncodeAndHttpPost` overrides it to expose its configured URL
  (the bearer token has no slot in `Url` so AC-SH-4 holds by
  construction). `drained_consume`'s `Dropped` arm now formats the
  §5.2 step-4 line (`php-analyze: dropped <N> records from trace
  <uuid>: <url> <status_or_error> (attempt <K>)`) and pushes it
  onto a process-global `Mutex<VecDeque<String>>` queue. The
  bootstrap layer drains the queue from `rshutdown_body` (per PHP
  request) and from `drain_shipper_if_enabled` (during MSHUTDOWN,
  after the shipper join), feeding each line through
  `php_error(E_NOTICE, ...)`.
  **Why a queue instead of a direct `php_error` call from the
  shipper:** the shipper runs on a background OS thread. `php_error`
  → `zend_error_va_list` reads TSRM / `EG(...)` globals that are
  only bound on the PHP-thread side; calling it from a non-PHP
  thread is undefined behaviour. The queue lets the shipper format
  the line correctly (with full access to `DropReason::Display`, the
  rendered UUID, and the configured URL) and defers the actual Zend
  call to the next PHP-thread tick — RSHUTDOWN for steady-state
  drops, MSHUTDOWN for the final-deadline drain. Drops that happen
  in a window with no following PHP-thread tick (extremely narrow:
  between MSHUTDOWN's final drain and process exit) are lost; this
  is a minor known limitation worth noting but not fixing in this
  slice. Tests:
  `format_drop_notice_matches_spec_5_2_step_4_wording`,
  `format_drop_notice_renders_each_drop_reason_token_per_5_2`,
  `push_and_drain_drop_notices_round_trips_in_push_order`,
  `drained_consume_pushes_a_notice_on_dropped_outcomes`,
  `drained_consume_does_not_push_a_notice_on_sent_outcomes`.
- **SEH-3 (major) — fixed.** `drained_consume`'s doc now reads
  "encode → bump drop counter → accounting::sub → format/queue
  notice → count", matching the code. The rationale (bump first to
  establish the cross-thread `Arc<AtomicU64>` invariant before the
  byte budget is released; release budget next to make a
  hypothetical encode-panic non-double-subtracting; format/queue
  last because it's deferrable without affecting accounting
  correctness) is inlined in the doc.
- **SEH-4 (major) — fixed.** The `let wakeup = ...; let _ = wakeup;`
  two lines in `run_with_retry`'s deadline check were deleted.
- **SEH-5 (major) — deviation documented + follow-up queued.** The
  doc on `bump_drop_counter_on_drop` now spells out that the impl
  bumps on **every** `OnBatchOutcome::Dropped` (retry-exhaust,
  `EncodeFailed`, `DeadlineExceeded`), not only retry-exhaust as
  §5.2 step 3 names. The records are genuinely lost on every path,
  so OBJ-5 ("no silent loss") still holds; the deviation folds all
  three into a single counter rather than splitting into separate
  `encode_dropped` / `deadline_dropped` counters that would need
  new `meta.*` fields. The follow-up
  `shipper-drop-counter-attribution` (above, in deferred list)
  carries the split if downstream operators need the distinction.
- **SEH-6 (minor) — fixed.** `RmpEncodeAndHttpPost::handle` no
  longer clones `url`, `user_agent`, or `agent`. The closure
  borrows `&self.server_url.as_str()`, `&self.user_agent.as_str()`,
  and `&self.agent` for its lifetime. Only the `bearer` string
  remains a local (the `expose_secret()` boundary needs an owned
  `String` for the `Bearer <token>` formatting; this matches the
  spec's "the token plaintext lives only in this local").
- **SEH-7 (minor) — fixed.** `RmpEncodeAndHttpPost::new` now
  `debug_assert!`s that `retry_backoff.as_millis() <= u128::from(u32::MAX)`
  before the `try_into` clamp. A future directive-validation
  loosening will surface as a debug-build panic at the call site
  instead of a silent 49-day backoff in release.
- **SEH-8 (minor) — fixed.** `run_with_retry`'s post-loop
  fallthrough is now `unreachable!("...")`; the local
  `last_reason` is dropped. The compiler proves the invariant via
  the `unreachable!`.
- **SEH-9 (minor) — closed.** Implemented in OpenSpec change
  `shipper-deadline-at-recv-loop-head` (branch
  `feat/shipper-deadline-at-recv-loop-head`). The
  `drain_and_join_at_mshutdown` function now publishes the deadline
  to a process-global `Mutex<Option<Instant>>` cell *before* sending
  the `Drain` message and dropping the canonical `Sender`. The
  shipper's `run_loop` snapshots the cell at the head of each
  pre-drain iteration and transitions to the same deadline-aware
  recv body that the post-`Drain` phase uses — bounding each
  in-flight batch's `run_with_retry` budget by the published deadline
  even when the channel is saturated and the `Drain` message itself
  is stuck behind a pile of pre-`Drain` batches. The `TODO(slice-2)`
  comment in `drain_and_join_at_mshutdown` is removed. See
  forward-looking design decision C-17 above for the full rationale.
- **SEH-10 (minor) — follow-ups queued.** The five `stub-ingest-*`
  follow-ups (above, in deferred list) record the 7.5–7.10
  deferrals from this slice's `tasks.md` so the deferred state is
  visible to anyone reading `COMMENTS.md` rather than `git log`.
- **SEH-11 (minor) — fixed.** `RmpEncodeAndHttpPost::handle`'s
  `attempt` closure is now `|_| -> AttemptOutcome` (the unused
  `_attempt_idx: u32` parameter is dropped).

### Phase 4 — slice-4 (`shipper-deadline-at-recv-loop-head`) round-1 fix status

Recorded on branch `feat/shipper-deadline-at-recv-loop-head`. Same
precedent as the SEH-* / PF-* / C-* slices above: blocking findings
land on this branch as additional commits under the
`shipper-deadline-at-recv-loop-head` OpenSpec change.

- **DRL-1 (critical) — fixed.** Local cargo test runs were green
  but CI (and local stress runs at `--test-threads=8`) tripped
  `debug_assert_eq!(deadline, drain_msg_deadline)` inside
  `run_drain_phase` for two slice-3 tests
  (`run_loop_with_drain_future_deadline_finishes_queued_batches`
  and `run_loop_with_drain_past_deadline_abandons_queued_batches`).
  Root cause: `run_loop` now reads the process-global
  `DRAIN_DEADLINE` cell on every pre-drain iteration. The nine
  pre-existing `run_loop_*` tests used local channels and did
  not acquire the shipper test lock (they did not need to, before
  the cell existed). Under parallel test execution, one of the
  new cell-publish tests could leave the cell in `Some(_)` mid-
  flight (between `set_drain_deadline_for_test` and
  `reset_for_test`), and a concurrent `run_loop_*` test would
  read the stale value, enter `run_drain_phase` with a deadline
  that did not match its own in-channel `Drain` message, and
  trip the debug assert. Fix: every `run_loop_*` test now
  acquires `let _guard = lock();` and brackets its body with
  `reset_for_test()` — the standard pattern for any test that
  observes process-global state. The debug assert itself is
  load-bearing (catches a future refactor that publishes the
  cell from a code path other than `drain_and_join_at_mshutdown`)
  and stays. Stress run: 5 × `--test-threads=8` passes green.

## Phase 5 anchor — AC-RC-5 zero-alloc audit harness

The `flush_into_pending_batch` accessor is zero-alloc by
construction (`mem::take` + three `Arc::clone` calls into the
`MetaPartial` — `host`, `sapi`, `uri_or_script`). The third
`Arc::clone` (on `uri_or_script`) was not zero-alloc as
originally landed; the PF-1 round-1 fix lifted
`Trace::uri_or_script` and `RequestIdentity::uri_or_script`
from `String` to `Arc<str>` so the construction is now
zero-alloc end-to-end. The audit harness that pins this
property — including the `// NOTE for Phase 5` markers near
the remaining dict-miss `to_owned()` allocations in
`begin_with_snapshots` — lands in Phase 5.
