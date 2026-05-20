# COMMENTS

This file accumulates clarifications, review notes, and out-of-scope
discoveries that supplement `SPECIFICATION.md`. If a statement here
conflicts with `SPECIFICATION.md`, this file is the more recent
clarification.

## Open blockers

### B-2 — `git push` blocked on remote auth

**Status**: blocks §10.3 only.

**Cause**: this build host has no SSH key registered with
`git@github.com:kalaspuffar/php-analyze.git`. `git push -u origin
feat/scaffold-workspace-and-config` fails with `Permission denied
(publickey)`.

**To unblock**: push the branch from a workstation that has push
credentials:

```bash
git push -u origin feat/scaffold-workspace-and-config
```

The branch is fully committed locally and ready to push.

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

