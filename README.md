# iot-ftp-upload-gateway

A minimal FTP gateway, written in Rust, that accepts plain FTP uploads from IoT devices
(USER/PASS/PASV/STOR) and relays them to one of several backend FTP servers. It exists to let
FTP-only IoT devices upload through AWS Network Load Balancer / Kubernetes Service without
publishing a huge PASV port range per backend, and without running a general-purpose FTP proxy.

See [PROJECT.ja.md](PROJECT.ja.md) (Japanese) for the full design rationale and scope.

## Scope

Only the commands an IoT device needs to upload a file are implemented: `USER`, `PASS`,
`SYST`, `TYPE`, `PWD`, `CWD`, `PASV`, `STOR`, `QUIT`, `NOOP`. IPv4 and passive mode only;
no FTPS/TLS, no Active mode, no IPv6, no general FTP command support (`RETR`, `LIST`, `PORT`,
etc. are not implemented).

## Build

Requires the Rust toolchain pinned in [rust-toolchain.toml](rust-toolchain.toml) (managed via
[rustup](https://rustup.rs/)).

```sh
make build   # cargo build
make test    # cargo test
make check   # fmt-check + clippy + test
```

## Run

```sh
cargo run -- --config path/to/config.yaml
```

`--config` is optional. See [config.example.yaml](config.example.yaml) for the file format.
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

Backends are selected round-robin per session; a session's control connection and any PASV
data connections always stay on the same backend for the lifetime of that session.

**Important:** `passive.address` / `GATEWAY_PASSIVE_ADDRESS` is only the address advertised to
clients in the PASV reply — it is *not* the bind address. The PASV data listener always binds
`0.0.0.0`. Set this to whatever address clients can actually reach the gateway on (e.g. a
load balancer or container-published address); binding to that same address instead would make
the listener unreachable behind NAT/containers, which is exactly the deployment this gateway
targets.

## Docker

Multi-stage build producing a static (musl) binary on a `distroless/static` base — no shell,
no package manager, runs as a non-root user.

```sh
docker build -t iot-ftp-upload-gateway .
docker run -e GATEWAY_BACKENDS="ftp01:21,ftp02:21" \
           -e GATEWAY_PASSIVE_ADDRESS=<address clients can reach> \
           -e GATEWAY_PASSIVE_PORT_RANGE_START=10000 \
           -e GATEWAY_PASSIVE_PORT_RANGE_END=10010 \
           -p 21:21 -p 10000-10010:10000-10010 \
           iot-ftp-upload-gateway
```

`docker-compose.yml` spins up the gateway alongside two `delfer/alpine-ftp-server` backends on
a dedicated bridge network with static IPs, for local end-to-end testing without relying on the
host's loopback (which real containerized backends won't share with the gateway):

```sh
docker compose up -d --build
curl -T myfile.txt "ftp://iot:pass123@127.0.0.1:2131/myfile.txt"
```

Graceful shutdown: the gateway stops accepting new connections on SIGTERM/SIGINT, waits (up to
30s) for in-flight sessions to finish on their own, then exits — compatible with `docker stop`
and ECS task termination.

## Logging

Structured logs via `tracing`; set `RUST_LOG=info` (or `debug`) to see them. Every log line for
a session carries its client address and selected backend. FTP passwords are never logged
(`PASS` arguments are always redacted).
