#!/usr/bin/env bash
# xdebug-spot-check driver: builds the cdylib + stub-ingest,
# runs the chosen fixture once under Xdebug and once under
# php-analyze, then invokes compare.py to render REPORT.md.
#
# See ./README.md for host requirements and how to read the
# resulting report.
#
# Exit status:
#   0   success — REPORT.md was written
#   1   prerequisite missing (xdebug, msgpack, php-config, etc.)
#   2   module API mismatch between cdylib and PHP
#   3   PHP fixture run failed (Xdebug or analyze side)
#   4   compare.py failed
#   5   internal scripting error (unset var, bad fixture path)

set -euo pipefail

# Resolve repo paths from this script's location, not from the
# operator's CWD. The operator can invoke us from anywhere.
SCRIPT_PATH="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
SCRIPT_DIR="$(dirname "$SCRIPT_PATH")"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LAST_RUN_DIR="$SCRIPT_DIR/last-run"

print_help() {
    cat <<EOF
Usage: $0 [-h|--help] [<fixture.php>]

Runs the xdebug-spot-check accuracy comparison once and writes
the report to tools/xdebug-spot-check/REPORT.md.

Arguments:
  <fixture.php>  Optional path to a PHP fixture. Defaults to
                 tests/php-bench/recursive_walk.php.

Host requirements:
  - php8.3 or php8.4 (matching the freshly-built cdylib's
    module API; \`update-alternatives --config php-config\`
    selects which one cargo builds against).
  - The matching xdebug 3.x package (php8.3-xdebug /
    php8.4-xdebug on Debian-family).
  - python3 with the msgpack module (python3-msgpack on
    Debian-family, or \`pip install --user msgpack\`).

See tools/xdebug-spot-check/README.md for the full guide.
EOF
}

case "${1:-}" in
    -h | --help)
        print_help
        exit 0
        ;;
esac

FIXTURE_ARG="${1:-}"
if [[ -z "$FIXTURE_ARG" ]]; then
    FIXTURE="$REPO_ROOT/tests/php-bench/recursive_walk.php"
else
    # Resolve to an absolute path; PHP needs absolute paths in
    # the included-files column of the Xdebug trace for the
    # comparator's closure-name normalisation to work.
    if [[ -f "$FIXTURE_ARG" ]]; then
        FIXTURE="$(cd "$(dirname "$FIXTURE_ARG")" && pwd)/$(basename "$FIXTURE_ARG")"
    else
        echo "error: fixture not found: $FIXTURE_ARG" >&2
        exit 5
    fi
fi

if [[ ! -r "$FIXTURE" ]]; then
    echo "error: fixture not readable: $FIXTURE" >&2
    exit 5
fi

# --- Prerequisite #1: a usable PHP binary. ----------------------------------

resolve_php() {
    # Prefer the bare name (if on PATH) so the operator's chosen
    # update-alternatives wins. Fall back to /usr/bin/php<ver>.
    local name fallback
    for name in php8.4 php8.3; do
        if command -v "$name" > /dev/null 2>&1; then
            echo "$name"
            return 0
        fi
    done
    for fallback in /usr/bin/php8.4 /usr/bin/php8.3; do
        if [[ -x "$fallback" ]]; then
            echo "$fallback"
            return 0
        fi
    done
    return 1
}

PHP_BIN="$(resolve_php)" || {
    echo "error: no php8.3 or php8.4 on PATH; install with: apt install php8.4" >&2
    exit 1
}

# --- Prerequisite #2: Xdebug loaded into that PHP. -------------------------

# Use `extension_loaded` rather than parsing `php -m`'s output:
# the latter went through a pipe under `set -euo pipefail` and
# was observed to flake (the pipe segment occasionally
# reported failure even when grep matched, on this host). The
# `-r` invocation has a single deterministic exit code.
if ! "$PHP_BIN" -r 'exit(extension_loaded("xdebug") ? 0 : 1);' > /dev/null 2>&1; then
    PHP_SHORT=$("$PHP_BIN" -r 'echo PHP_MAJOR_VERSION . "." . PHP_MINOR_VERSION;' 2>/dev/null || echo "?.?")
    echo "error: Xdebug is not loaded in $PHP_BIN" >&2
    echo "       install with: apt install php${PHP_SHORT}-xdebug" >&2
    exit 1
fi

XDEBUG_VERSION="$("$PHP_BIN" --ri xdebug 2>/dev/null | sed -n 's/.*Version => \([0-9.]*\).*/\1/p' | head -1)"
if [[ -z "$XDEBUG_VERSION" ]]; then
    echo "error: could not detect Xdebug version from \`$PHP_BIN --ri xdebug\`" >&2
    exit 1
fi
case "$XDEBUG_VERSION" in
    3.*) : ;; # 3.x is supported
    *)
        echo "error: Xdebug $XDEBUG_VERSION found — only 3.x is supported (trace format differs in 2.x)" >&2
        exit 1
        ;;
esac

# --- Prerequisite #3: python3 with msgpack. --------------------------------

if ! python3 -c 'import msgpack' > /dev/null 2>&1; then
    echo "error: python3 is missing the \`msgpack\` module" >&2
    echo "       install with: apt install python3-msgpack" >&2
    echo "       or:           pip install --user msgpack" >&2
    exit 1
fi

# --- Build the cdylib + stub-ingest. ---------------------------------------

echo "==> building libphp_analyze.so + stub-ingest"
(cd "$REPO_ROOT" && cargo build -p php-analyze -p stub-ingest --quiet)
CDYLIB="$REPO_ROOT/target/debug/libphp_analyze.so"
STUB_BIN="$REPO_ROOT/target/debug/stub-ingest"

if [[ ! -f "$CDYLIB" ]]; then
    echo "error: cdylib not found after cargo build: $CDYLIB" >&2
    exit 5
fi

# --- Module API check. -----------------------------------------------------

# Write a throwaway php.ini that loads the cdylib, then invoke
# PHP with `extension_loaded("php_analyze")`. If the cdylib's
# module API doesn't match this PHP's, PHP prints "Module
# compiled with module API=NNN / PHP compiled with module
# API=MMM" to stderr and `extension_loaded` returns false.
MODULE_API_INI="$(mktemp --suffix=.ini)"
trap 'rm -f "$MODULE_API_INI"' EXIT
# Set `server_url` + `auth_token` so the probe doesn't trip
# the silent-disable startup warning ("server_url is not
# configured"). `display_startup_errors = Off` plus
# `error_reporting = 0` silences the master-switch-off notice
# that fires when MINIT runs with `enabled = 0` — both of
# which go to stdout in this PHP build's startup error path,
# escaping a `2> /dev/null` redirect.
cat > "$MODULE_API_INI" <<EOF
display_startup_errors = Off
error_reporting = 0
extension = $CDYLIB

[php_analyze]
php_analyze.enabled = 0
php_analyze.server_url = "http://127.0.0.1:1/probe"
php_analyze.auth_token = "probe"
EOF

if ! "$PHP_BIN" -n -c "$MODULE_API_INI" -r 'exit(extension_loaded("php_analyze") ? 0 : 7);' 2> /dev/null; then
    EXIT=$?
    if [[ $EXIT -eq 7 ]]; then
        PHP_API=$("$PHP_BIN" -r 'echo PHP_VERSION;' 2>/dev/null || echo "?")
        echo "error: cdylib module API does not match $PHP_BIN ($PHP_API)" >&2
        echo "       the cdylib was built against whatever \`php-config\` resolves to;" >&2
        echo "       run \`sudo update-alternatives --config php-config\` to point it at" >&2
        echo "       the matching version, then \`cargo clean -p php-analyze && cargo build -p php-analyze\`." >&2
        exit 2
    fi
    echo "error: PHP failed to evaluate the module-API probe (exit $EXIT)" >&2
    exit 2
fi

# --- Per-run working directory. --------------------------------------------

rm -rf "$LAST_RUN_DIR"
mkdir -p "$LAST_RUN_DIR"

# --- Run the fixture under Xdebug. -----------------------------------------

# Find xdebug's `.so` path so we can load it explicitly (some
# Debian builds drop the auto-load .ini under
# /etc/php/<ver>/cli/conf.d, but `-n` skips those; we need an
# explicit `zend_extension = ...` line). PHP's --ri output names
# the module file; failing that, glob the standard PHP extension
# directories.
find_xdebug_so() {
    local maybe
    maybe="$("$PHP_BIN" --ini 2>/dev/null | sed -n 's|^Loaded Configuration File: *||p' || true)"
    # Try a more reliable approach: the extension directory.
    local ext_dir
    ext_dir="$("$PHP_BIN" -i 2>/dev/null | sed -n 's|^extension_dir => \([^ ]*\) .*|\1|p' | head -1)"
    if [[ -n "$ext_dir" && -f "$ext_dir/xdebug.so" ]]; then
        echo "$ext_dir/xdebug.so"
        return 0
    fi
    return 1
}

XDEBUG_SO="$(find_xdebug_so)" || {
    echo "error: could not locate xdebug.so in PHP's extension_dir" >&2
    exit 1
}

XDEBUG_INI="$LAST_RUN_DIR/xdebug.ini"
cat > "$XDEBUG_INI" <<EOF
zend_extension = $XDEBUG_SO
xdebug.mode = trace
xdebug.start_with_request = yes
xdebug.trace_format = 1
xdebug.collect_params = 0
xdebug.collect_return = 0
xdebug.output_dir = $LAST_RUN_DIR
xdebug.trace_output_name = trace
; Xdebug 3.x writes \`.xt.gz\` by default; disable compression
; so compare.py can read the plain TAB-delimited \`.xt\` file
; without piping through a decompressor.
xdebug.use_compression = false
EOF

echo "==> running fixture under Xdebug ($FIXTURE)"
"$PHP_BIN" -n -c "$XDEBUG_INI" "$FIXTURE" > "$LAST_RUN_DIR/xdebug.stdout" 2> "$LAST_RUN_DIR/xdebug.stderr" || {
    echo "error: PHP under Xdebug exited non-zero ($?); see $LAST_RUN_DIR/xdebug.stderr" >&2
    exit 3
}

XDEBUG_TRACE="$LAST_RUN_DIR/trace.xt"
if [[ ! -f "$XDEBUG_TRACE" ]]; then
    echo "error: Xdebug trace file not written: $XDEBUG_TRACE" >&2
    echo "       Xdebug stderr:" >&2
    sed 's/^/         /' "$LAST_RUN_DIR/xdebug.stderr" >&2
    exit 3
fi

# --- Spawn stub-ingest on a loopback port. ---------------------------------

STUB_TOKEN="spot-check-token-$$"
STUB_PATH="/v1/ingest"

# Use a coproc + a temp file for the handshake — coproc's fd
# wiring is finicky across bash versions; the temp-file
# approach is portable.
STUB_STDOUT="$LAST_RUN_DIR/stub.stdout"
STUB_STDERR="$LAST_RUN_DIR/stub.stderr"
"$STUB_BIN" --bind 127.0.0.1:0 --auth-token "$STUB_TOKEN" --path "$STUB_PATH" \
    > "$STUB_STDOUT" 2> "$STUB_STDERR" &
STUB_PID=$!

cleanup_stub() {
    if [[ -n "${STUB_PID:-}" ]] && kill -0 "$STUB_PID" 2> /dev/null; then
        kill -TERM "$STUB_PID" 2> /dev/null || true
        wait "$STUB_PID" 2> /dev/null || true
    fi
}
trap 'cleanup_stub; rm -f "$MODULE_API_INI"' EXIT

# Wait up to 5 seconds for the `bound: <addr>` and `ready` lines.
STUB_PORT=""
for _ in $(seq 1 50); do
    if grep -q '^ready$' "$STUB_STDOUT" 2> /dev/null; then
        STUB_PORT="$(sed -n 's/^bound: 127\.0\.0\.1:\([0-9]*\)$/\1/p' "$STUB_STDOUT" | head -1)"
        break
    fi
    sleep 0.1
done

if [[ -z "$STUB_PORT" ]]; then
    echo "error: stub-ingest didn't print \`ready\` within 5 s" >&2
    echo "       stub-ingest stdout:" >&2
    sed 's/^/         /' "$STUB_STDOUT" >&2
    echo "       stub-ingest stderr:" >&2
    sed 's/^/         /' "$STUB_STDERR" >&2
    exit 3
fi

echo "==> running fixture under php-analyze (stub on 127.0.0.1:$STUB_PORT)"

ANALYZE_INI="$LAST_RUN_DIR/analyze.ini"
cat > "$ANALYZE_INI" <<EOF
extension = $CDYLIB
opcache.enable = 0

[php_analyze]
php_analyze.enabled = 1
php_analyze.server_url = "http://127.0.0.1:$STUB_PORT$STUB_PATH"
php_analyze.auth_token = "$STUB_TOKEN"
php_analyze.spike_observer = 0
php_analyze.shutdown_grace_ms = 4000
php_analyze.shipper_queue_depth = 1024
EOF

"$PHP_BIN" -n -c "$ANALYZE_INI" "$FIXTURE" > "$LAST_RUN_DIR/analyze.stdout" 2> "$LAST_RUN_DIR/analyze.stderr" || {
    echo "error: PHP under php-analyze exited non-zero ($?); see $LAST_RUN_DIR/analyze.stderr" >&2
    exit 3
}

# Allow MSHUTDOWN's shipper drain to finish + the stub's
# tiny_http thread to acknowledge before fetching.
sleep 1

BATCHES_JSON="$LAST_RUN_DIR/batches.json"
if ! curl --silent --fail "http://127.0.0.1:$STUB_PORT/debug/batches" > "$BATCHES_JSON"; then
    echo "error: GET /debug/batches failed against the stub on port $STUB_PORT" >&2
    exit 3
fi

# Tear down the stub before invoking compare.py so the
# operator's `top` doesn't show a lingering child.
cleanup_stub

# --- Invoke the comparator. ------------------------------------------------

echo "==> rendering REPORT.md"
REPORT="$SCRIPT_DIR/REPORT.md"
python3 "$SCRIPT_DIR/compare.py" \
    --xdebug "$XDEBUG_TRACE" \
    --analyze "$BATCHES_JSON" \
    --fixture "$FIXTURE" \
    --cdylib "$CDYLIB" \
    --php-bin "$PHP_BIN" \
    --xdebug-version "$XDEBUG_VERSION" \
    --output "$REPORT" || {
    echo "error: compare.py failed (exit $?)" >&2
    exit 4
}

echo
echo "Spot-check report written to $REPORT (open it to read the verdict)."
