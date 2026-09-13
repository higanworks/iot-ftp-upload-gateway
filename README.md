# iot-ftp-upload-gateway

A minimal FTP gateway, written in Rust, that accepts plain FTP uploads from IoT devices
(USER/PASS/PASV/EPSV/STOR) and relays them to one of several backend FTP servers. It exists to
let FTP-only IoT devices upload through AWS Network Load Balancer / Kubernetes Service without
publishing a huge PASV port range per backend, and without running a general-purpose FTP proxy.

## Scope

Only the commands an IoT device needs to upload a file are implemented: `USER`, `PASS`,
`SYST`, `TYPE`, `PWD`, `CWD`, `PASV`, `EPSV`, `STOR`, `QUIT`, `NOOP`. IPv4 and passive mode
only; no FTPS/TLS, no Active mode, no IPv6, no general FTP command support (`RETR`, `LIST`,
`PORT`, etc. are not implemented).

`EPSV` (RFC 2428) is accepted alongside `PASV` and shares the exact same port pool and data
relay — some clients (and any client whose control connection happens to be IPv6, since PASV's
reply format can't represent an IPv6 address) prefer or require it. Unlike PASV's reply, EPSV's
`229` reply carries no address — the client is expected to reuse the control connection's
address — so it isn't affected by the PASV bind-vs-advertise distinction described below.

## Build

Requires the Rust toolchain pinned in [rust-toolchain.toml](rust-toolchain.toml) (managed via
[rustup](https://rustup.rs/)).

```sh
make build   # cargo build
make test    # cargo test
make check   # fmt-check + clippy + test
```

## Releases

Versions follow CalVer: `vYYYY.M.PATCH` (year, non-zero-padded month, and a patch number that
resets to `0` at the start of each new year/month) — e.g. `v2026.9.0`. `v0.1.0` was released
under the project's original SemVer scheme, before this switch; every version from `v2026.9.0`
onward is CalVer. See [CHANGELOG.md](CHANGELOG.md) for what changed in each release.

Pushing a version tag triggers [`.github/workflows/release.yml`](.github/workflows/release.yml),
which publishes a GitHub Release with prebuilt Linux binaries (`x86_64` and `aarch64`, built
natively rather than cross-compiled) and pushes a multi-arch (`linux/amd64`, `linux/arm64`)
Docker image to `ghcr.io/higanworks/iot-ftp-upload-gateway`, tagged with both the version and
`latest`. Building from source (below) is only needed for development.

**Prebuilt binary** — the `.../releases/latest/download/...` URL always resolves to the newest
release, so this never needs updating for new versions:

```sh
curl -L -o iot-ftp-upload-gateway.tar.gz \
  https://github.com/higanworks/iot-ftp-upload-gateway/releases/latest/download/iot-ftp-upload-gateway-x86_64-unknown-linux-gnu.tar.gz
# aarch64 hosts (e.g. AWS Graviton): swap in iot-ftp-upload-gateway-aarch64-unknown-linux-gnu.tar.gz

tar xzf iot-ftp-upload-gateway.tar.gz
./iot-ftp-upload-gateway --config path/to/config.yaml
```

**Docker image**:

```sh
docker pull ghcr.io/higanworks/iot-ftp-upload-gateway:latest
# or pin a specific version for production, e.g. :2026.9.0
```

See [Docker](#docker) below for how to run it.

## Run

```sh
cargo run -- --config path/to/config.yaml
```

`--config` is optional (works the same way with the prebuilt binary above, run directly instead
of via `cargo run --`). See [config.example.yaml](config.example.yaml) for the file format.
Configuration is layered as **defaults -> YAML file -> environment variables**, with
environment variables taking the highest priority — the same setting can be defined in the
config file and overridden per-environment via env vars.

| Setting | Config path | Environment variable |
|---|---|---|
| Control listen address | `listen.address` | `GATEWAY_LISTEN_ADDRESS` |
| Control listen port | `listen.port` | `GATEWAY_LISTEN_PORT` |
| Address advertised in PASV replies | `passive.address` | `GATEWAY_PASSIVE_ADDRESS` |
| PASV port range start | `passive.port_range.start` | `GATEWAY_PASSIVE_PORT_RANGE_START` |
| PASV port range end | `passive.port_range.end` | `GATEWAY_PASSIVE_PORT_RANGE_END` |
| Backend servers | `backends` | `GATEWAY_BACKENDS` (`host:port,host:port`) |
| Backend connect timeout (secs) | `timeouts.connection_timeout_secs` | `GATEWAY_CONNECTION_TIMEOUT_SECS` |
| Control connection idle timeout (secs) | `timeouts.idle_timeout_secs` | `GATEWAY_IDLE_TIMEOUT_SECS` |
| Backend command response timeout (secs) | `timeouts.command_timeout_secs` | `GATEWAY_COMMAND_TIMEOUT_SECS` |
| Data connection idle timeout (secs) | `timeouts.data_idle_timeout_secs` | `GATEWAY_DATA_IDLE_TIMEOUT_SECS` |
| Max control-line length (bytes) | `limits.max_command_line_bytes` | `GATEWAY_MAX_COMMAND_LINE_BYTES` |

A control line (a client command or a backend reply) that exceeds `max_command_line_bytes`
without a terminating newline ends the session — a client hits `500 Command line too long`; a
backend hitting it is treated as a backend failure. Without this cap, a peer that never sends a
newline could make the gateway buffer an unbounded amount of memory for a single line.

Backends are selected round-robin per session; a session's control connection and any PASV
data connections always stay on the same backend for the lifetime of that session.

`data_idle_timeout_secs` is inactivity-based, not a cap on total transfer time: the timer
resets on every byte moved in either direction during a `STOR`. This matters for IoT devices on
mobile networks, which can have long stretches of low throughput without the connection actually
being dead — a slow-but-active upload is never cut off, but a connection that goes completely
silent is detected and cleaned up rather than held open (and its PASV port leaked) forever.

**Important:** `passive.address` / `GATEWAY_PASSIVE_ADDRESS` is only the address advertised to
clients in the PASV reply — it is *not* the bind address. The PASV data listener always binds
`0.0.0.0`. Set this to whatever address clients can actually reach the gateway on (e.g. a
load balancer or container-published address); binding to that same address instead would make
the listener unreachable behind NAT/containers, which is exactly the deployment this gateway
targets.

## Docker

```sh
docker pull ghcr.io/higanworks/iot-ftp-upload-gateway:latest
docker run -e GATEWAY_BACKENDS="ftp01:21,ftp02:21" \
           -e GATEWAY_PASSIVE_ADDRESS=<address clients can reach> \
           -e GATEWAY_PASSIVE_PORT_RANGE_START=10000 \
           -e GATEWAY_PASSIVE_PORT_RANGE_END=10010 \
           -p 21:21 -p 10000-10010:10000-10010 \
           ghcr.io/higanworks/iot-ftp-upload-gateway:latest
```

To build the image yourself instead of pulling the published one (e.g. for local changes): a
multi-stage build producing a glibc-linked binary on a `distroless/cc` base — no shell, no
package manager, runs as a non-root user.

```sh
docker build -t iot-ftp-upload-gateway .
# then run it the same way as above, using this tag instead of the ghcr.io one
```

`docker-compose.yml` spins up the gateway alongside three `delfer/alpine-ftp-server` backends
on a dedicated bridge network with static IPs — needed because each backend advertises its PASV
address as a literal IP (the FTP protocol has no way to say "ask DNS"), so each container's IP
has to be known ahead of time to configure it:

```sh
docker compose up -d --build
curl -T myfile.txt "ftp://iot:pass123@127.0.0.1:2131/myfile.txt"
```

The same topology, on its own subnet, also runs in CI as
[`docker-compose.ci.yml`](docker-compose.ci.yml) via
[`.github/workflows/integration.yml`](.github/workflows/integration.yml): it brings the stack up,
uploads through the gateway from 4 simulated clients, and reads each file back directly from the
backend it should have landed on (round-robin, including the wraparound) to confirm both content
integrity and correct backend selection — see
[`scripts/docker-compose-integration-test.sh`](scripts/docker-compose-integration-test.sh).

Graceful shutdown: the gateway stops accepting new connections on SIGTERM/SIGINT, waits (up to
30s) for in-flight sessions to finish on their own, then exits — compatible with `docker stop`
and ECS task termination.

### Production deployment on EC2 (host networking)

The bridge-network-with-static-IPs setup above is for running multiple backends as sibling
containers on one host (local dev, CI); a single production gateway has no such conflict. On
EC2, use `network_mode: host` instead so the gateway listens directly on the
host's network — no port mapping needed, and `GATEWAY_PASSIVE_ADDRESS` should be the EC2
instance's own address:

```yaml
services:
  gateway:
    image: ghcr.io/higanworks/iot-ftp-upload-gateway:latest # pin a version tag in production
    network_mode: host
    environment:
      GATEWAY_BACKENDS: "ftp01:21,ftp02:21"
      GATEWAY_PASSIVE_ADDRESS: "<EC2 instance address>"
      GATEWAY_PASSIVE_PORT_RANGE_START: "10000"
      GATEWAY_PASSIVE_PORT_RANGE_END: "20000"
```

The EC2 Security Group must allow inbound access to the control port and the full configured
PASV port range — there is no Docker port mapping to fall back on with host networking.

## Operational notes

**Resource limits.** One active upload holds roughly four sockets/file descriptors at once:
client control, backend control, client data, backend data. Size the host/container's `nofile`
ulimit for `max_concurrent_sessions × ~4`, not daily upload count — the gateway is designed
around many long-lived concurrent sessions from mobile IoT devices, not a high daily volume of
short ones. The gateway itself doesn't impose an artificial concurrency cap; a transient
`accept()` failure (e.g. hitting the fd limit) is logged and the gateway keeps serving existing
sessions rather than crashing.

**DNS round-robin deployment.** Each gateway instance is fully independent — no state is shared
between instances (the PASV port pool and backend round-robin counter are both in-process only).
DNS round-robin distributes *new* sessions across instances; a long-running session stays pinned
to whichever instance accepted it. If an instance is lost, its in-flight sessions fail and the
IoT device is expected to reconnect and retry — the gateway does not attempt to resume or
migrate sessions itself.

## Logging

Structured logs via `tracing`; set `RUST_LOG=info` (or `debug`) to see them. Every log line for
a session carries a `session_id` (unique per accepted connection, independent of the client's
ephemeral source port — the key to group one client's log lines together, including across a
reconnect), the `client_ip` it came from, and the selected backend. FTP passwords are never
logged (`PASS` arguments are always redacted). Uploads log their transfer duration (`duration_ms`)
and byte count alongside the filename. Client-supplied strings (the filename, and command
arguments in the raw-command debug log) have any control characters escaped before being written
to the human-readable text log format, so a client can't forge extra log lines or terminal
control sequences by embedding them in a filename.

Raw per-command traffic (`received command`) is logged at DEBUG rather than INFO — it's
high-volume and low-signal for normal operation, and log processors like CloudWatch Logs
Insights bill per byte ingested. The commands that matter operationally (PASV/STOR outcomes,
QUIT ending the session) are already logged at INFO in their own right. Set `RUST_LOG=debug` to
see raw commands too.

By default logs are human-readable text. Set `GATEWAY_LOG_FORMAT=json` for structured JSON
output (one object per line) instead — suited to log processors that parse JSON fields directly,
such as AWS CloudWatch Logs Insights. This is read directly from the environment before startup,
independently of the layered YAML/env `Config` system used for everything else, since logging
has to be initialized before there's anything to log with.

Connection loss is expected on mobile IoT networks, not exceptional: a client disconnecting
(cleanly or via a reset/broken pipe) is logged at INFO ("client disconnected"), while a backend
failure or timeout is logged at WARN ("backend connection failed") — normal client churn should
never show up as a warning.

## Changelog

See [CHANGELOG.md](CHANGELOG.md).

## License

MIT — see [LICENSE](LICENSE).
