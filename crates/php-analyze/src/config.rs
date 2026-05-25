//! Configuration — parses, validates, range-clamps, and freezes the
//! `php_analyze.*` ini directives into an immutable [`Config`] held in a
//! process-wide [`OnceLock`].
//!
//! The validation logic lives in the pure constructor
//! [`Config::from_ini_values`], which takes a [`RawIni`] of plain
//! strings/integers (not the live PHP INI subsystem). That keeps the
//! validation testable without PHP and lets the bootstrap layer keep its
//! `ext-php-rs`-dependent code minimal — a tiny adapter that reads INI
//! values, packs them into a [`RawIni`], and feeds the result through
//! [`initialise_from_ini`].
//!
//! Behaviour mirrors `SPECIFICATION.md`:
//!
//! - **§3.5** documents every directive's default and range; clamps emit
//!   one [`ConfigWarning::OutOfRange`] per offending directive.
//! - **§6.3** mandates that the bearer token never appear in any log line
//!   or `phpinfo()` output; the token is held in [`SecretString`], which
//!   redacts on `Debug` and refuses `Display`.
//! - **AD-4 / NFR-USE-2** silent-disable: missing/invalid `server_url` or
//!   missing usable token => [`Config::enabled`] is `false` and **at most
//!   one** disable warning is emitted.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use secrecy::SecretString;

/// Resolved, frozen configuration. Populated once at `MINIT` and never
/// mutated thereafter. See `SPECIFICATION.md` §4.1.1.
///
/// Deviations from the literal §4.1.1 sketch, with rationale:
///
/// - `server_url` is `Option<String>` rather than `String`. The disabled
///   state carries no validated URL, so requiring a `String` would force
///   a sentinel value. `None` makes the silent-disable case
///   unrepresentable in a misleading way. The string itself is
///   scheme-checked at construction (must start with `http://` or
///   `https://`) and trimmed of surrounding whitespace by the upstream
///   `RawIni`; it is otherwise the operator's bytes verbatim and is
///   passed to the HTTP client (`ureq::Agent::post`) without further
///   parsing.
/// - `disable_reason` is added so `MINFO` can render the literal
///   `enabled (false: <reason>)` line required by the `extension-bootstrap`
///   spec without re-deriving the reason from the warnings list.
#[derive(Debug)]
pub struct Config {
    pub enabled: bool,
    pub disable_reason: Option<DisableReason>,
    pub server_url: Option<String>,
    pub auth_token: SecretString,
    pub auth_token_source: TokenSource,
    pub flush_records: usize,
    pub flush_bytes: usize,
    pub buffer_cap_bytes: usize,
    pub max_depth: u16,
    pub retry_count: u8,
    pub retry_backoff: Duration,
    pub http_timeout: Duration,
    pub shutdown_grace: Duration,
    pub shipper_queue_depth: usize,
    /// Per-call CPU snapshot policy. Default [`CpuSnapshotMode::PerCall`]
    /// (spec-current). Operators can opt into [`CpuSnapshotMode::Off`]
    /// for high-volume pools where the per-call `getrusage` syscall is
    /// a measurable share of recorder overhead — see
    /// `COMMENTS.md` C-19.
    pub cpu_snapshot_mode: CpuSnapshotMode,
}

/// Where the resolved bearer token came from. Rendered in `MINFO` (the
/// path is **not** secret; the token itself is).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    None,
    Inline,
    File(PathBuf),
}

/// Per-call CPU snapshot policy for the recorder hot path.
///
/// `SPECIFICATION.md` §3.2 mandates `getrusage(RUSAGE_THREAD)` per
/// begin/end snapshot to populate `CallRecord::cpu_u_ns` /
/// `cpu_s_ns`. On hosts without vDSO acceleration for `getrusage`
/// each call costs ~500 ns of real syscall time — ~1000 ns per
/// PHP call across begin+end. The `Off` variant lets operators
/// opt that cost away in exchange for losing per-call CPU
/// attribution (all `cpu_u_ns` / `cpu_s_ns` values then read `0`).
///
/// `R-11` already permits `cpu_*_ns == 0` for sub-microsecond
/// functions; `Off` extends that permission to every function
/// regardless of duration. See `COMMENTS.md` C-19 for the gap
/// analysis that motivated this directive and
/// `openspec/changes/recorder-cpu-snapshot-cadence/` for the
/// design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CpuSnapshotMode {
    /// Spec-current behaviour: every begin/end snapshot calls
    /// `getrusage(RUSAGE_THREAD)` and records the actual CPU
    /// times. **Default** when the directive is absent.
    #[default]
    PerCall,
    /// Skip the `getrusage` call entirely; emit
    /// `cpu_u_ns = cpu_s_ns = 0` in every `CallRecord`. Saves
    /// ~1000 ns/call (host-dependent) at the cost of per-call
    /// CPU attribution.
    Off,
}

impl CpuSnapshotMode {
    /// Operator-facing string form: matches the
    /// `php_analyze.cpu_snapshot_mode` directive values that
    /// resolve to each variant. Used by the `phpinfo()` renderer
    /// and the parser's diagnostic output.
    pub fn as_ini_str(self) -> &'static str {
        match self {
            Self::PerCall => "per-call",
            Self::Off => "off",
        }
    }

    /// Parse the directive's raw string. Case-insensitive and
    /// whitespace-trimmed. `None` on an unrecognised value (the
    /// caller emits the warning and falls back to the default).
    pub fn parse(raw: &str) -> Option<Self> {
        let normalised = raw.trim();
        if normalised.eq_ignore_ascii_case("per-call") {
            Some(Self::PerCall)
        } else if normalised.eq_ignore_ascii_case("off") {
            Some(Self::Off)
        } else {
            None
        }
    }
}

/// Why `Config::enabled` is `false`. Rendered verbatim into `MINFO` as
/// `enabled (false: <reason>)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisableReason {
    /// Operator set `php_analyze.enabled = 0`. Not a misconfiguration.
    MasterSwitchOff,
    ServerUrlNotConfigured,
    ServerUrlInvalid,
    ServerUrlSchemeUnsupported,
    TokenNotConfigured,
    TokenFileUnreadable,
    TokenFileEmpty,
}

impl DisableReason {
    /// One-line human-readable reason. Used by `MINFO` and by the
    /// `ConfigWarning::Display` impls. Token-free by construction.
    pub fn human(&self) -> &'static str {
        match self {
            Self::MasterSwitchOff => "php_analyze.enabled = 0",
            Self::ServerUrlNotConfigured => "server_url not configured",
            Self::ServerUrlInvalid => "server_url failed to parse",
            Self::ServerUrlSchemeUnsupported => "server_url scheme not supported",
            Self::TokenNotConfigured => "no bearer token configured",
            Self::TokenFileUnreadable => "auth_token_file could not be read",
            Self::TokenFileEmpty => "auth_token_file is empty",
        }
    }
}

/// Raw INI values exactly as the PHP INI subsystem hands them back. `None`
/// means the directive was not set at all (use the documented default).
///
/// Integer fields use `i64` because PHP's `zend_long` is 64-bit on
/// supported platforms and INI values may be negative (which we then
/// catch via range clamping).
#[derive(Debug, Default)]
pub struct RawIni {
    pub enabled: Option<bool>,
    pub server_url: Option<String>,
    pub auth_token: Option<String>,
    pub auth_token_file: Option<String>,
    pub flush_records: Option<i64>,
    pub flush_bytes: Option<i64>,
    pub buffer_cap_bytes: Option<i64>,
    pub max_depth: Option<i64>,
    pub retry_count: Option<i64>,
    pub retry_backoff_ms: Option<i64>,
    pub http_timeout_ms: Option<i64>,
    pub shutdown_grace_ms: Option<i64>,
    pub shipper_queue_depth: Option<i64>,
    /// Raw `php_analyze.cpu_snapshot_mode` value. `None` means
    /// the directive was absent; otherwise the string is fed
    /// through [`CpuSnapshotMode::parse`] in
    /// [`Config::from_ini_values`].
    pub cpu_snapshot_mode: Option<String>,
}

/// Observation emitted while resolving a [`Config`]. The bootstrap layer
/// logs every returned warning at `E_WARNING` so the operator sees it in
/// the PHP error log. The `Display` impl renders one line of token-free
/// text per variant.
///
/// Naming note: the OpenSpec tasks use the name `ConfigError`, but these
/// values do **not** abort `MINIT` — they recover and continue. The
/// `Warning` suffix is more honest. A `ConfigError` alias is exported
/// below for callers that prefer the original name.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ConfigWarning {
    #[error("php_analyze.server_url: failed to parse '{value}' as a URL: {reason}")]
    InvalidUrl { value: String, reason: String },

    #[error("php_analyze.server_url: scheme '{scheme}' is not supported (must be http or https)")]
    UnsupportedScheme { scheme: String },

    #[error("php_analyze.server_url uses http://; TLS is recommended for production deployments")]
    HttpScheme,

    #[error("php_analyze disabled: server_url is not configured")]
    MissingServerUrl,

    #[error(
        "php_analyze disabled: no bearer token configured (set php_analyze.auth_token or php_analyze.auth_token_file)"
    )]
    MissingToken,

    #[error("php_analyze disabled: auth_token_file at {path:?} could not be read: {details}")]
    TokenFileUnreadable { path: PathBuf, details: String },

    #[error(
        "php_analyze disabled: auth_token_file at {path:?} is empty after trimming whitespace"
    )]
    EmptyTokenFile { path: PathBuf },

    #[error("php_analyze.{directive}: value {value} is out of range, clamped to {clamped_to}")]
    OutOfRange {
        directive: &'static str,
        value: i64,
        clamped_to: i64,
    },

    #[error(
        "php_analyze.cpu_snapshot_mode: unrecognised value '{raw_value}', falling back to '{fallback}'"
    )]
    UnknownCpuSnapshotMode {
        raw_value: String,
        fallback: &'static str,
    },
}

/// Compatibility alias for the OpenSpec task wording (`pub enum ConfigError`).
pub type ConfigError = ConfigWarning;

// --- defaults & ranges (§3.5) -------------------------------------------------

const DEFAULT_ENABLED: bool = true;

const DEFAULT_FLUSH_RECORDS: i64 = 10_000;
const RANGE_FLUSH_RECORDS: (i64, i64) = (1, 1_000_000_000);

const DEFAULT_FLUSH_BYTES: i64 = 1_048_576;
const RANGE_FLUSH_BYTES: (i64, i64) = (1024, 1_000_000_000);

const DEFAULT_BUFFER_CAP_BYTES: i64 = 67_108_864;
const RANGE_BUFFER_CAP_BYTES: (i64, i64) = (1024, 10_000_000_000);

const DEFAULT_MAX_DEPTH: i64 = 1024;
const RANGE_MAX_DEPTH: (i64, i64) = (1, 65_535);

const DEFAULT_RETRY_COUNT: i64 = 3;
const RANGE_RETRY_COUNT: (i64, i64) = (0, 10);

const DEFAULT_RETRY_BACKOFF_MS: i64 = 100;
const RANGE_RETRY_BACKOFF_MS: (i64, i64) = (1, 60_000);

const DEFAULT_HTTP_TIMEOUT_MS: i64 = 2_000;
const RANGE_HTTP_TIMEOUT_MS: (i64, i64) = (100, 60_000);

const DEFAULT_SHUTDOWN_GRACE_MS: i64 = 5_000;
const RANGE_SHUTDOWN_GRACE_MS: (i64, i64) = (0, 60_000);

const DEFAULT_SHIPPER_QUEUE_DEPTH: i64 = 8;
const RANGE_SHIPPER_QUEUE_DEPTH: (i64, i64) = (1, 1024);

// --- public constructor & global --------------------------------------------

impl Config {
    /// A `Config` representing the silent-disable state, with every
    /// numeric directive at its documented §3.5 default. Used by the
    /// `master_enabled = false` short-circuit in [`from_ini_values`] so
    /// stale URL / range warnings do not surface on a deliberately-off
    /// pool.
    fn disabled(reason: DisableReason) -> Self {
        Self {
            enabled: false,
            disable_reason: Some(reason),
            server_url: None,
            auth_token: SecretString::default(),
            auth_token_source: TokenSource::None,
            flush_records: DEFAULT_FLUSH_RECORDS as usize,
            flush_bytes: DEFAULT_FLUSH_BYTES as usize,
            buffer_cap_bytes: DEFAULT_BUFFER_CAP_BYTES as usize,
            max_depth: DEFAULT_MAX_DEPTH as u16,
            retry_count: DEFAULT_RETRY_COUNT as u8,
            retry_backoff: Duration::from_millis(DEFAULT_RETRY_BACKOFF_MS as u64),
            http_timeout: Duration::from_millis(DEFAULT_HTTP_TIMEOUT_MS as u64),
            shutdown_grace: Duration::from_millis(DEFAULT_SHUTDOWN_GRACE_MS as u64),
            shipper_queue_depth: DEFAULT_SHIPPER_QUEUE_DEPTH as usize,
            // CPU snapshot mode keeps its documented default in the
            // disabled state — the directive's value is irrelevant
            // while the extension is off but rendering `phpinfo()`
            // still needs a defined value.
            cpu_snapshot_mode: CpuSnapshotMode::PerCall,
        }
    }

    /// Pure constructor: takes raw INI values and returns the resolved
    /// [`Config`] plus the list of warnings the caller must log at
    /// `E_WARNING`. Has no side effects (it may read `auth_token_file` from
    /// disk, but never mutates any global).
    ///
    /// `auth_token_file` is read once here. Failure to read or an empty
    /// file silently disables the extension; we deliberately do **not**
    /// fall back to the inline `auth_token` (§3.7 / configuration spec
    /// scenario "Unreadable token file triggers silent disable").
    ///
    /// When `php_analyze.enabled = 0` the function bails before any other
    /// validation runs. That makes the disabled-pool quiet: no
    /// `OutOfRange`/URL warnings even if stale directives remain in
    /// `php.ini` (NFR-USE-2 / review finding R-5).
    pub fn from_ini_values(raw: &RawIni) -> (Self, Vec<ConfigWarning>) {
        let master_enabled = raw.enabled.unwrap_or(DEFAULT_ENABLED);

        // Operator turned the extension off via php_analyze.enabled = 0.
        // Do not re-validate the rest of php.ini: stale URL or
        // out-of-range numeric directives must not clutter the error
        // log on a deliberately-disabled pool (NFR-USE-2). Every
        // numeric directive keeps its documented default.
        if !master_enabled {
            return (Self::disabled(DisableReason::MasterSwitchOff), Vec::new());
        }

        let mut warnings = Vec::new();

        let flush_records = clamp_directive(
            raw.flush_records,
            DEFAULT_FLUSH_RECORDS,
            RANGE_FLUSH_RECORDS,
            "flush_records",
            &mut warnings,
        );
        let flush_bytes = clamp_directive(
            raw.flush_bytes,
            DEFAULT_FLUSH_BYTES,
            RANGE_FLUSH_BYTES,
            "flush_bytes",
            &mut warnings,
        );

        // Cross-field: buffer_cap_bytes must be >= flush_bytes. Treat
        // flush_bytes as the effective lower bound; clamp upwards and warn
        // if smaller.
        let buffer_cap_lower = std::cmp::max(RANGE_BUFFER_CAP_BYTES.0, flush_bytes);
        let buffer_cap_bytes = clamp_directive(
            raw.buffer_cap_bytes,
            DEFAULT_BUFFER_CAP_BYTES.max(flush_bytes),
            (buffer_cap_lower, RANGE_BUFFER_CAP_BYTES.1),
            "buffer_cap_bytes",
            &mut warnings,
        );

        let max_depth = clamp_directive(
            raw.max_depth,
            DEFAULT_MAX_DEPTH,
            RANGE_MAX_DEPTH,
            "max_depth",
            &mut warnings,
        );
        let retry_count = clamp_directive(
            raw.retry_count,
            DEFAULT_RETRY_COUNT,
            RANGE_RETRY_COUNT,
            "retry_count",
            &mut warnings,
        );
        let retry_backoff_ms = clamp_directive(
            raw.retry_backoff_ms,
            DEFAULT_RETRY_BACKOFF_MS,
            RANGE_RETRY_BACKOFF_MS,
            "retry_backoff_ms",
            &mut warnings,
        );
        let http_timeout_ms = clamp_directive(
            raw.http_timeout_ms,
            DEFAULT_HTTP_TIMEOUT_MS,
            RANGE_HTTP_TIMEOUT_MS,
            "http_timeout_ms",
            &mut warnings,
        );
        let shutdown_grace_ms = clamp_directive(
            raw.shutdown_grace_ms,
            DEFAULT_SHUTDOWN_GRACE_MS,
            RANGE_SHUTDOWN_GRACE_MS,
            "shutdown_grace_ms",
            &mut warnings,
        );
        let shipper_queue_depth = clamp_directive(
            raw.shipper_queue_depth,
            DEFAULT_SHIPPER_QUEUE_DEPTH,
            RANGE_SHIPPER_QUEUE_DEPTH,
            "shipper_queue_depth",
            &mut warnings,
        );

        let (server_url, server_url_outcome) = resolve_server_url(raw, &mut warnings);

        // Token resolution and disable-summary selection. Exactly one
        // disable-summary warning is pushed per call: either the specific
        // failure (Invalid/Unsupported/TokenFileUnreadable/EmptyTokenFile)
        // or the generic Missing{ServerUrl,Token}. See the per-variant
        // commentary in §3.8. The `MasterSwitchOff` arm is handled by the
        // early-return above.
        let (auth_token, token_source, disable_reason) =
            if let Some(reason) = server_url_outcome.disable_reason() {
                if matches!(reason, DisableReason::ServerUrlNotConfigured) {
                    warnings.push(ConfigWarning::MissingServerUrl);
                }
                (SecretString::default(), TokenSource::None, Some(reason))
            } else {
                match resolve_token(raw, &mut warnings) {
                    TokenOutcome::Resolved(secret, source) => (secret, source, None),
                    TokenOutcome::Missing => {
                        warnings.push(ConfigWarning::MissingToken);
                        (
                            SecretString::default(),
                            TokenSource::None,
                            Some(DisableReason::TokenNotConfigured),
                        )
                    }
                    TokenOutcome::FileUnreadable => (
                        SecretString::default(),
                        TokenSource::None,
                        Some(DisableReason::TokenFileUnreadable),
                    ),
                    TokenOutcome::FileEmpty => (
                        SecretString::default(),
                        TokenSource::None,
                        Some(DisableReason::TokenFileEmpty),
                    ),
                }
            };

        let enabled = disable_reason.is_none();

        // `cpu_snapshot_mode`: parse the raw string. Absent → default
        // (PerCall). Present and recognised → matched variant.
        // Present but unrecognised → push one warning and fall back to
        // the default. Matches the §3.5 directive-table posture:
        // clamp/fallback with one E_WARNING; never disable on bad
        // input.
        let cpu_snapshot_mode = match raw.cpu_snapshot_mode.as_deref() {
            None => CpuSnapshotMode::PerCall,
            Some(raw_value) => match CpuSnapshotMode::parse(raw_value) {
                Some(mode) => mode,
                None => {
                    warnings.push(ConfigWarning::UnknownCpuSnapshotMode {
                        raw_value: raw_value.to_owned(),
                        fallback: CpuSnapshotMode::PerCall.as_ini_str(),
                    });
                    CpuSnapshotMode::PerCall
                }
            },
        };

        let config = Self {
            enabled,
            disable_reason,
            server_url,
            auth_token,
            auth_token_source: token_source,
            flush_records: flush_records as usize,
            flush_bytes: flush_bytes as usize,
            buffer_cap_bytes: buffer_cap_bytes as usize,
            max_depth: max_depth as u16,
            retry_count: retry_count as u8,
            retry_backoff: Duration::from_millis(retry_backoff_ms as u64),
            http_timeout: Duration::from_millis(http_timeout_ms as u64),
            shutdown_grace: Duration::from_millis(shutdown_grace_ms as u64),
            shipper_queue_depth: shipper_queue_depth as usize,
            cpu_snapshot_mode,
        };

        (config, warnings)
    }

    /// Process-wide read accessor for the frozen config.
    ///
    /// Returns `None` only before [`initialise_from_ini`] has run — i.e.
    /// before `MINIT`. After `MINIT` this is lock-free and returns the
    /// same `&'static Config` on every call.
    pub fn global() -> Option<&'static Config> {
        CONFIG.get()
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Resolve a [`Config`] from raw INI values, store it in the process-wide
/// `OnceLock`, and return the warnings the caller (bootstrap layer) must
/// log at `E_WARNING`. Calling this a second time in the same process is
/// a no-op for the global and **does not** produce a fresh `Config` —
/// returning the original-call's warnings would be misleading, so the
/// fresh warning list from the second call is returned for the caller to
/// inspect, but [`Config::global`] still references the first-stored
/// value.
pub fn initialise_from_ini(raw: RawIni) -> Vec<ConfigWarning> {
    let (config, warnings) = Config::from_ini_values(&raw);
    // Ignore the result: a second MINIT in the same process is unusual
    // (PHP-FPM workers inherit the master's CONFIG), but if it does
    // happen the first value wins. The bootstrap layer treats `MINIT` as
    // a one-shot.
    let _ = CONFIG.set(config);
    warnings
}

// --- helpers ----------------------------------------------------------------

fn clamp_directive(
    raw: Option<i64>,
    default: i64,
    range: (i64, i64),
    directive: &'static str,
    warnings: &mut Vec<ConfigWarning>,
) -> i64 {
    let (min, max) = range;
    let value = raw.unwrap_or(default);
    if value < min {
        warnings.push(ConfigWarning::OutOfRange {
            directive,
            value,
            clamped_to: min,
        });
        min
    } else if value > max {
        warnings.push(ConfigWarning::OutOfRange {
            directive,
            value,
            clamped_to: max,
        });
        max
    } else {
        value
    }
}

/// Outcome of resolving `server_url`. Distinguishes "explicit parse/scheme
/// failure" (which produces its own warning) from "directive simply not
/// set" (which the caller turns into [`ConfigWarning::MissingServerUrl`]).
enum ServerUrlOutcome {
    Ok,
    Unset,
    InvalidUrl,
    UnsupportedScheme,
}

impl ServerUrlOutcome {
    fn disable_reason(&self) -> Option<DisableReason> {
        match self {
            Self::Ok => None,
            Self::Unset => Some(DisableReason::ServerUrlNotConfigured),
            Self::InvalidUrl => Some(DisableReason::ServerUrlInvalid),
            Self::UnsupportedScheme => Some(DisableReason::ServerUrlSchemeUnsupported),
        }
    }
}

// `server_url` validation is scheme-only by design. The value travels
// verbatim to `ureq::Agent::post(&str)`; full RFC-3987 IDNA parsing
// (what `url::Url::parse` provided) is unnecessary because malformed
// hosts, paths, or percent-encodings surface at HTTP-request time via
// the existing shipper error-reporting path. See the
// `drop-url-crate-for-scheme-validator` change's `design.md` D-1.
fn resolve_server_url(
    raw: &RawIni,
    warnings: &mut Vec<ConfigWarning>,
) -> (Option<String>, ServerUrlOutcome) {
    let raw_url = raw.server_url.as_deref().unwrap_or("").trim();
    if raw_url.is_empty() {
        return (None, ServerUrlOutcome::Unset);
    }
    let Some((scheme, _rest)) = raw_url.split_once("://") else {
        warnings.push(ConfigWarning::InvalidUrl {
            value: raw_url.to_owned(),
            reason: "missing scheme (expected http:// or https://)".to_owned(),
        });
        return (None, ServerUrlOutcome::InvalidUrl);
    };
    match scheme {
        "https" => (Some(raw_url.to_owned()), ServerUrlOutcome::Ok),
        "http" => {
            warnings.push(ConfigWarning::HttpScheme);
            (Some(raw_url.to_owned()), ServerUrlOutcome::Ok)
        }
        other => {
            warnings.push(ConfigWarning::UnsupportedScheme {
                scheme: other.to_owned(),
            });
            (None, ServerUrlOutcome::UnsupportedScheme)
        }
    }
}

enum TokenOutcome {
    Resolved(SecretString, TokenSource),
    Missing,
    FileUnreadable,
    FileEmpty,
}

fn resolve_token(raw: &RawIni, warnings: &mut Vec<ConfigWarning>) -> TokenOutcome {
    let file_path = raw
        .auth_token_file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);

    if let Some(path) = file_path {
        // File precedence: file wins over inline; failure does NOT fall
        // back to the inline token (§3.7).
        //
        // Trim both ends, matching the inline branch (`raw.auth_token`
        // is `trim()`med below): a token written with
        // `echo "  secret  " > /etc/php-analyze/token` must yield the
        // same `SecretString` as the same content inline. Without this
        // the server gets a leading-whitespace bearer token, 401s every
        // batch, and the silent-disable posture never catches it.
        return match std::fs::read_to_string(&path) {
            Ok(content) => {
                let trimmed = content.trim();
                if trimmed.is_empty() {
                    warnings.push(ConfigWarning::EmptyTokenFile { path });
                    TokenOutcome::FileEmpty
                } else {
                    TokenOutcome::Resolved(
                        SecretString::from(trimmed.to_owned()),
                        TokenSource::File(path),
                    )
                }
            }
            Err(e) => {
                warnings.push(ConfigWarning::TokenFileUnreadable {
                    path,
                    details: e.to_string(),
                });
                TokenOutcome::FileUnreadable
            }
        };
    }

    // No file set — fall through to inline.
    let inline = raw.auth_token.as_deref().map(str::trim).unwrap_or("");
    if inline.is_empty() {
        TokenOutcome::Missing
    } else {
        TokenOutcome::Resolved(SecretString::from(inline.to_owned()), TokenSource::Inline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;
    use std::io::Write;

    fn minimal_valid_raw() -> RawIni {
        RawIni {
            enabled: Some(true),
            server_url: Some("https://ingest.example.com/v1/ingest".to_owned()),
            auth_token: Some("inline-token".to_owned()),
            ..RawIni::default()
        }
    }

    #[test]
    fn valid_https_url_with_inline_token_is_enabled() {
        let (config, warnings) = Config::from_ini_values(&minimal_valid_raw());
        assert!(config.enabled);
        assert!(config.disable_reason.is_none());
        assert_eq!(
            config.server_url.as_deref(),
            Some("https://ingest.example.com/v1/ingest")
        );
        assert_eq!(config.auth_token.expose_secret(), "inline-token");
        assert_eq!(config.auth_token_source, TokenSource::Inline);
        assert!(
            warnings.is_empty(),
            "expected no warnings, got {warnings:?}"
        );
    }

    #[test]
    fn http_url_is_accepted_with_warning() {
        let raw = RawIni {
            server_url: Some("http://localhost:8080/ingest".to_owned()),
            ..minimal_valid_raw()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        assert!(config.enabled);
        assert_eq!(
            warnings
                .iter()
                .filter(|w| matches!(w, ConfigWarning::HttpScheme))
                .count(),
            1
        );
        let rendered = warnings
            .iter()
            .map(|w| w.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !rendered.contains("inline-token"),
            "warning leaked the token: {rendered}"
        );
        assert!(rendered.contains("http://"));
    }

    #[test]
    fn invalid_url_silently_disables_with_one_warning() {
        let raw = RawIni {
            server_url: Some("not-a-url".to_owned()),
            ..minimal_valid_raw()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        assert!(!config.enabled);
        assert_eq!(config.disable_reason, Some(DisableReason::ServerUrlInvalid));
        let disable_warnings: Vec<_> = warnings
            .iter()
            .filter(|w| {
                !matches!(
                    w,
                    ConfigWarning::OutOfRange { .. } | ConfigWarning::HttpScheme
                )
            })
            .collect();
        assert_eq!(disable_warnings.len(), 1, "{warnings:?}");
        assert!(matches!(
            disable_warnings[0],
            ConfigWarning::InvalidUrl { .. }
        ));
    }

    #[test]
    fn missing_server_url_silently_disables_with_one_warning() {
        let raw = RawIni {
            server_url: None,
            ..minimal_valid_raw()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        assert!(!config.enabled);
        assert_eq!(
            config.disable_reason,
            Some(DisableReason::ServerUrlNotConfigured)
        );
        let disable_warnings: Vec<_> = warnings
            .iter()
            .filter(|w| matches!(w, ConfigWarning::MissingServerUrl))
            .collect();
        assert_eq!(disable_warnings.len(), 1);
    }

    #[test]
    fn missing_token_silently_disables_with_one_warning() {
        let raw = RawIni {
            auth_token: None,
            ..minimal_valid_raw()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        assert!(!config.enabled);
        assert_eq!(
            config.disable_reason,
            Some(DisableReason::TokenNotConfigured)
        );
        let disable_warnings: Vec<_> = warnings
            .iter()
            .filter(|w| matches!(w, ConfigWarning::MissingToken))
            .collect();
        assert_eq!(disable_warnings.len(), 1);
    }

    #[test]
    fn auth_token_file_overrides_inline_token() {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        writeln!(file, "file-token").expect("write");
        let raw = RawIni {
            auth_token: Some("inline-token".to_owned()),
            auth_token_file: Some(file.path().to_string_lossy().into_owned()),
            ..minimal_valid_raw()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        assert!(config.enabled);
        assert_eq!(config.auth_token.expose_secret(), "file-token");
        assert!(matches!(config.auth_token_source, TokenSource::File(_)));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn auth_token_file_with_surrounding_whitespace_is_fully_trimmed() {
        // Regression for the R-4 review finding: the file branch used
        // `trim_end()` while the inline branch used `trim()`, so
        // `"  file-token  \n"` from a file produced the
        // `SecretString` `"  file-token"` (leading whitespace kept).
        // Both branches must produce exactly `"file-token"`.
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        writeln!(file, "  file-token  ").expect("write");
        let raw = RawIni {
            auth_token_file: Some(file.path().to_string_lossy().into_owned()),
            ..minimal_valid_raw()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        assert!(config.enabled, "{warnings:?}");
        assert_eq!(config.auth_token.expose_secret(), "file-token");
        assert!(matches!(config.auth_token_source, TokenSource::File(_)));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn unreadable_auth_token_file_silently_disables_does_not_fall_back_to_inline() {
        let raw = RawIni {
            auth_token: Some("inline-token".to_owned()),
            auth_token_file: Some("/nonexistent/path/to/php-analyze-token".to_owned()),
            ..minimal_valid_raw()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        assert!(!config.enabled);
        assert_eq!(
            config.disable_reason,
            Some(DisableReason::TokenFileUnreadable)
        );
        assert_eq!(config.auth_token.expose_secret(), "");
        let rendered = warnings
            .iter()
            .map(|w| w.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains("inline-token"), "leaked: {rendered}");
        assert!(rendered.contains("/nonexistent/path/to/php-analyze-token"));
    }

    #[test]
    fn empty_auth_token_file_silently_disables() {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        let raw = RawIni {
            auth_token_file: Some(file.path().to_string_lossy().into_owned()),
            ..minimal_valid_raw()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        assert!(!config.enabled);
        assert_eq!(config.disable_reason, Some(DisableReason::TokenFileEmpty));
        assert!(warnings
            .iter()
            .any(|w| matches!(w, ConfigWarning::EmptyTokenFile { .. })));
    }

    #[test]
    fn master_switch_off_with_garbage_directives_emits_no_warnings() {
        // Regression for R-5: when the operator deliberately turns the
        // extension off, the rest of php.ini is not the operator's
        // immediate concern. Re-validating stale URL / out-of-range
        // numeric directives would clutter the PHP error log for no
        // benefit. The disabled-pool path is silent.
        let raw = RawIni {
            enabled: Some(false),
            server_url: Some("not-a-url".to_owned()),
            auth_token: None,
            flush_records: Some(-1),
            http_timeout_ms: Some(999_999),
            ..RawIni::default()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        assert!(!config.enabled);
        assert_eq!(config.disable_reason, Some(DisableReason::MasterSwitchOff));
        assert!(
            warnings.is_empty(),
            "master-switch-off path must be warning-free, got {warnings:?}"
        );
        // Numeric directives keep their documented defaults rather than
        // the clamped-from-garbage values.
        assert_eq!(config.flush_records, DEFAULT_FLUSH_RECORDS as usize);
        assert_eq!(config.http_timeout, Duration::from_millis(2_000));
    }

    #[test]
    fn flush_records_below_min_is_clamped_to_one_with_warning() {
        let raw = RawIni {
            flush_records: Some(0),
            ..minimal_valid_raw()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        assert_eq!(config.flush_records, 1);
        let clamp_warnings: Vec<_> = warnings
            .iter()
            .filter(|w| {
                matches!(
                    w,
                    ConfigWarning::OutOfRange {
                        directive: "flush_records",
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(clamp_warnings.len(), 1);
    }

    #[test]
    fn http_timeout_above_max_is_clamped_with_warning() {
        let raw = RawIni {
            http_timeout_ms: Some(999_999),
            ..minimal_valid_raw()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        assert_eq!(config.http_timeout, Duration::from_millis(60_000));
        assert!(warnings.iter().any(|w| matches!(
            w,
            ConfigWarning::OutOfRange {
                directive: "http_timeout_ms",
                clamped_to: 60_000,
                ..
            }
        )));
    }

    #[test]
    fn buffer_cap_smaller_than_flush_bytes_is_clamped_up_with_warning() {
        let raw = RawIni {
            flush_bytes: Some(1_048_576),
            buffer_cap_bytes: Some(1024),
            ..minimal_valid_raw()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        assert_eq!(config.buffer_cap_bytes, 1_048_576);
        assert!(warnings.iter().any(|w| matches!(
            w,
            ConfigWarning::OutOfRange {
                directive: "buffer_cap_bytes",
                clamped_to: 1_048_576,
                ..
            }
        )));
    }

    #[test]
    fn token_is_redacted_in_debug_output_and_phpinfo_render() {
        let raw = RawIni {
            auth_token: Some("sk_live_unique_marker_xyz".to_owned()),
            ..minimal_valid_raw()
        };
        let (config, _warnings) = Config::from_ini_values(&raw);
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("sk_live_unique_marker_xyz"),
            "token leaked into Debug output: {debug}"
        );
        // The `MINFO` rendering hands the token field over as the literal
        // string `***`; see the bootstrap-layer renderer. Here we assert
        // that the only way to extract the plaintext is via
        // `expose_secret`, which the renderer never calls.
        assert_eq!(
            config.auth_token.expose_secret(),
            "sk_live_unique_marker_xyz"
        );
    }

    #[test]
    fn at_most_one_disable_warning_is_emitted_when_multiple_required_values_missing() {
        let raw = RawIni {
            // server_url and auth_token both missing.
            server_url: None,
            auth_token: None,
            ..RawIni::default()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        assert!(!config.enabled);
        let disable_warnings: Vec<_> = warnings
            .iter()
            .filter(|w| {
                matches!(
                    w,
                    ConfigWarning::MissingServerUrl
                        | ConfigWarning::MissingToken
                        | ConfigWarning::InvalidUrl { .. }
                        | ConfigWarning::UnsupportedScheme { .. }
                        | ConfigWarning::TokenFileUnreadable { .. }
                        | ConfigWarning::EmptyTokenFile { .. }
                )
            })
            .collect();
        assert_eq!(disable_warnings.len(), 1, "got {warnings:?}");
    }

    // --- cpu_snapshot_mode (recorder-cpu-snapshot-cadence) ----------------

    #[test]
    fn cpu_snapshot_mode_absent_directive_defaults_to_per_call() {
        // The minimal raw INI does not set cpu_snapshot_mode; the parser
        // must default to PerCall without emitting any warning.
        let (config, warnings) = Config::from_ini_values(&minimal_valid_raw());
        assert_eq!(config.cpu_snapshot_mode, CpuSnapshotMode::PerCall);
        assert!(
            !warnings
                .iter()
                .any(|w| matches!(w, ConfigWarning::UnknownCpuSnapshotMode { .. })),
            "no UnknownCpuSnapshotMode warning expected when directive is absent: {warnings:?}"
        );
    }

    #[test]
    fn cpu_snapshot_mode_explicit_off_parses_to_off_variant() {
        let raw = RawIni {
            cpu_snapshot_mode: Some("off".to_owned()),
            ..minimal_valid_raw()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        assert_eq!(config.cpu_snapshot_mode, CpuSnapshotMode::Off);
        assert!(
            !warnings
                .iter()
                .any(|w| matches!(w, ConfigWarning::UnknownCpuSnapshotMode { .. })),
            "no UnknownCpuSnapshotMode warning expected for recognised value: {warnings:?}"
        );
    }

    #[test]
    fn cpu_snapshot_mode_explicit_per_call_round_trips_to_default_variant() {
        let raw = RawIni {
            cpu_snapshot_mode: Some("per-call".to_owned()),
            ..minimal_valid_raw()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        assert_eq!(config.cpu_snapshot_mode, CpuSnapshotMode::PerCall);
        assert!(
            !warnings
                .iter()
                .any(|w| matches!(w, ConfigWarning::UnknownCpuSnapshotMode { .. })),
            "no UnknownCpuSnapshotMode warning expected for recognised value: {warnings:?}"
        );
    }

    #[test]
    fn cpu_snapshot_mode_case_and_whitespace_normalised_before_match() {
        // Mixed case + leading/trailing whitespace must resolve to the
        // canonical variant without warning. Mirrors the
        // case-insensitive + trimmed posture documented on
        // `CpuSnapshotMode::parse`.
        let cases = [
            ("  OFF  ", CpuSnapshotMode::Off),
            ("Off", CpuSnapshotMode::Off),
            ("PER-CALL", CpuSnapshotMode::PerCall),
            ("\tper-call\n", CpuSnapshotMode::PerCall),
        ];
        for (raw_value, expected) in cases {
            let raw = RawIni {
                cpu_snapshot_mode: Some(raw_value.to_owned()),
                ..minimal_valid_raw()
            };
            let (config, warnings) = Config::from_ini_values(&raw);
            assert_eq!(
                config.cpu_snapshot_mode, expected,
                "raw value {raw_value:?} should parse to {expected:?}"
            );
            assert!(
                !warnings
                    .iter()
                    .any(|w| matches!(w, ConfigWarning::UnknownCpuSnapshotMode { .. })),
                "no warning expected for {raw_value:?}: {warnings:?}"
            );
        }
    }

    #[test]
    fn cpu_snapshot_mode_unknown_value_warns_once_and_falls_back_to_per_call() {
        let raw = RawIni {
            cpu_snapshot_mode: Some("sampled".to_owned()),
            ..minimal_valid_raw()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        // The unrecognised value falls back to the safe default.
        assert_eq!(config.cpu_snapshot_mode, CpuSnapshotMode::PerCall);
        // Exactly one UnknownCpuSnapshotMode warning is pushed, naming
        // the raw value and the fallback.
        let unknown: Vec<_> = warnings
            .iter()
            .filter_map(|w| match w {
                ConfigWarning::UnknownCpuSnapshotMode {
                    raw_value,
                    fallback,
                } => Some((raw_value.as_str(), *fallback)),
                _ => None,
            })
            .collect();
        assert_eq!(
            unknown.len(),
            1,
            "expected one UnknownCpuSnapshotMode warning: {warnings:?}"
        );
        assert_eq!(unknown[0], ("sampled", "per-call"));
        // The extension stays enabled — this is a clamp-and-continue
        // directive, not a disable trigger.
        assert!(config.enabled);
    }

    #[test]
    fn cpu_snapshot_mode_display_for_unknown_warning_names_value_and_fallback() {
        // Drift guard: the `Display` impl is what the bootstrap layer
        // sends to `php_error(E_WARNING, ...)`. The rendered text must
        // include the raw value and the fallback so the operator can
        // diagnose the typo from the log alone.
        let warning = ConfigWarning::UnknownCpuSnapshotMode {
            raw_value: "verbose".to_owned(),
            fallback: "per-call",
        };
        let rendered = warning.to_string();
        assert!(
            rendered.contains("verbose"),
            "expected raw value in display: {rendered}"
        );
        assert!(
            rendered.contains("per-call"),
            "expected fallback in display: {rendered}"
        );
        assert!(
            rendered.contains("cpu_snapshot_mode"),
            "expected directive name in display: {rendered}"
        );
    }

    #[test]
    fn cpu_snapshot_mode_under_master_switch_off_holds_default() {
        // The master-switch-off short-circuit returns `Config::disabled`;
        // the mode field carries the documented default (`PerCall`)
        // regardless of what the operator wrote in the disabled-pool's
        // php.ini.
        let raw = RawIni {
            enabled: Some(false),
            cpu_snapshot_mode: Some("off".to_owned()),
            ..RawIni::default()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        assert!(!config.enabled);
        assert_eq!(config.cpu_snapshot_mode, CpuSnapshotMode::PerCall);
        // No warnings — master-switch-off path skips all validation.
        assert!(
            warnings.is_empty(),
            "no warnings expected under master-switch-off: {warnings:?}"
        );
    }

    #[test]
    fn config_global_returns_same_reference_on_repeated_reads() {
        // This test is the only one that touches the OnceLock. Other tests
        // must use `Config::from_ini_values` directly to avoid contaminating
        // the global. The harness runs tests in parallel within a process,
        // but only one `set` succeeds; that's the value `global()` returns.
        let warnings = initialise_from_ini(minimal_valid_raw());
        // A second initialise call should be a no-op for the global.
        let _ = initialise_from_ini(RawIni::default());

        let first = Config::global().expect("global initialised");
        let second = Config::global().expect("global initialised");
        assert!(
            std::ptr::eq(first, second),
            "Config::global() returned different references"
        );
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}
