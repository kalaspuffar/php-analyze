//! `php-analyze` — PHP function-call tracing extension.
//!
//! This crate compiles to a `cdylib` loaded into PHP 8.3 / 8.4 via the
//! `extension=` directive in `php.ini`. It is the first slice of Phase 1
//! from `SPECIFICATION.md`: it brings up the configuration surface and
//! the lifecycle-hook skeletons. Observer hooks, the recorder, and the
//! shipper arrive in subsequent OpenSpec changes.
//!
//! Module map:
//!
//! - [`bootstrap`] — PHP lifecycle hooks (`MINIT`/`MSHUTDOWN`/`RINIT`/
//!   `RSHUTDOWN`/`MINFO`) and `php.ini` directive registration via
//!   `ext-php-rs`. The only file in the crate that depends on `ext-php-rs`.
//! - [`clocks`] — POSIX `clock_gettime` / `getrusage` wrappers and a
//!   Zend `zend_memory_usage` wrapper, returning `i64` nanoseconds or
//!   bytes. Substrate for the recorder hot path; the Zend wrapper is
//!   `cfg(test)`-stubbed so unit tests link without PHP.
//! - [`config`] — parses, validates, range-clamps, and freezes the
//!   `php_analyze.*` directives into an immutable [`Config`] global at
//!   `MINIT`. Pure-Rust; testable without PHP headers.
//! - [`recorder`] — per-trace in-memory data model (`Trace`,
//!   `CallFrame`, `CallRecord`, `DictEntry`, …), the function-
//!   dictionary interner (`Dictionary`), the production `Recorder`
//!   (`FcallObserver` impl), the `BootObserver` dispatcher, and the
//!   `RINIT`/`RSHUTDOWN` lifecycle entry points.
//! - [`spike`] — Phase-0 spike: an `FcallObserver` that logs every
//!   begin/end event to a configurable destination. Off by default
//!   (`php_analyze.spike_observer = 0`). Reached only through the
//!   `BootObserver::Spike` variant; production loads with the default
//!   directive set route through `BootObserver::Recorder`.
//! - [`wire`] — serde-derived types matching `SPECIFICATION.md` §4.2
//!   (the MessagePack batch schema the Phase-4 shipper will encode and
//!   the `stub-ingest` crate decodes). Production-side encode-only in
//!   this slice: no Recorder→Wire conversion until Phase 4.

pub mod bootstrap;
pub mod clocks;
pub mod config;
pub mod recorder;
pub mod spike;
pub mod wire;

pub use config::initialise_from_ini;
pub use config::{Config, ConfigError, ConfigWarning, DisableReason, RawIni, TokenSource};

use ext_php_rs::prelude::*;

/// User-side `MINIT` shim invoked by the `#[php_module]` macro before its
/// own auto-generated startup runs. Returning non-zero would abort PHP
/// startup; [`bootstrap::startup`] is contractually fixed to always
/// return zero per the silent-disable posture.
fn startup(ty: i32, mod_num: i32) -> i32 {
    bootstrap::startup(ty, mod_num)
}

/// Module entry. PHP looks up the exported `get_module` symbol generated
/// by `#[php_module]` and reads the resulting `ModuleEntry` to discover
/// the lifecycle hooks. The module's PHP-visible name is forced to
/// `php_analyze` (with an underscore) so `--ri php_analyze` works,
/// regardless of the Cargo package name `php-analyze`.
///
/// The `fcall_observer` factory runs once at `MINIT`, **after** our
/// `startup` shim has populated `Config::global()` (this ordering is
/// load-bearing; see `openspec/changes/spike-zend-observer/design.md`
/// §D-1 Resolution). [`recorder::build_boot_observer`] consults
/// `Config::global()` to build a [`recorder::BootObserver`] that
/// dispatches to (a) the recorder when `enabled && !spike_observer`,
/// (b) the spike when `enabled && spike_observer`, (c) a no-op
/// `Disabled` variant otherwise.
#[php_module]
#[php(startup = startup)]
pub fn get_module(module: ModuleBuilder) -> ModuleBuilder {
    module
        .name("php_analyze")
        .shutdown_function(bootstrap::mshutdown)
        .request_startup_function(bootstrap::rinit)
        .request_shutdown_function(bootstrap::rshutdown)
        .info_function(bootstrap::minfo)
        .fcall_observer(recorder::build_boot_observer)
}
