# Requirements Document: `php-analyze` — PHP Profiling Extension

| Field | Value |
| --- | --- |
| Version | 0.1 (draft) |
| Date | 2026-05-20 |
| Author | Requirements Analyst (with Daniel Persson, daniel.persson@textalk.se) |
| Scope of this document | **The PHP extension only.** The ingest server and visualization layers are referenced as consumers/constraints but specified separately. |
| Audience | Solution Architect (next in the pipeline), Rust Developer |

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Business Context](#2-business-context)
3. [Goals and Objectives](#3-goals-and-objectives)
4. [Scope](#4-scope)
5. [Stakeholders](#5-stakeholders)
6. [User Personas / Actors](#6-user-personas--actors)
7. [Functional Requirements](#7-functional-requirements)
8. [Non-Functional Requirements](#8-non-functional-requirements)
9. [Data Requirements](#9-data-requirements)
10. [Integration Requirements](#10-integration-requirements)
11. [Constraints](#11-constraints)
12. [Assumptions](#12-assumptions)
13. [Dependencies](#13-dependencies)
14. [Risks](#14-risks)
15. [Success Criteria](#15-success-criteria)
16. [Open Questions](#16-open-questions)
17. [Appendices](#17-appendices)

---

## 1. Executive Summary

`php-analyze` is a PHP extension that records per-function-call performance metrics (wall time, CPU time, memory delta) for PHP code running under CLI or PHP-FPM, buffers them in process memory, and ships them in MessagePack batches over HTTP to an ingest server. The data is later visualized as two drillable tree views (time and memory) showing each function's percentage share of its parent.

The extension is the first deliverable in a three-part system:

1. **`php-analyze` extension** *(this document)* — captures and ships data.
2. **Ingest server** *(future document)* — accepts, validates, and stores batches.
3. **Visualization / report builder** *(future document)* — renders the two tree views.

**Primary differentiator**: existing tools (Xdebug, hxprof, commercial profilers) produce data, but their analysis UI makes it hard to find the actual bottleneck. This project's design is biased toward capturing rich, well-structured, per-call data so the visualization layer can deliver a superior "where is the time/memory going?" experience.

**Initial deployment scope**: development and staging environments only. The extension is **not** designed to be production-safe in v1.

---

## 2. Business Context

### Background

PHP applications, especially those running under PHP-FPM, frequently exhibit performance problems whose root causes are difficult to isolate. The tools in common use today have characteristic failures:

- **Xdebug** — high overhead, primarily a debugger; its profiler output (cachegrind) is hard to navigate.
- **hxprof (Meta)** — narrow use case, complex setup, weak general-purpose UI.
- **Commercial APMs** — expensive, vendor lock-in, often coarse-grained call data.

The common gap: **none of them make it easy to look at a trace and immediately see where time or memory actually went.**

### Current state vs. desired state

| | Current | Desired |
| --- | --- | --- |
| Capturing data | Possible via Xdebug/hxprof but high friction | One-line php.ini setup; loaded = profiling on |
| Multi-process data | Fragmented per file/per-process | Correlated by `(host, pid, start_time, sapi, uri)` on the server |
| Finding the bottleneck | Manual cachegrind reading or paid SaaS | Drillable tree, root = 100%, descend by percentage |
| Cost | Free-but-painful OR paid-and-locked-in | Open source, self-hostable |

---

## 3. Goals and Objectives

### Business goals

- **G1.** Provide a free, self-hostable PHP profiler whose primary value is the quality of its analysis UI (rather than overhead, coverage, or feature count).
- **G2.** Be safe and pleasant to use across many PHP processes in dev/staging environments.
- **G3.** Lay a foundation that a future "production-safe" mode could extend, without re-architecting v1.

### Engineering objectives for the extension

- **O1.** Capture per-call wall time, CPU time, and memory delta for every PHP user function and internal function call.
- **O2.** Identify and segregate data from many concurrent PHP processes (CLI runs and FPM workers) without external coordination.
- **O3.** Ship data efficiently enough that a typical staging request remains usable (≤ 2× unprofiled wall time).
- **O4.** Never block, crash, or destabilize the PHP process being profiled, even on server outage.

### Measurable success criteria (KPIs)

| KPI | Target |
| --- | --- |
| Overhead on a representative PHP-FPM request (geometric mean of N=10 workloads) | ≤ 2.0× unprofiled wall time |
| Per-call records correctly emitted (sample workload, vs. ground truth Xdebug trace) | ≥ 99.5% of expected calls present |
| Data loss on a healthy server | 0 records lost from a completed trace |
| Data loss on a slow/unreachable server | PHP request never blocks; dropped records reported via drop counter |
| Time to first useful trace (fresh install) | ≤ 15 minutes (install extension, point at server, run script) |

---

## 4. Scope

### 4.1 In scope (v1)

- PHP extension implementing the runtime instrumentation, buffering, batching, and HTTP transport described in [§7](#7-functional-requirements).
- Support for PHP 8.3 and PHP 8.4, on Linux x86_64.
- Support for both CLI and PHP-FPM SAPIs.
- Capture of wall time, CPU time (user + sys), and PHP memory delta per call.
- Function dictionary with per-trace interning.
- MessagePack-encoded batches transmitted via HTTP POST with bearer-token auth.
- Configuration via `php.ini` directives.
- Correct behavior on exceptions / abnormal function exits.
- Configurable maximum tracked call depth (with overflow counting).
- Source distribution and PECL package.
- MIT license.

### 4.2 Out of scope (v1)

- The ingest server itself (covered by a separate requirements doc).
- The visualization / report builder (covered by a separate requirements doc).
- Production-safe operation (sub-percentage overhead, sampling, etc.).
- Windows, macOS, FreeBSD, or non-x86_64 Linux builds.
- PHP 8.2 and earlier; PHP 7.x.
- Allocation count / allocation byte tracking.
- Function-argument or return-value capture.
- Cross-service / distributed trace correlation (no upstream trace ID propagation, no user-defined tags).
- Environment variable / runtime `ini_set()` configuration (planned future enhancement; see §4.3).
- Mutual TLS or any auth scheme beyond static bearer token.
- Correct handling of generators, fibers, `eval()`-defined functions, and destructors fired after final flush (see §7.10 *Known limitations*).
- On-disk spooling of failed batches.
- Backpressure to the PHP process (the buffer overflow policy is drop-newest; see §7.7).

### 4.3 Future considerations (not committed to v1)

- Production-safe mode with sampling and reduced overhead.
- Environment-variable configuration (`PHP_ANALYZE_*`) and runtime `ini_set()` for non-system directives.
- Generators / fibers as first-class concepts.
- Upstream distributed-trace ID ingestion.
- User-defined tags (service, env, version) embedded in trace metadata.
- Cross-platform builds (Linux ARM64, macOS dev builds).
- Older PHP versions (8.0–8.2).
- Allocation counts / bytes for finer-grained memory analysis.

---

## 5. Stakeholders

| Stakeholder | Role | Concerns |
| --- | --- | --- |
| **Daniel Persson** | Project sponsor, primary user | Differentiated UX; safe to run in staging; doesn't destabilize PHP |
| **Solution Architect** *(next persona)* | Translates this doc into `SPECIFICATION.md` | Adequate detail to make design decisions; clear constraints; explicit unknowns |
| **Rust Developer** *(third persona)* | Implements per OpenSpec changes | Testable requirements; concrete numeric thresholds; bounded edge cases |
| **Future PHP application developers** | End users running the extension on their codebases | Easy install, predictable behavior, useful data on the other end |

Decision-making authority: **Daniel Persson** is the sole decision-maker for product scope, defaults, and trade-offs.

---

## 6. User Personas / Actors

### 6.1 Human users

**"Staging Operator" Sam** — a PHP developer or SRE who installs `php-analyze`, points it at a staging ingest server, runs a workload (manual click-through, load test, replayed traffic), then opens the visualization to find the slow function. Sam is technically proficient with PHP, comfortable with `php.ini`, and has shell access to the staging box.

**"CLI Investigator" Charlie** — same audience, but profiling a long-running CLI job (cron, queue worker, batch import) rather than HTTP requests.

### 6.2 System actors

- **PHP runtime (Zend Engine)** — invokes the extension on every function entry and exit.
- **Ingest server** — receives HTTP POSTs, returns success/failure.
- **`php.ini`** — supplies configuration at process startup.

### 6.3 Volume expectations

| Dimension | Expected order of magnitude |
| --- | --- |
| Concurrent PHP processes per host | 10–500 (typical FPM pool) |
| Function calls per request (typical web app) | 10⁴–10⁶ |
| Function calls per long CLI run | up to 10⁸ |
| Hosts shipping to one ingest server | 1–10 in v1 |

---

## 7. Functional Requirements

Each requirement is tagged **MUST**, **SHOULD**, or **MAY** (RFC 2119) and assigned an ID for traceability.

### 7.1 Lifecycle

- **F-LC-1 (MUST)** The extension MUST initialize per-process state at module startup (`MINIT`).
- **F-LC-2 (MUST)** The extension MUST reset per-trace state at request startup (`RINIT`): new trace UUID, empty call stack, empty function dictionary, `call_id = 0`, empty buffer, zeroed drop counter.
- **F-LC-3 (MUST)** The extension MUST treat **one trace as exactly one request** (FPM) or **one CLI invocation** (CLI).
- **F-LC-4 (MUST)** The extension MUST perform a final flush of any non-empty buffer at request shutdown (`RSHUTDOWN`), regardless of whether the threshold was hit.
- **F-LC-5 (MUST)** The extension MUST release per-trace state at `RSHUTDOWN` so that the next request on a long-lived FPM worker starts clean.

### 7.2 Activation

- **F-AC-1 (MUST)** When the extension is loaded, profiling MUST be active by default for every request/invocation.
- **F-AC-2 (MUST)** A `php.ini` directive `php_analyze.enabled` (default `1`) MUST allow disabling all instrumentation at process startup.
- **F-AC-3 (SHOULD)** When `php_analyze.enabled = 0`, the extension SHOULD impose zero per-call cost (no instrumentation hooks active).

### 7.3 Instrumentation

- **F-IN-1 (MUST)** On every function entry, the extension MUST record: wall-clock timestamp (nanosecond resolution, monotonic), CPU user time, CPU system time, current PHP memory usage.
- **F-IN-2 (MUST)** Instrumentation MUST cover **all PHP user-defined functions, methods, and closures**.
- **F-IN-3 (MUST)** Instrumentation MUST cover **all internal (C-implemented) function calls** invoked from PHP code (e.g., `array_map`, `preg_match`, `PDO::query`).
- **F-IN-4 (MUST)** On function exit, the extension MUST emit exactly one record per call (see §9.1 for record shape).
- **F-IN-5 (MUST)** The extension MUST correctly emit an exit record when a function exits **via thrown exception**, and MUST flag such records with an `abnormal_exit = true` field.
- **F-IN-6 (MUST)** The extension MUST enforce a configurable maximum tracked call depth, default `1024`, configurable via `php_analyze.max_depth`.
- **F-IN-7 (MUST)** Calls beyond `max_depth` MUST NOT produce per-call records but MUST be counted; the count MUST be included in the trace's drop counter.

### 7.4 Function identification and dictionary

- **F-FN-1 (MUST)** The first time a function is seen in a given trace, it MUST be assigned a small monotonic integer `fn_id` (starting at 1).
- **F-FN-2 (MUST)** A dictionary entry MUST be emitted (bundled into the next batch) the first time each `fn_id` appears, containing: `fn_id`, fully qualified name, declaring file, declaring line, kind (`function` / `method` / `closure` / `internal`).
- **F-FN-3 (MUST)** Subsequent call records reference the function by `fn_id` only — never by name.
- **F-FN-4 (MUST)** `fn_id`s are scoped to a single trace; they MUST NOT be reused or persisted across traces.

### 7.5 Per-trace metadata

- **F-MD-1 (MUST)** The extension MUST capture, at `RINIT`, the following metadata for the trace:
  - `trace_id` — UUID generated at trace start (proposed: UUID v7 for sortability; final choice open, see §16)
  - `host` — system hostname
  - `pid` — process ID
  - `start_time` — wall-clock timestamp at `RINIT` (nanoseconds since epoch)
  - `sapi` — `cli` / `fpm-fcgi` / `cgi-fcgi` / etc., as reported by PHP
  - `uri_or_script` — request URI (FPM) or invoked script path (CLI)
- **F-MD-2 (MUST)** Per-trace metadata MUST be embedded as a header on **every** outgoing batch (not only the first), so that the server can process batches that arrive out of order or without a "begin" message.

### 7.6 Buffering and batching

- **F-BF-1 (MUST)** Call records, dictionary entries, and metadata MUST be buffered in process-local memory between flushes.
- **F-BF-2 (MUST)** A flush MUST be triggered whenever **either** of these thresholds is reached, whichever comes first:
  - `php_analyze.flush_records` — buffered record count (default: `10000`, configurable)
  - `php_analyze.flush_bytes` — buffered byte size (default: `1048576` / 1 MiB, configurable)
- **F-BF-3 (MUST)** A flush MUST also be triggered at `RSHUTDOWN` if the buffer is non-empty (final flush; see F-LC-4).
- **F-BF-4 (MUST)** Each flush MUST emit exactly one batch containing: metadata header, any new dictionary entries since the last flush, and the buffered call records in emission order.

### 7.7 Buffer overflow

- **F-OV-1 (MUST)** The total in-memory buffer size MUST be capped by `php_analyze.buffer_cap_bytes` (default: `67108864` / 64 MiB).
- **F-OV-2 (MUST)** When the cap is reached and a flush has not completed, new call records and dictionary entries MUST be **dropped (newest-first)** and a per-trace `dropped_records` counter MUST be incremented.
- **F-OV-3 (MUST)** The `dropped_records` counter MUST be included in every outgoing batch so the server (and ultimately the UI) can surface "N records lost from this trace".
- **F-OV-4 (MUST)** The extension MUST NEVER block PHP execution to wait for buffer drainage.

### 7.8 Transport

- **F-TR-1 (MUST)** Batches MUST be sent as HTTP POST to a URL configured via `php_analyze.server_url`.
- **F-TR-2 (MUST)** The request body MUST be MessagePack-encoded per the schema in §9.
- **F-TR-3 (MUST)** The request MUST include header `Authorization: Bearer <token>` where the token is set via `php_analyze.auth_token`.
- **F-TR-4 (MUST)** The request MUST include header `Content-Type: application/vnd.php-analyze.v1+msgpack` (proposed; final media type subject to §16).
- **F-TR-5 (MUST)** A response with HTTP status `2xx` MUST be treated as successful ingestion.
- **F-TR-6 (MUST)** Any other status, network error, or timeout MUST be treated as failure and trigger the retry policy in §7.9.
- **F-TR-7 (SHOULD)** The HTTP client SHOULD reuse a persistent connection across flushes within a trace where practical.

### 7.9 Failure handling

- **F-FH-1 (MUST)** On flush failure, the extension MUST retry the same batch up to `php_analyze.retry_count` times (default: `3`).
- **F-FH-2 (MUST)** Retries MUST use exponential backoff with base `php_analyze.retry_backoff_ms` (default: `100`), doubling per attempt (100, 200, 400 ms with defaults).
- **F-FH-3 (MUST)** Each HTTP attempt MUST observe `php_analyze.http_timeout_ms` (default: `2000`).
- **F-FH-4 (MUST)** After all retries are exhausted, the batch MUST be dropped and the records counted toward the `dropped_records` counter described in §7.7.
- **F-FH-5 (MUST)** Retry attempts MUST NOT block PHP request processing beyond the cumulative timeout described above; if a retry would extend past `RSHUTDOWN`, the extension MAY abandon retries early.
- **F-FH-6 (SHOULD)** Failures SHOULD be logged to the PHP error log at `E_NOTICE` level with enough detail to diagnose (URL, status, attempt number).

### 7.10 Known limitations / explicit non-behaviors (v1)

These are **not bugs in v1** — they are explicitly documented as outside the v1 contract:

- **L-1** Generators (`yield` / resume) are treated as ordinary functions. Time spent "suspended" between a `yield` and the next `send`/`next` will be attributed to the generator. **No correctness guarantee.**
- **L-2** Fibers (PHP 8.1+) are treated as ordinary functions. Time spent suspended (`Fiber::suspend`) is attributed to the fiber. **No correctness guarantee.**
- **L-3** Functions defined inside `eval()` have synthetic file/line information; their dictionary entries will reflect whatever PHP reports.
- **L-4** Destructors fired during `RSHUTDOWN`, **after** the final flush, are not captured.
- **L-5** Memory delta is `mem_exit - mem_enter`. Transient allocations freed within the call are invisible.
- **L-6** No cross-service correlation: there is no upstream trace-ID propagation and no user-defined tags. Correlation in v1 is limited to `(host, pid, start_time, sapi, uri_or_script)`.

---

## 8. Non-Functional Requirements

### 8.1 Performance

| ID | Requirement |
| --- | --- |
| NFR-PERF-1 | Profiled wall-clock time for a representative request MUST NOT exceed **2.0×** the unprofiled wall-clock time (geometric mean across the canonical benchmark set defined in §15). |
| NFR-PERF-2 | Per-call instrumentation overhead SHOULD be dominated by syscalls/clock reads, not by allocation. The hot path SHOULD avoid heap allocation. |
| NFR-PERF-3 | A flush MUST NOT pause PHP execution for more than `http_timeout_ms × (retry_count + 1)` in the worst case. |

### 8.2 Security

| ID | Requirement |
| --- | --- |
| NFR-SEC-1 | All HTTP traffic to the ingest server MUST support TLS (`https://` URLs). The extension MUST validate server certificates against the system trust store. |
| NFR-SEC-2 | The bearer token MUST be configurable only via `php.ini` (server-side configuration), never via PHP user code, to limit exfiltration risk. |
| NFR-SEC-3 | Captured data MUST NOT include function arguments, return values, or local variables — only the metrics and identifiers specified in §9. |
| NFR-SEC-4 | The extension MUST NOT log the bearer token, even at debug levels. |

### 8.3 Availability and reliability

| ID | Requirement |
| --- | --- |
| NFR-REL-1 | The extension MUST NOT crash, abort, or panic the PHP process on any failure path. Errors are logged and absorbed. |
| NFR-REL-2 | Server unavailability MUST degrade gracefully: the PHP request continues to completion; data is dropped per §7.7–§7.9. |
| NFR-REL-3 | The extension MUST NOT leak memory across requests on a long-lived FPM worker. |

### 8.4 Maintainability

| ID | Requirement |
| --- | --- |
| NFR-MAINT-1 | All configuration directives MUST be documented in the package README with default, range, and effect. |
| NFR-MAINT-2 | The wire format media type (`application/vnd.php-analyze.v1+msgpack`) MUST encode the major version so future incompatible changes can ship under a new media type. |
| NFR-MAINT-3 | Source code MUST conform to the project's Rust style guidelines (per `CLAUDE.md`): named-for-meaning, small functions, types over conventions, tests as documentation. |

### 8.5 Usability (operator-facing)

| ID | Requirement |
| --- | --- |
| NFR-USE-1 | Installing the extension and pointing it at a server MUST require editing only `php.ini` (no application code changes). |
| NFR-USE-2 | A misconfiguration (e.g., missing `server_url` or `auth_token`) MUST cause profiling to be silently disabled with a single startup warning to the PHP error log, not a crash. |

---

## 9. Data Requirements

### 9.1 Per-call record

Each function call produces exactly one record at exit:

| Field | Type | Notes |
| --- | --- | --- |
| `call_id` | uint64 | Monotonic, per-trace, starts at 1. |
| `parent` | uint64 | `call_id` of the parent call, or `0` if this is the trace root. |
| `fn` | uint32 | Interned function id (see §7.4). |
| `depth` | uint16 | Call-stack depth (0 = root). |
| `t_in` | int64 | Wall-clock entry timestamp, nanoseconds, monotonic per trace. |
| `t_out` | int64 | Wall-clock exit timestamp, nanoseconds. |
| `cpu_u` | int64 | User CPU time at exit minus at entry, nanoseconds. |
| `cpu_s` | int64 | System CPU time at exit minus at entry, nanoseconds. |
| `mem_in` | int64 | PHP memory usage at entry, bytes. |
| `mem_out` | int64 | PHP memory usage at exit, bytes. |
| `abnormal_exit` | bool | `true` iff the function unwound via exception. |

### 9.2 Function dictionary entry

Emitted at most once per `fn_id` per trace:

| Field | Type | Notes |
| --- | --- | --- |
| `fn_id` | uint32 | Matches `fn` in call records. |
| `fqn` | string | Fully qualified name (e.g., `App\\Billing\\Invoice::recalculate`, `array_map`, `Closure@/path/file.php:123`). |
| `file` | string | Declaring file path (absolute when known; synthetic for `eval`/internal). |
| `line` | uint32 | Declaring line (`0` if not applicable, e.g., internal functions). |
| `kind` | enum | `function` / `method` / `closure` / `internal`. |

### 9.3 Trace metadata header

Embedded in every batch:

| Field | Type | Notes |
| --- | --- | --- |
| `schema_version` | uint8 | Currently `1`. |
| `trace_id` | string (UUID) | See §7.5. |
| `host` | string | Hostname. |
| `pid` | uint32 | Process ID. |
| `start_time` | int64 | Trace start, nanoseconds since UNIX epoch. |
| `sapi` | string | PHP SAPI name. |
| `uri_or_script` | string | Request URI or CLI script path. |
| `dropped_records` | uint64 | Cumulative across the trace at the time this batch is sent. |

### 9.4 Batch envelope

A batch is a MessagePack-encoded map with fields:

| Field | Type | Notes |
| --- | --- | --- |
| `meta` | map | The §9.3 metadata header. |
| `dict` | array | Zero or more §9.2 dictionary entries (new since last batch). |
| `calls` | array | Zero or more §9.1 call records, in emission order. |

### 9.5 Data volumes

| Scenario | Records per trace | Approx. batch payload (after MessagePack) |
| --- | --- | --- |
| Small FPM request | 10³–10⁴ | < 100 KiB total |
| Typical FPM request | 10⁵ | ~5–10 MiB total across ~10 batches |
| Heavy CLI run | 10⁷ | hundreds of MiB across many batches |

### 9.6 Data retention

Data retention is the server's concern, not the extension's. The extension neither persists data locally nor caches it beyond the in-memory buffer.

---

## 10. Integration Requirements

### 10.1 Ingest server contract (consumer constraints)

The extension assumes the following from the ingest server. These shape the extension's design; the server itself is specified in a separate document.

| ID | Requirement |
| --- | --- |
| INT-1 | Server MUST accept HTTP POST at the URL configured via `php_analyze.server_url` (full URL including path). |
| INT-2 | Server MUST accept `Content-Type: application/vnd.php-analyze.v1+msgpack`. |
| INT-3 | Server MUST validate `Authorization: Bearer <token>` and respond `401 Unauthorized` on failure. |
| INT-4 | Server MUST respond `2xx` only after the batch is durably accepted (in whatever sense the server defines durability). Anything else MUST be treated by the extension as failure. |
| INT-5 | Server MUST tolerate batches from a single trace arriving in any order, including in parallel. (The extension flushes from a single thread per process, but multiple processes may share a `trace_id` if a misconfigured deployment recycles UUIDs — defensive constraint.) |
| INT-6 | Server SHOULD respond quickly (target: median < 50 ms) so flushes don't dominate PHP request latency. |

### 10.2 PHP runtime integration

| ID | Requirement |
| --- | --- |
| INT-7 | Extension MUST integrate via the standard PHP/Zend extension ABI for PHP 8.3 and 8.4. The specific interception mechanism (e.g., `zend_observer` API vs. `zend_execute_ex`/`zend_execute_internal` overrides) is left to the Solution Architect. |
| INT-8 | Extension MUST be loadable via `extension=` (or `zend_extension=`, per architectural choice) in `php.ini`. |

---

## 11. Constraints

### 11.1 Technical constraints

- **C-T-1** Target platform: Linux x86_64 only.
- **C-T-2** Target PHP versions: 8.3 and 8.4 only.
- **C-T-3** Implementation language: **Rust** (per project conventions in `CLAUDE.md`). Choice of PHP/Zend binding crate (`ext-php-rs` or equivalent) is left to the Solution Architect.
- **C-T-4** Build toolchain MUST be standard `cargo` plus whatever PHP development headers are required.
- **C-T-5** Wire format: MessagePack.
- **C-T-6** Transport: HTTP (1.1 minimum; HTTP/2 acceptable). TLS supported.

### 11.2 Process constraints

- **C-P-1** All implementation work proceeds via OpenSpec changes (per `CLAUDE.md`), one branch per change, no commits to `main`.
- **C-P-2** Changes MUST be small (few hundred lines diff max).
- **C-P-3** `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test` MUST all pass before any commit.
- **C-P-4** Code style follows the Rust guidelines in `CLAUDE.md`: meaningful names, small functions, types-over-conventions, comment the *why*, tests as documentation.

### 11.3 Business / scope constraints

- **C-B-1** Dev/staging only in v1. Production safety is **future work**.
- **C-B-2** Single decision-maker (Daniel Persson) for product trade-offs.

---

## 12. Assumptions

| ID | Assumption |
| --- | --- |
| A-1 | The ingest server will be deployed on the same private network as the PHP hosts, or otherwise reachable with low latency (< 50 ms typical RTT). |
| A-2 | Operators will configure a long, secret bearer token; the extension does not enforce token strength. |
| A-3 | The system clock on each PHP host is monotonic within a trace (we use a monotonic clock for `t_in`/`t_out`) and approximately correct for `start_time` (used for correlation, not arithmetic). |
| A-4 | PHP processes have permission to read CPU time from the OS (`clock_gettime(CLOCK_PROCESS_CPUTIME_ID, ...)` or equivalent). |
| A-5 | A typical staging workload produces traces that fit within the default 64 MiB buffer cap. Heavier traces will see `dropped_records` > 0, which the operator can address by raising the cap or tightening flush thresholds. |
| A-6 | The Solution Architect will choose between the `zend_observer` API and lower-level VM hook strategies; both are presumed available in PHP 8.3/8.4. |
| A-7 | The visualization layer will rebuild the call tree from `(call_id, parent)` links; the extension is not responsible for tree construction. |

---

## 13. Dependencies

### 13.1 Build/runtime dependencies

- **Rust toolchain** (stable, version pinned in the Cargo workspace once scaffolded).
- **PHP 8.3 / 8.4 development headers** for the target builds.
- A MessagePack encoder (Rust crate; specific choice left to the Architect).
- An HTTP client library supporting TLS, persistent connections, and timeouts (Rust crate; specific choice left to the Architect).
- A Zend binding layer (`ext-php-rs` or chosen alternative).

### 13.2 External dependencies (organizational / pipeline)

- **Ingest server** must exist (even as a stub) to validate the extension end-to-end. Without it, only unit tests are possible.
- **Visualization layer** is not a build dependency but is the eventual consumer; the data shape in §9 must not break before the visualization layer reads from the server.

### 13.3 Prerequisites

- Rust project scaffolding (`cargo init`) — currently not done; see `CLAUDE.md`.
- CI configuration to run `fmt` / `clippy` / `test` on every change.

---

## 14. Risks

| ID | Risk | Likelihood | Impact | Mitigation |
| --- | --- | --- | --- | --- |
| R-1 | Per-call hook overhead exceeds 2× target on realistic workloads | Medium | High | Define benchmark suite early; budget Architect time for hot-path tuning. Accept fallback target (5×) if necessary. |
| R-2 | `zend_observer` API does not provide all needed hooks for internal-function calls | Medium | Medium | Architect to confirm during design; fallback to `zend_execute_internal` override. |
| R-3 | MessagePack payload size for huge traces overwhelms HTTP timeouts | Medium | Medium | Flush thresholds bound batch size; per-batch byte cap is tunable. |
| R-4 | Buffer overflow drop policy hides real performance problems behind "data missing" | Low | Medium | `dropped_records` surfaced in every batch; UI should display prominently. Defaults sized for typical traces. |
| R-5 | Generators/fibers not handled correctly leads to surprising results for modern frameworks (Symfony, ReactPHP) | Medium | Medium | Documented as known limitation; operators warned in README. Followup change planned. |
| R-6 | No cross-service correlation limits adoption beyond single-service profiling | Low (v1 scope) | Low (v1) | Documented; future-work item. |
| R-7 | Rust + PHP extension build complexity (FFI, Zend ABI, packaging) underestimated | Medium | High | Architect should produce a small spike/proof of concept before committing the full design. |
| R-8 | PECL packaging for a Rust-built extension is non-standard; may require source-only distribution in practice | Medium | Low | Source distribution is the MUST; PECL is SHOULD. Drop PECL if it proves disproportionately costly. |
| R-9 | Server outage during a long CLI trace causes silent data loss | Medium | Low (v1) | `dropped_records` exposes loss; v1 scope explicitly excludes spooling-to-disk. |

---

## 15. Success Criteria

### 15.1 Acceptance criteria for v1

The extension is considered v1-complete when **all** of the following are demonstrably true:

1. The extension builds from source on Linux x86_64 for both PHP 8.3 and PHP 8.4.
2. The extension is loadable in CLI and PHP-FPM SAPIs.
3. Running a canonical PHP test workload (TBD: see §16) with the extension loaded and pointing at a stub ingest server:
   - Produces batches conforming to §9.
   - All function calls in the workload are represented (≥ 99.5% of expected calls).
   - Per-call `t_out − t_in` aggregates within ±5% of an Xdebug reference trace for the same workload.
   - Geometric-mean overhead ≤ 2.0× unprofiled.
4. Killing the ingest server mid-trace does not crash PHP; the request completes; logs report drops.
5. Throwing an exception mid-call produces a record with `abnormal_exit = true`.
6. Exceeding `max_depth` does not crash; records past the cap are reflected in `dropped_records`.
7. `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test` all pass cleanly.
8. README documents every `php.ini` directive with default, range, and effect.

### 15.2 Validation approach

- **Unit tests** in the Rust crate for: buffer behavior, threshold logic, retry/backoff, record serialization.
- **Integration tests** that load the extension into a real PHP and run scripted workloads against a localhost stub server.
- **Benchmark suite** comparing profiled vs. unprofiled wall time on a curated set of PHP workloads.
- **Manual smoke test** with an actual ingest server and visualization (once those exist).

### 15.3 Canonical benchmark workloads (TBD)

To be defined jointly with the Solution Architect. Initial candidates:

- A small Symfony "hello world" controller request.
- A WordPress homepage render.
- A CLI batch job that processes ~10⁵ rows from JSON.

Marked as an open question; see §16.

---

## 16. Open Questions

Items the Solution Architect (or follow-up clarification from Daniel) should resolve before or during design:

| ID | Question | Suggested default |
| --- | --- | --- |
| OQ-1 | Exact `trace_id` generation scheme — UUID v4, UUID v7, or other? | UUID v7 (sortable, useful for server-side time-bucketing) |
| OQ-2 | Final media-type string for the wire format. | `application/vnd.php-analyze.v1+msgpack` |
| OQ-3 | Exact HTTP endpoint path layout (e.g. `POST /v1/ingest` vs. `POST /traces`). | `POST /v1/ingest` |
| OQ-4 | Specific Zend hook strategy — `zend_observer` API vs. `zend_execute_ex` override vs. hybrid. | Architect to decide. |
| OQ-5 | Choice of Rust crates for MessagePack, HTTP, UUID, PHP bindings. | Architect to decide. |
| OQ-6 | Whether to ship a stub ingest server (e.g., a tiny Rust binary) alongside v1 to make local development bearable. | Recommend yes, as a separate small change. |
| OQ-7 | Exact canonical benchmark workloads (§15.3). | TBD |
| OQ-8 | Tolerance for slight clock-source variations between FPM workers (e.g., `CLOCK_MONOTONIC_RAW` vs. `CLOCK_MONOTONIC`). | Recommend `CLOCK_MONOTONIC` for `t_in`/`t_out`; `CLOCK_REALTIME` for `start_time`. |
| OQ-9 | Behavior when `php_analyze.server_url` is unset: silent disable, or noisy refuse-to-start? | Recommend silent disable + single startup warning per §NFR-USE-2. |
| OQ-10 | Whether the bearer token may be loaded from a file path (`php_analyze.auth_token_file`) instead of inlined in `php.ini` for secret-management hygiene. | Recommend yes as a SHOULD for v1. |

---

## 17. Appendices

### 17.1 Glossary

| Term | Meaning |
| --- | --- |
| **Trace** | All data captured between `RINIT` and `RSHUTDOWN` for one request or CLI invocation. |
| **Batch** | A single HTTP POST payload, containing metadata, optional dictionary entries, and zero-or-more call records. |
| **Call record** | The §9.1 record emitted on function exit. |
| **Function dictionary** | The per-trace mapping from interned `fn_id` to fully qualified function metadata. |
| **SAPI** | Server API — PHP's term for the runtime host (`cli`, `fpm-fcgi`, etc.). |
| **RINIT / RSHUTDOWN** | PHP extension lifecycle hooks fired at request start and end. |
| **MINIT** | PHP extension lifecycle hook fired at module load (once per process). |
| **Drop counter** | A trace-scoped counter incremented whenever a record is discarded due to depth cap, buffer overflow, or retry exhaustion. |

### 17.2 Configuration directives summary

All directives are `php.ini`-only (v1).

| Directive | Type | Default | Notes |
| --- | --- | --- | --- |
| `php_analyze.enabled` | bool | `1` | Master on/off switch. |
| `php_analyze.server_url` | string | *(none)* | Full URL of the ingest endpoint. |
| `php_analyze.auth_token` | string | *(none)* | Bearer token (subject to OQ-10). |
| `php_analyze.flush_records` | int | `10000` | Flush after N buffered records. |
| `php_analyze.flush_bytes` | int | `1048576` (1 MiB) | Flush after N buffered bytes. |
| `php_analyze.buffer_cap_bytes` | int | `67108864` (64 MiB) | Hard memory cap. |
| `php_analyze.max_depth` | int | `1024` | Max tracked stack depth. |
| `php_analyze.retry_count` | int | `3` | HTTP retry attempts. |
| `php_analyze.retry_backoff_ms` | int | `100` | Base backoff (doubles per attempt). |
| `php_analyze.http_timeout_ms` | int | `2000` | Per-attempt HTTP timeout. |

### 17.3 References

- `personas/ANALYST.md` — this persona.
- `personas/SOLUTION_ARCHITECT.md` — next persona, consumes this doc.
- `personas/RUST_DEVELOPER.md` — third persona, consumes `SPECIFICATION.md`.
- `CLAUDE.md` — project workflow, code style, OpenSpec rules.
- PHP internals documentation (Zend Engine 8.x).
- MessagePack specification (https://msgpack.org/).

---

*End of document. Hand off to Solution Architect for design.*
