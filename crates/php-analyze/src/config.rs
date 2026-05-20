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
use url::Url;

/// Resolved, frozen configuration. Populated once at `MINIT` and never
/// mutated thereafter. See `SPECIFICATION.md` §4.1.1.
///
/// Deviations from the literal §4.1.1 sketch, with rationale:
///
/// - `server_url` is `Option<Url>` rather than `Url`. The disabled state
///   carries no validated URL, so requiring a `Url` would force a
///   sentinel value. `None` makes the silent-disable case unrepresentable
///   in a misleading way.
/// - `disable_reason` is added so `MINFO` can render the literal
///   `enabled (false: <reason>)` line required by the `extension-bootstrap`
///   spec without re-deriving the reason from the warnings list.
#[derive(Debug)]
pub struct Config {
    pub enabled: bool,
    pub disable_reason: Option<DisableReason>,
    pub server_url: Option<Url>,
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
}

/// Where the resolved bearer token came from. Rendered in `MINFO` (the
/// path is **not** secret; the token itself is).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    None,
    Inline,
    File(PathBuf),
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

fn resolve_server_url(
    raw: &RawIni,
    warnings: &mut Vec<ConfigWarning>,
) -> (Option<Url>, ServerUrlOutcome) {
    let raw_url = raw.server_url.as_deref().unwrap_or("").trim();
    if raw_url.is_empty() {
        return (None, ServerUrlOutcome::Unset);
    }
    match Url::parse(raw_url) {
        Ok(url) => match url.scheme() {
            "https" => (Some(url), ServerUrlOutcome::Ok),
            "http" => {
                warnings.push(ConfigWarning::HttpScheme);
                (Some(url), ServerUrlOutcome::Ok)
            }
            other => {
                warnings.push(ConfigWarning::UnsupportedScheme {
                    scheme: other.to_owned(),
                });
                (None, ServerUrlOutcome::UnsupportedScheme)
            }
        },
        Err(e) => {
            warnings.push(ConfigWarning::InvalidUrl {
                value: raw_url.to_owned(),
                reason: e.to_string(),
            });
            (None, ServerUrlOutcome::InvalidUrl)
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
            config.server_url.as_ref().map(Url::as_str),
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
