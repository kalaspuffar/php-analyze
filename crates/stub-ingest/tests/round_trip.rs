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
//! processes. The guard also retains shared handles on
//! background-thread-drained stdout / stderr buffers so individual
//! tests can assert on the stub's post-`ready` log output (added in
//! the C-12 round-1 fix-round to close WSI-1 #2 and WSI-3).

use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpListener};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use php_analyze::wire;

/// Wraps a spawned `stub-ingest` child so dropping the guard kills
/// the subprocess and reaps its exit status — even if the test
/// panics before reaching teardown. The guard also retains shared
/// handles on background-thread-drained stdout / stderr buffers so
/// individual tests can assert on the stub's log output without
/// plumbing pipes through every helper.
struct ChildGuard {
    child: Child,
    stdout_after_ready: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl ChildGuard {
    fn stdout_after_ready_snapshot(&self) -> Vec<u8> {
        snapshot(&self.stdout_after_ready)
    }

    fn stderr_snapshot(&self) -> Vec<u8> {
        snapshot(&self.stderr)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Best-effort: `kill()` and `wait()` errors are swallowed
        // because we are already in the unwinding-or-cleanup path.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn snapshot(buf: &Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    match buf.lock() {
        Ok(g) => g.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Spawn the stub on `127.0.0.1:0` and block until it prints the
/// `bound:` and `ready` lines. Returns the guard + the parsed
/// loopback address.
fn spawn_stub(token: &str) -> (ChildGuard, SocketAddr) {
    spawn_stub_at(token, "127.0.0.1:0")
}

/// Spawn the stub on an explicit `--bind <addr>` and block until it
/// prints the `bound:` and `ready` lines. Used by the explicit-port
/// scenario (WSI-1 #1) and any future test that needs to control the
/// bind.
fn spawn_stub_at(token: &str, bind: &str) -> (ChildGuard, SocketAddr) {
    let bin = env!("CARGO_BIN_EXE_stub-ingest");
    let mut child = Command::new(bin)
        .args(["--auth-token", token, "--bind", bind])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn stub-ingest");

    let stdout = child.stdout.take().expect("child stdout was piped");
    let stderr = child.stderr.take().expect("child stderr was piped");
    let mut reader = BufReader::new(stdout);

    // Synchronous gate on the bind protocol: read line-by-line off
    // the just-piped stdout until we see `bound: …` and then the
    // bare `ready` line. We do this on the main thread because the
    // tests need the parsed `SocketAddr` before they can send any
    // request. A reader-with-timeout refactor is deferred per the
    // round-1 review's WSI-5 follow-up.
    let mut addr: Option<SocketAddr> = None;
    let mut ready = false;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .expect("read stub-ingest stdout");
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        if let Some(rest) = trimmed.strip_prefix("bound: ") {
            addr = Some(
                rest.parse()
                    .unwrap_or_else(|e| panic!("parse bound line {rest:?}: {e}")),
            );
        } else if trimmed == "ready" {
            ready = true;
            break;
        }
    }
    let addr = addr.expect("stub-ingest printed `bound: …`");
    assert!(ready, "stub-ingest printed `ready` after the bound line");

    // After `ready`, the spec mandates stdout stays silent for the
    // lifetime of the process; the `stdout_stays_silent_after_ready_line`
    // test asserts the invariant by inspecting this buffer. The
    // drain thread exits naturally when the child closes its stdout
    // (Drop calls kill() + wait()).
    let stdout_after_ready = Arc::new(Mutex::new(Vec::<u8>::new()));
    spawn_drain_thread(reader, Arc::clone(&stdout_after_ready));

    // Stderr is the channel for stub-side log output (decode errors,
    // status reasons). Draining lets `malformed_body_returns_400_…`
    // and similar tests assert the expected one-line summary was
    // emitted.
    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    spawn_drain_thread(stderr, Arc::clone(&stderr_buf));

    (
        ChildGuard {
            child,
            stdout_after_ready,
            stderr: stderr_buf,
        },
        addr,
    )
}

/// Background-drain a `Read` source into the shared byte buffer.
/// Exits when the source EOFs (which the kill-on-drop guarantees
/// at test teardown).
fn spawn_drain_thread<R: Read + Send + 'static>(mut source: R, sink: Arc<Mutex<Vec<u8>>>) {
    thread::spawn(move || {
        let mut chunk = [0u8; 1024];
        loop {
            match source.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut guard = match sink.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    guard.extend_from_slice(&chunk[..n]);
                }
            }
        }
    });
}

/// Briefly yield to give the background drain threads time to copy
/// recently-written child output into the shared buffers before a
/// test snapshots them. Kernel pipe latency is microseconds, but
/// `cargo test` parallelism can stretch scheduling; 150ms is a
/// conservative margin for a test fixture.
fn wait_for_drain_to_settle() {
    thread::sleep(Duration::from_millis(150));
}

fn url(addr: SocketAddr, path: &str) -> String {
    format!("http://{addr}{path}")
}

fn sample_batch() -> wire::Batch {
    sample_batch_with_pid(9999)
}

/// `pid` varies so callers can build a sequence of distinguishable
/// batches for ordering / count assertions.
fn sample_batch_with_pid(pid: u32) -> wire::Batch {
    wire::Batch {
        meta: wire::MetaFull {
            schema_version: wire::SCHEMA_VERSION,
            trace_id: format!("0190b5e7-1c2d-7000-8000-{pid:012x}"),
            host: "round-trip-host".to_owned(),
            pid,
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

fn post_valid_batch(agent: &ureq::Agent, addr: SocketAddr, token: &str, batch: &wire::Batch) {
    let body = rmp_serde::to_vec_named(batch).expect("encode wire::Batch");
    let response = post_batch_with_headers(
        agent,
        addr,
        Some(&format!("Bearer {token}")),
        wire::MEDIA_TYPE,
        body,
    );
    assert_eq!(
        response.status().as_u16(),
        200,
        "POST /v1/ingest with valid headers returns 200",
    );
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
    let (guard, addr) = spawn_stub("test-token");
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

    // Spec scenario "A malformed body returns 400" also mandates a
    // one-line summary on stderr mentioning the decode error
    // (WSI-3). The stub writes `stub-ingest: decode error: <err>`
    // before responding 400.
    wait_for_drain_to_settle();
    let stderr = String::from_utf8_lossy(&guard.stderr_snapshot()).into_owned();
    assert!(
        stderr.contains("decode error"),
        "stderr should contain a `decode error` summary; got: {stderr:?}",
    );
}

#[test]
fn fresh_store_returns_empty_array() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    let batches = fetch_batches(&agent, addr);
    assert!(
        batches.is_empty(),
        "a freshly-spawned stub has an empty store; got {} batches",
        batches.len(),
    );
}

#[test]
fn two_posted_batches_appear_in_order() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    let b0 = sample_batch_with_pid(1000);
    let b1 = sample_batch_with_pid(2000);
    post_valid_batch(&agent, addr, "test-token", &b0);
    post_valid_batch(&agent, addr, "test-token", &b1);

    let stored = fetch_batches(&agent, addr);
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0], b0);
    assert_eq!(stored[1], b1);
}

#[test]
fn ten_sequential_posts_appear_in_order() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    let originals: Vec<wire::Batch> = (0..10).map(sample_batch_with_pid).collect();
    for batch in &originals {
        post_valid_batch(&agent, addr, "test-token", batch);
    }

    let stored = fetch_batches(&agent, addr);
    assert_eq!(stored.len(), 10);
    for (i, (expected, actual)) in originals.iter().zip(stored.iter()).enumerate() {
        assert_eq!(actual, expected, "batch at index {i} mismatched");
    }
}

#[test]
fn stdout_stays_silent_after_ready_line() {
    let (guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    // Drive ten valid POSTs and one GET so the stub processes a
    // representative workload. None of the happy-path handlers
    // write to stdout — the spec mandates stdout silence after
    // `ready` and the test asserts the invariant.
    for batch in (0..10).map(sample_batch_with_pid) {
        post_valid_batch(&agent, addr, "test-token", &batch);
    }
    assert_eq!(fetch_batches(&agent, addr).len(), 10);

    wait_for_drain_to_settle();
    let extra_stdout = guard.stdout_after_ready_snapshot();
    assert!(
        extra_stdout.is_empty(),
        "stdout must stay silent after `ready`; got: {:?}",
        String::from_utf8_lossy(&extra_stdout),
    );
}

#[test]
fn explicit_bind_port_is_honoured() {
    // Pick a free port via a throwaway listener, then immediately
    // release it. There is a small race window between releasing and
    // the stub binding (acceptable per spec — "or fails fast"); in
    // practice the loopback ephemeral range is wide enough that we
    // do not observe collisions on `cargo test` runs.
    let listener = TcpListener::bind("127.0.0.1:0").expect("pick a free loopback port");
    let chosen_port = listener
        .local_addr()
        .expect("local_addr on throwaway listener")
        .port();
    drop(listener);

    let bind = format!("127.0.0.1:{chosen_port}");
    let (_guard, addr) = spawn_stub_at("test-token", &bind);
    assert_eq!(
        addr.port(),
        chosen_port,
        "the stub honours the explicit --bind port",
    );

    // And the bound address actually accepts the round-trip POST,
    // confirming the explicit-port path is wired all the way through
    // the dispatch table.
    let agent = ureq_agent_no_status_err();
    post_valid_batch(&agent, addr, "test-token", &sample_batch());
    assert_eq!(fetch_batches(&agent, addr).len(), 1);
}

/// Mirror of the stub's `HeaderPair` serialisation shape. Local
/// to the integration test because the production `wire::*` types
/// have no business with header capture.
#[derive(serde::Deserialize, Debug)]
struct HeaderPair {
    name: String,
    value: String,
}

/// Fetch `/debug/last_request_headers`. Returns `None` on the
/// stub's 404 ("no ingest seen yet") and `Some(...)` on the 200
/// (decoded JSON body). Mirrors `fetch_batches`'s pattern but
/// distinguishes the two wire states the new endpoint exposes.
fn fetch_last_request_headers(agent: &ureq::Agent, addr: SocketAddr) -> Option<Vec<HeaderPair>> {
    let mut response = agent
        .get(url(addr, "/debug/last_request_headers"))
        .call()
        .expect("GET /debug/last_request_headers");
    match response.status().as_u16() {
        404 => None,
        200 => {
            let body: Vec<u8> = response
                .body_mut()
                .read_to_vec()
                .expect("read /debug/last_request_headers body");
            Some(
                serde_json::from_slice(&body)
                    .expect("parse /debug/last_request_headers as Vec<HeaderPair>"),
            )
        }
        other => panic!(
            "GET /debug/last_request_headers returned unexpected status {other}; expected 200 or 404",
        ),
    }
}

fn find_header_value<'a>(headers: &'a [HeaderPair], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

#[test]
fn last_request_headers_returns_404_before_any_ingest() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    let snapshot = fetch_last_request_headers(&agent, addr);
    assert!(
        snapshot.is_none(),
        "a freshly-spawned stub has not received any ingest; \
         /debug/last_request_headers must return 404, got {snapshot:?}",
    );
}

#[test]
fn last_request_headers_captures_authorization_content_type_and_user_agent_on_accepted_ingest() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    // Send a custom User-Agent so the assertion below is exact;
    // the default ureq User-Agent would also work but ties the
    // test to ureq's defaults.
    let body = rmp_serde::to_vec_named(&sample_batch()).expect("encode");
    let response = agent
        .post(url(addr, "/v1/ingest"))
        .header("Authorization", "Bearer test-token")
        .header("Content-Type", wire::MEDIA_TYPE)
        .header("User-Agent", "integration-test/1.0")
        .send(&body[..])
        .expect("POST /v1/ingest");
    assert_eq!(response.status().as_u16(), 200);

    let captured = fetch_last_request_headers(&agent, addr)
        .expect("after one accepted ingest the headers slot is populated");

    assert_eq!(
        find_header_value(&captured, "Authorization"),
        Some("Bearer test-token"),
        "captured Authorization must equal the byte-exact wire value; got {captured:?}",
    );
    assert_eq!(
        find_header_value(&captured, "Content-Type"),
        Some(wire::MEDIA_TYPE),
        "captured Content-Type must equal wire::MEDIA_TYPE; got {captured:?}",
    );
    assert_eq!(
        find_header_value(&captured, "User-Agent"),
        Some("integration-test/1.0"),
        "captured User-Agent must equal what the client sent; got {captured:?}",
    );
}

#[test]
fn last_request_headers_captures_authorization_on_401_rejected_ingest() {
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
    assert_eq!(
        response.status().as_u16(),
        401,
        "fixture sanity: the wrong bearer is rejected",
    );

    let captured = fetch_last_request_headers(&agent, addr)
        .expect("a 401-rejected ingest still populates the headers slot (capture-before-validate)");
    assert_eq!(
        find_header_value(&captured, "Authorization"),
        Some("Bearer wrong-token"),
        "the captured Authorization must equal what the client sent on the rejected attempt; got {captured:?}",
    );
}

#[test]
fn last_request_headers_returns_only_the_most_recent_request() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    let body = rmp_serde::to_vec_named(&sample_batch()).expect("encode");

    let first = agent
        .post(url(addr, "/v1/ingest"))
        .header("Authorization", "Bearer test-token")
        .header("Content-Type", wire::MEDIA_TYPE)
        .header("User-Agent", "first-agent/1.0")
        .send(&body[..])
        .expect("first POST /v1/ingest");
    assert_eq!(first.status().as_u16(), 200);

    let second = agent
        .post(url(addr, "/v1/ingest"))
        .header("Authorization", "Bearer test-token")
        .header("Content-Type", wire::MEDIA_TYPE)
        .header("User-Agent", "second-agent/2.0")
        .send(&body[..])
        .expect("second POST /v1/ingest");
    assert_eq!(second.status().as_u16(), 200);

    let captured =
        fetch_last_request_headers(&agent, addr).expect("slot populated after two ingests");
    assert_eq!(
        find_header_value(&captured, "User-Agent"),
        Some("second-agent/2.0"),
        "most-recent-wins: second ingest's User-Agent must be visible; got {captured:?}",
    );
    assert!(
        captured.iter().all(|h| h.value != "first-agent/1.0"),
        "no captured value should equal the first ingest's User-Agent; got {captured:?}",
    );
}

#[test]
fn last_request_headers_does_not_capture_debug_batches_request() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    // Drive a request that does NOT target the ingest path.
    // /debug/batches is the natural choice — every test calls it,
    // so a regression where it accidentally populated the slot
    // would surface fast.
    let batches = fetch_batches(&agent, addr);
    assert!(
        batches.is_empty(),
        "fixture sanity: fresh store starts empty",
    );

    let snapshot = fetch_last_request_headers(&agent, addr);
    assert!(
        snapshot.is_none(),
        "/debug/batches must NOT populate /debug/last_request_headers; got {snapshot:?}",
    );
}

#[test]
fn debug_reset_clears_the_captured_headers_slot() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    post_valid_batch(&agent, addr, "test-token", &sample_batch());
    assert!(
        fetch_last_request_headers(&agent, addr).is_some(),
        "fixture sanity: an accepted ingest populates the headers slot",
    );

    reset_store(&agent, addr);

    let snapshot = fetch_last_request_headers(&agent, addr);
    assert!(
        snapshot.is_none(),
        "POST /debug/reset must clear the captured headers slot; got {snapshot:?}",
    );
}

#[test]
fn readme_documents_the_five_routes_and_bind_protocol() {
    // `include_str!` is resolved at compile time relative to this
    // test source file, so the README's content is pinned into the
    // test binary alongside the assertions. The seven substrings
    // come straight from the `stub-ingest-server/spec.md` scenario
    // (widened from six → seven by `stub-ingest-connection-counter`).
    let readme = include_str!("../README.md");
    for needle in [
        "POST /v1/ingest",
        "GET /debug/batches",
        "GET /debug/last_request_headers",
        "GET /debug/connection_count",
        "POST /debug/reset",
        "--bind",
        "bound:",
    ] {
        assert!(
            readme.contains(needle),
            "stub-ingest/README.md must mention {needle:?} per the spec scenario",
        );
    }
}

/// Mirror of the stub's `CountBody` serialisation shape.
#[derive(serde::Deserialize, Debug)]
struct CountBody {
    count: usize,
}

/// GET `/debug/connection_count` and return the decoded count.
fn fetch_connection_count(agent: &ureq::Agent, addr: SocketAddr) -> usize {
    let mut response = agent
        .get(url(addr, "/debug/connection_count"))
        .call()
        .expect("GET /debug/connection_count");
    assert_eq!(
        response.status().as_u16(),
        200,
        "GET /debug/connection_count is always 200 (zero is meaningful; no 404 here)",
    );
    let body: Vec<u8> = response
        .body_mut()
        .read_to_vec()
        .expect("read /debug/connection_count body");
    let decoded: CountBody = serde_json::from_slice(&body).unwrap_or_else(|e| {
        panic!(
            "decode /debug/connection_count JSON: {e}; body: {}",
            String::from_utf8_lossy(&body)
        )
    });
    decoded.count
}

#[test]
fn connection_count_returns_zero_on_a_fresh_stub() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    let count = fetch_connection_count(&agent, addr);
    assert_eq!(
        count, 0,
        "a freshly-spawned stub has not seen any ingest-path connection; expected count=0, got {count}",
    );
}

#[test]
fn connection_count_returns_one_after_a_single_ingest() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    post_valid_batch(&agent, addr, "test-token", &sample_batch());

    let count = fetch_connection_count(&agent, addr);
    assert_eq!(
        count, 1,
        "one ingest from one agent yields one distinct connection; got {count}",
    );
}

#[test]
fn connection_count_stays_at_one_for_one_thousand_posts_through_one_agent() {
    // Stub-side direct binding of AC-SH-6: one ureq::Agent over
    // loopback keep-alive must result in exactly one distinct
    // remote_addr in the connection-set, regardless of how many
    // sequential POSTs the agent sends.
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    let body = rmp_serde::to_vec_named(&sample_batch()).expect("encode");
    for _ in 0..1000 {
        let response = post_batch_with_headers(
            &agent,
            addr,
            Some("Bearer test-token"),
            wire::MEDIA_TYPE,
            body.clone(),
        );
        assert_eq!(
            response.status().as_u16(),
            200,
            "every POST in the 1000-iteration loop must succeed",
        );
    }

    let count = fetch_connection_count(&agent, addr);
    assert_eq!(
        count, 1,
        "1000 sequential POSTs through one ureq::Agent over keep-alive \
         must use exactly one TCP connection (AC-SH-6 stub-side); got {count}",
    );
}

#[test]
fn connection_count_increments_when_a_new_agent_posts() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent_a = ureq_agent_no_status_err();
    let agent_b = ureq_agent_no_status_err();

    post_valid_batch(&agent_a, addr, "test-token", &sample_batch());
    post_valid_batch(&agent_b, addr, "test-token", &sample_batch());

    let count = fetch_connection_count(&agent_a, addr);
    assert!(
        count >= 2,
        "two distinct ureq::Agent instances must yield count >= 2; got {count}",
    );
}

#[test]
fn connection_count_counts_401_rejected_requests() {
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
    assert_eq!(
        response.status().as_u16(),
        401,
        "fixture sanity: the wrong bearer is rejected",
    );

    let count = fetch_connection_count(&agent, addr);
    assert_eq!(
        count, 1,
        "a 401-rejected request still consumed a TCP connection and must count; got {count}",
    );
}

#[test]
fn connection_count_does_not_count_debug_batches_request() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    // Drive a non-ingest request that does NOT target the ingest
    // path. The connection used by this GET must NOT be counted.
    let batches = fetch_batches(&agent, addr);
    assert!(
        batches.is_empty(),
        "fixture sanity: fresh store starts empty"
    );

    let count = fetch_connection_count(&agent, addr);
    assert_eq!(
        count, 0,
        "/debug/batches must NOT populate /debug/connection_count; got {count}",
    );
}

#[test]
fn debug_reset_clears_the_connection_set() {
    let (_guard, addr) = spawn_stub("test-token");
    let agent = ureq_agent_no_status_err();

    post_valid_batch(&agent, addr, "test-token", &sample_batch());
    assert_eq!(
        fetch_connection_count(&agent, addr),
        1,
        "fixture sanity: one ingest yields count=1",
    );

    reset_store(&agent, addr);

    let count = fetch_connection_count(&agent, addr);
    assert_eq!(
        count, 0,
        "POST /debug/reset must clear the connection-set; got {count}",
    );
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
