# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Versions follow CalVer: `vYYYY.M.PATCH` (year, non-zero-padded month, and a patch number that
resets to `0` at the start of each new year/month). `v0.1.0` was released under the project's
original SemVer scheme, before this switch; every version from `v2026.9.0` onward is CalVer.

## [Unreleased]

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

[Unreleased]: https://github.com/higanworks/iot-ftp-upload-gateway/compare/v2026.9.1...HEAD
[2026.9.1]: https://github.com/higanworks/iot-ftp-upload-gateway/compare/v2026.9.0...v2026.9.1
[2026.9.0]: https://github.com/higanworks/iot-ftp-upload-gateway/compare/v0.1.0...v2026.9.0
[0.1.0]: https://github.com/higanworks/iot-ftp-upload-gateway/releases/tag/v0.1.0
