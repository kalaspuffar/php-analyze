# Project Specification: `php-analyze` — PHP Profiling Extension

| Field | Value |
| --- | --- |
| Version | 0.1 (draft) |
| Date | 2026-05-20 |
| Author | Solution Architect (with Daniel Persson, daniel.persson@textalk.se) |
| Sources | `REQUIREMENTS.md` v0.1 (2026-05-20); architectural decisions ratified in Phase-2 review |
| Audience | Rust Developer (consumes this doc via OpenSpec changes) |
| Companion docs | `REQUIREMENTS.md` (intent), `COMMENTS.md` (later clarifications, if any), `CLAUDE.md` (project workflow & style) |

> **Conflict rule (from `CLAUDE.md`)**: if `SPECIFICATION.md` and `COMMENTS.md` disagree, `COMMENTS.md` wins (it is the more recent clarification).

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Architecture Overview](#2-architecture-overview)
3. [System Components](#3-system-components)
4. [Data Architecture](#4-data-architecture)
5. [Interface Specifications](#5-interface-specifications)
6. [Security Architecture](#6-security-architecture)
7. [Infrastructure and Deployment](#7-infrastructure-and-deployment)
8. [Integration Points](#8-integration-points)
9. [Testing Strategy](#9-testing-strategy)
10. [Implementation Plan](#10-implementation-plan)
11. [Risks and Mitigations](#11-risks-and-mitigations)
12. [Appendices](#12-appendices)

---

## 1. Executive Summary

### 1.1 Project overview

`php-analyze` is a **Rust-implemented PHP extension** for PHP 8.3 and 8.4 on Linux x86_64 that instruments every PHP function call (user-defined and internal), records `(wall-time, CPU-user, CPU-sys, memory-delta)` per call, buffers the records in process memory, and ships them as MessagePack batches over HTTPS to an ingest server using a **per-process background shipper thread**. The data is later visualized as drillable percentage trees by the (out-of-scope) visualization layer.

### 1.2 Key objectives

| ID | Objective | Source |
| --- | --- | --- |
| OBJ-1 | Per-call wall/CPU/memory metrics for all PHP user + internal functions | REQ §3.O1, §7.3 |
| OBJ-2 | ≤ 2.0× geo-mean wall-time overhead vs. unprofiled | REQ §3 KPI, §8.1 NFR-PERF-1 |
| OBJ-3 | Never block, crash, or destabilize the PHP process | REQ §3.O4, §8.3 NFR-REL-1 |
| OBJ-4 | Identify and segregate data from many concurrent PHP processes | REQ §3.O2, §7.5 |
| OBJ-5 | Drop-newest overflow with surfaced drop counter (no silent loss) | REQ §7.7, §7.9 |
| OBJ-6 | Configurable via `php.ini` only; no PHP-side API surface | REQ §7.2, §8.2 NFR-SEC-2 |

### 1.3 Success criteria

V1 is complete when **all** acceptance criteria in REQ §15.1 are demonstrably met:

1. Builds from source on Linux x86_64 for PHP 8.3 and 8.4.
2. Loadable in CLI and PHP-FPM SAPIs.
3. Canonical workload (TBD per OQ-7) emits §4 batches; ≥ 99.5% call coverage vs. Xdebug; per-call `t_out − t_in` within ±5% of Xdebug; geo-mean overhead ≤ 2.0×.
4. Killing the ingest server mid-trace does not crash PHP; logs report drops.
5. Exception unwind produces records with `abnormal_exit = true`.
6. Exceeding `max_depth` does not crash; depth overflow counted in `dropped_records`.
7. `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test` all clean.
8. README documents every `php.ini` directive (default, range, effect).

### 1.4 Open-question resolutions (ratifies REQ §16)

| OQ | Resolution | Notes |
| --- | --- | --- |
| OQ-1 | **UUID v7** for `trace_id` | `uuid` crate with `v7` feature. Sortable & opaque to PHP code. |
| OQ-2 | **Media type** `application/vnd.php-analyze.v1+msgpack` | Version embedded; future incompatibilities ship under `v2+msgpack` etc. |
| OQ-3 | **Endpoint** `POST /v1/ingest` | Path is configured by the operator via the full `server_url`; default-path-suggestion documented in README. |
| OQ-4 | **`zend_observer` API** (begin/end fcall handlers) | Single registration covers user + internal calls. Risk R-2 mitigated by a spike (see §10.1, Phase 0). |
| OQ-5 | Crates: `ext-php-rs`, `rmp` + `rmp-serde`, `ureq` (rustls TLS), `uuid` (v7), `crossbeam-channel`, `thiserror` | See §11 dependencies. |
| OQ-6 | **Yes, ship stub ingest server** as a workspace member (`stub-ingest/`) for integration tests | Tiny `axum` or `tiny_http` binary. |
| OQ-7 | **Deferred**: canonical benchmark workloads marked TBD here, resolved in Phase 5 jointly with the operator | Initial candidates per REQ §15.3 carried forward. |
| OQ-8 | `CLOCK_MONOTONIC` for `t_in`/`t_out`; `CLOCK_REALTIME` for `start_time` | Avoid `_RAW`: cheaper, monotonic guarantee sufficient within a trace. |
| OQ-9 | **Silent disable + single startup `E_WARNING`** when `server_url` or `auth_token` missing | Per NFR-USE-2. |
| OQ-10 | **`php_analyze.auth_token_file`** supported alongside `php_analyze.auth_token` | If both set, `_file` wins; failure to read the file → silent disable + warning. |

### 1.5 Architectural decisions ratified in Phase 2

| AD | Decision | Implication |
| --- | --- | --- |
| AD-1 | **Background shipper thread** (one per process), records handed off via a bounded MPSC channel | HTTP I/O off the PHP request thread; FPM fork-safety requires lazy spawn on first `RINIT` (see §3.4). |
| AD-2 | **`zend_observer` API** as the sole interception mechanism | Risk R-2 retired by a Phase-0 spike. |
| AD-3 | **MessagePack encoding executes on the shipper thread**, not the request thread | Removes encoding cost from the hot path; allows `dropped_records` to be stamped at send time (REQ §9.3 wording). |
| AD-4 | **Silent-disable** posture on misconfiguration | Operator-friendly; one `E_WARNING` only. |
| AD-5 | New directive `php_analyze.shutdown_grace_ms` (default `5000`) | Bounds the shipper drain at `MSHUTDOWN` so a hung server can't stall process exit. |

---

## 2. Architecture Overview

### 2.1 High-level description

`php-analyze` is a single Rust crate that builds to a shared library (`.so`) loaded by PHP as an extension. Inside one PHP process the extension owns two logical actors that share no mutable state except a small set of atomics and a bounded channel:

- **Recorder** (runs on the PHP request thread). Registers with `zend_observer`, intercepts every function call, accumulates per-trace state, and on flush threshold or `RSHUTDOWN` hands a `PendingBatch` to the shipper.
- **Shipper** (runs on a dedicated background thread, one per process). Drains the channel, MessagePack-encodes each batch, performs HTTP POST with retries, and accounts retry-exhausted drops.

### 2.2 Topology diagram (text)

```
                         ┌──────────────────────────────────────────────────────────────┐
                         │                  ONE PHP PROCESS (CLI or FPM worker)         │
                         │                                                              │
   Zend Engine ──fcall──▶│  ┌──────────────────────────┐    crossbeam::bounded(N)       │
   begin / end           │  │ RECORDER (request thread)│    ┌────────────────────────┐ │
                         │  │  - Per-trace state       │───▶│   PendingBatch queue   │─┼─┐
                         │  │  - Function dictionary   │    └────────────────────────┘ │ │
                         │  │  - Buffer accounting     │                               │ │
                         │  │  - Drop-newest on cap    │     ┌─────────────────────┐   │ │
                         │  └──────────────────────────┘     │ SHIPPER (bg thread) │◀──┼─┘
                         │                                   │  - MsgPack encode   │   │
   php.ini ──MINIT──────▶│  Config (immutable post-MINIT)    │  - HTTP POST + TLS  │   │
                         │                                   │  - Retry / backoff  │   │
   PHP error log ◀───────┤                                   │  - Drain on MSHUT.  │   │
                         │                                   └─────────────────────┘   │
                         │           atomics: total_bytes_in_memory, trace.drop_count   │
                         └────────────────────────────────────┬─────────────────────────┘
                                                              │ HTTPS POST (rustls)
                                                              │ Authorization: Bearer
                                                              ▼
                                                ┌────────────────────────┐
                                                │   Ingest Server (out)  │
                                                │   (separate document)  │
                                                └────────────────────────┘
```

### 2.3 Key architectural decisions (and why)

| # | Decision | Rationale |
| --- | --- | --- |
| ADR-01 | Rust shared library, no C glue | Single language, OpenSpec workflow already standardized on Rust (CLAUDE.md). `ext-php-rs` handles Zend ABI. |
| ADR-02 | `zend_observer` API for interception | Official PHP 8.x mechanism; one registration covers user + internal; less invasive than execute-pointer override. |
| ADR-03 | Background shipper thread per process | HTTP I/O must not appear in PHP request wall time beyond enqueue cost; matches NFR-PERF-3 budget structure. |
| ADR-04 | Lazy thread spawn on first `RINIT` | POSIX `fork()` (FPM master → worker) kills non-calling threads; spawn must be post-fork, per-worker. |
| ADR-05 | MessagePack encoding on shipper thread | Removes encoding cost from the request hot path; lets `dropped_records` be stamped at send time. |
| ADR-06 | Bounded channel + global byte-accounting atomic | Drop-newest is enforced at the recorder before enqueue; one source of truth for memory budget. |
| ADR-07 | `ureq` + `rustls` HTTP client | Blocking API matches the synchronous shipper loop; no async runtime; small dep footprint; system CA store via `rustls-native-certs`. |
| ADR-08 | All config via `php.ini`, immutable after `MINIT` | Locked-down ops surface; matches REQ §8.2 NFR-SEC-2; no `ini_set()` exposure. |
| ADR-09 | Per-trace `Arc<AtomicU64>` drop counter | Both threads need to bump; fresh Arc per trace at `RINIT` prevents cross-trace contamination. |
| ADR-10 | Silent-disable on misconfig | A typo in `php.ini` must not take down an FPM pool (NFR-USE-2). |

### 2.4 Threading model

| Thread | Owns | May read | May write |
| --- | --- | --- | --- |
| **Recorder** (PHP request thread) | per-trace state struct, function dictionary, pending record buffer | global config (read-only), `Arc<AtomicU64> drop_counter`, `AtomicUsize bytes_in_memory` | drop counter, bytes_in_memory, the channel sender |
| **Shipper** (background) | HTTP client, retry/backoff state | global config, `Arc<AtomicU64> drop_counter`, channel receiver | drop counter (on retry exhaust), bytes_in_memory (on batch consumed) |
| **Module-init** (PHP master) | global config struct (read once from `php.ini`) | — | — |

Synchronization primitives:
- `crossbeam_channel::bounded(N)` for batch handoff (N = `shipper_queue_depth`, default `8`).
- `AtomicUsize` for total memory accounting (relaxed ordering — monotonicity-of-bound suffices).
- `AtomicBool` (compare-exchange) for "shipper thread spawned in this process".
- `Once`/`OnceLock` for global config initialization at `MINIT`.

### 2.5 What we are explicitly **not** designing

Per REQ §4.2 and §7.10:
- Production-safe / sampled mode.
- Generators, fibers, eval'd functions, late destructors as first-class concepts.
- Cross-service trace correlation.
- On-disk spool for failed batches.
- Allocation-count tracking (only memory delta).
- Function arguments / return values capture.

---

## 3. System Components

### 3.1 Component: Bootstrapper

**Purpose**: Wire the extension into PHP's lifecycle hooks.

**Responsibilities**:
- Register the extension via `ext-php-rs` macros.
- Implement `MINIT`, `MSHUTDOWN`, `RINIT`, `RSHUTDOWN`, `MINFO` callbacks.
- At `MINIT`: read all `php.ini` directives into a frozen `Config` struct; validate; emit a single `E_WARNING` if `server_url`/`auth_token` missing and mark the extension disabled for this process.
- At `MSHUTDOWN`: signal shipper to drain (bounded by `shutdown_grace_ms`), join thread, free globals.
- At `RINIT`: allocate a fresh `Trace` struct (UUIDv7, fresh drop counter, empty buffer); lazy-spawn shipper if not yet spawned in this process.
- At `RSHUTDOWN`: final-flush the pending buffer (regardless of threshold), release `Trace`.

**Interfaces consumed**:
- `ext-php-rs` macros: `#[php_module]`, lifecycle hook registration.
- `zend_observer` registration (called from `MINIT`).

**Interfaces produced**:
- Extension symbol exported per PHP/Zend ABI.

**Acceptance criteria**:
- AC-BS-1: Loading the extension with `php_analyze.enabled=0` activates no observer hooks (verifiable via `phpinfo()` plus a zero-overhead micro-benchmark).
- AC-BS-2: Missing `server_url` or `auth_token` produces exactly one `E_WARNING` per process and disables instrumentation; PHP continues normally.
- AC-BS-3: After 10⁴ requests on a single FPM worker, RSS growth is bounded (no per-request leak detectable by valgrind-style accounting).
- AC-BS-4: `MSHUTDOWN` returns within `shutdown_grace_ms + 200ms` even when the ingest server is hung.

### 3.2 Component: Recorder

**Purpose**: Capture per-call metrics on the PHP request thread; assemble batches; enforce caps.

**Responsibilities**:
- Implement `zend_observer` begin and end handlers.
- On **call begin**: capture `(t_now, cpu_user_now, cpu_sys_now, mem_now)`; push a `CallFrame { call_id, parent, fn_id, depth, t_in, cpu_u_in, cpu_s_in, mem_in }` onto the trace's call stack; if `depth ≥ max_depth`, do not push, bump `depth_overflow_count` (rolled into `dropped_records`).
- On **call end** (normal or via exception): pop the frame; compute `cpu_u = cpu_user_now − cpu_u_in` (saturating, may be `0` on monotonic-skew); emit a `CallRecord` into the pending buffer; on exception unwind set `abnormal_exit = true`.
- Maintain the **function dictionary**: on first sight of a function in this trace, allocate a `fn_id` (monotonic from 1) and stage a `DictEntry` for inclusion in the next batch.
- Check flush thresholds after each emitted record: if `records ≥ flush_records` OR `estimated_bytes ≥ flush_bytes`, hand current buffer to shipper.
- Enforce `buffer_cap_bytes`: if a new record would push `bytes_in_memory` (atomic snapshot) past the cap, drop the record (and any pending dict entry) and bump `drop_counter`.
- At `RSHUTDOWN`: emit final batch even if empty? **No** — only if non-empty (REQ F-BF-3 / F-LC-4).

**Clock sources**:
- Wall time (`t_in`, `t_out`): `clock_gettime(CLOCK_MONOTONIC, …)`.
- CPU time: `clock_gettime(CLOCK_PROCESS_CPUTIME_ID, …)` split into user/sys via `getrusage(RUSAGE_THREAD)` deltas — **caveat**: `getrusage` granularity is coarse (typically microseconds). See §11 R-PERF for the trade-off; default implementation uses `getrusage` deltas and exposes the granularity in the README. `RUSAGE_THREAD` (Linux 2.6.26+) returns CPU time for the calling thread only — required because the Recorder runs on the PHP request thread and the Phase-4 shipper runs on a separate thread; a `RUSAGE_SELF` reading would conflate the two and inflate per-call CPU deltas under load. **Operator opt-out** (`recorder-cpu-snapshot-cadence`): the `php_analyze.cpu_snapshot_mode` directive accepts `per-call` (default; this paragraph's behaviour) or `off`. Under `off` the `getrusage(RUSAGE_THREAD)` call is **skipped** at every begin/end snapshot and `CallRecord::cpu_u_ns` / `cpu_s_ns` are emitted as `0` regardless of function duration — saving ~1000 ns/call on hosts without vDSO for the syscall, at the cost of per-call CPU attribution. The wire format is unchanged in both modes; only the values of those two fields are affected. See `COMMENTS.md` C-19 for the syscall-cost gap analysis that motivated the directive.
- Memory: PHP `zend_memory_usage(true)` (real usage, including allocator overhead).
- `start_time` (per-trace metadata only): `clock_gettime(CLOCK_REALTIME, …)`.

**Size estimation**: a per-record fixed approximation (`64` bytes/record + per-dict-entry `len(fqn) + len(file) + 24`) suffices for the `flush_bytes` / `buffer_cap_bytes` checks. Exact bytes are known after encoding (shipper). The estimate is an over-approximation tuned to bound real-memory headroom.

**Interfaces consumed**:
- `zend_observer_fcall_register` (via ext-php-rs raw FFI if no high-level binding).
- POSIX `clock_gettime`, `getrusage`.
- PHP `zend_memory_usage`, `EG(exception)` (to detect exception unwind in the end handler).

**Interfaces produced**:
- `PendingBatch` values sent into the channel to the shipper.

**Acceptance criteria**:
- AC-RC-1: For a workload of N calls, exactly N `CallRecord`s are emitted modulo `max_depth` and `buffer_cap_bytes` drops (which are counted in `dropped_records`).
- AC-RC-2: A function that throws produces a record with `abnormal_exit = true`.
- AC-RC-3: For a recursive function calling itself >`max_depth` times, no crash; overflow count equals (calls − `max_depth`).
- AC-RC-4: After `RSHUTDOWN` returns, the per-trace state has been deallocated (verifiable in tests by holding a `Weak` reference).
- AC-RC-5: Hot path (begin + end handlers) performs **zero heap allocations** in the steady state (call stack and buffer pre-grown after warmup). Verified via an allocator-counting harness.
- AC-RC-6: `t_in`/`t_out` use a monotonic clock; running the system clock backwards mid-trace does not produce negative durations.

### 3.3 Component: Shipper

**Purpose**: Drain the channel, encode MessagePack, POST to the ingest server with retries, account for retry-exhaustion drops.

**Responsibilities**:
- Run a loop on a dedicated thread: `recv()` from the channel; on each `PendingBatch`:
  1. Build the `meta` map by reading the live `drop_counter.load(Relaxed)`.
  2. Encode `{ meta, dict, calls }` via `rmp_serde` into a `Vec<u8>`.
  3. Subtract `pending_batch.size_estimate` from the global `bytes_in_memory` atomic (memory is "released" from the budget once encoded; the encoded `Vec<u8>` itself is short-lived and not budgeted).
  4. POST via `ureq` with the configured headers, timeout = `http_timeout_ms`.
  5. On non-2xx / network error / timeout: backoff `retry_backoff_ms × 2^attempt`; retry up to `retry_count` times.
  6. On retry exhaustion: increment `drop_counter` by `records_in_this_batch`; log `E_NOTICE` (without token).
- On a special "drain" signal at `MSHUTDOWN`: process remaining batches with the same retry policy but observe a global deadline `now + shutdown_grace_ms`; any batches remaining past the deadline are abandoned and counted as dropped.
- Persistent connection: reuse the `ureq::Agent` across requests (it does HTTP keep-alive by default).

**Interfaces consumed**:
- `crossbeam_channel::Receiver<ShipperMessage>` where `ShipperMessage = Batch(PendingBatch) | Drain { deadline: Instant }`.
- `rmp_serde::to_vec_named`.
- `ureq::Agent` + system CA roots via `rustls-native-certs`.

**Interfaces produced**:
- HTTP POSTs to `server_url`.
- PHP error log lines (`E_NOTICE` on failure, `E_WARNING` on misconfig at startup — but startup warnings come from the Bootstrapper, not the Shipper).

**Acceptance criteria**:
- AC-SH-1: A 2xx response from the stub server removes the batch from queue and does not increment `drop_counter`.
- AC-SH-2: An always-500 stub increments `drop_counter` by exactly `records_in_batch` after `retry_count + 1` total attempts per batch.
- AC-SH-3: A black-holed server (no response) causes the shipper to time out per attempt at `http_timeout_ms ± 100ms`, then back off as configured.
- AC-SH-4: Bearer token never appears in any log output (verified by grepping `E_NOTICE` lines).
- AC-SH-5: HTTPS to a self-signed cert fails fast; HTTPS to a system-trusted cert succeeds (rustls + system CA).
- AC-SH-6: After 1000 successful sends, only one TCP connection has been opened (keep-alive verified).

### 3.4 Component: Process-wide bootstrap & shutdown

**Purpose**: Manage the once-per-process shipper thread spawn and shutdown.

**Responsibilities**:
- At `RINIT` (first time per process, guarded by `static SHIPPER_SPAWNED: AtomicBool`):
  - `compare_exchange(false, true)` → if won the race, spawn the shipper thread with a clone of the `Receiver`.
  - The `Sender` half lives in module-global `OnceLock<Sender<ShipperMessage>>`, initialized at `MINIT`.
- At `MSHUTDOWN`:
  - Send `ShipperMessage::Drain { deadline: now + shutdown_grace_ms }`.
  - Drop the `Sender` to signal channel close.
  - `JoinHandle::join()` with a `recv_timeout`-style guard (since `std::thread::JoinHandle::join` itself blocks indefinitely, we wrap the shipper to self-exit at the deadline and signal via a separate completion channel).

**Why this lives in its own component**: it spans the boundary between module-scoped globals (Bootstrapper's concern) and the shipper's runtime; isolating it keeps lifecycle bugs localized.

**Acceptance criteria**:
- AC-PB-1: Under PHP-FPM with `pm.max_children=8`, every worker process owns exactly one shipper thread after its first request (verifiable by `ls /proc/<pid>/task`).
- AC-PB-2: An FPM worker recycled by `pm.max_requests` runs `MSHUTDOWN`, joins the shipper, and exits within `shutdown_grace_ms + 200ms` even if the server hangs.

### 3.5 Component: Configuration

**Purpose**: Read, validate, and freeze `php.ini` directives at `MINIT`.

**Responsibilities**:
- Declare directives via `ext-php-rs` ini-entry macros (or raw `zend_ini_entry_def` if necessary).
- Read into a strongly-typed `Config` struct with field-level validation:
  - URL parse for `server_url` (must be `http://` or `https://`).
  - Integer ranges for thresholds (rejects ≤ 0).
  - Token loading: prefer `auth_token_file` if set; else `auth_token`; both empty → silent disable.
- Expose an immutable `&'static Config` via `OnceLock`.
- `php.ini` is the **only** configuration surface; no `ini_set()` for these directives (PHP_INI_SYSTEM scope).

**Configuration directives** (consolidates REQ §17.2 plus AD-5):

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
| `php_analyze.cpu_snapshot_mode` | string | `per-call` | `{per-call, off}` | Per-call CPU snapshot policy. `per-call` (spec-current) calls `getrusage(RUSAGE_THREAD)` per begin/end. `off` skips the syscall and emits `cpu_u_ns = cpu_s_ns = 0` in every record. See §3.2 for the trade-off. |

**Acceptance criteria**:
- AC-CF-1: Every directive has a default, range, and effect documented in README (§MAINT-1).
- AC-CF-2: An out-of-range value is logged at `E_WARNING` and clamped to the nearest in-range value; the process continues.
- AC-CF-3: `auth_token_file` overrides `auth_token` when both are set.
- AC-CF-4: All directives are `PHP_INI_SYSTEM` scope (no `ini_set()` from PHP code).

---

## 4. Data Architecture

### 4.1 In-memory data models

> All Rust types below are `#[repr(C)]`/POD-shaped where it matters for hot-path locality; serde derives are gated to the encoding boundary in the shipper.

#### 4.1.1 `Config` (frozen at `MINIT`)

```rust
pub struct Config {
    pub enabled: bool,
    pub server_url: Url,                 // validated
    pub auth_token: SecretString,        // never Debug-printed, never logged
    pub flush_records: usize,
    pub flush_bytes: usize,
    pub buffer_cap_bytes: usize,
    pub max_depth: u16,
    pub retry_count: u8,
    pub retry_backoff: Duration,
    pub http_timeout: Duration,
    pub shutdown_grace: Duration,
    pub shipper_queue_depth: usize,
}
```

#### 4.1.2 `Trace` (per-request, owned by Recorder)

```rust
pub struct Trace {
    pub trace_id: Uuid,                       // v7
    pub start_time_realtime_ns: i64,          // CLOCK_REALTIME at RINIT
    pub host: Arc<str>,                       // shared with module globals
    pub pid: u32,
    pub sapi: Arc<str>,
    pub uri_or_script: String,
    pub call_id_seq: u64,                     // monotonic; next = ++
    pub stack: Vec<CallFrame>,                // SmallVec<[CallFrame; 64]> in practice
    pub dict: FxHashMap<FunctionKey, u32>,    // key → fn_id
    pub dict_new_since_last_flush: Vec<DictEntry>,
    pub buffer: Vec<CallRecord>,
    pub buffer_estimated_bytes: usize,
    pub drop_counter: Arc<AtomicU64>,         // shared with shipper
}
```

`FunctionKey` is the identity of a PHP function for interning purposes. For user functions: `(file_path, function_name, line)`. For methods: `(class_name, method_name)`. For closures: the runtime pointer plus declaring `(file, line)` to disambiguate across requests. For internal functions: the function name only.

#### 4.1.3 `CallFrame` (stack-local, transient)

```rust
pub struct CallFrame {
    pub call_id: u64,
    pub parent: u64,
    pub fn_id: u32,
    pub depth: u16,
    pub t_in_ns: i64,        // CLOCK_MONOTONIC
    pub cpu_u_in_ns: i64,
    pub cpu_s_in_ns: i64,
    pub mem_in_bytes: i64,
}
```

#### 4.1.4 `CallRecord` (emitted at call exit; the §4.2 wire shape)

```rust
pub struct CallRecord {
    pub call_id: u64,
    pub parent: u64,
    pub fn_id: u32,
    pub depth: u16,
    pub t_in_ns: i64,
    pub t_out_ns: i64,
    pub cpu_u_ns: i64,       // exit − entry, saturating
    pub cpu_s_ns: i64,
    pub mem_in_bytes: i64,
    pub mem_out_bytes: i64,
    pub abnormal_exit: bool,
}
```

#### 4.1.5 `DictEntry`

```rust
pub struct DictEntry {
    pub fn_id: u32,
    pub fqn: String,
    pub file: String,
    pub line: u32,
    pub kind: FunctionKind,  // enum: Function | Method | Closure | Internal
}
```

#### 4.1.6 `PendingBatch` (the channel payload)

```rust
pub enum ShipperMessage {
    Batch(PendingBatch),
    Drain { deadline: Instant },
}

pub struct PendingBatch {
    pub meta_partial: MetaPartial,    // everything except dropped_records
    pub dict: Vec<DictEntry>,
    pub calls: Vec<CallRecord>,
    pub drop_counter: Arc<AtomicU64>, // shipper reads at encode time
    pub size_estimate: usize,         // for bytes_in_memory accounting
}

pub struct MetaPartial {
    pub schema_version: u8,           // 1
    pub trace_id: Uuid,
    pub host: Arc<str>,
    pub pid: u32,
    pub start_time_realtime_ns: i64,
    pub sapi: Arc<str>,
    pub uri_or_script: Arc<str>,      // immutable for trace lifetime
}
```

### 4.2 Wire format (MessagePack, schema v1)

Encoded media type: `application/vnd.php-analyze.v1+msgpack`.

Top-level batch is a MessagePack map with three keys:

| Key | Type | Content |
| --- | --- | --- |
| `meta` | map | `MetaFull` (see below) |
| `dict` | array | zero or more `DictEntry` maps (new since last batch) |
| `calls` | array | zero or more `CallRecord` maps, in emission order |

#### 4.2.1 `meta` map (`MetaFull`)

| Field | Type | Notes |
| --- | --- | --- |
| `schema_version` | uint8 | `1` |
| `trace_id` | string (36-char UUID) | UUID v7 |
| `host` | string | hostname |
| `pid` | uint32 | OS PID |
| `start_time` | int64 | nanoseconds since UNIX epoch (REALTIME) |
| `sapi` | string | `cli` / `fpm-fcgi` / etc. |
| `uri_or_script` | string | request URI or script path |
| `dropped_records` | uint64 | cumulative at send time |

#### 4.2.2 `dict` array entries

| Field | Type | Notes |
| --- | --- | --- |
| `fn_id` | uint32 | Matches `fn` in call records. |
| `fqn` | string | Fully qualified name. |
| `file` | string | Declaring file path (`""` if internal). |
| `line` | uint32 | Declaring line (`0` if internal). |
| `kind` | uint8 | `0=function`, `1=method`, `2=closure`, `3=internal` |

> **Note**: `kind` is encoded as a small int (not a string) to keep payloads tight. The README documents the integer mapping. This is a v1-frozen decision; v2 may re-encode as string.

#### 4.2.3 `calls` array entries

Each entry is a MessagePack map with the keys from §4.1.4 in the same names: `call_id, parent, fn, depth, t_in, t_out, cpu_u, cpu_s, mem_in, mem_out, abnormal_exit`. Note `fn` is the wire-shortened name for `fn_id` (per REQ §9.1).

> **Forward-compatibility rule**: unknown extra keys in any map MUST be ignored by the server (REQ-side defensive constraint). The extension only ever emits the keys above.

### 4.3 Persistence

The extension does not persist data. There is no on-disk cache, no spool file, no shared-memory ring. Process-local memory only; loss on process crash is acceptable per REQ §4.2 (no on-disk spooling).

### 4.4 Data volumes (for capacity planning, from REQ §9.5)

| Scenario | Records | Approx batch payload |
| --- | --- | --- |
| Small FPM req | 10³–10⁴ | < 100 KiB total |
| Typical FPM req | 10⁵ | ~5–10 MiB across ~10 batches |
| Heavy CLI | 10⁷ | hundreds of MiB across many batches |

The default `buffer_cap_bytes = 64 MiB` covers ~10⁶ records at ~64 bytes/record estimate; heavier traces accept some `dropped_records` unless the operator raises the cap.

---

## 5. Interface Specifications

### 5.1 PHP-facing surface (`php.ini` only)

The complete surface is the directive table in §3.5. **No PHP functions, constants, or classes are exposed.** Specifically:
- No `php_analyze_*()` userland functions.
- No global constants.
- No `ini_set()`-mutable directives (all are `PHP_INI_SYSTEM`).
- `phpinfo()` reports loaded version and the resolved config (with `auth_token` redacted as `***`).

### 5.2 Egress HTTP: `POST <server_url>`

#### Request

```
POST /<configured path>  HTTP/1.1
Host: <from server_url>
User-Agent: php-analyze/0.1
Authorization: Bearer <token>
Content-Type: application/vnd.php-analyze.v1+msgpack
Content-Length: <N>

<MessagePack-encoded Batch per §4.2>
```

- `User-Agent`: `php-analyze/<crate-version>`.
- `Connection`: keep-alive (default for HTTP/1.1; HTTP/2 negotiated by `ureq` if available).
- No `Content-Encoding`: payloads are not gzipped in v1 (MessagePack is already compact; gzip is a follow-up).

#### Response (server contract, REQ §10.1)

| Status | Meaning to extension |
| --- | --- |
| `2xx` | Success — batch removed from queue. |
| `4xx` | Treated as failure → retry policy → drop on exhaustion. (`401` and `400` will exhaust without recovery; logged.) |
| `5xx` | Treated as failure → retry policy. |
| network error / timeout / TLS error | Treated as failure → retry policy. |

The extension does **not** inspect response bodies in v1.

#### Error handling and retries

1. Attempt 0: timeout = `http_timeout_ms`.
2. On failure: sleep `retry_backoff_ms × 2^attempt`, then attempt N+1.
3. After `retry_count` retries (i.e., `retry_count + 1` total attempts), drop the batch and add `records_in_batch` to `drop_counter`.
4. Log one `E_NOTICE` per dropped batch: `php-analyze: dropped <N> records from trace <uuid>: <url> <status_or_error> (attempt <K>)`.

#### Authentication / authorization

- Static bearer token from `auth_token` (or `auth_token_file`).
- Token-strength validation is the operator's responsibility (REQ A-2).
- Token is never written to any log or `phpinfo()` output.

### 5.3 Process-internal interface: channel between Recorder and Shipper

- `crossbeam_channel::bounded::<ShipperMessage>(config.shipper_queue_depth)`.
- Producer (Recorder): `try_send` — if `Err(Full)`, drop the batch newest-first, bump `drop_counter` by `batch.calls.len()`. Never `send` (blocking).
- Consumer (Shipper): `recv` (blocking) until `Drain` message; then process queued items until `deadline` or channel close.

---

## 6. Security Architecture

### 6.1 Authentication

- **To ingest server**: HTTP Bearer token. Static, configured server-side via `php.ini`. No rotation in v1 — operators rotate by updating the file and reloading the PHP process pool.

### 6.2 Authorization

- The extension has no PHP-side privileges to check. The server enforces token validity (REQ INT-3).

### 6.3 Data protection

| Aspect | Spec |
| --- | --- |
| TLS | All HTTPS traffic uses rustls with system CA roots (`rustls-native-certs`). Certificate validation is **mandatory**; `http://` URLs are accepted (for local stub) but logged as `E_WARNING` at `MINIT`. |
| Sensitive data in records | Per REQ NFR-SEC-3: records contain **only** metrics + identifiers; no function arguments, return values, or local variables. The Recorder MUST NOT call into the Zend execute-data argument array. |
| Token storage | `SecretString` (`secrecy` crate) wraps the token; `Debug` is redacted; `Display` is forbidden. The token enters memory once at `MINIT`. |
| Logging | The token MUST NOT appear in any log line. Tests grep all error-log output to confirm. |
| Memory zeroization | Best-effort: `SecretString` zeroizes on drop. Not a hard guarantee against memory dumps (out of scope). |

### 6.4 Threat model (brief)

| Threat | Mitigation |
| --- | --- |
| Untrusted PHP code reads the token | Token only in `Config`, in C-side memory; PHP has no `ini_get` access (PHP_INI_SYSTEM) for sensitive directives. Token field is excluded from `phpinfo()` (`PHP_INI_DISP_NO` equivalent). |
| MITM on the wire | TLS with cert validation. |
| Token exfiltration via process memory dump | Out of scope; users should rotate tokens regularly. |
| Malicious server response causing parser crash | Extension does not parse response bodies; only inspects status code. |
| Extension being used against an attacker-controlled `server_url` | Operator's responsibility; the extension is dev/staging-only. |

### 6.5 Compliance

V1 has no compliance certifications (GDPR/SOC/etc. are downstream concerns for the visualization layer, which is what would store and display traces). The extension itself ships no PII unless a PHP application puts PII into function names or file paths — which it should not.

---

## 7. Infrastructure and Deployment

### 7.1 Build toolchain

| Component | Requirement |
| --- | --- |
| Rust | Stable (pinned in `rust-toolchain.toml` once scaffolded). Currently target `1.78+`. |
| Cargo | Bundled with toolchain. |
| PHP dev headers | 8.3 and 8.4 (`php-dev` / `php8.3-dev` / `php8.4-dev` on Debian-family). |
| C compiler | Needed by `ext-php-rs` and `rustls` for build scripts (`clang` or `gcc`). |
| Linker | System default; no special flags expected. |

### 7.2 Cargo workspace layout

```
php-analyze/
├── Cargo.toml                 # workspace
├── crates/
│   ├── php-analyze/           # the extension (cdylib)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs         # Bootstrapper + ext-php-rs entry
│   │       ├── config.rs      # Configuration component
│   │       ├── recorder.rs    # Recorder component (hot path)
│   │       ├── shipper.rs     # Shipper component
│   │       ├── bootstrap.rs   # process-wide thread spawn/shutdown
│   │       ├── wire.rs        # serde structs for §4.2 wire format
│   │       └── clocks.rs      # clock_gettime / getrusage wrappers
│   └── stub-ingest/           # tiny test server (binary)
│       ├── Cargo.toml
│       └── src/main.rs
├── tests/                     # integration tests (load real PHP)
├── benches/                   # benchmark suite (Phase 5)
└── README.md
```

`php-analyze` is `crate-type = ["cdylib"]`. The build artifact is `target/release/libphp_analyze.so` (or `.so` for some target triples). Renamed/symlinked to `php_analyze.so` for installation.

### 7.3 Installation

1. Build: `cargo build --release -p php-analyze`.
2. Copy `target/release/libphp_analyze.so` to PHP's extension directory (`php -i | grep extension_dir`).
3. Add to `php.ini`:
   ```ini
   extension=php_analyze.so
   php_analyze.server_url = "https://ingest.example.com/v1/ingest"
   php_analyze.auth_token_file = "/etc/php-analyze/token"
   ```
4. Reload PHP-FPM or restart the CLI script.

A PECL package recipe is **SHOULD-not-MUST** (REQ R-8); if PECL packaging proves disproportionately costly for a Rust-built extension, source distribution is the contractually-required deliverable.

### 7.4 Runtime environment

| Aspect | Spec |
| --- | --- |
| OS | Linux x86_64 (kernel ≥ 4.4 for `CLOCK_MONOTONIC` semantics). |
| PHP | 8.3.0+ and 8.4.0+. |
| SAPIs | `cli`, `fpm-fcgi`. Other SAPIs may work but are unsupported. |
| Memory | Default 64 MiB extension buffer; add to PHP's `memory_limit` budget. |
| Network | Outbound TCP to `server_url`; TLS preferred. |
| Permissions | `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` and `getrusage(RUSAGE_THREAD)` are unprivileged. No CAP_SYS_* needed. |

### 7.5 Scaling

The extension scales **horizontally trivially**: each PHP process is independent, sharing only the configured `server_url`. The constraint is the ingest server, not the extension. Per-host scaling: 10–500 concurrent FPM workers per host all running the extension simultaneously is the design target (REQ §6.3).

Per-process memory: bounded by `buffer_cap_bytes` (default 64 MiB) + small overhead (~2 MiB for shipper thread stack, `ureq` agent, dictionary). For an FPM pool with `pm.max_children = 64`, worst-case extension memory across the pool is ~64 × 66 MiB ≈ 4 GiB. Operators should size hosts accordingly.

### 7.6 Monitoring and logging

| Signal | Where | Level |
| --- | --- | --- |
| `MINIT` misconfig | PHP error log (`E_WARNING`) | per process startup, once |
| HTTP failure (per batch) | PHP error log (`E_NOTICE`) | per dropped batch |
| `dropped_records` | embedded in every batch's `meta` | continuous, server-visible |
| Loaded directives | `phpinfo()` | on demand |

V1 does **not** expose internal metrics via a sidecar endpoint or shared memory. Observability of the extension itself is via the PHP error log and the data the ingest server receives (the cumulative `dropped_records` reveals shipper health).

---

## 8. Integration Points

### 8.1 PHP/Zend (consumed)

| Interface | Purpose | Source |
| --- | --- | --- |
| `zend_observer_fcall_register` | Begin/end function-call callbacks (user + internal) | Zend Engine 8.x |
| `MINIT` / `MSHUTDOWN` / `RINIT` / `RSHUTDOWN` / `MINFO` | Lifecycle hooks | Standard PHP ABI |
| `zend_memory_usage(true)` | Per-call memory snapshot | Zend Engine |
| `EG(exception)` | Detect exception unwind in end-handler | Zend Engine |
| `sapi_module.name` | SAPI identification | PHP SAPI layer |
| `SG(request_info).request_uri` (FPM) / `SG(argv)` (CLI) | `uri_or_script` field | PHP SAPI layer |
| `zend_ini_entry_def` (via ext-php-rs) | Directive registration | Zend INI subsystem |

### 8.2 Ingest server (consumed via HTTP)

Per REQ §10.1; see also §5.2 here for the request shape. The server is a separate deliverable. A **stub ingest server** ships with this project (workspace member `stub-ingest/`) for integration testing — see §10.1 Phase 0.

### 8.3 Future integration points (out of v1 scope, but architecturally accommodated)

- Sampled / production mode: would attach in the Recorder as a pre-instrument gate. No data-model changes expected.
- Upstream trace-ID ingestion: an additional optional `parent_trace` field in §4.2 `meta`. Schema bump to v2 if added.
- User-defined tags: a `tags: map<string, string>` field in `meta`. Schema bump to v2.

---

## 9. Testing Strategy

### 9.1 Unit tests (in-crate, `cargo test`)

| Target | Tests |
| --- | --- |
| `config.rs` | Range clamping; URL validation; precedence of `auth_token_file` over `auth_token`; missing-config → disabled. |
| `recorder.rs` (pure-Rust harness, no PHP) | Buffer threshold logic; depth overflow counting; drop-on-cap behavior; allocation count on hot path (zero-alloc assertion via `dhat` or `allocator-api2`). |
| `shipper.rs` (against an in-test HTTP mock) | Retry/backoff timing; success/4xx/5xx/timeout paths; persistent connection reuse; drain-with-deadline. |
| `wire.rs` | Round-trip `Batch ↔ MessagePack ↔ Batch`; field name stability; unknown-extra-key tolerance (forward-compat sanity). |
| `clocks.rs` | Monotonic guarantee under simulated clock skew (mocked). |

### 9.2 Integration tests (load real PHP)

A test harness using `phpt`-style scripts or a custom Rust test runner that:
- Spawns a fresh PHP CLI process with the freshly-built extension.
- Configures it to point at the in-test stub ingest server (listening on a random port).
- Runs scripted workloads (`tests/php/*.php`).
- Asserts on the batches the stub server received.

| Workload | Assertion |
| --- | --- |
| `flat_calls.php` (10⁴ calls to one function) | Exactly 10⁴ records; one dict entry; `dropped_records = 0`. |
| `nested.php` (parent/child structure) | `(call_id, parent)` reconstructs the expected tree. |
| `throws.php` (exception unwind) | Records on the throwing path have `abnormal_exit = true`. |
| `deep_recursion.php` (recurse 2000× with `max_depth = 100`) | Depth overflow accounted in `dropped_records`; no crash. |
| `slow_server.php` (stub returns 500) | Records eventually drop; `dropped_records` grows; PHP request finishes. |
| `unreachable_server.php` (stub returns nothing) | PHP request still completes; drops logged. |
| `cli_long_run.php` (10⁵ calls, multiple flushes) | Multiple batches received in order; all records present. |
| `fpm_repeated.php` (100 requests on one FPM worker) | No memory growth across requests; each trace has fresh `trace_id`. |

### 9.3 End-to-end smoke test

Manual procedure documented in README: install extension on a real Linux box, point at the stub ingest server running on localhost, run a small Symfony or WordPress workload, confirm batches received and `(host, pid, start_time, sapi, uri_or_script)` correlate as expected.

### 9.4 Performance / benchmark suite (Phase 5)

`criterion`-based benches comparing profiled vs. unprofiled wall time on a curated workload set (TBD per OQ-7, candidates from REQ §15.3). Pass criterion: geo-mean ≤ 2.0× unprofiled.

A separate micro-benchmark measures **per-call overhead** in nanoseconds for a tight loop of identical calls (a proxy for the hot-path tax).

### 9.5 Security tests

- `cargo audit` in CI.
- Token-leak test: grep all logs from the integration test suite for the configured token; MUST find zero hits.
- TLS test: stub server with a self-signed cert → connection MUST fail; with a system-trusted cert (test CA injected for the test) → connection MUST succeed.

### 9.6 CI gates

Per CLAUDE.md, every commit must pass:
- `cargo fmt -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all`

Plus, additionally for this project:
- `cargo audit` (warning-only initially).
- Integration tests against a stub PHP install (CI image with `php8.3-dev` and `php8.4-dev`).

---

## 10. Implementation Plan

Each phase below corresponds to **one OpenSpec change** on **one branch** (per CLAUDE.md). Phases are sequential — later phases assume earlier ones merged.

### Phase 0: Spike — `zend_observer` viability (retires R-2)

**Goal**: Confirm that `ext-php-rs` (or the chosen binding crate) gives us access to `zend_observer_fcall_register` AND that the observer end handler can detect internal-function calls reliably on PHP 8.3 and 8.4.

**Deliverables**:
- Throwaway branch (not merged) with the smallest possible extension that prints `entry: <fn>` and `exit: <fn>` for every call.
- A short `COMMENTS.md` note recording: which crate version, what works, what doesn't, any fallback needed.

**Acceptance**: Architect confirms zend_observer covers internal calls (or specifies a hybrid fallback if not).

**Effort**: 1–2 days.

### Phase 1: Project scaffolding

**Components**: §3.1 Bootstrapper (skeleton), §3.5 Configuration.

**Deliverables**:
- `cargo init` workspace per §7.2 layout.
- `crates/php-analyze/` builds as a cdylib; `phpinfo()` shows the loaded extension.
- All directives declared and read into `Config`; range clamping; missing-config → silent disable + `E_WARNING`.
- README skeleton with directive table.
- CI config (`fmt`, `clippy`, `test`).

**Acceptance criteria**: AC-BS-1, AC-BS-2 (without observer hooks active yet), AC-CF-1 through AC-CF-4.

**Dependencies**: Phase 0.

**Effort**: 2–3 days.

### Phase 2: Recorder (observer hooks + per-trace state)

**Components**: §3.2 Recorder; §4.1 in-memory types.

**Deliverables**:
- `zend_observer` begin/end handlers wired.
- `Trace` lifecycle at `RINIT`/`RSHUTDOWN`.
- Function dictionary interning.
- `CallRecord` emission in a `Vec<CallRecord>` buffer.
- Clock and memory snapshot in `clocks.rs`.
- Depth overflow → drop counter.
- **No HTTP yet**: the buffer simply grows and is discarded at `RSHUTDOWN`. Unit-test the buffer contents in-Rust.

**Acceptance criteria**: AC-RC-1, AC-RC-2, AC-RC-3, AC-RC-4, AC-RC-6.

**Dependencies**: Phase 1.

**Effort**: 5–7 days.

### Phase 3: Stub ingest server

**Components**: `crates/stub-ingest/`.

**Deliverables**:
- A tiny Rust binary (`tiny_http` or `axum`) that accepts POST, validates bearer token, decodes MessagePack, stores received batches in an in-memory `Vec`, exposes `GET /debug/batches` for tests to inspect.
- README section on running it.

**Acceptance criteria**: starts on a configurable port; decodes a hand-crafted batch and echoes its summary on `/debug/batches`.

**Dependencies**: §4.2 wire format (defined here).

**Effort**: 1–2 days.

### Phase 4: Shipper + transport

**Components**: §3.3 Shipper; §3.4 Process-wide bootstrap.

**Deliverables**:
- `ShipperMessage` channel; bounded `crossbeam` channel sized by `shipper_queue_depth`.
- Background thread spawn at first `RINIT` per process (guarded by `AtomicBool`).
- `rmp_serde` encoding of full batches including stamped `dropped_records`.
- `ureq` POST with keep-alive, configured headers, timeout, retry/backoff.
- Drop on retry exhaustion → drop counter bump + `E_NOTICE` log.
- Drain-on-`MSHUTDOWN` with deadline.

**Acceptance criteria**: AC-SH-1 through AC-SH-6; AC-PB-1, AC-PB-2; AC-BS-3, AC-BS-4.

**Dependencies**: Phases 2, 3.

**Effort**: 5–7 days.

### Phase 5: Hot-path tuning + benchmarks

**Components**: hot-path of §3.2 Recorder.

**Deliverables**:
- `criterion` bench suite under `benches/`.
- Canonical workload set (resolves OQ-7).
- Hot-path zero-alloc assertion (`AC-RC-5`).
- Geo-mean ≤ 2.0× overhead verified across the canonical set.

**Acceptance criteria**: AC-RC-5; overall NFR-PERF-1 met.

**Dependencies**: Phase 4 (full E2E pipeline working).

**Effort**: 5–10 days (open-ended; tuning iterations).

### Phase 6: Hardening, docs, packaging

**Deliverables**:
- Full README with every directive (default/range/effect).
- Token-leak grep test.
- TLS / system-CA integration test.
- `cargo audit` integrated.
- Source-distribution tarball recipe; PECL packaging attempt (SHOULD per REQ R-8).
- Final acceptance against REQ §15.1 1–8.

**Acceptance criteria**: REQ §15.1 fully satisfied.

**Dependencies**: Phase 5.

**Effort**: 3–5 days.

### Total estimate

V1 ≈ 4–6 weeks of focused work, dominated by Phase 5 (tuning) and Phase 4 (transport robustness). All numeric estimates are guidance for sequencing only.

---

## 11. Risks and Mitigations

Pulls forward REQ §14 with architect-level updates:

| ID | Risk | L | I | Mitigation | Status |
| --- | --- | --- | --- | --- | --- |
| R-1 | Hot-path overhead > 2× target | M | H | Phase 5 dedicated to tuning; AC-RC-5 zero-alloc guarantee; fallback target 5× if budget cannot be hit. Re-evaluate hook strategy if observer overhead dominates. | Open |
| R-2 | `zend_observer` doesn't cover internal calls | M | M | **Phase 0 spike** retires this risk before any other work commits. | Closed for PHP 8.3 and PHP 8.4. See `COMMENTS.md` C-5 (Phase 0) and C-7 (slice-2 PHP-8.3 verification). |
| R-3 | Huge batches exceed HTTP timeouts | M | M | `flush_bytes` default 1 MiB caps per-batch payload; timeouts independent of batch count. | Mitigated by design |
| R-4 | Drop policy silently loses data | L | M | `dropped_records` in every `meta`; visualization layer obligated to surface. | Mitigated by design |
| R-5 | Generators / fibers produce surprising results | M | M | Documented limitation L-1/L-2; follow-up change. | Accepted |
| R-6 | No cross-service correlation | L | L | Documented; future-work field reserved in §8.3. | Accepted |
| R-7 | Rust + PHP extension build complexity underestimated | M | H | Phase 0 spike; small phase sizes; CI runs full build on every change. | Active monitoring |
| R-8 | PECL packaging for Rust extension non-standard | M | L | Source dist is the MUST; PECL is best-effort in Phase 6. | Accepted |
| R-9 | Server outage during long CLI causes silent loss | M | L | `dropped_records` exposes; v1 explicitly excludes spooling. | Accepted |
| **R-10** (new) | **POSIX `fork()` + background thread surprise**: shipper thread spawned in master would not exist in FPM worker | L | H | Lazy spawn at first `RINIT` per process, guarded by `AtomicBool` (AD-4). Integration test `fpm_repeated.php` verifies each worker has its own thread. | Mitigated by design |
| **R-11** (new) | **`getrusage` granularity** is coarser than `t_in`/`t_out` resolution → CPU times look quantized | L | L | Documented in README; for sub-microsecond functions, `cpu_u`/`cpu_s` may be `0`. Under `php_analyze.cpu_snapshot_mode = off` (`recorder-cpu-snapshot-cadence`) the `getrusage` call is skipped entirely and `cpu_u_ns / cpu_s_ns` are forced to `0` regardless of function duration. Acceptable for staging-level analysis. | Accepted |
| **R-12** (new) | **`MSHUTDOWN` hang** if shipper is mid-HTTP and server unresponsive | L | M | `shutdown_grace_ms` deadline; shipper self-aborts pending HTTP at deadline; remaining batches counted as dropped. AC-BS-4 verifies. | Mitigated by design |
| **R-13** (new) | **Channel-full** under burst load → recorder drops at the channel boundary rather than the buffer boundary, hiding why | L | L | Both drop paths increment the same `drop_counter`; the `E_NOTICE` distinguishes "buffer cap" vs "channel full" in its message. | Mitigated by design |

---

## 12. Appendices

### 12.1 Glossary

Inherits REQ §17.1; new terms added by this document:

| Term | Meaning |
| --- | --- |
| **Recorder** | The §3.2 component running on the PHP request thread that captures call records. |
| **Shipper** | The §3.3 background thread that encodes and POSTs batches. |
| **`PendingBatch`** | The §4.1.6 cross-thread payload moved from Recorder to Shipper. |
| **Hot path** | The execution path inside `zend_observer` begin/end handlers; subject to NFR-PERF-1 budget. |
| **Drain** | The `MSHUTDOWN` procedure that flushes the shipper queue under a bounded deadline. |
| **Lazy spawn** | The fork-safe pattern of creating the shipper thread on first `RINIT` rather than at `MINIT`. |

### 12.2 Traceability: Requirements ↔ Specification

| REQ ID | Addressed in |
| --- | --- |
| F-LC-1 … F-LC-5 | §3.1 Bootstrapper |
| F-AC-1 … F-AC-3 | §3.5 Configuration; §3.2 Recorder (observer skip when `enabled=0`) |
| F-IN-1 … F-IN-7 | §3.2 Recorder |
| F-FN-1 … F-FN-4 | §3.2 Recorder (dictionary section) |
| F-MD-1 … F-MD-2 | §4.1.2 `Trace`; §4.2.1 `meta` map |
| F-BF-1 … F-BF-4 | §3.2 Recorder threshold logic; §3.3 Shipper consumption |
| F-OV-1 … F-OV-4 | §3.2 cap enforcement; §3.4 channel-full handling |
| F-TR-1 … F-TR-7 | §3.3 Shipper; §5.2 |
| F-FH-1 … F-FH-6 | §3.3 Shipper retry block |
| L-1 … L-6 | §2.5; documented as accepted limitations |
| NFR-PERF-1 … NFR-PERF-3 | §10 Phase 5; §3.3 retry timing |
| NFR-SEC-1 … NFR-SEC-4 | §6 |
| NFR-REL-1 … NFR-REL-3 | §3.1 Bootstrapper; §3.4 process bootstrap; integration tests |
| NFR-MAINT-1 … NFR-MAINT-3 | §3.5 README; §4.2 versioned media type; CLAUDE.md style |
| NFR-USE-1 … NFR-USE-2 | §3.5; §3.1 silent-disable path |
| §9 data | §4 |
| §10 integrations | §5.2; §8 |
| §11 constraints | §7; §10 |
| OQ-1 … OQ-10 | §1.4 resolutions |

### 12.3 Configuration directives — quick reference

(Authoritative copy lives in §3.5; reproduced here for the README handoff.)

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

;php_analyze.cpu_snapshot_mode   = per-call           ; or "off" — see §3.2 / C-19
```

### 12.4 References

- `REQUIREMENTS.md` v0.1 (this repo).
- `CLAUDE.md` (project workflow & style).
- `personas/SOLUTION_ARCHITECT.md` (this persona's contract).
- `personas/RUST_DEVELOPER.md` (downstream consumer).
- PHP internals — Zend Observer API: <https://wiki.php.net/rfc/zend_observer_api>.
- ext-php-rs: <https://github.com/davidcole1340/ext-php-rs>.
- MessagePack spec: <https://msgpack.org/>.
- rustls + native CA roots: <https://docs.rs/rustls-native-certs>.

### 12.5 Change history

| Version | Date | Author | Notes |
| --- | --- | --- | --- |
| 0.1 | 2026-05-20 | Solution Architect | Initial draft, derived from `REQUIREMENTS.md` v0.1, ratified Phase-2 decisions AD-1…AD-5. |

---

*End of document. Hand off to Rust Developer for OpenSpec-driven implementation. Begin with Phase 0 (zend_observer spike).*
