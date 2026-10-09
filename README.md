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

With [`backend_tls`](#backend-ftps) enabled, the gateway-to-backend leg is Explicit FTPS
instead — both the control and the data connections are encrypted — while devices still speak
plain FTP. The hop that crosses the network you don't control can be secured without touching
the devices:

```mermaid
flowchart LR
    subgraph internet["Internet"]
        device1["Legacy IoT device"]
        device2["Legacy IoT device"]
        deviceN["..."]
    end

    subgraph dmz["Public / DMZ — only thing exposed"]
        gw["iot-ftp-upload-gateway<br/>write-only, allow-listed FTP commands<br/>backend_tls: explicit"]
    end

    subgraph private["Private network / VPC"]
        b1["Backend FTPS #1"]
        b2["Backend FTPS #2"]
        b3["Backend FTPS #3"]
    end

    device1 -- "plain FTP<br/>control + PASV data" --> gw
    device2 -- "plain FTP" --> gw
    deviceN -- "plain FTP" --> gw
    gw -- "STOR only, round-robin<br/>Explicit FTPS (TLS)<br/>control + data" --> b1
    gw -.-> b2
    gw -.-> b3
```

A session's control connection and its PASV data connection always land on the same
backend (picked round-robin when the session starts), so a client's upload is never split
across backends. See [Operational notes](#operational-notes) for how this behaves across
multiple gateway instances.

For backends that serve data connections from a very small port range, such as AWS Transfer
Family, the gateway can also spread its backend connections over several local addresses
([`backend_source`](#backend-source-rotation)); that setup is described in
[Using AWS Transfer Family as the backend](#using-aws-transfer-family-as-the-backend).

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
| Require data connection from the control connection's IP | `limits.require_data_ip_match` | `GATEWAY_REQUIRE_DATA_IP_MATCH` (`true` / `false`) | `true` |
| Command used to open a backend data connection | `limits.backend_passive_mode` | `GATEWAY_BACKEND_PASSIVE_MODE` (`epsv` / `pasv`) | `epsv` |
| Use the backend's address when its PASV reply says `0.0.0.0` | `limits.backend_pasv_fallback_to_control_ip` | `GATEWAY_BACKEND_PASV_FALLBACK_TO_CONTROL_IP` (`true` / `false`) | `true` |
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

`backend_passive_mode` (default `epsv`): to open a data connection to the backend, the gateway
sends `EPSV` first. Its reply carries only a port, and the gateway connects to the address its
control connection goes to, so a backend cannot send it to a wrong address (a private or `0.0.0.0`
address in `PASV`'s reply, for instance). A backend that rejects `EPSV` with a `5xx` reply is
asked with `PASV` instead on the same connection. Set `pasv` to send only `PASV`, as versions up
to v2026.10.3 did. This is independent of what the *client* sends the gateway: either command is
accepted there.

`backend_pasv_fallback_to_control_ip` (default `true`): when `PASV` is used, some backends reply to `PASV` with the
address `0.0.0.0` -- vsftpd does when it cannot work out its own address, e.g. with
`listen_ipv6=YES`. Connecting to `0.0.0.0` reaches the gateway's own host, so the data connection
would fail with `Connection refused`. With this option the gateway connects to the address its
control connection goes to instead, as most FTP clients do, and logs a warning. The better fix is
on the backend (vsftpd: `pasv_address`). Set the option to `false` to take the reply literally.

`require_data_ip_match` (default `true`) only accepts a client's data connection from the same IP
address as that client's control connection. The PASV port the gateway announces is open to the
whole network until the client connects to it, so without this check any host that reached the
port first would become the data connection for that client's upload — its bytes stored under the
real client's filename. A connection from any other address is logged at `WARN`, closed, and
counted (`ftp_gateway_data_connections_rejected_total`); the gateway keeps waiting for the real
one until the data connection wait (`timeouts.connection_timeout_secs`) runs out, so connecting
to the port cannot keep the real client out. Two deployments need care:

- **Devices whose control and data connections leave from different addresses** — some
  carrier-grade NAT pools do this — would be refused. Set it to `false` for them.
- **A load balancer in front of the gateway that does not preserve the client IP** makes every
  connection look like it comes from the balancer, so the check always passes and protects
  nothing.

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
  a data connection that resumes the control connection's TLS session. Each FTP session gets a
  TLS resumption store of its own, shared by that session's control and data connections and by
  nothing else, so a data connection resumes *its own* control connection's session — never one
  left behind by another client's session. (TLS 1.3 tickets are single-use; the gateway keeps the
  new ones the backend sends on every connection of the session, so a session can make many
  uploads — verified with a dozen in a row on both TLS 1.3 and 1.2 against a rustls server, not
  against your backend.) If a backend rejects data connections under TLS 1.3, try
  `max_version: "1.2"`, whose sessions can be reused any number of times. Confirm resumption
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
  addresses (EC2 secondary private IPs) counts once per address, and they are used in ascending
  order of address. An address the OS labels as an alias (`ens5:1`) belongs to its interface
  (`ens5`) for `include_interfaces` / `exclude_interfaces`; naming the alias itself
  (`ens5:1`, `ens5:*`) singles it out. By default all are used except
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
  include_interfaces: [ens5, ens6]   # optional: omit to auto-detect (see "Sources" above)
```

On a typical EC2 host, `mode: rotate` on its own is usually enough: auto-detection already skips
loopback, link-local and Docker's interfaces and keeps the instance's real addresses.
`include_interfaces` is for pinning the choice explicitly.

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
  capacity is lost. For AWS Transfer Family this is also AWS's own guidance — see
  [Endpoint placement](#endpoint-placement-no-nlb-no-nat).

Check the interfaces the gateway chose in its startup log (`backend source address rotation
enabled`, listing the addresses). Each session's log lines carry a `source_ip` field, and a
`WARN` is logged for every failed connection from a source.

Whether Transfer Family really limits data ports per client IP, and so whether extra source
addresses raise your concurrency, should be verified against your own endpoint.

## Using AWS Transfer Family as the backend

AWS Transfer Family's FTP/FTPS endpoints differ from a self-hosted FTP server in several ways
that matter here, and this gateway has settings for the ones it can help with. The gateway is the
FTP client of the endpoint, so everything below is about its connections *to* Transfer Family;
devices are unaffected.

### Endpoint placement: no NLB, no NAT

FTP and FTPS servers on Transfer Family can only be created inside a VPC — there is no public
endpoint for them
([AWS docs](https://docs.aws.amazon.com/transfer/latest/userguide/infrastructure-security.html#nlb-considerations)).
A common workaround is to put a Network Load Balancer (NLB) in front of the server, but **AWS
recommends against that for FTP and FTPS**: an NLB in the path increases costs and reduces the
number of simultaneous connections the server accepts
([Working with Network Load Balancers](https://docs.aws.amazon.com/transfer/latest/userguide/working-with-nlb.html)).

The reason is that the server only sees the address of the NLB or NAT gateway, not the real
client. AWS documents that Transfer Family *uses the source IP address to shard connections
across its data plane*, so for FTPS a server with an NLB or NAT gateway in the path is limited to
about **300 simultaneous connections instead of 10,000**, and you can no longer audit who is
connecting
([Avoid placing NLBs and NATs in front of AWS Transfer Family servers](https://docs.aws.amazon.com/transfer/latest/userguide/infrastructure-security.html#nlb-considerations)).
AWS's recommended alternative is a VPC endpoint with an Elastic IP address.

What that means for this gateway:

- Connect the gateway to the endpoint **directly**: same VPC, or a routed path such as peering,
  Transit Gateway, VPN or Direct Connect that does not translate addresses. A NAT gateway between
  them makes every gateway connection come from one address, which both hits the lower connection
  limit and defeats [source rotation](#backend-source-rotation).
- This applies to the path *between the gateway and Transfer Family* only. Putting an NLB in
  front of the **gateway** — which is what this project exists to allow for devices — is a
  different hop and unaffected.
- With direct connections, the gateway's own source addresses are what Transfer Family shards
  on, which is why using several of them helps (next section).

### The PASV problem: nine data ports

An FTP upload needs a data connection, and in passive mode the *server* chooses its port and
announces it in the `PASV` reply. Transfer Family announces ports from a fixed range of
**8192–8200 — nine ports**
([AWS docs](https://docs.aws.amazon.com/transfer/latest/userguide/create-server-ftps.html)).
Consequences for a gateway that funnels many devices into one place:

- Every in-flight upload occupies one of those nine ports. If the capacity is per client IP — a
  plausible reading, given that AWS shards connections by source IP (see above) and says the
  endpoint's security group should allow the range *from the client IP CIDR ranges*, but
  **not stated outright in the docs, so verify it against your endpoint** — then a gateway
  connecting from a single address can have about nine uploads in flight to that endpoint, no
  matter how many devices are connected to it.
- The gateway cannot pick the port or widen the range: the port arrives in the backend's reply.
  Without `backend_source` (the default) it also does not limit concurrency, so what happens to
  a burst of uploads beyond the capacity is decided by the backend (expect refused or failed
  transfers).
- The way to raise the ceiling is more client addresses: with *N* source addresses the capacity
  is roughly *N* × 9. That is what `backend_source` does — it connects from each local address
  in turn, keeps a session on one address (control and data connections must share the client
  IP), and limits each address to the nine transfers the backend can serve it, making extra
  uploads wait briefly (and then get `425`) instead of failing at the backend. See
  [Backend source rotation](#backend-source-rotation) for the mechanics.

### FTPS requirements

Transfer Family's FTPS endpoints require the data channel to be protected (`PROT P`; `PROT C` is
not supported), and by default enforce **TLS session resumption** on data connections: a data
connection that doesn't resume the control connection's TLS session is refused with
`522 data connection must use cached TLS session`
([ProtocolDetails](https://docs.aws.amazon.com/transfer/latest/userguide/API_ProtocolDetails.html)).
That is the case [`backend_tls`](#backend-ftps) is built for: the gateway negotiates
`AUTH TLS` / `PBSZ 0` / `PROT P` itself, and each session's control and data connections share a
TLS resumption store of their own so that data connections resume that session's control
connection. If your endpoint presents a certificate for a
custom hostname rather than the one you connect to, set `backend_tls.server_name`; use
`ca_file` if it is signed by a private CA. If resumption fails against your endpoint with TLS 1.3,
try `max_version: "1.2"`.

### Simplest setup: one ENI, several private IPs

You do not need extra ENIs. Every private IPv4 address on one ENI is a separate source, used in
ascending order of address: new sessions take the current one, which moves on to the next address
after as many data connections as the backend's data port range holds (9 for Transfer Family) and
wraps around after the last. Sessions already running stay on the address they started with.
Packets sent from a secondary IP leave through the same ENI as the primary address, so no extra
routing is needed.

1. **Assign the secondary private IPs** to the ENI — in the console, or for example
   `aws ec2 assign-private-ip-addresses --network-interface-id eni-… --secondary-private-ip-address-count 3`.
   How many IPs an ENI can hold depends on the instance type.
2. **Make sure the OS has them configured.** AWS assigns an address to the ENI, but the instance
   only uses it once its operating system has it configured. Some images do that automatically;
   others need it done by hand or by their network configuration. Check that every address you
   assigned is listed:

   ```sh
   ip -4 addr show dev ens5
   ```

   An address that is assigned but not configured is invisible to the gateway and is simply not
   used.
3. **Run the gateway with `network_mode: host` and `GATEWAY_BACKEND_SOURCE=rotate`.**
   Auto-detection picks the addresses up (see the note under step 5 below);
   `GATEWAY_BACKEND_SOURCE_INCLUDE=ens5` pins the choice to that interface, including any
   addresses the OS labels as aliases (`ens5:1`). An address assigned later is picked up within
   `refresh_secs`.
4. **Check the startup log**: `backend source address rotation enabled` lists the addresses the
   gateway chose — it should be every address from step 1, plus the primary one.

With the primary address and three secondary ones, that is four sources and about 4 × 9 = 36
concurrent uploads.

### Several network interfaces on the EC2 instance

To give the gateway several source addresses on EC2 with extra network interfaces (ENIs) instead
— or in addition — attach them to the instance and run the container with `network_mode: host`,
so it sees every address and can bind its outgoing connections to each:

```mermaid
flowchart LR
    devices["IoT devices<br/>plain FTP"] --> gw

    subgraph ec2["EC2 instance — docker, network_mode: host"]
        gw["iot-ftp-upload-gateway<br/>backend_tls + backend_source"]
        eni1["ENI 1 (primary)<br/>10.0.1.10"]
        eni2["ENI 2<br/>10.0.2.20"]
        eni3["ENI 3<br/>10.0.3.30"]
        gw --> eni1
        gw --> eni2
        gw --> eni3
    end

    subgraph vpc["VPC"]
        tf["AWS Transfer Family endpoint<br/>control :21, data :8192-8200"]
    end

    eni1 -- "Explicit FTPS, source 10.0.1.10" --> tf
    eni2 -- "Explicit FTPS, source 10.0.2.20" --> tf
    eni3 -- "Explicit FTPS, source 10.0.3.30" --> tf
```

Here three source addresses give about 3 × 9 = 27 concurrent uploads, instead of 9. Setting it
up:

1. **Attach the interfaces.** The number of ENIs and of IPs per ENI an instance can have depends
   on its instance type. Each address becomes one source; a secondary IP on an existing ENI works
   the same way as an extra ENI (see [Simplest setup](#simplest-setup-one-eni-several-private-ips)).
2. **Route each extra ENI's traffic out of that ENI.** Linux sends everything out of the default
   route's interface, and AWS drops a packet whose source address doesn't belong to the ENI it
   leaves through, so each additional ENI needs source-based routing (see the example under
   [Backend source rotation](#backend-source-rotation)). Secondary IPs on the primary ENI
   normally don't.
3. **Keep NAT and load balancers out of the path.** Transfer Family has to see each source
   address as the client IP; behind a NAT every connection looks like it comes from one address,
   and the extra capacity disappears. AWS recommends the same for FTP/FTPS (see
   [Endpoint placement](#endpoint-placement-no-nlb-no-nat)).
4. **Open the endpoint's security group** for port 21 and 8192–8200 from every source address
   (or their subnets), in addition to whatever you allow for devices to reach the gateway itself.
5. **Configure the gateway.** Interface names are normally **not** needed: with
   `GATEWAY_BACKEND_SOURCE=rotate` alone, auto-detection uses every IPv4 address on the host
   except loopback, link-local, and virtual interfaces (`lo`, `docker*`, `br-*`, `veth*`, ...),
   which on an EC2 host running Docker leaves the instance's own ENI addresses (`ens5`, `ens6`,
   ... on current instance types). New ENIs are picked up automatically as well (re-listed every
   30 seconds).

   ```yaml
   services:
     gateway:
       image: ghcr.io/higanworks/iot-ftp-upload-gateway:latest # pin a version tag in production
       network_mode: host
       environment:
         GATEWAY_BACKENDS: "s-0123456789abcdef0.server.transfer.us-east-1.amazonaws.com:21@8192:8200"
         GATEWAY_PASSIVE_ADDRESS: "<address devices can reach>"
         GATEWAY_BACKEND_TLS: "explicit"
         GATEWAY_BACKEND_SOURCE: "rotate"
         # Optional -- only to pin the interfaces instead of auto-detecting them:
         # GATEWAY_BACKEND_SOURCE_INCLUDE: "ens5,ens6,ens7"
   ```

   Pin them with `GATEWAY_BACKEND_SOURCE_INCLUDE` (wildcards such as `ens*` work) when the host
   also has real interfaces that must not be used toward the backend — a VPN, a management
   network, an ENI that has no routing to the endpoint — or when you want the set of sources to
   stay fixed regardless of what is attached later. `GATEWAY_BACKEND_SOURCE_EXCLUDE` removes
   individual interfaces from the auto-detected set instead. Either way, the startup log shows
   which addresses were chosen (step 6).

6. **Check it.** At startup the log lists the chosen source addresses (`backend source address
   rotation enabled`); during use, each session's log lines carry a `source_ip` field. A source
   that can't connect is logged at `WARN` and skipped for a minute. Verify with a load test that
   concurrent uploads really exceed nine, since the per-client-IP behavior of your endpoint is
   the assumption all of this rests on.

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

**Timeouts toward the backend.** Every wait on a backend is bounded, so one that stops
responding cannot hold a session — and the PASV port and source-address slot it has claimed —
forever: connecting takes at most `timeouts.connection_timeout_secs`, each reply (including the
reply to the gateway's own `PASV`) at most `timeouts.command_timeout_secs`, and an idle data
transfer at most `timeouts.data_idle_timeout_secs`. A backend `PASV` reply that never arrives, is
cut off, or is longer than `limits.max_command_line_bytes` ends the session with `421`: the
connection can no longer be trusted to be in step, since a reply that turned up late would be
read as the answer to the next command. (A reply that arrives but isn't a PASV address, or a
data port that can't be reached, only fails that transfer with `425`.)

**Several gateway instances behind a load balancer.** The PASV reply tells the client where to
open its data connection, and that connection must reach the *same instance* that holds the
control connection. Each instance therefore has to advertise an address that leads to that
instance itself (`passive.address`), not the load balancer's shared address: a balancer may send
the data connection to a different instance, which either has nothing listening on that port or
— worse — has *another client's* PASV listener on the same port number. `require_data_ip_match`
refuses such a mix-up when the two clients have different IPs, but it cannot tell two clients
behind one NAT apart, so do not rely on it for this. A single gateway behind an NLB, or several
reached through per-instance addresses (see DNS round-robin below), has no such problem.

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
| `ftp_gateway_pasv_ports_capacity` | gauge | PASV/EPSV data ports in the configured range (capacity − active is what is left) |
| `ftp_gateway_sessions_total` | counter | Total control connections accepted |
| `ftp_gateway_upload_bytes_total` | counter | Total bytes relayed to a Backend across all completed `STOR` transfers |
| `ftp_gateway_connections_rejected_total` | counter | Total connections rejected by the per-IP connection limit |
| `ftp_gateway_uploads_started_total` | counter | `STOR` transfers that reached the Backend (both data connections up) |
| `ftp_gateway_uploads_completed_total` | counter | Started transfers the Backend confirmed with a `2xx` reply |
| `ftp_gateway_uploads_failed_total` | counter | Started transfers that did not complete, however they ended |
| `ftp_gateway_backend_pasv_failures_total` | counter | Times the Backend gave no usable reply to `PASV` (none in time, cut off, too long, or not an address) |
| `ftp_gateway_backend_data_connection_failures_total` | counter | Failures to establish a data connection to the Backend (TCP connect, or the TLS handshake with `backend_tls`) |
| `ftp_gateway_data_connections_rejected_total` | counter | Client data connections closed for coming from a different IP than the control connection |
| `ftp_gateway_backend_timeouts_total` | counter | Timeouts waiting on, or connecting to, a Backend |
| `ftp_gateway_session_idle_timeouts_total` | counter | Control sessions closed for being idle too long |

With [source rotation](#backend-source-rotation) on, there are also per-source-address series,
which show whether a source has reached the backend's concurrency limit
(`transfers_active` against `transfers_capacity`, and `slot_timeouts_total` counting the uploads
answered `425` for lack of a slot):

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `ftp_gateway_backend_source_unhealthy` | gauge | `source` | `1` while the source address is left out of the rotation after failing to connect |
| `ftp_gateway_backend_source_transfers_active` | gauge | `backend`, `source` | Data transfers in flight from this source to this backend |
| `ftp_gateway_backend_source_transfers_capacity` | gauge | `backend`, `source` | Most transfers allowed at once (the backend's `passive_ports` size) |
| `ftp_gateway_backend_source_transfers_total` | counter | `backend`, `source` | Transfers started from this source to this backend |
| `ftp_gateway_backend_source_slot_timeouts_total` | counter | `backend`, `source` | Transfers that gave up waiting for a free slot |

The number of these series is fixed by the number of source addresses and backends — nothing a
client sends can add one.
Every `_total` counter uses saturating addition, so it holds at `u64::MAX` under sustained load
instead of wrapping back to a small number.

By default `metrics.address` / `GATEWAY_METRICS_ADDRESS` binds to loopback (`127.0.0.1`) only —
this endpoint carries no per-client data (no filenames, client IPs, or credentials) — with source
rotation on it does show the gateway's own source addresses and the backends' `host:port` — but
it also isn't meant to be reachable straight from the Internet by default. A sidecar/same-pod Prometheus
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
