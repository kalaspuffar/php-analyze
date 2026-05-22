//! `stub-ingest` — test-only HTTP ingest receiver for `php-analyze`.
//!
//! This binary is the receiving end of the `SPECIFICATION.md` §5.2
//! egress contract. It accepts `POST <path>` requests carrying a
//! MessagePack-encoded `wire::Batch` (§4.2), validates the bearer
//! token, decodes the body via the production crate's `wire` module,
//! stores accepted batches in process memory, and exposes two debug
//! endpoints (`GET /debug/batches`, `POST /debug/reset`) that
//! integration tests use to inspect and reset the store.
//!
//! ### What this is, what it isn't
//!
//! - **Test fixture.** Loopback-only by convention; not hardened
//!   against adversarial input. The bearer compare is non-constant-
//!   time (design D-7); there is no body-size cap (design Non-Goals).
//! - **Not a production ingest.** The real ingest is a separate
//!   deliverable per `SPECIFICATION.md` §8.2.
//!
//! ### Bind protocol
//!
//! With `--bind 127.0.0.1:0`, the stub asks the OS to pick a free
//! port, then writes exactly two lines to stdout — `bound: <addr>`
//! followed by `ready` — and flushes after each. Integration test
//! harnesses parse the `bound:` line to discover the port and gate
//! on `ready` before sending requests. After `ready`, the stub
//! writes only to stderr; stdout stays silent for the lifetime of
//! the process.

use std::collections::BTreeSet;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use php_analyze::wire;
use serde::Serialize;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

/// In-memory batch store shared between the request handlers and the
/// `/debug/batches` reader. `tiny_http`'s default `Server::recv` is
/// single-threaded, so the `Mutex` only serialises against the
/// (currently nonexistent) future case where the stub serves
/// concurrent connections.
type Store = Arc<Mutex<Vec<wire::Batch>>>;

/// Captured headers from the most-recent request the stub routed to
/// `handle_ingest`. `None` until the first ingest request arrives;
/// also reset to `None` by `POST /debug/reset`. Populated *before*
/// any validation so a 401/415/400-rejected request is still
/// observable through `/debug/last_request_headers`.
type LastHeaders = Arc<Mutex<Option<Vec<(String, String)>>>>;

/// Distinct `remote_addr` values observed on the ingest path. Each
/// unique value represents one accepted TCP connection (HTTP/1.1
/// keep-alive on a single `ureq::Agent` reuses the same ephemeral
/// port, so the set's `.len()` is the number of distinct TCP
/// connections that reached `handle_ingest`). Cleared by
/// `POST /debug/reset`; surfaced via `GET /debug/connection_count`.
/// `BTreeSet` rather than `HashSet` for deterministic iteration if
/// the address list is ever exposed for diagnostics.
type Connections = Arc<Mutex<BTreeSet<SocketAddr>>>;

/// JSON shape returned by `GET /debug/last_request_headers`. One
/// object per header in the order the client sent them. Header names
/// are preserved with the as-sent spelling; tests SHOULD compare with
/// `eq_ignore_ascii_case` (the HTTP semantic).
#[derive(Serialize)]
struct HeaderPair<'a> {
    name: &'a str,
    value: &'a str,
}

/// JSON shape returned by `GET /debug/connection_count`. Wrapping the
/// count in an object (instead of returning a bare integer body) lets
/// us add fields later (e.g. `addrs: [...]`) without breaking
/// existing consumers.
#[derive(Serialize)]
struct CountBody {
    count: usize,
}

#[derive(Debug, Parser)]
#[command(
    name = "stub-ingest",
    about = "Test-only HTTP ingest receiver for php-analyze."
)]
struct Args {
    /// Address to bind, e.g. `127.0.0.1:8080`. Port `0` lets the OS
    /// pick a free port; the bound port is then printed on stdout
    /// (see the bind protocol in the module doc).
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: String,

    /// Bearer token clients must present in the `Authorization`
    /// header (`Authorization: Bearer <token>`). Required.
    #[arg(long)]
    auth_token: String,

    /// HTTP path the stub accepts batches on. Defaults to the
    /// `SPECIFICATION.md` OQ-3 path.
    #[arg(long, default_value = "/v1/ingest")]
    path: String,

    /// HTTP status to return on the ingest path's *success path*
    /// (i.e. after bearer/content-type/body-decode validation has
    /// passed AND the decoded batch has been pushed onto the
    /// in-memory store). Default `200`. Range `[100, 599]`.
    ///
    /// Used by retry-exhaust integration tests (e.g.
    /// `tests/php-shipper/retry_exhaust.php`) to drive the
    /// shipper's `DropReason::HttpStatus(N)` path without standing
    /// up a separate failing server. `--respond-with` does NOT
    /// override the existing validation-failure responses (401 on
    /// bad bearer, 415 on wrong content-type, 400 on body-read or
    /// decode failure): those status codes are themselves the
    /// integration-test signal, and the flag only governs the
    /// success path's response code.
    #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u16).range(100..600))]
    respond_with: u16,

    /// Sleep this many milliseconds on the ingest path's
    /// *success path* (i.e. after bearer/content-type/body-decode
    /// validation has passed AND the decoded batch has been
    /// pushed onto the in-memory store), **before** the response
    /// is built. Default `0` (no sleep).
    ///
    /// Used by MSHUTDOWN-drain integration tests (e.g.
    /// `tests/php-shipper/mshutdown_drain.php`) to drive the
    /// shipper's deadline cell + per-attempt timeout (AC-SH-3 /
    /// AC-BS-4 / AC-PB-2) without standing up a separate slow
    /// server.
    ///
    /// `--simulate-slow` composes with `--respond-with`: both
    /// operate on the same success-path tail. The flag does NOT
    /// delay the validation-failure paths (401/415/400) or the
    /// `/debug/*` routes — only the success path of
    /// `handle_ingest` sleeps.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u64))]
    simulate_slow: u64,
}

fn main() {
    if let Err(err) = run(Args::parse()) {
        eprintln!("stub-ingest: {err}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<(), String> {
    let server = Server::http(&args.bind).map_err(|e| format!("bind {}: {e}", args.bind))?;
    let bound = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| "server_addr returned a non-IP socket".to_owned())?;

    // Bind protocol: two lines, both flushed, exactly in this order.
    // Test harnesses depend on the literal `bound:` prefix and the
    // bare `ready` line.
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "bound: {bound}").map_err(|e| format!("stdout: {e}"))?;
    stdout.flush().map_err(|e| format!("stdout flush: {e}"))?;
    writeln!(stdout, "ready").map_err(|e| format!("stdout: {e}"))?;
    stdout.flush().map_err(|e| format!("stdout flush: {e}"))?;
    drop(stdout);

    let store: Store = Arc::new(Mutex::new(Vec::new()));
    let last_headers: LastHeaders = Arc::new(Mutex::new(None));
    let connections: Connections = Arc::new(Mutex::new(BTreeSet::new()));
    serve(server, args, store, last_headers, connections);
    Ok(())
}

fn serve(
    server: Server,
    args: Args,
    store: Store,
    last_headers: LastHeaders,
    connections: Connections,
) {
    loop {
        let request = match server.recv() {
            Ok(req) => req,
            Err(err) => {
                eprintln!("stub-ingest: recv error: {err}");
                continue;
            }
        };
        dispatch(request, &args, &store, &last_headers, &connections);
    }
}

fn dispatch(
    request: Request,
    args: &Args,
    store: &Store,
    last_headers: &LastHeaders,
    connections: &Connections,
) {
    // Strip query string if any — `/debug/batches?…` should still
    // hit the debug-batches handler. None of our routes care about
    // query params today; documenting the slice the dispatch makes
    // it obvious to a future reader why we don't compare against
    // the full URL.
    let path = request.url().split('?').next().unwrap_or("").to_owned();
    let method = request.method().clone();

    let method_name = request_method_name(&request);
    match (&method, path.as_str()) {
        (Method::Post, p) if p == args.path => {
            handle_ingest(request, args, store, last_headers, connections)
        }
        (Method::Get, "/debug/batches") => handle_debug_batches(request, store),
        (Method::Get, "/debug/last_request_headers") => {
            handle_debug_last_request_headers(request, last_headers)
        }
        (Method::Get, "/debug/connection_count") => {
            handle_debug_connection_count(request, connections)
        }
        (Method::Post, "/debug/reset") => {
            handle_debug_reset(request, store, last_headers, connections)
        }
        (_, p) if p == args.path => respond_status(request, 405, &format!("{method_name} {p}")),
        _ => respond_status(request, 404, &format!("{method_name} {path}")),
    }
}

fn handle_ingest(
    mut request: Request,
    args: &Args,
    store: &Store,
    last_headers: &LastHeaders,
    connections: &Connections,
) {
    // Capture the request's headers BEFORE any validation so a
    // 401/415/400-rejected request is still observable through
    // `/debug/last_request_headers`. The token's `Authorization`
    // value is intentionally surfaced — `/debug/last_request_headers`
    // is the test seam for AC-SH-4-adjacent assertions; see
    // `stub-ingest-header-capture`'s `design.md` D-1 / D-6.
    let captured = capture_headers(request.headers());
    match last_headers.lock() {
        Ok(mut guard) => *guard = Some(captured),
        Err(err) => {
            eprintln!("stub-ingest: last_headers mutex poisoned: {err}");
            respond_status(request, 500, "last_headers mutex poisoned");
            return;
        }
    }

    // Record the TCP connection's ephemeral source address BEFORE
    // bearer/content-type validation, for the same reason headers
    // are captured early: a 401-rejected request still consumed a
    // TCP connection, which the AC-SH-6 contract counts.
    match record_connection(request.remote_addr(), connections) {
        Ok(()) => {}
        Err(err) => {
            eprintln!("stub-ingest: connections mutex poisoned: {err}");
            respond_status(request, 500, "connections mutex poisoned");
            return;
        }
    }

    if !validate_bearer(request.headers(), &args.auth_token) {
        respond_status(request, 401, "missing or invalid bearer token");
        return;
    }
    if !validate_content_type(request.headers()) {
        respond_status(request, 415, "wrong content-type");
        return;
    }

    let mut body = Vec::new();
    if let Err(err) = request.as_reader().read_to_end(&mut body) {
        eprintln!("stub-ingest: body read error: {err}");
        respond_status(request, 400, "body read failure");
        return;
    }

    let batch: wire::Batch = match rmp_serde::from_slice(&body) {
        Ok(b) => b,
        Err(err) => {
            eprintln!("stub-ingest: decode error: {err}");
            respond_status(request, 400, "msgpack decode failure");
            return;
        }
    };

    match store.lock() {
        Ok(mut guard) => guard.push(batch),
        Err(err) => {
            eprintln!("stub-ingest: store mutex poisoned: {err}");
            // Out of design's scope (D-11): we surface 500 so the
            // test that triggered the prior panic still sees an
            // observable failure.
            respond_status(request, 500, "store mutex poisoned");
            return;
        }
    }
    // Storage happens BEFORE the response is built, so a
    // configured failure status (`--respond-with 500`, etc.)
    // still appends the decoded batch to the store. Retry-exhaust
    // tests rely on this: with `--respond-with 500` and
    // `retry_count = 3`, the shipper makes 4 attempts and
    // `/debug/batches.len()` reports `4` — one per attempt body.
    // See `stub-ingest-configurable-failure`'s `design.md` D-3.
    //
    // `--simulate-slow <ms>` then sleeps before the response is
    // built. Order matters: sleeping AFTER storage means a client
    // that times out mid-sleep still leaves the body in the store
    // (the `/debug/batches` invariant survives client timeouts).
    // Sleeping BEFORE the response means the client perceives the
    // delay as response-wait time, which is what AC-SH-3 /
    // AC-BS-4 / AC-PB-2 binding tests exercise. tiny_http's
    // `Server::recv()` is single-threaded, so the sleep blocks
    // the stub's main accept thread; subsequent client
    // connections queue in the kernel TCP accept queue until the
    // sleep completes. See `stub-ingest-slow-mode`'s
    // `design.md` D-1 / D-2.
    if args.simulate_slow > 0 {
        std::thread::sleep(Duration::from_millis(args.simulate_slow));
    }
    respond_empty(request, args.respond_with);
}

fn handle_debug_batches(request: Request, store: &Store) {
    let snapshot = match store.lock() {
        Ok(guard) => guard.clone(),
        Err(err) => {
            eprintln!("stub-ingest: store mutex poisoned: {err}");
            respond_status(request, 500, "store mutex poisoned");
            return;
        }
    };
    let body = match serde_json::to_vec_pretty(&snapshot) {
        Ok(b) => b,
        Err(err) => {
            eprintln!("stub-ingest: json encode error: {err}");
            respond_status(request, 500, "json encode failure");
            return;
        }
    };
    let response = Response::from_data(body).with_header(json_header());
    if let Err(err) = request.respond(response) {
        eprintln!("stub-ingest: respond error: {err}");
    }
}

fn handle_debug_reset(
    request: Request,
    store: &Store,
    last_headers: &LastHeaders,
    connections: &Connections,
) {
    // The three locks are acquired and released sequentially, never
    // held together (design D-4): each clear is independent and
    // there is no atomicity story to tell across them — a
    // concurrent ingest landing between the clears is still
    // visible after reset via whichever slot it populated.
    match store.lock() {
        Ok(mut guard) => guard.clear(),
        Err(err) => {
            eprintln!("stub-ingest: store mutex poisoned: {err}");
            respond_status(request, 500, "store mutex poisoned");
            return;
        }
    }
    match last_headers.lock() {
        Ok(mut guard) => *guard = None,
        Err(err) => {
            eprintln!("stub-ingest: last_headers mutex poisoned: {err}");
            respond_status(request, 500, "last_headers mutex poisoned");
            return;
        }
    }
    match connections.lock() {
        Ok(mut guard) => guard.clear(),
        Err(err) => {
            eprintln!("stub-ingest: connections mutex poisoned: {err}");
            respond_status(request, 500, "connections mutex poisoned");
            return;
        }
    }
    respond_empty(request, 200);
}

fn handle_debug_last_request_headers(request: Request, last_headers: &LastHeaders) {
    let snapshot = match last_headers.lock() {
        Ok(guard) => guard.clone(),
        Err(err) => {
            eprintln!("stub-ingest: last_headers mutex poisoned: {err}");
            respond_status(request, 500, "last_headers mutex poisoned");
            return;
        }
    };
    let Some(pairs) = snapshot else {
        // The empty state goes through `respond_empty` rather than
        // `respond_status` so the legitimate "no ingest yet" branch
        // does not produce a noisy `stub-ingest: 404 ...` line on
        // stderr for every test that gates on it. See design D-3.
        respond_empty(request, 404);
        return;
    };
    let body = match serde_json::to_vec(
        &pairs
            .iter()
            .map(|(name, value)| HeaderPair { name, value })
            .collect::<Vec<_>>(),
    ) {
        Ok(b) => b,
        Err(err) => {
            eprintln!("stub-ingest: json encode error: {err}");
            respond_status(request, 500, "json encode failure");
            return;
        }
    };
    let response = Response::from_data(body).with_header(json_header());
    if let Err(err) = request.respond(response) {
        eprintln!("stub-ingest: respond error: {err}");
    }
}

/// Convert the request's headers into an owned, JSON-friendly
/// vector preserving order and as-sent spelling. The allocations
/// are negligible for a test fixture; see design D-2.
fn capture_headers(headers: &[Header]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|h| {
            (
                h.field.as_str().as_str().to_owned(),
                h.value.as_str().to_owned(),
            )
        })
        .collect()
}

/// Insert the request's `remote_addr` into the connection-set so
/// `/debug/connection_count` reports the number of distinct TCP
/// connections that reached `handle_ingest`. Returns `Err` only on
/// mutex poison — the caller decides how to respond to that. A
/// `None` `remote_addr` (theoretically possible on non-IP transports
/// `tiny_http` does not currently support) is tolerated: we log one
/// stderr line and skip the insert, so the count undercounts rather
/// than panics. On Linux loopback the value is always `Some(_)`.
fn record_connection(
    remote_addr: Option<&SocketAddr>,
    connections: &Connections,
) -> Result<(), String> {
    let Some(addr) = remote_addr.copied() else {
        eprintln!(
            "stub-ingest: remote_addr missing on ingest request; \
             /debug/connection_count may undercount",
        );
        return Ok(());
    };
    let mut guard = connections.lock().map_err(|e| e.to_string())?;
    guard.insert(addr);
    Ok(())
}

/// `GET /debug/connection_count` — return the number of distinct
/// ingest-path TCP connections as JSON `{"count": N}`. `0` on a
/// freshly-spawned stub (zero is a meaningful count; see design
/// D-3, no 404 here). The slot is cleared by `POST /debug/reset`.
fn handle_debug_connection_count(request: Request, connections: &Connections) {
    let count = match connections.lock() {
        Ok(guard) => guard.len(),
        Err(err) => {
            eprintln!("stub-ingest: connections mutex poisoned: {err}");
            respond_status(request, 500, "connections mutex poisoned");
            return;
        }
    };
    let body = match serde_json::to_vec(&CountBody { count }) {
        Ok(b) => b,
        Err(err) => {
            eprintln!("stub-ingest: json encode error: {err}");
            respond_status(request, 500, "json encode failure");
            return;
        }
    };
    let response = Response::from_data(body).with_header(json_header());
    if let Err(err) = request.respond(response) {
        eprintln!("stub-ingest: respond error: {err}");
    }
}

/// Design D-7: byte-equal compare, **not** constant-time. The stub is
/// loopback-only by convention; the production ingest server (out of
/// this project's scope) is the right place for `subtle::ConstantTimeEq`.
fn validate_bearer(headers: &[Header], configured: &str) -> bool {
    let Some(value) = find_header_value(headers, "Authorization") else {
        return false;
    };
    let prefix = "Bearer ";
    if !value.starts_with(prefix) {
        return false;
    }
    let presented = &value[prefix.len()..];
    presented.as_bytes() == configured.as_bytes()
}

fn validate_content_type(headers: &[Header]) -> bool {
    find_header_value(headers, "Content-Type")
        .map(|v| v.as_bytes() == wire::MEDIA_TYPE.as_bytes())
        .unwrap_or(false)
}

fn find_header_value<'h>(headers: &'h [Header], name: &str) -> Option<&'h str> {
    headers
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

fn json_header() -> Header {
    // `unwrap`: both strings are static, well-formed header tokens.
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("static header parses")
}

fn respond_empty(request: Request, status: u16) {
    let response: Response<std::io::Empty> = Response::empty(StatusCode(status));
    if let Err(err) = request.respond(response) {
        eprintln!("stub-ingest: respond error: {err}");
    }
}

fn respond_status(request: Request, status: u16, reason: &str) {
    eprintln!("stub-ingest: {status} {reason}");
    respond_empty(request, status);
}

fn request_method_name(request: &Request) -> String {
    request.method().to_string()
}
