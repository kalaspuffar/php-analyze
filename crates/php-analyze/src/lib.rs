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

pub mod bootstrap;
pub mod config;

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
#[php_module]
#[php(startup = startup)]
pub fn get_module(module: ModuleBuilder) -> ModuleBuilder {
    module
        .name("php_analyze")
        .shutdown_function(bootstrap::mshutdown)
        .request_startup_function(bootstrap::rinit)
        .request_shutdown_function(bootstrap::rshutdown)
        .info_function(bootstrap::minfo)
}
