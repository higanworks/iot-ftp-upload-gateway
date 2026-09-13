# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
  always redacted.
- Docker multi-stage build (`rust:bookworm` -> `distroless/cc`) and a `docker-compose.yml` local
  test environment with real FTP backends.

### Fixed

- A `PASV`/`EPSV` data connection could be starved by the client sending further commands
  immediately afterward, since control-line reads were always prioritized over accepting the
  pending data connection; the data connection is now accepted synchronously once `STOR`
  arrives instead of racing it against control reads.
- The address advertised to clients in PASV replies and the address the data listener bound to
  were the same config value; behind NAT/containers this made the listener unreachable. The
  listener now always binds `0.0.0.0`, independent of the advertised address.

[Unreleased]: https://github.com/higanworks/iot-ftp-upload-gateway/commits/main
