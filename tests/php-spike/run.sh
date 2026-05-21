#!/usr/bin/env bash
#
# Phase-0 zend_observer spike — shell-level driver invoked by
# `tests/spike_observer.rs`. Builds the cdylib if needed, writes a
# minimal php.ini that turns the spike on, runs the three fixtures
# (user_calls, internal_calls, throws) under the freshly-built
# extension, and prints the path of the captured log to stdout. The
# Rust test reads that path and asserts on the log contents.
#
# Skips with exit 77 (the autotools "skip" convention) if `php` is not
# on PATH. Returns non-zero on any real failure with diagnostics on
# stderr.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# --- Preflight ---------------------------------------------------------

if ! command -v php >/dev/null 2>&1; then
    echo "spike-driver: skipping — \`php\` not on PATH" >&2
    exit 77
fi

# Build the cdylib (idempotent — cargo no-ops when up to date).
cargo build -p php-analyze --manifest-path "$REPO_ROOT/Cargo.toml" >&2

# Locate the built cdylib. We use `cargo metadata` rather than
# hard-coding `target/debug` so this works under `CARGO_TARGET_DIR`
# overrides used by some CI configurations.
TARGET_DIR="$(
    cargo metadata --format-version 1 --manifest-path "$REPO_ROOT/Cargo.toml" \
    | python3 -c "import sys, json; print(json.load(sys.stdin)['target_directory'])"
)"
CDYLIB="$TARGET_DIR/debug/libphp_analyze.so"
if [[ ! -f "$CDYLIB" ]]; then
    echo "spike-driver: cdylib not found at $CDYLIB after build" >&2
    exit 1
fi

# --- Per-run scratch dir -----------------------------------------------

TMPDIR_RUN="$(mktemp -d -t php-analyze-spike-XXXXXX)"
trap 'rm -rf "$TMPDIR_RUN"' EXIT
LOG_FILE="$TMPDIR_RUN/spike.log"
INI_FILE="$TMPDIR_RUN/spike.ini"

# Token resolution silently disables the extension when missing
# (Phase-1 behaviour). The spike module gates its own activation on
# Config.enabled AND Config.spike_observer, so we MUST supply a token
# and a server_url to clear the silent-disable bar. The values are
# never actually used — no shipper exists yet.
cat >"$INI_FILE" <<EOF
extension=$CDYLIB
php_analyze.enabled              = 1
php_analyze.server_url           = "https://spike.invalid/ingest"
php_analyze.auth_token           = "spike-driver-token-not-real"
php_analyze.spike_observer       = 1
php_analyze.spike_log_path       = "$LOG_FILE"
EOF

# --- Run each fixture --------------------------------------------------

for fixture in user_calls.php internal_calls.php throws.php; do
    if ! php -n -c "$INI_FILE" "$SCRIPT_DIR/$fixture" >>"$LOG_FILE.stdout" 2>>"$LOG_FILE.stderr"; then
        echo "spike-driver: php exited non-zero for $fixture" >&2
        echo "--- stderr ---" >&2
        cat "$LOG_FILE.stderr" >&2
        exit 1
    fi
done

# --- Hand the log path back to the caller ------------------------------

# The trap would `rm -rf` $TMPDIR_RUN on exit, deleting the log before
# the Rust test can read it. So we move the log to a path the caller
# owns (also under /tmp; the caller is responsible for cleanup).
KEEP_LOG="$(mktemp -t php-analyze-spike-XXXXXX.log)"
mv "$LOG_FILE" "$KEEP_LOG"

echo "$KEEP_LOG"
