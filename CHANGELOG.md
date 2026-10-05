# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Versions follow CalVer: `vYYYY.M.PATCH` (year, non-zero-padded month, and a patch number that
resets to `0` at the start of each new year/month). `v0.1.0` was released under the project's
original SemVer scheme, before this switch; every version from `v2026.9.0` onward is CalVer.

## [Unreleased]

### Added

- More metrics on `/metrics`: `ftp_gateway_uploads_started_total`, `_completed_total` and
  `_failed_total`; `ftp_gateway_backend_pasv_failures_total`,
  `ftp_gateway_backend_data_connection_failures_total`, `ftp_gateway_backend_timeouts_total`,
  `ftp_gateway_data_connections_rejected_total`, `ftp_gateway_session_idle_timeouts_total` and
  `ftp_gateway_pasv_ports_capacity`; and, with source rotation on, per-source series
  (`ftp_gateway_backend_source_unhealthy`, `..._transfers_active`, `..._transfers_capacity`,
  `..._transfers_total`, `..._slot_timeouts_total`) showing whether a source address has reached
  the backend's concurrency limit.

### Security

- A client's data connection is now only accepted from the same IP address as its control
  connection (`limits.require_data_ip_match`, **on by default**; `GATEWAY_REQUIRE_DATA_IP_MATCH`).
  The PASV port the gateway announces is open to the whole network until the client connects to
  it, so previously any host that reached the port first became the data connection for that
  client's upload — its bytes stored under the real client's filename. Connections from other
  addresses are closed and logged, and the gateway keeps waiting for the real one. **Upgrade
  note:** devices whose control and data connections leave from different addresses (some
  carrier-grade NAT pools) are refused with the default; set the option to `false` for them. It
  does nothing behind a load balancer that does not preserve the client IP.

### Fixed

- A backend that stopped answering `PASV` could hold a gateway session, the PASV port and (with
  source rotation) the source-address slot forever, because the reply was awaited without any
  timeout, line-length limit, or a timeout on connecting to the data port it announced. The reply
  now gets `timeouts.command_timeout_secs` and the `limits.max_command_line_bytes` cap, and the
  data port connection `timeouts.connection_timeout_secs`.

### Changed

- When the backend's `PASV` reply does not arrive, is cut off, or is overlong, or the backend
  closes its control connection, the session now ends with `421` instead of answering the
  upload with `425` and carrying on: the backend connection can no longer be trusted to be in
  step with the commands sent, since a late reply would be read as the answer to the next one.
  (A complete reply that is not a PASV address, or a data port that cannot be reached, still
  fails only that upload with `425`.)

## [2026.10.0] - 2026-10-06

### Added

- Optional Explicit FTPS (RFC 4217) between the gateway and the backends:
  `backend_tls.mode: explicit` (`GATEWAY_BACKEND_TLS=explicit`) makes the gateway negotiate
  `AUTH TLS` / `PBSZ 0` / `PROT P` with each backend, encrypting both the control and data
  connections, while clients keep speaking plain FTP. Supports a private CA
  (`backend_tls.ca_file`), a certificate name override (`backend_tls.server_name`), and pinning
  TLS 1.2 (`backend_tls.max_version`). Data connections resume the control connection's TLS
  session for backends that require session reuse. Fails closed: a backend that can't complete
  the negotiation ends the session with `421`, never falling back to plain FTP. Off by default;
  existing deployments are unaffected.
- Optional source-address rotation for backend connections, for backends that serve data
  connections from a tiny port range (AWS Transfer Family: 8192–8200): with
  `backend_source.mode: rotate` (`GATEWAY_BACKEND_SOURCE=rotate`) the gateway connects to
  backends from each local IPv4 address in turn instead of the one the OS picks. A session keeps
  one source for its control and data connections; per backend, new sessions move on to the
  next source after as many data connections as the backend's data port range holds, and at most
  that many transfers run at once per source address (extra uploads wait, then get `425`).
  Sources are chosen with `backend_source.include_interfaces` / `exclude_interfaces` (wildcards
  allowed; by default every address except loopback, link-local and virtual interfaces),
  re-listed every `refresh_secs`, and a source that fails to connect is skipped for 60 seconds.
  Off by default; existing deployments are unaffected.
- A per-backend data port range, `backends[].passive_ports` (`GATEWAY_BACKENDS` entries accept
  `host:port@first:last`), default `8192:8200`. Only its size is used, and only with source
  rotation on.

## [2026.9.5] - 2026-09-16

### Changed

- Backend connections now resolve DNS through a 30-second cache instead of on every new
  session, and enable `TCP_NODELAY` on both the client and backend control connections to avoid
  Nagle-induced latency on their frequent small command/reply round trips.
- The client-to-backend data relay buffer grew from 8 KiB to 64 KiB, and its idle-timeout
  tracking switched from re-arming a timer on every read to a single timer reset on progress,
  cutting per-chunk overhead during uploads.
- PASV/EPSV port allocation now starts its search for a free port where the previous allocation
  left off, instead of always rescanning from the start of the configured range, keeping
  allocation cost low under a busy port pool.

## [2026.9.4] - 2026-09-14

### Added

- `GET /metrics` HTTP endpoint in Prometheus text exposition format, exposing
  `ftp_gateway_sessions_active`, `ftp_gateway_uploads_active`, `ftp_gateway_pasv_ports_active`,
  `ftp_gateway_sessions_total`, `ftp_gateway_upload_bytes_total`, and
  `ftp_gateway_connections_rejected_total`. Opt-in via `metrics.port` / `GATEWAY_METRICS_PORT`,
  on a separate TCP port from the FTP listener, bound to loopback by default
  (`metrics.address` / `GATEWAY_METRICS_ADDRESS`).

### Security

- The metrics HTTP responder bounds request-line and header sizes, applies a read timeout
  against slowloris-style clients, serves exactly one request per connection, and never panics
  on malformed input. All `*_total` counters use saturating addition so they hold at `u64::MAX`
  under sustained load instead of wrapping.

## [2026.9.3] - 2026-09-14

### Added

- `MKD` support, so clients like `curl --ftp-create-dirs` can create the directories a `STOR`
  path implies (it `CWD`s into each path component, `MKD` + retries `CWD` on a `550`, then
  `STOR`s the bare filename).

### Security

- Unrecognized commands (`RETR`, `DELE`, `LIST`, etc.) are now rejected with `502` instead of
  being forwarded to the Backend. Previously any command not special-cased for
  `PASV`/`EPSV`/`STOR` was relayed verbatim regardless of whether the Gateway recognized it;
  only the commands PROJECT_SECURITY.md section 2 lists as supported are now ever forwarded.

## [2026.9.2] - 2026-09-14

### Security

- The control-line reader now rejects any command whose argument still contains an embedded CR
  or LF after the trailing line terminator is stripped, responding `501` instead of forwarding
  it. Previously such a line was relayed to the Backend verbatim, so a crafted argument (e.g. a
  `STOR` filename) could smuggle a second command past the Backend (PROJECT_SECURITY.md section
  4).

## [2026.9.1] - 2026-09-13

### Added

- `limits.max_connections_per_ip` / `GATEWAY_MAX_CONNECTIONS_PER_IP` config option (default
  `10`): caps how many concurrent connections a single client IP may hold open, so one
  misbehaving or malicious source can't exhaust file descriptors/memory on its own. `0` disables
  the check.

## [2026.9.0] - 2026-09-13

### Added

- Per-session `session_id` and `client_ip` log fields, so every log line for one client
  connection can be grouped and filtered independently of its ephemeral source port.
- `limits.max_command_line_bytes` / `GATEWAY_MAX_COMMAND_LINE_BYTES` config option: caps how much
  a single control line (client command or backend reply) can grow before a terminating newline,
  closing an unbounded-memory-growth vector for a peer that never sends one.
- CI now runs a real Docker Compose integration test (gateway + 3 real FTP backends) alongside
  the existing `cargo test` suite.

### Changed

- Raw per-command logging (`received command`) moved from INFO to DEBUG to reduce log volume
  (and log-processor ingestion cost) for normal operation.
- Versioning policy switched from SemVer to CalVer (`vYYYY.M.PATCH`); this is the first CalVer
  release.

### Security

- Control characters in client-supplied strings (filenames, command arguments) are now escaped
  before being written to the human-readable text log format, preventing log/terminal injection
  via a crafted filename.

## [0.1.0] - 2026-09-13

### Added

- Minimal FTP gateway relaying `USER`/`PASS`/`SYST`/`TYPE`/`PWD`/`CWD`/`PASV`/`EPSV`/`STOR`/
  `QUIT`/`NOOP` from IoT clients to a backend FTP server (IPv4, passive mode only).
- Real PASV/EPSV data port allocation from a configurable port range, with bidirectional TCP
  relay to the backend's own passive data connection.
- Round-robin backend selection; a session's control connection and any PASV/EPSV data
  connections always stay on the same backend for the session's lifetime.
- Configuration layered as defaults -> YAML file -> environment variables, with environment
  variables taking highest priority.
- Configurable timeouts: backend connect, control-connection idle, backend command response,
  and data-connection idle (inactivity-based, not a total-transfer-time cap).
- Graceful shutdown on SIGTERM/SIGINT: stop accepting new connections, drain in-flight sessions.
- Structured logging via `tracing`, with client disconnects logged at INFO and backend
  failures/timeouts at WARN, upload duration and byte count on every transfer, and FTP passwords
  always redacted. Optional JSON output (`GATEWAY_LOG_FORMAT=json`) for log processors such as
  AWS CloudWatch Logs Insights.
- Docker multi-stage build (`rust:bookworm` -> `distroless/cc`) and a `docker-compose.yml` local
  test environment with real FTP backends.

[Unreleased]: https://github.com/higanworks/iot-ftp-upload-gateway/compare/v2026.10.0...HEAD
[2026.10.0]: https://github.com/higanworks/iot-ftp-upload-gateway/compare/v2026.9.5...v2026.10.0
[2026.9.5]: https://github.com/higanworks/iot-ftp-upload-gateway/compare/v2026.9.4...v2026.9.5
[2026.9.4]: https://github.com/higanworks/iot-ftp-upload-gateway/compare/v2026.9.3...v2026.9.4
[2026.9.3]: https://github.com/higanworks/iot-ftp-upload-gateway/compare/v2026.9.2...v2026.9.3
[2026.9.2]: https://github.com/higanworks/iot-ftp-upload-gateway/compare/v2026.9.1...v2026.9.2
[2026.9.1]: https://github.com/higanworks/iot-ftp-upload-gateway/compare/v2026.9.0...v2026.9.1
[2026.9.0]: https://github.com/higanworks/iot-ftp-upload-gateway/compare/v0.1.0...v2026.9.0
[0.1.0]: https://github.com/higanworks/iot-ftp-upload-gateway/releases/tag/v0.1.0
