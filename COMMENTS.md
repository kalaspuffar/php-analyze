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
