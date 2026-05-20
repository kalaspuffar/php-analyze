//! Configuration — populated in §3 of the `scaffold-workspace-and-config`
//! OpenSpec change. This file is intentionally a stub so the workspace
//! builds at the §1.9 checkpoint, before the dependencies and real
//! implementation land in the next commit.

/// Resolved, frozen configuration. Populated in §3.
#[derive(Debug)]
pub struct Config;

/// Warning produced while resolving the configuration. Populated in §3.
#[derive(Debug)]
pub struct ConfigWarning;

/// Raw INI values as read from the PHP INI subsystem. Populated in §3.
#[derive(Debug, Default)]
pub struct RawIni;
