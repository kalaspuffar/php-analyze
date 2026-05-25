# php-analyze

A function-call profiler for PHP, written in Rust. Loaded into PHP 8.3
or 8.4 as a `cdylib`, captures per-call timing + memory metrics for
**every** function call (user-defined and internal), and ships them as
MessagePack-encoded batches over HTTP/HTTPS to an ingest endpoint of
your choice.

The extension is the *data producer*. The collector + storage + UI
that turn those batches into flame graphs is a separate project,
deliberately decoupled by a versioned wire-format contract
([`SPECIFICATION.md` § 4.2](./SPECIFICATION.md)).

---

## Status

**Pre-v1, MVP-shippable.** The data-production side is feature-complete
for the v1 contract. The visualizer side is **not built** — that's a
separate downstream repo this project hands off to via wire-format
batches.

What works today:

- ✅ Loads cleanly into PHP 8.3 and 8.4 (CLI and PHP-FPM SAPIs), tested
  by integration tests in both.
- ✅ Captures every PHP function call (user + internal) via the Zend
  Observer API.
- ✅ Records `(t_in, t_out, cpu_user, cpu_sys, mem_in, mem_out, depth,
  abnormal_exit)` per call.
- ✅ Buffers per-trace, flushes on size/count thresholds, ships as
  MessagePack over HTTP with bounded retries.
- ✅ Bounded per-process memory (default 64 MiB); drops are surfaced via
  the `dropped_records` counter in every batch.
- ✅ Honours a hard MSHUTDOWN-drain deadline so a hung collector never
  stalls PHP process exit.
- ✅ Silent-disable on misconfiguration; never crashes PHP.
- ✅ Operator-driven Xdebug spot-check (`tools/xdebug-spot-check/`)
  validates the recorder's accuracy. Most recent run: 100% call
  coverage on `recursive_walk.php` vs Xdebug 3.5 ([REPORT.md](./tools/xdebug-spot-check/REPORT.md)).
- ✅ Reference MessagePack captures committed under
  [`tools/captured-batches/`](./tools/captured-batches/) for downstream
  parser tests.

What's known-incomplete (queued, not blocking MVP):

- ⏳ `meta.trace_id` is a zero-bytes placeholder (`00000000-0000-0000-0000-000000000000`).
  UUID v7 generation is a follow-up; the field is on the wire, just
  empty.
- ⏳ The downstream visualizer stack (collector, ingester, UI) is a
  separate repo and **not yet built**. The reference HTTP collector at
  [`crates/stub-ingest/`](./crates/stub-ingest/) is a test-only fixture,
  not a production collector. See the [handover docs](#handover-to-the-visualizer-team)
  for the contract.
- ⏳ PECL packaging, TLS-CA integration test, and `cargo audit` in CI
  are deferred; install is from source today.

### Should you try it?

| If you... | Then... |
| --- | --- |
| ...want to **profile a PHP 8.3 / 8.4 app on Linux** and have somewhere to ship the data | **Yes**, give it a try. Start with the [`stub-ingest`](./crates/stub-ingest/) reference collector to validate the pipeline end-to-end. |
| ...are building or planning to build a **PHP profiling UI** and want a clean data source | **Yes**, the wire format is stable and documented. The [handover docs](./tools/captured-batches/) include reference batches you can parse. |
| ...want a **drop-in flame-graph UI** with no integration work | **Not yet.** The visualizer stack is the next downstream project. |
| ...are on **macOS, Windows, or PHP < 8.3** | **No.** The extension targets Linux x86_64 + PHP 8.3 / 8.4 specifically. |
| ...need **production-grade security hardening** (TLS pinning, signed binaries, mTLS) | **Not yet.** The extension uses `rustls` with system CAs; everything beyond that is yours to add. |
| ...want **sub-2× overhead** on hot paths | Measure first. See [Performance](#performance). |

---

## Getting started

The fastest path from `git clone` to a working profiler. Takes ~10
minutes on a fresh Debian/Ubuntu host.

### 1. Install prerequisites

```bash
# PHP 8.3 (or 8.4) + dev headers
sudo apt install php8.3 php8.3-cli php8.3-dev

# Rust toolchain (if you don't have it)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

> **PHP 8.4?** Replace `php8.3` with `php8.4` everywhere. The extension
> works against both; pick whichever your application uses. If your
> system has both, `update-alternatives --config php-config` selects
> which one `cargo build` targets.

### 2. Clone and build

```bash
git clone https://github.com/kalaspuffar/php-analyze.git
cd php-analyze
cargo build --release -p php-analyze
```

The extension lands at `target/release/libphp_analyze.so`.

### 3. Run the reference collector locally

In a separate terminal, start the reference HTTP collector so the
extension has somewhere to send data:

```bash
cargo run --release -p stub-ingest -- \
    --bind 127.0.0.1:8765 \
    --auth-token "dev-token" \
    --path /v1/ingest
```

The collector prints `bound: 127.0.0.1:8765` and `ready`, then waits
for POSTs. Decoded batches are visible at `http://127.0.0.1:8765/debug/batches`
(JSON).

### 4. Write a minimal `php.ini`

```bash
cat > /tmp/php-analyze.ini <<EOF
extension = $(pwd)/target/release/libphp_analyze.so

[php_analyze]
php_analyze.enabled     = 1
php_analyze.server_url  = "http://127.0.0.1:8765/v1/ingest"
php_analyze.auth_token  = "dev-token"
EOF
```

### 5. Run a PHP script under the extension

```bash
cat > /tmp/hello.php <<'EOF'
<?php
function greet(string $name): string {
    return "Hello, $name!";
}
echo greet("world"), "\n";
EOF

php8.3 -n -c /tmp/php-analyze.ini /tmp/hello.php
```

### 6. Inspect what got captured

```bash
curl -s http://127.0.0.1:8765/debug/batches | head -50
```

You'll see one batch containing `meta` + `dict` + `calls` for the
`greet` call plus the script body's closure entry. That's the
end-to-end pipeline working.

For a **system-wide install** so every PHP request goes through the
extension:

```bash
# Find PHP's extension dir
EXT_DIR=$(php8.3 -i | sed -n 's/^extension_dir => \([^ ]*\) .*/\1/p')

# Install the .so
sudo cp target/release/libphp_analyze.so "$EXT_DIR/php_analyze.so"

# Edit /etc/php/8.3/cli/php.ini (and fpm/php.ini for FPM) to add:
#   extension = php_analyze.so
#   php_analyze.server_url = "https://your-collector.example.com/v1/ingest"
#   php_analyze.auth_token_file = "/etc/php-analyze/token"
sudo systemctl reload php8.3-fpm   # if you're profiling FPM
```

Verify the install:

```bash
php8.3 --ri php_analyze
```

The output lists every directive's resolved value, with `auth_token`
redacted as `***`.

---

## Configuration

The extension reads its configuration from **`php.ini` only**. Every
directive below is `PHP_INI_SYSTEM` scope: userland `ini_set()` calls
will not change them and return `false`.

### Core directives

| Directive | Type | Default | Range | Effect |
| --- | --- | --- | --- | --- |
| `php_analyze.enabled` | bool | `1` | `0` or `1` | Master on/off switch. `0` registers no observer hooks; the extension is a no-op load. |
| `php_analyze.server_url` | string | *(none)* | full URL (`http://` or `https://`) | Ingest endpoint. Missing → silent-disable + one `E_WARNING`. |
| `php_analyze.auth_token` | string | *(none)* | non-empty | Bearer token sent as `Authorization: Bearer <token>`. Missing AND `auth_token_file` missing → silent-disable + one `E_WARNING`. |
| `php_analyze.auth_token_file` | string | *(none)* | absolute path | Read the bearer token from a file (UTF-8, trailing whitespace trimmed). **Wins over `auth_token` if both are set.** Failure to read → silent-disable + one `E_WARNING` (does NOT fall back to inline token). |

### Buffering and flushing

| Directive | Type | Default | Range | Effect |
| --- | --- | --- | --- | --- |
| `php_analyze.flush_records` | int | `10000` | `[1, 10⁹]` | Flush a batch after this many call records have been buffered. Tightening makes batches smaller and more frequent. |
| `php_analyze.flush_bytes` | int | `1048576` (1 MiB) | `[1024, 10⁹]` | Flush a batch when its estimated wire size reaches this many bytes. Whichever of `flush_records` / `flush_bytes` is hit first wins. |
| `php_analyze.buffer_cap_bytes` | int | `67108864` (64 MiB) | `[flush_bytes, 10¹⁰]` | **Hard per-process memory cap.** New records that would push the in-memory total past this are dropped, with the drop counted in `meta.dropped_records`. The PHP request continues normally. |
| `php_analyze.max_depth` | int | `1024` | `[1, 65535]` | Maximum tracked call-stack depth. Deeper recursion is not recorded; the overflow count is added to `meta.dropped_records`. |

### HTTP transport

| Directive | Type | Default | Range | Effect |
| --- | --- | --- | --- | --- |
| `php_analyze.retry_count` | int | `3` | `[0, 10]` | HTTP retry attempts on non-2xx / network error. After `retry_count + 1` total attempts, the batch is dropped. |
| `php_analyze.retry_backoff_ms` | int | `100` | `[1, 60000]` | Base backoff (ms). Doubles per attempt (`100 → 200 → 400 → 800` …). |
| `php_analyze.http_timeout_ms` | int | `2000` | `[100, 60000]` | Per-attempt HTTP timeout in milliseconds. |
| `php_analyze.shutdown_grace_ms` | int | `5000` | `[0, 60000]` | Maximum time (ms) the MSHUTDOWN drain spends flushing remaining batches before abandoning. Under PHP-FPM, FPM's own `~5s` SIGQUIT→SIGTERM ceiling caps this regardless of the configured value. |
| `php_analyze.shipper_queue_depth` | int | `8` | `[1, 1024]` | Capacity of the in-process MPSC channel between the recorder and the shipper thread. Full → recorder drops the new batch (drop-newest) and bumps `dropped_records`. Raise this for bursty workloads where the recorder out-paces the shipper. |

### Performance tuning

| Directive | Type | Default | Range | Effect |
| --- | --- | --- | --- | --- |
| `php_analyze.cpu_snapshot_mode` | string | `per-call` | `{per-call, off}` | Per-call CPU-time capture policy. `per-call` (default) calls `getrusage(RUSAGE_THREAD)` at every begin/end snapshot; `cpu_u` / `cpu_s` in every record reflect real CPU time (with microsecond granularity — sub-µs calls read `0`). `off` skips the syscall entirely; `cpu_u` and `cpu_s` are always `0`. Saves ~1000 ns/call on hot paths. Use `off` in high-volume pools where per-call CPU attribution doesn't matter; the wire shape is unchanged. |

### Behaviour notes

- **Silent-disable posture.** When configuration is incomplete or
  invalid, the extension marks itself disabled and emits exactly **one**
  `E_WARNING` per process at PHP startup. PHP itself starts normally;
  the rest of the request runs as if the extension weren't there. This
  is intentional: a typo in `php.ini` must never take down an FPM pool.
- **Range clamping.** Numeric directives outside their `[min, max]`
  range are clamped to the nearest bound, with one `E_WARNING` per
  offending directive.
- **HTTP warning.** `http://` URLs are accepted (useful for the
  local-stub case) but emit one `E_WARNING` flagging the absence of
  TLS. Production deployments should use `https://`.
- **No PHP-facing API.** The extension exposes no userland functions,
  constants, or stream wrappers. Userland code cannot detect, query,
  or alter its behaviour at runtime. Inspection lives in `phpinfo()`
  / `php --ri php_analyze`.

---

## Architecture (in one diagram)

```
                ┌─────────────────────────────────────────────────────┐
                │              ONE PHP PROCESS (CLI or FPM worker)    │
                │                                                     │
 Zend Engine ──▶│  Recorder (request thread)                          │
 fcall begin    │   - per-trace state                                 │
       /end     │   - function dictionary                             │
                │   - buffer accounting                               │
                │   - drop-newest on cap                              │
                │           │                                         │
                │           ▼  bounded MPSC channel                   │
                │   ┌──────────────────────────┐                      │
                │   │ Shipper (bg thread)      │ HTTPS POST           │
                │   │  - MsgPack encode        │─────┐                │
                │   │  - HTTP + retry          │     │                │
                │   │  - MSHUTDOWN-bounded     │     │                │
                │   │    drain                 │     │                │
                │   └──────────────────────────┘     │                │
                └────────────────────────────────────┼────────────────┘
                                                     ▼
                                        Your HTTP ingest endpoint
                                        (collector → storage → UI)
```

- One **Recorder** lives on the PHP request thread. The hot path
  (begin + end handlers) is zero-allocation in steady state.
- One **Shipper** thread per PHP process, lazy-spawned at first
  `RINIT` so the extension is fork-safe under PHP-FPM.
- Communication is a bounded MPSC channel; the recorder enforces the
  memory budget at enqueue time. Both sides accept that data loss is
  preferable to blocking the PHP request.

For the full architecture see [`SPECIFICATION.md`](./SPECIFICATION.md);
for design context, operator-stated priorities, and the MVP-handoff
posture see [`COMMENTS.md`](./COMMENTS.md).

---

## Performance

Numbers from the build host (a developer laptop, x86_64 Linux,
PHP 8.4.21). These are **rough** characterisations; the right
benchmark is your own workload.

Geo-mean wall-clock overhead vs unprofiled PHP across the three
canonical workloads:

| Mode | Geo-mean overhead | `json_batch.php` (most realistic) |
| --- | ---: | ---: |
| `cpu_snapshot_mode = per-call` (default) | ~9× | ~2.3× |
| `cpu_snapshot_mode = off` | ~5× | ~1.9× |

The overhead is dominated by **per-call syscall floor**: each function
call costs roughly one `clock_gettime` (~50 ns vDSO) and one
`getrusage(RUSAGE_THREAD)` (~500 ns real syscall). For sub-microsecond
PHP calls (very tight loops over `noop` etc.) this floor is structural;
for typical application calls (anything that touches the database, IO,
or non-trivial computation) the overhead is much smaller in relative
terms. See [`COMMENTS.md` C-19](./COMMENTS.md) for the detailed
syscall-cost breakdown and trade-off analysis.

**Accuracy versus Xdebug** (from a recent
[`tools/xdebug-spot-check/REPORT.md`](./tools/xdebug-spot-check/REPORT.md)
run on `recursive_walk.php`, 245,681 calls):

- Call coverage: **100%** (every call Xdebug observed, `php-analyze`
  also observed).
- Per-call duration |Δ%|: p50 ≈ 20%, p95 ≈ 86%, p99 ≈ 480%.

The timing-delta numbers reflect that **both** Xdebug and
`php-analyze` add per-call instrumentation overhead — durations are
not directly comparable to unprofiled PHP. The relevant signal is per-
call **shape**, not nanosecond-for-nanosecond agreement.

---

## Wire format and downstream consumers

The extension talks to its collector via HTTP POSTs carrying
MessagePack batches. The full byte-level contract is in
[`SPECIFICATION.md` § 4.2](./SPECIFICATION.md). Key facts:

- **Schema version:** v1 (the `meta.schema_version` field is `1`).
- **Media type:** `application/vnd.php-analyze.v1+msgpack`.
- **Default path suggestion:** `/v1/ingest` (operator-configurable
  via `server_url`).
- **Auth:** `Authorization: Bearer <token>` with a static token.

### For collector authors

Three primary sources documenting the contract:

1. [`SPECIFICATION.md` § 4.2 and § 5.2](./SPECIFICATION.md) — the
   authoritative wire format and HTTP contract.
2. [`crates/stub-ingest/`](./crates/stub-ingest/) — a working
   minimal collector you can read as documentation (~700 lines of
   Rust). Not production-ready (in-memory only, single-threaded), but
   exhaustively tested against the real recorder.
3. [`tools/captured-batches/`](./tools/captured-batches/) — three
   real MessagePack batches per canonical workload, committed as
   reference parser-test inputs. Decode them with any MessagePack
   library to validate your collector's parser.

### Handover to the visualizer team

If you're building (or planning to build) the downstream visualizer,
there is a self-contained handover document set ready to be moved into
the visualizer's repository. It covers the wire format, the HTTP
contract, operational expectations, and a checklist of architectural
decisions the visualizer team owns. The set is **not** committed to
this repo — generate or regenerate it via
[`tools/capture-fixtures.sh`](./tools/capture-fixtures.sh) and copy
the [`tools/captured-batches/`](./tools/captured-batches/) tree
plus the per-doc set into your target repo.

---

## Documentation index

| Document | What it covers |
| --- | --- |
| [`README.md`](./README.md) | This file. The fast-path overview. |
| [`SPECIFICATION.md`](./SPECIFICATION.md) | Authoritative design: wire format, components, threading model, lifecycle hooks, error handling. |
| [`REQUIREMENTS.md`](./REQUIREMENTS.md) | Source requirements elicited from the operator; what V1 must do. |
| [`COMMENTS.md`](./COMMENTS.md) | Forward-looking design notes, deferred work, operator-stated priorities, and the MVP-handoff posture (§6). |
| [`crates/stub-ingest/README.md`](./crates/stub-ingest/README.md) | The reference HTTP collector binary's usage and CLI surface. |
| [`tools/xdebug-spot-check/README.md`](./tools/xdebug-spot-check/README.md) | The accuracy-vs-Xdebug spot-check tool's usage, host requirements, and known limitations. |
| [`tools/xdebug-spot-check/REPORT.md`](./tools/xdebug-spot-check/REPORT.md) | Most-recent accuracy-vs-Xdebug spot-check report from the build host. |
| [`tools/captured-batches/README.md`](./tools/captured-batches/README.md) | Reference MessagePack batches for downstream parser tests. |

---

## Development

Pre-commit gates (CI enforces all three):

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

The integration tests skip cleanly when their environment isn't set
up; opt them in with:

```bash
# Run the CLI shipper round-trip test (requires php8.3 or php8.4)
PHP_ANALYZE_RUN_SHIPPER=1 cargo test -p php-analyze --test shipper_round_trip

# Run the FPM integration test (requires php8.3-fpm or php8.4-fpm)
PHP_ANALYZE_RUN_FPM=1 cargo test -p php-analyze --test fpm_repeated_requests

# Run the recorder observer-hook test
PHP_ANALYZE_RUN_RECORDER=1 cargo test -p php-analyze --test recorder_observer
```

Run a single test by name:

```bash
cargo test -p php-analyze <test_name>
```

The project follows a spec-driven workflow: implementation work
happens on focused branches, one self-contained change per branch,
each gated by the three pre-commit checks above. The contract surface
for every component lives in [`SPECIFICATION.md`](./SPECIFICATION.md);
deviations and design notes accumulate in
[`COMMENTS.md`](./COMMENTS.md).

### Repository layout

```
.
├── crates/
│   ├── php-analyze/             # The extension (cdylib)
│   │   ├── src/                 # Recorder, shipper, config, bootstrap, wire
│   │   ├── tests/               # Rust-side integration tests
│   │   └── benches/             # criterion benches + workload-overhead bench
│   └── stub-ingest/             # Reference HTTP collector (test-only)
├── tests/
│   ├── php-bench/               # Canonical workloads (used by benches + captures)
│   ├── php-fpm/                 # PHP fixtures for FPM integration test
│   ├── php-recorder/            # PHP fixtures for recorder-observer test
│   └── php-shipper/             # PHP fixtures for shipper round-trip test
├── tools/
│   ├── xdebug-spot-check/       # Accuracy-vs-Xdebug spot-check tool
│   ├── capture-fixtures.sh      # Regenerate the reference batch captures
│   └── captured-batches/        # Committed reference MessagePack batches
├── SPECIFICATION.md             # Authoritative design
├── REQUIREMENTS.md              # Operator requirements
└── COMMENTS.md                  # Design notes + MVP handoff posture
```

### Contributing

Issues and pull requests are welcome. Before opening a PR:

1. Read [`SPECIFICATION.md`](./SPECIFICATION.md) (the contract) and
   [`COMMENTS.md`](./COMMENTS.md) (the discussion overlay) so your
   change lands in a place the rest of the codebase expects.
2. Run the three pre-commit gates above and confirm they pass.
3. For changes that touch the wire format, the configuration surface,
   or any cross-component contract: open an issue first to discuss
   the design — the contract surface is shared with downstream
   consumers and unannounced changes can break them.

The maintainer's email is on commits; please tag the maintainer for
review on substantive changes.

---

## Limitations

- **Linux x86_64 only.** No macOS, no Windows, no ARM. Adding ARM is
  mostly a matter of ext-php-rs and rustls cross-compilation; nobody
  has needed it yet.
- **PHP 8.3 / 8.4 only.** PHP 8.2 lacks the Zend Observer API
  features the recorder relies on; PHP 8.5+ should work but is
  untested.
- **Opcode-specialised internals are invisible.** A small set of
  PHP built-ins (notably `strlen` when called with a constant
  argument) are inlined by the opcode specializer and never invoke
  the Zend Observer callback. The recorder cannot see these calls.
  Xdebug observes them via a different mechanism; this is a known
  coverage gap inherited from the Observer API. See
  [`COMMENTS.md` C-5](./COMMENTS.md).
- **Trace IDs are zero-placeholder today.** `meta.trace_id` reads
  `00000000-0000-0000-0000-000000000000` until UUID v7 generation
  lands. Plan for unique IDs at the collector side; tolerate the
  zero placeholder.
- **Drops are best-effort surfaced.** When the MSHUTDOWN-drain
  deadline truncates the tail of the shipper queue, those drops are
  counted internally but may not surface in any received batch (no
  subsequent batch exists to stamp the bumped counter). Plan for the
  collector to occasionally miss the tail of long-running CLI traces
  if `shutdown_grace_ms` is set tight.
- **No replay.** If your collector returns 2xx and crashes before
  persisting, the batch is lost from the extension's perspective.
  The extension does not spool to disk.
- **Bearer-token auth only.** No mTLS, no OAuth, no key rotation
  signal from the extension. Operators rotate tokens by editing the
  file and reloading the FPM pool.

---

## License

[MIT](./LICENSE). © 2026 Daniel Persson.
