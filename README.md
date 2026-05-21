# php-analyze

A PHP function-call tracing extension. Built in Rust, loaded into PHP 8.3 /
8.4 as a `cdylib`, ships per-call metrics to an HTTP ingest endpoint over
MessagePack.

## Status

**Pre-v1.** This repository is in active early-phase development. The
current release contains the configuration surface and the PHP lifecycle
hook skeletons only. **Observer hooks (the actual call tracing) and the
HTTP transport are not yet implemented.** Follow the OpenSpec change
directory (`openspec/changes/`) for the rolling implementation plan.

See `SPECIFICATION.md` for the authoritative design and
`REQUIREMENTS.md` for the elicited requirements.

## Build

Requirements:

- Rust **stable**, ≥ 1.78 (pinned via `rust-toolchain.toml`).
- Linux x86_64.
- PHP development headers: `php8.3-dev` **or** `php8.4-dev` installed and
  `php-config` available on `PATH`. `ext-php-rs` invokes `php-config` at
  build time to discover Zend internals.

Build the extension:

```bash
cargo build --release -p php-analyze
```

The artifact is `target/release/libphp_analyze.so`. Rename or symlink to
`php_analyze.so` for installation.

Build everything in the workspace (extension + stub ingest placeholder):

```bash
cargo build --release --workspace
```

## Install

1. Build the extension as above.
2. Locate PHP's extension directory: `php -i | grep extension_dir`.
3. Copy `target/release/libphp_analyze.so` into that directory as
   `php_analyze.so` (rename or symlink).
4. Add to `php.ini`:

```ini
; --- php-analyze configuration (php.ini) ---
extension=php_analyze.so

php_analyze.enabled              = 1
php_analyze.server_url           = "https://ingest.example.com/v1/ingest"
php_analyze.auth_token_file      = "/etc/php-analyze/token"
;php_analyze.auth_token          = "..."             ; alternative to _file

php_analyze.flush_records        = 10000
php_analyze.flush_bytes          = 1048576           ; 1 MiB
php_analyze.buffer_cap_bytes     = 67108864          ; 64 MiB
php_analyze.max_depth            = 1024

php_analyze.retry_count          = 3
php_analyze.retry_backoff_ms     = 100
php_analyze.http_timeout_ms      = 2000
php_analyze.shutdown_grace_ms    = 5000
php_analyze.shipper_queue_depth  = 8
```

5. Reload PHP-FPM (or rerun the CLI script).

Verify the extension loaded and read its config:

```bash
php --ri php_analyze
```

The `auth_token` row is always rendered as `***`.

## Configuration

The extension reads its configuration from `php.ini` only. **No
`ini_set()`-mutable directives exist**; every directive below is at
`PHP_INI_SYSTEM` scope and any userland override returns `false`.

| Directive | Type | Default | Range | Effect |
| --- | --- | --- | --- | --- |
| `php_analyze.enabled` | bool | `1` | `0` or `1` | Master on/off switch. `0` disables all observer hooks. |
| `php_analyze.server_url` | string | *(none)* | full URL | Ingest endpoint. Missing → silent disable + warning. |
| `php_analyze.auth_token` | string | *(none)* | non-empty | Bearer token. |
| `php_analyze.auth_token_file` | string | *(none)* | absolute path | If set and readable, overrides `auth_token`. Failure to read → silent disable + warning. |
| `php_analyze.flush_records` | int | `10000` | `[1, 10⁹]` | Flush after N buffered records. |
| `php_analyze.flush_bytes` | int | `1048576` | `[1024, 10⁹]` | Flush after estimated N bytes. |
| `php_analyze.buffer_cap_bytes` | int | `67108864` | `[flush_bytes, 10¹⁰]` | Hard memory cap; drop-newest above. |
| `php_analyze.max_depth` | int | `1024` | `[1, 65535]` | Max tracked stack depth. |
| `php_analyze.retry_count` | int | `3` | `[0, 10]` | HTTP retry attempts. |
| `php_analyze.retry_backoff_ms` | int | `100` | `[1, 60000]` | Base backoff (doubles per attempt). |
| `php_analyze.http_timeout_ms` | int | `2000` | `[100, 60000]` | Per-attempt HTTP timeout. |
| `php_analyze.shutdown_grace_ms` | int | `5000` | `[0, 60000]` | Bounds shipper drain at `MSHUTDOWN`. |
| `php_analyze.shipper_queue_depth` | int | `8` | `[1, 1024]` | Batch channel capacity; full → drop-newest. |

Behaviour:

- **Silent-disable posture.** If `server_url` is missing/unparseable or no
  bearer token can be resolved, the extension marks itself disabled and
  emits exactly one `E_WARNING` per process at `MINIT`. PHP startup
  proceeds normally.
- **Range clamping.** Numeric directives outside their `[min, max]` range
  are clamped to the nearest bound, with one `E_WARNING` per offending
  directive.
- **Token precedence.** When both `auth_token_file` and `auth_token` are
  set, the file's contents (UTF-8, trailing whitespace trimmed) win. If
  the file is unreadable or empty, the extension silent-disables; it does
  **not** fall back to the inline token.
- **HTTP warning.** `http://` URLs are accepted but emit one `E_WARNING`
  noting the lack of TLS. Production deployments should use `https://`.

## Spike mode (developer-only)

> **Do not enable spike mode in production.** It writes one log line
> per PHP function call to the configured destination. It is slow,
> unbounded, and exists only to validate the `zend_observer`
> integration before the real Recorder lands. Phase 2's Recorder
> change removes the two directives below.

Two additional `PHP_INI_SYSTEM` directives gate the Phase-0 spike:

| Directive | Type | Default | Effect |
| --- | --- | --- | --- |
| `php_analyze.spike_observer` | bool | `0` | When `1` *and* the extension is enabled, install an `FcallObserver` that writes one line per function entry / exit. |
| `php_analyze.spike_log_path` | string | *(empty)* | Where the spike writes. Empty = stderr; an absolute path = open-create-append to that file (`LineWriter`-buffered so one flush per line). |

When spike mode is on, `php --ri php_analyze` shows a red-flag
banner row:

```
spike-mode => ENABLED (DEVELOPMENT-ONLY; do not enable in production)
```

Log line shape:

```
entry: <fqn>
exit: <fqn> (abnormal=<true|false>)
```

where `<fqn>` is one of `internal:<name>`, `method:<Class>::<name>`,
`closure:<file>:<line>`, or `function:<file>:<line>:<name>`.

Worked example invocation (assumes the cdylib is built and at
`target/debug/libphp_analyze.so`):

```bash
cat >/tmp/spike.ini <<'EOF'
extension=/abs/path/to/target/debug/libphp_analyze.so
php_analyze.enabled              = 1
php_analyze.server_url           = "https://spike.invalid/ingest"
php_analyze.auth_token           = "not-a-real-token"
php_analyze.spike_observer       = 1
php_analyze.spike_log_path       = "/tmp/spike.log"
EOF

php -n -c /tmp/spike.ini -r '
  function only_me(): void {}
  only_me();
  array_map(fn ($x) => $x + 1, [1, 2]);
'
head /tmp/spike.log
```

Known limitations of the spike's coverage (Phase-0 evidence in
`COMMENTS.md` C-5):

- PHP 8.x opcode-specialises a small set of internals (notably
  `strlen` when called with a constant argument). The observer
  surface does NOT see those calls. Other internals (`array_map`,
  `json_encode`, `preg_match`, …) are observed normally.

## Development

The mandatory pre-commit checklist; CI enforces all three:

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

Run a single test by name:

```bash
cargo test -p php-analyze <test_name>
```

The Rust developer workflow is documented in `personas/RUST_DEVELOPER.md`;
implementation work is driven through OpenSpec changes
(`openspec/changes/`).

## License

[MIT](LICENSE). © 2026 Daniel Persson.
