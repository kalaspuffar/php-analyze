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
    /// File-open failures are logged via `php_error(E_WARNING)` and
    /// the spike falls back to stderr. The active bit stays `true` in
    /// that case: a fallback-to-stderr is preferable to silently
    /// disabling a developer-requested spike.
    pub fn from_config(config: &Config) -> Self {
        let active = config.enabled && config.spike_observer;
        let sink: Box<dyn Write + Send> = if let Some(path) = config.spike_log_path.as_ref() {
            match OpenOptions::new().create(true).append(true).open(path) {
                Ok(file) => Box::new(LineWriter::new(file)),
                Err(e) => {
                    // The fallback intentionally does NOT silently
                    // disable: a spike that runs with stderr output is
                    // useful; a spike that runs with no output is not.
                    let message = format!(
                        "php-analyze spike: failed to open spike_log_path {:?} ({e}); falling back to stderr",
                        path.display(),
                    );
                    ext_php_rs::error::php_error(&ext_php_rs::flags::ErrorType::Warning, &message);
                    Box::new(io::stderr())
                }
            }
        } else {
            Box::new(io::stderr())
        };
        Self {
            sink: Arc::new(Mutex::new(sink)),
            active,
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
/// # Safety
///
/// `execute_data` must be a valid `&ExecuteData` such that
/// `(*execute_data).func` is either null or points at a live
/// `zend_function`, and any `zend_string` pointers in
/// `func.common.{function_name,scope->name}` and
/// `func.op_array.filename` are either null or valid for the duration
/// of the call. All of those invariants are upheld by the Zend
/// observer machinery for the duration of a `begin`/`end` callback.
unsafe fn extract_info(execute_data: &ExecuteData) -> LocalFcallInfo<'static> {
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

/// Convert a `*mut zend_string` into a borrowed `&'static str`. The
/// `'static` lifetime is a convenient fiction: the borrow is only
/// valid for the duration of the observer callback, but every consumer
/// in this module consumes the borrow inside the same callback and
/// does not retain it.
///
/// # Safety
///
/// `zs` must be either null or a pointer to a `zend_string` whose
/// payload bytes form valid UTF-8. The Zend observer surface only
/// passes us names for user functions (which the parser already
/// validated as 8-bit-clean identifiers) and for filenames
/// (path strings from disk; UTF-8-clean on Linux x86_64 in practice —
/// if not, we return `None` and the FQN falls back to `(unknown)`).
unsafe fn zend_string_to_str(zs: *mut ffi::zend_string) -> Option<&'static str> {
    if zs.is_null() {
        return None;
    }
    // SAFETY: non-null checked above; payload layout per Zend ABI.
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
}
