# COMMENTS

This file accumulates clarifications, review notes, and out-of-scope
discoveries that supplement `SPECIFICATION.md`. If a statement here
conflicts with `SPECIFICATION.md`, this file is the more recent
clarification.

## Open blockers

### B-1 — `ext-php-rs` integration deferred

**Status**: blocks the second half of `scaffold-workspace-and-config`
(§2.1, §2.6, §5, §6, §9.5, §9.6).

**Cause**: the build host used to land this OpenSpec change has PHP 8.4
(cli) installed but **not** the `php8.4-dev` (or `php8.3-dev`) headers,
and no passwordless sudo. `ext-php-rs`'s `build.rs` requires
`php-config` on `PATH`; without it the crate cannot compile, which
takes the entire workspace down with it.

**Consequence**: the first half of the change is fully implemented and
green (`fmt`, `clippy -D warnings`, all 14 `Config` unit tests pass,
`cargo build --release --workspace` succeeds). The `cdylib` artifact
`target/release/libphp_analyze.so` exists but is **not** a loadable PHP
extension yet — it contains no `MINIT`/`MSHUTDOWN`/`RINIT`/`RSHUTDOWN`/
`MINFO` entry points. The bootstrap module (`src/bootstrap.rs`) will
add those once headers are available.

**To unblock**:

1. Install `php8.3-dev` **or** `php8.4-dev` (Ubuntu/Debian) on the build
   host, plus the matching `libclang-dev` for `bindgen`.
2. Verify `php-config --version` runs.
3. Resume `/opsx:apply scaffold-workspace-and-config`. The remaining
   tasks are §2.1 (add `ext-php-rs`), §5 (INI registration + lifecycle
   skeletons in `bootstrap.rs`), §6 (`#[php_module]` entry +
   `PhpInfoRenderer`), §9.5/§9.6 (manual `php --ri php_analyze`
   verification).

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
