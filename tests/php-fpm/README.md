# `tests/php-fpm/` — PHP-FPM integration fixtures

PHP fixtures driven by the
[`crates/php-analyze/tests/fpm_repeated_requests.rs`](../../crates/php-analyze/tests/fpm_repeated_requests.rs)
integration test. The test self-skips loudly unless **all** of:

- `PHP_ANALYZE_RUN_FPM=1` is set in the environment.
- At least one of `php-fpm8.3` / `php-fpm8.4` is resolvable on `PATH`.
- The freshly-built `libphp_analyze.so`'s module API matches that
  `php-fpm` binary's expectation (otherwise the binary is recorded
  as `SkippedModuleApi`, identically to the CLI integration tests).

The fixtures are invoked over a hand-rolled FastCGI responder
client living inside the integration test file; no external
FastCGI tool (`cgi-fcgi`, `nginx`) is needed. The test owns its
own `php-fpm` master + its own `stub-ingest` HTTP server, both
spawned per-run on ephemeral loopback ports and killed on `Drop`.

Each fixture's header comment names the acceptance criteria it
binds; the wrapping `fpm_repeated_requests` integration test is
the authoritative source for the per-test assertions.

Capability binding:
[`openspec/changes/archive/2026-05-22-fpm-integration-test/`](../../openspec/changes/)
(once archived; currently under
`openspec/changes/fpm-integration-test/` if still active).
