# Code Review — Binary Size & Hot-Path Performance

**Branch:** `main` (HEAD `6a06217` — line numbers in this document are pinned to this SHA)
**Reviewer:** Claude Code
**Date:** 2026-05-25
**Scope:** `crates/php-analyze` (cdylib `libphp_analyze.so` only — `stub-ingest` is dev-only and ignored)
**Operating budget:** geo-mean overhead ≤ **5.0×** per `SPECIFICATION.md` §11 R-1 (activated by the Section 2 posture decision below — `cpu_snapshot_mode = PerCall` stays as the default, which makes OBJ-2's original 2× target jointly unachievable on the current syscall surface). The 2× number is retained as the long-term aspirational target; the 5× R-1 fallback is what `benches/workload_overhead.rs::GEOMEAN_BUDGET` must assert against once this review's fixes land.
**Binary-size posture:** no spec-level NFR today. Recommend adding one (≤ 5 MB shipped) once S-1 lands.

---

## Summary

The release cdylib is **18.5 MB on disk**, but the **executable code is only 1.85 MB** — the other ~15 MB is DWARF debug info that the linker keeps even though `[profile.release]` declares `debug = "line-tables-only"`. A three-line profile tweak (**S-1**) brings the shipped `.so` to ~2.7 MB with no code change. Dropping the `url` crate (**S-3**) and the throwaway `spike` module (**S-4**) brings it to ~2.35 MB.

The hot-path overhead (the user reports ~9× on tight-call workloads, against R-1's 5× operating budget) is dominated by **per-call work that runs even when the call will be dropped**. The single largest practical lever — and the one missing from the first pass of this review — is **P-0: an operator-controlled `skip_functions` directive that returns `false` from `should_observe` for a default list of ~35 noise-floor builtins (`strlen`, `count`, `is_array`, …)**. PHP caches that answer per function, so filtered functions cost **literally zero per call** after the first sight. That alone lands the budget at ~5–6×. **P-1 (gate-before-snapshot, no syscalls on dropped calls)** + **P-3 / P-5 / P-6 (small per-event polish)** carry the geo-mean comfortably under 5× with margin.

`cpu_snapshot_mode = PerCall` stays as the default per the operator's posture decision: per-call user-vs-system CPU resolution is required for argument-dependent leaf nodes (e.g. `hash_file($small)` vs `hash_file($big)` — the CPU/IO profile depends on the argument and sampling would smear them together). That decision is the *reason* the budget moves to 5×, not a concession around it. See `COMMENTS.md` C-19 for the syscall-floor arithmetic.

Both the size and the perf fixes are surgical and **do not change the wire format or any reported field value**. P-0 is the only fix that intentionally alters output (filtered functions disappear from the trace); §4.3's verification matrix prescribes how to prove it doesn't touch anything else.

---

## Section 1 — Binary Size

### Where the 18 MB actually goes

`objdump -h libphp_analyze.so` (run for this review):

| Section | Bytes | Note |
|---|---:|---|
| `.debug_info` | 4 899 935 | DWARF type/scope tree |
| `.debug_str` | 3 957 590 | DWARF string pool |
| `.debug_ranges` | 2 186 256 | DWARF range lists |
| `.debug_line` | 2 163 413 | line tables (the only part `line-tables-only` was supposed to keep) |
| **`.text`** | **1 847 254** | **actual machine code** |
| `.debug_loc` | 1 754 759 | location lists |
| `.rodata` | 479 256 | strings + `icu_*` static tables |
| `.eh_frame` | 184 892 | unwind tables |
| everything else | ~600 KB | section headers, GOT/PLT, dynsym, etc. |

Loadable size (text + data + bss) is **2.84 MB**. About **15.7 MB is debug info that PHP never reads** — `dlopen` ignores `.debug_*` sections; they only matter for `gdb`/`perf` and symbolicated backtraces.

### Finding S-1 (CRITICAL): the release profile is shipping full DWARF despite `line-tables-only`

- **File:** `Cargo.toml:20-25`
- **Effort:** five lines
- **Win:** ~15 MB off the shipped `.so` (18 MB → ~3 MB)

```toml
[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
debug = "line-tables-only"
```

Two things are wrong here:

1. The presence of populated `.debug_info`, `.debug_loc`, `.debug_ranges`, `.debug_macro` sections shows that the requested `line-tables-only` setting is **not being honoured for the whole closure** (dependencies built with their own debug settings, `build.rs` artefacts, etc.). A worktree experiment with `cargo build --release && strip --strip-debug libphp_analyze.so` shrinks the file to roughly 2.7 MB.
2. Even on the line-tables-only path, those tables are still useless once the extension is deployed — operators debug PHP scripts, not the recorder's Rust source.

**Suggested fix:**

```toml
[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
debug = false              # was "line-tables-only"
strip = "symbols"          # NEW — strips .debug_* and .symtab at link time
panic = "abort"            # NEW — see S-2
```

If you want symbolicated panics in dev/CI builds, add a `[profile.release-with-debug]` profile and have CI use it for the bench/test stages but not for the shipped artefact.

> **S-2 (`panic = "abort"`) was proposed and declined — see `COMMENTS.md` C-20 for the reliability rationale (shipper-thread panic isolation needs unwinding).**

### Finding S-3 (MAJOR): `url = "2"` brings in the entire ICU stack for a one-line check

- **Files:** `crates/php-analyze/Cargo.toml:29`, `crates/php-analyze/src/config.rs:29, 47, 611-633`, `crates/php-analyze/src/shipper/http.rs:37, 185, 197`
- **Effort:** half a day
- **Win:** ~700 KB of `.text` + the entire `.rodata` ICU table footprint, ~25 transitive crates gone (`idna`, `idna_adapter`, `icu_normalizer`, `icu_normalizer_data`, `icu_properties`, `icu_properties_data`, `icu_provider`, `icu_collections`, `icu_locale_core`, `tinystr`, `litemap`, `writeable`, `zerotrie`, `zerovec` + 4 derive crates, `yoke` + derive, `zerofrom` + derive, `potential_utf`, `utf8_iter`, `form_urlencoded`, `percent-encoding`)

The crate uses `url::Url` for exactly two things:

1. **`config::resolve_server_url` (`config.rs:611`)** — parses the directive value, then reads `.scheme()` and checks it is `"http"` or `"https"`. The actual error reason is rendered with `e.to_string()` into a `ConfigWarning::InvalidUrl { reason }`.
2. **`shipper::http::RmpEncodeAndHttpPost::server_url: Url` (`http.rs:185`)** — stored only to be passed to `ureq`'s `agent.post(...)`. **`ureq::Agent::post` accepts `&str` directly**, so this field never needs to be a `Url`.

The full IDNA machinery `url` drags in is overkill for a config knob that an operator hand-writes in `php.ini`. A scheme-only validator is enough:

```rust
// config.rs — replace resolve_server_url's Url::parse with:
fn resolve_server_url(raw: &RawIni, warnings: &mut Vec<ConfigWarning>)
    -> (Option<String>, ServerUrlOutcome)
{
    let raw_url = raw.server_url.as_deref().unwrap_or("").trim();
    if raw_url.is_empty() { return (None, ServerUrlOutcome::Unset); }
    let (scheme, _rest) = match raw_url.split_once("://") {
        Some(s) => s,
        None => {
            warnings.push(ConfigWarning::InvalidUrl {
                value: raw_url.to_owned(),
                reason: "missing scheme (expected http:// or https://)".into(),
            });
            return (None, ServerUrlOutcome::InvalidUrl);
        }
    };
    match scheme {
        "https" => (Some(raw_url.to_owned()), ServerUrlOutcome::Ok),
        "http"  => { warnings.push(ConfigWarning::HttpScheme);
                     (Some(raw_url.to_owned()), ServerUrlOutcome::Ok) }
        other   => { warnings.push(ConfigWarning::UnsupportedScheme {
                         scheme: other.to_owned() });
                     (None, ServerUrlOutcome::UnsupportedScheme) }
    }
}
```

`Config::server_url` becomes `Option<String>` (or a newtype wrapping `String` if you want type-level "this was scheme-checked" evidence), and `RmpEncodeAndHttpPost::server_url: String` is what gets handed to `ureq`. The existing test in `config.rs` that asserts on `e.to_string()` ("relative URL without a base", "invalid international domain name") needs to be adjusted to the new error wording — half a dozen lines.

### Finding S-4 (MAJOR): `spike` module is documented as throwaway and still ships

- **File:** `crates/php-analyze/src/spike.rs` (640 lines), plus the `spike_observer` / `spike_log_path` directives in `config.rs` and the `BootObserver::Spike` arm in `recorder/mod.rs`
- **Effort:** one OpenSpec change, ~half a day with tests
- **Win:** ~30–40 KB of `.text`, removes the `Arc<Mutex<Box<dyn Write + Send>>>` indirection from the boot dispatcher, removes the second `extract_info` walker that is now a duplicate of `extract_call_site`

The module's own doc comment opens with: *"This module is throwaway. … Phase 2's Recorder change deletes this whole file and its two `php_analyze.spike_*` directives in the same commit that replaces it with the production hot-path."* Phase 2 has shipped (recorder/observer is the production path). The spike's continued presence is dead weight: it gates behind `php_analyze.spike_observer = 0` (the default), so it has zero runtime cost in normal operation, but it inflates the binary and the codebase. The diagnostic value it provides is reachable via the `recorder-dump` cargo feature.

### Sizes recap (estimated, after each cumulative step)

| Step | `.so` size | Notes |
|---|---:|---|
| Today | 18.5 MB | as shipped |
| Apply S-1 (`strip` + `debug=false`) | ~2.7 MB | the headline win |
| Add S-3 (drop `url`) | ~2.4 MB | drops `.rodata` ICU tables + ~25 transitive crates |
| Add S-4 (drop `spike.rs`) | ~2.35 MB | small but warranted (and removes a documented "throwaway" liability) |

A ~2.35 MB Rust cdylib that bundles its own TLS stack (rustls + ring + webpki) is **in line with comparable extensions**. Going below that requires switching off bundled TLS (e.g. delegating HTTPS to PHP's `curl` via `ext_php_rs` bindings, or moving the shipper into a sidecar), and that's a bigger architectural conversation than the scope of this review.

---

## Section 2 — Hot-Path Performance

The bench harness (`benches/workload_overhead.rs`) targets a 2.0× geo-mean (OBJ-2) and skips by default. The user reports ~9× on `flat_calls`-shaped workloads. The hot path is `recorder::observer::Recorder::{begin_handler, end_handler}` (`observer.rs:1012-1049`).

### Posture decision (this review)

The user has confirmed two constraints that shape every finding below:

1. **`cpu_snapshot_mode = PerCall` stays as the default.** Per-call CPU resolution is required because leaf-node functions parameterised on their arguments (e.g. `hash_file($path)`, `curl_exec($handle)`) have argument-dependent CPU/IO profiles that sampling-with-inheritance would smear together. The on-CPU vs off-CPU determination is the whole reason to run the analyser.
2. **The wall-time budget moves from OBJ-2's 2× to R-1's 5×.** `SPECIFICATION.md` §11 row R-1 explicitly anticipates this: *"Phase 5 dedicated to tuning; AC-RC-5 zero-alloc guarantee; fallback target 5× if budget cannot be hit."* That fallback is now activated. The spec text should be updated to reflect the activation (one line in §11; flip the row to "Active").

Under these constraints, the path back to budget is **(a) the cheapest-possible should_observe filter for high-frequency builtins** (the lever that was missing from the original review), **(b) gate-before-snapshot** (no syscalls on dropped calls), and **(c) the smaller per-event wins**. The CPU snapshot itself is no longer up for debate — it stays.

### Finding P-0 (CRITICAL — added in this revision): `should_observe` is unconditionally `true`; an operator-controlled function filter is the cheapest possible cost reduction

- **File:** `crates/php-analyze/src/recorder/observer.rs:1052-1066`
- **Effort:** one OpenSpec change, ~one day with tests + a directive
- **Win:** functions for which `should_observe` returns `false` cost **literally zero per call** — PHP caches the result per function and never invokes `begin` / `end` again for that function. On workloads dominated by tight builtin calls (`flat_calls` is the canonical example), filtering out a small allowlist of high-frequency low-value functions can be a 2–3× wall-time reduction by itself.

Today:

```rust
impl FcallObserver for Recorder {
    /// Unconditionally `true`. See the module doc comment for why a
    /// runtime "is there a current trace?" filter cannot live here.
    fn should_observe(&self, _info: &FcallInfo) -> bool {
        true
    }
    ...
}
```

The module doc (`observer.rs:23-31`) correctly warns against a *transient* `false` here — a `false` based on per-request state would be cached permanently and silently drop that function from every later request. **A static filter does not have that problem**: if it's a deterministic function of the function name (and / or class scope), the answer is the same on the first observation as on the millionth, the PHP cache holds a correct value, and the per-call cost vanishes after the first sight.

**Suggested directive surface:**

```ini
; Comma-separated list of function names (and Class::method forms) to skip.
; Frozen at MINIT, deterministic on name, safe to cache.
; Default: a curated "noise-floor" list — see below.
php_analyze.skip_functions = "<built-in default, see below>"

; Coarser knob — skip ALL Zend internal functions. Useful when the
; operator only cares about user-code attribution. Default: 0 (observe).
php_analyze.skip_internal = 0
```

**Suggested built-in default list (~35 functions).** Every entry below satisfies all four selection criteria: (1) called very frequently in typical PHP code, (2) sub-microsecond cost per call so observer overhead is a much larger multiple than for "interesting" functions, (3) pure-CPU with no I/O or allocation worth measuring, (4) the CPU-vs-IO question this analyser is built to answer is uninformative for them (always 100% on-CPU, always cheap). An operator who explicitly needs one of these in their trace can override by setting `php_analyze.skip_functions` themselves — the default is a list, not a hardcoded set.

```text
# Type predicates (return bool, no allocation, branch-only)
is_array, is_bool, is_callable, is_countable, is_float, is_int,
is_integer, is_iterable, is_long, is_null, is_numeric, is_object,
is_scalar, is_string,

# Cheap introspection (symbol-table or zend_string lookups; no autoload)
gettype, get_class, get_called_class, get_parent_class,
function_exists, method_exists, property_exists, defined,
spl_object_id, spl_object_hash,

# Length / count (O(1) on PHP's internal types — strlen reads zend_string.len,
# count reads HashTable.nNumOfElements)
strlen, count, sizeof, array_key_exists, key_exists, key, current,

# Type conversions (no parse loops worth attributing per-call)
intval, floatval, doubleval, strval, boolval,

# Cheap math primitives (single-op CPU; never blocked)
abs, floor, ceil, round, intdiv
```

**Deliberately *not* on the default list** (and why — these are common asks that don't belong):

- `in_array`, `array_search` — linear scan, cost is O(needle-vs-haystack), per-call attribution is valuable.
- `array_keys`, `array_values`, `array_merge`, `array_filter`, `array_map` — allocating; cost depends on input size.
- `preg_match`, `preg_replace`, `preg_split` — regex compilation + backtracking; can be a real hotspot.
- `explode`, `implode`, `str_replace`, `substr`, `sprintf` — allocating; cost varies wildly with input.
- `json_encode`, `json_decode`, `serialize`, `unserialize`, `hash`, `md5`, `sha1` — pure CPU but with input-dependent cost; exactly the leaf-node case PerCall was kept for.
- `microtime`, `time`, `hrtime` — fast and tempting to skip, but application code that itself measures timing is interesting to see in the trace.
- `class_exists`, `interface_exists`, `trait_exists` — can trigger autoload, which is interesting.
- `ini_get`, `ini_set`, `error_reporting` — fast, but operators sometimes want to see configuration reads in the trace; safer left in by default.
- `func_get_args`, `func_num_args` — `func_get_args` allocates the args array; `func_num_args` is cheap but co-located with code patterns worth seeing.

Both directives are read once at MINIT into a frozen `HashSet<&'static str>` (or a small perfect-hash table built from the directive value); `should_observe` does one hash probe and returns. The hash probe runs **once per unique function in the lifetime of the process** — PHP's own caching does the rest.

**Why this isn't a sampling/aggregation compromise.** The user-stated reason for keeping `PerCall` is that leaf-node argument-dependent functions (`hash_file`, `curl_exec`, custom `loadConfig`) need per-call CPU resolution. The filter targets the opposite end of the cost-vs-information spectrum: it removes functions whose per-call cost is negligible *and* whose CPU/IO profile is uninteresting (`strlen`, `count`, `is_array` are pure CPU, well-understood, never blocked). The CPU signal on the leaf nodes you actually want is preserved at full resolution; the noise floor disappears.

**Worked numbers.** `flat_calls.php` is a tight loop calling builtins; a representative implementation calls `strlen` and `count` many times. If the loop dominates the workload and those two functions are excluded, the per-call observer overhead on the excluded calls drops from ~1 200 ns (two clock_gettime + two getrusage + two zend_memory_usage + bookkeeping) to **0 ns** — PHP doesn't even reach the trampoline. Even a five-name default skip list (`strlen`, `count`, `is_array`, `is_string`, `is_null`) covers the bulk of the high-frequency cheap-builtin cost in most PHP application code without compromising the signal on the functions operators care about.

### Finding P-1 (CRITICAL): snapshots are captured **before** the depth/cap gate

- **File:** `crates/php-analyze/src/recorder/observer.rs:1019-1028` (begin), `1040-1049` (end)
- **Effort:** one afternoon + tests
- **Win:** eliminates the syscall trio on every dropped call — on workloads that breach `max_depth` even once, this can be a 5–10× cliff because every descendant pays full price and is then thrown away

Current shape (begin):

```rust
with_current_trace(|trace| {
    let snapshots = EntrySnapshots::capture_now();   // ← 1× clock_gettime + 1× getrusage + 1× zend_memory_usage
    let lazy = categorise_lazy(&info);
    begin_with_snapshots_lazy(trace, &lazy, snapshots);
});
```

Inside `begin_with_snapshots_lazy` (`observer.rs:1197-1263`) the first two operations are the depth gate (`trace.virtual_depth > trace.max_depth → record_drop`) and the cap gate. **If either fires, `snapshots` is discarded.** That's three syscalls of work per dropped begin, and dropped begins come in storms once a recursion crosses `max_depth` (every descendant call is also dropped).

Similarly, `end_with_snapshots` (`observer.rs:1299-1319`) starts with `if trace.dropped_begins > 0 { … return; }` — the snapshots passed in are unused on this branch, but `end_handler` (line 1044) already paid the syscall trio + `has_exception()` to capture them.

**Suggested shape (begin):**

```rust
with_current_trace(|trace| {
    // Cheap gate first — depth is a u32 compare, no syscalls.
    if trace.virtual_depth.saturating_add(1) > trace.max_depth {
        trace.virtual_depth = trace.virtual_depth.saturating_add(1);
        trace.record_drop();
        return;
    }
    let lazy = categorise_lazy(&info);
    // Cap gate — also no syscalls (atomic load + len() arithmetic).
    let would_add = /* same projection as today */;
    if accounting::snapshot().saturating_add(would_add) > trace.buffer_cap_bytes {
        trace.virtual_depth = trace.virtual_depth.saturating_add(1);
        trace.record_drop();
        return;
    }
    // Only now do we pay for the syscalls.
    let snapshots = EntrySnapshots::capture_now();
    begin_with_snapshots_lazy_accept(trace, &lazy, snapshots);  // accept-only tail
});
```

`begin_with_snapshots_lazy` then splits into a gate-aware predicate and an accept-only tail; the existing function stays as the bench-seam entry point with the original semantics. The `end` path mirrors the same idea — peek `dropped_begins > 0` before capturing.

The change is mechanical and the existing tests (`observer.rs`'s `begin_at_max_depth_plus_one_is_dropped_and_bumps_counter`, etc.) keep their assertions. Add one test that asserts no snapshot is captured on the drop path via a test-only counter on `EntrySnapshots::capture_now`.

> **P-2 (`cpu_snapshot_mode` posture) was deliberated and is informational — `PerCall` stays per the Section 2 posture decision. Background syscall-floor analysis and the `/proc/thread-self/stat` optimisation hypothesis: see `COMMENTS.md` C-19. No action item in this review.**

### Finding P-3 (MAJOR): every event re-resolves `Config::global()` to pick the CPU mode

- **File:** `crates/php-analyze/src/recorder/observer.rs:339-347`
- **Effort:** 15 minutes
- **Win:** removes one `OnceLock::get` + `Option::map` + `unwrap_or` per snapshot, both at begin and end

```rust
fn current_cpu_snapshot_mode() -> crate::config::CpuSnapshotMode {
    #[cfg(test)]
    if let Some(mode) = cpu_snapshot_mode_test_override() { return mode; }
    crate::config::Config::global()
        .map(|c| c.cpu_snapshot_mode)
        .unwrap_or(crate::config::CpuSnapshotMode::PerCall)
}
```

The mode is frozen at MINIT and never mutates. Cache it once into a static `AtomicU8` (the test override slot already in this file is the template) at `bootstrap::startup`. The hot path reads one relaxed atomic. This is a tiny saving — but multiplied by two reads per call across millions of calls per request, it's measurable.

> **P-4 (`RefCell<Trace>` → `Cell<*mut Trace>`) is deferred — it introduces unsafe for a marginal gain and only earns its place if benches show the borrow check as next-dominant after P-0 / P-1 / P-3 / P-5 / P-6 land. See `COMMENTS.md` C-21 for the revisit condition.**

### Finding P-5 (MAJOR): two `from_utf8_lossy` passes per call

- **File:** `crates/php-analyze/src/recorder/observer.rs:598-606` (`zend_string_to_cow`)
- **Effort:** 1–2 hours
- **Win:** ~10–30 ns per call; cumulative on every call

`String::from_utf8_lossy` walks the byte slice twice: once to validate, once to decode if invalid. PHP function and class names are parser-validated identifiers (ASCII, by the language's rules); file paths can be non-UTF-8 on weird filesystems but the spike's own data (cited in the module doc) says this is rare.

```rust
unsafe fn zend_string_to_cow<'a>(zs: *mut ffi::zend_string) -> Option<Cow<'a, str>> {
    if zs.is_null() { return None; }
    let len = unsafe { (*zs).len };
    let ptr = unsafe { (*zs).val.as_ptr() };
    let slice = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
    // Fast path: pure ASCII (the common case for function/class names).
    // ASCII is a strict subset of valid UTF-8.
    if slice.is_ascii() {
        // SAFETY: ASCII bytes are valid UTF-8.
        return Some(Cow::Borrowed(unsafe { std::str::from_utf8_unchecked(slice) }));
    }
    // Non-ASCII path: try strict UTF-8, fall back to lossy only on failure.
    match std::str::from_utf8(slice) {
        Ok(s) => Some(Cow::Borrowed(s)),
        Err(_) => Some(Cow::Owned(String::from_utf8_lossy(slice).into_owned())),
    }
}
```

`slice::is_ascii` is SIMD-vectorised by LLVM (it lowers to `pmovmskb` on x86_64). For typical PHP identifiers (`strlen`, `User::find`, `MyApp\Controller`) it's a single 16-byte AVX2 chunk per name and one compare. Strictly faster than the two-pass `from_utf8_lossy`.

### Finding P-6 (MAJOR): `extract_call_site` is called even when the trace slot is empty

- **File:** `crates/php-analyze/src/recorder/observer.rs:1012-1018`
- **Effort:** 5 minutes
- **Win:** moves the unsafe Zend walk inside the `with_current_trace` closure — saves it for out-of-request observer fires (between MINIT and the first RINIT, after RSHUTDOWN if PHP retains the observer cache, etc.)

```rust
fn begin_handler(&self, execute_data: &ExecuteData) {
    let info = unsafe { extract_call_site(execute_data) };  // ← always paid
    with_current_trace(|trace| { ... });
}
```

The crate's own RO-6 commentary (`observer.rs:1006-1011`) does this for snapshots but not for `extract_call_site`. The walk is cheap (~50–100 ns) but it touches unsafe pointer chains, which is a separate cost. Moving it inside the `with_current_trace` closure means out-of-request fires pay zero. Trivial to fix.

> **P-7 (`unknown_placeholder` `format!`) and P-8 (`rmp_serde::to_vec_named` vs `to_vec`) are noted but not actionable — P-7 is too rare to matter and P-8 is off the request hot path with wire-format implications. See `COMMENTS.md` C-22.**

### Performance recap (under the chosen posture: PerCall stays, budget = 5×)

| Step | Expected new geo-mean (rough) | Notes |
|---|---:|---|
| Today (`PerCall` default, no filtering) | ~9× | reported |
| Apply P-0 with the default builtin skip list | ~5–6× | removes the high-frequency-builtin noise floor; PHP's own cache makes the saving free per-call after first sight |
| Apply P-1 (gate before snapshot) | ~4.5–5.5× | no syscall trio on dropped begins / ends |
| Apply P-3 (cache mode at MINIT) + P-5 (ASCII fast path) + P-6 (extract inside slot check) | ~3.5–4.5× | small wins compound across the every-call path |

These numbers are honest guesses based on per-syscall costs and per-instruction counts; the `workload_overhead` bench is the source of truth once P-0 + P-1 ship. The 5× R-1 budget is **comfortably achievable** under this posture — likely with a 1–2× margin once P-3 / P-5 / P-6 land. P-4 stays parked (`COMMENTS.md` C-21) and is only revisited if the bench post-P-6 still shows the borrow check as the next dominant term.

**Why P-0 is now the headline.** It's the only finding that costs literally zero per call (PHP caches the negative answer), and on builtin-heavy workloads the saving compounds the most. Every other finding shaves nanoseconds off each event; P-0 deletes events entirely.

---

## Section 3 — Dead and Quasi-Dead Code

A handful of items that can come out cleanly without affecting any documented behaviour:

| Item | Location | Reason |
|---|---|---|
| `spike` module | `src/spike.rs` (640 lines) | Doc comment says it is throwaway and was due for deletion in Phase 2. Phase 2 has shipped. |
| `RawCallSite::empty` constructor | `observer.rs:497-511` | Only reached on `func_ptr.is_null()` in `extract_call_site`. The branch already returns immediately. The helper exists only so tests can construct an empty one; gate it behind `#[cfg(test)]`. |
| Test-only CPU snapshot override (`CPU_SNAPSHOT_MODE_TEST_OVERRIDE` and its `Mutex`) | `observer.rs:358-465` | Already `cfg(test)`-gated. Kept here for the record — verify it doesn't leak into the cdylib via inlining (it shouldn't, but a `cargo bloat --release` on `Recorder::*` will confirm). |
| `Dictionary::intern` (owning-key version) | `recorder/dictionary.rs:63-79` | Production path is `intern_ref`. The owning version remains for the slice-2 bench-seam and tests. If the bench-seam re-exports can be narrowed to `intern_ref` only, delete `intern`. |
| `Categorised` + `categorise` | `observer.rs:620-746` (~130 lines) | Production path is `LazyCategorised` + `categorise_lazy`. The owning version exists only for tests and the bench-seam. Same comment as `intern`: collapse if the bench-seam can ride on `categorise_lazy` alone. |
| `FunctionKey::matches_ref` / `as_ref` | `recorder/types.rs:176-235` | Audit which directions are still used; on a `LazyCategorised`-only world several of these conversions become dead. |

None of these is structural; together they shed ~1 200 lines of production code and a handful of public-by-necessity items.

---

## Section 4 — Verification Plan

The repo has enough test infrastructure to validate every change in this review against the goal **"same report, faster, smaller library."** This section inventories what exists and then maps each change to the specific tests/benches that will catch a regression.

### 4.1 Test infrastructure inventory

| Layer | Where | What it validates | Gate | When it runs |
|---|---|---|---|---|
| **Pure-Rust unit tests** | `src/**/*.rs` `#[cfg(test)]` (~390 tests) | Parsing, categorisation, dictionary, accounting, wire schema, config, fallback paths. Heaviest in `observer.rs` (106), `bootstrap.rs` (58), `shipper/mod.rs` (42), `types.rs` (37). | none | `cargo test --all` — CI every push |
| **Zero-alloc audit** | `tests/recorder_zero_alloc.rs` (4 tests, custom `CountingAllocator`) | Steady-state begin/end path performs **exactly zero** heap allocations. Binds AC-RC-5. | none | `cargo test --all` — CI every push |
| **Recorder integration** | `tests/recorder_observer.rs` + `tests/php-recorder/*.php` + `recorder-dump` feature | End-to-end recorder dump byte-shape: function categorisation, depth/cap drops, threshold flushes, RSHUTDOWN drain, exception-unwind detection. | `PHP_ANALYZE_RUN_RECORDER=1` + `php8.x` on PATH | CI matrix `[8.3, 8.4]` |
| **Shipper round-trip** | `tests/shipper_round_trip.rs` + `tests/php-shipper/*.php` | PHP → recorder → channel → shipper thread → `rmp_serde` encode → `ureq` POST → stub-ingest → `/debug/batches`. Asserts decoded `wire::Batch` field-equality, auth header, User-Agent. | `PHP_ANALYZE_RUN_SHIPPER=1` | CI matrix |
| **FPM integration** | `tests/fpm_repeated_requests.rs` + `tests/php-fpm/fpm_repeated.php` | Loadable under `fpm-fcgi` SAPI; 100 sequential requests don't leak RSS > 2 MiB; exactly one shipper thread per FPM worker. Binds §1.3 #2, AC-BS-3, AC-PB-1. | `PHP_ANALYZE_RUN_FPM=1` + `php-fpm8.x` | not in CI today (operator-driven) |
| **Stub-ingest tests** | `crates/stub-ingest/tests/round_trip.rs` (36 tests) | The reference collector implementation: decoder, `/debug/batches`, header capture, capture-dir. | none | `cargo test --all` |
| **Per-call microbench** | `benches/recorder_hot_path.rs` (criterion) | Per-call `begin + end` cost in nanoseconds against a pre-allocated `Trace` (no PHP, no observer trampoline, no HTTP). The hot-path floor. | `bench-seam` feature | operator-driven |
| **Workload microbench** | `benches/recorder_workload.rs` (criterion) | The three canonical workloads measured against the criterion harness directly. | `bench-seam` feature | operator-driven |
| **PHP-driven NFR-PERF-1 bench** | `benches/workload_overhead.rs` + `tests/php-bench/*.php` | Unprofiled vs profiled wall-time geo-mean across `flat_calls`, `json_batch`, `recursive_walk`. Asserts `geo-mean <= GEOMEAN_BUDGET`. **This is the single most authoritative perf gate.** | `PHP_ANALYZE_RUN_BENCH=1` | operator-driven |
| **xdebug accuracy spot-check** | `tools/xdebug-spot-check/` (`run.sh` + `compare.py` → `REPORT.md`) | Runs a PHP fixture under both Xdebug trace mode and `php-analyze`; produces a Markdown report with per-function call coverage and per-call timing-delta histogram (p50/p95/p99). Binds §1.3 #3b (≥99.5% coverage) and #3c (±5% timing) at MVP-closing scope. | host needs `php8.x-xdebug` + `python3-msgpack` | operator-driven, not in CI |
| **Captured reference batches** | `tools/captured-batches/` + `tools/capture-fixtures.sh` | Real MessagePack batches from canonical workloads, committed to git for the visualiser team's parser tests. Schema-shape regression detector. | host needs `php8.x` | operator-driven |
| **CI matrix** | `.github/workflows/ci.yml` | `cargo fmt --check` → `cargo clippy -D warnings` → `cargo test --all` → `cargo build --features recorder-dump` → recorder integration → shipper round-trip. Run against PHP 8.3 + 8.4. | n/a | every push/PR |

### 4.2 What's missing today (intentionally noted, not part of the review's fixes)

- **No CI-gated comparator against Xdebug.** `tools/xdebug-spot-check/` is operator-driven; it can be promoted to a CI step but currently isn't (per `COMMENTS.md` §6.4 it's deferred).
- **No CI-gated workload_overhead.** Same posture: PHP-subprocess variance + 30s runtime per fixture × 3 fixtures × 2 PHP versions makes it expensive for every PR. It belongs as a nightly job, not per-push.
- **No structural diff of captured batches across changes.** Each run produces fresh timestamps/pids; a "schema regression" detector that normalises those fields and structurally diffs the wire shape would be useful but doesn't exist yet.

### 4.3 Per-change verification matrix

For each accepted change, here is the **baseline-then-after** protocol: what to capture before the change lands, what to assert after, and which signal proves "same report, faster, smaller."

#### S-1 — `[profile.release]` strip + `debug = false`

Risk to reported data: **none.** Only changes debuginfo emission to the binary.

| Check | Command | Pass criterion |
|---|---|---|
| Binary size dropped | `ls -la target/release/libphp_analyze.so` | ≤ 3 MB (was 18.5 MB) |
| No DWARF sections remain | `objdump -h target/release/libphp_analyze.so \| grep debug` | no output (or only `.debug_line` if you keep line tables) |
| Full unit suite passes | `cargo test --all` | green |
| Clippy still clean | `cargo clippy --all-targets --all-features -- -D warnings` | green |
| Integration tests pass | `PHP_ANALYZE_RUN_RECORDER=1 cargo test --test recorder_observer --features recorder-dump` and `PHP_ANALYZE_RUN_SHIPPER=1 cargo test --test shipper_round_trip` | green |
| Loads in real PHP | `php -n -d extension=$(pwd)/target/release/libphp_analyze.so -r 'echo "ok\n";'` | prints `ok` |

#### S-3 — drop `url` crate, replace with scheme-only validator

Risk to reported data: **none at runtime.** Only affects MINIT-time config parsing and the wording of `ConfigWarning::InvalidUrl::reason`.

| Check | Command | Pass criterion |
|---|---|---|
| Config unit tests pass | `cargo test -p php-analyze --lib config::` | green — note: ~5 tests assert specific `Url::parse` error wording (`"relative URL without a base"`, etc.). These will need to be updated to match the new error wording. |
| Lockfile shrinks | `cargo tree -p php-analyze --target x86_64-unknown-linux-gnu \| wc -l` (before vs after) | drops by ~25 crates (idna, icu_*, zerovec*, yoke, zerofrom, percent-encoding, form_urlencoded) |
| Shipper still POSTs | `PHP_ANALYZE_RUN_SHIPPER=1 cargo test --test shipper_round_trip` | green — proves the new `String` URL is still accepted by `ureq::Agent::post(&str)` |
| MINIT still rejects bad URLs | unit test the four cases: valid https, valid http (with warning), bare hostname (invalid), `ftp://` (unsupported scheme) | all four produce the documented `ServerUrlOutcome` |

#### S-4 — delete `spike` module

Risk to reported data: **none.** Spike is off by default and reached only via `BootObserver::Spike`.

| Check | Command | Pass criterion |
|---|---|---|
| `tests/spike_observer.rs` deleted | `git status` | file gone |
| `php_analyze.spike_observer` + `spike_log_path` directives removed | grep `crates/php-analyze/src/config.rs` | no matches |
| `BootObserver::Spike` arm removed from bootstrap | grep `crates/php-analyze/src/recorder/mod.rs` | no match |
| Full unit suite passes | `cargo test --all` | green (21 spike unit tests removed cleanly) |
| Recorder integration unchanged | `PHP_ANALYZE_RUN_RECORDER=1 cargo test --test recorder_observer --features recorder-dump` | green |
| Shipper round-trip unchanged | `PHP_ANALYZE_RUN_SHIPPER=1 cargo test --test shipper_round_trip` | green |

#### P-0 — `skip_functions` + `skip_internal` directives

Risk to reported data: **this is the change that intentionally alters output.** Operators see fewer calls for skipped functions. Verification must prove (a) skipped functions are absent from the output, (b) non-skipped functions are unchanged, (c) defaults are sensible.

| Check | Command | Pass criterion |
|---|---|---|
| New directive parsing tests | `cargo test -p php-analyze --lib config::skip_functions_` (new test module) | each: empty default, comma list, whitespace tolerance, Class::method form, unknown name → silently kept (not validated against real PHP), `skip_internal=1` parses, invalid value warns + falls back |
| `should_observe` unit tests | new tests in `observer.rs` | each function name in the default list → `false`; arbitrary user function name → `true`; method shape `Foo::bar` honours `Class::method` filter; internal flag interacts with `skip_internal` |
| **New PHP integration fixture** | new `tests/php-recorder/skip_functions.php` calling `strlen("x"); strlen("y"); foo();` + assertion in `recorder_observer.rs` | recorder-dump contains exactly one `noop`-shaped record for `foo`, zero records for `strlen` |
| Existing recorder integration unchanged | `PHP_ANALYZE_RUN_RECORDER=1 cargo test --test recorder_observer --features recorder-dump` | green — slice-2/3 fixtures don't call default-skipped builtins, so dump shapes are byte-equal vs. today |
| Wire round-trip unchanged for non-skipped funcs | `PHP_ANALYZE_RUN_SHIPPER=1 cargo test --test shipper_round_trip` | green — `noop.php` calls `noop()`, which isn't in the default skip list |
| **xdebug spot-check on recursive_walk** | `./tools/xdebug-spot-check/run.sh tests/php-bench/recursive_walk.php` | call-coverage table unchanged — `recursive_walk` uses user-defined recursion, no default-skipped builtins. If coverage changes here, the default list is too aggressive. |
| **xdebug spot-check on json_batch** | `./tools/xdebug-spot-check/run.sh tests/php-bench/json_batch.php` | `count()` may disappear from the coverage table (it's on the default skip list) — that's the **expected** signal. `json_encode` / `json_decode` / `array_map` must be unchanged. |
| **Workload bench shows the win** | `PHP_ANALYZE_RUN_BENCH=1 cargo bench -p php-analyze --bench workload_overhead` | `flat_calls` ratio drops the most; `recursive_walk` barely changes (no skipped builtins); geo-mean improves measurably |
| Captured batches structural diff | `./tools/capture-fixtures.sh` and inspect `tools/captured-batches/*/batch-*.msgpack` (decode via Python `msgpack`) | non-skipped function records unchanged in shape and count; skipped-function records absent |

#### P-1 — gate-before-snapshot

Risk to reported data: **none.** Same data captured on accepted calls. Dropped calls didn't have data to begin with — their snapshot was thrown away.

| Check | Command | Pass criterion |
|---|---|---|
| Existing drop-counter tests pass | `cargo test -p php-analyze --lib observer::tests::begin_at_max_depth_plus_one_is_dropped` and `cargo test ... begin_above_cap_is_dropped` | green — drop counts and `dropped_begins` LIFO matcher unchanged |
| **New test: no snapshot on drop** | add a `cfg(test)`-only counter on `EntrySnapshots::capture_now`; assert it is unchanged after a drop | counter delta = 0 across the drop test |
| Recorder dump byte-equal on slice-3 fixtures | `PHP_ANALYZE_RUN_RECORDER=1 cargo test --test recorder_observer --features recorder-dump` | green — `deep_recursion.php` and `cap_drops.php` dumps must match byte-for-byte vs the pre-change baseline |
| Zero-alloc audit unchanged | `cargo test --test recorder_zero_alloc` | green — accept path's alloc count is still 0 |
| **Microbench shows the win** | `cargo bench -p php-analyze --features bench-seam --bench recorder_hot_path` | accept-path time unchanged; **add a new bench case that exercises a deep_recursion shape** to capture the dropped-call win |
| Workload bench (long-running) | `PHP_ANALYZE_RUN_BENCH=1 cargo bench --bench workload_overhead` | none of the canonical workloads naturally hits `max_depth`, so the bench-visible effect is small; the proof is in the unit-level cost of a dropped call |

#### P-3 — cache `cpu_snapshot_mode` in static `AtomicU8` at MINIT

Risk to reported data: **none.** Same field value, retrieved faster.

| Check | Command | Pass criterion |
|---|---|---|
| Existing CPU-mode tests pass | `cargo test -p php-analyze --lib clocks::` and `observer::current_cpu_snapshot_mode` | green — the test-override slot already exists; new MINIT-cache must respect the override under `#[cfg(test)]` |
| **New test: cache reset between tests** | tests must reset the new static `AtomicU8` to its uninitialised sentinel; existing `CpuSnapshotModeTestGuard` is the template | no flake under `cargo test -- --test-threads=8` |
| Microbench shows the win | `cargo bench --features bench-seam --bench recorder_hot_path` | ~5-10 ns reduction per begin+end (visible above criterion's noise floor) |
| Wire output byte-equal | `PHP_ANALYZE_RUN_SHIPPER=1 cargo test --test shipper_round_trip` | green |

#### P-5 — `is_ascii()` fast path in `zend_string_to_cow`

Risk to reported data: **none.** ASCII is a strict subset of valid UTF-8; `from_utf8_unchecked` on confirmed-ASCII bytes is sound. The non-ASCII path still falls back to lossy decode.

| Check | Command | Pass criterion |
|---|---|---|
| Categorise tests pass | `cargo test -p php-analyze --lib observer::tests::categorise_` | green — function/method/closure routing unchanged |
| **New non-ASCII test** | new test feeding a `zend_string` with invalid UTF-8 (e.g. `b"\xff\xfeabc"`) through `categorise_lazy` | result is `Cow::Owned` with U+FFFD substitutions; the lossy-decode path is reached |
| **New ASCII fast-path test** | new test asserting that for pure-ASCII input the returned `Cow` is `Borrowed` | identity (not just equality) — confirm the borrowed branch via `matches!(cow, Cow::Borrowed(_))` |
| Wire output for non-ASCII paths byte-equal | dedicated PHP fixture with a non-UTF-8 file path (e.g. `tests/php-recorder/non_utf8_path.php` if feasible on the CI host) → recorder-dump | the lossy-substituted name appears in the dump exactly as before |
| Recorder dump byte-equal on all existing fixtures | `PHP_ANALYZE_RUN_RECORDER=1 cargo test --test recorder_observer --features recorder-dump` | green — slice-2/3 fixtures' dumps unchanged |
| Microbench shows the win | `cargo bench --features bench-seam --bench recorder_hot_path` | ~10-30 ns reduction per begin |

#### P-6 — move `extract_call_site` inside `with_current_trace`

Risk to reported data: **none.** Same data captured when a trace exists.

| Check | Command | Pass criterion |
|---|---|---|
| Unit tests pass | `cargo test -p php-analyze --lib` | green |
| **New test: no `extract_call_site` call when slot is empty** | use a test-only counter on `extract_call_site` (or a feature-gated equivalent); assert 0 after a `begin` with no current trace | counter delta = 0 |
| Recorder integration unchanged | `PHP_ANALYZE_RUN_RECORDER=1 cargo test --test recorder_observer --features recorder-dump` | green |
| Shipper round-trip unchanged | `PHP_ANALYZE_RUN_SHIPPER=1 cargo test --test shipper_round_trip` | green |

### 4.4 The "did I break anything, and did it actually get faster" recipe

Most assertions in §4.3 compare **before vs after** — so the workflow has two phases: capture the baseline on `main`, then re-measure after the change lands on the feature branch.

#### Step 0 — capture the baseline on `main` (before starting work)

Do this **once per change batch**, before any code edits. The artefacts produced here are the ground truth that every "byte-equal" / "geo-mean improved" assertion compares against.

```sh
# Make sure you're on a clean main.
git checkout main && git pull --ff-only && git status   # working tree must be clean

# Pick a scratch dir outside the repo so artefacts survive branch switches.
BASELINE=/tmp/php-analyze-baseline && mkdir -p "$BASELINE"

# 0a. Binary size baseline (for S-1).
cargo build --release -p php-analyze
ls -la target/release/libphp_analyze.so > "$BASELINE/binary-size.txt"
objdump -h target/release/libphp_analyze.so > "$BASELINE/elf-sections.txt"

# 0b. Dependency tree baseline (for S-3).
cargo tree -p php-analyze --target x86_64-unknown-linux-gnu > "$BASELINE/cargo-tree.txt"

# 0c. Recorder-dump baseline (for P-0 / P-1 / P-5 — "byte-equal" assertions).
#     The dump files are written to $PHP_ANALYZE_DUMP_PATH; the helper script
#     under tests/php-recorder/run.sh already plumbs that env var per-fixture.
mkdir -p "$BASELINE/dumps"
PHP_ANALYZE_DUMP_BASELINE_DIR="$BASELINE/dumps" \
  PHP_ANALYZE_RUN_RECORDER=1 \
  cargo test --test recorder_observer --features recorder-dump -- --nocapture \
  2>&1 | tee "$BASELINE/recorder-observer.log"
# (If recorder_observer.rs does not yet write dumps to a baseline-archive dir,
# `cp tests/php-recorder/*.dump $BASELINE/dumps/` after the test run is the
# fallback — the dump files land next to the .php fixture by default.)

# 0d. Criterion baselines for the per-call and per-workload benches.
cargo bench -p php-analyze --features bench-seam --bench recorder_hot_path \
  -- --save-baseline main
cargo bench -p php-analyze --features bench-seam --bench recorder_workload \
  -- --save-baseline main

# 0e. NFR-PERF-1 workload geo-mean baseline (the perf gate).
#     workload_overhead.rs prints a markdown table to stdout; capture it.
PHP_ANALYZE_RUN_BENCH=1 PHP_ANALYZE_BENCH_NO_ASSERT=1 \
  cargo bench -p php-analyze --bench workload_overhead \
  2>&1 | tee "$BASELINE/workload-overhead.md"

# 0f. (Optional but recommended) Xdebug spot-check baseline.
#     Re-run after the change and diff the REPORT.md files.
./tools/xdebug-spot-check/run.sh tests/php-bench/recursive_walk.php
cp tools/xdebug-spot-check/REPORT.md "$BASELINE/xdebug-recursive_walk.md"
./tools/xdebug-spot-check/run.sh tests/php-bench/flat_calls.php
cp tools/xdebug-spot-check/REPORT.md "$BASELINE/xdebug-flat_calls.md"
./tools/xdebug-spot-check/run.sh tests/php-bench/json_batch.php
cp tools/xdebug-spot-check/REPORT.md "$BASELINE/xdebug-json_batch.md"

# 0g. Captured reference batches baseline.
./tools/capture-fixtures.sh
cp -r tools/captured-batches "$BASELINE/captured-batches-main"
```

You now have a complete `$BASELINE/` directory the rest of the workflow diffs against. Tag it with the SHA so you don't accidentally mix two runs: `echo "$(git rev-parse HEAD)" > "$BASELINE/baseline-sha.txt"`.

#### Step 1 — develop on the feature branch as normal

Make the code changes for the OpenSpec change. Iterate on `cargo test` + `cargo clippy` until those are green. The fast gates are:

```sh
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all                                          # ~1 min
cargo test --test recorder_zero_alloc                     # ~10 s — alloc contract
```

#### Step 2 — re-measure on the feature branch (validate the change)

After the gates in Step 1 are green, run the heavier validations against the same fixtures the baseline used:

```sh
# 2a. Binary size after (compare against $BASELINE/binary-size.txt).
cargo build --release -p php-analyze
ls -la target/release/libphp_analyze.so
diff "$BASELINE/elf-sections.txt" <(objdump -h target/release/libphp_analyze.so) || true

# 2b. Dependency tree after.
diff "$BASELINE/cargo-tree.txt" <(cargo tree -p php-analyze --target x86_64-unknown-linux-gnu)

# 2c. Recorder integration (~30 s; needs php8.x on PATH).
PHP_ANALYZE_RUN_RECORDER=1 \
  cargo test --test recorder_observer --features recorder-dump -- --nocapture
# Then diff the new dump files against $BASELINE/dumps/ — byte-equal proves
# P-1 / P-3 / P-5 / P-6 didn't alter recorded data. For P-0, the new fixture's
# dump should differ ONLY in the expected ways (skipped functions absent).

# 2d. Shipper round-trip (~30 s; needs php8.x on PATH).
PHP_ANALYZE_RUN_SHIPPER=1 cargo test --test shipper_round_trip

# 2e. FPM integration (~2 min; needs php-fpm8.x on PATH).
PHP_ANALYZE_RUN_FPM=1 cargo test --test fpm_repeated_requests

# 2f. Per-call microbench, compared against the baseline.
cargo bench -p php-analyze --features bench-seam --bench recorder_hot_path \
  -- --baseline main
cargo bench -p php-analyze --features bench-seam --bench recorder_workload \
  -- --baseline main
# criterion prints a per-case improvement / regression report. The change
# should show an improvement on at least one case and no regression > 5% on any.

# 2g. NFR-PERF-1 workload geo-mean (~3 min).
PHP_ANALYZE_RUN_BENCH=1 PHP_ANALYZE_BENCH_NO_ASSERT=1 \
  cargo bench -p php-analyze --bench workload_overhead \
  2>&1 | tee /tmp/workload-overhead-after.md
diff "$BASELINE/workload-overhead.md" /tmp/workload-overhead-after.md
# Geo-mean should be lower than the baseline. Once the cumulative effect of the
# fix series lands the geo-mean under 5.0×, flip GEOMEAN_BUDGET back on
# (remove `PHP_ANALYZE_BENCH_NO_ASSERT=1`) and let workload_overhead assert
# against the new R-1 budget.

# 2h. Xdebug spot-check (the "same data" gate).
./tools/xdebug-spot-check/run.sh tests/php-bench/recursive_walk.php
diff "$BASELINE/xdebug-recursive_walk.md" tools/xdebug-spot-check/REPORT.md
# For P-0 specifically: the json_batch run is expected to show `count()` /
# `is_array()` etc. disappearing from the coverage table — that IS the
# correct signal, not a regression.

# 2i. Captured reference batches.
./tools/capture-fixtures.sh
diff -r "$BASELINE/captured-batches-main" tools/captured-batches/
# Raw binary diff is noisy (timestamps, pids vary); §4.5 #4 recommends a
# masked structural-diff helper to make this assertion clean. Until that
# tool exists, eyeball the decoded JSON from /debug/batches in the
# shipper_round_trip test as a sanity check.
```

#### What CI runs vs what the developer runs locally

| Stage | Steps | When |
|---|---|---|
| **CI (per-push)** | Step 1 (fmt + clippy + `cargo test --all`) + Step 2c (`recorder_observer`) + Step 2d (`shipper_round_trip`) | every push and PR |
| **Developer local (per-change)** | Step 0 (baseline) + Step 2a-2g | before opening the PR |
| **Developer local (when relevant)** | Step 2h (xdebug spot-check) + Step 2i (captured batches) | for P-0 specifically; optional for the others |
| **Operator (nightly / pre-release)** | Step 2e (FPM) | not currently in CI; consider promoting once it stabilises |

### 4.5 Recommended test additions (to land alongside the fixes)

The following tests don't exist today but each fix's confidence increases significantly if they do:

1. **A "snapshot capture count" test seam** — a `cfg(test)`-only `AtomicUsize` incremented inside `EntrySnapshots::capture_now` / `ExitSnapshots::capture_now`. Lets P-1's "no syscalls on drop" assertion become a true unit test, and gives every future change a way to assert "I didn't introduce a wasted snapshot."
2. **An `extract_call_site` call-count seam** — same shape, lets P-6's "no walk when slot is empty" become a unit test.
3. **A bench variant for the dropped-call path** in `recorder_hot_path.rs` — currently the bench uses `flush_records = usize::MAX` and oversized limits; add a sibling bench that configures `max_depth = 0` so every begin is dropped, exposing the P-1 win directly.
4. **A "captured-batches structural diff" helper** — a small Rust binary (or Python script) that loads two batch files, masks `meta.start_time` / `meta.pid` / `meta.host` / `meta.trace_id`, and diffs the remaining structure. This makes "the wire output didn't change" assertable across changes that go through any of the captured workloads.

None of these blocks shipping a fix — they're tooling investments that make the next round of changes safer.

---

## Specification Compliance

- ✅ The audited paths still meet `SPECIFICATION.md` §3.2 / §3.3 / §3.4 functional contracts. Nothing proposed above changes the wire schema, the lifecycle order, the dropped-records accounting, or the buffer cap discipline.
- ⚠️ **NFR-PERF-1 binding number needs to flip.** The original 2× target is jointly unachievable with per-call `PerCall` CPU resolution on the current syscall surface. Per the Section 2 posture decision, the budget is moving to R-1's 5× fallback. Update §11 row R-1 (status: Active) and §8.1 NFR-PERF-1 (binding number: 5×, with 2× retained as the long-term aspirational target). `benches/workload_overhead.rs::GEOMEAN_BUDGET` follows the spec — update both together or the bench will assert against a stale 2.0×.
- ✅ Operator default for `cpu_snapshot_mode` stays at `PerCall` per the posture decision. The directive surface grows in P-0 (new `skip_functions` + `skip_internal` directives) but doesn't change existing semantics.
- ❌ No spec-level NFR on binary size today. Recommend adding one (e.g. "shipped `.so` ≤ 5 MB on x86_64 release after `strip`") so the size discipline survives future changes.

---

## Overall Recommendation

**REQUEST CHANGES — but the changes are small and the order is forgiving.**

If you can ship only one thing this week, ship **S-1** (`debug = false` + `strip = "symbols"` in `[profile.release]`). It's three lines, lossless, and turns an 18 MB extension into a 3 MB one without anyone needing to think about it.

If you can ship two things, add **P-0** — an operator-controlled function-skip directive (`php_analyze.skip_functions = "strlen,count,is_array,..."` plus optionally `php_analyze.skip_internal = 0|1`). Returning `false` from `should_observe` for the listed functions makes them cost literally zero per call after the first sight — PHP caches the answer. Per the posture decision (PerCall stays, budget = 5×), this is the single largest practical lever, because every per-call optimisation everywhere else is bounded by the syscall floor of `getrusage` + `clock_gettime` + `zend_memory_usage`; P-0 deletes the call from observation entirely. Pair it with a sensible default skip list (5–10 high-frequency cheap builtins) so out-of-the-box overhead drops without the operator having to know.

If you can ship three things, add **P-1** — gate the depth/cap checks before `EntrySnapshots::capture_now()` and before the `dropped_begins > 0` shortcut. It costs one afternoon and gives back every syscall paid for a dropped call, with zero data loss on accepted calls.

After that, **S-3** (drop `url`) and **S-4** (delete `spike`) are the next bang-for-buck size levers, and **P-3 / P-5 / P-6** are quick per-call polish that compound. The Section 3 dead-code list (`Categorised`/`categorise` owning duplicates, `Dictionary::intern`, `RawCallSite::empty`, `FunctionKey::matches_ref`/`as_ref`) is a separate cleanup pass and not on the critical path for the budget.

Items deliberated and parked (full reasoning in `COMMENTS.md`):

- **S-2** — `panic = "abort"`: declined; would break shipper-thread panic isolation (C-20).
- **P-2** — `cpu_snapshot_mode = PerCall` posture: informational only; the per-call CPU signal is required for argument-dependent leaf nodes (C-19).
- **P-4** — `Cell<*mut Trace>` unsafe path: deferred until benches show the `RefCell` is next-dominant (C-21).
- **P-7 / P-8** — minor items: too rare or off the hot path to schedule (C-22).

**One spec change to land alongside this work:** `SPECIFICATION.md` §11 row R-1 currently lists the 5× fallback as a contingency. Per the posture decision in Section 2, the contingency is now active. Flip the R-1 row's status to "Active" and adjust §8.1 NFR-PERF-1's binding number from 2.0× to 5.0× (or leave 2.0× as the aspirational target and document 5.0× as the operating bound — your call). Without this, `workload_overhead.rs`'s `GEOMEAN_BUDGET` constant will assert against the old 2.0× and CI will fail on a passing run.

The codebase is in very good shape for the architectural review: the hot path is already careful about allocations (`intern_ref`, `categorise_lazy`, `LazyCategorised`, `FqnSpec::render_len`), the cap accounting is principled, the test seam exists, and the OpenSpec-per-change discipline is visible in the commit history. The findings above are honest cost-vs-benefit choices that the project has accumulated, not architectural debt — the spec's own R-1 risk row anticipated landing here.
