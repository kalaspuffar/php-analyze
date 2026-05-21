//! Round-trip integration tests for the `stub-ingest` binary.
//!
//! Each test spawns the just-built binary as a subprocess, parses the
//! bound port from its stdout, gates on the `ready` line, then drives
//! one or more HTTP requests through `ureq` and asserts on the
//! server's responses (and on the contents of the `/debug/batches`
//! endpoint).
//!
//! A `ChildGuard` newtype wraps `std::process::Child` so the
//! subprocess is killed and reaped on `Drop` — including on panic.
//! This keeps test failures from leaking zombie `stub-ingest`
//! processes.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use php_analyze::wire;

/// Wraps a spawned `stub-ingest` child so dropping the guard kills
/// the subprocess and reaps its exit status — even if the test
/// panics before reaching teardown.
struct ChildGuard {
    child: Child,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Best-effort: `kill()` and `wait()` errors are swallowed
        // because we are already in the unwinding-or-cleanup path.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn the stub on `127.0.0.1:0` and block until it prints the
/// `bound:` and `ready` lines. Returns the guard + the parsed
/// loopback address.
fn spawn_stub(token: &str) -> (ChildGuard, SocketAddr) {
    let bin = env!("CARGO_BIN_EXE_stub-ingest");
    let mut child = Command::new(bin)
        .args(["--auth-token", token, "--bind", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn stub-ingest");

    let stdout = child.stdout.take().expect("child stdout was piped");
    let reader = BufReader::new(stdout);

    let mut addr: Option<SocketAddr> = None;
    let mut ready = false;
    for line in reader.lines() {
        let line = line.expect("read stub-ingest stdout");
        if let Some(rest) = line.strip_prefix("bound: ") {
            addr = Some(
                rest.parse()
                    .unwrap_or_else(|e| panic!("parse bound line {rest:?}: {e}")),
            );
        } else if line == "ready" {
            ready = true;
            break;
        }
    }
    let addr = addr.expect("stub-ingest printed `bound: …`");
    assert!(ready, "stub-ingest printed `ready` after the bound line");

    (ChildGuard { child }, addr)
}

fn url(addr: SocketAddr, path: &str) -> String {
    format!("http://{addr}{path}")
}

fn sample_batch() -> wire::Batch {
    wire::Batch {
        meta: wire::MetaFull {
            schema_version: wire::SCHEMA_VERSION,
            trace_id: "0190b5e7-1c2d-7000-8000-000000000abc".to_owned(),
            host: "round-trip-host".to_owned(),
            pid: 9999,
            start_time: 1_716_400_000_000_000_000,
            sapi: "cli".to_owned(),
            uri_or_script: "/tmp/round_trip.php".to_owned(),
            dropped_records: 0,
        },
        dict: vec![wire::DictEntry {
            fn_id: 1,
            fqn: "round_trip_fn".to_owned(),
            file: "/tmp/round_trip.php".to_owned(),
            line: 1,
            kind: wire::FunctionKind::Function,
        }],
        calls: vec![wire::CallRecord {
            call_id: 1,
            parent: 0,
            fn_id: 1,
            depth: 0,
            t_in: 1_000_000,
            t_out: 1_500_000,
            cpu_u: 200,
            cpu_s: 50,
            mem_in: 4096,
            mem_out: 4096,
            abnormal_exit: false,
        }],
    }
}

fn post_batch_with_headers(
    agent: &ureq::Agent,
    addr: SocketAddr,
    auth: Option<&str>,
    content_type: &str,
    body: Vec<u8>,
) -> ureq::http::Response<ureq::Body> {
    let mut req = agent
        .post(url(addr, "/v1/ingest"))
        .header("Content-Type", content_type);
    if let Some(value) = auth {
        req = req.header("Authorization", value);
    }
    // `ureq` 3 returns `Result<Response<Body>, Error>`, where some
    // failure cases (4xx) are surfaced as Err. We map both arms to
    // a Response so the test assertions can match on status_code()
    // uniformly.
    match req.send(&body[..]) {
        Ok(resp) => resp,
        Err(ureq::Error::StatusCode(code)) => panic!(
            "stub-ingest returned a status-code-only error: {code}; the test \
             helper expected the body to be readable but ureq's error did not \
             carry one. Re-check the agent's `.http_status_as_error(false)` \
             config."
        ),
        Err(err) => panic!("POST /v1/ingest network error: {err}"),
    }
}

fn fetch_batches(agent: &ureq::Agent, addr: SocketAddr) -> Vec<wire::Batch> {
    let mut response = agent
        .get(url(addr, "/debug/batches"))
        .call()
        .expect("GET /debug/batches");
    assert_eq!(response.status().as_u16(), 200);
    let body: Vec<u8> = response
        .body_mut()
        .read_to_vec()
        .expect("read /debug/batches body");
    serde_json::from_slice(&body).expect("parse /debug/batches as Vec<wire::Batch>")
}

fn reset_store(agent: &ureq::Agent, addr: SocketAddr) {
    let response = agent
        .post(url(addr, "/debug/reset"))
        .send_empty()
        .expect("POST /debug/reset");
    assert_eq!(response.status().as_u16(), 200);
}

#[test]
fn round_trip_post_and_get_debug_batches() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    let original = sample_batch();
    let body = rmp_serde::to_vec_named(&original).expect("encode wire::Batch");

    let response = post_batch_with_headers(
        &agent,
        addr,
        Some("Bearer test-token"),
        wire::MEDIA_TYPE,
        body,
    );
    assert_eq!(response.status().as_u16(), 200, "POST returns 200");

    let stored = fetch_batches(&agent, addr);
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0], original);
}

#[test]
fn reset_clears_the_store_between_scenarios() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    let body = rmp_serde::to_vec_named(&sample_batch()).expect("encode");
    let response = post_batch_with_headers(
        &agent,
        addr,
        Some("Bearer test-token"),
        wire::MEDIA_TYPE,
        body,
    );
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(fetch_batches(&agent, addr).len(), 1);

    reset_store(&agent, addr);
    assert_eq!(fetch_batches(&agent, addr).len(), 0);
}

#[test]
fn missing_bearer_returns_401_and_does_not_store() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    let body = rmp_serde::to_vec_named(&sample_batch()).expect("encode");
    let response = post_batch_with_headers(&agent, addr, None, wire::MEDIA_TYPE, body);
    assert_eq!(response.status().as_u16(), 401);
    assert_eq!(fetch_batches(&agent, addr).len(), 0);
}

#[test]
fn wrong_bearer_returns_401_and_does_not_store() {
    let (_guard, addr) = spawn_stub("real-token");
    let agent = ureq_agent_no_status_err();

    let body = rmp_serde::to_vec_named(&sample_batch()).expect("encode");
    let response = post_batch_with_headers(
        &agent,
        addr,
        Some("Bearer wrong-token"),
        wire::MEDIA_TYPE,
        body,
    );
    assert_eq!(response.status().as_u16(), 401);
    assert_eq!(fetch_batches(&agent, addr).len(), 0);
}

#[test]
fn wrong_content_type_returns_415_and_does_not_store() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    let body = rmp_serde::to_vec_named(&sample_batch()).expect("encode");
    let response = post_batch_with_headers(
        &agent,
        addr,
        Some("Bearer test-token"),
        "application/octet-stream",
        body,
    );
    assert_eq!(response.status().as_u16(), 415);
    assert_eq!(fetch_batches(&agent, addr).len(), 0);
}

#[test]
fn malformed_body_returns_400_and_does_not_store() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    let response = post_batch_with_headers(
        &agent,
        addr,
        Some("Bearer test-token"),
        wire::MEDIA_TYPE,
        b"not a valid msgpack batch".to_vec(),
    );
    assert_eq!(response.status().as_u16(), 400);
    assert_eq!(fetch_batches(&agent, addr).len(), 0);
}

/// `ureq` v3 by default turns 4xx/5xx responses into `Err`s. We want
/// the body-ful response so the tests can assert directly on status
/// codes; this helper builds an agent that surfaces non-2xx as
/// `Ok` instead.
fn ureq_agent_no_status_err() -> ureq::Agent {
    ureq::config::Config::builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .http_status_as_error(false)
        .build()
        .new_agent()
}
