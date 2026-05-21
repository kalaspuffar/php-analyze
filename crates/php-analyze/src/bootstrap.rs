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

use ext_php_rs::error::php_error;
use ext_php_rs::ffi::{php_printf, zend_ini_entry};
use ext_php_rs::flags::{ErrorType, IniEntryPermission};
use ext_php_rs::zend::{ExecutorGlobals, IniEntryDef, ModuleEntry, SapiGlobals, SapiModule};
use ext_php_rs::{info_table_end, info_table_header, info_table_row, info_table_start};

use crate::config::{initialise_from_ini, Config, DisableReason, RawIni, TokenSource};
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

/// `MINIT` — module startup. Wired into the `#[php_module]`-generated
/// startup via `#[php(startup = startup)]` in `lib.rs`. Returns `0` on
/// success per the Zend convention, and **always** returns `0` (silent-disable
/// posture).
pub fn startup(_type_: i32, module_number: i32) -> i32 {
    register_directives(module_number);

    let raw = read_raw_ini();
    let warnings = initialise_from_ini(raw);

    for warning in warnings {
        // `Display` for `ConfigWarning` renders a one-line, token-free
        // message; see the variant `#[error("…")]` attributes.
        php_error(&ErrorType::Warning, &warning.to_string());
    }

    // Install the Phase-4 shipper channel for enabled extensions only.
    // The disabled path stays silent — no channel, no thread, no
    // `MSHUTDOWN` work at process exit (the `shipper::*` slots remain
    // `None` / `false`).
    install_shipper_if_enabled(Config::global());

    0
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
    shipper::spawn_if_needed_at_rinit();
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
    // Slice-3 cap thresholds, cached onto the `Trace` so the hot path
    // does not need to re-read `Config::global()` per call.
    // `Config::max_depth: u16` widens losslessly to `u32` so the gate's
    // comparison with `Trace::virtual_depth: u32` happens without a
    // cast.
    let limits = TraceLimits {
        max_depth: u32::from(config.max_depth),
        buffer_cap_bytes: config.buffer_cap_bytes,
    };
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
    let _ = std::panic::catch_unwind(recorder::rshutdown_release_trace);
    0
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
    let request_uri = {
        let globals = SapiGlobals::get();
        let info = globals.request_info();
        info.request_uri()
            .map(str::to_owned)
            .or_else(|| info.argv0().map(str::to_owned))
    };
    let host = read_hostname();
    request_identity_from_sapi(&sapi_name, request_uri.as_deref(), host.as_deref())
}

/// Pure helper: assemble a [`RequestIdentity`] from string inputs and
/// the process PID. Factored out of [`build_request_identity`] so unit
/// tests can exercise the value-building without acquiring PHP locks.
///
/// `request_uri` is the SAPI-resolved URI when present (FPM populates
/// `SG(request_info).request_uri`; CLI populates `argv0`). When both
/// are missing — exotic SAPI, or a request that hasn't reached its
/// URI-parse stage — we fall back to a stable placeholder so the
/// trace's `uri_or_script` is never empty.
fn request_identity_from_sapi(
    sapi_name: &str,
    request_uri: Option<&str>,
    hostname: Option<&str>,
) -> RequestIdentity {
    let host: Arc<str> = Arc::from(hostname.unwrap_or("(unknown-host)"));
    let sapi: Arc<str> = Arc::from(sapi_name);
    let uri_or_script = request_uri.unwrap_or("(unknown-uri)").to_owned();
    RequestIdentity {
        host,
        sapi,
        pid: std::process::id(),
        uri_or_script,
    }
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
/// entirely in [`raw_ini_from_ini_map`].
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
            c.server_url
                .as_ref()
                .map(url::Url::as_str)
                .unwrap_or("(unset)")
                .to_owned(),
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
    use std::time::Duration;

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

    // --- request_identity_from_sapi ---------------------------------------

    #[test]
    fn request_identity_from_sapi_uses_request_uri_under_fpm_fcgi() {
        let id = request_identity_from_sapi(
            "fpm-fcgi",
            Some("/api/v1/users?page=2"),
            Some("worker-7.prod"),
        );
        assert_eq!(&*id.sapi, "fpm-fcgi");
        assert_eq!(id.uri_or_script, "/api/v1/users?page=2");
        assert_eq!(&*id.host, "worker-7.prod");
        assert_eq!(id.pid, std::process::id());
    }

    #[test]
    fn request_identity_from_sapi_falls_back_to_argv_under_cli() {
        // Under CLI we pass argv0 in the `request_uri` slot per the
        // FFI caller in `build_request_identity` (it tries
        // `request_info.request_uri()` first then `argv0()`).
        let id = request_identity_from_sapi(
            "cli",
            Some("/usr/local/bin/my-script.php"),
            Some("dev-laptop"),
        );
        assert_eq!(&*id.sapi, "cli");
        assert_eq!(id.uri_or_script, "/usr/local/bin/my-script.php");
    }

    #[test]
    fn request_identity_from_sapi_uses_placeholders_when_inputs_are_missing() {
        // Defensive: a non-CLI/non-FPM SAPI with no request URI must
        // still produce a usable RequestIdentity (no panic, no empty
        // strings the wire format would have to special-case later).
        let id = request_identity_from_sapi("apache2handler", None, None);
        assert_eq!(&*id.sapi, "apache2handler");
        assert_eq!(id.uri_or_script, "(unknown-uri)");
        assert_eq!(&*id.host, "(unknown-host)");
    }

    #[test]
    fn request_identity_carries_the_current_pid_and_a_non_empty_host() {
        // The host name read here is whatever `gethostname` returns
        // on the test runner; we only assert it's populated.
        let host = read_hostname();
        let id = request_identity_from_sapi("cli", Some("/x.php"), host.as_deref());
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
}
