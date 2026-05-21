#!/usr/bin/env bash
#
# Slice-2 recorder integration harness — shell-level driver invoked by
# `crates/php-analyze/tests/recorder_observer.rs`. Builds the cdylib
# (with `--features recorder-dump`) if needed, writes a minimal
# `php.ini` that points the configured PHP binary at the freshly-built
# extension, sets `PHP_ANALYZE_DUMP_PATH` to a caller-supplied path,
# runs one fixture, and prints the dump path to stdout. The Rust test
# reads that path and parses the dump via `recorder::dump::parse_dump`.
#
# Usage: run.sh <php_binary> <fixture_path> <dump_path>
#
# Skips with exit 77 (autotools convention) if `<php_binary>` is not
# executable. Returns non-zero on any real failure with diagnostics on
# stderr.

set -euo pipefail

if [[ $# -ne 3 ]]; then
    echo "recorder-driver: usage: run.sh <php_binary> <fixture_path> <dump_path>" >&2
    exit 2
fi

PHP_BIN="$1"
FIXTURE="$2"
DUMP_PATH="$3"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# --- Preflight ---------------------------------------------------------

if ! command -v "$PHP_BIN" >/dev/null 2>&1; then
    echo "recorder-driver: skipping — \`$PHP_BIN\` not on PATH" >&2
    exit 77
fi

if [[ ! -f "$FIXTURE" ]]; then
    echo "recorder-driver: fixture not found: $FIXTURE" >&2
    exit 1
fi

# Build the cdylib with the `recorder-dump` feature on. Cargo no-ops
# when up to date, so the rebuild cost across fixtures is one-shot.
cargo build -p php-analyze --features recorder-dump --manifest-path "$REPO_ROOT/Cargo.toml" >&2

# Locate the built cdylib. S-11 follow-up suggested replacing the
# python3 dependency with a `$CARGO_TARGET_DIR` fallback; this
# harness picks that up directly.
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
CDYLIB="$TARGET_DIR/debug/libphp_analyze.so"
if [[ ! -f "$CDYLIB" ]]; then
    echo "recorder-driver: cdylib not found at $CDYLIB after build" >&2
    exit 1
fi

# --- Per-run scratch dir for the php.ini -------------------------------

TMPDIR_RUN="$(mktemp -d -t php-analyze-recorder-XXXXXX)"
trap 'rm -rf "$TMPDIR_RUN"' EXIT
INI_FILE="$TMPDIR_RUN/recorder.ini"

# `server_url` and `auth_token` must be present so Phase-1's silent-
# disable does not kick in. Their values are never used (no shipper
# exists yet). `spike_observer = 0` is the default, but we set it
# explicitly so `BootObserver::Recorder` is the resolved variant.
cat >"$INI_FILE" <<EOF
extension=$CDYLIB
php_analyze.enabled              = 1
php_analyze.server_url           = "https://recorder.invalid/ingest"
php_analyze.auth_token           = "recorder-driver-token-not-real"
php_analyze.spike_observer       = 0
EOF

# Ensure the dump path is empty before the run so a re-run picks up
# only the new content (the dump module appends; tests parse the file
# fresh).
: >"$DUMP_PATH"

# --- Run the fixture ---------------------------------------------------

if ! PHP_ANALYZE_DUMP_PATH="$DUMP_PATH" \
        "$PHP_BIN" -n -c "$INI_FILE" "$FIXTURE" >>"$DUMP_PATH.stdout" 2>>"$DUMP_PATH.stderr"; then
    echo "recorder-driver: php exited non-zero for $FIXTURE" >&2
    echo "--- stderr ---" >&2
    cat "$DUMP_PATH.stderr" >&2
    exit 1
fi

# Module-API mismatch: PHP CLI prints the "module API" warning to
# stdout (NOT stderr — startup warnings go to stdout unless
# `display_startup_errors=stderr` is set), exits 0, and never loads
# the extension. Treat this as a skip with the autotools 77
# convention so the Rust harness can move on to a matching PHP
# binary instead of asserting on a never-populated dump.
if grep -q "module API" "$DUMP_PATH.stdout" 2>/dev/null \
        || grep -q "module API" "$DUMP_PATH.stderr" 2>/dev/null; then
    echo "recorder-driver: skipping $PHP_BIN — module API mismatch with the cdylib's php-config target" >&2
    exit 77
fi

# --- Hand the dump path back to the caller -----------------------------

echo "$DUMP_PATH"
