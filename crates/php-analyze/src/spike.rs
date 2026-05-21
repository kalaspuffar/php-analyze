//! Phase-0 spike: prove `zend_observer` viability through
//! `ext-php-rs`'s `FcallObserver` wrapper.
//!
//! This module is **throwaway**. It exists to retire Risk **R-2** from
//! `SPECIFICATION.md` §11 by showing, on a real PHP runtime, that the
//! observer surface (a) reaches both user-defined and internal function
//! calls, (b) reports exception unwinds via `EG(exception)`, and (c)
//! does so through a stable public API on the locked `ext-php-rs =
//! "=0.15.13"`. Phase 2's Recorder change deletes this whole file and
//! its two `php_analyze.spike_*` directives in the same commit that
//! replaces it with the production hot-path.
//!
//! The spike is **off by default**. With `php_analyze.spike_observer =
//! 0` (the directive default), the observer's
//! [`SpikeObserver::should_observe`] returns `false` for every input
//! and `begin` / `end` never fire — the cost on a default-configured
//! load is a single `should_observe(&info) -> false` per unique
//! function (whose result PHP caches), and nothing else.
//!
//! Wire shape (see `design.md` §D-3):
//!
//! - `entry: <fqn>\n`
//! - `exit: <fqn> (abnormal=<true|false>)\n`
//!
//! Where `<fqn>` is one of:
//!
//! - `internal:<function_name>`
//! - `method:<class>::<method>`
//! - `closure:<file>:<line>`
//! - `function:<file>:<line>:<function_name>`

use std::fs::OpenOptions;
use std::io::{self, LineWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use ext_php_rs::ffi;
use ext_php_rs::types::Zval;
use ext_php_rs::zend::{ExecuteData, ExecutorGlobals, FcallInfo, FcallObserver};

use crate::config::Config;

/// The spike's `FcallObserver`. Holds the log sink behind an `Arc<Mutex>`
/// so the trait's `&self` methods can serialise their writes, and a
/// `bool` recording whether the spike is on for this process. The bool
/// is read by `should_observe` so the active/inactive decision is made
/// once per unique function and cached by PHP — i.e. when the spike is
/// off, the cost is one virtual call per unique function and zero
/// per-event cost thereafter.
pub struct SpikeObserver {
    sink: Arc<Mutex<Box<dyn Write + Send>>>,
    active: bool,
}

impl SpikeObserver {
    /// Build the observer from the frozen `Config`. Called once at
    /// `MINIT` (after our user `startup` runs, by `observer_startup`
    /// inside `ModuleStartup::startup`) — `Config::global()` is `Some`
    /// at this point.
    ///
    /// When the spike is inactive (the production default) this short-
    /// circuits before touching the filesystem: a stale
    /// `php_analyze.spike_log_path` left in `php.ini` next to
    /// `php_analyze.spike_observer = 0` must not produce a startup
    /// `E_WARNING` for an unused feature (review finding S-2, mirroring
    /// the master-switch quietness from R-5). The inactive sink is
    /// [`io::sink`], a zero-cost no-op writer that satisfies the
    /// `Mutex<Box<dyn Write + Send>>` invariant without holding a real
    /// file descriptor.
    ///
    /// In the active path, file-open failures are logged via
    /// `php_error(E_WARNING)` and the spike falls back to stderr. The
    /// `active` bit stays `true` in that case: a fallback-to-stderr is
    /// preferable to silently disabling a developer-requested spike.
    /// The spike warning here can only fire when the operator has
    /// explicitly turned the spike on, and the bootstrap layer's
    /// disable-summary warning only fires when the extension is
    /// disabled (which forces `active = false` here). The two paths
    /// are therefore mutually exclusive and the
    /// at-most-one-startup-`E_WARNING` invariant (NFR-USE-2 / AD-4) is
    /// preserved. See `COMMENTS.md` C-6 for the full argument.
    pub fn from_config(config: &Config) -> Self {
        let active = config.enabled && config.spike_observer;
        if !active {
            return Self::inactive_sink();
        }

        let (sink, warning) = open_spike_sink(config.spike_log_path.as_deref());
        if let Some(message) = warning {
            emit_spike_log_warning(&message);
        }
        Self {
            sink: Arc::new(Mutex::new(sink)),
            active,
        }
    }

    /// Build an inactive observer with a no-op sink. Used by the
    /// inactive short-circuit in [`from_config`] and by tests that need
    /// to verify the `should_observe == false` path without constructing
    /// a real file sink.
    fn inactive_sink() -> Self {
        Self {
            sink: Arc::new(Mutex::new(Box::new(io::sink()))),
            active: false,
        }
    }

    /// Test-only constructor that plugs an arbitrary sink. Used by the
    /// `fqn`-and-log unit tests below; not part of the production
    /// surface.
    #[cfg(test)]
    fn with_sink(sink: Box<dyn Write + Send>, active: bool) -> Self {
        Self {
            sink: Arc::new(Mutex::new(sink)),
            active,
        }
    }

    /// Write a single line to the sink. Errors are swallowed: a spike
    /// that can't reach its sink is unhelpful but must not panic into
    /// PHP. The Mutex poison case is also swallowed — same reasoning.
    fn write_line(&self, line: &str) {
        if let Ok(mut sink) = self.sink.lock() {
            let _ = writeln!(sink, "{line}");
        }
    }
}

impl FcallObserver for SpikeObserver {
    fn should_observe(&self, _info: &FcallInfo) -> bool {
        // PHP caches this per unique function. When `active` is false
        // (the production default for default-off installs), the false
        // is cached forever and `begin` / `end` never fire.
        self.active
    }

    fn begin(&self, execute_data: &ExecuteData) {
        // SAFETY: the observer trait hands us a `&ExecuteData` that is
        // valid for the duration of the call. `extract_info` reads
        // through `(*execute_data).func` and a handful of `zend_string`
        // pointers, all of which remain valid until `end` returns.
        let info = unsafe { extract_info(execute_data) };
        self.write_line(&format!("entry: {}", fqn(&info)));
    }

    fn end(&self, execute_data: &ExecuteData, _retval: Option<&Zval>) {
        // SAFETY: same lifetime invariant as `begin`.
        let info = unsafe { extract_info(execute_data) };
        // Non-destructive read: `has_exception` does not consume
        // `EG(exception)`, so the original `try { } catch` handler
        // still sees it. The handler runs whether the function returned
        // normally or unwound; this distinguishes the two.
        let abnormal = ExecutorGlobals::has_exception();
        self.write_line(&format!("exit: {} (abnormal={abnormal})", fqn(&info)));
    }
}

/// Emit the spike's file-open warning through PHP's error log.
///
/// In production builds this is a thin wrapper over
/// `ext_php_rs::error::php_error`. Under `cargo test`, the symbol
/// `php_error_docref` is not resolvable (no live PHP runtime in the
/// test binary), so the shim becomes a no-op for unit-test purposes
/// — the active-with-bad-path branch is unit-testable through
/// [`open_spike_sink`] alone, which already verifies the warning
/// string. Integration coverage of the wired-up `php_error` call
/// lives in `tests/spike_observer.rs`.
#[cfg(not(test))]
fn emit_spike_log_warning(message: &str) {
    ext_php_rs::error::php_error(&ext_php_rs::flags::ErrorType::Warning, message);
}

#[cfg(test)]
fn emit_spike_log_warning(_message: &str) {
    // Intentionally empty: see the production-path doc comment above.
}

/// Resolve the sink for an active spike. Pure: no PHP interaction;
/// returns the optional warning string the caller should pass through
/// `php_error(E_WARNING)`. Factored out so unit tests can verify the
/// behaviour without needing a live PHP runtime.
///
/// - `None` path → stderr, no warning.
/// - `Some(path)` that opens → append-mode file sink, no warning.
/// - `Some(path)` that fails to open → stderr fallback with a
///   token-free warning string for the caller to log.
fn open_spike_sink(path: Option<&Path>) -> (Box<dyn Write + Send>, Option<String>) {
    let Some(path) = path else {
        return (Box::new(io::stderr()), None);
    };

    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => (Box::new(LineWriter::new(file)), None),
        Err(e) => {
            // The fallback intentionally does NOT silently disable: a
            // spike that runs with stderr output is useful; a spike
            // that runs with no output is not.
            let message = format!(
                "php-analyze spike: failed to open spike_log_path {:?} ({e}); falling back to stderr",
                path.display(),
            );
            (Box::new(io::stderr()), Some(message))
        }
    }
}

/// Local, pure-Rust copy of `ext_php_rs::zend::FcallInfo` (which has
/// `pub fields but only `pub(crate)` constructors in 0.15.13). The
/// upstream type cannot be built from our crate, so we replicate its
/// shape and the half-dozen lines of parsing logic against the same
/// public `ffi::*` bindgen surface. If `ext-php-rs` makes the
/// constructor public in a future release, the right move is to drop
/// `LocalFcallInfo` and `extract_info` and use the upstream pair
/// directly.
#[derive(Debug, Clone)]
struct LocalFcallInfo<'a> {
    function_name: Option<&'a str>,
    class_name: Option<&'a str>,
    filename: Option<&'a str>,
    lineno: u32,
    is_internal: bool,
}

impl LocalFcallInfo<'_> {
    /// A `LocalFcallInfo` with no inner borrows. The `'static` return
    /// is the most-general lifetime: by reference covariance,
    /// `LocalFcallInfo<'static>` coerces into `LocalFcallInfo<'a>` for
    /// any `'a` at the call site, which is exactly what
    /// [`extract_info`]'s null branch needs.
    fn empty() -> LocalFcallInfo<'static> {
        LocalFcallInfo {
            function_name: None,
            class_name: None,
            filename: None,
            lineno: 0,
            is_internal: false,
        }
    }
}

/// Walk the Zend execute-data → function → (op_array | internal_function)
/// chain and pull out the four identity fields we need for the spike's
/// log shape. The structure of this walk mirrors
/// `ext_php_rs::zend::observer::FcallInfo::from_execute_data` (private)
/// — diverging from it would silently produce a different shape of
/// `<fqn>` strings, so any future change here should be cross-checked
/// against the upstream private implementation.
///
/// The returned [`LocalFcallInfo`] borrows from the Zend strings that
/// hang off `execute_data`. Tying the borrow lifetime to the input
/// (`<'a>`) keeps the borrow checker honest: a future refactor that
/// tries to stash the result past the callback's return will be a
/// compile error rather than a silent use-after-free (review finding
/// S-1 / S-6).
///
/// # Safety
///
/// `execute_data` must be a valid `&ExecuteData` such that
/// `(*execute_data).func` is either null or points at a live
/// `zend_function`, and any `zend_string` pointers in
/// `func.common.{function_name,scope->name}` and
/// `func.op_array.filename` are either null or valid for the duration
/// of the call. All of those invariants are upheld by the Zend
/// observer machinery for the duration of a `begin`/`end` callback.
unsafe fn extract_info<'a>(execute_data: &'a ExecuteData) -> LocalFcallInfo<'a> {
    let func_ptr = execute_data.func;
    if func_ptr.is_null() {
        return LocalFcallInfo::empty();
    }

    // SAFETY: `func_ptr` non-null per the check above; pointed-to
    // `zend_function` is alive for the callback's duration.
    let func = unsafe { &*func_ptr };
    // `_zend_function` is a union. The `common` arm overlaps the head
    // of every other arm; the bindgen type names this `_zend_function__bindgen_ty_1`.
    // Accessing `.common` reads the shared prefix and is well-defined.
    let common = unsafe { &func.common };
    #[allow(clippy::cast_possible_truncation)]
    let is_internal = common.type_ == ffi::ZEND_INTERNAL_FUNCTION as u8;

    let function_name = unsafe { zend_string_to_str(common.function_name) };

    let class_name = if common.scope.is_null() {
        None
    } else {
        // SAFETY: `scope` is null-checked; `name` may itself be null
        // (handled by `zend_string_to_str`).
        let ce = unsafe { &*common.scope };
        unsafe { zend_string_to_str(ce.name) }
    };

    let (filename, lineno) = if is_internal {
        (None, 0)
    } else {
        // SAFETY: for user functions the `op_array` arm of the union
        // is the active member; reading `func.op_array.filename` and
        // `.line_start` is well-defined.
        let op_array = unsafe { &func.op_array };
        let filename = unsafe { zend_string_to_str(op_array.filename) };
        (filename, op_array.line_start)
    };

    LocalFcallInfo {
        function_name,
        class_name,
        filename,
        lineno,
        is_internal,
    }
}

/// Convert a `*mut zend_string` into a borrowed `&'a str`. The
/// caller picks `'a`; at the call sites inside [`extract_info`] the
/// inference picks the lifetime of the enclosing `&'a ExecuteData`,
/// which is exactly the borrow-checker bound the Zend observer
/// surface guarantees (the `zend_string` lives at least as long as
/// the callback's `ExecuteData`).
///
/// Previously this returned `Option<&'static str>` as a deliberate
/// convenience, which silently invited use-after-free if a caller
/// retained the borrow past the callback's return. Tying the
/// lifetime to the input restores the compile-time guarantee
/// (review finding S-1).
///
/// # Safety
///
/// `zs` must be either null or a pointer to a `zend_string` whose
/// payload bytes form valid UTF-8 and remain alive for the chosen
/// `'a`. The Zend observer surface only passes us names for user
/// functions (which the parser already validated as 8-bit-clean
/// identifiers) and for filenames (path strings from disk; UTF-8-clean
/// on Linux x86_64 in practice — if not, we return `None` and the FQN
/// falls back to `(unknown)`).
unsafe fn zend_string_to_str<'a>(zs: *mut ffi::zend_string) -> Option<&'a str> {
    if zs.is_null() {
        return None;
    }
    // SAFETY: non-null checked above; payload layout per Zend ABI.
    // The lifetime `'a` is chosen by the caller and bounds how long
    // the returned slice is allowed to live.
    let len = unsafe { (*zs).len };
    let ptr = unsafe { (*zs).val.as_ptr() };
    let slice = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
    std::str::from_utf8(slice).ok()
}

/// Compose the fully-qualified name string per `design.md` §D-3. Pure;
/// takes only `LocalFcallInfo`. Categorisation order matters: an
/// `is_internal` function may carry a `function_name` but no
/// `class_name`/`filename`, while a user closure carries a
/// `function_name` of the literal form `"{closure:/path.php:42}"`.
///
/// PHP's closure naming convention (`{closure:<file>:<line>}`) is the
/// stable signal that distinguishes a closure from a regular user
/// function in the observer surface, since both report
/// `type_ != ZEND_INTERNAL_FUNCTION` and both populate
/// `function_name`. The check is a substring match rather than an
/// equality check because PHP has historically extended the format
/// (e.g. `{closure:Foo::bar():42}` for closures defined inside a
/// method); future variants stay caught.
fn fqn(info: &LocalFcallInfo) -> String {
    if info.is_internal {
        let name = info.function_name.unwrap_or("(anonymous)");
        return format!("internal:{name}");
    }

    if let Some(class) = info.class_name {
        let method = info.function_name.unwrap_or("(unknown)");
        return format!("method:{class}::{method}");
    }

    let file = info.filename.unwrap_or("(unknown)");
    let line = info.lineno;

    let is_closure = info
        .function_name
        .is_some_and(|n| n.starts_with("{closure"));
    if is_closure || info.function_name.is_none() {
        return format!("closure:{file}:{line}");
    }

    let name = info.function_name.unwrap_or("(unknown)");
    format!("function:{file}:{line}:{name}")
}

#[cfg(test)]
mod tests {
    //! Pure-Rust tests. PHP-runtime coverage lives in the integration
    //! test under `tests/spike_observer.rs`, gated by
    //! `PHP_ANALYZE_RUN_SPIKE=1`.

    use super::*;

    fn empty_local_info() -> LocalFcallInfo<'static> {
        LocalFcallInfo {
            function_name: None,
            class_name: None,
            filename: None,
            lineno: 0,
            is_internal: false,
        }
    }

    // --- fqn ---------------------------------------------------------------

    #[test]
    fn fqn_categorises_internal_calls() {
        let info = LocalFcallInfo {
            function_name: Some("strlen"),
            is_internal: true,
            ..empty_local_info()
        };
        assert_eq!(fqn(&info), "internal:strlen");
    }

    #[test]
    fn fqn_categorises_methods_with_class_and_method_names() {
        let info = LocalFcallInfo {
            function_name: Some("greet"),
            class_name: Some("Greeter"),
            filename: Some("/srv/app.php"),
            lineno: 7,
            is_internal: false,
        };
        assert_eq!(fqn(&info), "method:Greeter::greet");
    }

    #[test]
    fn fqn_categorises_closures_via_function_name_prefix() {
        // PHP reports closures with the literal `{closure...}` prefix.
        let info = LocalFcallInfo {
            function_name: Some("{closure:/srv/app.php:42}"),
            filename: Some("/srv/app.php"),
            lineno: 42,
            is_internal: false,
            class_name: None,
        };
        assert_eq!(fqn(&info), "closure:/srv/app.php:42");
    }

    #[test]
    fn fqn_categorises_closures_when_function_name_is_absent() {
        let info = LocalFcallInfo {
            filename: Some("/srv/app.php"),
            lineno: 7,
            ..empty_local_info()
        };
        assert_eq!(fqn(&info), "closure:/srv/app.php:7");
    }

    #[test]
    fn fqn_categorises_top_level_user_functions() {
        let info = LocalFcallInfo {
            function_name: Some("only_me"),
            filename: Some("/srv/user_calls.php"),
            lineno: 3,
            is_internal: false,
            class_name: None,
        };
        assert_eq!(fqn(&info), "function:/srv/user_calls.php:3:only_me");
    }

    #[test]
    fn fqn_internal_check_precedes_class_check() {
        // Internal functions never have user class scopes, but if Zend
        // ever surfaces one (e.g. internal method on an SplObject) the
        // `internal:` prefix is still the right label for v1 wire-shape
        // purposes. The check order in `fqn` is load-bearing for this.
        let info = LocalFcallInfo {
            function_name: Some("count"),
            class_name: Some("SplObjectStorage"),
            is_internal: true,
            ..empty_local_info()
        };
        assert_eq!(fqn(&info), "internal:count");
    }

    // --- SpikeObserver inactive path --------------------------------------

    #[test]
    fn should_observe_returns_false_when_observer_is_inactive() {
        let buf: Box<dyn Write + Send> = Box::new(Vec::<u8>::new());
        let observer = SpikeObserver::with_sink(buf, false);
        // `FcallInfo` content is irrelevant when active=false; we
        // construct a default for the trait call.
        let info = FcallInfo {
            function_name: None,
            class_name: None,
            filename: None,
            lineno: 0,
            is_internal: false,
        };
        assert!(!observer.should_observe(&info));
    }

    #[test]
    fn should_observe_returns_true_when_observer_is_active() {
        let buf: Box<dyn Write + Send> = Box::new(Vec::<u8>::new());
        let observer = SpikeObserver::with_sink(buf, true);
        let info = FcallInfo {
            function_name: None,
            class_name: None,
            filename: None,
            lineno: 0,
            is_internal: false,
        };
        assert!(observer.should_observe(&info));
    }

    // --- write_line wiring -------------------------------------------------
    //
    // The hot-path tests pull bytes back out of the sink via a thread-safe
    // shared `Vec<u8>` so we can assert on what was written without having
    // to drive the full `FcallObserver` trait (which requires a real
    // `ExecuteData` pointer).

    #[test]
    fn write_line_writes_a_single_terminated_line() {
        let shared: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        struct Tee(Arc<Mutex<Vec<u8>>>);
        impl Write for Tee {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let sink: Box<dyn Write + Send> = Box::new(Tee(shared.clone()));
        let observer = SpikeObserver::with_sink(sink, true);
        observer.write_line("entry: function:/x.php:1:only_me");
        observer.write_line("exit: function:/x.php:1:only_me (abnormal=false)");

        let got = String::from_utf8(shared.lock().unwrap().clone()).expect("utf-8 log");
        assert_eq!(
            got,
            "entry: function:/x.php:1:only_me\n\
             exit: function:/x.php:1:only_me (abnormal=false)\n"
        );
    }

    // --- open_spike_sink (S-2 helper) -------------------------------------
    //
    // These tests exercise the pure sink-resolution helper without
    // touching `from_config`, so no `php_error` call is invoked. The
    // helper is the unit that decides "open the file or fall back to
    // stderr"; the test contract is that a missing path produces no
    // warning and a bad path produces a token-free warning string.

    #[test]
    fn open_spike_sink_returns_stderr_and_no_warning_when_path_is_none() {
        let (_sink, warning) = open_spike_sink(None);
        assert!(warning.is_none(), "no path → no warning; got {warning:?}",);
    }

    #[test]
    fn open_spike_sink_warns_and_falls_back_when_path_cannot_be_opened() {
        // A path under a non-existent parent dir cannot be opened with
        // `O_APPEND | O_CREAT`; `OpenOptions::open` returns `ENOENT`.
        let path = Path::new("/this/path/should/not/exist/spike.log");
        let (_sink, warning) = open_spike_sink(Some(path));
        let warning = warning.expect("a warning should be produced");
        assert!(
            warning.contains("falling back to stderr"),
            "warning should mention stderr fallback, got: {warning}",
        );
        assert!(
            warning.contains("spike_log_path"),
            "warning should mention the directive name, got: {warning}",
        );
    }

    // --- from_config (S-2 gate) -------------------------------------------
    //
    // The inactive-gate test reaches `from_config` directly. Constructing
    // a `Config` via `from_ini_values` lets us avoid touching `Config`'s
    // many public fields by hand, and exercises the same data path the
    // PHP runtime would.

    #[test]
    fn from_config_with_spike_disabled_does_not_open_the_log_path() {
        // Spike is off, but a bogus path is set. Without the S-2 gate
        // this would trigger `OpenOptions::open` and then `php_error`,
        // which we'd be unable to invoke safely outside a PHP process.
        // With the gate, the constructor short-circuits to the inactive
        // sink and never touches the filesystem.
        let raw = crate::config::RawIni {
            enabled: Some(true),
            server_url: Some("https://example.com/v1/ingest".into()),
            auth_token: Some("test-token".into()),
            spike_observer: Some(false),
            spike_log_path: Some("/this/path/should/not/exist/spike.log".into()),
            ..Default::default()
        };
        let (config, _warnings) = Config::from_ini_values(&raw);
        let observer = SpikeObserver::from_config(&config);

        // Confirm the gate landed: `should_observe` is the surface
        // through which the active bit is exposed.
        let info = FcallInfo {
            function_name: None,
            class_name: None,
            filename: None,
            lineno: 0,
            is_internal: false,
        };
        assert!(!observer.should_observe(&info));
    }

    #[test]
    fn from_config_with_extension_disabled_does_not_open_the_log_path() {
        // Master switch off; spike directive still flipped on with a
        // bogus path. The `enabled && spike_observer` gate must close
        // on the disabled side and skip the file-open.
        let raw = crate::config::RawIni {
            enabled: Some(false),
            spike_observer: Some(true),
            spike_log_path: Some("/this/path/should/not/exist/spike.log".into()),
            ..Default::default()
        };
        let (config, warnings) = Config::from_ini_values(&raw);
        // Confirm the bootstrap layer would be silent (R-5 invariant).
        assert!(
            warnings.is_empty(),
            "master-switch-off must produce no bootstrap warnings; got {warnings:?}",
        );

        let observer = SpikeObserver::from_config(&config);
        let info = FcallInfo {
            function_name: None,
            class_name: None,
            filename: None,
            lineno: 0,
            is_internal: false,
        };
        assert!(!observer.should_observe(&info));
    }
}
