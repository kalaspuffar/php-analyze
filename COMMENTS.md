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
