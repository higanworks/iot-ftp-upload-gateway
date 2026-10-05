# iot-ftp-upload-gateway

A minimal FTP gateway, written in Rust, that accepts plain FTP uploads from IoT devices
(USER/PASS/PASV/EPSV/STOR) and relays them to one of several backend FTP servers. It exists to
let FTP-only IoT devices upload through AWS Network Load Balancer / Kubernetes Service without
publishing a huge PASV port range per backend, and without running a general-purpose FTP proxy.

It's also, deliberately, **write-only**: `RETR`, `LIST`, and every other command that could read
data back out are rejected with `502` and never reach a Backend (see [Scope](#scope) below) — the
gateway is structurally incapable of serving files, only accepting them. That makes it a useful
boundary for legacy IoT fleets that still speak plain FTP and can't be upgraded to FTPS/TLS:
instead of exposing the real FTP servers directly to the Internet (a full read/write/list/delete
surface, in the clear, on hardware nobody wants to patch), put this gateway on the public/DMZ
edge and move the actual FTP servers onto a private network the gateway alone can reach. Devices
keep speaking the same plain FTP they always have; everything they can actually do is limited to
"upload a file," and even a fully compromised gateway process can't be used to read anything back
off the backends.

## Architecture

```mermaid
flowchart LR
    subgraph internet["Internet"]
        device1["Legacy IoT device"]
        device2["Legacy IoT device"]
        deviceN["..."]
    end

    subgraph dmz["Public / DMZ — only thing exposed"]
        gw["iot-ftp-upload-gateway<br/>write-only, allow-listed FTP commands"]
    end

    subgraph private["Private network — not Internet-reachable"]
        b1["Backend FTP #1"]
        b2["Backend FTP #2"]
        b3["Backend FTP #3"]
    end

    device1 -- "plain FTP<br/>control + PASV data" --> gw
    device2 -- "plain FTP" --> gw
    deviceN -- "plain FTP" --> gw
    gw -- "STOR only, round-robin<br/>plain FTP" --> b1
    gw -.-> b2
    gw -.-> b3
```

A session's control connection and its PASV data connection always land on the same
backend (picked round-robin when the session starts), so a client's upload is never split
across backends. See [Operational notes](#operational-notes) for how this behaves across
multiple gateway instances.

## Scope

Only the commands an IoT device needs to upload a file are implemented: `USER`, `PASS`,
`SYST`, `TYPE`, `PWD`, `CWD`, `PASV`, `EPSV`, `STOR`, `MKD`, `QUIT`, `NOOP`. Any other command
(`RETR`, `LIST`, `PORT`, etc.) is rejected with `502` and never reaches a Backend. IPv4 and
passive mode only; no Active mode, no IPv6, no general FTP command support. Clients always speak
plain FTP to the gateway (it does not terminate FTPS); the gateway-to-backend leg can optionally
be Explicit FTPS — see [Backend FTPS](#backend-ftps).

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
resets to `0` at the start of each new year/month) — e.g. `v2026.9.0`. See
[CHANGELOG.md](CHANGELOG.md) for what changed in each release.

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

| Setting | Config path | Environment variable | Default |
|---|---|---|---|
| Control listen address | `listen.address` | `GATEWAY_LISTEN_ADDRESS` | `0.0.0.0` |
| Control listen port | `listen.port` | `GATEWAY_LISTEN_PORT` | `21` |
| Address advertised in PASV replies | `passive.address` | `GATEWAY_PASSIVE_ADDRESS` | `0.0.0.0` |
| PASV port range start | `passive.port_range.start` | `GATEWAY_PASSIVE_PORT_RANGE_START` | `10000` |
| PASV port range end | `passive.port_range.end` | `GATEWAY_PASSIVE_PORT_RANGE_END` | `20000` |
| Backend servers | `backends` | `GATEWAY_BACKENDS` (`host:port[@first:last],host:port`) | *(required — startup fails if empty)* |
| Backend connect timeout (secs) | `timeouts.connection_timeout_secs` | `GATEWAY_CONNECTION_TIMEOUT_SECS` | `10` |
| Control connection idle timeout (secs) | `timeouts.idle_timeout_secs` | `GATEWAY_IDLE_TIMEOUT_SECS` | `300` |
| Backend command response timeout (secs) | `timeouts.command_timeout_secs` | `GATEWAY_COMMAND_TIMEOUT_SECS` | `30` |
| Data connection idle timeout (secs) | `timeouts.data_idle_timeout_secs` | `GATEWAY_DATA_IDLE_TIMEOUT_SECS` | `60` |
| Max control-line length (bytes) | `limits.max_command_line_bytes` | `GATEWAY_MAX_COMMAND_LINE_BYTES` | `4096` |
| Max concurrent connections per client IP | `limits.max_connections_per_ip` | `GATEWAY_MAX_CONNECTIONS_PER_IP` | `10` (`0` disables) |
| Metrics endpoint bind address | `metrics.address` | `GATEWAY_METRICS_ADDRESS` | `127.0.0.1` |
| Metrics endpoint port | `metrics.port` | `GATEWAY_METRICS_PORT` | *(unset — endpoint disabled)* |
| Backend TLS mode | `backend_tls.mode` | `GATEWAY_BACKEND_TLS` (`off` / `explicit`) | `off` |
| Backend TLS CA bundle (PEM) | `backend_tls.ca_file` | `GATEWAY_BACKEND_TLS_CA_FILE` | *(unset — bundled public roots)* |
| Backend TLS server name | `backend_tls.server_name` | `GATEWAY_BACKEND_TLS_SERVER_NAME` | *(unset — each backend's `host`)* |
| Backend TLS max version | `backend_tls.max_version` | `GATEWAY_BACKEND_TLS_MAX_VERSION` (`1.3` / `1.2`) | `1.3` |
| Backend data port range (per backend) | `backends[].passive_ports` | `@first:last` after a `GATEWAY_BACKENDS` entry | `8192:8200` |
| Backend source rotation | `backend_source.mode` | `GATEWAY_BACKEND_SOURCE` (`off` / `rotate`) | `off` |
| Source interfaces to use | `backend_source.include_interfaces` | `GATEWAY_BACKEND_SOURCE_INCLUDE` (comma-separated) | *(unset — all usable)* |
| Source interfaces to skip | `backend_source.exclude_interfaces` | `GATEWAY_BACKEND_SOURCE_EXCLUDE` (comma-separated) | *(unset)* |
| Interface re-listing interval (secs) | `backend_source.refresh_secs` | `GATEWAY_BACKEND_SOURCE_REFRESH_SECS` | `30` |

A control line (a client command or a backend reply) that exceeds `max_command_line_bytes`
without a terminating newline ends the session — a client hits `500 Command line too long`; a
backend hitting it is treated as a backend failure. Without this cap, a peer that never sends a
newline could make the gateway buffer an unbounded amount of memory for a single line.

`max_connections_per_ip` (default `10`) rejects a new connection outright (no reply, connection
closed) once a single client IP already holds that many open — without it, one misbehaving or
malicious source could open unlimited connections and exhaust file descriptors/memory on its
own. Set to `0` to disable the check entirely. Operators behind carrier-grade NAT, where many IoT
devices can share one public IP, should raise this or disable it.

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

## Backend FTPS

By default the gateway talks plain FTP to the backends. Setting `backend_tls.mode: explicit`
(`GATEWAY_BACKEND_TLS=explicit`) encrypts that leg with Explicit FTPS (RFC 4217) while clients
keep speaking plain FTP to the gateway. The setting applies to every backend.

For each backend session the gateway itself sends `AUTH TLS`, completes the TLS handshake, then
sends `PBSZ 0` and `PROT P` — before the client sees the backend's banner — so both the control
connection and every data connection to the backend are encrypted. Clients never see or send any
of these commands (a client-sent `AUTH` is still rejected with `502`).

- **Fails closed.** If the backend refuses `AUTH TLS`/`PROT P`, the handshake fails, or the
  certificate doesn't verify, the client gets `421` and the session ends. There is no fallback to
  plain FTP, and no option to skip certificate verification.
- **Certificate verification.** The certificate is checked against `backend_tls.server_name`, or
  each backend's `host` when unset — never against the IP in a PASV reply. Set `server_name` when
  the backend is reached by a name its certificate doesn't cover. `ca_file` (PEM, may hold several
  certificates) replaces the bundled public CA roots, for a private CA. A bad `ca_file` is a
  startup error.
- **Session reuse.** Many FTPS servers (e.g. vsftpd's default `require_ssl_reuse=YES`) only accept
  a data connection that resumes the control connection's TLS session. The gateway shares one TLS
  client configuration across all connections so data connections attempt to resume it. If a
  backend rejects data connections under TLS 1.3, try `max_version: "1.2"`. Confirm resumption
  works against your actual backend before relying on it.
- **Data connection handshake.** As servers expect, the data connection's TLS handshake happens
  after `STOR` is sent and answered with `150`, not when the data connection is opened.
- **Logging.** A failure to secure the control connection is logged at `WARN` ("failed to
  establish TLS with backend"). A data-connection handshake failure is logged at `WARN` as
  "upload failed: data relay error", with the TLS error in the `error` field.

Because the gateway acts as the TLS client on both connections, it trusts the backend's
certificate exactly as strictly as any other FTPS client would; it does not terminate TLS for
devices.

## Backend source rotation

Some backends serve data connections from a very small port range — AWS Transfer Family uses
8192–8200, nine ports — which caps how many uploads one client IP can have in flight. With
`backend_source.mode: rotate` (`GATEWAY_BACKEND_SOURCE=rotate`) the gateway connects to backends
from several local addresses instead of the one the OS would pick, multiplying that capacity by
the number of addresses. It is off by default; with it off, nothing here applies.

- **Sources.** Every IPv4 address on the host is a separate source — an interface with several
  addresses (EC2 secondary private IPs) counts once per address. By default all are used except
  loopback, link-local, and addresses on virtual interfaces (`lo`, `docker*`, `br-*`, `veth*`,
  `virbr*`, `cni*`, `flannel*`, `cali*`, `tun*`, `tap*`, `tailscale*`, `wg*`). Set
  `include_interfaces` to use only the interfaces you name (naming a virtual one is then
  honored), and `exclude_interfaces` to drop some; both accept `*` and `?` wildcards.
  Interfaces are listed again every `refresh_secs`, so one attached later (an EC2 ENI) is picked
  up without a restart. Startup fails if no usable source is found.
- **A session keeps one source.** Its control connection and every data connection come from the
  same address — backends tie a data connection to its control connection's client IP.
- **When the source changes.** Per backend, the gateway counts data connections (uploads) as they
  start, and after as many as that backend's `passive_ports` holds (9 for `8192:8200`) it moves
  its "current" source to the next address. *New* sessions are handed the current source;
  sessions already running stay where they are.
- **Capacity cap.** At most that many data transfers run at once from one source address to one
  backend. A further upload waits up to `timeouts.connection_timeout_secs` for a slot, then the
  client gets `425`. Without rotation there is no such cap.
- **Failures.** If connecting from a source fails because of the source (the address isn't
  usable, the network or host is unreachable, or the attempt times out), the gateway tries the
  next one for that session and leaves the failed one out of the rotation for 60 seconds. A
  refused connection is not held against the source — that is the backend, and every source would
  hit it. If no source can connect, the client gets `421`. The OS does not report whether an
  interface is up, so a down interface is found out this way.
- **`passive_ports` is only a size.** It sets each backend's capacity per source address (default
  `8192:8200`, i.e. 9). The port in a backend's PASV reply is not checked against it (one WARN per
  session is logged if it falls outside, with rotation on). Because the default applies to every
  backend, set `passive_ports` explicitly for any backend that isn't AWS Transfer Family once
  rotation is on, or it will be capped at 9 transfers per source address too.

```yaml
backends:
  - host: s-0123456789abcdef0.server.transfer.us-east-1.amazonaws.com
    port: 21
    passive_ports: "8192:8200"   # AWS Transfer Family's data ports (the default)
backend_source:
  mode: rotate
  include_interfaces: [ens5, ens6]
```

**What the host must provide** (not something the gateway can set up):

- **Several addresses reachable from outside the host**, e.g. one or more extra ENIs or secondary
  private IPs on the EC2 instance, with `network_mode: host` so the container sees them.
- **Source-based routing for any additional ENI.** A packet sent from a secondary ENI's address
  but routed out the primary ENI is dropped (AWS checks that a packet's source address belongs to
  the ENI it leaves through). Typically, per extra ENI (here `ens6`, address `10.0.2.20`, subnet
  gateway `10.0.2.1`):

  ```sh
  ip route add default via 10.0.2.1 dev ens6 table 100
  ip rule add from 10.0.2.20/32 table 100
  ```

  Secondary IPs on the *same* ENI as the primary address normally need no extra routing.
  A source without working routing is not fatal — the gateway skips it as described above — but
  it wastes a connect attempt (up to `connection_timeout_secs`) on each session that tries it.
- **No NAT or load balancer between the gateway and the backend.** The backend must see each
  source address as the client IP; behind a NAT every source looks the same and the extra
  capacity is lost. (AWS notes Transfer Family cannot recognize the client IP behind an NLB or
  NAT.)

Check the interfaces the gateway chose in its startup log (`backend source address rotation
enabled`, listing the addresses). Each session's log lines carry a `source_ip` field, and a
`WARN` is logged for every failed connection from a source.

Whether Transfer Family really limits data ports per client IP, and so whether extra source
addresses raise your concurrency, should be verified against your own endpoint.

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

#### Host kernel tuning

With `network_mode: host` and many concurrent long-lived sessions (see *Resource limits* under
[Operational notes](#operational-notes) below), a few kernel settings are worth setting ahead of
load rather than discovering under it:

```ini
# /etc/sysctl.d/99-iot-ftp-upload-gateway.conf

# Ephemeral port range: the gateway opens its own outbound connection to each Backend for
# every session's control connection and every STOR's data connection (it acts as a PASV
# *client* toward Backends -- see relay::open_backend_data_connection). Widen this if
# concurrent sessions approach the default range's ~28000 ports.
net.ipv4.ip_local_port_range = 10240 65535

# Mobile IoT devices reconnect often -- connection loss is expected, normal operation, not
# exceptional (see the client_io! handling in server/session.rs). The resulting TIME_WAIT
# churn can eat into the ephemeral port range faster than a steadier workload would. Reusing
# TIME_WAIT sockets for new outgoing connections is safe; shortening FIN_WAIT2 reclaims them
# sooner too.
net.ipv4.tcp_tw_reuse = 1
net.ipv4.tcp_fin_timeout = 30

# System-wide file descriptor ceiling -- distinct from (and must be >= ) the process-level
# `nofile` ulimit described under Resource limits below.
fs.file-max = 262144

# Accept-queue depth, for a burst of devices reconnecting at once after a network blip rather
# than a steady trickle.
net.core.somaxconn = 4096
net.ipv4.tcp_max_syn_backlog = 4096
```

```sh
sudo sysctl --system   # applies immediately; also picked up on reboot
```

Raise the process's own open-file ulimit to match (systemd unit `LimitNOFILE=`, or Docker's
`--ulimit nofile=<n>:<n>` / compose `ulimits:`) — sized as described under *Resource limits*
below, not left at the kernel/shell default.

**Not recommended:** `net.ipv4.tcp_tw_recycle` — removed from the kernel since Linux 4.12, and
even where still present, breaks clients sitting behind NAT (exactly where many IoT devices
are). `tcp_tw_reuse` above is the safe replacement for the same problem.

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

## Metrics

Opt-in `GET /metrics` HTTP endpoint in [Prometheus text exposition
format](https://github.com/prometheus/docs/blob/main/content/docs/instrumenting/exposition_formats.md),
disabled by default. Set `metrics.port` / `GATEWAY_METRICS_PORT` to enable it, on its own TCP
port — separate from the FTP control port, since the two protocols can't share one:

```sh
GATEWAY_METRICS_PORT=9273 cargo run -- --config path/to/config.yaml
curl http://127.0.0.1:9273/metrics
```

| Metric | Type | Meaning |
|---|---|---|
| `ftp_gateway_sessions_active` | gauge | FTP control connections currently open |
| `ftp_gateway_uploads_active` | gauge | `STOR` transfers currently in progress |
| `ftp_gateway_pasv_ports_active` | gauge | PASV/EPSV data ports currently allocated |
| `ftp_gateway_sessions_total` | counter | Total control connections accepted |
| `ftp_gateway_upload_bytes_total` | counter | Total bytes relayed to a Backend across all completed `STOR` transfers |
| `ftp_gateway_connections_rejected_total` | counter | Total connections rejected by the per-IP connection limit |

Every `_total` counter uses saturating addition, so it holds at `u64::MAX` under sustained load
instead of wrapping back to a small number.

By default `metrics.address` / `GATEWAY_METRICS_ADDRESS` binds to loopback (`127.0.0.1`) only —
this endpoint carries no per-client secrets (no filenames, IPs, or credentials), but it also
isn't meant to be reachable straight from the Internet by default. A sidecar/same-pod Prometheus
scraper (the common Kubernetes shape) reaches loopback fine; set this to `0.0.0.0` (and publish
the port, e.g. in `docker-compose.yml`) if something outside the container/pod needs to scrape
it directly.

The HTTP responder is hand-written rather than pulling in a framework (this gateway already
hand-writes the FTP side, and the endpoint only ever needs to answer `GET /metrics`): it bounds
request-line and header sizes, applies a read timeout against slow/stalled clients, serves
exactly one request per connection and then closes it, and returns `404`/`405` rather than
panicking on anything it doesn't recognize.

No CloudWatch/Prometheus integration ships with the gateway itself, but
[samples/cloudwatch_metrics.py](samples/cloudwatch_metrics.py) is a reference script that
scrapes this endpoint and publishes the values as CloudWatch custom metrics — see
[samples/README.md](samples/README.md) for usage and a systemd timer example.

## Changelog

See [CHANGELOG.md](CHANGELOG.md).

## License

MIT — see [LICENSE](LICENSE).
