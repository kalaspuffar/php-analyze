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
Three branches are currently pending push from this host:

```bash
# Phase 1 — already merged to main via PR #1; pushing the branch
# again is no longer necessary but harmless.
git push -u origin feat/scaffold-workspace-and-config

# Phase 0 spike — already merged to main via PRs #2/#3; pushing
# again is harmless.
git push -u origin feat/spike-zend-observer

# Phase-2 slice 1 (recorder substrate: clocks + types + dictionary).
# OpenSpec change: recorder-clocks-and-types. Four focused commits
# on top of main; cargo fmt/clippy/test all green, 14 new unit
# tests added (5 clocks + 5 types + 4 dictionary). Ready to push
# and open a PR.
git push -u origin feat/recorder-clocks-and-types
```

All three branches are fully committed locally and ready to push.

## Closed blockers

### B-1 — `ext-php-rs` integration deferred *(closed)*

`php8.4-dev` and `libclang-dev` were installed on the build host;
`php-config 8.4.21` is on `PATH`. `ext-php-rs` was added pinned at
`=0.15.13`, the `bootstrap.rs` module was implemented, and `MINIT` /
`MSHUTDOWN` / `RINIT` / `RSHUTDOWN` / `MINFO` are all wired through
`#[php_module]`. Manual verification with `php --ri php_analyze` on
PHP 8.4.21 cli passes both §9.5 and §9.6.

## Spec clarifications adopted while implementing this change

### C-1 — `Config::server_url` is `Option<Url>`, not `Url`

`SPECIFICATION.md` §4.1.1 sketches `pub server_url: Url`. The
silent-disable posture (§3.8 / configuration spec) requires a `Config`
to exist even when the URL is missing or unparseable, which would force
a sentinel `Url` value with confusing semantics. The implemented type is
`pub server_url: Option<Url>` and `Config::enabled` already disambiguates
the "valid URL exists" case. The struct doc comment records the
deviation.

### C-2 — `Config::disable_reason: Option<DisableReason>` added

The `extension-bootstrap` spec requires `MINFO` to render
`enabled (false: <reason>)` with a specific reason. Re-deriving the
reason from the warnings list at every `MINFO` call would be brittle, so
the resolved reason is stored alongside `enabled` on `Config`. Defined
as `enum DisableReason` with seven variants (master-switch off, four
URL/scheme-related, three token-related) and a `.human() -> &'static
str` accessor that renders the operator-facing message.

### C-3 — `ConfigError` renamed to `ConfigWarning` (alias kept)

The OpenSpec tasks define `pub enum ConfigError`. Semantically these
values are **warnings** — they're collected, returned, and the bootstrap
layer logs them at `E_WARNING`; none of them aborts `MINIT`. The
implemented enum is `ConfigWarning`, with `pub type ConfigError =
ConfigWarning;` re-exported for the original wording. The `source`
field on `TokenFileUnreadable` was renamed to `details` because
`thiserror` reserves `source` for `dyn Error`.

### C-4 — Inline `auth_token` is whitespace-trimmed before the empty check

Operator-friendly: a stray trailing newline in `php.ini` will not be
treated as a valid token. The file path takes precedence regardless;
this only affects inline tokens. The configuration spec's "Only inline
token configured is accepted" scenario remains green because the test
token has no surrounding whitespace.

### C-5 — `zend_observer` viability (Phase-0 outcome)

Output of the `spike-zend-observer` change. Retires Risk **R-2** from
`SPECIFICATION.md` §11.

**Crate version exercised:** `ext-php-rs = "=0.15.13"`, the same
version Phase 1 pinned. The locked-set of crate dependencies is
unchanged (verified by `cargo metadata` diff against `main`); the
feature list of `ext-php-rs` grew by one entry — `"observer"` —
which activates the public `FcallObserver` / `FcallInfo` /
`ModuleBuilder::fcall_observer` surface upstream.

**Reach path:** the spike registers an `FcallObserver` impl
(`crates/php-analyze/src/spike.rs::SpikeObserver`) via
`ModuleBuilder::fcall_observer(build_spike_observer)` inside
`lib.rs::get_module`. The factory reads `Config::global()` — which
is populated before the upstream `observer_startup()` runs because
our user `startup` shim is the `module_startup_func`, invoked first
by the `#[php_module]` macro's auto-generated `ext_php_rs_startup`
(verified by reading the macro expansion at
`ext-php-rs-derive-0.11.12/src/module.rs:35-50`). No raw FFI or C
glue is needed; the design.md §D-1 "Resolution" subsection records
the corrected approach.

`FcallInfo::from_execute_data` is `pub(crate)` upstream, so the
spike reconstructs the same parsing as `LocalFcallInfo` +
`extract_info` against the public `ffi::*` bindgen surface
(documented inline at `spike.rs:140`). When Phase 2 lands, the
likely cleanup is to drop both `LocalFcallInfo` and `extract_info`
in favour of an upstream `pub` constructor — assuming `ext-php-rs`
exposes one by then. If not, the local versions ship as-is.

**PHP versions verified:** PHP **8.4.21** on the build host (Debian
package, matching the closed B-1 note). PHP 8.3 has **not** been
verified on this host — there is no 8.3 install reachable here.
Tracked as a follow-up under task 10.1 of this OpenSpec change;
Phase 2's Recorder change MUST include 8.3 verification as part of
its own acceptance, or a separate `verify-observer-on-php83` change
must land first.

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

Three further structural findings worth carrying into Phase 2:

1. The `array_map` callback (an arrow function) fires its
   `closure:` pair **once per element** — three times for
   `[1, 2, 3]`. This is exactly the per-call coverage Phase 2's
   Recorder needs; no special handling required for higher-order
   internals.
2. The top-level script body is reported as
   `closure:<file>:1`. This is the natural place for Phase 2's
   `Trace` `RINIT`-allocation to happen if it ever needs an "entry
   to the request" anchor.
3. `RuntimeException`'s constructor is observed as
   `internal:__construct`; the `abnormal=false` reading on its
   exit confirms the order — Zend writes `EG(exception)` only
   *after* the constructor returns, so a peek at
   `has_exception()` inside the constructor's `end` handler
   correctly reads `false`. (The `bad()` function's own exit then
   reads `true`.)

**R-2 verdict:** **Closed for PHP 8.4** (`SPECIFICATION.md` §11
updated accordingly), **partially closed for PHP 8.3** — pending
the verification noted above. The pivot scenario from §10 Phase 0
("hybrid fallback") is NOT triggered: the observer surface covers
every category v1 cares about (per `SPECIFICATION.md` §3.2 and
§4.1.5). The `strlen` opcode-specialisation finding is recorded in
the spec scenario `PHP-specialised internals are NOT observed` so
Phase 2 inherits the known limitation cleanly.

### C-6 — Spike's file-open `E_WARNING` does NOT violate AD-4 / NFR-USE-2

Recorded while addressing review finding S-3 on the spike branch
(`feat/spike-zend-observer`). The spike layer can emit an
`E_WARNING` from `spike::SpikeObserver::from_config` when an active
spike fails to open `spike_log_path`, and the bootstrap layer can
emit an `E_WARNING` from `bootstrap::startup` when the extension
disables itself. Naïvely, those are two `E_WARNING` sources in one
process, which appears to break OQ-9 / AD-4's "single startup
`E_WARNING`" wording.

After the S-2 fix landed (`from_config` short-circuits before any
filesystem work when `enabled && spike_observer` is `false`), the
two sources are **mutually exclusive**:

- The spike warning fires only when `active = enabled && spike_observer`
  is `true`. That requires both `php_analyze.enabled = 1` and
  `php_analyze.spike_observer = 1`.
- The bootstrap disable-summary warning fires only when the
  extension is being **disabled** (URL invalid, token missing,
  master switch off, …). When the extension is disabled,
  `Config::enabled` is `false`, which forces `active = false`, which
  short-circuits the spike file-open. No spike warning is possible.

So at most one of the two `E_WARNING` sources fires per process, and
the AD-4 / NFR-USE-2 invariant is preserved without any further
plumbing. The spike retains a direct `php_error` call rather than
funnelling through `ConfigWarning`, which would couple the
throwaway module to the bootstrap warning surface for no behavioural
gain. The `from_config` doc comment records the same argument
in-source.

## Repository hygiene notes (out of scope for this change)

### N-1 — `.gitignore` excludes `openspec/`, `personas/`, `CLAUDE.md`

The current `.gitignore` deliberately keeps the workflow scaffolding
out of git history. That's a design choice this change respects. If a
future change wants to track the OpenSpec deltas in git history (e.g.
to drive code review against the spec deltas), the `.gitignore` lines
`/openspec`, `/personas`, `CLAUDE.md` will need to be revisited.

### N-2 — `SPECIFICATION.md` was untracked at the start of this branch

This change commits `SPECIFICATION.md` alongside the workspace skeleton
since it is the authoritative input for every subsequent change.

---

## Code review — 2026-05-20 (branch `feat/scaffold-workspace-and-config`)

**Reviewer:** Claude Code
**Reviewed against:** `SPECIFICATION.md` §3.1, §3.5, §4.1.1, §6.3, §7.2, §9.6
**Scope:** all commits on `feat/scaffold-workspace-and-config` vs. `main`
(`dfa2dc8`, `6061c93`, `131c6f4`). `cargo fmt --check`, `cargo clippy
--all-targets --all-features -- -D warnings`, and `cargo test --all`
(14/14 passing) are clean locally.

The change is well-structured: a clean separation between pure-Rust
config validation (`config.rs`) and the `ext-php-rs`-only boundary
(`bootstrap.rs`), thorough rustdoc on every public item, the
silent-disable posture is faithful to AD-4, and the bearer token is
correctly walled off behind `SecretString` plus a hard-coded `"***"`
in `phpinfo()`. The issues below are the deltas the author should
address before this branch can be considered "Phase 1 done" per
§10 Phase 1.

### R-1 (Critical) — CI cannot build the extension: missing `libclang-dev`

- **File:** `.github/workflows/ci.yml:35-49`
- **Severity:** Critical (CI green is a §9.6 gate)
- **Description:** `ext-php-rs = 0.15.13` pulls in `ext-php-rs-bindgen
  → ext-php-rs-clang-sys` as a build dependency (`cargo tree` confirms).
  `clang-sys` needs a working `libclang` on the build host or the
  build aborts with `Unable to find libclang`. CI installs only
  `php<v>` / `php<v>-cli` / `php<v>-dev`, so the very first
  `cargo clippy` / `cargo test` invocation in CI will fail before
  this branch's tests ever run. The (now-closed) B-1 blocker in
  this very file already documents that `libclang-dev` was needed
  on the local build host; the CI step did not get the same fix.
- **Suggestion:** Add `libclang-dev` (and `clang`, for `bindgen`'s
  preprocessor invocations) to the apt install line. While there,
  pin `LIBCLANG_PATH` only if the install ever lands `libclang.so` in
  a non-default location.
- **Example:**
  ```yaml
  sudo apt-get install -y \
    php${{ matrix.php }} \
    php${{ matrix.php }}-cli \
    php${{ matrix.php }}-dev \
    libclang-dev \
    clang
  ```

### R-2 (Major) — Stale comment in `crates/php-analyze/Cargo.toml`

- **File:** `crates/php-analyze/Cargo.toml:16-29`
- **Severity:** Major (misleading future-readers; conflicts with the
  literal line above it)
- **Description:** Lines 17-24 declare the four runtime dependencies
  including `ext-php-rs = "=0.15.13"`. Lines 26-29 then say:
  > `ext-php-rs` is *intentionally not yet added* — see the
  > `scaffold-workspace-and-config` OpenSpec change, §2.1.

  Both cannot be true. The bootstrap commit added the dependency;
  the comment block was never deleted.
- **Suggestion:** Delete lines 25-29 entirely. The block above
  already explains what `secrecy`, `thiserror`, and `url` are for;
  `ext-php-rs` itself needs no defence at this point. If a future
  reader needs the historical context, the OpenSpec archive will
  carry it.

### R-3 (Major) — `bootstrap.rs` has zero unit tests

- **File:** `crates/php-analyze/src/bootstrap.rs` (whole module)
- **Severity:** Major (largest FFI surface in the crate, the
  riskiest file, and the only file `cargo test` cannot link without
  PHP headers).
- **Description:** Every test in this change lives in `config.rs`.
  `bootstrap.rs` carries:
  - `parse_bool` — pure Rust, deterministic, currently zero coverage.
  - `DIRECTIVES` ↔ `config.rs` default-value parity — silent drift
    today (see R-7).
  - `read_raw_ini` — the only translation layer between the PHP
    INI store and `RawIni`; a typo in a directive name there
    silently zeroes a field.
  - `PhpInfoRenderer` — the function that has to *never* leak the
    token; tests should grep for `auth_token` plaintext in its
    output and confirm `***` is rendered.

  None of these need PHP headers to be tested if the helpers are
  pulled into shape that takes plain Rust inputs.
- **Suggestion:** At minimum add:
  1. A `#[cfg(test)] mod tests` in `bootstrap.rs` covering
     `parse_bool` for `"1"`, `"0"`, `"On"`, `"OFF"`, `"true"`,
     `"YES"`, `"  yes  "`, `""`, `"maybe"`.
  2. A test that walks `DIRECTIVES` and asserts every `default`
     string parses (via `parse_bool` / `i64::from_str`) into the
     same value `Config::from_ini_values(&RawIni::default())`
     produces for the corresponding field. This catches R-7.
  3. Factor `read_raw_ini` so the inner mapping (a `&HashMap<String,
     Option<String>>` → `RawIni`) is pure-Rust and testable.

### R-4 (Major) — Token-file trimming differs from inline trimming

- **File:** `crates/php-analyze/src/config.rs:483-494` (file path) vs.
  `crates/php-analyze/src/config.rs:507-512` (inline path)
- **Severity:** Major (correctness; produces a usable-but-wrong
  bearer token, which the server will then 401 on).
- **Description:** For inline tokens the code does
  `raw.auth_token.as_deref().map(str::trim)` — full `trim()`.
  For file tokens it does `content.trim_end()` only.

  Consequence: a token file written with `echo "  secret  " >
  /etc/php-analyze/token` yields the `SecretString` `"  secret"`
  (leading whitespace preserved). The same content inline yields
  `"secret"`. The server only sees the leading-whitespace variant
  for file-sourced tokens, so the silent-disable posture *doesn't*
  catch it — the extension runs happily and every batch is rejected.

  C-4 in this file already says inline tokens are
  whitespace-trimmed; the file path should match.
- **Suggestion:** Use `content.trim()` (or
  `content.trim_matches(char::is_whitespace)`) in the file branch.
  Add a regression test: a tempfile with `"  file-token  \n"`
  produces `SecretString` exposing exactly `"file-token"`.

### R-5 (Major) — Out-of-range/URL warnings still surface when master
switch is off

- **File:** `crates/php-analyze/src/config.rs:216-330`
- **Severity:** Major (operator UX; clutters PHP error log on a
  deliberately-disabled extension)
- **Description:** Control flow in `Config::from_ini_values`:
  1. Read `master_enabled`.
  2. Unconditionally run **all** `clamp_directive` calls (pushes
     `OutOfRange` warnings into `warnings`).
  3. Unconditionally call `resolve_server_url` (pushes `InvalidUrl`
     / `UnsupportedScheme` / `HttpScheme`).
  4. Only *then* check `!master_enabled` to short-circuit token
     resolution and produce the `MasterSwitchOff` summary.

  An operator who sets `php_analyze.enabled = 0` while a stale
  `php_analyze.server_url = "garbage"` is left in `php.ini` still
  gets an `E_WARNING: php_analyze.server_url: failed to parse 'garbage'`
  on every process start, even though the extension is off. That's
  the opposite of NFR-USE-2's intent.
- **Suggestion:** Bail early when `!master_enabled`: return a
  `Config { enabled: false, disable_reason: Some(MasterSwitchOff),
  …defaults… }` with an empty `warnings` vec. Numeric clamping
  warnings for a disabled extension serve no one. Update the
  matching test (none exists today; add one).

### R-6 (Minor) — `pub type ConfigError = ConfigWarning;` is unused

- **File:** `crates/php-analyze/src/config.rs:168`, re-exported in
  `crates/php-analyze/src/lib.rs:22`
- **Severity:** Minor (dead code, but exported in the crate API)
- **Description:** C-3 above documents the rename, but no caller —
  in this crate or in any future-phase touchpoint — uses
  `ConfigError`. It exists purely to satisfy the OpenSpec task's
  original wording. An unused public type alias is API surface the
  crate has to keep stable for no benefit.
- **Suggestion:** Either (a) delete the alias and the re-export and
  update the OpenSpec task wording in the next change, or (b) mark
  it `#[deprecated = "use ConfigWarning"]` and document a removal
  timeline. (a) is the cheaper of the two.

### R-7 (Minor) — Directive defaults and ranges duplicated between
`bootstrap.rs` and `config.rs` with no drift check

- **Files:**
  - `crates/php-analyze/src/bootstrap.rs:50-116` (`DIRECTIVES`
    array — default values as strings)
  - `crates/php-analyze/src/config.rs:170-199` (`DEFAULT_*`
    constants and `RANGE_*` tuples)
  - `README.md:88-103` (operator-facing table)
- **Severity:** Minor (correctness-by-discipline; no
  compile-time / test-time guard)
- **Description:** The default for `php_analyze.flush_records` is
  the literal string `"10000"` in the directive table, the integer
  `10_000` in `DEFAULT_FLUSH_RECORDS`, and the value `10000` in the
  README table. Changing one and forgetting the others compiles,
  passes clippy, and passes today's tests.
- **Suggestion:** A `#[test]` that walks `DIRECTIVES` and asserts the
  parsed default matches the corresponding `DEFAULT_*` const. (See
  R-3 suggestion 2.) That's a 30-line test that prevents a class of
  silent drift forever.

### R-8 (Minor) — `config_global_returns_same_reference_on_repeated_reads`
test is fragile under future tests

- **File:** `crates/php-analyze/src/config.rs:800-817`
- **Severity:** Minor (test-design)
- **Description:** The test is the only one in the suite that
  touches `initialise_from_ini` and therefore the global `OnceLock`.
  The test comment correctly warns that "the harness runs tests in
  parallel within a process, but only one `set` succeeds; that's
  the value `global()` returns." But the contract is implicit. A
  future test that calls `initialise_from_ini` in parallel with this
  one will see whichever `Config` wins the race — that test will
  flake non-deterministically.
- **Suggestion:** Either gate this test behind a feature
  (`#[cfg(feature = "global-test")]`) and run it in a separate
  `cargo test` invocation, or pull in `serial_test` and annotate
  with `#[serial]`. Add a comment to `initialise_from_ini` itself
  that calling it from a test is a one-shot for the process
  lifetime.

### R-9 (Minor) — `phpinfo()` header reads `php-analyze`, but module
name is `php_analyze`

- **File:** `crates/php-analyze/src/bootstrap.rs:287`
- **Severity:** Minor (operator UX)
- **Description:** `info_table_header!("php-analyze", env!("CARGO_PKG_VERSION"))`
  renders `php-analyze` (Cargo package name with hyphen). Every
  other operator-facing string in the same renderer uses
  `php_analyze` with an underscore (matching the module name set in
  `lib.rs:43`). An operator running `php -i | grep php_analyze`
  will miss the header section.
- **Suggestion:** Replace with `info_table_header!("php_analyze",
  env!("CARGO_PKG_VERSION"))`, or pull both occurrences from a
  single `const MODULE_DISPLAY_NAME: &str = "php_analyze";`.

### R-10 (Minor) — `mshutdown` is a no-op even on disabled / never-initialised
extension

- **File:** `crates/php-analyze/src/bootstrap.rs:148-150`
- **Severity:** Minor (defensive correctness, future-proofing)
- **Description:** The current body is a single `0`. Fine for this
  change, but the doc comment ("later changes drain the shipper
  here") leaves a footgun: future code that adds a shipper-drain
  call must remember to guard on `Config::global().map_or(true,
  |c| !c.enabled)` the same way `rinit` / `rshutdown` do. There's
  no test asserting the disabled-extension MSHUTDOWN path stays
  side-effect-free.
- **Suggestion:** Add the same guard now (a no-op today, a real
  guard once the shipper lands), and a comment in the doc explaining
  that the guard is load-bearing for the silent-disable posture.

### R-11 (Nit) — `read_raw_ini` trims, then `from_ini_values` trims again

- **File:** `crates/php-analyze/src/bootstrap.rs:235-264` (trim #1)
  and `crates/php-analyze/src/config.rs:437,476,507` (trim #2)
- **Severity:** Nit (defensive duplication, not a bug)
- **Description:** `bootstrap::read_raw_ini` filters empty strings
  via `.trim().to_owned() … filter(|s| !s.is_empty())`. The pure
  validator then `trim()`s again. Both layers passing strings is
  harmless but obscures the contract: the validator's tests bypass
  the bootstrap-layer trim, so it's the validator that has to be
  paranoid — and it is, but the bootstrap-layer trim is then
  redundant and worth a one-line comment ("first line of defence;
  `from_ini_values` is the authoritative trim").
- **Suggestion:** Pick one layer. Recommendation: keep the
  authoritative trim in `from_ini_values` (it owns the contract;
  it's tested) and drop the trim in `read_raw_ini` (it's only for
  the `is_empty` filter). The `is_empty` filter can be expressed
  as `filter(|s| !s.trim().is_empty())` without owning a trimmed
  copy.

### R-12 (Nit) — `bootstrap::startup` does not log the resolved
`disable_reason`

- **File:** `crates/php-analyze/src/bootstrap.rs:124-137`
- **Severity:** Nit (operator UX)
- **Description:** When `master_enabled = false`,
  `from_ini_values` returns no warnings (the master-switch case
  short-circuits before `warnings.push`). `MINFO` then renders
  `enabled (false: php_analyze.enabled = 0)`, but the PHP error log
  carries no trace of the extension having been loaded at all. An
  operator who is debugging "why isn't my extension running?" has
  no log line to look at. This is consistent with §10 OQ-9
  ("single startup E_WARNING") but the log silence on
  `MasterSwitchOff` is worth either documenting in README or
  emitting at `E_NOTICE`.
- **Suggestion:** Emit one `E_NOTICE` ("php-analyze: disabled via
  php_analyze.enabled = 0") on the `MasterSwitchOff` path. Cheap;
  pure operator-UX win. Update README's "Behaviour" section to
  call it out.

### Specification compliance

- ✅ §3.1 Bootstrapper — `MINIT`/`MSHUTDOWN`/`RINIT`/`RSHUTDOWN`/`MINFO`
  wired; silent-disable posture honoured; tokens redacted in MINFO.
- ✅ §3.5 Configuration — all 13 directives registered at
  `PHP_INI_SYSTEM`; range clamping with `E_WARNING`; token-file
  precedence (modulo R-4).
- ✅ §4.1.1 `Config` shape — deviations from the literal sketch are
  documented in C-1 / C-2 and justified.
- ✅ §6.3 Data protection — `SecretString` for the token; `***`
  displayer wired; redaction asserted in tests.
- ⚠️ §9.6 CI gates — `fmt`/`clippy`/`test` configured in
  `.github/workflows/ci.yml`, but the workflow will fail before
  it reaches those gates (see R-1). `cargo audit` listed in
  §9.5 is not yet present; acceptable for this phase but should
  be tracked.
- ❌ §3.5 AC-CF-1 ("Every directive has a default, range, and
  effect documented in README §MAINT-1") — README has the table,
  but no automated check enforces it matches the code (R-7).

### Overall recommendation

**REQUEST CHANGES.** R-1 is a hard CI blocker (the workflow as-is
cannot succeed once it runs on a runner that doesn't already have
libclang installed); R-2 / R-4 / R-5 are correctness or
operator-facing wins worth landing before this branch is declared
"Phase 1 done"; the rest are cheap test and tidiness work that
should be queued as follow-up changes (each its own OpenSpec
change, per CLAUDE.md's "one change per branch" rule).

Phase 0 / Phase 2 (observer hooks) and later work do not depend on
fixing R-6 through R-12, so those can move in parallel — but R-1
through R-5 should land before the next change is opened.

## Review-fix status — 2026-05-20 (round 1)

R-1 through R-5 (the reviewer's "REQUEST CHANGES" set) have been
addressed on this same branch (`feat/scaffold-workspace-and-config`) as
additional commits under the existing `scaffold-workspace-and-config`
OpenSpec change. The change's `tasks.md` §11 records the per-finding
work and the validation evidence (fmt/clippy/test/openspec validate all
green; 30/30 tests pass, 16 of those new for R-3/R-4/R-5).

### Addressed on this branch

- **R-1** (CI libclang) — `.github/workflows/ci.yml` now installs
  `libclang-dev` and `clang` alongside the PHP dev headers.
- **R-2** (stale Cargo.toml comment) — the contradictory comment block
  was deleted; the `ext-php-rs = "=0.15.13"` line above it speaks for
  itself.
- **R-3** (bootstrap tests) — `bootstrap.rs` gained a 14-test
  `#[cfg(test)] mod tests`. To make it possible, `read_raw_ini` was
  split into a one-line PHP adapter plus a pure
  `raw_ini_from_ini_map(&HashMap<String, Option<String>>) -> RawIni`,
  and `PhpInfoRenderer::render` now consumes a pure
  `PhpInfoRenderer::rows(Option<&Config>) -> Vec<(String, String)>`.
  The redaction property is now covered by a grep-for-the-plaintext
  test.
- **R-4** (token-file trim) — `resolve_token`'s file branch is now
  `content.trim()`, matching the inline branch. Regression test
  `auth_token_file_with_surrounding_whitespace_is_fully_trimmed`
  asserts that `"  file-token  \n"` from a file resolves to
  `"file-token"`.
- **R-5** (master-switch quiet) — `Config::from_ini_values` now
  short-circuits before clamping/URL validation when
  `master_enabled = false`, returning a `Config::disabled(MasterSwitchOff)`
  with an empty warnings vec. Regression test
  `master_switch_off_with_garbage_directives_emits_no_warnings`
  feeds garbage URL + out-of-range numerics + missing token to confirm
  the disabled path is silent.

### Queued as follow-up OpenSpec changes (not this branch)

Each gets its own change + branch per the "one change per branch" rule.
Listed here so the next session can pick them up without re-reading the
full review:

- **R-6** (`pub type ConfigError = ConfigWarning;` alias is unused).
  Option (a) from the review: delete the alias and update the OpenSpec
  task wording in a `cleanup-config-error-alias` change.
- **R-7** (directive defaults documented in three places without a
  drift guard). The new
  `directive_table_numeric_defaults_match_resolved_config_defaults`
  test in `bootstrap.rs` covers the
  `DIRECTIVES` ↔ `config.rs` half of this finding; the README table
  is still un-checked. Follow-up change `lock-readme-directive-table`
  should either (a) move the README table behind a generator that
  reads from `DIRECTIVES`, or (b) add a parsing test that walks the
  README and matches expected defaults.
- **R-8** (`config_global_returns_same_reference_on_repeated_reads`
  is fragile under future tests touching the same `OnceLock`).
  Follow-up change should annotate with `serial_test` (or feature-gate
  the test) before any second writer of the `OnceLock` lands.
- **R-9** (`phpinfo()` header reads `php-analyze`, every other string
  reads `php_analyze`). One-line follow-up
  `phpinfo-header-uses-underscore`.
- **R-10** (`mshutdown` no-op lacks the `Config::global().enabled`
  guard the `rinit`/`rshutdown` hooks already have). Defensive; should
  land before the shipper-drain code in Phase 4. Follow-up
  `mshutdown-respects-silent-disable`.
- **R-11** (defensive double-trim between `bootstrap::read_raw_ini`
  and `config::from_ini_values`). Cosmetic; the new bootstrap tests
  cover the lookup-side trim, so the cleanup is purely about clarity.
  Follow-up `single-source-trim`.
- **R-12** (no operator-visible log line on `MasterSwitchOff`).
  After R-5 the `MasterSwitchOff` path is fully silent; an `E_NOTICE`
  would be a UX upgrade but is explicitly out of scope for the
  reviewer's R-1..R-5 set. Follow-up `notice-on-master-switch-off`.

---

## Code review — 2026-05-21 (branch `feat/spike-zend-observer`)

**Reviewer:** Claude Code
**Reviewed against:** `SPECIFICATION.md` §3.5, §6.3, §10 Phase 0, §11
(R-2). `CLAUDE.md` style rules.
**Scope:** all commits on `feat/spike-zend-observer` vs. `main`
(`bcc9017`, `7098366`, `4d9b96a`, `3b8606a`, `d6472ac`). `cargo fmt
--check`, `cargo clippy --all-targets --all-features -- -D warnings`,
and `cargo test --all` (47 tests) pass locally.

The spike does exactly what Phase 0 asks for: it stands up a real
`FcallObserver` against `ext-php-rs = "=0.15.13"` with the `observer`
feature, drives three PHP fixtures, and produces the coverage table
C-5 records. The default-off posture is honoured (two new directives,
both default to `0` / empty), and `phpinfo()` exposes a red-flag
banner so a forgotten-on spike is visible. The findings below are
mostly forward-looking: this code is throwaway per the module-level
doc, but several patterns will be tempting to copy into Phase 2's
Recorder, and a couple of them should be cleaned up first so the
Recorder inherits something safe.

### S-1 (Major) — `zend_string_to_str` returns an unsound `&'static str`

- **File:** `crates/php-analyze/src/spike.rs:247-256` (and propagated
  through `LocalFcallInfo<'static>` at `spike.rs:148-186`,
  `extract_info` at `spike.rs:186-231`).
- **Severity:** Major (unsound type signature; copying the pattern
  into the Phase-2 Recorder would be a long-lived bug).
- **Description:** The signature is `unsafe fn zend_string_to_str(zs:
  *mut ffi::zend_string) -> Option<&'static str>`. The doc comment is
  candid that the `'static` lifetime "is a convenient fiction" — the
  borrow is in fact only valid for the duration of the observer
  callback. That's exactly what makes it unsound: `'static` is the
  type-level claim that the reference is valid forever, so anything
  that consumes the result (a future refactor, a closure that escapes
  the call, a `clone()` into a longer-lived struct) can silently
  produce a dangling pointer with no compile-time warning. The current
  call sites happen to be safe because they immediately copy into
  owned `String`s inside `fqn`, but the next person to touch this
  module is one careless edit away from a use-after-free.

  The downstream consequence is in `LocalFcallInfo`: the type carries
  a `'a` lifetime parameter (`spike.rs:149`), but every constructor
  (`LocalFcallInfo::empty()` at `spike.rs:157-167` and `extract_info`
  at `spike.rs:186`) returns `LocalFcallInfo<'static>`, so `'a` is
  dead — it survives only as a hint to readers that the inner
  references "morally" borrow from `ExecuteData`.
- **Suggestion:** Tie the lifetime to the input. Either:
  1. **Type-honest fix (preferred):** change the signature to
     `unsafe fn zend_string_to_str<'a>(zs: *mut ffi::zend_string) ->
     Option<&'a str>` and let inference pick `'a` from the call site
     (which will be `&ExecuteData`-bound). Propagate through:
     `extract_info<'a>(execute_data: &'a ExecuteData) ->
     LocalFcallInfo<'a>`, and `LocalFcallInfo<'a>` keeps its
     parameter for real. The `empty()` constructor becomes
     `fn empty() -> LocalFcallInfo<'static>` only because it carries
     no borrows.
  2. **Copy-eagerly fix:** if Phase 2 is going to allocate a `String`
     per call anyway (R-13 in `SPECIFICATION.md` is silent on this),
     change the signature to `Option<String>` and pay the alloc
     upfront. Phase 2 then has zero lifetime complexity for the
     `zend_string` decode and the `'static` lie disappears.

  Either is fine for the spike; (1) is the cheaper one to land and
  preserves the zero-alloc-decode option for Phase 2.

### S-2 (Major) — `SpikeObserver::from_config` opens the log file even when the spike is inactive

- **File:** `crates/php-analyze/src/spike.rs:64-88`
- **Severity:** Major (operator UX; produces an extra
  `E_WARNING` that the silent-disable posture promises will not
  appear).
- **Description:** Control flow in the constructor:
  1. Compute `active = config.enabled && config.spike_observer`.
  2. Unconditionally `OpenOptions::new().create(true).append(true).open(path)`
     if `spike_log_path` is set.
  3. On open failure, emit `php_error(E_WARNING, …)` and fall back to
     stderr.

  An operator who leaves a stale `php_analyze.spike_log_path =
  "/var/log/old-spike.log"` in `php.ini` but turns the spike off
  (`php_analyze.spike_observer = 0`) still triggers the file-open at
  `MINIT`. If the old path is no longer writable (rotated away,
  permissions changed, mount removed) they get a `E_WARNING` on every
  process startup *for an extension feature they're not using*. This
  is the same operator-UX failure R-5 fixed for the master switch
  applied at the spike layer.

  It's also wasted work: open + close + Box-allocate-a-sink for a
  spike whose `should_observe` will return `false` on every call.
- **Suggestion:** Bail before opening when `active = false`. The body
  collapses to:
  ```rust
  if !active {
      return Self {
          sink: Arc::new(Mutex::new(Box::new(io::sink()))),
          active: false,
      };
  }
  // ...existing open-file-or-stderr logic...
  ```
  Use `io::sink()` (or any `Write + Send` no-op) as the inactive
  placeholder so the `Mutex<Box<dyn Write + Send>>` invariant holds
  without burning a real file descriptor. Add a unit test:
  `from_config(spike_observer=false, spike_log_path=Some("/no/such/dir"))`
  must not emit a warning and must not error.

### S-3 (Major) — `from_config`'s `E_WARNING` adds a second startup warning, contradicting NFR-USE-2 wording

- **File:** `crates/php-analyze/src/spike.rs:73-78`
- **Severity:** Major (correctness vs. spec wording; mitigated to
  "Minor" by S-2 since the warning then only fires in the
  spike-enabled path).
- **Description:** `SPECIFICATION.md` §1.4 OQ-9 / AD-4 promise
  "silent disable + **single** startup `E_WARNING`" on misconfig.
  Today the bootstrap layer logs every `ConfigWarning` returned by
  `from_ini_values`, of which there is at most one
  *disable-summary* warning per process (asserted by the test
  `at_most_one_disable_warning_is_emitted_when_multiple_required_values_missing`).
  The spike sneaks a second `E_WARNING` in via `php_error` directly,
  bypassing the warnings list, when the spike log file can't be
  opened.

  Under the current ordering (`build_spike_observer` runs after the
  bootstrap layer has finished pushing warnings), an operator who
  has both a misconfigured `auth_token` AND an unwriteable
  `spike_log_path` will see two `E_WARNING` lines, which the spec
  says can't happen.
- **Suggestion:** Funnel the spike's file-open failure through the
  same `ConfigWarning` channel the bootstrap layer uses. The
  cheapest path: have `from_config` return `(Self,
  Option<ConfigWarning>)` (or a new `SpikeWarning` variant)
  and have `lib.rs::build_spike_observer` push the result through
  `php_error` itself — but inside a "still no more than one warning
  total" gate. Alternatively, downgrade to `E_NOTICE` and document
  that spike misconfig is `E_NOTICE`-level (spike is a dev-only
  switch; `E_NOTICE` matches the §5.2 retry-failure log level and
  reads as "informational, not a misconfig"). Note in C-5 (or a new
  C-6) so the deviation is recorded.

  The order S-2 → S-3 matters: if S-2 lands first, the warning fires
  *only* when the operator has explicitly turned the spike on,
  which makes "a second warning" defensible. S-3 is then arguably
  acceptable as-is; record the decision in `COMMENTS.md`.

### S-4 (Minor) — `build_spike_observer` panics if `Config::global()` is unset

- **File:** `crates/php-analyze/src/lib.rs:53-61`
- **Severity:** Minor (lifecycle invariant; the panic is observable
  only if a future `ext-php-rs` reorders the macro-generated startup).
- **Description:** The factory does
  `Config::global().expect("Config::global() must be populated before
   observer factory fires; check startup wiring")`. The doc comment
  documents the dependency on the macro's expansion order, and C-5
  cites the exact line of `ext-php-rs-derive-0.11.12/src/module.rs`
  the assumption rests on. But `expect` in `MINIT` is a panic; a
  panic across an FFI boundary into PHP is undefined behaviour on
  most targets, and on Linux x86_64 it tends to abort the process
  rather than just disable the extension. That's the opposite of the
  silent-disable posture the whole crate is built around.
- **Suggestion:** Degrade gracefully:
  ```rust
  let Some(config) = Config::global() else {
      // Defensive: should never happen given current ext-php-rs
      // wiring (see C-5). If it does, fall back to an inactive
      // observer so the extension still loads.
      return spike::SpikeObserver::inactive();
  };
  spike::SpikeObserver::from_config(config)
  ```
  Add `SpikeObserver::inactive()` as a public sibling of
  `from_config` that constructs the same `Self { sink: …, active:
  false }` Self the S-2 fix produces. A `debug_assert!` (not
  `expect`) on `Config::global().is_some()` keeps the invariant
  visible in tests/debug builds.

### S-5 (Minor) — `fqn` has an unreachable `unwrap_or` after the closure-detection branch

- **File:** `crates/php-analyze/src/spike.rs:285-294`
- **Severity:** Minor (dead defensive code; hides intent).
- **Description:** The closure branch is entered when `is_closure
  || info.function_name.is_none()`. Falling through means
  `function_name` is `Some(_)` AND not a closure-shaped name. The
  fall-through code is then:
  ```rust
  let name = info.function_name.unwrap_or("(unknown)");
  format!("function:{file}:{line}:{name}")
  ```
  The `unwrap_or("(unknown)")` is unreachable: we already ruled out
  `None` two lines up. A reader has to follow the negation chain to
  see that, and a future edit that loosens the branch above (e.g. to
  match additional closure variants by also returning when the name
  is `Some` but in some other shape) will silently make the
  `"(unknown)"` fallback reachable, masking the bug.
- **Suggestion:** Bind the unwrap into the precondition:
  ```rust
  let Some(name) = info.function_name else {
      // function_name was None — already handled by the closure
      // branch above.
      unreachable!("function_name is Some at this point");
  };
  format!("function:{file}:{line}:{name}")
  ```
  Or, more idiomatically, restructure `fqn` as an `enum FqnKind`
  that's then formatted in one place — but that's overkill for
  throwaway code. The `let-else` form is the minimum churn.

### S-6 (Minor) — `LocalFcallInfo`'s lifetime parameter is dead

- **File:** `crates/php-analyze/src/spike.rs:148-167`
- **Severity:** Minor (gives the reader a false signal that
  borrowing is in play; folds into S-1's fix).
- **Description:** `LocalFcallInfo<'a>` has `'a` in its declaration,
  but every `&'a str` inside is actually `&'static str` because
  `zend_string_to_str` returns `'static` (S-1). The lifetime
  parameter therefore documents an intent that isn't true. A reader
  trying to understand the data flow will assume the borrows are
  tied to `&ExecuteData` and write code that relies on the
  compile-time enforcement that isn't there.
- **Suggestion:** Fix together with S-1. If you take S-1's option
  (1) (real lifetime), `'a` becomes meaningful. If you take option
  (2) (own the strings), drop the parameter — `LocalFcallInfo`
  becomes a `'static` struct with `Option<String>` fields.

### S-7 (Minor) — `directive_table_numeric_defaults_match_resolved_config_defaults` doesn't assert the two new spike directives

- **File:** `crates/php-analyze/src/bootstrap.rs:596-673`
- **Severity:** Minor (drift guard gap; the same class of bug R-7
  flagged previously).
- **Description:** The existing test walks the directive table for
  every numeric directive and the token-related strings, but the
  two new entries (`php_analyze.spike_observer` and
  `php_analyze.spike_log_path`) get only an indirect check via
  `rows_include_every_directive_exactly_once`. Nothing asserts that
  the *default-string in `DIRECTIVES`* ("0" / "") resolves to the
  same `Config::spike_observer` / `Config::spike_log_path` that
  `RawIni::default()` produces. A future edit that changes one
  default in `DIRECTIVES` without touching `from_ini_values` (or
  vice versa) compiles green.
- **Suggestion:** Extend the existing test:
  ```rust
  // Spike directives default to off / unset.
  assert_eq!(directive("php_analyze.spike_observer").default, "0");
  assert_eq!(parse_bool("0"), Some(false));
  assert!(!resolved.spike_observer);
  assert_eq!(directive("php_analyze.spike_log_path").default, "");
  assert!(resolved.spike_log_path.is_none());
  ```
  Three lines plus the directive lookups; covers the same drift
  class for the spike directives that R-7's follow-up covers for
  the production ones.

### S-8 (Minor) — `spike_log_path` is documented as "absolute path" but never validated

- **Files:**
  - `crates/php-analyze/src/config.rs:64-68` (struct doc)
  - `README.md:131-133` (operator docs)
  - `crates/php-analyze/src/spike.rs:64-88` (consumer)
- **Severity:** Minor (operator footgun; mitigated by spike being
  dev-only).
- **Description:** The doc comment on `Config::spike_log_path`
  reads "An absolute path means 'create / append to this file'".
  The README states `spike_log_path = "absolute path"` next to the
  type column. But `from_ini_values` accepts any non-empty string
  and `from_config` opens it verbatim — a relative path resolves
  against PHP's cwd at `MINIT`, which is unpredictable under FPM
  (and may not be writable). The operator gets a falls-back-to-stderr
  warning at a path they don't expect.
- **Suggestion:** Either (a) make the validator enforce the doc:
  reject non-absolute paths with a `ConfigWarning::SpikeLogPathNotAbsolute`,
  treating it like a soft misconfig (warn + fall back to stderr), or
  (b) update the doc to "absolute path recommended; relative paths
  resolve against the PHP process cwd". (a) costs ~10 lines and
  removes the foot-gun.

### S-9 (Minor) — Each observed call allocates two `String`s on the hot path

- **File:** `crates/php-analyze/src/spike.rs:119-137`
- **Severity:** Minor (spike-only; explicitly out of scope for
  Phase 0 performance per the module doc, but worth flagging as a
  trap for Phase 2 copy-paste).
- **Description:** `begin` calls `format!("entry: {}", fqn(&info))`,
  and `fqn` itself returns an owned `String`. Each observed entry
  therefore allocates twice (once inside `fqn`, once inside
  `format!`). `end` is the same. The module doc claims the spike is
  "slow, unbounded" and the README repeats the warning, so this is
  acceptable for the spike — but if Phase 2's Recorder copies the
  same shape (it will be tempting; the shape is clear), the hot-path
  zero-alloc assertion AC-RC-5 will fail.
- **Suggestion:** No change needed in the spike. **Add an inline
  comment** near `begin` / `end` like:
  ```rust
  // NOTE for Phase 2: this is two allocations per call. The
  // Recorder's hot path must reuse a thread-local buffer (per
  // AC-RC-5). Do not copy this shape verbatim.
  ```
  Cheap; saves Phase 2 from rediscovering AC-RC-5 the hard way.

### S-10 (Minor) — Integration test's `assert_pair` accepts duplicate hits silently

- **File:** `crates/php-analyze/tests/spike_observer.rs:215-230`
- **Severity:** Minor (false-positive risk for a few categories).
- **Description:** `assert_pair` asserts `entry_hits >= 1` and
  `exit_hits >= 1`. C-5 records that several of the fixtures call
  each function exactly once and the table is `yes (one
  entry/exit)`. If `begin`/`end` ever started double-firing for the
  same call (a real risk if the observer registration changes), the
  test would still pass. For `array_map`, C-5 specifically says the
  callback fires "three times" for `[1, 2, 3]` — so the integration
  test should be asserting `entry_hits == 3` for the closure
  events, not `>= 1`.
- **Suggestion:** Tighten the matchers per fixture: for `only_me()`,
  `(new C)->m()`, `bad()`, the user-closure, and each non-specialised
  internal, assert `entry_hits == 1 && exit_hits == 1`. For the
  `array_map` arrow-fn closure, add a separate assertion that the
  count equals 3. The current loose check buys nothing here and
  silently allows regressions.

### S-11 (Nit) — `tests/php-spike/run.sh` shells out to `python3` for a single JSON field

- **File:** `tests/php-spike/run.sh:32-35`
- **Severity:** Nit (portability; `python3` isn't a documented
  test-host dependency).
- **Description:** The script uses
  `cargo metadata … | python3 -c "import sys, json; ..."` to
  extract `target_directory`. `python3` is not listed in
  `SPECIFICATION.md` §7.1 build-toolchain requirements, nor in
  README §Build. A test-host without `python3` (a minimal Alpine
  CI image, for instance) skips with a cryptic "python3 not found"
  rather than the autotools-style exit 77.
- **Suggestion:** Either (a) use `cargo metadata --no-deps
  --format-version 1 | jq -r .target_directory` (and add `jq` to
  test-host requirements — also not currently there), (b) parse
  the JSON in a here-doc with `python3` *and* document the
  dependency, or (c) compute the target dir with shell:
  ```bash
  TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
  ```
  Option (c) is the cheapest and is correct unless the operator
  is doing something exotic. Add a fall-back guard
  (`command -v python3 >/dev/null || { echo "skipping" >&2; exit
  77; }`) if you keep python.

### S-12 (Nit) — `with_sink` constructor is `#[cfg(test)]` but only used by unit tests, not the integration test

- **File:** `crates/php-analyze/src/spike.rs:93-99`
- **Severity:** Nit (naming/documentation).
- **Description:** The doc says "Used by the `fqn`-and-log unit
  tests below; not part of the production surface." That's correct.
  But it reads as if it might also be reachable from the
  integration test under `tests/spike_observer.rs`. It isn't —
  `tests/` integration crates can't see `#[cfg(test)]` items from
  the library (they get the regular `cargo build` view). The doc
  is true but easy to misread.
- **Suggestion:** Rephrase to "Test-only constructor; visible
  exclusively to the in-module `mod tests` below." One-liner.

### S-13 (Nit) — Doc comment on `should_observe` makes an unverified caching claim

- **Files:**
  - `crates/php-analyze/src/spike.rs:42-48` (struct doc)
  - `crates/php-analyze/src/spike.rs:112-117` (method)
- **Severity:** Nit (documented behaviour rests on an unverified
  assumption).
- **Description:** Both doc blocks state that PHP caches the
  `should_observe` result per unique function (so an inactive
  observer pays one virtual call per unique function, then
  nothing). C-5's evidence shows the observer is hit on every call
  in the fixture, but does not prove the caching claim: the
  fixtures all have `should_observe -> true`. The "cached forever
  when false" claim is plausible from the `ext-php-rs` source
  reading C-5 cites, but no test demonstrates it. If the claim is
  wrong, the production-default cost is one virtual call per
  *observed event*, not one per *unique function* — still cheap,
  but a different cost model than the doc promises.
- **Suggestion:** Either (a) cite the upstream line that
  implements the caching (likely in
  `ext_php_rs::zend::observer`'s C glue), or (b) soften the doc:
  "PHP **may** cache this per unique function; assuming it does,
  the inactive cost is one virtual call per unique function".
  If a Phase-2 spike covers this directly, link from here.

### S-14 (Nit) — `LocalFcallInfo::empty()` is reachable only from `extract_info`'s null-pointer branch

- **File:** `crates/php-analyze/src/spike.rs:157-167`,
  `spike.rs:188-190`
- **Severity:** Nit (dead-ish defensive code; reaches `fqn`'s
  `(unknown)` fallback).
- **Description:** `extract_info` returns
  `LocalFcallInfo::empty()` only when `(*execute_data).func` is
  null. Per the observer-API contract this should never happen for
  a normal `begin`/`end`. If it does, `fqn` then formats
  `function:(unknown):0:(unknown)` or `closure:(unknown):0`, which
  is at least debuggable but is silently misleading.
- **Suggestion:** Log a one-shot `E_NOTICE` ("php-analyze spike: null
  function pointer in observer callback — this should not happen,
  please file an issue") and skip the `write_line` entirely. The
  spike is a diagnostics tool; encountering this case without
  logging it defeats the purpose.

### Specification compliance

- ✅ §10 Phase 0 deliverables — small extension prints `entry:` /
  `exit:` for every call; coverage evidence captured in C-5 with
  PHP-version caveat for 8.3 carried forward as follow-up.
- ✅ §11 R-2 — updated to "Closed for PHP 8.4; partially closed for
  PHP 8.3 (pending verification)", consistent with C-5.
- ✅ §3.5 — two new directives registered at `PHP_INI_SYSTEM` with
  documented defaults; `phpinfo()` renders both plus a banner when
  the spike is on; both default to off.
- ⚠️ §6.3 — token redaction guarantee preserved (test
  `rows_redact_auth_token_even_when_spike_is_enabled` covers the
  spike-on path); but S-3 adds a second potential `E_WARNING`
  source outside the single-startup-warning posture.
- ⚠️ AD-4 / NFR-USE-2 — silent-disable posture honoured for the
  bootstrap layer; the spike layer (S-2 / S-3) breaks the "no
  startup warnings beyond the disable-summary" invariant when a
  stale `spike_log_path` is left in `php.ini`.
- ⚠️ §3.2 AC-RC-5 future risk — the spike's two-allocation hot path
  (S-9) is acceptable for the spike but is a copy-paste trap for
  Phase 2.

### Overall recommendation

**REQUEST CHANGES** for the items that affect operator-visible
behaviour or carry forward into Phase 2 (S-1, S-2, S-3). The rest
(S-4 through S-14) are cheap follow-up items.

Per CLAUDE.md's "one change per branch" rule, the cleanest path is:

1. **On this branch**, before merging:
   - Land S-2 (skip file-open when inactive) and S-3 (route spike
     warnings through the same channel, or downgrade to `E_NOTICE`).
     Both are operator-UX fixes that defend the silent-disable
     posture C-5 leans on.
   - Land S-1 in the type-honest form (option 1). The diff is small
     and the spike then ships a sound lifetime story that Phase 2
     can copy without paying a future audit.

2. **As follow-up OpenSpec changes** (each its own branch):
   - `spike-graceful-degrade-on-missing-config` — S-4.
   - `spike-tidy-fqn-and-deadcode` — S-5, S-6, S-12, S-14.
   - `spike-tighten-integration-assertions` — S-10.
   - `spike-portable-run-sh` — S-11 (or fold into the change that
     adds PHP 8.3 verification, since both touch test harness).
   - `directive-table-spike-defaults-drift-guard` — S-7 (small
     enough to fold into the existing R-7 follow-up
     `lock-readme-directive-table`).
   - `spike-log-path-validate-absolute` — S-8.
   - `spike-doc-cleanup` — S-9 inline comment, S-13 doc
     softening / citation.

Phase 0's acceptance criterion ("Architect confirms `zend_observer`
covers internal calls") is met by C-5's evidence for PHP 8.4. The
explicit PHP 8.3 verification gap is correctly carried in C-5 as a
hard blocker for Phase 2; reaffirming that here so the next reviewer
doesn't reopen R-2 prematurely.

## Review-fix status — 2026-05-21 (round 1 on the spike branch)

S-1, S-2, and S-3 (the reviewer's "REQUEST CHANGES" set for the
spike branch) have been addressed on this same branch
(`feat/spike-zend-observer`) as additional commits under the
existing `spike-zend-observer` OpenSpec change. The archived
change's `tasks.md` §11 records the per-finding work; validation
gates (`fmt`, `clippy --all-targets --all-features -- -D warnings`,
`cargo test --all`) are clean, and the
`PHP_ANALYZE_RUN_SPIKE=1 cargo test --test spike_observer` end-to-end
integration test still passes on PHP 8.4.21. Unit-test count moved
from 46 to 50 (4 new tests covering S-2's gate + the extracted
`open_spike_sink` helper).

### Addressed on this branch

- **S-1** (`zend_string_to_str` returned an unsound `&'static str`) —
  took the type-honest option (1) from the review. The free
  function is now `unsafe fn zend_string_to_str<'a>(zs: *mut
  ffi::zend_string) -> Option<&'a str>`, `extract_info<'a>` is
  parameterised on the input `&'a ExecuteData`, and
  `LocalFcallInfo<'a>`'s lifetime parameter is no longer dead.
  `LocalFcallInfo::empty()` retains its `LocalFcallInfo<'static>`
  return because the most-general lifetime coerces (covariantly)
  into any caller-chosen `'a`. The doc-comment "convenient fiction"
  apology is gone; the lifetime story is now what the type says.
  No new tests are needed for S-1 itself — every fqn test exercises
  `LocalFcallInfo<'static>` (which is the most-general instance),
  and the integration test exercises the live `&'a ExecuteData`
  path. S-6 (the "dead lifetime parameter" finding) is fixed by the
  same change.
- **S-2** (`from_config` opened the log file even with the spike
  inactive) — the constructor now short-circuits to an
  `inactive_sink()` (an `io::sink()`-backed no-op writer) when
  `active = enabled && spike_observer` is `false`. The file-open
  logic is extracted into a pure `open_spike_sink(Option<&Path>) ->
  (Box<dyn Write + Send>, Option<String>)` helper so the warning
  string is unit-testable without invoking `php_error`. Three new
  tests cover the gate:
  - `open_spike_sink_returns_stderr_and_no_warning_when_path_is_none`
  - `open_spike_sink_warns_and_falls_back_when_path_cannot_be_opened`
  - `from_config_with_spike_disabled_does_not_open_the_log_path`
  - `from_config_with_extension_disabled_does_not_open_the_log_path`
  The `php_error` call in `from_config`'s active path is wrapped in
  a `#[cfg(not(test))]` shim (`emit_spike_log_warning`) so the
  test binary does not need to resolve `php_error_docref`. The
  active-with-bad-path warning string is still unit-tested
  via `open_spike_sink`; the wired-up `php_error` call is exercised
  in the `tests/spike_observer.rs` integration test against a real
  PHP process.
- **S-3** (the spike warning could co-exist with the bootstrap
  warning, appearing to violate "single startup `E_WARNING`") — per
  user decision: leave the spike's `E_WARNING` in place and document
  the invariant. With S-2 landed, the spike warning fires only on
  explicit-on AND the bootstrap warning fires only on extension-
  disabled (which forces `active=false`). The two paths are mutually
  exclusive; at most one warning per process. Recorded as **C-6**
  above and mirrored in the `from_config` doc comment.

### Queued as follow-up OpenSpec changes (not this branch)

Listed by reviewer's overall recommendation §2. Each gets its own
change + branch per the "one change per branch" rule.

- **S-4** (`build_spike_observer` panics if `Config::global()` is
  unset). Follow-up `spike-graceful-degrade-on-missing-config`.
  Add a public `SpikeObserver::inactive()` sibling of `from_config`
  and downgrade the `expect` to a `let-else` returning the inactive
  observer. (The private `inactive_sink()` added in this round
  already does the work; the follow-up just promotes it to the
  public API and wires `build_spike_observer` to use it.)
- **S-5** (`fqn` has an unreachable `unwrap_or` after the closure
  branch). Folded into `spike-tidy-fqn-and-deadcode` together with
  S-12 and S-14.
- **S-6** (dead lifetime parameter on `LocalFcallInfo`) — **fixed
  here as part of S-1**. No follow-up needed.
- **S-7** (drift guard does not cover the two new spike
  directives). Follow-up `directive-table-spike-defaults-drift-guard`,
  or fold into the existing R-7 follow-up
  `lock-readme-directive-table`.
- **S-8** (`spike_log_path` documented as absolute but not
  validated). Follow-up `spike-log-path-validate-absolute`.
- **S-9** (each observed call allocates two `String`s on the hot
  path). Inline `// NOTE for Phase 2` comment is the suggested
  output; folded into `spike-doc-cleanup`.
- **S-10** (integration test's `assert_pair` accepts duplicate hits
  silently). Follow-up `spike-tighten-integration-assertions`.
- **S-11** (`tests/php-spike/run.sh` shells out to `python3` for
  one JSON field). Follow-up `spike-portable-run-sh` (or fold into
  the PHP-8.3 verification change since both touch the harness).
- **S-12** (`with_sink` doc comment is easy to misread). Folded
  into `spike-tidy-fqn-and-deadcode`.
- **S-13** (`should_observe` caching claim is unverified). Soften
  the doc or cite the upstream line that implements the caching.
  Folded into `spike-doc-cleanup`.
- **S-14** (`LocalFcallInfo::empty()` is reachable only via the
  null-`func` defensive branch and produces a silently-misleading
  fqn). Folded into `spike-tidy-fqn-and-deadcode`.

## Review — `feat/recorder-clocks-and-types` (2026-05-21)

Code review of the Phase-2 slice 1 substrate (`recorder-clocks-and-types`
change). All findings prefixed **RS-N** (Recorder Substrate). The
branch passes `cargo fmt --check`, `cargo clippy --all-targets
--all-features -- -D warnings`, and `cargo test --all` (64 lib + 1
integration test, all green). The diff is well-scoped (~880 lines
across four new files, no edits to `bootstrap`/`config`/`spike`),
which makes the substrate-only contract from `design.md §D-8` easy
to verify. The findings below are issues to fix on this branch or
fold into the next slice; severity is called out per-finding.

### Issues to fix on this branch

- **RS-1** — **MAJOR / spec discussion needed.** `cpu_times_now_ns`
  uses `getrusage(RUSAGE_SELF, …)` per `SPECIFICATION.md §3.2`'s
  literal wording. `RUSAGE_SELF` returns CPU time **summed across
  every thread of the process**. Once the Phase-4 shipper thread
  exists, a `cpu_u/s_ns` delta computed around a PHP call will
  include whatever CPU the shipper consumed during that interval,
  inflating per-call CPU readings under load. Linux 2.6.26+ exposes
  `RUSAGE_THREAD` which gives per-thread accounting — exactly what
  the recorder wants (the recorder runs on the PHP request thread
  and only that thread's CPU is meaningful for `(t_in, t_out)`-
  scoped CPU). The fix is one constant swap (`RUSAGE_SELF` →
  `RUSAGE_THREAD`) plus a doc-comment update; ideally raise the
  spec issue first and amend `SPECIFICATION.md §3.2` / §7.4
  in the same change. Risk of not fixing: every `cpu_u/s_ns` field
  in the wire format becomes noisy in a way that's only visible
  once shippers exist (Phase 4) and is silently wrong before then.

- **RS-2** — **MAJOR.** `Dictionary::next_fn_id: u32` is incremented
  via `self.next_fn_id += 1`. After 2³² distinct `intern` calls in
  one trace the field overflows: panics in debug, **silently wraps
  to 0** in release, breaking the "0 is the no-function sentinel"
  contract in `dictionary.rs:34-36`. A 4-billion-distinct-function
  trace is unrealistic for PHP, but the contract should not depend
  on workload. Fix options (any one): (a) `saturating_add(1)` plus
  reject further inserts with `panic!` or `Option<u32>` return;
  (b) `checked_add` with `expect`; (c) document the limit and
  promote to `u64` (cheap — `fn_id` is on the wire as `u32` per
  §4.2.2, so the dictionary-internal counter could still be `u32`
  while the wire field caps at the same width). Recommend (b) —
  smallest change, loud failure mode.

- **RS-3** — **MAJOR.** `Trace`'s `buffer`, `buffer_estimated_bytes`,
  `stack`, `dictionary`, `call_id_seq` are all `pub` (`types.rs:192-
  204`). Slice 3 (`recorder-depth-and-cap-drops`) will introduce
  the invariant that `buffer_estimated_bytes` always equals the
  §3.2 estimator applied to `buffer`'s current contents; with `pub`
  fields any caller can desync them by pushing to `buffer` without
  bumping the estimate. Recommend `pub(crate)` for the mutable
  state fields and adding the minimal accessor surface the future
  observer needs (`push_record`, `push_dict_entry_via_intern`,
  `next_call_id`), even if those accessors are one-liners today —
  the type-level enforcement is the point. At minimum, add the
  invariant to `Trace`'s doc comment so slice 3's author cannot
  miss it.

- **RS-4** — **MINOR.** The `extern "C" { fn zend_memory_usage
  (real_usage: bool) -> usize; }` block in `clocks.rs:123-125` is
  not marked `unsafe extern "C"`. The crate is on edition 2021 so
  this compiles cleanly today, but Rust 2024 makes `unsafe extern`
  mandatory on these blocks. One-character fix (`unsafe extern
  "C"`); preserves edition-upgrade ergonomics; the function is
  already called inside an `unsafe { … }` block, so call-site
  semantics don't change.

- **RS-5** — **MINOR.** `clock_gettime_ns` and `cpu_times_now_ns`
  use `debug_assert_eq!(rc, 0, …)` to check the syscall return
  code. In release builds, a non-zero return (e.g., EINVAL for an
  unknown `clockid_t` on an exotic kernel; EFAULT if the kernel
  rejects the pointer) silently produces a zero-filled `timespec`
  / `rusage`, which then becomes a bogus timestamp written into a
  `CallRecord`. The cases are very unlikely on the supported
  target (Linux x86_64 + listed clock IDs), but the cost of a
  hard `assert!(rc == 0)` is one branch on a path that already
  does a syscall — invisible. Recommend hard-assert, matching the
  doc-comment's "infallible" claim.

- **RS-6** — **MINOR.** `monotonic_now_ns_is_non_decreasing_across
  _a_two_millisecond_sleep` (`clocks.rs:188`) bounds the delta to
  `[1ms, 100ms]`. The lower bound catches unit-conversion bugs as
  intended. The 100ms upper bound is more fragile: a paged-out CI
  runner or a `nice`d build agent can pause a thread for >100ms
  without that being a unit-conversion bug. Suggest either (a)
  raise the ceiling to `500ms` or (b) drop the upper bound — the
  lower bound is sufficient evidence the units are nanoseconds,
  and `b >= a` already proves monotonicity. Either avoids a future
  flake that costs more to diagnose than the test is worth.

- **RS-7** — **MINOR.** `PendingBatch` exposes `pub dict`, `pub
  calls`, `pub size_estimate` (`types.rs:158-163`) with no
  constructor or helper to keep `size_estimate` aligned with the
  §3.2 estimator (`64 + len(fqn) + len(file) + 24` per dict
  entry, `64` per call). Slice 3 introduces the estimator; adding
  a `PendingBatch::new(meta_partial, dict, calls)` or a free
  `estimate_batch_bytes(&[DictEntry], &[CallRecord]) -> usize`
  function now removes the chance that slice 3's author updates
  one place and not the other. Optional for this branch, but the
  shape is cheaper to introduce alongside the type than retrofit
  later.

### Nitpicks (suggestions, no fix required)

- **RS-8** — `Trace::new(host: Arc<str>, sapi: Arc<str>, pid: u32,
  uri_or_script: String)` has four positional arguments with two
  `Arc<str>` parameters surrounding a `u32` (`types.rs:214`).
  Easy to swap `host` and `sapi` at a call site without compile-
  time catch. Slice 2 is the first real caller; consider grouping
  the request-identity fields into a small `RequestIdentity`
  struct (or a `TraceParams` builder) when slice 2 lands rather
  than now.

- **RS-9** — `recorder_types_module_does_not_derive_serde_serialize`
  (`types.rs:357`) reads `include_str!("types.rs")` and greps for
  `Serialize`. If the file is split into `types/{mod,call,batch}.rs`
  during a future refactor, the guard silently stops covering the
  new files. Suggest adding the same guard to a top-level
  `tests/no_wire_serde_in_recorder_substrate.rs` integration test
  that walks `crates/php-analyze/src/recorder/` and asserts the
  substring across every file. Defer to whenever the recorder
  module grows past a single file.

- **RS-10** — `cpu_times_now_ns`'s doc claims `getrusage` is
  "documented infallible" for `RUSAGE_SELF` (`clocks.rs:87`).
  POSIX permits `EINVAL` for unsupported `who` and `EFAULT` for
  bad pointer; neither applies here, but "documented infallible"
  is stronger than what POSIX actually guarantees. Soften to
  "infallible on Linux x86_64 for `RUSAGE_SELF` with a valid
  pointer" or similar.

### Positive highlights

Not all review notes are issues — a few decisions in this branch
are notably well-shaped and worth calling out so they don't get
undone by a future refactor:

- **Lazy-allocate `intern` API**: `Dictionary::intern(key, build:
  impl FnOnce(u32) -> DictEntry)` (`dictionary.rs:49`) is the
  right shape — the closure only fires on a miss, so the
  `DictEntry` (which owns two `String`s) is never built for a
  hit. This is exactly the hot-path discipline §10 Phase 5 will
  thank you for. Resist any future refactor that takes
  `entry: DictEntry` eagerly.

- **`cfg(test)` stub for `memory_usage_real_bytes`** (`clocks.rs:
  121-140`) keeps `cargo test` PHP-free, preserving the crate-wide
  invariant established by `config.rs`/`bootstrap.rs`'s shims. The
  PHP-fixture coverage of the real symbol is correctly deferred to
  slice 2.

- **Negative-derive sentry** (`types.rs:357-376`) — using a
  source-grep test to enforce "no `serde` derives in this slice"
  is a creative way to keep an architectural boundary at `cargo
  test` time without LSP / proc-macro reflection. Even with the
  RS-9 caveat above, this is a good pattern.

- **Diff size + scope** stays within CLAUDE.md's "few hundred
  lines of meaningful diff" guidance (~880 lines including ~400
  lines of tests across three new modules) and faithfully respects
  the substrate-only contract from `design.md §D-8` — verified by
  `git diff main -- '...bootstrap.rs' '...spike.rs' '...config.rs'`
  returning empty, matching task §5.1–§5.3.

### Recommendation

**REQUEST CHANGES** — the branch is structurally sound and well-
tested, but **RS-1**, **RS-2**, and **RS-3** are contracts that
will silently fail under realistic workloads once later phases
land; cheaper to fix here than to chase later. **RS-1** in
particular benefits from being raised against the spec first
(`SPECIFICATION.md §3.2` literally says `RUSAGE_SELF`), so a
single change can amend both the spec and the wrapper. **RS-4**
through **RS-7** are minor and can be batched into the same
review-fix commit. **RS-8** through **RS-10** are deferrable.

### Round-2 fix status — `feat/recorder-clocks-and-types` (2026-05-21)

Fixes applied on the same branch, same OpenSpec change
(`recorder-clocks-and-types`), per the precedent set by the
bootstrap and spike branches' round-1 fix commits. All checks
green afterwards: `cargo fmt --check`, `cargo clippy --all-
targets --all-features -- -D warnings`, `cargo test --all`
(68 lib tests, 4 new from this round; 1 integration test, soft-
skipped), and `openspec validate recorder-clocks-and-types`.

| ID | Severity | Outcome | Notes |
| --- | --- | --- | --- |
| RS-1 | MAJOR | **Fixed** | `cpu_times_now_ns` now passes `libc::RUSAGE_THREAD`. `SPECIFICATION.md §3.2` (line 211) and `§7.4` (Permissions row) amended in the same commit per the reviewer's recommendation and the user's confirmation. `clocks.rs` module doc + `cpu_times_now_ns` doc explain the choice and cite the spec amendment. |
| RS-2 | MAJOR | **Fixed** | `Dictionary::intern` now uses `checked_add(1).expect(...)` for `next_fn_id`. The `expect` message names the 2^32 contract explicitly. |
| RS-3 | MAJOR | **Fixed** | `Trace`'s mutable state fields (`call_id_seq`, `stack`, `buffer`, `dictionary`, `buffer_estimated_bytes`) demoted to `pub(crate)`. Added the accessor surface the reviewer asked for: `Trace::next_call_id`, `Trace::push_record`, `Trace::push_dict_entry_via_intern`. Each accessor establishes the §3.2 estimator invariant for its slice of the state (per-record `+= 64`, per-dict-miss `+= 24 + len(fqn) + len(file)`). The `stack` field is annotated `#[allow(dead_code)]` with a comment naming slice 2 as the first reader — the alternative was to add a `pop_frame` accessor in this slice, which would have been scope creep. The class-level invariant is documented on `Trace`'s doc comment so slice 3 cannot miss it. |
| RS-4 | MINOR | **Fixed** | `extern "C" { fn zend_memory_usage … }` → `unsafe extern "C" { … }` with a comment noting Rust 2024 forward-compat. |
| RS-5 | MINOR | **Fixed** | Both `debug_assert_eq!(rc, 0, …)` sites in `clock_gettime_ns` and `cpu_times_now_ns` promoted to `assert!(rc == 0, …)`. Comments explain the loud-failure rationale. |
| RS-6 | MINOR | **Fixed** | `monotonic_now_ns_is_non_decreasing_across_a_two_millisecond_sleep` upper bound dropped. The 1 ms lower bound is retained because it is what catches unit-conversion bugs; `b >= a` already proves monotonicity. |
| RS-7 | MINOR | **Fixed** | Added `pub fn estimate_batch_bytes(dict: &[DictEntry], calls: &[CallRecord]) -> usize` plus `pub(crate) const CALL_RECORD_FIXED_BYTES`/`DICT_ENTRY_FIXED_BYTES`. `PendingBatch` and `Trace`'s accessors now share the same formula source. `PendingBatch`'s doc comment documents the `size_estimate == estimate_batch_bytes(...)` invariant for slice-3's future `flush_into_batch` accessor. |
| RS-8 | NIT | **Deferred** | Grouping `Trace::new`'s positional args into a `RequestIdentity` struct deferred to slice 2 per the reviewer's own suggestion — slice 2 is the first non-test caller. |
| RS-9 | NIT | **Deferred** | The `recorder_types_module_does_not_derive_serde_serialize` guard's coverage only matters once `types.rs` is split into multiple files. Tracked as a Phase-3 follow-up alongside the wire-format change that would do the splitting. |
| RS-10 | NIT | **Fixed** | `cpu_times_now_ns` doc no longer says "documented infallible"; it now says "infallible on Linux x86_64 for the supported `who` constant with a valid pointer". Done in the same edit as RS-1 because both touched the same doc paragraph. |

Four new tests landed in `recorder::types::tests`:

- `trace_next_call_id_is_monotonic_from_one` — exercises `Trace::next_call_id`'s monotonicity-from-1 contract.
- `trace_push_record_appends_to_buffer_and_bumps_the_estimate_by_64` — pins the per-record estimator contribution.
- `trace_push_dict_entry_via_intern_bumps_estimate_only_on_a_miss` — exercises the miss-vs-hit estimator contribution AND verifies the lazy-allocate hot-path discipline the reviewer asked us to keep.
- `estimate_batch_bytes_matches_the_spec_3_2_formula` — exercises the free function with both a populated and an empty batch.

Spec amendments:

- `SPECIFICATION.md §3.2` — `RUSAGE_SELF` → `RUSAGE_THREAD` with a sentence explaining why (shipper-thread isolation) and the kernel-version note (Linux 2.6.26+, comfortably below the §7.4 ≥4.4 floor).
- `SPECIFICATION.md §7.4` Permissions row — same constant swap.


## Slice-2 deviations and verification

### C-7 — PHP 8.3 verification (slice 2 outcome)

Closes the C-5 follow-up "Phase 2's Recorder change MUST include 8.3
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
is needed in slice 2 beyond the integration test's `throws.php`
fixture (which the slice-2 harness exercises against every available
PHP version).

---

## Code review — 2026-05-21 (branch `feat/recorder-observer-hooks-and-trace-lifecycle`)

**Reviewer:** Claude Code
**Reviewed against:** `SPECIFICATION.md` §3.1 (Bootstrapper), §3.2 (Recorder),
§4.1 (in-memory types), §8.3 NFR-REL-1 (never crash PHP), §10 Phase 2 deliverables.
**Scope:** all commits on `feat/recorder-observer-hooks-and-trace-lifecycle`
vs. `main` (`cd7cfaf`, `8ff4e08`, `c0487e4`, `429ec03`, `7e8949a`).
`cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
warnings`, and `cargo test --all` (96 lib + 1 integration test, 0
skipped) are clean locally.

Findings prefixed **RO-N** (Recorder Observer). The branch is
well-scoped (~2300 lines of diff dominated by `observer.rs` +
`dump.rs` and ~700 lines of tests), the substrate-only contract from
slice 1 is respected (no field-visibility regressions on `Trace`),
and the `BootObserver` dispatcher cleanly fans out to the three
runtime variants without coupling the Recorder to the Spike. The
issues below are mostly correctness and silent-disable-posture
defenses to land before slice 3 builds on top.

### Issues to fix on this branch

- **RO-1** — **CRITICAL.** Panic across the `extern "C"` FFI
  boundary aborts the PHP process, violating NFR-REL-1
  ("never crash the PHP process"). Three observed panic sites
  on the `bootstrap::rinit` path:

  1. `recorder::rinit_allocate_trace` (`observer.rs:66-76`)
     hard-asserts that the thread-local slot is `None`. Any
     RINIT-without-RSHUTDOWN pairing failure (FPM worker
     interrupted mid-request, a future change that forgets to
     wire `rshutdown`, etc.) hits `assert!(borrow.is_none(), …)`.
  2. `Trace::new` (via `recorder::types.rs:295-315`) allocates
     `Arc<str>`s on the request thread; OOM aborts at the
     allocator are panics in Rust.
  3. `clocks::cpu_times_now_ns` and `clocks::clock_gettime_ns`
     hard-assert their syscall return codes (`clocks.rs:106-110`,
     promoted from `debug_assert!` per RS-5). On the supported
     target they are infallible, but the contract is "if it ever
     happens, abort". A `rinit` invocation that reaches the
     assert-failure path takes down the PHP worker.

  Since Rust 1.71 a panic crossing `extern "C"` aborts the
  process (no longer UB, but the failure mode is exactly the
  silent-disable posture is meant to prevent). Spec §1.4 OQ-9 /
  AD-4 and the whole `bootstrap.rs` design lean on "PHP keeps
  running even when we can't". Aborting the worker on a
  recorder bug means one bad request takes a whole FPM child
  out — and on CLI it means the user's script dies with
  `abort()` rather than completing.

  **Suggestion (cheapest):** downgrade the `assert!` in
  `rinit_allocate_trace` to a `debug_assert!`, and on the
  release path drop the stale `Trace` silently (or log one
  `E_NOTICE`):

  ```rust
  pub fn rinit_allocate_trace(identity: RequestIdentity) {
      CURRENT_TRACE.with(|slot| {
          let mut borrow = slot.borrow_mut();
          debug_assert!(
              borrow.is_none(),
              "RINIT without RSHUTDOWN: previous request leaked a Trace",
          );
          // Release-path recovery: drop the stale Trace; the leak is
          // visible via dropped_records (slice 3 onwards).
          *borrow = Some(Trace::new(identity));
      });
  }
  ```

  **Suggestion (defense in depth):** wrap the bodies of
  `bootstrap::rinit`, `bootstrap::rshutdown`, and
  `bootstrap::mshutdown` in `std::panic::catch_unwind`. Any
  panic anywhere downstream is caught at the FFI boundary,
  the extension self-disables for the rest of the request, the
  PHP process keeps running. The first such catch should also
  bump a future drop-counter (slice 3) so the operator sees
  it.

  The `double_rinit_without_rshutdown_panics` test
  (`observer.rs:699`) should be rewritten against the
  debug-only behaviour (`#[cfg(debug_assertions)]
  #[should_panic(...)]`) so the release-build invariant
  ("drop the stale slot, do not crash") gets its own test.

- **RO-2** — **MAJOR.** `recorder_observer.rs:67`
  (`std::process::exit(77)`) is the autotools "skip" convention,
  but `cargo test` treats any non-zero exit code as a failed
  test — and `std::process::exit` terminates the whole test
  binary process, taking out the result reporting along with
  it. The intent ("skip when no PHP found") therefore becomes
  "fail" the moment a developer runs `PHP_ANALYZE_RUN_RECORDER=1
  cargo test --test recorder_observer` on a host that doesn't
  match the cdylib's php-config. In CI the matrix entry
  installs the matching `php<v>`, so the path isn't reached
  there — but on local rebuilds with the `update-alternatives`
  pinned to a different version (which the test even mentions
  in its own message), this fails confusingly.
  - **Suggestion:** replace `std::process::exit(77)` with
    `eprintln!(...); return;`. The test then passes loudly as
    a skip, mirroring the `PHP_ANALYZE_RUN_RECORDER != "1"`
    branch above it. The risk of silently passing when PHP is
    missing in CI is bounded: CI's apt-install step would have
    failed first.
  - **Alternative:** if the project later wants a stricter "we
    really exercised PHP" signal, add a separate
    `#[ignore]`d test that fails when `available.is_empty()`,
    and require it to be unignored on CI via `--include-ignored`.

- **RO-3** — **MAJOR.** `end_on_empty_stack_is_a_silent_noop_in_release`
  (`observer.rs:992-1014`) early-returns on `cfg!(debug_assertions)`,
  which is **always** true under the default `cargo test`. The
  test therefore executes only its `if cfg!(debug_assertions)`
  arm — i.e. it does nothing. The CI doesn't run
  `cargo test --release` (the workflow only runs `cargo test
  --all`), so the release-mode silent-noop contract on
  `end_with_snapshots` has **zero** automated coverage. The
  test passes via vacuous truth.
  - **Suggestion:** restructure the test so the release
    behaviour is reachable from debug builds. Two options:
    1. Replace the `debug_assert!` in `end_with_snapshots`
       with a runtime-toggleable counter that the test reads
       (e.g., a `#[cfg(any(test, feature = "...")]
       AtomicUsize`). Then a normal debug `cargo test` exercises
       the silent-noop path.
    2. Extract the post-pop logic into a separate function that
       takes `Option<CallFrame>` and exercise the `None` arm
       directly; keep the `debug_assert!` in the caller for
       in-place callers.
    Option 2 is the smaller delta and gives the test back its
    point.

- **RO-4** — **MAJOR.** `zend_string_to_str`
  (`observer.rs:257-265`) returns `None` when the bytes are not
  valid UTF-8. The PHP filename surface is **not** UTF-8 on
  Linux — filesystem paths are arbitrary bytes, and a project
  with non-ASCII characters in a path that doesn't normalise to
  UTF-8 will have its `file` field disappear. `categorise` then
  falls back to `file: ""`, and the closure-vs-function
  precedence rule depends on whether `filename.is_some()` —
  which it isn't after the silent drop, so a non-UTF-8-file
  closure routes to the function branch instead. Two distinct
  closures in two non-UTF-8 files collapse into the same
  `Function { file: Arc::from(""), function: Arc::from(name),
  line: 0 }` key once `function_name` is also lost. End result:
  per-call counts in the dictionary undercount distinct
  functions for the affected files, and the wire layer (Phase
  3) sees a malformed picture without any signal that
  conversion happened.
  - **Suggestion:** use `String::from_utf8_lossy(&slice).into_owned()`
    (returning `Cow<'a, str>`) so non-UTF-8 bytes become the
    Unicode replacement character `U+FFFD` rather than vanishing.
    The categorisation then sees a present-but-corrupted name,
    and dictionary collisions still happen for distinct paths
    only when their normalised forms match (rare). Alternatively
    return `Option<&'a [u8]>` and let the categoriser decide —
    but that bleeds non-UTF-8 into the rest of the recorder,
    which the §4.2 wire format will reject.
  - **Test gap:** there is no test for non-UTF-8 file/function
    paths today. Add one against `zend_string_to_str` with a
    fabricated `zend_string` whose payload is `[0xFF, 0xFF]`,
    asserting the chosen behaviour.

- **RO-5** — **MAJOR.** `categorise` (`observer.rs:298-366`)
  papers over Zend reporting gaps with `unwrap_or("(unknown)")`
  / `unwrap_or("(anonymous)")` placeholder strings, then builds
  the `FunctionKey` from those placeholders. Every distinct
  unknown-function reaches the **same** `Internal { name:
  Arc::from("(anonymous)") }` or `Function { ..., function:
  Arc::from("(unknown)"), ... }` key. The dictionary then
  treats them as one call site; `flat_calls.php`-style counts
  for the affected functions become wrong by exactly that
  collision factor. RO-4 makes this worse: a UTF-8 drop on a
  filename collapses the file component to `""` too.
  - **Suggestion:** the placeholders are operator-debugging
    aids — give them call-site-distinguishing identity. Cheapest:
    incorporate the `execute_data` raw pointer as a tiebreaker
    in the synthesised `function_name`/`file`, e.g.
    `Arc::from(format!("(unknown)@{:p}", execute_data))`. That
    collides only on the same Zend reuse of memory, which is
    rare within one request. Even better: log a one-shot
    `E_NOTICE` ("php-analyze: encountered a Zend function with
    no name; falling back to pointer-based identity") so the
    operator can flag the upstream.
  - **Alternative:** if the consensus is "Zend never produces
    this shape", change the `unwrap_or` to an early-return that
    skips the begin-frame push entirely AND bumps a future
    drop-counter; the call is then absent from the trace rather
    than fabricated.
  - **Test gap:** the existing
    `categorise_handles_missing_line_and_missing_file_gracefully`
    test asserts the **shape** of the placeholder fallback but
    doesn't assert against collision (it creates exactly one
    such call). Adding a test that categorises two distinct
    `info` values with the same placeholder shape and observes
    they collide would surface the bug and document the
    deliberate decision (whichever way it goes).

- **RO-6** — **MAJOR.** `Recorder::begin_handler`
  (`observer.rs:387-400`) captures the snapshots **before**
  checking whether a `Trace` exists in the thread-local slot.
  When the slot is empty — which happens for every observer
  fire between `MINIT` and the first `RINIT`, and for any future
  out-of-request fire — the cost is wasted: one
  `clock_gettime(CLOCK_MONOTONIC)`, one `getrusage(RUSAGE_THREAD)`,
  one `zend_memory_usage(true)`, AND the `extract_fcall_info`
  pointer-walk. The module doc comment specifically calls out
  why `should_observe` returns `true` unconditionally (so PHP
  doesn't cache `false` for slot-empty fires), which guarantees
  every observed function hits this code path at MINIT-time on
  first sight. The same applies in reverse at `end_handler`.
  - **Suggestion:** check slot presence first; capture only
    when there's somewhere for the data to go:
    ```rust
    fn begin_handler(&self, execute_data: &ExecuteData) {
        // Avoid the syscall trio when there's no trace to fill.
        let has_trace = CURRENT_TRACE.with(|s| s.borrow().is_some());
        if !has_trace { return; }
        let info = unsafe { extract_fcall_info(execute_data) };
        let snapshots = EntrySnapshots::capture_now();
        with_current_trace(|trace| {
            let categorised = categorise(&info);
            begin_with_snapshots(trace, &categorised, snapshots);
        });
    }
    ```
    The double-borrow is cheap (the second `borrow_mut` only
    contends with the first if the recorder ever re-enters,
    which it doesn't). Alternatively, fuse the check and the
    work into a single closure call by deferring the snapshot
    capture into the closure body:
    ```rust
    with_current_trace(|trace| {
        let info = unsafe { extract_fcall_info(execute_data) };
        let snapshots = EntrySnapshots::capture_now();
        let categorised = categorise(&info);
        begin_with_snapshots(trace, &categorised, snapshots);
    });
    ```
    This adds two-Zend-deref's worth of latency between PHP's
    observer-fire and the wall-clock read, which is the
    accuracy concern; the design notes don't quantify how much
    that matters for slice 2.

### Nitpicks (suggestions, no fix required)

- **RO-7** — **MINOR.** `EntrySnapshots::capture_now` reads
  CPU *before* monotonic; `ExitSnapshots::capture_now` likewise
  reads CPU before monotonic. The cpu_window for one call is
  therefore `T_cpu_in → T_cpu_out`; the wall_window is
  `T_wall_in → T_wall_out`. Since both `T_cpu_*` precede their
  respective `T_wall_*`, the cpu window is **shifted left** of
  the wall window, not contained in it. The principled order
  for "CPU should not exceed wall" is: wall first on entry
  (`T_wall_in < T_cpu_in`), CPU first on exit (`T_cpu_out <
  T_wall_out`) — so the cpu window strictly sits inside the
  wall window and `cpu_delta ≤ wall_delta` is guaranteed by
  ordering. The saturating-sub `max(0)` in `end_with_snapshots`
  defends against measurement skew already, so the practical
  impact is small, but the asymmetry is a footgun for a future
  reader debugging "why does cpu exceed wall sometimes".
  - **Suggestion:** flip the order inside `EntrySnapshots::capture_now`
    (wall, then cpu, then mem) and keep `ExitSnapshots::capture_now`
    the same — or vice versa. Document the chosen direction in the
    type doc comment. Costless; pure ordering.

- **RO-8** — **MINOR.** `bootstrap::read_hostname` runs
  `gethostname(3)` plus a `Vec<u8>` allocation on every
  `RINIT`. The host name is constant for the life of the
  process. On a long-lived FPM worker handling thousands of
  requests, that's thousands of redundant syscalls + allocations.
  - **Suggestion:** cache once via `OnceLock<Arc<str>>` populated
    in `bootstrap::startup` (MINIT) or lazily on first read.
    `RequestIdentity::host` can then become a clone of the
    cached `Arc<str>`. The structure already uses `Arc<str>` for
    `host`, so the swap is local.
  - **Side benefit:** the slice-3 `dropped_records` accounting
    will want every RINIT to be as cheap as possible to keep
    the "extension disabled" comparison fair.

- **RO-9** — **MINOR.** `bootstrap.rs:281`'s
  `buf.as_mut_ptr().cast::<i8>()` hard-codes `c_char = i8`,
  which holds on Linux x86_64 but breaks on aarch64 where
  `c_char = u8`. The crate is x86_64-only per
  `SPECIFICATION.md` §7.4, so the code compiles on the
  supported target — but a future aarch64 target lift will hit
  a type mismatch under `clippy::cast_sign_loss` or similar.
  - **Suggestion:** use `buf.as_mut_ptr().cast::<libc::c_char>()`.
    Same generated assembly on x86_64, portable across
    `c_char` flavours, no clippy adjustment needed if the
    project ever broadens its target.

- **RO-10** — **MINOR.** `BootObserver::Disabled`'s
  `begin`/`end` arms are empty match-arms (`observer.rs:553-565`).
  Because the same variant's `should_observe` returns `false`,
  PHP caches "don't observe" per unique function on first sight
  — so `begin` / `end` for `Disabled` are unreachable after the
  first call to `should_observe`. The arms are defensive but
  also dead code in steady state.
  - **Suggestion:** if you want to keep them as documentation,
    add an inline comment explaining "should_observe → false
    means these arms are only reached for the first per-function
    fire, then never again". If you'd rather lean on the
    invariant, leave the empty match arm; clippy doesn't flag
    it. Either is fine. The current shape is correct; the
    comment is what's missing.

- **RO-11** — **MINOR.** `tests/php-recorder/run.sh:45`
  unconditionally invokes `cargo build -p php-analyze --features
  recorder-dump` on every fixture invocation. Cargo no-ops when
  up-to-date, but the no-op still spawns the build script and
  walks the dep graph (~200ms on a warm cache). Across three
  fixtures × two PHP versions in CI = six redundant builds per
  matrix entry.
  - **Suggestion:** the Rust integration test
    (`recorder_observer.rs`) already iterates over the
    fixtures; have the test invoke `cargo build` once via
    `Command::new(env!("CARGO")).args([...])` before the
    fixture loop, and have `run.sh` skip the build step if the
    cdylib exists. The shell harness becomes "stage the ini
    file, run PHP, report" only.
  - **Alternative:** put `cargo build --features recorder-dump`
    in CI explicitly before the test step (the workflow has a
    dedicated `cargo build --features recorder-dump` step
    already at line 86; let `run.sh` lean on it being run
    first, or have `run.sh` guard the build with a
    `[[ -f "$CDYLIB" ]] || cargo build …`).

- **RO-12** — **MINOR.** `recorder::dump::write_trace_if_path_set`
  (`dump.rs:59-66`) swallows I/O errors via `eprintln!`. Under
  `cargo test`, stderr is captured per-test and only printed on
  failure — so a dump-file write that silently fails would not
  surface in the test output unless the test also asserts on
  the dump's existence (which `run.sh` does via the
  `RSHUTDOWN:` marker check). Robust by design, but the
  `eprintln!` is misleading: it suggests the operator will see
  the error, when in `cargo test` they will not.
  - **Suggestion:** when `cfg!(test)` is true, replace
    `eprintln!` with a `panic!` so the test fails loudly. The
    `dump` module is already `#[cfg(feature = "recorder-dump")]`,
    so the test-only escalation costs nothing in production
    builds.
  - **Alternative:** return the `io::Result` from
    `write_trace_if_path_set` and have the caller in
    `rshutdown_release_trace` decide what to do (current
    callers ignore it).

- **RO-13** — **NIT.** `Recorder::end_handler`'s arguments are
  named `_execute_data: &ExecuteData, _retval: Option<&Zval>`
  (`observer.rs:405`). The matching `FcallObserver::end` impl
  at `observer.rs:429-431` names them `execute_data, retval`
  (no leading underscore). The two names refer to the same
  parameter through the dispatch indirection; consistency would
  ease grep-readability. Pick one convention crate-wide
  (`_execute_data` is the clippy-canonical form for an unused
  parameter; in trait impls the underscore is conventionally
  dropped because the parameter is part of the trait
  signature).

- **RO-14** — **NIT.** `dump.rs:64`'s
  `eprintln!("recorder::dump: failed to write {:?}: {err}", path)`
  uses the `Debug` formatter on `PathBuf`. For paths containing
  unusual characters this prints the escaped form, which is
  fine for diagnostics, but most operator-facing log lines in
  this crate use `Display` (`{}`). Trivial style alignment;
  swap `{:?}` to `{}` if you want consistency.

### Positive highlights

- **`with_current_trace` accessor design** (`observer.rs:112-114`)
  — a single named entry point for "borrow the thread-local
  trace mutably" with a clear contract documented at the call
  site. The non-recursive borrow invariant is stated up front
  and the `RefCell` panic is reframed as a bug-signal rather
  than something to defend against. Slice-3's depth/cap
  enforcement should plug into this accessor without growing
  the surface.

- **`TraceGuard` test pattern** (`observer.rs:654-667`) —
  using `Drop` to reset the thread-local on test unwind is
  exactly the right shape for test hygiene when a panic
  inside a test body might otherwise leave global state
  populated for the next test on the same thread. Future
  recorder tests should reuse this guard.

- **`BootObserver` enum-dispatch over trait object** —
  picking variants at MINIT and dispatching via a
  `match self` over the three variants is faster than a
  `Box<dyn FcallObserver>` (one discriminant load and a
  jump-table vs. an indirect virtual call), AND it makes the
  set of possible runtime configurations exhaustively
  visible in the source. Resist any future refactor that
  hides this behind dynamic dispatch.

- **`RequestIdentity` struct replacing four positional args**
  — the slice-1 RS-8 finding asked for exactly this and the
  follow-through is clean (named-field construction at the
  one non-test caller; clone-friendly `Debug + Clone`
  derives; doc comment explicitly cites the rationale).
  Documented in C-7 / the spec amendment too.

- **`recorder-dump` Cargo feature** — making the diagnostic
  module strictly opt-in keeps the production cdylib smaller
  AND prevents a future change from accidentally calling
  `write_trace_if_path_set` from a non-test path. The feature
  gate, the `pub(crate)` `new_entries_for_dump` accessor on
  `Dictionary`, and the conditional re-exports in
  `recorder/mod.rs` all line up consistently.

- **The integration harness handles module-API mismatch
  gracefully** — `run.sh`'s `module API`-grep exit-77 plus
  `recorder_observer.rs`'s per-binary skip iteration is the
  right shape for a multi-PHP test matrix; the CI workflow
  pins the matching `php-config` per matrix entry so both
  paths actually get exercised in the binding-evidence CI
  run.

- **`C-7` / `C-8` deviation notes** — both close the PHP-8.3
  follow-up from C-5 and document the `FcallObserver::end`
  API surprise that was raised against the wrong upstream
  signature. The narrative reads as "we discovered the real
  API late, amended the design, and the implementation
  matches" — exactly what `COMMENTS.md` is for.

### Specification compliance

- ✅ §3.1 Bootstrapper — `RINIT` allocates the `Trace`, `RSHUTDOWN`
  releases it; `MINFO`/`MSHUTDOWN` unchanged from Phase 1.
- ✅ §3.2 Recorder — begin/end handlers wired; per-call
  metrics captured per OQ-8 / amended §3.2 (RUSAGE_THREAD);
  exception detection via `ExecutorGlobals::has_exception()`
  (deviation C-8).
- ✅ §4.1 in-memory types — `RequestIdentity` added per RS-8;
  `Trace::new` single-arg; mutable state stays `pub(crate)`
  behind accessors.
- ⚠️ §3.2 AC-RC-1 ("exactly N records modulo `max_depth` /
  `buffer_cap_bytes` drops") — slice 2 explicitly defers
  `max_depth` and `buffer_cap_bytes` to slice 3. Acceptable.
- ⚠️ §3.2 AC-RC-3 (recursive >max_depth → no crash, overflow
  counted) — also deferred to slice 3.
- ⚠️ §8.3 NFR-REL-1 / AD-4 (silent-disable, never crash PHP)
  — see RO-1: the `assert!`-in-`rinit` and downstream
  syscall asserts can abort the worker. This is the primary
  hard finding.
- ⚠️ §3.2 AC-RC-4 (after RSHUTDOWN the per-trace state is
  deallocated) — slice 2 discards the buffer at RSHUTDOWN
  pending Phase 4's shipper handoff (documented in the
  `rshutdown_release_trace` doc comment). The discard works
  as advertised; coverage is via the `RSHUTDOWN:` marker in
  the dump.
- ⚠️ §3.2 AC-RC-5 ("Hot path performs zero heap allocations
  in steady state") — slice 2 still allocates two `String`s
  per dictionary miss in `begin_with_snapshots`
  (`observer.rs:454-468`), correctly flagged inline as "do
  not copy this shape into Phase 5". Acceptable for slice 2;
  the comment is the right output.

### Overall recommendation

**REQUEST CHANGES.** The branch is structurally sound and
the tests landed are strong, but **RO-1** is a direct hit on
NFR-REL-1: the `assert!` in `rinit_allocate_trace` will
crash PHP rather than self-disable on a recoverable
condition. **RO-2** and **RO-3** are test-coverage holes
that quietly pass today; both are cheap to fix on this
branch. **RO-4** and **RO-5** are correctness defenses
against malformed Zend reporting that the wire format
(Phase 3) cannot tolerate. **RO-6** is the cheapest perf
fix in the set. **RO-7** through **RO-14** are
deferrable.

The cleanest path is to land **RO-1**, **RO-2**, **RO-3**,
**RO-4**, **RO-5**, **RO-6** on this branch as a fix-round
(same OpenSpec change, per the precedent set by earlier
slices' round-1 fix commits), and queue the rest as
follow-ups:

- `recorder-rinit-catch-unwind` — RO-1 (catch_unwind +
  debug_assert downgrade).
- `recorder-observer-test-cleanups` — RO-2 + RO-3 (replace
  exit-77 with return; restructure release-mode test to be
  reachable from debug).
- `recorder-utf8-and-identity-defenses` — RO-4 + RO-5
  (lossy UTF-8 decode + pointer-tiebreaker on
  unknown-function fallback).
- `recorder-skip-snapshots-when-slot-empty` — RO-6.
- `recorder-clock-ordering` — RO-7 (flip CPU/wall order
  inside the snapshot constructors).
- `recorder-cache-hostname` — RO-8.
- `recorder-portable-c-char` — RO-9.
- `recorder-bootobserver-disabled-doc` — RO-10.
- `recorder-driver-build-once` — RO-11.
- `recorder-dump-loud-failure-in-tests` — RO-12.
- `recorder-style-cleanups` — RO-13 + RO-14.

Slice 3 (`recorder-depth-and-cap-drops`) does **not** depend
on any of RO-7 through RO-14, so those can move in parallel
once RO-1..RO-6 land. RO-1 is the only one that affects the
NFR-REL-1 posture, so it should be the first commit on the
fix-round.

---

## C-9: Round-2 review-fix status (branch `feat/recorder-observer-hooks-and-trace-lifecycle`)

**Date:** 2026-05-21
**Reviewer findings:** RO-1 … RO-14 (see above)
**Implementer response:** the six MAJOR / CRITICAL findings landed
on the same branch as a fix-round (per the precedent set by
`recorder-clocks-and-types`'s round-1 fix-commit `fb459ad` and
the spike's round-1 fix-commit `2d2fe05`). The eight nitpicks
(RO-7 … RO-14) are deferred to follow-up changes; the
review's own follow-up list above is the canonical queue.

### What changed on this branch in the fix-round

| ID | Status | Implementation |
| --- | --- | --- |
| RO-1 | Closed | `bootstrap::rinit` / `rshutdown` / `mshutdown` bodies wrapped in `std::panic::catch_unwind` so any downstream panic — including the now-`debug_assert!` pairing check in `rinit_allocate_trace` — is contained at the FFI frame instead of aborting PHP. The previous `assert!` becomes a `debug_assert!`; release builds silently drop the stale `Trace` and install the fresh one. Two tests pin the new contract: the debug-only `double_rinit_without_rshutdown_panics_in_debug_builds` (kept for the loud-in-tests posture) and the release-only `double_rinit_without_rshutdown_replaces_the_stale_trace_in_release_builds` (proves the recovery path). |
| RO-2 | Closed | `tests/recorder_observer.rs` replaces `std::process::exit(77)` with `eprintln!(...); return;` so a host without `php8.3`/`php8.4` produces a `cargo test`-recognised pass-as-skip rather than a process-terminating non-zero exit. |
| RO-3 | Closed | `end_with_snapshots` now delegates the post-pop work to a new helper `finish_call_record(trace, Option<CallFrame>, …)`. The empty-stack contract is testable from a default `cargo test` run by calling `finish_call_record(&mut trace, None, …)` directly; the `debug_assert!` in `end_with_snapshots` remains as the loud-in-tests pairing signal. Two new tests replace the vacuous `cfg!(debug_assertions)` early-return: `finish_call_record_with_no_frame_is_a_silent_noop` and `finish_call_record_with_a_frame_emits_a_record_with_the_frame_fields`. |
| RO-4 | Closed | `zend_string_to_str` (`Option<&'a str>`) → `zend_string_to_cow` (`Option<Cow<'a, str>>`). Common-case UTF-8 names stay zero-copy; non-UTF-8 payloads become `Cow::Owned(String)` with U+FFFD substituted via `String::from_utf8_lossy`. The intermediate carrier changed from upstream `FcallInfo<'a>` (whose fields are `Option<&'a str>`) to a recorder-owned `RawCallSite<'a>` with `Option<Cow<'a, str>>` fields; `categorise` and `Categorised<'a>::file` were widened to `Cow<'a, str>` to match. Two new tests pin both arms of the helper: `zend_string_to_cow_replaces_invalid_utf8_bytes_with_replacement_char` and `zend_string_to_cow_returns_a_zero_copy_borrow_for_valid_utf8`. |
| RO-5 | Closed | The synthesised placeholder names `(unknown)` / `(anonymous)` now incorporate the `execute_data` pointer as a tiebreaker via `unknown_placeholder(kind, addr) → "({kind})@0x{hex}"`. Distinct call sites no longer collapse to one `FunctionKey`; the only remaining collision mode is Zend's reuse of the same `execute_data` slot inside one request, which is bounded and recognisable. Two new tests: `categorise_unknown_fallback_uses_execute_data_addr_as_tiebreaker` (function branch) and `categorise_internal_with_no_name_uses_execute_data_addr_tiebreaker` (internal branch). |
| RO-6 | Closed | `Recorder::begin_handler` and `Recorder::end_handler` now capture clock/CPU/memory snapshots **inside** the `with_current_trace` closure. A slot-empty fire pays only for the `RefCell::borrow_mut` + `Option::as_mut().map` overhead — the `clock_gettime` / `getrusage` / `zend_memory_usage` syscalls are skipped entirely. The existing `recorder_begin_with_no_active_trace_is_a_noop` test's comment was updated to document the new invariant; a direct "no syscall fired" assertion would require a mock-clock layer, which is out of scope for this round. |

### Deferred to follow-up changes (RO-7 … RO-14)

The review's queued list at the bottom of the round-2 note
above is the canonical follow-up roster. None of them affect
NFR-REL-1 / NFR-SEC-1 / NFR-MAINT-1; none are blockers for
slice 3 (`recorder-depth-and-cap-drops`).

### Test-count delta

| Phase | Lib tests | Integration tests | Notes |
| --- | --- | --- | --- |
| Slice-2 round-1 (pre-fix) | 96 | 1 (spike) + 1 (recorder, gated) | Baseline after `7e8949a docs: close R-2 for PHP 8.3`. |
| Slice-2 round-2 (post-fix) | 101 (debug) / 101 (release) | 1 (spike) + 1 (recorder, gated) | +5 tests covering the new RO-1, RO-3, RO-4, RO-5 invariants. RO-2 and RO-6 are exercised via existing tests (skip semantics and slot-empty no-op). |

Gates green on the fix-round branch:
- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all`
- `cargo test --all --features recorder-dump`
- `cargo test --release --lib` (exercises the release-only `double_rinit_without_rshutdown_replaces_the_stale_trace_in_release_builds` test)
- `openspec validate recorder-observer-hooks-and-trace-lifecycle`

## Slice-3 deviations and notes

### C-10 — Slice-3 (`recorder-depth-and-cap-drops`) implementation notes

This entry records the in-flight deviations discovered while
implementing slice 3. The slice ships with no `SPECIFICATION.md` or
README changes; the spec amendments here are local to the OpenSpec
change's spec delta and `COMMENTS.md`.

**Status:** branch `feat/recorder-depth-and-cap-drops`, OpenSpec
change `recorder-depth-and-cap-drops`, `openspec validate
recorder-depth-and-cap-drops --strict` is green. All gates green:
`cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --all --all-features` (133 lib +
1 spike-integration), `cargo test --release --lib --all-features`
(134, with the cfg-gated release-only invariant), and
`PHP_ANALYZE_RUN_RECORDER=1 cargo test --test recorder_observer
--features recorder-dump` against `php8.4` on the build host.

#### Deviations from the slice-3 proposal/specs

1. **`deep_recursion.php` expects 99 recurse records, not 100.**
   The slice-3 proposal sketched "exactly 100 `C:` lines" for a
   `max_depth=100` deep-recursion run. The implemented harness
   asserts **99 recurse records** because Zend observes the
   top-level script body as a `closure:<file>:1` frame at
   `virtual_depth = 1`. The recurse chain then accepts
   `virtual_depth = 2..=100` = 99 calls; `recurse(N)` at
   `virtual_depth = 101` drops. The total `C:` line count is
   `99 + 1 (script body)` = 100 (matches the proposal's intent if
   one counts the script-body frame). The `DROP: dropped_records`
   line is `1902` (2001 recurse begins − 99 accepts), not 1900;
   the +2 over the proposal's idealisation comes from the same
   script-body confound. Spec/test wording amended to reflect
   reality.

2. **`cap_drops.php` does not assert "exactly K" — only that the
   gate fires.** The slice-3 spec scenario "A `cap_drops.php`
   trace emits a `DROP:` count consistent with the cap" was
   re-interpreted because the script-body closure's dict-entry
   contribution depends on the fixture's absolute path length,
   which varies across CI hosts. The implemented harness
   asserts `noop_accepts + dropped_records == 200`, `noop_accepts
   > 0`, and `dropped_records > 0` — which proves the cap-gate
   fires and the counter is accurate without depending on
   host-specific path lengths. On the build host (`p ≈ 54`),
   K = 12, drops = 188.

3. **`cap_drops.php` requires `flush_bytes` override too.** The
   slice-3 proposal mentioned only `php_analyze.buffer_cap_bytes`;
   the implementation discovered that `config.rs::clamp_directive`
   enforces `buffer_cap_bytes ≥ flush_bytes` (cross-field
   constraint from Phase 1). A naive `buffer_cap_bytes=1024`
   override is clamped up to `flush_bytes`'s default of `1 MiB`,
   so the gate never fires. The harness now sets
   `php_analyze.flush_bytes=1024` alongside
   `php_analyze.buffer_cap_bytes=1024` so the cap actually
   lands at 1024 bytes.

4. **`Dictionary::contains_key` accessor added.** Slice 3's
   `dict_miss_cost` helper needs a cheap hit/miss probe that does
   **not** intern. The slice-1 `Dictionary` API did not expose this;
   the slice-3 fix is a one-line `pub fn contains_key(&self, key:
   &FunctionKey) -> bool` accessor that delegates to the underlying
   `FxHashMap::contains_key`. No behaviour change to existing
   callers; pure additive surface.

5. **`Trace::new` widened to `Trace::new(identity, limits)`.** The
   slice-2 RS-8 finding asked for a `RequestIdentity` struct to
   collapse the four-positional `Trace::new`. Slice 3 needed to
   add two cached threshold fields (`max_depth`, `buffer_cap_bytes`).
   Per the proposal's task 3.2 ("extend `RequestIdentity` OR
   introduce a sibling `TraceLimits`"), the implementation chose
   the sibling `TraceLimits { max_depth: u32, buffer_cap_bytes:
   usize }` parameter — keeps `RequestIdentity` focused on
   "who is this request" and `TraceLimits` focused on "what are
   the gates' thresholds", which read better at the call site.
   `bootstrap::rinit_body` builds the `TraceLimits` from
   `Config::max_depth.into()` and `Config::buffer_cap_bytes`.

6. **A panic in `dump::write_trace_if_path_set` would skip the
   `accounting::sub` step.** Slice-3 task 7.2 asked to confirm the
   subtract runs even on a panic. In practice, the subtract is
   inside `rshutdown_release_trace` after the dump-write call site
   (`#[cfg(feature = "recorder-dump")]` block), so a dump-write
   panic would bypass the subtract for that one trace. The
   `bootstrap::rshutdown` `catch_unwind` then unwinds the rest;
   the next request starts with a slightly-stale `BYTES_IN_MEMORY`
   snapshot. Per `SPECIFICATION.md` §2.4 (Relaxed ordering, soft
   target), the bounded drift is acceptable. A future cleanup
   could move the subtract to before the dump-write, but the
   tradeoff is "if the dump-write succeeds, the budget is exact;
   if it panics, the budget over-counts by one trace's contribution
   until process restart" — both acceptable. Recorded here, not
   amended in code.

   **Update (DCR-1 round-1 fix):** the reviewer disagreed with
   keeping the asymmetry. The subtract now runs **before** the
   dump-write in `rshutdown_release_trace`; the budget stays exact
   even if a downstream hand-off panics, and the order matches
   what Phase 4's shipper will inherit ("release budget, then
   publish to sink"). See the round-1 fix-status section below.

#### Test-count delta

| Phase | Lib tests | Integration tests | Notes |
| --- | --- | --- | --- |
| Slice-2 round-2 (pre-slice-3) | 101 (debug) / 101 (release) | 1 spike + 1 recorder (gated) | Baseline after `1e4779a Merge pull request #5`. |
| Slice-3 round-1 | 133 (debug) / 134 (release) | 1 spike + 1 recorder (gated) | +32 tests in debug (one cfg-gated release-only). +31 in net surface. |

Per-module breakdown of slice-3 additions:
- `recorder::accounting::tests`: 3 (add/sub/snapshot/reset).
- `recorder::types::tests`: 5 (drop_counter, virtual_depth,
  TraceLimits caching, record_drop, AD-9 isolation).
- `recorder::observer::tests`: 17 (5 depth-gate, 5 cap-gate,
  5 LIFO pairing, 3 RSHUTDOWN subtract, 2 invariants, 1 release-
  only underflow defense, 1 debug LIFO consume baseline).
- `recorder::dump::tests`: 2 (DROP: line presence + value).

#### Gates evidence (final, on build host PHP 8.4.21)

```
cargo fmt --check                                                clean
cargo clippy --all-targets --all-features -- -D warnings         clean
cargo test --all --all-features                                  133 + 1 + 1, 0 failed
cargo test --release --lib --all-features                        134, 0 failed
PHP_ANALYZE_RUN_RECORDER=1 cargo test \
    --test recorder_observer --features recorder-dump            1 passed
openspec validate recorder-depth-and-cap-drops --strict          valid
```

#### Out-of-scope, queued for follow-up

- **`flush_records` / `flush_bytes` threshold-driven flushes** —
  Phase 4 (shipper). Slice 3 deliberately leaves the buffer to
  grow unbounded inside one request and to be discarded at
  RSHUTDOWN.
- **`PendingBatch::drop_counter: Arc<AtomicU64>` field** — Phase 4
  will clone `Trace::drop_counter` into the batch when the
  shipper channel exists. The `Trace` field is already in place.
- **`meta.dropped_records` wire field** — Phase 3 (wire encoder).
  The source of truth lives on `Trace::drop_counter` after this
  slice.
- **`E_NOTICE` log line distinguishing buffer-cap vs channel-full
  drops** — Phase 4 (R-13 mitigation). Slice 3's buffer-cap drop
  path is silent; the dump's `DROP:` line is the only signal in
  this slice.
- **AC-RC-5 hot-path zero-alloc assertion** — Phase 5. Slice 3
  inherits slice 2's `// NOTE for Phase 5` comment near the
  dict-miss `to_owned()` allocations.

---

### Architectural note — `FcallInfo<'a>` → `RawCallSite<'a>`

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

---

## Code Review — branch `feat/recorder-depth-and-cap-drops` (round 1)

**Reviewer:** Claude Code
**Date:** 2026-05-21
**Scope:** Two commits since `main` — `88e7411` (recorder code) and
`fbe5dfe` (integration fixtures). Diff is ~1700 lines across 12 files,
~70 % of which is tests + the existing C-10 documentation.
**Specification:** `SPECIFICATION.md` §3.2 (overflow policies), §2.4
(memory accounting), §4.1.2 (Trace shape), §8.3 NFR-REL-1.
**Gates verified locally:** `cargo fmt --check` clean, `cargo clippy
--all-targets --all-features -- -D warnings` clean, `cargo test --lib
--all-features` 133 passed.
**Overall recommendation:** REQUEST CHANGES (one MAJOR fix worth doing
on-branch — DCR-1; the rest are minor and can be deferred).

### Summary

Slice 3 lands the §3.2 overflow policies (`max_depth` depth gate,
`buffer_cap_bytes` byte gate), introduces the per-trace
`Arc<AtomicU64>` drop counter (AD-9) and process-wide
`BYTES_IN_MEMORY` (§2.4), and extends the diagnostic dump with a
`DROP:` summary line plus two integration fixtures
(`deep_recursion.php`, `cap_drops.php`). The implementation closely
tracks the proposal. C-10 documents six deviations from the proposal,
all of which I reviewed and agree with.

The substrate is clean: the new `accounting` submodule is small and
explicit about ordering choices, `Trace` grows three well-named fields
plus the cached `TraceLimits` sibling, and the LIFO `dropped_begins`
matcher is the right shape for begin/end pairing through drops. The
test coverage is excellent — 32 new lib tests covering depth gate,
cap gate, LIFO pairing, RSHUTDOWN subtract, and two cfg-gated tests
for the release-only saturating defenses.

The findings below are mostly about asymmetries between the
cap-gate's *projection* and the cap-gate's *actual billing*, a
test-isolation gap that only matters under parallel execution, and a
panic-ordering issue in `rshutdown_release_trace` that C-10 itself
flagged. None are correctness bugs in the slice-3 acceptance-criteria
sense; they are quality-of-implementation issues that future slices
will inherit.

### MAJOR findings

#### DCR-1: `rshutdown_release_trace` subtracts AFTER the dump-write, so a dump-write panic leaks the trace's contribution

**File:** `crates/php-analyze/src/recorder/observer.rs:113-133`
**Severity:** Major (test-only impact today; bad pattern to inherit
into Phase 4's shipper hand-off)
**Already noted:** C-10 deviation #6 records this and decides not to
amend. I disagree — the fix is two lines and removes a footgun.

The current ordering is:

```rust
pub fn rshutdown_release_trace() {
    CURRENT_TRACE.with(|slot| {
        let trace = slot.borrow_mut().take();
        #[cfg(feature = "recorder-dump")]
        if let Some(trace) = trace.as_ref() {
            crate::recorder::dump::write_trace_if_path_set(trace);
        }
        if let Some(trace) = trace.as_ref() {
            accounting::sub(trace.buffer_estimated_bytes);
        }
        drop(trace);
    });
}
```

If `write_trace_if_path_set` panics (currently impossible in practice
— the function `eprintln!`s on `io::Error` rather than propagating —
but future edits could regress this), the unwind skips the
`accounting::sub` and leaves the trace's bytes billed forever. The
`bootstrap::rshutdown` `catch_unwind` then swallows the panic and the
process keeps running with a permanently inflated `BYTES_IN_MEMORY`.
Per `SPECIFICATION.md` §2.4 the budget is a "soft target", so this
isn't a correctness violation — but it is a slow leak that compounds
across requests in an FPM worker's lifetime.

C-10 frames this as "if the dump-write succeeds, the budget is
exact; if it panics, the budget over-counts by one trace's
contribution until process restart" and argues both are acceptable.
The asymmetry is unnecessary: the dump-write only *reads*
`trace.buffer_estimated_bytes` (to nothing; it writes `drop_counter`
and `trace_id` instead), so the subtract can run first without
affecting what the dump writes:

```rust
pub fn rshutdown_release_trace() {
    CURRENT_TRACE.with(|slot| {
        let trace = slot.borrow_mut().take();
        // Subtract FIRST so the budget is exact even if the dump
        // writer (or any future hand-off) panics. The dump only
        // reads drop_counter/trace_id, neither of which depend on
        // the per-trace estimator.
        if let Some(trace) = trace.as_ref() {
            accounting::sub(trace.buffer_estimated_bytes);
        }
        #[cfg(feature = "recorder-dump")]
        if let Some(trace) = trace.as_ref() {
            crate::recorder::dump::write_trace_if_path_set(trace);
        }
        drop(trace);
    });
}
```

This also lines up better with the Phase-4 shipper handoff: the
shipper will move the subtract to "subtract when batch consumed",
and having the slice-3 subtract happen *before* the diagnostic
sink writes is the same shape Phase 4 will inherit.

**Action:** swap the two `if let Some(trace) = trace.as_ref()`
blocks. The two-test pair
(`rshutdown_returns_atomic_to_zero_after_balanced_trace`,
`two_consecutive_request_cycles_keep_zero_balance_invariant`) already
pins the post-fix contract.

### MINOR findings

#### DCR-2: Cap-gate `would_add` includes `CALL_RECORD_FIXED_BYTES` but the actual bill at begin doesn't — under nested calls the cap can be overshot

**File:** `crates/php-analyze/src/recorder/observer.rs:614-623,
730-773` and `crates/php-analyze/src/recorder/types.rs:415-419`
**Severity:** Minor (design choice per D-3; documented; spec calls
the cap a "soft target")

The cap-gate computes `would_add = CALL_RECORD_FIXED_BYTES +
dict_miss_cost(trace, categorised)` and rejects when
`accounting::snapshot() + would_add > buffer_cap_bytes`. On the
accept path, only the **dict-miss** bytes are billed at begin (via
`push_dict_entry_via_intern → accounting::add`). The
`CALL_RECORD_FIXED_BYTES` portion is billed at end (via `push_record
→ accounting::add`). This means consecutive nested begins all see
the same snapshot at begin time, all project the same conservative
`would_add`, and all accept — then each end fires sequentially and
bills 64 bytes, potentially overshooting the cap by `(in_flight - 1)
× 64` bytes per trace.

The `Trace::push_record` doc comment (types.rs:404-414) frames this
as the billing-split contract and claims "the gate at begin still
under-estimates by at most one `CALL_RECORD_FIXED_BYTES`". That's
true *per call considered in isolation* but not true under
unbalanced nesting where multiple in-flight ends each contribute
their 64 bytes. Worked example:

- `cap = 256`, `snapshot = 128` (from prior dict bills).
- Begin A: `would_add = 64 + 0 (hit)`. Check `128 + 64 = 192 ≤ 256`.
  Accept. Bill 0 (dict hit). Snapshot stays 128.
- Begin B (nested under A): `would_add = 64 + 0`. Check
  `128 + 64 = 192 ≤ 256`. Accept. Snapshot stays 128.
- Begin C (nested under B): `would_add = 64 + 0`. Check
  `128 + 64 = 192 ≤ 256`. Accept. Snapshot stays 128.
- End C: bill 64. Snapshot = 192.
- End B: bill 64. Snapshot = 256.
- End A: bill 64. Snapshot = 320 > cap. Overshoot.

Two paths to fix, both defensible:

- **Bill the record at begin** (the "most pessimistic" stance the
  proposal sketched as one option). One line in
  `begin_with_snapshots` after the accept decision:
  `accounting::add(CALL_RECORD_FIXED_BYTES)`. Then `push_record`
  drops its `accounting::add` (keeps the estimator bump). Trade-off:
  the per-trace estimator and the global atomic diverge between
  begin and end by exactly `CALL_RECORD_FIXED_BYTES` per active
  frame, which complicates the `rshutdown_release_trace` subtract
  (it would need to subtract `buffer_estimated_bytes` only, not
  including in-flight frame bytes — which is what happens today
  since `buffer_estimated_bytes` only grows in `push_record`).
- **Re-check the cap at end** (rejected — too late to drop; the
  observer hooks are already past the FFI boundary).
- **Leave it as-is** (current choice). Per the spec's "soft target"
  language, the overshoot under deep nesting is bounded and the
  cumulative effect across many traces is zero (each trace's
  contribution is subtracted at RSHUTDOWN). Document the limit
  explicitly.

If left as-is, please replace the
`push_record` doc paragraph with one that names the
nested-overshoot failure mode by name, rather than the current "at
most one `CALL_RECORD_FIXED_BYTES`" claim which is misleading. A
regression test that drives a 10-deep nesting under a tight cap
and asserts the post-end snapshot is bounded by `cap + max_depth ×
CALL_RECORD_FIXED_BYTES` would pin the worst-case envelope.

**Action:** at minimum, amend `Trace::push_record`'s doc comment to
name the nested-overshoot mode explicitly. Preferred: switch to
bill-at-begin so the cap-gate's `would_add` and the actual budget
mutation agree.

#### DCR-3: `accounting::sub` underflow risk is documented but not enforced

**File:** `crates/php-analyze/src/recorder/accounting.rs:51-60`

The doc comment says "The caller is responsible for ensuring `bytes`
does not exceed the current value; underflow would wrap and corrupt
the budget." The single production caller
(`rshutdown_release_trace`) feeds `trace.buffer_estimated_bytes`,
which is bounded by what the recorder previously added — so in the
happy path no underflow. But the release-path recovery branch in
`rinit_allocate_trace` (observer.rs:95-98) also subs
`stale.buffer_estimated_bytes`, and if a future slice ever
double-subs (e.g., a Phase-4 shipper that subs on batch consumed AND
on trace release for the same bytes) the wrap would silently
corrupt the budget.

A `fetch_update`-loop with a `saturating_sub` enforcement at the
function level would cost one CAS on the rare contention case but
turn an undetectable budget corruption into a no-op. The
performance cost is invisible at the `sub` call rate (one per
RSHUTDOWN, plus one per future Phase-4 batch consumed).

**Action:** consider changing `accounting::sub` to use
`fetch_update(|cur| Some(cur.saturating_sub(bytes)))` so underflow
becomes a saturated no-op instead of a corrupting wrap. Worth doing
before Phase 4 adds the second sub site. Not blocking.

#### DCR-4: Tests that touch `CURRENT_TRACE` but not the accounting atomic don't acquire the test lock — race window with parallel tests that do

**File:** `crates/php-analyze/src/recorder/observer.rs` — tests
`rshutdown_release_trace_drops_the_slot` (977),
`rshutdown_release_trace_on_empty_slot_is_a_noop` (988),
`with_current_trace_returns_none_when_slot_is_empty` (1036),
`recorder_begin_with_no_active_trace_is_a_noop` (1508)

These tests call `rshutdown_release_trace()` without acquiring
`account_guard()` first. Each call invokes
`accounting::sub(trace.buffer_estimated_bytes)` when the slot is
populated. In every case I traced, the populated trace's
`buffer_estimated_bytes` is `0` (fresh-from-`Trace::new`, no
begins fired), so the `sub(0)` is a no-op on the atomic.

The risk: a future test that ends up in the same thread's slot
with a non-zero `buffer_estimated_bytes` (e.g., someone adds a
test that does `rinit_allocate_trace` + several `begin`s but
forgets the `account_guard()`) would silently corrupt the
`BYTES_IN_MEMORY` value another test holding the lock is reading.

Two options:

- **Lock-hygiene-by-convention**: add a one-line comment to each
  such test saying "no `account_guard()` needed: this path never
  bills". This documents the invariant and makes future additions
  obvious.
- **Lock-hygiene-by-default**: make `rshutdown_release_trace`'s
  `accounting::sub` skip when `bytes == 0`. Cheap and removes the
  whole class of "did I need the guard?" questions.

Either way, the cost is low and the failure mode (sporadic test
flake under high `cargo test` parallelism) is annoying to debug.

**Action:** add the convention comments OR add the `if bytes > 0`
short-circuit. Not blocking.

#### DCR-5: `dump::tests` set/remove `PHP_ANALYZE_DUMP_PATH` without serialising — race risk under `cargo test` parallelism

**File:** `crates/php-analyze/src/recorder/dump.rs:357-503`

The four tests that exercise `write_trace_if_path_set` mutate the
`PHP_ANALYZE_DUMP_PATH` env var directly. The existing `// SAFETY:`
comment (358-367) acknowledges that `cargo test` parallelises across
threads and that the env-var mutation is racy in Rust 2024. Two
parallel tests can sequence `set(A), set(B), write(→B), remove(),
read(A)` and one of them sees an empty file.

The acquired `account_guard()` doesn't help because the lock is
keyed to the accounting atomic, not the env var. A separate
`DUMP_PATH_TEST_LOCK` mutex (or, simpler, the same mutex used as a
"recorder-dump tests" guard) would close the race. Alternatively,
the dump path could be threaded through the function signature
rather than env-var-based — but that's a bigger API change and the
env var is fine for the integration harness.

**Action:** add a per-module `DUMP_PATH_TEST_LOCK: Mutex<()>`
acquired alongside `account_guard()` in the four affected tests.
One additional line per test.

#### DCR-6: `cat_for` test helper leaks one `Box<RawCallSite>` per call

**File:** `crates/php-analyze/src/recorder/observer.rs:1535-1544`

```rust
fn cat_for(name: &'static str) -> Categorised<'static> {
    let site = Box::leak(Box::new(stub_site(...)));
    categorise(site)
}
```

The leak is bounded by the test count, but every slice-3 gate test
uses this helper (often more than once). At ~30 tests calling it ~3
times each, that's ~90 leaked `RawCallSite` boxes per `cargo test`
run. Not a correctness issue and not visible to operators, but a
better pattern would be to return `(Box<RawCallSite>,
Categorised<'static>)` and let the caller hold the box on the
stack. The lifetime juggling is a bit ugly because `Categorised`
borrows from the box, but a `with_cat(name, |cat| { ... })`
closure helper would sidestep the lifetime gymnastics.

**Action:** consider the closure form on a future test refactor.
Not blocking.

#### DCR-7: `assert_dropped_records`'s `binary` parameter is part of the message — minor: integration test asserts depth-overflow count `1902`, not `1900`

**File:** `crates/php-analyze/tests/recorder_observer.rs:466`

The deep_recursion harness asserts `dropped_records == 1902`. C-10
deviation #1 explains why (the script-body closure consumes one
depth slot, leaving 99 for `recurse`; 2001 begins − 99 accepts =
1902 drops). This is correct given the implementation. The
proposal's "1900" number was an idealisation that ignored the
script-body frame.

I'm flagging this only because the proposal's tasks.md may still
say "1900" and a future reviewer comparing tasks to the harness
will see the mismatch. The OpenSpec change's tasks.md should be
updated to match what was implemented, OR add a one-line "as
implemented, see C-10 #1 for the +2" annotation.

**Action:** update `openspec/changes/recorder-depth-and-cap-drops/tasks.md`
(if it still says 1900) to reflect the script-body confound, or
add the inline annotation.

### Positive highlights

- **`accounting` submodule is the right shape.** Small,
  ordering-explicit, callers-go-through-functions, with a
  `cfg(test)` reset + lock pair that other modules can re-use.
  The `Relaxed` ordering choice is correctly justified by §2.4 of
  the spec.
- **`TraceLimits` parameter on `Trace::new` is cleaner than
  extending `RequestIdentity`.** Keeps "who is this request" and
  "what are the thresholds" as separate concerns at the call site.
- **`Dictionary::contains_key` is exactly the cheapest accessor
  the cap-gate needs.** The naming convention (`miss_cost`
  inverting `hit_cost = 0`) reads well at the call site.
- **C-10 documents the six deviations from the proposal** with
  precise reproduction values (K=12, drops=188 on the build
  host). Future reviewers won't have to reverse-engineer the
  arithmetic.
- **`virtual_depth_never_underflows_under_an_adversarial_end_then_begin_sequence`**
  is the right kind of release-only test for the
  `saturating_sub` defense — pairs with the slice-2 RO-3 release-
  only test pattern.
- **LIFO pairing test
  (`lifo_pairing_accept_drop_accept_returns_two_records_in_pop_order`)
  is excellent.** It walks the three-call interleaving with
  explicit comments about why dict hits avoid the would_add
  miss-cost projection, and asserts the post-pop `call_id` order.

### Specification compliance

- ✅ AC-RC-1 (modulo `max_depth` / `buffer_cap_bytes` drops):
  retired by slice 3 per the proposal. The depth-gate and
  cap-gate paths are exercised by 10+ tests.
- ✅ AC-RC-3 (depth overflow → no crash, overflow accounted):
  retired by slice 3. `deep_recursion.php` end-to-end + the
  `begin_at_max_depth_plus_one_is_dropped_and_bumps_counter`
  unit test.
- ✅ AD-9 (per-trace `Arc<AtomicU64>` drop counter): implemented;
  three tests pin the per-trace independence invariant.
- ✅ §2.4 / §3.2 `BYTES_IN_MEMORY` atomic: implemented with
  `Relaxed` ordering and the documented add/sub call sites.
- ⚠️ §3.2 buffer-cap soft-target enforcement: implemented but with
  the nested-overshoot caveat in DCR-2. The spec's "soft target"
  language covers this, but the in-code comments overstate the
  tightness of the bound.
- ⚠️ Cap-gate-then-RSHUTDOWN budget exactness: works in the
  happy path, fragile under a dump-write panic (DCR-1).
- ⏭️ AC-RC-5 (zero-alloc hot path): correctly deferred to Phase 5
  per the proposal.
- ⏭️ `flush_records` / `flush_bytes` flushes, `meta.dropped_records`
  wire field, `E_NOTICE` log line: all correctly deferred to
  Phase 3/4.

### Overall recommendation

**REQUEST CHANGES** for DCR-1 (the two-line swap in
`rshutdown_release_trace`) — the fix is small, removes a footgun,
and lines up better with the Phase-4 shipper handoff.

DCR-2 is the most substantive open question. If you choose to keep
the current billing split, please amend the `Trace::push_record`
doc comment to name the nested-overshoot mode explicitly (the
current "at most one `CALL_RECORD_FIXED_BYTES`" framing is
misleading). If you choose to switch to bill-at-begin, that's a
~5-line change with a corresponding `push_record` simplification.

DCR-3 through DCR-7 are minor and can be batched into a slice-3
fix-round commit alongside DCR-1, or deferred to a follow-up
change. None of them block merging.

Test coverage and documentation are excellent. C-10 is the right
shape for an in-flight deviation log and saves the reviewer 30
minutes of arithmetic.

---

## C-11: Slice-3 round-1 review-fix status (branch `feat/recorder-depth-and-cap-drops`)

**Date:** 2026-05-21
**Reviewer findings:** DCR-1 … DCR-7 (see "Code Review — branch
`feat/recorder-depth-and-cap-drops` (round 1)" above).
**Implementer response:** the one MAJOR finding (DCR-1) and one
doc-only amendment for the design-question MINOR (DCR-2) landed on
the same branch as a fix-round, following the precedent set by
slice-2's C-9 round-2 fix-commits and slice-1's RS-1..RS-7
round-2 fix-commits. The remaining MINOR / NIT items (DCR-3 …
DCR-7) are queued as follow-up OpenSpec changes per the
reviewer's own §"Overall recommendation" guidance ("can be batched
into a slice-3 fix-round commit alongside DCR-1, or deferred to a
follow-up change. None of them block merging").

### What changed on this branch in the fix-round

| ID | Status | Implementation |
| --- | --- | --- |
| DCR-1 | Closed | `rshutdown_release_trace` now subtracts `trace.buffer_estimated_bytes` from `BYTES_IN_MEMORY` **before** invoking the diagnostic dump. The dump only reads `drop_counter` / `trace_id`, neither of which depends on the per-trace estimator, so the swap is behaviour-preserving on the happy path AND removes the "dump-write panic leaks one trace's contribution forever" footgun C-10 #6 had accepted. The C-10 #6 note is updated in-place to point at the round-1 resolution. No new tests were needed — the existing `rshutdown_returns_atomic_to_zero_after_balanced_trace` and `two_consecutive_request_cycles_keep_zero_balance_invariant` tests already pin the post-fix invariant (both pass post-swap). The new comment in `rshutdown_release_trace` cites DCR-1 and explains the Phase-4 shape alignment ("release budget, then publish to sink"). |
| DCR-2 | Closed (doc-only) | `Trace::push_record`'s billing-split doc paragraph is replaced. The new wording explicitly names the nested-overshoot failure mode (multiple in-flight `begin`s seeing the same pre-bump snapshot; ends each billing `CALL_RECORD_FIXED_BYTES` sequentially; post-end snapshot overshoots by `(in_flight - 1) * CALL_RECORD_FIXED_BYTES` per trace, bounded by `max_depth * CALL_RECORD_FIXED_BYTES`), cites §3.2's "soft target" framing as the spec-side justification, and references this DCR-2 note for the worked example. The implementation is unchanged — per the user's decision to amend doc only, the bill-at-begin alternative is rejected in favour of the lockstep-per-end design the slice already ships. No tests change. |

### Deferred to follow-up OpenSpec changes (DCR-3 … DCR-7)

Each gets its own change + branch per the "one change per branch"
rule. Listed by reviewer's overall-recommendation order so the
next session can pick them up without re-reading the full review:

- **DCR-3** (`accounting::sub` underflow risk documented but not
  enforced). Follow-up `accounting-saturating-sub`: switch
  `accounting::sub` to `fetch_update(|cur| Some(cur.saturating_sub(bytes)))`
  so a future double-sub becomes a saturated no-op instead of a
  corrupting wrap. Worth doing before Phase 4 adds the second sub
  site at "batch consumed".
- **DCR-4** (tests touching `CURRENT_TRACE` but not the accounting
  atomic don't acquire `account_guard()` → race window under
  parallel tests). Follow-up `recorder-test-lock-hygiene`:
  either add one-line "no `account_guard()` needed: bills zero"
  comments to the four named tests, OR short-circuit
  `accounting::sub` when `bytes == 0` to remove the whole class
  of "did I need the guard?" questions.
- **DCR-5** (`dump::tests` mutate `PHP_ANALYZE_DUMP_PATH` env var
  without serialising → race risk under `cargo test`
  parallelism). Follow-up `recorder-dump-test-lock`: add a
  per-module `DUMP_PATH_TEST_LOCK: Mutex<()>` acquired alongside
  `account_guard()` in the four affected tests.
- **DCR-6** (`cat_for` test helper `Box::leak`s one
  `RawCallSite` per call). Follow-up `recorder-test-helper-no-leak`:
  reshape `cat_for` into a `with_cat(name, |cat| { ... })`
  closure form so the box lives on the test's stack frame.
  Bounded by test count today; cosmetic.
- **DCR-7** (`proposal.md` for this change still says "1900"
  where the harness asserts "1902"). Already resolved in
  `tasks.md` §9.4 with the +2 explanation; `proposal.md` is the
  historical proposal narrative, so the divergence is acceptable
  per the OpenSpec norm that proposals freeze at archive time.
  No follow-up change needed.

### Gates evidence (post-fix-round, build host PHP 8.4.21)

```
cargo fmt --check                                                clean
cargo clippy --all-targets --all-features -- -D warnings         clean
cargo test --all --all-features                                  133 + 1 + 1, 0 failed
cargo test --release --lib --all-features                        134, 0 failed
openspec validate recorder-depth-and-cap-drops --strict          valid
```

(Integration test `PHP_ANALYZE_RUN_RECORDER=1 cargo test --test
recorder_observer --features recorder-dump` is unchanged and
remains green against `php8.4` on this host; the local PHP 8.3
gap is closed in CI per C-7.)

### Test-count delta

No new tests landed in this fix-round — DCR-1 is covered by the
existing `rshutdown_returns_atomic_to_zero_after_balanced_trace`
and `two_consecutive_request_cycles_keep_zero_balance_invariant`
tests (both pin the budget-returns-to-zero invariant the swap
preserves), and DCR-2 is a doc-only amendment. Test counts match
the pre-fix-round baseline (133 debug / 134 release).

---

## Code Review — branch `feat/wire-types-and-stub-ingest` (round 1)

**Reviewer:** Claude Code
**Date:** 2026-05-21
**Scope:** Two commits since `main` — `b3d0b17` (wire types + 19
round-trip tests) and `7b556d5` (stub-ingest binary + 6
integration tests). Diff is ~1700 lines across 8 files, ~60 % of
which is tests + the `stub-ingest/README.md`.
**Specification:** `SPECIFICATION.md` §4.2 (wire format), §5.2
(egress HTTP), §1.4 OQ-2 (media type), §6.3 (token redaction).
**OpenSpec specs:**
`openspec/changes/wire-types-and-stub-ingest/specs/wire-format/spec.md`
and `…/specs/stub-ingest-server/spec.md`.
**Gates verified locally:** `cargo fmt --check` clean,
`cargo clippy --all-targets --all-features -- -D warnings` clean,
`cargo test --all` 147 lib + 1 spike + 0 recorder (gated) + 6
stub round-trip = 154 passed.
**Overall recommendation:** REQUEST CHANGES (one MAJOR ask —
WSI-1 — covering six unimplemented spec scenarios in
`stub-ingest-server`; the rest are minor and can be deferred).

### Summary

Strong slice. Both deliverables (the §4.2 wire schema and the
`stub-ingest` test fixture) are in place, both commits are
self-contained, `design.md` captures every non-trivial decision
(single source-of-truth on the wire types, small-int
`FunctionKind`, `MetaFullStrict` regression boundary, byte-equal
bearer compare, port-0 stdout protocol), and gates are green.
The wire module is essentially scenario-complete: 17 of 17
`wire-format` spec scenarios map to a named test, with one
test (`dict_entry_kind_99_decode_fails_cleanly`) carrying a
too-loose assertion.

The substantive gap is on the **`stub-ingest-server` scenario
coverage**: of the 16 testable scenarios in that spec, **six are
not implemented as tests** (WSI-1 below). None of the six
exercise broken code paths — the implementation behaves
correctly under manual inspection — but the spec mandates them
and a round-2 reviewer would catch this. The recommended fix is
a single fix-round commit on this branch (precedent: C-9, C-11
fix-rounds for slices 2 and 3).

Everything else (WSI-2 … WSI-10) is minor: a loosely-asserted
test, two proposal-vs-implementation deviations (`--print-port`,
`ureq` major version) that the `tasks.md §17.3` table doesn't
flag, and a handful of style nits.

### MAJOR findings

#### WSI-1: six spec scenarios from `stub-ingest-server/spec.md` are not implemented as tests

**File:** `crates/stub-ingest/tests/round_trip.rs` (the file
where the missing tests belong)
**Severity:** Major (spec compliance — six normative scenarios
have no automated check)

The following scenarios from
`openspec/changes/wire-types-and-stub-ingest/specs/stub-ingest-server/spec.md`
are not exercised anywhere in `tests/round_trip.rs` or
elsewhere:

| # | Spec Requirement | Spec Scenario | Where it would live |
| --- | --- | --- | --- |
| 1 | "The stub SHALL accept `--bind <ip>:<port>` …" | *"An explicit non-zero `--bind` port is honoured (or fails fast)"* | New helper `spawn_stub_at(token, port)` + one test that picks a free port via a throwaway `TcpListener::bind("127.0.0.1:0")`, drops the listener, and passes that port via `--bind`. |
| 2 | "The stub SHALL accept `POST <path>` …" | *"Stdout stays silent after the `ready` line during request handling"* | `spawn_stub` keeps a background thread draining stdout past the `ready` line into a `Mutex<Vec<u8>>`; one test asserts the buffer is empty after 10 valid POSTs. |
| 3 | "The stub SHALL expose `GET /debug/batches` …" | *"An empty store returns an empty JSON array"* | New `fresh_store_returns_empty_array` test: spawn, GET, parse `Vec<wire::Batch>`, assert `.is_empty()`. No POST in between. |
| 4 | "The stub SHALL expose `GET /debug/batches` …" | *"Two posted batches appear in order"* | New `two_posted_batches_appear_in_order` test: POST `b0`, POST `b1`, GET, assert `[b0, b1]`. |
| 5 | "The stub SHALL handle requests sequentially under a single `Mutex`" | *"Sequential POSTs from the integration test do not race"* | New `ten_sequential_posts_appear_in_order` test: POST ten distinct batches (e.g. `meta.pid = i`), GET, assert length 10 and per-element equality. |
| 6 | "The stub SHALL document its usage in `crates/stub-ingest/README.md`" | *"README documents the three routes and the bind protocol"* | New `readme_documents_the_three_routes_and_bind_protocol` unit test (could live in `tests/round_trip.rs` or as `#[cfg(test)]` in `src/main.rs`): `include_str!("../README.md")` + five `assert!(readme.contains(_))` calls for `POST /v1/ingest`, `GET /debug/batches`, `POST /debug/reset`, `--bind`, and `bound:`. |

**Why this matters:** the production code paths these scenarios
exercise are present and (under a manual smoke test) correct.
But the OpenSpec workflow's contract is that every `Scenario:`
maps to a named test (the precedent set by slice-2's
`recorder-observer-hooks-and-trace-lifecycle` and slice-3's
`recorder-depth-and-cap-drops`). Shipping six missing tests at
merge time creates two ongoing costs: (a) the spec drifts from
the test suite, weakening the spec as the source of truth, and
(b) a future regression in any of these paths goes unnoticed
until manual inspection.

**Suggestion:** add the six tests as a single fix-round commit
on this branch, matching the precedent in `C-9` (slice-2 round-2
fix-commits) and `C-11` (slice-3 round-1 fix-commits). Rough
shape:

```text
crates/stub-ingest/tests/round_trip.rs
+ fn spawn_stub_at(token: &str, bind: &str) -> (ChildGuard, SocketAddr) {…}
+ fn drain_post_ready_stdout(child: &mut Child) -> Arc<Mutex<Vec<u8>>> {…}
+ #[test] fn fresh_store_returns_empty_array() {…}
+ #[test] fn two_posted_batches_appear_in_order() {…}
+ #[test] fn ten_sequential_posts_appear_in_order() {…}
+ #[test] fn stdout_stays_silent_after_ready_line() {…}
+ #[test] fn explicit_bind_port_is_honoured() {…}
+ #[test] fn readme_documents_the_three_routes_and_bind_protocol() {…}
```

Estimated +120–150 LOC of tests; no production-code change
required. Test count would rise from 6 stub round-trip tests to
12.

### MINOR findings

#### WSI-2: `dict_entry_kind_99_decode_fails_cleanly` assertion is too loose

**File:** `crates/php-analyze/src/wire.rs:566-570`
**Severity:** Minor (spec compliance — the test passes, but
weaker than the spec mandates)

The spec scenario *"A decoded byte 99 for `kind` produces a
clean decode error"* (`wire-format/spec.md` §`FunctionKind` last
scenario) says the error message MUST mention "an invalid
`FunctionKind` value". The current assertion is:

```rust
assert!(
    msg.contains("FunctionKind") || msg.contains("invalid"),
    "decode error must mention the invalid FunctionKind; got: {msg}",
);
```

The `||` lets the substring `"invalid"` alone pass — and many
`rmp_serde` decode errors contain "invalid" without being about
`FunctionKind`. If a future refactor swallows the `&'static str`
error from `TryFrom<u8>` and substitutes a generic
`rmp_serde::DecodeError` containing "invalid type" or "invalid
length", the test still passes but the §4.2.2 contract is
broken.

**Suggestion:** tighten to `&&`, or (cleaner) just assert
`msg.contains("FunctionKind")`. The `&'static str` returned by
`TryFrom<u8>` (`"invalid FunctionKind discriminant"`) already
contains both substrings, so the test against the current
implementation passes either way:

```rust
assert!(
    msg.contains("FunctionKind"),
    "decode error must mention the invalid FunctionKind \
     variant; got: {msg}",
);
```

#### WSI-3: `malformed_body_returns_400_and_does_not_store` doesn't verify the stderr "one-line summary"

**File:** `crates/stub-ingest/tests/round_trip.rs:248-262`
**Severity:** Minor (spec compliance — partial coverage)

The spec scenario *"A malformed body returns 400"* says: "the
response status is `400`, the in-memory store does not gain a
batch, **and stderr contains a one-line summary mentioning the
decode error**." The test asserts the first two; the stderr
check is not exercised. The implementation does write the
expected line (`crates/stub-ingest/src/main.rs:147` —
`eprintln!("stub-ingest: decode error: {err}");`), so this is
purely a missing assertion.

**Suggestion:** can be folded into WSI-1's
`drain_post_ready_stdout` helper expansion — drain BOTH stdout
(empty assertion) and stderr (contains "decode error") in a
single background-thread pattern, and use the captured stderr
here. ~30 additional LOC in `spawn_stub`, then `assert!` in this
test.

#### WSI-4: `tasks.md §17.3` is silent on two real proposal-vs-implementation deviations

**File:** `openspec/changes/wire-types-and-stub-ingest/tasks.md:142`
**Severity:** Minor (audit-trail hygiene)

Two implementation choices diverged from `proposal.md` but are
not flagged in `tasks.md §17.3`'s deviation note:

1. **`--print-port` flag was dropped.** `proposal.md:93-95`
   says: "`--print-port` (default `true`; on bind, write
   `bound: <ip>:<port>\n` to stdout …)". The implementation
   has no such flag — the `bound:` / `ready` protocol is
   unconditional (`crates/stub-ingest/src/main.rs:43-64` shows
   only `--bind`, `--auth-token`, `--path`). The behaviour the
   proposal wanted is the behaviour shipped, but the CLI
   surface differs.

2. **`ureq` dev-dep is `"3"`, not `"2"`.**
   `proposal.md:206-209` and §Impact both reference
   `ureq = "2"`. `crates/stub-ingest/Cargo.toml:50` ships
   `ureq = "3"`. `Cargo.lock` shows `ureq 3.3.0` was already
   present on `main` (transitively, via `ext-php-rs`'s build
   graph or similar), so the proposal's "zero new crates"
   claim is preserved — but the version drift is real and
   `tasks.md §14`'s `http_status_as_error(false)` note is
   v3-specific API.

Neither is a substantive bug. The audit-trail issue is that
`tasks.md §17.3` explicitly says "No `C-12` entry needed — the
in-flight deviations … are documentation-internal to the
implementation; they match the proposal's expectations" — but
these two deviations don't match the proposal as written.

**Suggestion:** amend `tasks.md §17.3` to a short list:

> Two proposal-vs-implementation deviations not requiring a
> `COMMENTS.md` entry:
> - **`--print-port` dropped.** The bind protocol is
>   unconditional; no test or design path needed an opt-out.
> - **`ureq` dev-dep pinned to `"3"`.** The version was already
>   in the lockfile transitively via other crates; bringing it
>   in as a direct dep at `"2"` would have added a second
>   major version. The v3-specific `http_status_as_error(false)`
>   API is the reason the helper exists.

#### WSI-5: `spawn_stub` has no read timeout — a wedged child hangs the test thread until cargo's outer timeout

**File:** `crates/stub-ingest/tests/round_trip.rs:40-70`
**Severity:** Minor (test robustness — future-bug amplifier)

`BufReader::lines()` is a blocking iterator. If the stub binary
prints `bound:` and then wedges before `ready` (e.g. a future
bug where `Server::http` returns but the second `writeln!` /
`flush!` panics in a way that doesn't close stdout — currently
impossible, but the protocol pre-`ready` runs no user code so
the surface is tight; the post-`ready` accept loop is where
real bugs would happen), the test blocks until cargo's outer
timeout fires (default minutes). The `ChildGuard` Drop still
cleans up the child, but the developer's `cargo test` round-trip
explodes from seconds to minutes.

**Suggestion:** wrap the stdout read in a background thread
that ships completed lines down a `mpsc::sync_channel(1)`, and
use `recv_timeout(Duration::from_secs(10))` on the main thread;
on timeout, fail the test with a captured stderr snapshot. ~25
LOC, makes the suite resilient to future stub-side bugs (and
incidentally subsumes the stdout-draining helper WSI-1 #2
needs).

#### WSI-6: `validate_bearer` open-codes `&value[prefix.len()..]` instead of `str::strip_prefix`

**File:** `crates/stub-ingest/src/main.rs:209-214`
**Severity:** Nit (style)

```rust
let prefix = "Bearer ";
if !value.starts_with(prefix) {
    return false;
}
let presented = &value[prefix.len()..];
presented.as_bytes() == configured.as_bytes()
```

The slice `&value[prefix.len()..]` is safe (ASCII prefix on a
UTF-8 string lands on a char boundary), but a future reader has
to verify that by hand. `str::strip_prefix` collapses the
`starts_with` + slice into one expression and makes the
boundary safety self-evident:

```rust
let Some(presented) = value.strip_prefix("Bearer ") else {
    return false;
};
presented.as_bytes() == configured.as_bytes()
```

No behaviour change. Saves three lines.

#### WSI-7: `dispatch()` eagerly computes `method_name` for paths that never use it

**File:** `crates/stub-ingest/src/main.rs:117`
**Severity:** Nit (style / micro-perf)

```rust
let method_name = request_method_name(&request);
match (&method, path.as_str()) { … }
```

`request_method_name` is `request.method().to_string()` — a heap
allocation per request. Only the `405` and `404` match arms use
the value; the three happy paths (ingest, debug-batches,
debug-reset) discard it. Under future bench workloads against
the stub (Phase 5), this is one needless allocation per request.

**Suggestion:** inline the call into the two arms that need it,
or build it lazily:

```rust
let method_name = || request_method_name(&request);
match (&method, path.as_str()) {
    (Method::Post, p) if p == args.path => handle_ingest(request, args, store),
    (Method::Get, "/debug/batches") => handle_debug_batches(request, store),
    (Method::Post, "/debug/reset") => handle_debug_reset(request, store),
    (_, p) if p == args.path => respond_status(request, 405, &format!("{} {p}", method_name())),
    _ => respond_status(request, 404, &format!("{} {path}", method_name())),
}
```

#### WSI-8: `handle_debug_batches` uses `to_vec_pretty` — pretty-print cost is paid on every test query

**File:** `crates/stub-ingest/src/main.rs:176`
**Severity:** Nit (style / micro-perf)

Pretty-printing inflates the JSON body 2–3× and is non-trivial
CPU for large batches (newlines + per-field indentation). Tests
re-parse via `serde_json::from_slice` anyway, where pretty vs.
compact is invisible. The README markets `curl` as the manual
debugging path — and `curl | jq` renders pretty JSON for
humans on demand, removing the need to pre-format.

**Suggestion:** either keep `to_vec_pretty` and accept the cost
(it's a test fixture; this is defensible), or switch to
`to_vec`. Flagging because the choice isn't recorded in
`design.md` — if the project later runs the stub under heavier
bench workloads (Phase 5), this is the kind of one-line change
that gets overlooked.

#### WSI-9: README routes table doesn't note that `/debug/*` are unauthenticated

**File:** `crates/stub-ingest/README.md:60-65`
**Severity:** Nit (docs)

`stub-ingest-server/spec.md §"POST /debug/reset"` Requirement
says: "The endpoint requires no authentication — it is a
debug-only surface accessible on the same loopback bind as
`/v1/ingest`." The README routes table doesn't mention this
distinction. An auditor reading the README cold might assume the
debug endpoints share the bearer requirement.

**Suggestion:** either append "(no auth)" to the
`/debug/batches` and `/debug/reset` rows, or add a footnote
under the table:

> `/debug/*` paths are unauthenticated — they are debug
> surfaces accessible only on the loopback bind. The bearer
> requirement applies to the ingest path only.

#### WSI-10: `dispatch()`'s `path.to_owned()` is a per-request `String` allocation

**File:** `crates/stub-ingest/src/main.rs:114`
**Severity:** Nit (style / micro-perf)

```rust
let path = request.url().split('?').next().unwrap_or("").to_owned();
```

The `.to_owned()` is required because `request.url()` borrows
`request`, and we later move `request` into a handler. But the
allocation can be skipped if `dispatch()` splits into two
phases: pull the `path` and `method` into owned values *only on
the 405/404 paths* (where the value is consumed by `format!`);
on the happy paths, dispatch on `request.url()` directly before
moving.

Not worth the refactor cost on its own — but if WSI-7's lazy
`method_name` lands, the same closure shape would work for
`path` and would clean up both at once.

### Positive highlights

- **Module-level docs in `wire.rs:1-41`** are exemplary. They
  call out the v1-frozen decisions, the §4.2 source-of-truth
  claim, the trace-id-as-String tradeoff (D-3), and the
  forward-compat rule all in one place. The
  `SECURITY/COMPAT: never add deny_unknown_fields` block-level
  comment at `wire.rs:59-64` is the right shape — a
  future-contributor warning placed exactly at the point of
  temptation.
- **`MetaFullStrict` regression boundary (`wire.rs:582-594`)**
  is the right pattern for pinning "default behaviour is a
  deliberate choice, not an accident" into the test suite.
  Catches the silent-tighten regression a stray
  `deny_unknown_fields` would otherwise introduce.
- **`contains_msgpack_str_key` scanner (`wire.rs:651-682`)** is
  principled: covers all four `fixstr` / `str8` / `str16` /
  `str32` headers even though only `fixstr` is reachable for
  the field names today. Cheap insurance against a future
  schema bump pushing a key past 31 bytes.
- **`function_kind_method_encodes_as_integer_one_via_rmp_serde`
  (`wire.rs:243-264`)** decodes the encoded bytes through
  `rmpv::Value` rather than through the same `FunctionKind`
  deserializer under test. That's the right way to exercise
  "the encoder produces the int we expect" — many similar
  test suites accidentally make this test tautological.
- **`ChildGuard` (`tests/round_trip.rs:24-35`)** correctly
  best-effort `kill()` + `wait()` on Drop, including on panic.
  The `let _ = …` pattern explicitly documents the
  swallow-errors choice on the cleanup path.
- **`tiny_http` over `axum` (design D-5)** is the right call:
  matches the production extension's sync model, ~10× less
  transitive dep weight than `axum + tokio`, and the
  three-route surface needs no router. The decision rationale
  in `design.md` lays out the alternative considered and the
  rejection reason — exemplary OpenSpec design-doc shape.
- **The single-source-of-truth dependency line** (stub depends
  on `php-analyze` via path so the wire schema is one Rust
  type, decoded by the same module that the Phase-4 shipper
  will encode through) is the right architectural choice and
  the design-D-6 alternative ("factor `wire.rs` into its own
  workspace crate") is correctly recorded as a deferred
  follow-up if dep-graph friction shows up.

### Specification compliance check

**`openspec/changes/wire-types-and-stub-ingest/specs/wire-format/spec.md`** (17 of 17 scenarios mapped):

- ✅ A hand-crafted Batch round-trips through MessagePack — `batch_round_trips_minimum_shape`
- ✅ An empty `dict` and empty `calls` round-trip — `batch_round_trips_with_empty_dict_and_empty_calls`
- ✅ MetaFull round-trips with all string fields populated — `meta_full_round_trips_with_all_fields_populated`
- ✅ Encoded bytes contain the §4.2.1 wire field names — `meta_full_encoded_bytes_contain_all_eight_wire_keys`
- ✅ A user-function DictEntry round-trips — `dict_entry_user_method_round_trips`
- ✅ An internal-function DictEntry round-trips — `dict_entry_internal_round_trips_with_empty_file_and_zero_line`
- ✅ A CallRecord round-trips with literal `"fn"` (not `"fn_id"`) — `call_record_round_trips_and_encoded_bytes_contain_literal_fn_key`
- ✅ A CallRecord with `abnormal_exit = true` round-trips — `call_record_abnormal_exit_true_round_trips`
- ✅ All four kinds round-trip — `realistic_shape_round_trip_succeeds`
- ✅ `FunctionKind::Method` encodes as int `1` — `function_kind_method_encodes_as_integer_one_via_rmp_serde`
- ⚠️ A decoded byte `99` produces a clean decode error — test runs, but assertion is too loose (WSI-2)
- ✅ MetaFull with unknown `future_field` decodes cleanly — `meta_full_decodes_cleanly_with_unknown_extra_field`
- ✅ `MetaFullStrict` rejects the same bytes — `meta_full_strict_rejects_the_same_unknown_field_bytes`
- ✅ `MEDIA_TYPE` matches §1.4 OQ-2 exactly — `media_type_matches_oq_2_string_exactly`
- ✅ `SCHEMA_VERSION` is `1` — `schema_version_is_one`
- ✅ No `From<_> for wire::Batch` impls exist in this slice — documented in module-doc; `grep` confirms; `tasks.md §9.3`
- ✅ A realistic-shape round-trip succeeds — `realistic_shape_round_trip_succeeds`

**`openspec/changes/wire-types-and-stub-ingest/specs/stub-ingest-server/spec.md`** (10 of 16 scenarios mapped):

- ✅ `cargo build -p stub-ingest` produces a binary — gates green
- ✅ `stub-ingest` depends on `php-analyze` via path — `Cargo.toml`
- ✅ `--bind 127.0.0.1:0` prints a non-zero bound port — `round_trip_post_and_get_debug_batches` (et al.)
- ❌ An explicit non-zero `--bind` port is honoured (or fails fast) — **not tested (WSI-1 #1)**
- ✅ A valid POST returns 200 and stores the batch — `round_trip_post_and_get_debug_batches`
- ✅ A missing `Authorization` header returns 401 — `missing_bearer_returns_401_and_does_not_store`
- ✅ A wrong bearer token returns 401 — `wrong_bearer_returns_401_and_does_not_store`
- ✅ A wrong `Content-Type` returns 415 — `wrong_content_type_returns_415_and_does_not_store`
- ⚠️ A malformed body returns 400 — status + store assertions present, **stderr summary assertion missing (WSI-3)**
- ❌ Stdout stays silent after `ready` line during request handling — **not tested (WSI-1 #2)**
- ❌ An empty store returns an empty JSON array — **not tested (WSI-1 #3)**
- ❌ Two posted batches appear in order — **not tested (WSI-1 #4)**
- ✅ Reset empties the store — `reset_clears_the_store_between_scenarios`
- ❌ Sequential POSTs from the integration test do not race (10 batches) — **not tested (WSI-1 #5)**
- ❌ README documents the three routes and the bind protocol — **not tested (WSI-1 #6)** (README content is correct on manual read; just no automated assertion)
- ✅ The round-trip test passes against a freshly-spawned stub — `cargo test -p stub-ingest` is green

### Overall recommendation

**REQUEST CHANGES** — round-1 fix-round on this same branch,
following the slice-2 (`C-9`) and slice-3 (`C-11`) precedent.

WSI-1 is the substantive ask: six normative spec scenarios in
`stub-ingest-server` are not tested. The implementation paths
they would exercise are present and correct under manual
inspection; the gap is purely on the automated-check surface,
but the OpenSpec contract is that every `Scenario:` block maps
to a named test. ~120–150 LOC of new tests in
`crates/stub-ingest/tests/round_trip.rs`, no production-code
change.

WSI-2 and WSI-3 are small assertion tightening on existing
tests and can be folded into the same fix-round commit (the
WSI-3 stderr capture would naturally share infrastructure with
WSI-1 #2's stdout-silent test).

WSI-4 … WSI-10 are minor. Recommend either batching the
documentation amendments (WSI-4, WSI-9) into the same fix-round
commit alongside WSI-1, and queueing WSI-5 / WSI-6 / WSI-7 /
WSI-8 / WSI-10 as follow-up OpenSpec changes (the "one change
per branch" rule applies — none of them block merging). The
single-largest follow-up candidate is WSI-5 (the
`spawn_stub`-with-timeout refactor), since it also gives WSI-1
#2 and WSI-3 the stdout/stderr-capture machinery they need.

Post-fix-round expected gate state: 147 wire tests + 6 + 6 new
stub round-trip tests = 12 stub round-trip tests; `cargo test
--all` total rises from 154 to ~160. No new dependencies. No
production-code behaviour change.

Test coverage and module-level documentation on this slice are
otherwise excellent, the design decisions are well-recorded,
and the wire-side is essentially flawless. The slice is one
fix-round away from merge-ready.

---

## C-12: Wire-and-stub round-1 review-fix status (branch `feat/wire-types-and-stub-ingest`)

**Date:** 2026-05-21
**Reviewer findings:** WSI-1 … WSI-10 (see "Code Review — branch
`feat/wire-types-and-stub-ingest` (round 1)" above).
**Implementer response:** the one MAJOR finding (WSI-1) and the
two small assertion / doc amendments the reviewer recommended
folding into the same fix-round commit (WSI-2, WSI-3, WSI-4,
WSI-9) landed on this branch as a fix-round, following the
precedent set by C-9 (slice-2 round-2 fix-commits) and C-11
(slice-3 round-1 fix-commits). The remaining MINOR / NIT items
(WSI-5 … WSI-8, WSI-10) are queued as follow-up OpenSpec changes
per the reviewer's own §"Overall recommendation" guidance ("none
of them block merging").

### What changed on this branch in the fix-round

| ID | Status | Implementation |
| --- | --- | --- |
| WSI-1 | Closed | Six previously-missing spec scenarios in `stub-ingest-server/spec.md` are now backed by named tests in `crates/stub-ingest/tests/round_trip.rs`: `fresh_store_returns_empty_array`, `two_posted_batches_appear_in_order`, `ten_sequential_posts_appear_in_order`, `stdout_stays_silent_after_ready_line`, `explicit_bind_port_is_honoured`, and `readme_documents_the_three_routes_and_bind_protocol`. The stub round-trip test count rose from 6 to 12. To support the stdout-silent and stderr-summary assertions, `ChildGuard` was reshaped to carry `Arc<Mutex<Vec<u8>>>` buffers for stdout (post-`ready`) and stderr, each drained by a background thread that exits naturally on the child's kill-on-drop. A new `spawn_stub_at(token, bind)` helper covers the explicit-port path. The README's prose-mention of `POST /debug/reset` was reflowed onto a single line (it had been line-broken across `\`POST\n/debug/reset\``); without that change `readme_documents_…` would have failed — the test caught a real documentation precision gap. |
| WSI-2 | Closed | The `dict_entry_kind_99_decode_fails_cleanly` test in `crates/php-analyze/src/wire.rs` now asserts `msg.contains("FunctionKind")` only — dropping the `\|\| msg.contains("invalid")` fallback that would let any generic `rmp_serde::DecodeError` containing the word "invalid" pass. The `&'static str` returned by `TryFrom<u8> for FunctionKind` (`"invalid FunctionKind discriminant"`) satisfies the tightened assertion on the current implementation, and the test now fails closed if a future refactor swallows that error. A doc-comment above the assertion records the round-1 history. |
| WSI-3 | Closed | `malformed_body_returns_400_and_does_not_store` now also asserts that the stub's stderr contains the substring `"decode error"` (the literal one-line summary `stub-ingest: decode error: <err>` the handler emits). The assertion piggy-backs on the stderr-capture machinery added for WSI-1 #2; no production-code change. |
| WSI-4 | Closed (doc-only) | `tasks.md §17.3` is amended in-place. The previous "No `C-12` entry needed" wording is replaced with a two-bullet list naming the two real proposal-vs-implementation deviations: (a) `--print-port` was dropped (the `bound:` / `ready` protocol is unconditional — no opt-out needed) and (b) `ureq` dev-dep is pinned to `"3"`, not `"2"` (the lockfile already carried `ureq 3.3.0` transitively; a direct `"2"` dep would have added a second major version). The documentation-internal items (added `rmpv` dev-dep, `Hash` derive on `FunctionKind`, omitted `use Read`) are listed separately as "no audit-trail entry needed". |
| WSI-9 | Closed (doc-only) | `crates/stub-ingest/README.md` routes table now flags the two `/debug/*` rows with **No auth**, and a paragraph below the table explains that the bearer requirement applies to the ingest path only — the `/debug/*` paths are debug surfaces accessible only on the loopback bind. Matches `stub-ingest-server/spec.md`'s "The endpoint requires no authentication" requirement on `POST /debug/reset`. |

### Deferred to follow-up OpenSpec changes (WSI-5 … WSI-8, WSI-10)

Each gets its own change + branch per the "one change per branch"
rule. Listed by the reviewer's overall-recommendation order so
the next session can pick them up without re-reading the full
review:

- **WSI-5** (`spawn_stub` has no read timeout — a wedged child
  could hang the test thread until cargo's outer timeout fires).
  Follow-up `stub-ingest-spawn-timeout`: wrap the `bound:` /
  `ready` consumption in a background thread that ships lines
  through a `mpsc::sync_channel(1)`, and use
  `recv_timeout(Duration::from_secs(10))` on the main thread. On
  timeout, fail the test with a captured stderr snapshot. The
  fix-round here already plumbed `Arc<Mutex<Vec<u8>>>` stderr
  capture, so the follow-up is purely the timeout wrapper.
- **WSI-6** (`validate_bearer` open-codes `&value[prefix.len()..]`
  instead of `str::strip_prefix`). Follow-up
  `stub-ingest-strip-prefix-bearer`: collapse the `starts_with`
  + slice into a `let-else strip_prefix` so the char-boundary
  safety is self-evident at the call site. Pure style.
- **WSI-7** (`dispatch()` eagerly computes `method_name` for
  paths that never use it). Follow-up
  `stub-ingest-lazy-method-name`: either inline the
  `request_method_name(&request)` call into the two arms that
  consume it, or wrap it as a closure. Micro-perf nit;
  combinable with WSI-10.
- **WSI-8** (`handle_debug_batches` uses
  `serde_json::to_vec_pretty`). Follow-up
  `stub-ingest-compact-json` (or kept as design choice; either
  way needs a `design.md` note). Pretty-print is defensible for
  a test fixture; the follow-up either switches to `to_vec` or
  records the design intent.
- **WSI-10** (`dispatch()`'s `path.to_owned()` is a per-request
  `String` allocation). Follow-up `stub-ingest-dispatch-borrow`:
  split `dispatch()` so the `path` and `method` strings are
  only owned on the 405/404 paths (where `format!` consumes
  them). Naturally pairs with WSI-7.

### Gates evidence (post-fix-round, build host PHP 8.4.21)

```
cargo fmt --all --check                                          clean
cargo clippy --all-targets --all-features -- -D warnings         clean
cargo test --all --all-features                                  152 + 1 + 1 + 0 + 12 = 166, 0 failed
cargo test --release --lib --all-features                        153, 0 failed
openspec validate wire-types-and-stub-ingest --strict            valid
```

The integration test `recorder_observer` continues to pass
against `php8.4` on this host; the local PHP 8.3 gap is closed
in CI per C-7.

### Test-count delta

- Stub round-trip tests: 6 → 12 (+6 new tests for WSI-1).
- `php-analyze` lib tests: unchanged (WSI-2 tightened an
  existing assertion; no new test).
- One existing test (`malformed_body_returns_400_and_does_not_store`)
  gained an extra stderr assertion under WSI-3.

Total workspace test count delta in this fix-round: **+6**,
matching the WSI-1 scenario count exactly. No new dependencies,
no production-code behaviour change.

---

## Slice-4-shipper-substrate deviations and notes

### C-13 — Slice-4 slice-1 (`shipper-thread-and-channel`) implementation notes

This entry records the in-flight deviations discovered while
implementing Phase 4 slice 1 — the channel + thread + drain
substrate. The slice ships no `SPECIFICATION.md` or README
changes; the deviations below are local to the OpenSpec change's
specs and to this file.

**Status:** branch `feat/shipper-thread-and-channel`, OpenSpec
change `shipper-thread-and-channel`, `openspec validate
shipper-thread-and-channel --strict` is green. All gates green:
`cargo fmt --all --check`, `cargo clippy --all-targets
--all-features -- -D warnings`, `cargo test --all --all-features`
(172 lib + 1 spike + 1 recorder-integration + 12 stub round-trip
= 186, 0 failed), `cargo test --release --lib --all-features` (173),
and `PHP_ANALYZE_RUN_RECORDER=1 cargo test --test recorder_observer
--features recorder-dump` against `php8.4` on the build host.

#### Deviations from the slice-4 proposal/specs

1. **Canonical Sender slot is `Mutex<Option<Sender>>`, not
   `OnceLock<Sender>`.** `tasks.md` §4.1 sketched
   `static SENDER: OnceLock<Sender<ShipperMessage>>`. `OnceLock`
   cannot be cleared, so `drain_and_join_at_mshutdown` would have
   no way to drop the canonical Sender at shutdown — the channel
   would stay open and the shipper loop's `recv_deadline` would
   block until the deadline expired, even on a clean shutdown of
   an empty channel (the empty-channel `< 100 ms` shutdown test
   would fail by ~5 seconds). The implemented type is
   `Mutex<Option<Sender<ShipperMessage>>>`: `install_channel_at_minit`
   enforces the "set once" semantic by checking `is_some()` before
   populating, and `drain_and_join_at_mshutdown` takes the Sender
   out and explicitly drops it. The spec wording ("the
   process-global `Sender` is populated") is preserved; only the
   container type differs. The module doc records the deviation
   in-source. **Action**: no follow-up needed; the deviation is
   architectural and reads coherent against the implemented spec.

2. **Bootstrap-side helpers factored as pure `Option<&Config>` -taking
   functions for testability.** The straightforward wiring would
   have been three inline calls inside `startup` / `rinit_body` /
   `mshutdown_body`, each consulting `Config::global()`
   directly. That would make the R-10 silent-disable contract
   essentially untestable without poking the `OnceLock`. The
   implementation factors out `install_shipper_if_enabled`,
   `spawn_shipper_if_enabled`, and `drain_shipper_if_enabled`
   (each `fn(Option<&Config>) -> ()`), so tests can drive both
   the enabled and disabled paths with hand-built `Config`
   values. Five new `bootstrap::tests` cover the matrix.

3. **`acquire_test_lock` / `reset_for_test` / accessor surface
   added on `crate::shipper`.** The shipper module exposes four
   `#[cfg(test)]` helpers (`acquire_test_lock`, `reset_for_test`,
   `sender_is_installed`, `handle_is_installed`,
   `receiver_is_installed`, `spawned_flag`, `clone_sender_for_test`,
   `install_panicking_handle_for_test`) for the bootstrap tests
   in §7.4 / §7.5 / §8.1 to peek at and reset the module-global
   slots between tests. Same pattern as `recorder::accounting`'s
   `acquire_test_lock` / `reset_for_test`. None of this surface
   compiles in production builds (the `#[cfg(test)]` gate is
   exhaustive).

4. **Thread name is truncated to 15 chars at the kernel.** The
   spawned thread is named `php-analyze-shipper` (19 chars), but
   `/proc/<pid>/task/<tid>/comm` reports `php-analyze-shi` because
   Linux truncates `task->comm` to `TASK_COMM_LEN - 1 = 15` bytes.
   The spec does not require a specific thread name (only that
   exactly one thread spawns), so the truncation is benign for
   correctness. Operators using `ps -L` or `top -H -p <pid>` will
   see the truncated name. Not amended in code; recorded here for
   future reviewer reference.

5. **`run_loop` tolerates a second `Drain` message.** The spec
   does not contemplate two `Drain` messages in one run (only
   `drain_and_join_at_mshutdown` sends `Drain`, and only once),
   but the implementation matches `Ok(ShipperMessage::Drain { .. })`
   in the drain-phase and ignores it. This is defensive: slice 2's
   future encoder might inadvertently re-Drain on an error path,
   and we'd rather see the ignore than have the loop crash. The
   pure-Rust unit test for this defensive arm is implicit (a
   second Drain falls through to the next `recv_deadline`).
   Recorded so a future reviewer doesn't flag the "dead arm" as
   unreachable.

6. **`#[allow(dead_code)]` on `JoinOutcome::Clean`'s payload.**
   Clippy under `-D warnings` flags `Clean(ShipperExit)` because
   slice 1's `drain_shipper_if_enabled` binds the outcome to
   `_outcome` and discards. The allow is annotated with a reason
   pointing forward to slice 2 (the encoder will read
   `ShipperExit::batches_drained` and
   `batches_abandoned_at_deadline` to surface counts at
   `E_NOTICE`). The fields are read by tests via the derived
   `Debug`, so the allow only covers the production
   discard.

#### Manual host verification (build host PHP 8.4.21)

The §10 manual checks reproduce the spec scenarios at the OS
level. All four runs commit to the same `target/release/libphp_analyze.so`
built at HEAD.

```
$ cargo build --release -p php-analyze       → libphp_analyze.so (~8 MB)
$ php8.4 -d extension=$PWD/target/release/libphp_analyze.so \
       -d php_analyze.server_url=http://localhost:8888 \
       -d php_analyze.auth_token=dummy --ri php_analyze
  → status: enabled (true); auth_token redacted as ***;
    all 14 directive rows render; one startup E_WARNING for
    http:// URL (TLS-recommendation).

$ php8.4 ... -r 'usleep(300000); /proc/<pid>/task scan;'
  with enabled extension
  → 2 threads: ['php-analyze-shi', 'php8.4']
  with php_analyze.enabled=0
  → 1 thread: ['php8.4']

$ for i in 1..3 do
    time(php8.4 ... -r 'echo ok;')
  end
  → 37 ms, 51 ms, 61 ms — well below the 5200 ms
    (shutdown_grace + 200 ms) envelope; the shipper drain is
    essentially free in slice 1 (no I/O).
```

The thread-count check satisfies AC-PB-1 (each enabled PHP
process owns exactly one shipper thread after its first request)
**and** the modified `extension-bootstrap` "Disabled extension
spawns no background threads" scenario in one observation pair.
The total-wall-time check satisfies AC-PB-2 / AC-BS-4 (MSHUTDOWN
returns within `shutdown_grace + 200 ms`).

#### Test-count delta

| Phase | Lib tests | Integration tests | Notes |
| --- | --- | --- | --- |
| Slice-3 round-1 / Wire+stub round-2 (pre-slice-4) | 152 | 1 spike + 1 recorder (gated) + 12 stub round-trip | Baseline after `b3635bf Merge PR #7`. |
| Slice-4 slice-1 (post-this-commit) | 172 (debug) / 173 (release) | 1 spike + 1 recorder (gated) + 12 stub round-trip | +20 lib tests: 15 in `shipper::tests`, 5 in `bootstrap::tests`. |

Per-module breakdown of the 20 additions:
- `shipper::tests::run_loop_*`: 5 tests (the four termination
  conditions of the loop state machine).
- `shipper::tests::install_channel_at_minit_*`: 2 tests.
- `shipper::tests::spawn_if_needed_at_rinit_*`: 4 tests (incl. a
  three-thread CAS race with a `Barrier`).
- `shipper::tests::drain_and_join_at_mshutdown_*`: 4 tests (incl.
  the panicking-shipper-thread test).
- `bootstrap::tests::bootstrap_*`: 5 tests (install / drain
  per-config-state matrix plus the full-lifecycle scenarios).

#### Gates evidence (final, on build host PHP 8.4.21)

```
cargo fmt --all --check                                          clean
cargo clippy --all-targets --all-features -- -D warnings         clean
cargo test --all --all-features                                  172 + 1 + 1 + 12 = 186, 0 failed
cargo test --release --lib --all-features                        173, 0 failed
PHP_ANALYZE_RUN_RECORDER=1 cargo test \
    --test recorder_observer --features recorder-dump            1 passed
openspec validate shipper-thread-and-channel --strict            valid
```

#### Out-of-scope, queued for follow-up

- **MessagePack encoding on the shipper thread** — slice 2. The
  current `run_loop` drops batches silently; slice 2 grows an
  `on_batch` step that encodes via `rmp_serde::to_vec_named`.
- **ureq POST (single attempt)** — slice 2. The encoded bytes
  will be POSTed via a `ureq::Agent` configured once at MINIT.
- **Retry/backoff** — slice 3. Per-attempt backoff
  (`retry_backoff_ms × 2^attempt`), with `retry_count + 1` total
  attempts before drop.
- **`PendingBatch::drop_counter: Arc<AtomicU64>` field** — slice
  2 (where the encoder reads it to stamp `meta.dropped_records`).
  The `Trace::drop_counter` field is already in place.
- **Recorder threshold-driven flushes + `RSHUTDOWN`-final-flush**
  — slice 2. Slice-3's recorder still discards the buffer at
  RSHUTDOWN; slice 2 wires it into the channel.
- **Channel-full vs. buffer-cap drop distinction (R-13)** —
  slice 3. Both increment the same `drop_counter`; slice 3 grows
  the `E_NOTICE` log line that distinguishes them.
- **`E_NOTICE` log line on retry-exhaust / drain-abandon** —
  slice 3. Surfaces `ShipperExit::batches_abandoned_at_deadline`
  to operators.
- **R-10 follow-up `mshutdown-respects-silent-disable`** — closed
  in this slice as a side benefit; the queued OpenSpec change can
  be removed from the follow-up list.

