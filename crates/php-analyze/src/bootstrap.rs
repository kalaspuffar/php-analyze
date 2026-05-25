//! Bootstrap — PHP lifecycle hooks and `php.ini` directive registration.
//!
//! This module is the only file in the crate that touches `ext-php-rs`. It
//! owns the C-shaped boundary between Zend and the rest of the crate. The
//! hooks are deliberately minimal in this change:
//!
//! - **`MINIT`** registers every directive from `SPECIFICATION.md` §3.5 at
//!   `PHP_INI_SYSTEM` scope, reads the resolved values back via
//!   `ExecutorGlobals::ini_values()`, freezes them into [`Config`] through
//!   [`crate::config::initialise_from_ini`], and logs every returned
//!   warning at `E_WARNING`. It always returns success so PHP keeps
//!   starting (AD-4 / NFR-USE-2 silent-disable). When the resolved
//!   [`Config::enabled`] is `true`, it also installs the Phase-4
//!   shipper channel via [`crate::shipper::install_channel_at_minit`].
//! - **`MSHUTDOWN`** drains the Phase-4 shipper channel and joins the
//!   shipper thread (bounded by `Config::shutdown_grace + 200 ms`)
//!   when the extension is enabled. Disabled extensions take the
//!   no-op fast path so the silent-disable posture survives (R-10
//!   `mshutdown-respects-silent-disable` is satisfied here as a side
//!   benefit).
//! - **`RINIT` / `RSHUTDOWN`** short-circuit immediately when
//!   `Config::global().map_or(true, |c| !c.enabled)`. Observer registration
//!   is out of scope until the recorder change. `RINIT` additionally
//!   asks the shipper module to lazy-spawn its thread on the first
//!   per-process invocation (fork-safe per `SPECIFICATION.md` §3.4 /
//!   AD-4 / R-10).
//! - **`MINFO`** renders the resolved configuration. `auth_token` is
//!   *never* rendered from the [`secrecy::SecretString`] plaintext; the
//!   row is literally the string `"***"`. As belt-and-suspenders, the
//!   `auth_token` ini entry is registered with a `***` displayer so even
//!   PHP-internal paths (e.g. `display_ini_entries`) cannot leak it.

use std::collections::HashMap;
use std::ffi::CStr;
use std::sync::Arc;

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

// FFI-touching imports — gated to production builds because every
// symbol they reach (`php_error_docref`, `php_printf`, `zend_*`) is
// unresolvable at link time without a live PHP runtime. See
// [`startup_body`]'s doc comment.
#[cfg(not(test))]
use ext_php_rs::error::php_error;
#[cfg(not(test))]
use ext_php_rs::ffi::{php_printf, zend_ini_entry};
#[cfg(not(test))]
use ext_php_rs::flags::{ErrorType, IniEntryPermission};
#[cfg(not(test))]
use ext_php_rs::zend::{ExecutorGlobals, IniEntryDef};
use ext_php_rs::zend::{ModuleEntry, SapiGlobals, SapiModule};
use ext_php_rs::{info_table_end, info_table_header, info_table_row, info_table_start};

#[cfg(not(test))]
use crate::config::initialise_from_ini;
use crate::config::{Config, DisableReason, RawIni, TokenSource};
use crate::recorder::types::TraceLimits;
use crate::recorder::{self, RequestIdentity};
use crate::shipper;

// --- Directive table -------------------------------------------------------

struct Directive {
    /// Fully-qualified directive name as it appears in `php.ini`.
    name: &'static str,
    /// Default value, rendered as the operator would write it in `php.ini`.
    /// Empty string = "no default", which `Config::from_ini_values` then
    /// treats as "directive not set".
    default: &'static str,
    /// If `true`, register the directive with the `***` displayer so PHP's
    /// own rendering paths cannot leak the value. Only `auth_token`.
    /// `#[cfg_attr(test, allow(dead_code))]` because the only reader is
    /// the `#[cfg(not(test))]`-gated `register_directives`.
    #[cfg_attr(test, allow(dead_code))]
    redact_display: bool,
}

// The 13 directives from `SPECIFICATION.md` §3.5, in operator-facing
// order. The defaults here match the §3.5 table verbatim. The actual
// type-and-range coercion happens once values are read back into
// [`RawIni`] (see [`read_raw_ini`]); the per-directive logic lives there.
const DIRECTIVES: &[Directive] = &[
    Directive {
        name: "php_analyze.enabled",
        default: "1",
        redact_display: false,
    },
    Directive {
        name: "php_analyze.server_url",
        default: "",
        redact_display: false,
    },
    Directive {
        name: "php_analyze.auth_token",
        default: "",
        redact_display: true,
    },
    Directive {
        name: "php_analyze.auth_token_file",
        default: "",
        redact_display: false,
    },
    Directive {
        name: "php_analyze.flush_records",
        default: "10000",
        redact_display: false,
    },
    Directive {
        name: "php_analyze.flush_bytes",
        default: "1048576",
        redact_display: false,
    },
    Directive {
        name: "php_analyze.buffer_cap_bytes",
        default: "67108864",
        redact_display: false,
    },
    Directive {
        name: "php_analyze.max_depth",
        default: "1024",
        redact_display: false,
    },
    Directive {
        name: "php_analyze.retry_count",
        default: "3",
        redact_display: false,
    },
    Directive {
        name: "php_analyze.retry_backoff_ms",
        default: "100",
        redact_display: false,
    },
    Directive {
        name: "php_analyze.http_timeout_ms",
        default: "2000",
        redact_display: false,
    },
    Directive {
        name: "php_analyze.shutdown_grace_ms",
        default: "5000",
        redact_display: false,
    },
    Directive {
        name: "php_analyze.shipper_queue_depth",
        default: "8",
        redact_display: false,
    },
    // Per-call CPU snapshot policy (recorder-cpu-snapshot-cadence). The
    // default `per-call` preserves the spec-current behaviour (every
    // begin/end snapshot calls getrusage). Operators can set `off` for
    // high-volume pools where the per-call getrusage syscall is a
    // measurable share of recorder overhead; see COMMENTS.md C-19.
    Directive {
        name: "php_analyze.cpu_snapshot_mode",
        default: "per-call",
        redact_display: false,
    },
    // Spike-mode directives (Phase-0 `spike-zend-observer` change). Both
    // default to off so a production `php.ini` that does not mention them
    // exhibits no spike-mode behaviour. Removed in the same change that
    // lands Phase 2's Recorder.
    Directive {
        name: "php_analyze.spike_observer",
        default: "0",
        redact_display: false,
    },
    Directive {
        name: "php_analyze.spike_log_path",
        default: "",
        redact_display: false,
    },
];

// --- Lifecycle hooks -------------------------------------------------------

/// Test-only seam: when `true`, [`startup_body`] panics at the head
/// of its body *before* any FFI call. The matching
/// `startup_body_panic_is_contained_by_catch_unwind` and
/// `startup_returns_zero_on_panic` tests use this to force a panic
/// without standing up a live PHP runtime. Production builds never
/// see this static (the `#[cfg(test)]` strips both the slot and the
/// load inside [`startup_body`]).
#[cfg(test)]
static PANIC_IN_STARTUP_FOR_TEST: AtomicBool = AtomicBool::new(false);

/// `MINIT` — module startup. Wired into the `#[php_module]`-generated
/// startup via `#[php(startup = startup)]` in `lib.rs`.
///
/// The body runs inside `std::panic::catch_unwind` so that any panic
/// in [`register_directives`], [`read_raw_ini`], [`initialise_from_ini`],
/// the `php_error` warning loop, or [`install_shipper_if_enabled`] is
/// contained at the FFI frame instead of unwinding into Zend (undefined
/// behaviour / process abort on most builds). On a caught panic the
/// shim still returns `0` so PHP keeps starting and the extension
/// silent-disables — exactly the same posture every other lifecycle
/// hook in this module follows (`mshutdown`, `rinit`, `rshutdown`).
/// Matches `SPECIFICATION.md` §8.3 NFR-REL-1 ("never crash PHP") and
/// AD-4 (silent-disable on misconfig).
///
/// Returns `0` (Zend `SUCCESS`) unconditionally. The Zend convention
/// would normally let us signal startup failure via `-1`, but that
/// would also surface as an operator-visible PHP startup error — the
/// opposite of the silent-disable posture.
pub fn startup(_type_: i32, module_number: i32) -> i32 {
    // `module_number: i32` is `Copy + UnwindSafe`, so the closure is
    // `UnwindSafe` by construction; no `AssertUnwindSafe` needed.
    let _ = std::panic::catch_unwind(|| startup_body(module_number));
    0
}

/// Pure-Rust body of [`startup`]. Factored out so the `catch_unwind`
/// in the public shim captures every panic site downstream — including
/// FFI calls to `IniEntryDef::register`, `ExecutorGlobals::ini_values()`,
/// `php_error`, and the Phase-4 shipper-channel install. Returning `()`
/// keeps the closure `UnwindSafe` without needing `AssertUnwindSafe`.
///
/// ## Why the FFI body lives in a `#[cfg(not(test))]` sibling
///
/// The `cargo test` binary links without a live PHP runtime, so
/// every symbol the production body reaches
/// (`php_error_docref` via `php_error`, `php_printf` via the
/// `redact_displayer` address registered in `register_directives`,
/// `zend_hash_*` via `ExecutorGlobals::ini_values()`'s iterator) is
/// unresolvable at link time. Until this change the body was never
/// reachable from test code, so DCE eliminated those references
/// before link. The new unit tests
/// (`startup_body_panic_is_contained_by_catch_unwind` etc.) now
/// reference `startup_body`, which would re-introduce the
/// references and break the test link. The fix is to cfg-gate the
/// FFI body into [`startup_body_inner`], reachable only in
/// production. The integration tests in
/// `tests/recorder_observer.rs` and `tests/shipper_round_trip.rs`
/// exercise the inner end-to-end against a real PHP runtime.
fn startup_body(module_number: i32) {
    // Test seam: panic before any FFI call so the unit-test build
    // can exercise the panic-containment contract without
    // segfaulting on `register_directives` / `read_raw_ini` /
    // `php_error`.
    #[cfg(test)]
    if PANIC_IN_STARTUP_FOR_TEST.load(Ordering::Relaxed) {
        panic!("PANIC_IN_STARTUP_FOR_TEST: deliberate panic for unit test");
    }
    #[cfg(not(test))]
    startup_body_inner(module_number);
    #[cfg(test)]
    {
        // Touch the parameter under `cfg(test)` to silence the
        // unused-variable warning; in production the call to
        // `startup_body_inner` consumes it.
        let _ = module_number;
    }
}

/// FFI-touching half of [`startup_body`]. Production-only — the
/// `cargo test` binary cannot resolve the Zend symbols this reaches
/// (see `startup_body`'s doc comment). Behaviour is identical to the
/// pre-`bootstrap-startup-panic-safety` shape of `startup`'s body.
#[cfg(not(test))]
fn startup_body_inner(module_number: i32) {
    register_directives(module_number);

    let raw = read_raw_ini();
    let warnings = initialise_from_ini(raw);

    for warning in warnings {
        // `Display` for `ConfigWarning` renders a one-line, token-free
        // message; see the variant `#[error("…")]` attributes.
        php_error(&ErrorType::Warning, &warning.to_string());
    }

    // After the misconfig `E_WARNING` loop and before any shipper
    // side-effect, surface the deliberate `enabled = 0` case as one
    // `E_NOTICE` per process. Misconfig paths are already logged by
    // the warning loop above; this branch only fires for the
    // `MasterSwitchOff` `DisableReason`.
    emit_master_switch_notice_if_off(Config::global());

    // Install the Phase-4 shipper channel for enabled extensions only.
    // The disabled path stays silent — no channel, no thread, no
    // `MSHUTDOWN` work at process exit (the `shipper::*` slots remain
    // `None` / `false`).
    install_shipper_if_enabled(Config::global());
}

/// Pure helper: install the shipper channel iff the resolved config
/// is enabled. Factored out of [`startup`] so the silent-disable
/// posture (R-10) can be unit-tested against a hand-built
/// [`Config`] without touching the [`Config::global`] [`OnceLock`].
fn install_shipper_if_enabled(config: Option<&Config>) {
    let Some(config) = config else {
        return;
    };
    if !config.enabled {
        return;
    }
    shipper::install_channel_at_minit(config.shipper_queue_depth);
}

/// Pure helper: lazy-spawn the shipper thread iff the resolved config
/// is enabled. Factored out of [`rinit_body`] so the silent-disable
/// posture can be unit-tested without touching the
/// [`Config::global`] [`OnceLock`].
fn spawn_shipper_if_enabled(config: Option<&Config>) {
    let Some(config) = config else {
        return;
    };
    if !config.enabled {
        return;
    }
    shipper::spawn_if_needed_at_rinit(config);
}

/// `MSHUTDOWN` — module shutdown. No-op for this change; later changes
/// drain the shipper here.
///
/// # Safety
///
/// This function is the `C` ABI entry point called by Zend during module
/// shutdown. It must be invoked exactly once per module by the PHP
/// runtime, on the main thread, and not from Rust code. The body is
/// wrapped in `catch_unwind` so any panic from a future shipper-drain
/// path becomes a silent extension-disable rather than a process abort
/// (RO-1 / NFR-REL-1).
pub unsafe extern "C" fn mshutdown(_type_: i32, _module_number: i32) -> i32 {
    let _ = std::panic::catch_unwind(mshutdown_body);
    0
}

/// Pure-Rust body of [`mshutdown`]. Honours the silent-disable
/// posture: a disabled extension never installed a shipper channel,
/// so this short-circuits without touching the shipper module's
/// globals at all. R-10 (`mshutdown-respects-silent-disable`) is
/// satisfied by this guard as a side benefit of Phase 4 slice 1.
///
/// Delegates to [`drain_shipper_if_enabled`] so the per-config
/// branch is unit-testable against a hand-built [`Config`].
fn mshutdown_body() {
    drain_shipper_if_enabled(Config::global());
}

/// Pure helper: drain the shipper iff the resolved config is enabled.
/// The [`crate::shipper::JoinOutcome`] is discarded in slice 1:
/// a `Panicked` shipper is silently absorbed; a `Clean(_)` shipper's
/// counts are not surfaced yet. Slice 3 will introduce an `E_NOTICE`
/// log line here for the `Panicked` and abandoned-batches paths.
fn drain_shipper_if_enabled(config: Option<&Config>) {
    let Some(config) = config else {
        return;
    };
    if !config.enabled {
        return;
    }
    let _outcome = shipper::drain_and_join_at_mshutdown(config.shutdown_grace);
    // Drain any drop-notice lines queued by the shipper thread,
    // including those produced by the final-deadline drain that
    // `drain_and_join_at_mshutdown` just executed. Runs on the
    // main PHP thread so `php_error` is sound here.
    emit_queued_drop_notices();
}

/// Drain the shipper's `SPECIFICATION.md` §5.2 step-4 drop-notice
/// queue and feed each line through `php_error(E_NOTICE, ...)`.
///
/// Runs on the main PHP thread (from inside the `catch_unwind`
/// frames of [`rshutdown`] and [`mshutdown`]) so the Zend call is
/// sound. The shipper itself runs on a background OS thread and
/// MUST NOT call `php_error` directly — see the queue's module
/// comment in `crate::shipper`.
fn emit_queued_drop_notices() {
    for notice in shipper::drain_drop_notices() {
        emit_php_notice(&notice);
    }
}

/// Production builds dispatch through `ext_php_rs::error::php_error`
/// at `ErrorType::Notice`. The `cargo test` binary links without a
/// live PHP runtime, so `php_error_docref` (called transitively by
/// `php_error`) is unresolvable — the unit-test build replaces this
/// with a recorder (see the `#[cfg(test)]` body below) that captures
/// each message for assertion. Integration coverage of the wired-up
/// `php_error(E_NOTICE, ...)` call lives on
/// `tests/shipper_round_trip.rs`'s future retry-exhaust scenario
/// (deferred to the `stub-ingest-configurable-failure` follow-up,
/// per `COMMENTS.md` SEH-10). The same shim pattern is used by
/// `spike::emit_spike_log_warning`.
#[cfg(not(test))]
fn emit_php_notice(message: &str) {
    php_error(&ErrorType::Notice, message);
}

/// Test seam: append each notice message to a process-global
/// recorder so unit tests can assert on the emitted text. The
/// production path (`#[cfg(not(test))]` above) dispatches into
/// `php_error(E_NOTICE, ...)`, which is unresolvable at test-link
/// time. The recorder is drained via `drain_recorded_notices_for_test`
/// and reset via `reset_recorded_notices_for_test`. Tests that do
/// not inspect the recorder (e.g. the shipper drop-notice tests)
/// are unaffected: the recorder is a write-only sink from their
/// perspective.
#[cfg(test)]
fn emit_php_notice(message: &str) {
    tests::recorded_notices()
        .lock()
        .expect("notice recorder mutex poisoned (no other thread should panic while holding it)")
        .push(message.to_owned());
}

/// Surface the deliberate `php_analyze.enabled = 0` state as one
/// `E_NOTICE` per process. Only the `MasterSwitchOff` `DisableReason`
/// qualifies: every other `DisableReason` (missing `server_url`,
/// missing token, unreadable token file, …) already produces one
/// `E_WARNING` from the `ConfigWarning` loop in
/// `startup_body_inner`, so adding a notice would double-log those
/// cases. The enabled path stays silent.
///
/// The message is a fixed string — token-free by construction, and
/// deliberately decoupled from `DisableReason::human()` so a future
/// copy-edit of the `MINFO` rendering does not silently change the
/// error-log surface. See `notice-on-master-switch-off`'s `design.md`
/// D-2.
fn emit_master_switch_notice_if_off(config: Option<&Config>) {
    let Some(config) = config else {
        return;
    };
    if matches!(config.disable_reason, Some(DisableReason::MasterSwitchOff)) {
        emit_php_notice("php_analyze: disabled by php_analyze.enabled = 0");
    }
}

/// `RINIT` — request startup. Allocates the per-request `Trace` in the
/// recorder's thread-local slot when the extension is enabled AND the
/// spike is off. Skipped silently when the extension is disabled
/// (silent-disable posture) or when the spike is the active observer
/// (the spike doesn't need the recorder's trace).
///
/// The body runs inside `std::panic::catch_unwind` so any downstream
/// panic — an allocator-OOM in `Trace::new`, a future clock-syscall
/// failure, a programming error in a sub-slice — is contained at this
/// FFI frame instead of aborting the PHP process. The matching
/// `RSHUTDOWN` will then observe an empty slot and silently no-op,
/// honouring `SPECIFICATION.md` §8.3 NFR-REL-1 / AD-4 (RO-1).
///
/// # Safety
///
/// `C` ABI entry point called by Zend at the start of each PHP request.
/// Reads `Config::global()` which is set during `MINIT` and is
/// thereafter immutable for the lifetime of the process, so the read
/// requires no locking. The `SapiGlobals` / `SapiModule` reads acquire
/// short-lived `RwLock` guards via ext-php-rs.
pub unsafe extern "C" fn rinit(_type_: i32, _module_number: i32) -> i32 {
    let _ = std::panic::catch_unwind(rinit_body);
    0
}

/// Pure-Rust body of [`rinit`]. Factored out so the `catch_unwind`
/// frame in the FFI entry point captures every panic site downstream
/// — including the `Config::global()` read, identity construction,
/// and `Trace` allocation. Returning `()` keeps the closure
/// `UnwindSafe` without requiring `AssertUnwindSafe`.
fn rinit_body() {
    let Some(config) = Config::global() else {
        return;
    };
    if !config.enabled {
        return;
    }
    // Lazy-spawn the Phase-4 shipper thread on the first per-process
    // RINIT. Cheap CAS on the steady-state path. The spawn happens
    // even when the spike is the active observer — the channel was
    // installed in `startup` because the extension is enabled, and a
    // spike-only configuration still needs the channel torn down at
    // MSHUTDOWN. The shipper sits idle (no producer sends anything)
    // until slice 2 wires the Recorder.
    spawn_shipper_if_enabled(Some(config));
    if config.spike_observer {
        return;
    }
    let identity = build_request_identity();
    // Slice-3 cap thresholds and Phase-4-slice-2 flush thresholds, both
    // cached onto the `Trace` via [`TraceLimits::from(&Config)`] so the
    // hot path does not need to re-read `Config::global()` per call.
    // `Config::max_depth: u16` widens losslessly to `u32` inside the
    // impl so the gate's comparison with `Trace::virtual_depth: u32`
    // happens without a cast.
    let limits = TraceLimits::from(config);
    recorder::rinit_allocate_trace(identity, limits);
}

/// `RSHUTDOWN` — request shutdown. Releases the recorder's per-request
/// `Trace`. The recorder's helper is a no-op when the slot is empty,
/// so this is safe to call regardless of whether `RINIT` allocated.
///
/// As with [`rinit`], the body is wrapped in `catch_unwind` so any
/// panic — including a stale-trace drop that itself panics, or a
/// Phase-4 shipper-handoff failure — is contained inside this FFI
/// frame rather than aborting the PHP process (RO-1).
///
/// # Safety
///
/// `C` ABI entry point called by Zend at the end of each PHP request.
pub unsafe extern "C" fn rshutdown(_type_: i32, _module_number: i32) -> i32 {
    let _ = std::panic::catch_unwind(rshutdown_body);
    0
}

/// Pure-Rust body of [`rshutdown`]. Releases the recorder's
/// per-request `Trace` and drains any drop-notice lines queued by
/// the shipper thread since the previous request boundary. Both
/// steps run on the main PHP thread; `emit_queued_drop_notices`
/// requires this (the shipper background thread MUST NOT call
/// `php_error` itself).
///
/// If `recorder::rshutdown_release_trace` panics the outer
/// `catch_unwind` swallows it and the drop-notice drain is skipped
/// for this request — the queue is a `Mutex<VecDeque>` so the
/// notices remain visible to the next request's drain.
fn rshutdown_body() {
    recorder::rshutdown_release_trace();
    emit_queued_drop_notices();
}

/// Build a [`RequestIdentity`] from live SAPI state. Reads the SAPI
/// name, the request URI (FPM-family SAPIs) or argv-script (CLI),
/// the PID, and the host name. The PHP-touching reads are
/// short-scoped; the resulting strings are owned (so the
/// `RequestIdentity` can outlive the SAPI guards).
fn build_request_identity() -> RequestIdentity {
    let sapi_name = {
        let module = SapiModule::get();
        // `_sapi_module_struct::name` is a `*mut c_char` (bindgen-
        // exposed; ext-php-rs does not wrap it). On any valid SAPI the
        // pointer is non-null and points at a NUL-terminated ASCII
        // string ("cli", "fpm-fcgi", "apache2handler", …).
        if module.name.is_null() {
            String::new()
        } else {
            // SAFETY: pointer non-null, payload NUL-terminated and
            // valid for the duration of the read (the SAPI module
            // table is initialised at PHP startup and immutable
            // thereafter). Non-UTF-8 names fall through to an empty
            // string, which the helper handles.
            unsafe { CStr::from_ptr(module.name) }
                .to_str()
                .unwrap_or("")
                .to_owned()
        }
    };
    let (request_uri, argv0, path_translated) = {
        let globals = SapiGlobals::get();
        let info = globals.request_info();
        (
            info.request_uri().map(str::to_owned),
            info.argv0().map(str::to_owned),
            info.path_translated().map(str::to_owned),
        )
    };
    let host = read_hostname();
    request_identity_from_sapi(
        &sapi_name,
        request_uri.as_deref(),
        argv0.as_deref(),
        path_translated.as_deref(),
        host.as_deref(),
    )
}

/// Pure helper: assemble a [`RequestIdentity`] from string inputs and
/// the process PID. Factored out of [`build_request_identity`] so unit
/// tests can exercise the value-building without acquiring PHP locks.
///
/// `uri_or_script` is resolved by trying each of `request_uri`,
/// `argv0`, and `path_translated` in order — see
/// [`resolve_uri_or_script`] for the rationale. The final fallback
/// (`"(unknown-uri)"`) is preserved so the trace's `uri_or_script` is
/// never empty even on exotic SAPIs.
fn request_identity_from_sapi(
    sapi_name: &str,
    request_uri: Option<&str>,
    argv0: Option<&str>,
    path_translated: Option<&str>,
    hostname: Option<&str>,
) -> RequestIdentity {
    let host: Arc<str> = Arc::from(hostname.unwrap_or("(unknown-host)"));
    let sapi: Arc<str> = Arc::from(sapi_name);
    // Allocated once per request; the matching `Trace::uri_or_script`
    // field then carries the same `Arc<str>` for the trace's lifetime,
    // and `flush_into_pending_batch` clones it into every `MetaPartial`
    // it produces. See PF-1 in `COMMENTS.md`.
    let resolved = resolve_uri_or_script(request_uri, argv0, path_translated);
    let uri_or_script: Arc<str> = Arc::from(resolved);
    RequestIdentity {
        host,
        sapi,
        pid: std::process::id(),
        uri_or_script,
    }
}

/// Resolve `meta.uri_or_script` from the three SAPI-provided sources
/// in priority order:
///
/// 1. `request_uri` — populated under FPM and other web SAPIs.
/// 2. `argv0` — populated under PHP CLI in some PHP / ext-php-rs
///    versions.
/// 3. `path_translated` — the entry-script path PHP itself resolved
///    at startup. Reliably populated under PHP 8.3 / 8.4 CLI even
///    when `argv0` returns `None`.
/// 4. The literal `"(unknown-uri)"` placeholder — final fallback
///    for exotic SAPIs or genuinely-missing entry-script
///    information.
///
/// Split out as a pure free function so unit tests can drive each
/// fallback path independently without acquiring PHP locks.
fn resolve_uri_or_script<'a>(
    request_uri: Option<&'a str>,
    argv0: Option<&'a str>,
    path_translated: Option<&'a str>,
) -> &'a str {
    request_uri
        .or(argv0)
        .or(path_translated)
        .unwrap_or("(unknown-uri)")
}

/// Read the host name via `gethostname(3)`. Returns `None` on failure
/// (the syscall is essentially infallible on Linux, but we soft-fail
/// to keep the silent-disable posture intact). The returned value is
/// the UTF-8 view of the C string; non-UTF-8 host names fall back to
/// `None`.
fn read_hostname() -> Option<String> {
    let mut buf = vec![0u8; 256];
    // SAFETY: `gethostname` writes up to `buf.len()` bytes into `buf`
    // and NUL-terminates the result if it fits. A non-zero return means
    // failure (truncation, etc.). `buf` is a fresh, owned, properly-
    // aligned `Vec<u8>` so the pointer is valid for the call.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<i8>(), buf.len()) };
    if rc != 0 {
        return None;
    }
    // Find the NUL terminator the syscall wrote.
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf.truncate(len);
    String::from_utf8(buf).ok()
}

/// `MINFO` — `phpinfo()` rendering. Delegates to [`PhpInfoRenderer`],
/// which never accesses the [`secrecy::SecretString`] plaintext.
///
/// # Safety
///
/// `C` ABI entry point called by Zend when rendering `phpinfo()` /
/// `php --ri php_analyze`. The `_module` pointer is supplied by Zend
/// and not dereferenced here; the renderer reads only
/// [`Config::global`].
pub unsafe extern "C" fn minfo(_module: *mut ModuleEntry) {
    PhpInfoRenderer.render(Config::global());
}

// --- INI registration ------------------------------------------------------

/// FFI-only: registers each `php_analyze.*` directive with Zend.
/// `#[cfg(not(test))]` because the `cargo test` binary cannot resolve
/// the underlying `zend_register_ini_entries_ex` symbol — the same
/// reason [`startup_body_inner`] is cfg-gated. Test coverage for the
/// directive-name → `Config`-field mapping lives on
/// [`tests::raw_ini_from_ini_map_round_trips_non_default_directive_values`]
/// and friends, which never call this function.
#[cfg(not(test))]
fn register_directives(module_number: i32) {
    let entries: Vec<IniEntryDef> = DIRECTIVES
        .iter()
        .map(|d| {
            let mut entry = IniEntryDef::new(
                d.name.to_owned(),
                d.default.to_owned(),
                &IniEntryPermission::System,
            );
            if d.redact_display {
                entry.displayer = Some(redact_displayer);
            }
            entry
        })
        .collect();
    IniEntryDef::register(entries, module_number);
}

/// PHP-side displayer that prints the literal three characters `"***"` no
/// matter what value is registered. Wired into the `php_analyze.auth_token`
/// `IniEntryDef::displayer` so any PHP path that walks the ini entries
/// (e.g. `display_ini_entries`, `phpinfo()`'s configuration section)
/// cannot leak the bearer token.
///
/// # Safety
///
/// `C` ABI callback invoked by Zend's ini machinery. The `_entry`
/// pointer is supplied by Zend and not dereferenced; the body only
/// writes a static C string via `php_printf`.
#[cfg(not(test))]
unsafe extern "C" fn redact_displayer(_entry: *mut zend_ini_entry, _type_: i32) {
    // SAFETY: `c"***"` is a valid NUL-terminated C string with static
    // lifetime; `php_printf` does not retain the pointer after returning.
    unsafe {
        php_printf(c"***".as_ptr());
    }
}

// --- INI value read-back ---------------------------------------------------

/// PHP-side adapter: snapshot the live INI store and hand it to the pure
/// mapper. Kept as a single line so the testable surface area lives
/// entirely in [`raw_ini_from_ini_map`]. `#[cfg(not(test))]` because
/// `ExecutorGlobals::get().ini_values()` is unresolvable at link time
/// without a live PHP runtime — see [`startup_body`]'s doc comment.
#[cfg(not(test))]
fn read_raw_ini() -> RawIni {
    let ini = ExecutorGlobals::get().ini_values();
    raw_ini_from_ini_map(&ini)
}

/// Pure mapping from PHP's `ini_values()` snapshot (a
/// `HashMap<String, Option<String>>`) to the typed [`RawIni`]. Trims
/// whitespace, drops empty strings, parses booleans via [`parse_bool`],
/// and parses integers via `i64::from_str`. A directive whose name is
/// not in the map, whose value is `None`, or whose value is the empty
/// string is reported as `None` (the documented "directive not set"
/// state).
///
/// Factored out of [`read_raw_ini`] so unit tests can verify the
/// directive-name strings here match the registered names — a typo
/// would silently zero a field at runtime.
fn raw_ini_from_ini_map(ini: &HashMap<String, Option<String>>) -> RawIni {
    let lookup_str = |name: &str| -> Option<String> {
        ini.get(name)
            .cloned()
            .flatten()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    let lookup_int = |name: &str| -> Option<i64> { lookup_str(name).and_then(|s| s.parse().ok()) };
    let lookup_bool =
        |name: &str| -> Option<bool> { lookup_str(name).and_then(|s| parse_bool(&s)) };

    RawIni {
        enabled: lookup_bool("php_analyze.enabled"),
        server_url: lookup_str("php_analyze.server_url"),
        auth_token: lookup_str("php_analyze.auth_token"),
        auth_token_file: lookup_str("php_analyze.auth_token_file"),
        flush_records: lookup_int("php_analyze.flush_records"),
        flush_bytes: lookup_int("php_analyze.flush_bytes"),
        buffer_cap_bytes: lookup_int("php_analyze.buffer_cap_bytes"),
        max_depth: lookup_int("php_analyze.max_depth"),
        retry_count: lookup_int("php_analyze.retry_count"),
        retry_backoff_ms: lookup_int("php_analyze.retry_backoff_ms"),
        http_timeout_ms: lookup_int("php_analyze.http_timeout_ms"),
        shutdown_grace_ms: lookup_int("php_analyze.shutdown_grace_ms"),
        shipper_queue_depth: lookup_int("php_analyze.shipper_queue_depth"),
        cpu_snapshot_mode: lookup_str("php_analyze.cpu_snapshot_mode"),
        spike_observer: lookup_bool("php_analyze.spike_observer"),
        spike_log_path: lookup_str("php_analyze.spike_log_path"),
    }
}

/// Parse a PHP-ini boolean value. Matches Zend's own
/// `zend_ini_parse_bool` plus the textual forms operators commonly use.
fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" | "yes" => Some(true),
        "0" | "off" | "false" | "no" | "" => Some(false),
        _ => None,
    }
}

// --- MINFO renderer --------------------------------------------------------

/// `phpinfo()` renderer. A struct rather than a free function so that
/// every helper that prints a row has to go through the renderer, which
/// statically cannot access the `SecretString` plaintext (it borrows only
/// `&Config` and only ever writes the literal `"***"` for the token row).
///
/// The pure row production lives in [`rows`](Self::rows); [`render`](Self::render)
/// is the thin shim that hands each row to `info_table_row!`. Splitting
/// it this way lets tests exercise the token-redaction guarantee
/// without linking against PHP.
struct PhpInfoRenderer;

impl PhpInfoRenderer {
    fn render(&self, config: Option<&Config>) {
        info_table_start!();
        info_table_header!("php-analyze", env!("CARGO_PKG_VERSION"));
        for (label, value) in Self::rows(config) {
            info_table_row!(label.as_str(), value.as_str());
        }
        info_table_end!();
    }

    /// Produce the `(label, value)` rows the `phpinfo()` table will
    /// display. Pure; takes only `Option<&Config>`. The bearer token row
    /// is the literal string `"***"` — no code path here touches the
    /// `SecretString` plaintext, so leaking the token would require
    /// changing this function.
    fn rows(config: Option<&Config>) -> Vec<(String, String)> {
        let Some(c) = config else {
            return vec![("Status".to_owned(), "MINIT has not run".to_owned())];
        };
        let mut rows = Vec::with_capacity(17);
        rows.push(("Status".to_owned(), Self::status_row(c)));
        Self::push_directive_rows(c, &mut rows);
        Self::push_spike_rows(c, &mut rows);
        rows
    }

    fn status_row(c: &Config) -> String {
        if c.enabled {
            "enabled (true)".to_owned()
        } else {
            let reason = c
                .disable_reason
                .as_ref()
                .map_or("unknown", DisableReason::human);
            format!("enabled (false: {reason})")
        }
    }

    fn push_directive_rows(c: &Config, rows: &mut Vec<(String, String)>) {
        // For the master switch row, we want to reflect the operator's
        // intent (php_analyze.enabled = 0|1), not the effective enabled
        // state. The only way the operator's switch reads "off" is when
        // `disable_reason == MasterSwitchOff`.
        let master_on = !matches!(c.disable_reason, Some(DisableReason::MasterSwitchOff));
        rows.push((
            "php_analyze.enabled".to_owned(),
            if master_on { "On" } else { "Off" }.to_owned(),
        ));

        rows.push((
            "php_analyze.server_url".to_owned(),
            c.server_url.as_deref().unwrap_or("(unset)").to_owned(),
        ));
        // Hard-coded "***"; never touches `c.auth_token.expose_secret`.
        rows.push(("php_analyze.auth_token".to_owned(), "***".to_owned()));
        let token_file = match &c.auth_token_source {
            TokenSource::File(path) => path.display().to_string(),
            _ => "(unset)".to_owned(),
        };
        rows.push(("php_analyze.auth_token_file".to_owned(), token_file));

        rows.push((
            "php_analyze.flush_records".to_owned(),
            c.flush_records.to_string(),
        ));
        rows.push((
            "php_analyze.flush_bytes".to_owned(),
            c.flush_bytes.to_string(),
        ));
        rows.push((
            "php_analyze.buffer_cap_bytes".to_owned(),
            c.buffer_cap_bytes.to_string(),
        ));
        rows.push(("php_analyze.max_depth".to_owned(), c.max_depth.to_string()));
        rows.push((
            "php_analyze.retry_count".to_owned(),
            c.retry_count.to_string(),
        ));
        rows.push((
            "php_analyze.retry_backoff_ms".to_owned(),
            c.retry_backoff.as_millis().to_string(),
        ));
        rows.push((
            "php_analyze.http_timeout_ms".to_owned(),
            c.http_timeout.as_millis().to_string(),
        ));
        rows.push((
            "php_analyze.shutdown_grace_ms".to_owned(),
            c.shutdown_grace.as_millis().to_string(),
        ));
        rows.push((
            "php_analyze.shipper_queue_depth".to_owned(),
            c.shipper_queue_depth.to_string(),
        ));
        // Per-call CPU snapshot mode (recorder-cpu-snapshot-cadence).
        // Plain string row — no secret content; renders as `per-call`
        // or `off` matching the operator-facing directive vocabulary.
        rows.push((
            "php_analyze.cpu_snapshot_mode".to_owned(),
            c.cpu_snapshot_mode.as_ini_str().to_owned(),
        ));
    }

    /// Render the spike-mode directive rows. When the spike is off (the
    /// production default), we still surface the two directive values so
    /// operators can confirm they read what they wrote — but no banner.
    /// When the spike is on, a third row appears as a red-flag warning so
    /// the operator can spot it in a wall of `phpinfo()` output.
    fn push_spike_rows(c: &Config, rows: &mut Vec<(String, String)>) {
        rows.push((
            "php_analyze.spike_observer".to_owned(),
            if c.spike_observer { "On" } else { "Off" }.to_owned(),
        ));
        let path = c
            .spike_log_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(stderr)".to_owned());
        rows.push(("php_analyze.spike_log_path".to_owned(), path));
        if c.spike_observer {
            rows.push((
                "spike-mode".to_owned(),
                "ENABLED (DEVELOPMENT-ONLY; do not enable in production)".to_owned(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    //! These tests exercise the PHP-free helpers in this module:
    //! [`parse_bool`], [`raw_ini_from_ini_map`], and
    //! [`PhpInfoRenderer::rows`]. The lifecycle hooks themselves (and the
    //! `info_table_*!` macros they call) require a live PHP runtime to
    //! exercise meaningfully; integration coverage for those lives in
    //! the manual `php --ri` checks documented in
    //! `openspec/changes/scaffold-workspace-and-config/tasks.md` §9.

    use super::*;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    // --- emit_php_notice recorder ------------------------------------------
    //
    // The `#[cfg(test)]` `emit_php_notice` shim feeds every notice
    // through this recorder so the bootstrap tests below — and the
    // master-switch notice tests in particular — can assert on the
    // emitted text. The recorder lives behind a `Mutex` so concurrent
    // `cargo test` workers do not race when one of them logs a notice
    // while another is asserting. Tests that care about the contents
    // call `reset_recorded_notices_for_test` at entry; tests that
    // don't (e.g. the shipper drop-notice unit tests) are unaffected
    // by leftover messages because they never read the recorder.

    /// Process-global notice recorder used by the `#[cfg(test)]`
    /// `emit_php_notice` shim. Lazily initialised on first use.
    pub(super) fn recorded_notices() -> &'static Mutex<Vec<String>> {
        static RECORDER: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
        RECORDER.get_or_init(|| Mutex::new(Vec::new()))
    }

    /// Drain the notice recorder and return everything it has
    /// captured since the last drain or reset. Only used by the
    /// bootstrap unit tests in this module.
    fn drain_recorded_notices_for_test() -> Vec<String> {
        std::mem::take(
            &mut *recorded_notices()
                .lock()
                .expect("notice recorder mutex poisoned"),
        )
    }

    /// Clear the notice recorder. Call this at the start of any
    /// test that intends to assert on the captured notices, so a
    /// notice left over from another `cargo test` worker does not
    /// pollute the assertion.
    fn reset_recorded_notices_for_test() {
        recorded_notices()
            .lock()
            .expect("notice recorder mutex poisoned")
            .clear();
    }

    /// Serialise the `emit_master_switch_notice_if_off` tests so they
    /// do not race on the shared `recorded_notices` recorder. Held
    /// across the reset/emit/drain triple in each test, which makes
    /// the captured count deterministic under `cargo test`'s default
    /// multi-threaded runner. Other notice-producing test paths in
    /// the bootstrap suite do not fire `emit_php_notice` in practice
    /// (the shipper drop-notice path is exercised against an empty
    /// channel), so this lock is the only synchronisation point
    /// needed for the notice surface.
    fn acquire_notice_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // --- parse_bool ---------------------------------------------------------

    #[test]
    fn parse_bool_accepts_php_ini_truthy_forms() {
        for input in ["1", "on", "On", "ON", "true", "TRUE", "yes", "YES"] {
            assert_eq!(parse_bool(input), Some(true), "input={input:?}");
        }
    }

    #[test]
    fn parse_bool_accepts_php_ini_falsy_forms() {
        for input in ["0", "off", "Off", "OFF", "false", "FALSE", "no", "NO", ""] {
            assert_eq!(parse_bool(input), Some(false), "input={input:?}");
        }
    }

    #[test]
    fn parse_bool_trims_surrounding_whitespace() {
        assert_eq!(parse_bool("  yes  "), Some(true));
        assert_eq!(parse_bool("\toff\n"), Some(false));
    }

    #[test]
    fn parse_bool_returns_none_for_unknown_inputs() {
        for input in ["maybe", "2", "enabled", "y", "n"] {
            assert_eq!(parse_bool(input), None, "input={input:?}");
        }
    }

    // --- raw_ini_from_ini_map ----------------------------------------------

    fn ini_map_with_every_directive_at_its_declared_default() -> HashMap<String, Option<String>> {
        DIRECTIVES
            .iter()
            .map(|d| (d.name.to_owned(), Some(d.default.to_owned())))
            .collect()
    }

    #[test]
    fn raw_ini_from_ini_map_returns_none_for_unset_directives() {
        let ini = HashMap::new();
        let raw = raw_ini_from_ini_map(&ini);
        // Spot-check: nothing was set, so every field is None.
        assert_eq!(raw.enabled, None);
        assert_eq!(raw.server_url, None);
        assert_eq!(raw.auth_token, None);
        assert_eq!(raw.flush_records, None);
        assert_eq!(raw.shipper_queue_depth, None);
    }

    #[test]
    fn raw_ini_from_ini_map_treats_empty_string_as_unset() {
        let mut ini = HashMap::new();
        ini.insert("php_analyze.server_url".to_owned(), Some("".to_owned()));
        ini.insert("php_analyze.auth_token".to_owned(), Some("   ".to_owned()));
        let raw = raw_ini_from_ini_map(&ini);
        assert_eq!(raw.server_url, None);
        assert_eq!(raw.auth_token, None);
    }

    #[test]
    fn raw_ini_from_ini_map_round_trips_non_default_directive_values() {
        // Every directive set to a value distinct from its default;
        // ensures the directive-name strings in raw_ini_from_ini_map
        // exactly match the names registered in DIRECTIVES. A typo
        // would leave the corresponding field None and trip the
        // assertion below.
        let mut ini: HashMap<String, Option<String>> = HashMap::new();
        ini.insert(
            "php_analyze.enabled".to_owned(),
            Some("0".to_owned()), // != default "1"
        );
        ini.insert(
            "php_analyze.server_url".to_owned(),
            Some("https://example.test/ingest".to_owned()),
        );
        ini.insert(
            "php_analyze.auth_token".to_owned(),
            Some("test-token".to_owned()),
        );
        ini.insert(
            "php_analyze.auth_token_file".to_owned(),
            Some("/etc/test/token".to_owned()),
        );
        ini.insert("php_analyze.flush_records".to_owned(), Some("7".to_owned()));
        ini.insert(
            "php_analyze.flush_bytes".to_owned(),
            Some("2048".to_owned()),
        );
        ini.insert(
            "php_analyze.buffer_cap_bytes".to_owned(),
            Some("1234567".to_owned()),
        );
        ini.insert("php_analyze.max_depth".to_owned(), Some("12".to_owned()));
        ini.insert("php_analyze.retry_count".to_owned(), Some("5".to_owned()));
        ini.insert(
            "php_analyze.retry_backoff_ms".to_owned(),
            Some("250".to_owned()),
        );
        ini.insert(
            "php_analyze.http_timeout_ms".to_owned(),
            Some("4321".to_owned()),
        );
        ini.insert(
            "php_analyze.shutdown_grace_ms".to_owned(),
            Some("9001".to_owned()),
        );
        ini.insert(
            "php_analyze.shipper_queue_depth".to_owned(),
            Some("17".to_owned()),
        );

        let raw = raw_ini_from_ini_map(&ini);
        assert_eq!(raw.enabled, Some(false));
        assert_eq!(
            raw.server_url.as_deref(),
            Some("https://example.test/ingest")
        );
        assert_eq!(raw.auth_token.as_deref(), Some("test-token"));
        assert_eq!(raw.auth_token_file.as_deref(), Some("/etc/test/token"));
        assert_eq!(raw.flush_records, Some(7));
        assert_eq!(raw.flush_bytes, Some(2048));
        assert_eq!(raw.buffer_cap_bytes, Some(1_234_567));
        assert_eq!(raw.max_depth, Some(12));
        assert_eq!(raw.retry_count, Some(5));
        assert_eq!(raw.retry_backoff_ms, Some(250));
        assert_eq!(raw.http_timeout_ms, Some(4_321));
        assert_eq!(raw.shutdown_grace_ms, Some(9_001));
        assert_eq!(raw.shipper_queue_depth, Some(17));
    }

    // --- DIRECTIVES <-> Config defaults parity -----------------------------

    #[test]
    fn directive_table_numeric_defaults_match_resolved_config_defaults() {
        // Drift guard: the string defaults in DIRECTIVES (the values
        // Zend stores as each directive's registered default) must
        // resolve to the same Config values that
        // Config::from_ini_values(&RawIni::default()) produces for the
        // numeric fields. If anyone bumps a default in one place and
        // forgets the other, this test fires.

        // RawIni::default() leaves enabled=None → unwrap_or(true) →
        // proceeds to validation; server_url & auth_token are unset
        // → disable_reason = ServerUrlNotConfigured. The numeric
        // fields are still populated from defaults.
        let (resolved, _warnings) = Config::from_ini_values(&RawIni::default());

        let directive = |name: &str| -> &Directive {
            DIRECTIVES
                .iter()
                .find(|d| d.name == name)
                .unwrap_or_else(|| panic!("DIRECTIVES missing entry for {name}"))
        };

        // Master switch default ("1") matches the parse-bool behaviour:
        // boot proceeds into the rest of validation.
        assert_eq!(directive("php_analyze.enabled").default, "1");
        assert_eq!(parse_bool("1"), Some(true));

        // Numeric directives: parse the string default and compare to
        // the resolved value, accounting for the type cast each field
        // undergoes inside `from_ini_values`.
        let parse_int = |name: &str| -> i64 {
            directive(name)
                .default
                .parse::<i64>()
                .unwrap_or_else(|_| panic!("default for {name} must parse as i64"))
        };
        assert_eq!(
            parse_int("php_analyze.flush_records") as usize,
            resolved.flush_records
        );
        assert_eq!(
            parse_int("php_analyze.flush_bytes") as usize,
            resolved.flush_bytes
        );
        assert_eq!(
            parse_int("php_analyze.buffer_cap_bytes") as usize,
            resolved.buffer_cap_bytes
        );
        assert_eq!(
            parse_int("php_analyze.max_depth") as u16,
            resolved.max_depth
        );
        assert_eq!(
            parse_int("php_analyze.retry_count") as u8,
            resolved.retry_count
        );
        assert_eq!(
            resolved.retry_backoff,
            Duration::from_millis(parse_int("php_analyze.retry_backoff_ms") as u64)
        );
        assert_eq!(
            resolved.http_timeout,
            Duration::from_millis(parse_int("php_analyze.http_timeout_ms") as u64)
        );
        assert_eq!(
            resolved.shutdown_grace,
            Duration::from_millis(parse_int("php_analyze.shutdown_grace_ms") as u64)
        );
        assert_eq!(
            parse_int("php_analyze.shipper_queue_depth") as usize,
            resolved.shipper_queue_depth
        );

        // Token-related defaults must remain empty (= "not set"). A
        // shipped default token would be a textbook footgun.
        assert_eq!(directive("php_analyze.server_url").default, "");
        assert_eq!(directive("php_analyze.auth_token").default, "");
        assert_eq!(directive("php_analyze.auth_token_file").default, "");
    }

    #[test]
    fn registered_default_snapshot_resolves_to_baseline_config() {
        // Another angle on the parity guarantee: feed the directive
        // table back through raw_ini_from_ini_map and assert the
        // resolved Config matches the RawIni::default() baseline
        // field-by-field. Catches both "default-string drift" and
        // "directive-name typo in raw_ini_from_ini_map".
        let ini = ini_map_with_every_directive_at_its_declared_default();
        let from_table = raw_ini_from_ini_map(&ini);
        let (from_table_cfg, from_table_warnings) = Config::from_ini_values(&from_table);
        let (baseline_cfg, baseline_warnings) = Config::from_ini_values(&RawIni::default());

        assert_eq!(from_table_cfg.enabled, baseline_cfg.enabled);
        assert_eq!(from_table_cfg.disable_reason, baseline_cfg.disable_reason);
        assert_eq!(from_table_cfg.flush_records, baseline_cfg.flush_records);
        assert_eq!(from_table_cfg.flush_bytes, baseline_cfg.flush_bytes);
        assert_eq!(
            from_table_cfg.buffer_cap_bytes,
            baseline_cfg.buffer_cap_bytes
        );
        assert_eq!(from_table_cfg.max_depth, baseline_cfg.max_depth);
        assert_eq!(from_table_cfg.retry_count, baseline_cfg.retry_count);
        assert_eq!(from_table_cfg.retry_backoff, baseline_cfg.retry_backoff);
        assert_eq!(from_table_cfg.http_timeout, baseline_cfg.http_timeout);
        assert_eq!(from_table_cfg.shutdown_grace, baseline_cfg.shutdown_grace);
        assert_eq!(
            from_table_cfg.shipper_queue_depth,
            baseline_cfg.shipper_queue_depth
        );
        // Both paths arrive at the same disable_reason and therefore
        // emit the same warning set.
        assert_eq!(from_table_warnings, baseline_warnings);
    }

    // --- PhpInfoRenderer::rows ---------------------------------------------

    fn config_with_token(token: &str) -> Config {
        let raw = RawIni {
            enabled: Some(true),
            server_url: Some("https://ingest.example.com/v1/ingest".to_owned()),
            auth_token: Some(token.to_owned()),
            ..RawIni::default()
        };
        Config::from_ini_values(&raw).0
    }

    #[test]
    fn rows_redact_the_auth_token_and_never_leak_plaintext() {
        let token = "sk_live_unique_marker_abc123";
        let config = config_with_token(token);
        let rows = PhpInfoRenderer::rows(Some(&config));

        // No row anywhere may contain the bearer token plaintext.
        for (label, value) in &rows {
            assert!(
                !label.contains(token),
                "token leaked into row label: {label:?}"
            );
            assert!(
                !value.contains(token),
                "token leaked into row value: ({label:?}, {value:?})"
            );
        }

        // The auth_token row must read exactly "***".
        let token_row = rows
            .iter()
            .find(|(label, _)| label == "php_analyze.auth_token")
            .expect("php_analyze.auth_token row");
        assert_eq!(token_row.1, "***");
    }

    #[test]
    fn rows_with_no_config_report_minit_has_not_run() {
        let rows = PhpInfoRenderer::rows(None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "Status");
        assert_eq!(rows[0].1, "MINIT has not run");
    }

    #[test]
    fn rows_render_disable_reason_in_status_line() {
        // Missing server_url ⇒ silent-disable with that reason. The
        // status row must reflect it verbatim (the operator looks for
        // exactly this string).
        let raw = RawIni {
            enabled: Some(true),
            server_url: None,
            auth_token: Some("present".to_owned()),
            ..RawIni::default()
        };
        let (config, _warnings) = Config::from_ini_values(&raw);
        let rows = PhpInfoRenderer::rows(Some(&config));
        let status = rows.iter().find(|(l, _)| l == "Status").expect("status");
        assert_eq!(status.1, "enabled (false: server_url not configured)");

        // The php_analyze.enabled row reflects the operator's intent
        // (the master switch is ON); only the resolved Status row
        // shows "false".
        let master = rows
            .iter()
            .find(|(l, _)| l == "php_analyze.enabled")
            .expect("master switch row");
        assert_eq!(master.1, "On");
    }

    #[test]
    fn rows_render_master_switch_off_when_operator_disabled_extension() {
        let raw = RawIni {
            enabled: Some(false),
            ..RawIni::default()
        };
        let (config, _warnings) = Config::from_ini_values(&raw);
        let rows = PhpInfoRenderer::rows(Some(&config));
        let master = rows
            .iter()
            .find(|(l, _)| l == "php_analyze.enabled")
            .expect("master switch row");
        assert_eq!(master.1, "Off");
        let status = rows.iter().find(|(l, _)| l == "Status").expect("status");
        assert_eq!(status.1, "enabled (false: php_analyze.enabled = 0)");
    }

    #[test]
    fn rows_include_every_directive_exactly_once() {
        let raw = RawIni {
            enabled: Some(true),
            server_url: Some("https://ingest.example.com/v1/ingest".to_owned()),
            auth_token: Some("token".to_owned()),
            ..RawIni::default()
        };
        let (config, _warnings) = Config::from_ini_values(&raw);
        let rows = PhpInfoRenderer::rows(Some(&config));
        for d in DIRECTIVES {
            let count = rows.iter().filter(|(label, _)| label == d.name).count();
            assert_eq!(
                count, 1,
                "directive {} appears {} times in rows",
                d.name, count
            );
        }
    }

    // --- spike-mode rendering ----------------------------------------------

    fn enabled_config_with_spike(observer_on: bool, path: Option<&str>) -> Config {
        let raw = RawIni {
            enabled: Some(true),
            server_url: Some("https://ingest.example.com/v1/ingest".to_owned()),
            auth_token: Some("token".to_owned()),
            spike_observer: Some(observer_on),
            spike_log_path: path.map(str::to_owned),
            ..RawIni::default()
        };
        Config::from_ini_values(&raw).0
    }

    #[test]
    fn rows_render_cpu_snapshot_mode_as_plain_string_per_call_default() {
        // The mode directive is purely operator-facing — no secret
        // content, no banner. The phpinfo row mirrors the operator's
        // own `php.ini` value vocabulary so they can confirm the
        // resolved mode by name.
        let config = enabled_config_with_spike(false, None);
        let rows = PhpInfoRenderer::rows(Some(&config));
        let row = rows
            .iter()
            .find(|(l, _)| l == "php_analyze.cpu_snapshot_mode")
            .expect("cpu_snapshot_mode row");
        assert_eq!(row.1, "per-call");
        // The token redaction marker MUST NOT appear in this row.
        assert!(
            !row.1.contains("***"),
            "cpu_snapshot_mode row must be plain string, not redacted: {row:?}"
        );
    }

    #[test]
    fn rows_render_cpu_snapshot_mode_off_when_directive_resolves_to_off() {
        let raw = RawIni {
            enabled: Some(true),
            server_url: Some("https://ingest.example.com/v1/ingest".to_owned()),
            auth_token: Some("token".to_owned()),
            cpu_snapshot_mode: Some("off".to_owned()),
            ..RawIni::default()
        };
        let config = Config::from_ini_values(&raw).0;
        let rows = PhpInfoRenderer::rows(Some(&config));
        let row = rows
            .iter()
            .find(|(l, _)| l == "php_analyze.cpu_snapshot_mode")
            .expect("cpu_snapshot_mode row");
        assert_eq!(row.1, "off");
    }

    #[test]
    fn rows_hide_the_spike_mode_banner_when_spike_observer_is_off() {
        let config = enabled_config_with_spike(false, None);
        let rows = PhpInfoRenderer::rows(Some(&config));
        // The two directive rows are always present (operators want to
        // confirm the resolved value of what they wrote).
        let observer = rows
            .iter()
            .find(|(l, _)| l == "php_analyze.spike_observer")
            .expect("spike_observer row");
        assert_eq!(observer.1, "Off");
        let path = rows
            .iter()
            .find(|(l, _)| l == "php_analyze.spike_log_path")
            .expect("spike_log_path row");
        assert_eq!(path.1, "(stderr)");
        // But the red-flag banner is absent.
        assert!(
            !rows.iter().any(|(l, _)| l == "spike-mode"),
            "spike-mode banner must NOT appear when spike is off"
        );
    }

    #[test]
    fn rows_show_the_spike_mode_banner_when_spike_observer_is_on() {
        let config = enabled_config_with_spike(true, Some("/tmp/spike.log"));
        let rows = PhpInfoRenderer::rows(Some(&config));
        let observer = rows
            .iter()
            .find(|(l, _)| l == "php_analyze.spike_observer")
            .expect("spike_observer row");
        assert_eq!(observer.1, "On");
        let path = rows
            .iter()
            .find(|(l, _)| l == "php_analyze.spike_log_path")
            .expect("spike_log_path row");
        assert_eq!(path.1, "/tmp/spike.log");
        let banner = rows
            .iter()
            .find(|(l, _)| l == "spike-mode")
            .expect("spike-mode banner");
        assert_eq!(
            banner.1,
            "ENABLED (DEVELOPMENT-ONLY; do not enable in production)"
        );
    }

    // --- resolve_uri_or_script (the pure fallback chain) -----------------

    #[test]
    fn resolve_uri_or_script_prefers_request_uri_when_present() {
        let resolved = resolve_uri_or_script(
            Some("/api/v1/users?page=2"),
            Some("/usr/local/bin/my-script.php"),
            Some("/var/www/index.php"),
        );
        assert_eq!(resolved, "/api/v1/users?page=2");
    }

    #[test]
    fn resolve_uri_or_script_prefers_argv0_over_path_translated() {
        // Regression guard: under CLI with both `argv0` and
        // `path_translated` populated, the chain SHALL keep using
        // `argv0` (preserves today's CLI-with-argv0 behaviour).
        let resolved = resolve_uri_or_script(
            None,
            Some("/usr/local/bin/php-script.php"),
            Some("/different/path.php"),
        );
        assert_eq!(resolved, "/usr/local/bin/php-script.php");
    }

    #[test]
    fn resolve_uri_or_script_falls_back_to_path_translated_under_cli_8_4() {
        // The defect this change exists to fix: under PHP 8.4 CLI,
        // `request_uri` and `argv0` both return `None`, but
        // `path_translated` carries the script path.
        let resolved = resolve_uri_or_script(None, None, Some("/usr/local/bin/my-script.php"));
        assert_eq!(resolved, "/usr/local/bin/my-script.php");
    }

    #[test]
    fn resolve_uri_or_script_uses_the_placeholder_when_every_source_is_missing() {
        let resolved = resolve_uri_or_script(None, None, None);
        assert_eq!(resolved, "(unknown-uri)");
    }

    // --- request_identity_from_sapi ---------------------------------------

    #[test]
    fn request_identity_from_sapi_uses_request_uri_under_fpm_fcgi() {
        let id = request_identity_from_sapi(
            "fpm-fcgi",
            Some("/api/v1/users?page=2"),
            None,
            None,
            Some("worker-7.prod"),
        );
        assert_eq!(&*id.sapi, "fpm-fcgi");
        assert_eq!(&*id.uri_or_script, "/api/v1/users?page=2");
        assert_eq!(&*id.host, "worker-7.prod");
        assert_eq!(id.pid, std::process::id());
    }

    #[test]
    fn request_identity_from_sapi_falls_back_to_argv_under_cli() {
        // Under CLI with argv0 populated (the historical good case),
        // the resolver picks argv0 over path_translated.
        let id = request_identity_from_sapi(
            "cli",
            None,
            Some("/usr/local/bin/my-script.php"),
            Some("/should/not/win.php"),
            Some("dev-laptop"),
        );
        assert_eq!(&*id.sapi, "cli");
        assert_eq!(&*id.uri_or_script, "/usr/local/bin/my-script.php");
    }

    #[test]
    fn request_identity_from_sapi_falls_back_to_path_translated_under_cli_when_argv0_is_none() {
        // The PHP-8.4-CLI case this change exists to fix.
        let id = request_identity_from_sapi(
            "cli",
            None,
            None,
            Some("/usr/local/bin/php-8-4-script.php"),
            Some("dev-laptop"),
        );
        assert_eq!(&*id.uri_or_script, "/usr/local/bin/php-8-4-script.php");
    }

    #[test]
    fn request_identity_from_sapi_uses_placeholders_when_inputs_are_missing() {
        // Defensive: a non-CLI/non-FPM SAPI with no request URI must
        // still produce a usable RequestIdentity (no panic, no empty
        // strings the wire format would have to special-case later).
        let id = request_identity_from_sapi("apache2handler", None, None, None, None);
        assert_eq!(&*id.sapi, "apache2handler");
        assert_eq!(&*id.uri_or_script, "(unknown-uri)");
        assert_eq!(&*id.host, "(unknown-host)");
    }

    #[test]
    fn request_identity_carries_the_current_pid_and_a_non_empty_host() {
        // The host name read here is whatever `gethostname` returns
        // on the test runner; we only assert it's populated.
        let host = read_hostname();
        let id = request_identity_from_sapi("cli", Some("/x.php"), None, None, host.as_deref());
        assert_eq!(id.pid, std::process::id());
        assert!(!id.host.is_empty(), "host must be non-empty");
    }

    #[test]
    fn read_hostname_returns_a_non_empty_string_on_linux() {
        // `gethostname` is effectively infallible on Linux x86_64.
        // We don't assert a specific value (CI hosts vary), but the
        // contract is "Some non-empty string".
        let host = read_hostname().expect("gethostname succeeds on Linux x86_64");
        assert!(!host.is_empty(), "host must be non-empty");
    }

    #[test]
    fn rows_redact_auth_token_even_when_spike_is_enabled() {
        // R-3 from the Phase-0 review pattern: the spike module is the
        // newest entry in the crate; future readers must be reassured
        // that turning the spike on does not somehow weaken the token
        // redaction guarantee.
        let token = "spike-not-secret-zzz";
        let raw = RawIni {
            enabled: Some(true),
            server_url: Some("https://ingest.example.com/v1/ingest".to_owned()),
            auth_token: Some(token.to_owned()),
            spike_observer: Some(true),
            spike_log_path: Some("/tmp/spike.log".to_owned()),
            ..RawIni::default()
        };
        let config = Config::from_ini_values(&raw).0;
        let rows = PhpInfoRenderer::rows(Some(&config));
        for (label, value) in &rows {
            assert!(!label.contains(token), "token leaked into label: {label:?}");
            assert!(
                !value.contains(token),
                "token leaked into value: ({label:?}, {value:?})"
            );
        }
        let token_row = rows
            .iter()
            .find(|(l, _)| l == "php_analyze.auth_token")
            .expect("auth_token row");
        assert_eq!(token_row.1, "***");
    }

    // --- shipper lifecycle hooks -------------------------------------------

    /// Helper: build a fully-enabled `Config` for the lifecycle
    /// tests below. Server URL and token are valid so the resolved
    /// `enabled` is `true`.
    fn enabled_config_for_lifecycle_tests() -> Config {
        let raw = RawIni {
            enabled: Some(true),
            server_url: Some("https://ingest.example.com/v1/ingest".to_owned()),
            auth_token: Some("test-token".to_owned()),
            ..RawIni::default()
        };
        Config::from_ini_values(&raw).0
    }

    /// Helper: build a `Config` whose `enabled` is `false` for the
    /// silent-disable lifecycle tests. Uses the master-switch path
    /// so no warnings fire (slice-1 R-5 fix-up).
    fn disabled_config_for_lifecycle_tests() -> Config {
        let raw = RawIni {
            enabled: Some(false),
            ..RawIni::default()
        };
        Config::from_ini_values(&raw).0
    }

    #[test]
    fn bootstrap_startup_with_disabled_config_does_not_install_the_channel() {
        // R-10 / NFR-USE-2: a disabled extension must not even
        // construct the shipper channel. The `Config { enabled: false,
        // .. }` short-circuit in `install_shipper_if_enabled` is the
        // sole guard.
        let _guard = crate::shipper::acquire_test_lock();
        crate::shipper::reset_for_test();
        let disabled = disabled_config_for_lifecycle_tests();
        assert!(!disabled.enabled, "disabled fixture sanity");
        install_shipper_if_enabled(Some(&disabled));
        assert!(
            !crate::shipper::sender_is_installed(),
            "disabled startup must not install the sender"
        );
        assert!(
            !crate::shipper::receiver_is_installed(),
            "disabled startup must not stash a receiver"
        );
        crate::shipper::reset_for_test();
    }

    #[test]
    fn bootstrap_startup_with_enabled_config_installs_the_channel() {
        // Companion to the disabled test: the enabled path actually
        // installs the channel at the configured depth.
        let _guard = crate::shipper::acquire_test_lock();
        crate::shipper::reset_for_test();
        let enabled = enabled_config_for_lifecycle_tests();
        assert!(enabled.enabled, "enabled fixture sanity");
        install_shipper_if_enabled(Some(&enabled));
        assert!(crate::shipper::sender_is_installed());
        assert!(crate::shipper::receiver_is_installed());
        crate::shipper::reset_for_test();
    }

    #[test]
    fn bootstrap_mshutdown_with_disabled_config_is_a_noop() {
        // R-10 explicitly: mshutdown on a disabled extension must
        // not even attempt to drain a non-existent channel. The
        // shipper module's drain function would itself return
        // `NotInstalled` defensively, but the bootstrap-layer
        // `!config.enabled` guard is the load-bearing one for the
        // silent-disable posture.
        let _guard = crate::shipper::acquire_test_lock();
        crate::shipper::reset_for_test();
        let disabled = disabled_config_for_lifecycle_tests();
        // Pre-condition: no shipper state is installed. Calling
        // drain on a disabled config must keep it that way.
        assert!(!crate::shipper::sender_is_installed());
        drain_shipper_if_enabled(Some(&disabled));
        assert!(!crate::shipper::sender_is_installed());
        assert!(!crate::shipper::handle_is_installed());
        assert!(!crate::shipper::spawned_flag());
        crate::shipper::reset_for_test();
    }

    #[test]
    fn bootstrap_full_lifecycle_with_disabled_config_keeps_shipper_globals_empty() {
        // Modified `extension-bootstrap` scenario: "Disabled
        // extension spawns no background threads". Drives all three
        // lifecycle helpers (`install`, `spawn`, `drain`) against a
        // disabled config and asserts every shipper slot stays
        // empty throughout.
        let _guard = crate::shipper::acquire_test_lock();
        crate::shipper::reset_for_test();
        let disabled = disabled_config_for_lifecycle_tests();

        install_shipper_if_enabled(Some(&disabled));
        assert!(!crate::shipper::sender_is_installed(), "after install");
        assert!(!crate::shipper::receiver_is_installed(), "after install");
        assert!(!crate::shipper::spawned_flag(), "after install");
        assert!(!crate::shipper::handle_is_installed(), "after install");

        spawn_shipper_if_enabled(Some(&disabled));
        assert!(!crate::shipper::sender_is_installed(), "after spawn");
        assert!(!crate::shipper::receiver_is_installed(), "after spawn");
        assert!(!crate::shipper::spawned_flag(), "after spawn");
        assert!(!crate::shipper::handle_is_installed(), "after spawn");

        drain_shipper_if_enabled(Some(&disabled));
        assert!(!crate::shipper::sender_is_installed(), "after drain");
        assert!(!crate::shipper::receiver_is_installed(), "after drain");
        assert!(!crate::shipper::spawned_flag(), "after drain");
        assert!(!crate::shipper::handle_is_installed(), "after drain");

        crate::shipper::reset_for_test();
    }

    #[test]
    fn bootstrap_full_lifecycle_with_enabled_config_installs_spawns_and_drains() {
        // Companion to the disabled-lifecycle test: the enabled
        // path drives all three steps end-to-end and verifies the
        // shipper thread starts and joins cleanly. This is the
        // closest-to-PHP-free integration test for the §7 wiring.
        let _guard = crate::shipper::acquire_test_lock();
        crate::shipper::reset_for_test();
        let enabled = enabled_config_for_lifecycle_tests();

        install_shipper_if_enabled(Some(&enabled));
        assert!(crate::shipper::sender_is_installed());
        assert!(crate::shipper::receiver_is_installed());

        spawn_shipper_if_enabled(Some(&enabled));
        assert!(crate::shipper::spawned_flag());
        assert!(crate::shipper::handle_is_installed());

        drain_shipper_if_enabled(Some(&enabled));
        // After drain, the slots are taken-and-dropped; the
        // shipper has joined.
        assert!(!crate::shipper::sender_is_installed());
        assert!(!crate::shipper::handle_is_installed());

        crate::shipper::reset_for_test();
    }

    #[test]
    fn trace_limits_from_resolved_config_carries_flush_thresholds() {
        // Phase-4 slice 2 §8.3: the bootstrap-layer wiring uses
        // `TraceLimits::from(&Config)` so the two new
        // `flush_records` / `flush_bytes` directives land on the
        // trace exactly as configured. This test pins the contract
        // at the bootstrap surface so a future regression in
        // `rinit_body` (e.g. a hand-rolled `TraceLimits { .. }`
        // construction that misses the new fields) is caught here,
        // not in a fixture chase.
        let raw = RawIni {
            enabled: Some(true),
            server_url: Some("https://ingest.example.com/v1/ingest".to_owned()),
            auth_token: Some("test-token".to_owned()),
            flush_records: Some(2_500),
            flush_bytes: Some(131_072),
            ..RawIni::default()
        };
        let (config, _warnings) = Config::from_ini_values(&raw);
        let limits = TraceLimits::from(&config);
        assert_eq!(limits.flush_records, 2_500);
        assert_eq!(limits.flush_bytes, 131_072);
        // Tap the slice-3 fields too so a future widening of
        // `TraceLimits` is caught by the same test.
        assert_eq!(limits.max_depth, u32::from(config.max_depth));
        assert_eq!(limits.buffer_cap_bytes, config.buffer_cap_bytes);
    }

    // --- startup panic-containment ----------------------------------------
    //
    // These tests pin the `bootstrap-startup-panic-safety` contract:
    // the public `startup` shim wraps `startup_body` in
    // `std::panic::catch_unwind`, so any panic in the body is contained
    // at the FFI frame and the shim still returns `0` (silent-disable).
    //
    // We exercise the contract via the `#[cfg(test)] static
    // PANIC_IN_STARTUP_FOR_TEST` seam — the only panic site inside
    // `startup_body` reachable from the test build, because the FFI
    // calls downstream (`register_directives`, `read_raw_ini`,
    // `php_error`, …) are unresolvable at link time without a live
    // PHP runtime. The seam fires before any FFI call so a tripped
    // seam never reaches the FFI surface.
    //
    // The happy path (seam off) is exercised end-to-end by every
    // integration test that loads the cdylib into a real PHP — most
    // notably `tests/recorder_observer.rs` and
    // `tests/shipper_round_trip.rs`. It is not unit-testable in this
    // module by construction.

    /// Acquire a process-wide mutex over the test seam so two parallel
    /// `panic-startup` tests cannot race on `PANIC_IN_STARTUP_FOR_TEST`.
    /// One `OnceLock` lives in `tests::lock_startup_seam` below.
    fn lock_startup_seam() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static SEAM_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        SEAM_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Guard that flips the seam on, runs the test body, and resets
    /// the seam on drop — so a test that panics before clearing the
    /// seam still leaves the module in a clean state.
    struct StartupPanicSeamGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl StartupPanicSeamGuard {
        fn arm() -> Self {
            let lock = lock_startup_seam();
            PANIC_IN_STARTUP_FOR_TEST.store(true, Ordering::SeqCst);
            Self { _lock: lock }
        }
    }

    impl Drop for StartupPanicSeamGuard {
        fn drop(&mut self) {
            PANIC_IN_STARTUP_FOR_TEST.store(false, Ordering::SeqCst);
        }
    }

    #[test]
    fn startup_body_panic_is_contained_by_catch_unwind() {
        let _seam = StartupPanicSeamGuard::arm();
        // Direct catch_unwind around the body: this is what the
        // public shim does, with one extra layer for the unit-test
        // observation.
        let outcome = std::panic::catch_unwind(|| startup_body(0));
        assert!(
            outcome.is_err(),
            "seam is armed: startup_body MUST panic at its head before any FFI call"
        );
    }

    #[test]
    fn startup_returns_zero_on_panic() {
        let _seam = StartupPanicSeamGuard::arm();
        // The shim is the production-shaped surface: panic in the
        // body, catch_unwind absorbs, return value is still 0 so
        // PHP keeps starting (silent-disable per AD-4 / NFR-USE-2).
        let rc = startup(0, 0);
        assert_eq!(
            rc, 0,
            "even on a caught panic, MINIT MUST return 0 (Zend SUCCESS) so PHP keeps starting"
        );
    }

    #[test]
    fn startup_returns_zero_on_seam_off_no_panic_path() {
        // Negative companion to the panic tests: with the seam OFF
        // and the FFI body cfg-gated out under `#[cfg(test)]`,
        // calling the shim runs the cfg-test stub of `startup_body`
        // (which is empty modulo the seam check). The shim's
        // unconditional `return 0` is what we pin here. A future
        // refactor that changes the shim shape to ever return
        // anything other than `0` will trip this assertion.
        let _lock = lock_startup_seam();
        PANIC_IN_STARTUP_FOR_TEST.store(false, Ordering::SeqCst);
        assert_eq!(startup(0, 42), 0);
    }

    // --- cross-cutting: spawn failure × bootstrap drain ----------------

    /// Pin the bootstrap × shipper interaction across the
    /// spawn-failure boundary: after the spawn-failure recovery
    /// path runs (`SENDER_SLOT == None`, `SHIPPER_HANDLE == None`),
    /// `drain_shipper_if_enabled` MUST be a no-op — it cannot
    /// panic, cannot block, and cannot leave any state behind.
    /// The drain function's first guard (`SENDER_SLOT.take()` →
    /// `None` → `JoinOutcome::NotInstalled`) is the load-bearing
    /// short-circuit.
    #[test]
    fn bootstrap_full_lifecycle_with_spawn_failure_keeps_drain_a_noop() {
        use std::io;
        use std::sync::atomic::AtomicU64;
        use std::sync::Arc;

        let _guard = crate::shipper::acquire_test_lock();
        crate::shipper::reset_for_test();

        let enabled = enabled_config_for_lifecycle_tests();
        install_shipper_if_enabled(Some(&enabled));
        assert!(crate::shipper::sender_is_installed());
        assert!(crate::shipper::receiver_is_installed());

        // Drive the spawn-failure path via the shipper module's
        // injectable inner helper. The recovery sequence clears
        // SENDER_SLOT and stashes SHIPPER_SPAWN_FAILED; the
        // following drain MUST observe that and short-circuit.
        let invocations = Arc::new(AtomicU64::new(0));
        let invocations_for_closure = invocations.clone();
        crate::shipper::spawn_with_failing_factory_for_test(move |_name, _body| {
            invocations_for_closure.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(io::Error::other("test-induced spawn failure"))
        });
        assert_eq!(invocations.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(crate::shipper::spawn_failed_flag());
        assert!(!crate::shipper::sender_is_installed());
        assert!(!crate::shipper::handle_is_installed());

        // The bootstrap-layer drain must observe NotInstalled and
        // return cleanly without panic or hang. We can't observe
        // the JoinOutcome directly from this helper (it discards
        // the outcome — see slice-1 design), but we *can* observe
        // that the call returns and leaves the slots clean.
        drain_shipper_if_enabled(Some(&enabled));
        assert!(!crate::shipper::sender_is_installed(), "drain post-state");
        assert!(!crate::shipper::handle_is_installed(), "drain post-state");
        // Sticky flag survives the drain (no production code clears
        // it; only `reset_for_test` does).
        assert!(
            crate::shipper::spawn_failed_flag(),
            "SHIPPER_SPAWN_FAILED is sticky for the rest of the process",
        );

        crate::shipper::reset_for_test();
    }

    // --- master-switch E_NOTICE --------------------------------------------
    //
    // These four tests pin the `notice-on-master-switch-off` contract:
    // exactly one `E_NOTICE` per process when
    // `disable_reason == MasterSwitchOff`, silence for every other
    // arm and for the enabled / unset cases. They share a recorder
    // (see `recorded_notices`) and serialise via
    // `acquire_notice_test_lock` so the captured count is
    // deterministic under `cargo test`'s default parallel runner.

    fn config_with_disable_reason(reason: DisableReason) -> Config {
        // Hand-build a `Config` with an arbitrary `disable_reason` —
        // `Config::from_ini_values` only produces `MasterSwitchOff`
        // and the misconfig reasons that match the ini state, so the
        // "every other DisableReason" test below needs a direct
        // constructor.
        Config {
            enabled: false,
            disable_reason: Some(reason),
            server_url: None,
            auth_token: secrecy::SecretString::default(),
            auth_token_source: TokenSource::None,
            flush_records: 10_000,
            flush_bytes: 1_048_576,
            buffer_cap_bytes: 67_108_864,
            max_depth: 1024,
            retry_count: 3,
            retry_backoff: Duration::from_millis(100),
            http_timeout: Duration::from_millis(2_000),
            shutdown_grace: Duration::from_millis(5_000),
            shipper_queue_depth: 8,
            cpu_snapshot_mode: crate::config::CpuSnapshotMode::PerCall,
            spike_observer: false,
            spike_log_path: None,
        }
    }

    #[test]
    fn emit_master_switch_notice_if_off_emits_exactly_one_notice_when_master_switch_is_off() {
        let _guard = acquire_notice_test_lock();
        reset_recorded_notices_for_test();

        let disabled = disabled_config_for_lifecycle_tests();
        assert_eq!(
            disabled.disable_reason,
            Some(DisableReason::MasterSwitchOff),
            "fixture sanity: helper builds a MasterSwitchOff config",
        );

        emit_master_switch_notice_if_off(Some(&disabled));

        let captured = drain_recorded_notices_for_test();
        assert_eq!(
            captured.len(),
            1,
            "expected exactly one notice, got {captured:?}",
        );
        let line = &captured[0];
        assert!(
            line.contains("php_analyze.enabled = 0"),
            "notice must reference the directive that triggered it; got {line:?}",
        );
    }

    #[test]
    fn emit_master_switch_notice_if_off_is_silent_for_every_other_disable_reason() {
        let _guard = acquire_notice_test_lock();

        // Every `DisableReason` variant other than `MasterSwitchOff`.
        // If a new variant is added without updating this list, the
        // exhaustiveness of the match below will surface it at
        // compile time (see the explicit pattern match).
        let other_reasons = [
            DisableReason::ServerUrlNotConfigured,
            DisableReason::ServerUrlInvalid,
            DisableReason::ServerUrlSchemeUnsupported,
            DisableReason::TokenNotConfigured,
            DisableReason::TokenFileUnreadable,
            DisableReason::TokenFileEmpty,
        ];
        // Exhaustiveness guard: if a new `DisableReason` is added,
        // this match forces a compile-time decision about whether the
        // new variant should also be silent here.
        for reason in &other_reasons {
            match reason {
                DisableReason::MasterSwitchOff => {
                    unreachable!("MasterSwitchOff must not appear in the other-reasons list",)
                }
                DisableReason::ServerUrlNotConfigured
                | DisableReason::ServerUrlInvalid
                | DisableReason::ServerUrlSchemeUnsupported
                | DisableReason::TokenNotConfigured
                | DisableReason::TokenFileUnreadable
                | DisableReason::TokenFileEmpty => {}
            }
        }

        for reason in other_reasons {
            reset_recorded_notices_for_test();
            let config = config_with_disable_reason(reason.clone());
            emit_master_switch_notice_if_off(Some(&config));
            let captured = drain_recorded_notices_for_test();
            assert!(
                captured.is_empty(),
                "DisableReason::{reason:?} must not emit a master-switch notice; got {captured:?}",
            );
        }
    }

    #[test]
    fn emit_master_switch_notice_if_off_is_silent_when_extension_is_enabled() {
        let _guard = acquire_notice_test_lock();
        reset_recorded_notices_for_test();

        let enabled = enabled_config_for_lifecycle_tests();
        assert!(enabled.enabled, "fixture sanity: enabled config is enabled");
        assert_eq!(
            enabled.disable_reason, None,
            "fixture sanity: enabled config has no disable_reason",
        );

        emit_master_switch_notice_if_off(Some(&enabled));

        let captured = drain_recorded_notices_for_test();
        assert!(
            captured.is_empty(),
            "enabled config must not emit a master-switch notice; got {captured:?}",
        );
    }

    #[test]
    fn emit_master_switch_notice_if_off_is_silent_when_config_is_unset() {
        let _guard = acquire_notice_test_lock();
        reset_recorded_notices_for_test();

        emit_master_switch_notice_if_off(None);

        let captured = drain_recorded_notices_for_test();
        assert!(
            captured.is_empty(),
            "missing config must not emit a master-switch notice; got {captured:?}",
        );
    }
}
