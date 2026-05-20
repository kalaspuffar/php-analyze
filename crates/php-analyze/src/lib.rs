//! `php-analyze` — PHP function-call tracing extension.
//!
//! This crate compiles to a `cdylib` loaded into PHP 8.3 / 8.4 via the
//! `extension=` directive in `php.ini`. It is the first slice of Phase 1
//! from `SPECIFICATION.md`: it brings up the configuration surface and the
//! lifecycle-hook skeletons. Observer hooks, the recorder, and the shipper
//! arrive in subsequent OpenSpec changes.
//!
//! Module map:
//!
//! - [`config`] — parses, validates, range-clamps, and freezes the
//!   `php_analyze.*` directives into an immutable [`Config`] global at
//!   `MINIT`. Pure-Rust; testable without PHP.
//!
//! Subsequent changes will add a `bootstrap` module (PHP lifecycle hooks
//! and `ext-php-rs` INI registration) once `php-dev` headers are available
//! on the build host.

pub mod config;

pub use config::{Config, ConfigWarning, RawIni};
