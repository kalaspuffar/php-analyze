#!/usr/bin/env bash
# capture-reference-batches driver: for each of the three
# canonical workloads under `tests/php-bench/`, run the
# fixture under `php-analyze` against a freshly-spawned
# `stub-ingest --capture-dir`, populating
# `tools/captured-batches/<workload>/batch-NNNN.msgpack`.
#
# See ./captured-batches/README.md for what the captures are
# and how the downstream visualizer team consumes them.
#
# Exit status:
#   0   success — every workload's captures were written
#   1   prerequisite missing (no php binary, etc.)
#   2   module API mismatch between cdylib and PHP
#   3   PHP run failed for at least one workload
#   4   stub-ingest start failed or capture-dir validation failed
#   5   internal scripting error

set -euo pipefail

SCRIPT_PATH="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
SCRIPT_DIR="$(dirname "$SCRIPT_PATH")"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CAPTURED_DIR_ROOT="$SCRIPT_DIR/captured-batches"

print_help() {
    cat <<EOF
Usage: $0 [-h|--help]

Captures real wire-format MessagePack batches from each of
the three canonical workloads under tests/php-bench/ and
writes them to tools/captured-batches/<workload>/. Commits
to git so the downstream visualizer team has concrete test
fixtures.

Host requirements:
  - php8.3 or php8.4 (matching the freshly-built cdylib's
    module API; \`update-alternatives --config php-config\`
    selects which one cargo builds against).

The script is operator-driven — neither \`cargo test\` nor
CI invokes it.
EOF
}

case "${1:-}" in
    -h | --help)
        print_help
        exit 0
        ;;
esac

# --- Prerequisite: a usable PHP binary. ------------------------------------

resolve_php() {
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

# --- Build the cdylib + stub-ingest. ---------------------------------------

echo "==> building libphp_analyze.so + stub-ingest"
(cd "$REPO_ROOT" && cargo build -p php-analyze -p stub-ingest --quiet)
CDYLIB="$REPO_ROOT/target/debug/libphp_analyze.so"
STUB_BIN="$REPO_ROOT/target/debug/stub-ingest"

[[ -f "$CDYLIB" ]] || { echo "error: cdylib not built: $CDYLIB" >&2; exit 5; }
[[ -x "$STUB_BIN" ]] || { echo "error: stub-ingest not built: $STUB_BIN" >&2; exit 5; }

# --- Module-API mismatch probe. --------------------------------------------

MODULE_API_INI="$(mktemp --suffix=.ini)"
cleanup_module_ini() { rm -f "$MODULE_API_INI"; }
trap cleanup_module_ini EXIT
cat > "$MODULE_API_INI" <<EOF
display_startup_errors = Off
error_reporting = 0
extension = $CDYLIB

[php_analyze]
php_analyze.enabled = 0
php_analyze.server_url = "http://127.0.0.1:1/probe"
php_analyze.auth_token = "probe"
EOF

if ! "$PHP_BIN" -n -c "$MODULE_API_INI" -r 'exit(extension_loaded("php_analyze") ? 0 : 7);' > /dev/null 2>&1; then
    PHP_VER="$("$PHP_BIN" -r 'echo PHP_VERSION;' 2>/dev/null || echo "?")"
    echo "error: cdylib module API does not match $PHP_BIN ($PHP_VER)" >&2
    echo "       run \`sudo update-alternatives --config php-config\` then" >&2
    echo "       \`cargo clean -p php-analyze && cargo build -p php-analyze\`." >&2
    exit 2
fi

# --- Workload list. --------------------------------------------------------

WORKLOADS=(flat_calls json_batch recursive_walk)

# --- Helper: capture one workload. -----------------------------------------

capture_workload() {
    local workload="$1"
    local fixture="$REPO_ROOT/tests/php-bench/$workload.php"
    local capture_dir="$CAPTURED_DIR_ROOT/$workload"

    if [[ ! -r "$fixture" ]]; then
        echo "error: fixture not found: $fixture" >&2
        return 5
    fi

    # Save any existing README before nuking the workload dir,
    # then restore it after recreation. The README is part of
    # the human-written documentation; only the
    # `batch-NNNN.msgpack` files are regenerated.
    local saved_readme=""
    if [[ -f "$capture_dir/README.md" ]]; then
        saved_readme="$(cat "$capture_dir/README.md")"
    fi
    rm -rf "$capture_dir"
    mkdir -p "$capture_dir"
    if [[ -n "$saved_readme" ]]; then
        printf '%s' "$saved_readme" > "$capture_dir/README.md"
    fi

    # Spawn the stub on a loopback port, with capture-dir
    # pointed at the workload's directory.
    local stub_stdout stub_stderr stub_token stub_path
    stub_stdout="$(mktemp)"
    stub_stderr="$(mktemp)"
    stub_token="capture-$workload"
    stub_path="/v1/ingest"

    "$STUB_BIN" \
        --bind 127.0.0.1:0 \
        --auth-token "$stub_token" \
        --path "$stub_path" \
        --capture-dir "$capture_dir" \
        > "$stub_stdout" 2> "$stub_stderr" &
    local stub_pid=$!

    cleanup_stub() {
        if kill -0 "$stub_pid" 2>/dev/null; then
            kill -TERM "$stub_pid" 2>/dev/null || true
            wait "$stub_pid" 2>/dev/null || true
        fi
        rm -f "$stub_stdout" "$stub_stderr"
    }

    local stub_port=""
    for _ in $(seq 1 50); do
        if grep -q '^ready$' "$stub_stdout" 2>/dev/null; then
            stub_port="$(sed -n 's/^bound: 127\.0\.0\.1:\([0-9]*\)$/\1/p' "$stub_stdout" | head -1)"
            break
        fi
        sleep 0.1
    done

    if [[ -z "$stub_port" ]]; then
        echo "error: stub-ingest didn't print \`ready\` within 5s for $workload" >&2
        sed 's/^/  stderr: /' "$stub_stderr" >&2
        cleanup_stub
        return 4
    fi

    # Per-workload php.ini. shutdown_grace_ms = 5000 gives the
    # shipper plenty of time to drain whatever is queued at
    # MSHUTDOWN.
    local analyze_ini
    analyze_ini="$(mktemp --suffix=.ini)"
    cat > "$analyze_ini" <<EOF
extension = $CDYLIB
opcache.enable = 0

[php_analyze]
php_analyze.enabled = 1
php_analyze.server_url = "http://127.0.0.1:$stub_port$stub_path"
php_analyze.auth_token = "$stub_token"
php_analyze.spike_observer = 0
php_analyze.shutdown_grace_ms = 5000
php_analyze.shipper_queue_depth = 1024
EOF

    if ! "$PHP_BIN" -n -c "$analyze_ini" "$fixture" > /dev/null 2> "$capture_dir/.run.stderr"; then
        echo "error: PHP run failed for $workload; see $capture_dir/.run.stderr" >&2
        rm -f "$analyze_ini"
        cleanup_stub
        return 3
    fi
    rm -f "$analyze_ini"

    # Give the stub one second to drain any in-flight POSTs
    # (MSHUTDOWN-side shipping is asynchronous).
    sleep 1

    cleanup_stub

    # Cap the committed set to a small representative sample per
    # workload. The full run produces many batches (e.g.
    # flat_calls.php's 10⁶ calls produce ~50 batches of ~1 MB
    # each). Visualizer parser tests need only a handful of
    # samples to verify their decoder; committing ~100 MB of
    # binary fixtures per regen would grow the git history past
    # any reasonable threshold.
    #
    # We keep batch-0001, batch-0002, and the **final** batch:
    #
    #   - Batch 1 is the recorder's first flush, which carries
    #     the script-body's `closure:<file>:1` dict entry plus
    #     a partial run of the workload's hot path.
    #   - Batch 2 is a steady-state mid-run flush.
    #   - The final batch carries the MSHUTDOWN-drain output:
    #     one `CallRecord` per still-open CallFrame, with
    #     `abnormal_exit = true`. PHP's script-body closure is
    #     the canonical case — it sits on the stack from script
    #     start to MSHUTDOWN, so its record lands here as
    #     `(call_id=1, parent=0, depth=0, abnormal_exit=true)`.
    #     Without this sample the committed fixtures would not
    #     reflect the drained-root wire shape that
    #     `SPECIFICATION.md` §3.2 mandates.
    #
    # The downstream parser team can rerun
    # `tools/capture-fixtures.sh` and adjust the keep policy if
    # they need more variety.
    local total
    total="$(find "$capture_dir" -maxdepth 1 -name 'batch-*.msgpack' -printf . 2>/dev/null | wc -c)"
    if (( total > 3 )); then
        # Order files by their numeric NNNN suffix. Keep the
        # first two and the last one; delete the rest.
        local sorted
        sorted="$(find "$capture_dir" -maxdepth 1 -name 'batch-*.msgpack' -printf '%f\n' \
            | sort -t- -k2 -n)"
        local first_two last
        first_two="$(echo "$sorted" | head -n 2)"
        last="$(echo "$sorted" | tail -n 1)"
        echo "$sorted" \
            | grep -vxF -e "$last" -e "$(echo "$first_two" | sed -n 1p)" -e "$(echo "$first_two" | sed -n 2p)" \
            | while read -r victim; do
                [ -n "$victim" ] && rm -f "$capture_dir/$victim"
            done
    fi
    local committed
    committed="$(find "$capture_dir" -maxdepth 1 -name 'batch-*.msgpack' -printf . 2>/dev/null | wc -c)"
    rm -f "$capture_dir/.run.stderr"
    echo "==> $workload: captured $total batches, kept $committed for git (first two + final)"
    return 0
}

# --- Drive the workloads. --------------------------------------------------

mkdir -p "$CAPTURED_DIR_ROOT"

for workload in "${WORKLOADS[@]}"; do
    if ! capture_workload "$workload"; then
        echo "error: capture failed for $workload (see above)" >&2
        exit 3
    fi
done

echo
echo "Captures written to $CAPTURED_DIR_ROOT."
echo "Review the diff with \`git status tools/captured-batches/\` and commit if you want to update the committed samples."
