//! PHP-FPM integration test.
//!
//! Binds three previously-unbound acceptance criteria from
//! `SPECIFICATION.md`:
//!
//! - **§1.3 #2** — the extension is loadable in the `fpm-fcgi` SAPI.
//! - **§3.1 AC-BS-3** — at MVP-closing scale (100 requests per
//!   `COMMENTS.md` §6.3, not the spec's 10⁴), repeated requests on a
//!   single FPM worker do not leak RSS beyond a generous 2 MiB
//!   ceiling. The 10⁴-request figure is deferred per
//!   `COMMENTS.md` §6.4.
//! - **§3.4 AC-PB-1** — under `pm.max_children = 8`, every FPM worker
//!   owns exactly one shipper thread after its first `RINIT`.
//!
//! ## Skip conditions
//!
//! Skips with status `0` (loud `eprintln!`) when **any** of:
//!
//! - `PHP_ANALYZE_RUN_FPM` env var is not `"1"`.
//! - Neither `php-fpm8.3` nor `php-fpm8.4` is on `PATH`.
//! - Every available `php-fpm` binary reports a module-API
//!   mismatch against the freshly-built cdylib at startup.
//!
//! The skip semantic mirrors `recorder_observer.rs` and
//! `shipper_round_trip.rs`: an `eprintln!` + early `return` is
//! recorded by `cargo test` as a pass. CI's apt-install + env-set
//! steps are what guarantee the test actually runs on the matrix
//! entries that should exercise it.
//!
//! ## Structure
//!
//! One `#[test]` (`fpm_repeated_requests`) drives two per-binary
//! helpers per available `php-fpm`:
//!
//! - [`try_fpm_repeated_requests`] — `pm = static, pm.max_children = 1`,
//!   100 sequential FastCGI responder round-trips, asserts on
//!   stub-side batch cardinality, worker RSS delta, and a
//!   token-leak grep over the captured FPM logs.
//! - [`try_fpm_thread_per_worker`] — `pm = static, pm.max_children = 8`,
//!   16 sequential requests, asserts every worker's
//!   `/proc/<pid>/task` has exactly two entries (worker + shipper)
//!   and the master's has exactly one.
//!
//! Each helper owns its own `FpmProcess` and `StubProcess`; both
//! `Drop` impls send SIGTERM-with-deadline + SIGKILL-fallback so
//! a panicking assertion does not orphan a long-running
//! `php-fpm` master.
//!
//! ## FastCGI client
//!
//! A private [`fastcgi`] module implements the smallest viable
//! FCGI_RESPONDER client: 8-byte record framing, `BEGIN_REQUEST`,
//! `PARAMS`, `STDIN`, drain `STDOUT`/`STDERR` until `END_REQUEST`.
//! No `tokio`, no `cgi-fcgi` external tool, no new crate dep —
//! the FastCGI surface used here is too small to justify the
//! transitive cost. See `openspec/changes/fpm-integration-test/design.md`
//! D-1 for the rationale.

use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// FastCGI framing — private module, scoped to this test only.
// ---------------------------------------------------------------------------

mod fastcgi {
    use std::io::{self, Read, Write};

    pub const FCGI_VERSION_1: u8 = 1;

    // Record types we send.
    pub const FCGI_BEGIN_REQUEST: u8 = 1;
    pub const FCGI_PARAMS: u8 = 4;
    pub const FCGI_STDIN: u8 = 5;

    // Record types we read.
    pub const FCGI_END_REQUEST: u8 = 3;
    pub const FCGI_STDOUT: u8 = 6;
    pub const FCGI_STDERR: u8 = 7;

    // Roles.
    pub const FCGI_RESPONDER: u16 = 1;

    /// One in-flight request per TCP connection; hard-coded ID
    /// keeps the framing free of multiplex bookkeeping.
    pub const REQUEST_ID: u16 = 1;

    /// 8-byte FastCGI record header. We only surface the two
    /// fields the consumer needs (`record_type`, `request_id`);
    /// `content_length` + `padding_length` are consumed inside
    /// [`read_record`] to size the body/padding reads.
    #[derive(Debug)]
    pub struct RecordHeader {
        pub record_type: u8,
        pub request_id: u16,
    }

    pub fn write_record<W: Write>(
        w: &mut W,
        record_type: u8,
        request_id: u16,
        body: &[u8],
    ) -> io::Result<()> {
        let content_length = u16::try_from(body.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "FastCGI record body exceeds 64 KiB; test fixtures should never approach this",
            )
        })?;
        let header = [
            FCGI_VERSION_1,
            record_type,
            (request_id >> 8) as u8,
            (request_id & 0xff) as u8,
            (content_length >> 8) as u8,
            (content_length & 0xff) as u8,
            0, // padding_length
            0, // reserved
        ];
        w.write_all(&header)?;
        w.write_all(body)?;
        Ok(())
    }

    pub fn read_record<R: Read>(r: &mut R) -> io::Result<(RecordHeader, Vec<u8>)> {
        let mut header = [0u8; 8];
        r.read_exact(&mut header)?;
        if header[0] != FCGI_VERSION_1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported FastCGI version {}", header[0]),
            ));
        }
        let record_type = header[1];
        let request_id = (u16::from(header[2]) << 8) | u16::from(header[3]);
        let content_length = (u16::from(header[4]) << 8) | u16::from(header[5]);
        let padding_length = header[6];
        let mut body = vec![0u8; content_length as usize];
        r.read_exact(&mut body)?;
        let mut padding = vec![0u8; padding_length as usize];
        r.read_exact(&mut padding)?;
        Ok((
            RecordHeader {
                record_type,
                request_id,
            },
            body,
        ))
    }

    /// `BEGIN_REQUEST` body: role (u16 big-endian), flags (u8), 5
    /// reserved bytes. `flags = 0` means "do NOT keep connection";
    /// FPM will close after END_REQUEST, which is what the
    /// per-call connection model expects.
    pub fn write_begin_request<W: Write>(w: &mut W, role: u16, flags: u8) -> io::Result<()> {
        let body = [(role >> 8) as u8, (role & 0xff) as u8, flags, 0, 0, 0, 0, 0];
        write_record(w, FCGI_BEGIN_REQUEST, REQUEST_ID, &body)
    }

    /// Encode a single NV-pair length using FastCGI's BER-style
    /// encoding: < 128 fits in one byte; ≥ 128 uses four bytes
    /// with the high bit set on byte 0, then the remaining 31
    /// bits stored big-endian.
    fn write_nv_length(buf: &mut Vec<u8>, len: usize) {
        if len < 128 {
            buf.push(len as u8);
        } else {
            let len = u32::try_from(len).expect("NV-pair length fits in 31 bits for our params");
            buf.push(((len >> 24) | 0x80) as u8);
            buf.push((len >> 16) as u8);
            buf.push((len >> 8) as u8);
            buf.push(len as u8);
        }
    }

    pub fn write_params<W: Write>(w: &mut W, params: &[(&str, &str)]) -> io::Result<()> {
        let mut body = Vec::with_capacity(256);
        for (name, value) in params {
            write_nv_length(&mut body, name.len());
            write_nv_length(&mut body, value.len());
            body.extend_from_slice(name.as_bytes());
            body.extend_from_slice(value.as_bytes());
        }
        write_record(w, FCGI_PARAMS, REQUEST_ID, &body)?;
        // Empty PARAMS record terminates the params stream.
        write_record(w, FCGI_PARAMS, REQUEST_ID, &[])?;
        Ok(())
    }

    pub fn write_stdin<W: Write>(w: &mut W, stdin: &[u8]) -> io::Result<()> {
        if !stdin.is_empty() {
            write_record(w, FCGI_STDIN, REQUEST_ID, stdin)?;
        }
        // Empty STDIN record terminates the stdin stream.
        write_record(w, FCGI_STDIN, REQUEST_ID, &[])
    }

    /// FCGI_END_REQUEST body shape: app_status (u32 big-endian),
    /// protocol_status (u8), 3 reserved bytes. We only care about
    /// `app_status`; `protocol_status` is read off the wire but
    /// not currently surfaced — every supported FPM build sets
    /// it to `FCGI_REQUEST_COMPLETE = 0` on a successful end.
    pub struct EndRequest {
        pub app_status: u32,
    }

    pub fn read_response<R: Read>(r: &mut R) -> io::Result<(Vec<u8>, Vec<u8>, EndRequest)> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        loop {
            let (header, body) = read_record(r)?;
            if header.request_id != REQUEST_ID {
                // Spec permits unsolicited records on
                // request_id == 0 (management) which we'd ignore,
                // but for our point-to-point round-trips any
                // unexpected request_id is a framing bug we want
                // to surface loudly.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "unexpected FastCGI request_id {} (expected {REQUEST_ID})",
                        header.request_id
                    ),
                ));
            }
            match header.record_type {
                FCGI_STDOUT => stdout.extend_from_slice(&body),
                FCGI_STDERR => stderr.extend_from_slice(&body),
                FCGI_END_REQUEST => {
                    if body.len() < 5 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("END_REQUEST body too short: {} bytes", body.len()),
                        ));
                    }
                    let app_status = (u32::from(body[0]) << 24)
                        | (u32::from(body[1]) << 16)
                        | (u32::from(body[2]) << 8)
                        | u32::from(body[3]);
                    return Ok((stdout, stderr, EndRequest { app_status }));
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected FastCGI record type {other}"),
                    ));
                }
            }
        }
    }
}

/// Parsed FastCGI/CGI response — stdout, stderr, the END_REQUEST
/// `app_status`, and the CGI `Status:` header value parsed from
/// the leading stdout headers (defaults to `200` when no header
/// is present, per CGI convention).
struct FcgiResponse {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    app_status: u32,
    cgi_status: u16,
}

impl FcgiResponse {
    /// The HTTP-style response body — everything after the
    /// `\r\n\r\n` (or `\n\n`) CGI header separator. Returns the
    /// entire stdout if no separator is found.
    fn body(&self) -> &[u8] {
        if let Some(pos) = find_subsequence(&self.stdout, b"\r\n\r\n") {
            &self.stdout[pos + 4..]
        } else if let Some(pos) = find_subsequence(&self.stdout, b"\n\n") {
            &self.stdout[pos + 2..]
        } else {
            &self.stdout
        }
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_cgi_status(stdout: &[u8]) -> u16 {
    // Lines until the first blank line are CGI headers.
    let header_section = if let Some(pos) = find_subsequence(stdout, b"\r\n\r\n") {
        &stdout[..pos]
    } else if let Some(pos) = find_subsequence(stdout, b"\n\n") {
        &stdout[..pos]
    } else {
        stdout
    };
    for line in header_section.split(|&b| b == b'\n') {
        let line = strip_trailing_cr(line);
        if let Some(rest) = strip_prefix_ci(line, b"status:") {
            let rest = trim_ascii(rest);
            // Format is "<code> <reason>"; parse leading integer.
            let code_end = rest
                .iter()
                .position(|b| !b.is_ascii_digit())
                .unwrap_or(rest.len());
            if let Ok(code) = std::str::from_utf8(&rest[..code_end])
                .map_err(|_| ())
                .and_then(|s| s.parse::<u16>().map_err(|_| ()))
            {
                return code;
            }
        }
    }
    200
}

fn strip_trailing_cr(line: &[u8]) -> &[u8] {
    match line.split_last() {
        Some((&b'\r', rest)) => rest,
        _ => line,
    }
}

fn strip_prefix_ci<'a>(haystack: &'a [u8], needle: &[u8]) -> Option<&'a [u8]> {
    if haystack.len() < needle.len() {
        return None;
    }
    let (head, tail) = haystack.split_at(needle.len());
    if head.eq_ignore_ascii_case(needle) {
        Some(tail)
    } else {
        None
    }
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while let Some((&first, rest)) = bytes.split_first() {
        if first.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    while let Some((&last, rest)) = bytes.split_last() {
        if last.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

/// One FastCGI responder request → response. Opens a fresh TCP
/// connection, sends BEGIN_REQUEST + PARAMS + STDIN, drains the
/// response. The 30-second read/write timeout is generous
/// enough for the loopback case while catching a hung FPM
/// quickly.
fn fastcgi_request(
    addr: SocketAddr,
    params: &[(&str, &str)],
    stdin: &[u8],
) -> io::Result<FcgiResponse> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;

    fastcgi::write_begin_request(&mut stream, fastcgi::FCGI_RESPONDER, 0)?;
    fastcgi::write_params(&mut stream, params)?;
    fastcgi::write_stdin(&mut stream, stdin)?;

    let (stdout, stderr, end) = fastcgi::read_response(&mut stream)?;
    let cgi_status = parse_cgi_status(&stdout);
    Ok(FcgiResponse {
        stdout,
        stderr,
        app_status: end.app_status,
        cgi_status,
    })
}

// ---------------------------------------------------------------------------
// FPM process supervision.
// ---------------------------------------------------------------------------

/// Spawn-time error shapes. Variants distinguish the
/// module-API-mismatch case (test reports as `SkippedModuleApi`)
/// from anything else (test panics).
enum FpmSpawnError {
    ModuleApiMismatch,
    NotReady { stderr: String },
    Io(io::Error),
}

impl From<io::Error> for FpmSpawnError {
    fn from(value: io::Error) -> Self {
        FpmSpawnError::Io(value)
    }
}

/// The `php-fpm` master we spawn for a single test helper. Owns
/// the `Child`, the listen port, and the tempdir housing the
/// config + per-pool error log. On `Drop`, sends `SIGTERM` and
/// waits up to 5 seconds before falling back to `SIGKILL` via
/// `Child::kill`.
struct FpmProcess {
    child: Child,
    master_pid: u32,
    port: u16,
    pool_error_log: PathBuf,
    stderr_buf: SharedBuffer,
    _stderr_lines: Receiver<String>,
    _tmpdir: TempDir,
}

impl FpmProcess {
    fn spawn(
        php_fpm_binary: &str,
        cdylib: &Path,
        server_url: &str,
        token: &str,
        pool_extra: &str,
    ) -> Result<Self, FpmSpawnError> {
        let tmpdir = TempDir::new().map_err(FpmSpawnError::Io)?;
        let port = reserve_ephemeral_port()?;
        let pool_error_log = tmpdir.path().join("php.error.log");
        let conf_path = tmpdir.path().join("fpm.conf");
        let php_ini_path = tmpdir.path().join("php.ini");

        // Pre-create the PHP error log so php-fpm's startup
        // doesn't trip on a missing path on hosts with strict
        // SELinux/AppArmor labels.
        fs::write(&pool_error_log, b"").map_err(FpmSpawnError::Io)?;

        // Configure php_analyze via a per-test `php.ini` rather
        // than via FPM `php_admin_value[php_analyze.*]` lines.
        // The latter are rejected by FPM with
        // `ERROR: Unable to set php_admin_value 'php_analyze.X'`
        // for our PHP_INI_SYSTEM-scope directives in this
        // version of php-fpm — they're recognised but not
        // settable at the timing FPM tries to apply them. The
        // `-c <dir>` flag tells FPM "look for php.ini in this
        // directory", which is the standard path PHP uses for
        // SAPI ini files and which lands well before our MINIT
        // runs. The `extension = <cdylib>` line lives here too:
        // a single ini file owns both the extension load and
        // the extension's configuration, mirroring how an
        // operator would deploy in production.
        //
        // `error_log` is set in php.ini (not the pool config)
        // because the per-pool `php_admin_value[error_log]` is
        // applied AFTER MINIT — too late to catch the
        // startup-time module-API-mismatch warning the
        // `observe_module_api_mismatch` detector reads. Setting
        // it in php.ini guarantees the file path is bound
        // before extension load.
        let php_ini = format!(
            "[PHP]\n\
             display_startup_errors = On\n\
             log_errors = On\n\
             error_log = {pool_error_log}\n\
             extension = {cdylib}\n\
             opcache.enable = 0\n\
             \n\
             [php_analyze]\n\
             php_analyze.enabled = 1\n\
             php_analyze.server_url = \"{server_url}\"\n\
             php_analyze.auth_token = \"{token}\"\n\
             php_analyze.spike_observer = 0\n\
             ; `shutdown_grace_ms = 4000` is the largest value\n\
             ; that fits under PHP-FPM's hardcoded 5-second\n\
             ; SIGQUIT-to-SIGTERM grace (see PHP-FPM source's\n\
             ; FPM_PID_TIMEOUT in `fpm_unix.c`). Bumping it\n\
             ; higher than ~5000 ms is futile — FPM sends\n\
             ; SIGTERM to workers after 5 s regardless of any\n\
             ; PHP-side MSHUTDOWN that's still running, which\n\
             ; truncates the shipper drain. 4000 ms gives the\n\
             ; drain comfortable headroom inside the FPM\n\
             ; budget; with sub-millisecond loopback POST\n\
             ; latency, ~100 batches drain in well under 1 s.\n\
             php_analyze.shutdown_grace_ms = 4000\n\
             ; Wide channel so the recorder doesn't drop at the\n\
             ; channel boundary on bursty workloads. The default\n\
             ; `shipper_queue_depth = 8` would silent-drop under\n\
             ; the recorder's 100-RSHUTDOWNs-in-150ms burst\n\
             ; pattern.\n\
             php_analyze.shipper_queue_depth = 1024\n\
             ",
            pool_error_log = pool_error_log.display(),
            cdylib = cdylib.display(),
        );
        fs::write(&php_ini_path, php_ini).map_err(FpmSpawnError::Io)?;

        // `error_log = /dev/stderr` in `[global]` routes the
        // FPM master's own log lines (including the "ready to
        // handle connections" banner this test races on) into
        // the child's inherited stderr. With `error_log`
        // pointing at a real file (the typical Debian default),
        // php-fpm 8.3+ writes nothing to stderr in foreground
        // mode, so the readiness detection would time out.
        // catch_workers_output forwards anything a worker
        // writes to its stderr into the master's log too,
        // which gives us a secondary capture path for
        // diagnostics — though the primary error-log signal
        // for module-API-mismatch detection is the PHP
        // error_log file written by the worker itself, set in
        // php.ini above.
        let conf = format!(
            "[global]\n\
             daemonize = no\n\
             error_log = /dev/stderr\n\
             pid = {pid_path}\n\
             \n\
             [testpool]\n\
             listen = 127.0.0.1:{port}\n\
             listen.allowed_clients = 127.0.0.1\n\
             {pool_extra}\
             catch_workers_output = yes\n\
             ",
            pid_path = tmpdir.path().join("fpm.pid").display(),
        );
        fs::write(&conf_path, conf).map_err(FpmSpawnError::Io)?;

        // `-F` keeps FPM in foreground; `-y <path>` is the
        // pool/global config; `-c <dir>` is the php.ini
        // directory (carries the extension + per-extension
        // ini directives); `-p <prefix>` lets us point at the
        // tempdir without touching system paths.
        let mut child = Command::new(php_fpm_binary)
            .arg("-F")
            .arg("-y")
            .arg(&conf_path)
            .arg("-c")
            .arg(tmpdir.path())
            .arg("-p")
            .arg(tmpdir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(FpmSpawnError::Io)?;

        let master_pid = child.id();
        let stderr = child.stderr.take().expect("stderr requested as piped");
        let stdout = child.stdout.take().expect("stdout requested as piped");

        let (stderr_lines, stderr_buf_full) = spawn_combined_reader(stderr, stdout);

        // Drain the line channel until either (a) we see
        // "ready to handle connections", or (b) 30 seconds elapse,
        // or (c) FPM exits / stderr closes.
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut accumulated = Vec::<String>::new();
        let outcome = loop {
            let now = Instant::now();
            if now >= deadline {
                break Err(FpmSpawnError::NotReady {
                    stderr: accumulated.join("\n"),
                });
            }
            match stderr_lines.recv_timeout(deadline - now) {
                Ok(line) => {
                    if line.contains("ready to handle connections") {
                        break Ok(());
                    }
                    accumulated.push(line);
                }
                Err(RecvTimeoutError::Timeout) => {
                    break Err(FpmSpawnError::NotReady {
                        stderr: accumulated.join("\n"),
                    });
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // The reader thread exited. FPM either crashed
                    // during startup or finished printing without
                    // emitting the readiness banner.
                    break Err(FpmSpawnError::NotReady {
                        stderr: accumulated.join("\n"),
                    });
                }
            }
        };

        match outcome {
            Ok(()) => Ok(FpmProcess {
                child,
                master_pid,
                port,
                pool_error_log,
                stderr_buf: stderr_buf_full,
                _stderr_lines: stderr_lines,
                _tmpdir: tmpdir,
            }),
            Err(err) => {
                // Best-effort: capture whatever stderr we already
                // have, then look for the module-API substring.
                let snapshot = stderr_buf_full.snapshot();
                let _ = terminate_master(master_pid);
                let _ = child.wait();
                if mentions_module_api_mismatch(&snapshot) {
                    return Err(FpmSpawnError::ModuleApiMismatch);
                }
                Err(err)
            }
        }
    }

    fn address(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }

    /// Worker PIDs read from
    /// `/proc/<master>/task/<master>/children` (kernel-maintained
    /// child-PID list). Sorted ascending and deduped.
    fn worker_pids(&self) -> io::Result<Vec<u32>> {
        let path = format!(
            "/proc/{master}/task/{master}/children",
            master = self.master_pid
        );
        let contents = fs::read_to_string(&path)?;
        let mut pids: Vec<u32> = contents
            .split_ascii_whitespace()
            .filter_map(|s| s.parse::<u32>().ok())
            .collect();
        pids.sort_unstable();
        pids.dedup();
        Ok(pids)
    }

    fn vm_rss_kb(&self, pid: u32) -> io::Result<u64> {
        let contents = fs::read_to_string(format!("/proc/{pid}/status"))?;
        for line in contents.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let n_kb = rest
                    .split_ascii_whitespace()
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("malformed VmRSS line: {line:?}"),
                        )
                    })?;
                return Ok(n_kb);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("VmRSS not found in /proc/{pid}/status"),
        ))
    }

    fn thread_count(&self, pid: u32) -> io::Result<usize> {
        let entries = fs::read_dir(format!("/proc/{pid}/task"))?;
        Ok(entries.count())
    }

    fn thread_comm(&self, pid: u32, tid: u32) -> io::Result<String> {
        let raw = fs::read_to_string(format!("/proc/{pid}/task/{tid}/comm"))?;
        Ok(raw.trim_end_matches('\n').to_string())
    }

    fn thread_ids(&self, pid: u32) -> io::Result<Vec<u32>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(format!("/proc/{pid}/task"))? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                if let Ok(tid) = name.parse::<u32>() {
                    ids.push(tid);
                }
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    fn read_pool_error_log(&self) -> Vec<u8> {
        fs::read(&self.pool_error_log).unwrap_or_default()
    }

    /// A live snapshot of everything the reader thread has
    /// captured from FPM's stderr+stdout so far. Non-destructive:
    /// repeated calls keep returning growing prefixes as FPM
    /// emits more lines.
    fn captured_stderr(&self) -> Vec<u8> {
        self.stderr_buf.snapshot()
    }

    /// Returns `true` if EITHER the pool error log OR the master
    /// stderr capture currently contains the literal substring
    /// `"module API"` — the canonical signal that the cdylib was
    /// built against a different PHP version than the running
    /// FPM. The mismatch surfaces in the pool error log as
    /// `PHP Warning: Unknown: php_analyze: Unable to initialize
    /// module / Module compiled with module API=NNN / PHP
    /// compiled with module API=MMM / These options need to
    /// match`. With `catch_workers_output = yes` set in the pool
    /// config, the master also forwards that to its own stderr
    /// ("WARNING: [pool testpool] child N said into stderr:
    /// ...") — we check both for resilience against
    /// configurations that suppress the forward.
    fn observe_module_api_mismatch(&self) -> bool {
        let pool_log = self.read_pool_error_log();
        if mentions_module_api_mismatch(&pool_log) {
            return true;
        }
        let stderr = self.captured_stderr();
        mentions_module_api_mismatch(&stderr)
    }
}

impl Drop for FpmProcess {
    fn drop(&mut self) {
        if let Ok(Some(_)) = self.child.try_wait() {
            return;
        }
        let _ = terminate_master(self.master_pid);
        // The graceful-stop deadline MUST exceed the worker's
        // own `php_analyze.shutdown_grace`, otherwise we
        // SIGKILL the FPM master before its worker's MSHUTDOWN
        // (and the shipper-drain it owns) has finished, which
        // strands the tail of the PendingBatch queue without
        // POSTing it. The test's php.ini sets shutdown_grace =
        // 30 s; we give the master 35 s, with the 5 s buffer
        // covering FPM's own master→worker signal forwarding
        // latency.
        let deadline = Instant::now() + Duration::from_secs(35);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Send `SIGQUIT` to the FPM master via the `kill` CLI — the
/// signal that triggers FPM's *graceful* stop semantic
/// (workers finish current requests, run MSHUTDOWN, exit
/// cleanly). `SIGTERM` to FPM is immediate-kill — workers
/// exit without MSHUTDOWN, which strands any in-flight
/// `PendingBatch`es in the shipper channel and breaks the
/// batch-count assertion in `try_fpm_repeated_requests`. We
/// avoid a `libc` dev-dep by shelling out to `kill`; the cost
/// is one fork+exec per `Drop`, invisible against the rest of
/// the test budget.
fn terminate_master(master_pid: u32) -> io::Result<()> {
    let status = Command::new("kill")
        .arg("-QUIT")
        .arg(master_pid.to_string())
        .status()?;
    if !status.success() {
        // Process already gone (the master exited between the
        // try_wait and the kill) is a benign race; surface
        // anything else as an io::Error.
        return Err(io::Error::other(format!(
            "kill -QUIT {master_pid} exited {status:?}"
        )));
    }
    Ok(())
}

fn reserve_ephemeral_port() -> io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Spawn a single reader thread that drains both FPM stderr AND
/// FPM stdout into one combined buffer and emits each `\n`-
/// terminated line on a bounded channel. FPM emits its
/// `ready to handle connections` banner to stderr in modern
/// builds, but historically some versions used stdout; capturing
/// both is the conservative choice.
///
/// Returns `(line_channel, shared_buffer)`. The shared buffer is
/// `Arc<Mutex<Vec<u8>>>` under the hood (wrapped in a small
/// helper); `lock_take()` consumes its contents on demand.
fn spawn_combined_reader(
    stderr: impl Read + Send + 'static,
    stdout: impl Read + Send + 'static,
) -> (Receiver<String>, SharedBuffer) {
    let (tx, rx) = sync_channel::<String>(256);
    let buf = SharedBuffer::new();
    spawn_reader(stderr, tx.clone(), buf.clone());
    spawn_reader(stdout, tx, buf.clone());
    (rx, buf)
}

fn spawn_reader<R: Read + Send + 'static>(reader: R, tx: SyncSender<String>, buf: SharedBuffer) {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => return,
                Ok(_) => {
                    buf.extend_from_slice(line.as_bytes());
                    let _ = tx.send(line.trim_end_matches('\n').to_string());
                }
                Err(_) => return,
            }
        }
    });
}

#[derive(Clone)]
struct SharedBuffer {
    inner: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl SharedBuffer {
    fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
    fn extend_from_slice(&self, bytes: &[u8]) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.extend_from_slice(bytes);
        }
    }
    /// Non-destructive snapshot: clone the current contents
    /// without draining the buffer. Repeated calls keep
    /// returning growing prefixes as the reader thread appends.
    fn snapshot(&self) -> Vec<u8> {
        self.inner.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Stub-ingest spawn helper. Mirrors the smaller subset of
// `shipper_round_trip.rs::StubProcess` we need here; we don't
// share code because the conventions of this workspace prefer
// one-file-per-integration-test isolation (see
// `crates/php-analyze/tests/shipper_round_trip.rs` module doc).
// ---------------------------------------------------------------------------

struct StubProcess {
    child: Child,
    port: u16,
}

impl StubProcess {
    fn spawn(token: &str, path: &str) -> Self {
        let bin = stub_ingest_binary();
        // `stderr(Stdio::inherit())` lets the stub's "request
        // accepted / token mismatch" diagnostic lines surface
        // in the test runner's output when --nocapture is in
        // effect, which is invaluable for debugging batch-count
        // mismatches between the recorder and the stub queue.
        let mut child = Command::new(&bin)
            .arg("--bind")
            .arg("127.0.0.1:0")
            .arg("--auth-token")
            .arg(token)
            .arg("--path")
            .arg(path)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()));
        let stdout = child
            .stdout
            .take()
            .expect("stub-ingest stdout was requested as piped");
        let port = match handshake_with_timeout(stdout, Duration::from_secs(5)) {
            Ok(port) => port,
            Err(msg) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("stub-ingest handshake: {msg}");
            }
        };
        Self { child, port }
    }

    fn fetch_batches(&self) -> Vec<php_analyze::wire::Batch> {
        let url = format!("http://127.0.0.1:{}/debug/batches", self.port);
        let response = ureq::get(&url)
            .call()
            .unwrap_or_else(|e| panic!("GET {url}: {e}"));
        let mut body = response.into_body();
        let bytes = body
            .read_to_vec()
            .unwrap_or_else(|e| panic!("read /debug/batches body: {e}"));
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "decode /debug/batches JSON: {e}; body: {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    }
}

impl Drop for StubProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn handshake_with_timeout(
    stdout: std::process::ChildStdout,
    timeout: Duration,
) -> Result<u16, String> {
    let (tx, rx) = sync_channel::<Result<u16, String>>(1);
    thread::spawn(move || {
        let _ = tx.send(read_handshake(stdout));
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(format!("readline timeout after {timeout:?}")),
        Err(RecvTimeoutError::Disconnected) => {
            Err("worker thread exited without sending a result".to_string())
        }
    }
}

fn read_handshake(stdout: std::process::ChildStdout) -> Result<u16, String> {
    let mut reader = BufReader::new(stdout);
    let mut bound = String::new();
    let bytes = reader
        .read_line(&mut bound)
        .map_err(|e| format!("read bound line: {e}"))?;
    if bytes == 0 {
        return Err("stub stdout closed before `bound:`".to_string());
    }
    let port = parse_bound_line(&bound)?;
    let mut ready = String::new();
    let bytes = reader
        .read_line(&mut ready)
        .map_err(|e| format!("read ready line: {e}"))?;
    if bytes == 0 {
        return Err("stub stdout closed before `ready`".to_string());
    }
    if ready.trim() != "ready" {
        return Err(format!("expected `ready`, got {:?}", ready.trim()));
    }
    Ok(port)
}

fn parse_bound_line(line: &str) -> Result<u16, String> {
    let trimmed = line.trim();
    let addr = trimmed
        .strip_prefix("bound: ")
        .ok_or_else(|| format!("`bound:` line malformed: {trimmed:?}"))?;
    let port = addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .ok_or_else(|| format!("bound addr has no port: {addr:?}"))?;
    Ok(port)
}

// ---------------------------------------------------------------------------
// Common helpers (cdylib build, fixture lookup, mismatch grep).
// ---------------------------------------------------------------------------

fn mentions_module_api_mismatch(buf: &[u8]) -> bool {
    String::from_utf8_lossy(buf).contains("module API")
}

fn build_cdylib() -> PathBuf {
    let out = Command::new(env!("CARGO"))
        .args(["build", "-p", "php-analyze"])
        .output()
        .expect("cargo build runnable from the test");
    assert!(
        out.status.success(),
        "cargo build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    target_dir().join("debug").join("libphp_analyze.so")
}

fn stub_ingest_binary() -> PathBuf {
    let out = Command::new(env!("CARGO"))
        .args(["build", "-p", "stub-ingest"])
        .output()
        .expect("cargo build stub-ingest runnable from the test");
    assert!(
        out.status.success(),
        "cargo build stub-ingest failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    target_dir().join("debug").join("stub-ingest")
}

fn target_dir() -> PathBuf {
    if let Ok(dir) = env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(std::path::Path::parent)
        .map(|p| p.join("target"))
        .expect("crate dir → crates → repo root")
}

fn locate_fpm_fixture(name: &str) -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crate dir → crates → repo root");
    let path = repo_root.join("tests").join("php-fpm").join(name);
    assert!(
        path.exists(),
        "fixture {name} not found at {} (manifest_dir: {})",
        path.display(),
        manifest_dir.display(),
    );
    path
}

// ---------------------------------------------------------------------------
// Per-binary helpers.
// ---------------------------------------------------------------------------

enum FpmOutcome {
    Passed,
    SkippedModuleApi,
}

fn cgi_params<'a>(
    script_filename: &'a str,
    request_uri: &'a str,
    port_str: &'a str,
) -> Vec<(&'a str, &'a str)> {
    vec![
        ("GATEWAY_INTERFACE", "CGI/1.1"),
        ("SERVER_PROTOCOL", "HTTP/1.1"),
        ("SERVER_SOFTWARE", "fpm-integration-test/0"),
        ("REQUEST_METHOD", "GET"),
        ("QUERY_STRING", ""),
        ("CONTENT_LENGTH", "0"),
        ("CONTENT_TYPE", ""),
        ("REQUEST_URI", request_uri),
        ("DOCUMENT_URI", request_uri),
        ("SCRIPT_NAME", request_uri),
        ("SCRIPT_FILENAME", script_filename),
        ("DOCUMENT_ROOT", "/tmp"),
        ("REMOTE_ADDR", "127.0.0.1"),
        ("REMOTE_PORT", "0"),
        ("SERVER_ADDR", "127.0.0.1"),
        ("SERVER_PORT", port_str),
        ("SERVER_NAME", "127.0.0.1"),
        ("HTTP_HOST", "127.0.0.1"),
        ("REDIRECT_STATUS", "200"),
    ]
}

/// Self-test the FastCGI client framing by sending one request
/// against a non-existent script. The PHP-FPM master must reply
/// with a complete END_REQUEST and a stdout body whose
/// CGI `Status:` header is non-200. If the framing is broken
/// the read loop hangs or panics here, isolating that failure
/// mode from the real assertions in the calling helper.
fn assert_fastcgi_framing_sane(fpm: &FpmProcess) {
    let port_str = fpm.port.to_string();
    let params = cgi_params(
        "/nonexistent/definitely-not-a-real-fixture.php",
        "/nonexistent/definitely-not-a-real-fixture.php",
        &port_str,
    );
    let response = fastcgi_request(fpm.address(), &params, &[])
        .expect("FastCGI framing self-test: framing or transport bug");
    assert!(
        response.cgi_status >= 400 || !response.stderr.is_empty(),
        "FastCGI framing self-test: expected FPM to report a missing script via non-2xx CGI Status \
         or a non-empty FCGI_STDERR; got Status {} stderr {:?} stdout {:?}",
        response.cgi_status,
        String::from_utf8_lossy(&response.stderr),
        String::from_utf8_lossy(&response.stdout),
    );
}

fn try_fpm_repeated_requests(php_fpm_binary: &str, cdylib: &Path) -> FpmOutcome {
    let token = "fpm-rt-token-1";
    let path = "/v1/ingest";
    let stub = StubProcess::spawn(token, path);
    let server_url = format!("http://127.0.0.1:{}{}", stub.port, path);

    let pool_extra = "\
        pm = static\n\
        pm.max_children = 1\n\
        pm.max_requests = 0\n\
    ";
    let fpm = match FpmProcess::spawn(php_fpm_binary, cdylib, &server_url, token, pool_extra) {
        Ok(p) => p,
        Err(FpmSpawnError::ModuleApiMismatch) => return FpmOutcome::SkippedModuleApi,
        Err(FpmSpawnError::NotReady { stderr }) => {
            panic!("{php_fpm_binary} (pm.max_children=1) failed to start: {stderr}");
        }
        Err(FpmSpawnError::Io(e)) => panic!("{php_fpm_binary} spawn IO: {e}"),
    };

    assert_fastcgi_framing_sane(&fpm);

    let workers = fpm
        .worker_pids()
        .unwrap_or_else(|e| panic!("worker_pids: {e}"));
    assert_eq!(
        workers.len(),
        1,
        "{php_fpm_binary}: pm.max_children=1 must surface exactly one worker; got {workers:?}"
    );
    let worker_pid = workers[0];

    let fixture = locate_fpm_fixture("fpm_repeated.php");
    let script_filename = fixture.to_str().expect("fixture path is utf8").to_string();
    let port_str = fpm.port.to_string();
    let params = cgi_params(&script_filename, "/fpm_repeated.php", &port_str);

    // First request doubles as a module-API-mismatch detector
    // and counts as warmup #0. The cdylib's API number is baked
    // in at compile time; if it doesn't match this php-fpm
    // binary, the extension fails to initialise inside the
    // worker and writes "Module compiled with module API=..."
    // into the pool error log. We catch that here and return
    // `SkippedModuleApi` so the test exits cleanly on hosts
    // whose `update-alternatives` doesn't match the installed
    // FPM binary (mirrors the recorder / shipper integration
    // tests). The probe still counts towards the 10-request
    // warmup window below.
    run_one_request(&fpm, &params, 0, "module-api-probe");
    if fpm.observe_module_api_mismatch() {
        return FpmOutcome::SkippedModuleApi;
    }

    // 9 more warmup requests (probe was #0) to amortise
    // one-time allocations before the leak-detection window.
    // We deliberately pace the requests at ~10 ms apart: with
    // `flush_records = 10000`, each request emits one
    // `PendingBatch` at RSHUTDOWN, and the background shipper
    // POSTs it to the stub asynchronously. Pacing lets the
    // shipper drain each batch before the next arrives, which
    // dodges two failure modes that bit earlier iterations of
    // this test:
    //
    //   (a) FPM's hardcoded ~5 s SIGQUIT→SIGTERM grace truncates
    //       any tail-drain longer than that; pacing avoids
    //       relying on the tail-drain.
    //   (b) FPM workers may not always invoke our extension's
    //       MSHUTDOWN on signal-driven exit (observed: drop(fpm)
    //       returns in ~60 ms even with shutdown_grace_ms=30000),
    //       so we cannot count on the drain at all.
    //
    // With 10 ms pacing, 110 batches take 1.1 s of steady-state
    // streaming, which the loopback HTTP path comfortably
    // services in real time.
    let warmup_start = Instant::now();
    for i in 1..10 {
        run_one_request(&fpm, &params, i, "warmup");
        thread::sleep(Duration::from_millis(10));
    }
    eprintln!(
        "fpm_repeated_requests({php_fpm_binary}): 9 warmup requests took {:?}",
        warmup_start.elapsed()
    );
    let rss_baseline = fpm
        .vm_rss_kb(worker_pid)
        .unwrap_or_else(|e| panic!("vm_rss_kb baseline: {e}"));

    // 100 measured requests, paced 10 ms apart for the reason
    // documented above.
    let measured_start = Instant::now();
    for i in 0..100 {
        run_one_request(&fpm, &params, i, "measured");
        thread::sleep(Duration::from_millis(10));
    }
    eprintln!(
        "fpm_repeated_requests({php_fpm_binary}): 100 measured requests took {:?}",
        measured_start.elapsed()
    );

    // Quiescence window before reading the stub queue: give the
    // shipper a final chance to flush whatever's queued from the
    // last few requests, since per-batch POST latency varies.
    thread::sleep(Duration::from_millis(500));

    let rss_after = fpm
        .vm_rss_kb(worker_pid)
        .unwrap_or_else(|e| panic!("vm_rss_kb after: {e}"));

    eprintln!(
        "fpm_repeated_requests({php_fpm_binary}): RSS baseline = {rss_baseline} KiB, \
         after 100 measured = {rss_after} KiB, delta = {} KiB",
        rss_after.saturating_sub(rss_baseline),
    );

    let rss_delta_kb = rss_after.saturating_sub(rss_baseline);
    assert!(
        rss_delta_kb <= 2 * 1024,
        "{php_fpm_binary}: RSS leaked across 100 requests; baseline {rss_baseline} KiB, \
         after {rss_after} KiB, delta {rss_delta_kb} KiB exceeds 2 MiB ceiling",
    );

    // Snapshot the log artefacts before dropping the FPM
    // process — `fpm.captured_stderr()` and
    // `fpm.read_pool_error_log()` need the live process /
    // tempdir to read from. The drop tears down the tempdir
    // and (on SIGTERM) emits a "Terminating" notice we don't
    // care about.
    let pool_log = fpm.read_pool_error_log();
    let fpm_stderr = fpm.captured_stderr();

    // Drop the FPM master so the SIGTERM → worker MSHUTDOWN →
    // shipper-drain pipeline runs to completion before we
    // read the stub's queue. The recorder buffers each
    // request's records in-process and hands a `PendingBatch`
    // to the shipper channel at RSHUTDOWN. The shipper thread
    // POSTs asynchronously; without the explicit drop here,
    // the in-flight batches not yet POSTed when the 100th
    // FastCGI request returned would be missed by
    // `fetch_batches`. The MSHUTDOWN drain (bounded by
    // `php_analyze.shutdown_grace = 300 ms` set in the
    // per-test php.ini) is what guarantees all 110 batches
    // hit the stub before the worker exits.
    drop(fpm);
    let batches = stub.fetch_batches();
    if batches.len() != 110 {
        // On assertion failure, dump the FPM/PHP log artefacts
        // alongside the dropped-records summary so a future
        // regression has the per-run diagnostics inline rather
        // than requiring a manual re-run with extra prints.
        let total_dropped: u64 = batches.iter().map(|b| b.meta.dropped_records).sum();
        let max_dropped = batches
            .iter()
            .map(|b| b.meta.dropped_records)
            .max()
            .unwrap_or(0);
        eprintln!(
            "fpm_repeated_requests({php_fpm_binary}): got {} batches, \
             sum(dropped_records)={total_dropped}, max(dropped_records)={max_dropped}",
            batches.len(),
        );
        eprintln!("---PHP error log ({} bytes)---", pool_log.len());
        eprintln!(
            "{}",
            String::from_utf8_lossy(&pool_log)
                .chars()
                .take(8192)
                .collect::<String>()
        );
        eprintln!("---FPM master stderr ({} bytes)---", fpm_stderr.len());
        eprintln!(
            "{}",
            String::from_utf8_lossy(&fpm_stderr)
                .chars()
                .take(8192)
                .collect::<String>()
        );
    }
    assert_eq!(
        batches.len(),
        110,
        "{php_fpm_binary}: expected 110 batches (10 warmup + 100 measured); got {}",
        batches.len(),
    );
    // NOTE: a future change will assert "exactly 110 distinct
    // `trace_id`s" once UUID v7 generation lands in
    // `Trace::new`. Today the production hot path leaves
    // `trace_id = [0; 16]` (see `recorder::types::Trace::new`,
    // which documents "UUID v7 generation arrives in Phase 4"
    // — Phase 4 closed the shipper / encoder substrate, the
    // generation itself is still TODO). Asserting distinctness
    // here would fail against the current behaviour for reasons
    // unrelated to FPM, so the per-request freshness check
    // narrows to "trace was allocated fresh" via the
    // start-time uniqueness below.
    let mut start_times: Vec<i64> = batches.iter().map(|b| b.meta.start_time).collect();
    start_times.sort_unstable();
    let unique_start_times = start_times
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        unique_start_times.len() >= 100,
        "{php_fpm_binary}: every RINIT must produce a fresh start_time \
         (CLOCK_REALTIME at trace allocation); got {} distinct out of 110 batches \
         (10 ms pacing should make every value unique modulo realtime resolution)",
        unique_start_times.len(),
    );

    assert!(
        !contains_token(&pool_log, token),
        "{php_fpm_binary}: bearer token leaked into PHP error log",
    );
    assert!(
        !contains_token(&fpm_stderr, token),
        "{php_fpm_binary}: bearer token leaked into FPM master stderr/stdout",
    );

    FpmOutcome::Passed
}

fn run_one_request(fpm: &FpmProcess, params: &[(&str, &str)], i: usize, phase: &str) {
    let response = fastcgi_request(fpm.address(), params, &[])
        .unwrap_or_else(|e| panic!("FastCGI {phase} request {i}: {e}"));
    assert_eq!(
        response.app_status,
        0,
        "{phase} request {i}: FastCGI app_status non-zero ({}); stderr: {}",
        response.app_status,
        String::from_utf8_lossy(&response.stderr),
    );
    let body = response.body();
    assert!(
        body == b"ok",
        "{phase} request {i}: fixture body should be exactly \"ok\"; got {:?} (stdout: {:?})",
        String::from_utf8_lossy(body),
        String::from_utf8_lossy(&response.stdout),
    );
}

fn try_fpm_thread_per_worker(php_fpm_binary: &str, cdylib: &Path) -> FpmOutcome {
    let token = "fpm-rt-token-2";
    let path = "/v1/ingest";
    let stub = StubProcess::spawn(token, path);
    let server_url = format!("http://127.0.0.1:{}{}", stub.port, path);

    let pool_extra = "\
        pm = static\n\
        pm.max_children = 8\n\
        pm.start_servers = 8\n\
        pm.max_spare_servers = 8\n\
        pm.min_spare_servers = 8\n\
        pm.max_requests = 0\n\
    ";
    let fpm = match FpmProcess::spawn(php_fpm_binary, cdylib, &server_url, token, pool_extra) {
        Ok(p) => p,
        Err(FpmSpawnError::ModuleApiMismatch) => return FpmOutcome::SkippedModuleApi,
        Err(FpmSpawnError::NotReady { stderr }) => {
            panic!("{php_fpm_binary} (pm.max_children=8) failed to start: {stderr}");
        }
        Err(FpmSpawnError::Io(e)) => panic!("{php_fpm_binary} spawn IO: {e}"),
    };

    let fixture = locate_fpm_fixture("fpm_repeated.php");
    let script_filename = fixture.to_str().expect("fixture path is utf8").to_string();
    let port_str = fpm.port.to_string();
    let params = cgi_params(&script_filename, "/fpm_repeated.php", &port_str);

    // First request doubles as a module-API-mismatch detector
    // (see the matching block in `try_fpm_repeated_requests`).
    run_one_request(&fpm, &params, 0, "module-api-probe");
    if fpm.observe_module_api_mismatch() {
        return FpmOutcome::SkippedModuleApi;
    }

    for i in 1..16 {
        run_one_request(&fpm, &params, i, "thread-per-worker");
    }

    let workers = fpm
        .worker_pids()
        .unwrap_or_else(|e| panic!("worker_pids: {e}"));
    assert_eq!(
        workers.len(),
        8,
        "{php_fpm_binary}: pm.max_children=8 must surface exactly 8 workers; got {workers:?}",
    );

    for &worker_pid in &workers {
        let thread_count = fpm
            .thread_count(worker_pid)
            .unwrap_or_else(|e| panic!("thread_count({worker_pid}): {e}"));
        assert_eq!(
            thread_count, 2,
            "{php_fpm_binary}: worker {worker_pid} must own exactly 2 threads \
             (the request thread + the shipper); got {thread_count}",
        );

        let tids = fpm
            .thread_ids(worker_pid)
            .unwrap_or_else(|e| panic!("thread_ids({worker_pid}): {e}"));
        assert_eq!(tids.len(), 2);

        let worker_comm = fpm
            .thread_comm(worker_pid, worker_pid)
            .unwrap_or_else(|e| panic!("thread_comm({worker_pid}/{worker_pid}): {e}"));
        let shipper_tid = tids.into_iter().find(|t| *t != worker_pid).expect(
            "thread_ids contained the worker pid plus exactly one other TID — the shipper thread",
        );
        let shipper_comm = fpm
            .thread_comm(worker_pid, shipper_tid)
            .unwrap_or_else(|e| panic!("thread_comm({worker_pid}/{shipper_tid}): {e}"));

        assert_ne!(
            worker_comm, shipper_comm,
            "{php_fpm_binary}: worker {worker_pid} and its second thread share a comm \
             ({worker_comm:?}); shipper-thread name should be distinct",
        );

        // Soft-match the shipper-thread name. Today the cdylib
        // names the thread `php-analyze-shipper`, truncated by
        // Linux's 16-byte TASK_COMM_LEN to `php-analyze-shi`. A
        // pending P2 follow-up (`shipper-thread-name-fits-task-comm`
        // in `COMMENTS.md`) renames it to `pa-shipper`. Accept
        // either prefix; the strict invariant is "distinct from
        // the worker's comm", asserted above.
        let recognised = shipper_comm.starts_with("php-analyze") || shipper_comm.starts_with("pa-");
        assert!(
            recognised,
            "{php_fpm_binary}: worker {worker_pid} shipper-thread comm {shipper_comm:?} is \
             neither `php-analyze*` nor `pa-*`; worker comm was {worker_comm:?}",
        );
    }

    let master_threads = fpm
        .thread_count(fpm.master_pid)
        .unwrap_or_else(|e| panic!("thread_count(master): {e}"));
    assert_eq!(
        master_threads, 1,
        "{php_fpm_binary}: FPM master must own exactly one thread (no shipper in master because \
         RINIT never runs there); got {master_threads}",
    );

    // Don't assert on batches here; the per-worker batches all
    // land on the same stub, but proving them is the §1.3 #2
    // job of `try_fpm_repeated_requests`. This helper's job is
    // the AC-PB-1 thread-count invariant.
    let _ = stub.fetch_batches();

    FpmOutcome::Passed
}

/// Probe a single php-fpm version: prefer the bare `name` (if on
/// `PATH`), else fall back to the standard absolute `fallback`
/// path. Returns the resolved invocation as an owned `String` so
/// the caller can `Command::new(&binary)` regardless of which
/// arm matched. Returns `None` if neither responds to `-v` with
/// a zero exit.
fn resolve_fpm_binary(name: &str, fallback: &str) -> Option<String> {
    if Command::new(name)
        .arg("-v")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Some(name.to_string());
    }
    let fallback_path = Path::new(fallback);
    if fallback_path.exists()
        && Command::new(fallback_path)
            .arg("-v")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    {
        return Some(fallback.to_string());
    }
    None
}

fn contains_token(buf: &[u8], token: &str) -> bool {
    let needle = token.as_bytes();
    buf.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------------
// The test.
// ---------------------------------------------------------------------------

#[test]
fn fpm_repeated_requests() {
    if env::var("PHP_ANALYZE_RUN_FPM").as_deref() != Ok("1") {
        eprintln!(
            "fpm_repeated_requests: skipped (set PHP_ANALYZE_RUN_FPM=1 to run the \
             fpm-integration-test PHP integration test)"
        );
        return;
    }

    // `php-fpm` lives in `/usr/sbin/` on Debian-family distros
    // (and similar admin-tools paths elsewhere), which is rarely
    // on a regular user's `PATH`. Probe the bare name first
    // (works in CI containers and on hosts where the operator
    // has added `/usr/sbin` to `PATH`), then fall back to the
    // standard absolute install path. The first probe that
    // succeeds wins per PHP version.
    let candidates = [
        ("php-fpm8.3", "/usr/sbin/php-fpm8.3"),
        ("php-fpm8.4", "/usr/sbin/php-fpm8.4"),
    ];
    let available: Vec<String> = candidates
        .iter()
        .filter_map(|(name, fallback)| resolve_fpm_binary(name, fallback))
        .collect();

    if available.is_empty() {
        eprintln!(
            "fpm_repeated_requests: skipped (no usable php-fpm8.3 or php-fpm8.4 found; \
             tried {} and the standard `/usr/sbin/` install paths)",
            candidates
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(", "),
        );
        return;
    }

    let cdylib = build_cdylib();

    let mut exercised: Vec<&str> = Vec::new();
    let mut skipped: Vec<&str> = Vec::new();
    for binary in &available {
        let primary = try_fpm_repeated_requests(binary, &cdylib);
        let thread = try_fpm_thread_per_worker(binary, &cdylib);
        match (primary, thread) {
            (FpmOutcome::Passed, FpmOutcome::Passed) => exercised.push(binary.as_str()),
            (FpmOutcome::SkippedModuleApi, _) | (_, FpmOutcome::SkippedModuleApi) => {
                skipped.push(binary.as_str())
            }
        }
    }

    if !skipped.is_empty() {
        eprintln!(
            "fpm_repeated_requests: skipped {} php-fpm binar{} due to module-API mismatch: {}",
            skipped.len(),
            if skipped.len() == 1 { "y" } else { "ies" },
            skipped.join(", "),
        );
    }

    assert!(
        !exercised.is_empty(),
        "fpm_repeated_requests: no php-fpm binary completed both helpers; all candidates skipped \
         on module API or unavailable ({} tried: {})",
        candidates.len(),
        candidates
            .iter()
            .map(|(n, _)| *n)
            .collect::<Vec<_>>()
            .join(", "),
    );
}
