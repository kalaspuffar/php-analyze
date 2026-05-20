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
//!   starting (AD-4 / NFR-USE-2 silent-disable).
//! - **`MSHUTDOWN`** is a no-op for this change. The Shipper drain that
//!   later changes add will live here.
//! - **`RINIT` / `RSHUTDOWN`** short-circuit immediately when
//!   `Config::global().map_or(true, |c| !c.enabled)`. Observer registration
//!   is out of scope until the recorder change.
//! - **`MINFO`** renders the resolved configuration. `auth_token` is
//!   *never* rendered from the [`secrecy::SecretString`] plaintext; the
//!   row is literally the string `"***"`. As belt-and-suspenders, the
//!   `auth_token` ini entry is registered with a `***` displayer so even
//!   PHP-internal paths (e.g. `display_ini_entries`) cannot leak it.

use ext_php_rs::error::php_error;
use ext_php_rs::ffi::{php_printf, zend_ini_entry};
use ext_php_rs::flags::{ErrorType, IniEntryPermission};
use ext_php_rs::zend::{ExecutorGlobals, IniEntryDef, ModuleEntry};
use ext_php_rs::{info_table_end, info_table_header, info_table_row, info_table_start};

use crate::config::{initialise_from_ini, Config, DisableReason, RawIni, TokenSource};

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

    0
}

/// `MSHUTDOWN` — module shutdown. No-op for this change; later changes
/// drain the shipper here.
///
/// # Safety
///
/// This function is the `C` ABI entry point called by Zend during module
/// shutdown. It must be invoked exactly once per module by the PHP
/// runtime, on the main thread, and not from Rust code. The body does no
/// pointer-deref so the contract is trivial in this change.
pub unsafe extern "C" fn mshutdown(_type_: i32, _module_number: i32) -> i32 {
    0
}

/// `RINIT` — request startup. Short-circuits when the extension is
/// disabled. No per-request work in this change.
///
/// # Safety
///
/// `C` ABI entry point called by Zend at the start of each PHP request.
/// Reads `Config::global()` which is set during `MINIT` and is
/// thereafter immutable for the lifetime of the process, so the read
/// requires no locking.
pub unsafe extern "C" fn rinit(_type_: i32, _module_number: i32) -> i32 {
    if Config::global().map_or(true, |c| !c.enabled) {
        return 0;
    }
    0
}

/// `RSHUTDOWN` — request shutdown. Symmetric to [`rinit`].
///
/// # Safety
///
/// `C` ABI entry point called by Zend at the end of each PHP request.
/// Same `Config::global()` invariant as [`rinit`].
pub unsafe extern "C" fn rshutdown(_type_: i32, _module_number: i32) -> i32 {
    if Config::global().map_or(true, |c| !c.enabled) {
        return 0;
    }
    0
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

fn read_raw_ini() -> RawIni {
    let ini = ExecutorGlobals::get().ini_values();

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
struct PhpInfoRenderer;

impl PhpInfoRenderer {
    fn render(&self, config: Option<&Config>) {
        info_table_start!();
        info_table_header!("php-analyze", env!("CARGO_PKG_VERSION"));

        match config {
            None => {
                info_table_row!("Status", "MINIT has not run");
            }
            Some(c) => {
                self.render_status(c);
                self.render_directives(c);
            }
        }

        info_table_end!();
    }

    fn render_status(&self, c: &Config) {
        let line = if c.enabled {
            "enabled (true)".to_owned()
        } else {
            let reason = c
                .disable_reason
                .as_ref()
                .map_or("unknown", DisableReason::human);
            format!("enabled (false: {reason})")
        };
        info_table_row!("Status", line.as_str());
    }

    fn render_directives(&self, c: &Config) {
        // For the master switch row, we want to reflect the operator's
        // intent (php_analyze.enabled = 0|1), not the effective enabled
        // state. The only way the operator's switch reads "off" is when
        // `disable_reason == MasterSwitchOff`.
        let master_on = !matches!(c.disable_reason, Some(DisableReason::MasterSwitchOff));
        info_table_row!("php_analyze.enabled", if master_on { "On" } else { "Off" });

        info_table_row!(
            "php_analyze.server_url",
            c.server_url
                .as_ref()
                .map(url::Url::as_str)
                .unwrap_or("(unset)")
        );
        // Hard-coded "***"; never touches `c.auth_token.expose_secret`.
        info_table_row!("php_analyze.auth_token", "***");
        let token_file = match &c.auth_token_source {
            TokenSource::File(path) => path.display().to_string(),
            _ => "(unset)".to_owned(),
        };
        info_table_row!("php_analyze.auth_token_file", token_file.as_str());

        info_table_row!(
            "php_analyze.flush_records",
            c.flush_records.to_string().as_str()
        );
        info_table_row!(
            "php_analyze.flush_bytes",
            c.flush_bytes.to_string().as_str()
        );
        info_table_row!(
            "php_analyze.buffer_cap_bytes",
            c.buffer_cap_bytes.to_string().as_str()
        );
        info_table_row!("php_analyze.max_depth", c.max_depth.to_string().as_str());
        info_table_row!(
            "php_analyze.retry_count",
            c.retry_count.to_string().as_str()
        );
        info_table_row!(
            "php_analyze.retry_backoff_ms",
            c.retry_backoff.as_millis().to_string().as_str()
        );
        info_table_row!(
            "php_analyze.http_timeout_ms",
            c.http_timeout.as_millis().to_string().as_str()
        );
        info_table_row!(
            "php_analyze.shutdown_grace_ms",
            c.shutdown_grace.as_millis().to_string().as_str()
        );
        info_table_row!(
            "php_analyze.shipper_queue_depth",
            c.shipper_queue_depth.to_string().as_str()
        );
    }
}
