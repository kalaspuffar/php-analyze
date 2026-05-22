# `stub-ingest`

Test-only HTTP ingest receiver for `php-analyze`.

This binary is the receiving end of the `SPECIFICATION.md` §5.2 egress
contract — it accepts MessagePack-encoded `wire::Batch` payloads on
`POST /v1/ingest`, validates the bearer token, decodes the body via
the production crate's [`php_analyze::wire`] module (so the schema is
single-source-of-truth), and stores accepted batches in process
memory. Four debug endpoints (`GET /debug/batches`,
`GET /debug/last_request_headers`, `GET /debug/connection_count`,
and `POST /debug/reset`) let integration tests inspect and isolate
scenarios.

It is **not** a production ingest server. There is no TLS, no
gzip, no body-size cap, and the bearer comparison is non-constant-
time by design. The stub is loopback-only by convention.

## Build

```sh
cargo build -p stub-ingest
```

Like the rest of `php-analyze`, this crate path-deps on the production
extension and therefore requires `php-dev` headers on the host (see
the project's top-level `README.md` for the install matrix).

## Run

```sh
cargo run -p stub-ingest -- \
    --auth-token "$(cat /etc/php-analyze/token)" \
    --bind 127.0.0.1:8080
```

CLI flags:

| Flag | Default | Effect |
| --- | --- | --- |
| `--bind <addr>` | `127.0.0.1:0` | Address to bind. Port `0` lets the OS pick a free port; the bound port is then announced on stdout (see *Bind protocol* below). |
| `--auth-token <token>` | *(required)* | Bearer token clients must present as `Authorization: Bearer <token>`. |
| `--path <path>` | `/v1/ingest` | HTTP path on which the stub accepts ingest POSTs. The default matches `SPECIFICATION.md` OQ-3. |

## Bind protocol

With `--bind 127.0.0.1:0`, the stub writes exactly two lines to
stdout and flushes:

```
bound: 127.0.0.1:<port>
ready
```

Integration test harnesses parse the `bound:` line to discover the
port and gate on `ready` before sending requests. After `ready`, the
stub writes only to stderr; stdout stays silent for the rest of the
process's lifetime.

## Routes

| Method | Path | Status codes | Effect |
| --- | --- | --- | --- |
| `POST` | `/v1/ingest` (configurable via `--path`) | `200`, `401`, `415`, `400`, `405` | Validate bearer + content-type, decode the MessagePack body via `wire::Batch`, push onto the in-memory store. |
| `GET` | `/debug/batches` | `200` | Return the in-memory store as JSON (`Vec<wire::Batch>`). `Content-Type: application/json`. **No auth.** |
| `GET` | `/debug/last_request_headers` | `200`, `404` | Return the headers of the most-recent ingest request as a JSON array of `{name, value}` objects; `404` if no ingest has been received since process start or last `/debug/reset`. Populated *before* bearer/content-type/body validation so rejected requests are still observable. **No auth.** |
| `GET` | `/debug/connection_count` | `200` | Return the number of distinct ingest-path TCP connections as JSON `{"count": N}`. `0` on a freshly-spawned stub. Each unique `remote_addr` seen on the ingest path counts once; HTTP/1.1 keep-alive on a single client agent therefore keeps the count at `1` regardless of how many sequential POSTs the agent sent. **No auth.** |
| `POST` | `/debug/reset` | `200` | Empty the in-memory store, clear the `/debug/last_request_headers` slot, AND clear the `/debug/connection_count` set. **No auth.** |

The `/debug/*` paths are unauthenticated — they are debug surfaces
accessible only on the loopback bind, and integration tests use
them to inspect and isolate scenarios. The bearer requirement
applies to the ingest path only.

Status-code semantics for `POST /v1/ingest`:

- `200`: body decoded as a `wire::Batch`, appended to the store.
- `401`: `Authorization` header missing or `Bearer <token>` mismatch.
- `415`: `Content-Type` is missing or not
  `application/vnd.php-analyze.v1+msgpack` (the
  `php_analyze::wire::MEDIA_TYPE` constant).
- `400`: body could not be decoded as a `wire::Batch`.
- `405`: a non-POST method targeted the ingest path.

Any other route returns `404`.

## Out of scope

- **TLS / HTTPS.** The stub is `http://` only. The production-side
  TLS path (rustls + `rustls-native-certs`) is exercised in Phase-6
  hardening against the real ingest server.
- **gzip / `Content-Encoding`.** Per `SPECIFICATION.md` §5.2, v1
  payloads are not compressed.
- **Body-size cap.** The stub accepts arbitrarily large bodies; not
  appropriate for fuzz-tester use.
- **Real authentication.** The bearer compare is a plain byte-slice
  equality, not constant-time. A production server should use
  `subtle::ConstantTimeEq`.
- **Debug-route confidentiality.** `/debug/last_request_headers`
  returns the `Authorization` header value verbatim, which contains
  the bearer token the client sent. This is by design — the
  endpoint is the test seam that lets integration tests assert
  byte-equal on what the shipper transmitted. The loopback-only
  posture is what keeps the token off the network; do not expose
  the stub on a routable interface.

## References

- `SPECIFICATION.md` §4.2 — the MessagePack `Batch` schema (the
  `wire::Batch` type the stub decodes).
- `SPECIFICATION.md` §5.2 — the egress HTTP contract (the request
  shape the stub accepts).
- `SPECIFICATION.md` §10 Phase 3 — the project-plan slice that ships
  this crate.
