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
//! - [`config`] — parses, validates, range-clamps, and freezes the
//!   `php_analyze.*` directives into an immutable [`Config`] global at
//!   `MINIT`. Pure-Rust; testable without PHP headers.
//! - [`bootstrap`] — PHP lifecycle hooks (`MINIT`/`MSHUTDOWN`/`RINIT`/
//!   `RSHUTDOWN`/`MINFO`) and `php.ini` directive registration via
//!   `ext-php-rs`. The only file in the crate that depends on `ext-php-rs`.
//! - [`spike`] — Phase-0 spike: an `FcallObserver` that logs every
//!   begin/end event to a configurable destination. Off by default
//!   (`php_analyze.spike_observer = 0`). Removed by Phase 2's Recorder
//!   change.

pub mod bootstrap;
pub mod config;
pub mod spike;

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
/// §D-1 Resolution). The factory consults `Config::global()` to build
/// a [`spike::SpikeObserver`] whose `should_observe` is `false` for
/// every input when either gate (`enabled`, `spike_observer`) is
/// closed — so a default-configured load installs an observer that
/// returns `false` once per unique function and does nothing else.
fn build_spike_observer() -> spike::SpikeObserver {
    // `Config::global()` is `Some` at this point per the wiring
    // documented above; the `expect` here is a load-bearing invariant
    // that we'd want to know about loudly if it ever broke.
    let config = Config::global().expect(
        "Config::global() must be populated before observer factory fires; check startup wiring",
    );
    spike::SpikeObserver::from_config(config)
}

#[php_module]
#[php(startup = startup)]
pub fn get_module(module: ModuleBuilder) -> ModuleBuilder {
    module
        .name("php_analyze")
        .shutdown_function(bootstrap::mshutdown)
        .request_startup_function(bootstrap::rinit)
        .request_shutdown_function(bootstrap::rshutdown)
        .info_function(bootstrap::minfo)
        .fcall_observer(build_spike_observer)
}
