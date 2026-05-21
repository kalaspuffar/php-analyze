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

use std::io::Write;
use std::sync::{Arc, Mutex};

use clap::Parser;
use php_analyze::wire;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

/// In-memory batch store shared between the request handlers and the
/// `/debug/batches` reader. `tiny_http`'s default `Server::recv` is
/// single-threaded, so the `Mutex` only serialises against the
/// (currently nonexistent) future case where the stub serves
/// concurrent connections.
type Store = Arc<Mutex<Vec<wire::Batch>>>;

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
    serve(server, args, store);
    Ok(())
}

fn serve(server: Server, args: Args, store: Store) {
    loop {
        let request = match server.recv() {
            Ok(req) => req,
            Err(err) => {
                eprintln!("stub-ingest: recv error: {err}");
                continue;
            }
        };
        dispatch(request, &args, &store);
    }
}

fn dispatch(request: Request, args: &Args, store: &Store) {
    // Strip query string if any — `/debug/batches?…` should still
    // hit the debug-batches handler. None of our routes care about
    // query params today; documenting the slice the dispatch makes
    // it obvious to a future reader why we don't compare against
    // the full URL.
    let path = request.url().split('?').next().unwrap_or("").to_owned();
    let method = request.method().clone();

    let method_name = request_method_name(&request);
    match (&method, path.as_str()) {
        (Method::Post, p) if p == args.path => handle_ingest(request, args, store),
        (Method::Get, "/debug/batches") => handle_debug_batches(request, store),
        (Method::Post, "/debug/reset") => handle_debug_reset(request, store),
        (_, p) if p == args.path => respond_status(request, 405, &format!("{method_name} {p}")),
        _ => respond_status(request, 404, &format!("{method_name} {path}")),
    }
}

fn handle_ingest(mut request: Request, args: &Args, store: &Store) {
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
    respond_empty(request, 200);
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

fn handle_debug_reset(request: Request, store: &Store) {
    match store.lock() {
        Ok(mut guard) => guard.clear(),
        Err(err) => {
            eprintln!("stub-ingest: store mutex poisoned: {err}");
            respond_status(request, 500, "store mutex poisoned");
            return;
        }
    }
    respond_empty(request, 200);
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
