# tcode-traverse

One binary that is a complete Traverse instance for Tcode: an
[iroh](https://iroh.computer) relay with QUIC address discovery, a pkarr
store for endpoint lookup, and the `relays.json` manifest a Tcode machine
reads to use it. It is the same server for any operator; there is nothing
that only an official instance can do.

## Run

```sh
tcode-traverse --print-default-config > traverse.toml   # edit hostname, contact, data_dir
tcode-traverse --config traverse.toml
```

Ports, with the defaults:

| Port | Protocol | Purpose |
| --- | --- | --- |
| 443 | TCP, HTTPS | relay (`/relay` WebSocket), `/pkarr/<key>`, `/relays.json`, `/healthz` |
| 80 | TCP, HTTP | the same routes without the relay; everything with `tls.mode = "off"` |
| 7842 | UDP, QUIC | address discovery (skipped when `tls.mode = "off"`) |
| 9090 | TCP, loopback | Prometheus metrics (`[metrics] bind`; absent means off) |

A Tcode machine needs public TCP 443 and UDP 7842 on the instance plus a
domain and certificate. The manifest advertises the QUIC port whenever
`relay.quic_addr_discovery` is on and TLS is terminated here, and the
server does not probe whether that port is reachable: either expose the
advertised UDP port or set `relay.quic_addr_discovery = false`, which
advertises `"quic_port": 0` so devices skip address discovery through this
relay. `tls.mode = "off"` also turns address discovery off.

`tcode-traverse --dev` serves plain HTTP on `127.0.0.1:8080` with the store in
`./tcode-traverse-dev`: no TLS, no QUIC address discovery, no metrics.
`RUST_LOG` controls logging (`info` by default).

## TLS

`[tls] mode`:

- `letsencrypt` — ACME over TLS-ALPN-01 on the `tls.bind` port; needs
  `hostname` and `contact`. Certificates are cached in `<data_dir>/acme`.
  `acme_directory` points at staging or a local ACME server.
- `manual` — `cert` and `key` are PEM files read once at start.
- `reloading` — like `manual`, re-read once a day, for certificates renewed by
  another tool.
- `off` — plain HTTP on `http.bind`. Use it behind a reverse proxy that
  terminates TLS and forwards WebSocket upgrades (`hostname` still names the
  public `https://` URL), or for development. QUIC address discovery is
  unavailable in this mode.

The manifest URL carries the `tls.bind` port unless it is 443. With `manual`
or `reloading` behind a proxy that owns 443 and forwards TLS to `tls.bind`
(so the instance still holds the certificate, and QUIC address discovery
works), set `tls.public_port = 443`.

## Manifest

`GET /relays.json` describes the instance to clients:

```json
{
  "version": 1,
  "updatedAt": "2026-09-21T00:00:00Z",
  "relays": [
    { "url": "https://traverse.example.org/", "quic_port": 7842, "region": "eu-central" }
  ],
  "pkarr": ["https://traverse.example.org/pkarr"]
}
```

`updatedAt` is the config file's modification time. `[[peers]]` entries are
appended to `relays`.

## pkarr store

`PUT /pkarr/<z32 endpoint id>` stores a signed packet (signature verified,
at most 1072 bytes, `409` when a newer packet is stored, `400` when invalid,
`413` when oversized, `429` above `pkarr.put_per_second`/`put_burst` per IP).
`GET` returns it with `Cache-Control: public, max-age=300`, `404` when
unknown, `429` above `pkarr.get_per_second`/`get_burst` per IP. `/<key>` at
the root is accepted too. Records not refreshed within `pkarr.eviction` are
removed. Behind a reverse proxy set `[http] trust_forwarded_for = true` so
the limits count the client, not the proxy.

## Limits

| Setting | Default | Scope |
| --- | --- | --- |
| `pkarr.put_per_second` / `put_burst` | 4 / 8 | `PUT /pkarr/<key>`, per client IP |
| `pkarr.get_per_second` / `get_burst` | 20 / 40 | `GET /pkarr/<key>` and `GET /<key>`, per client IP |
| `relay.rx_bytes_per_second` / `rx_max_burst_bytes` | 2 000 000 / 4 000 000 | bytes received per relay connection; `0` disables |
| `lock.far_connection_quota` | 64 | concurrent far connections with the region lock on |

## Region lock

An instance can keep its relay for the clients that are close to it. With
`[lock] enabled = true` a relay connection is admitted when its TCP RTT is at
most `home_rtt_max_ms`, or its address is in `allow_cidrs`; every other client
shares `far_connection_quota` concurrent connections and is refused once that
is used up.

iroh-relay only hands the access hook the HTTP upgrade request, not the
socket, so RTT and address are read from headers a reverse proxy adds:
`X-TCP-RTT` in microseconds and `X-Forwarded-For`. A client can send those
headers itself, so they are read only with `trust_proxy_headers = true`,
which is meaningful only behind a reverse proxy that overwrites both.
Without it every client counts as far and only the quota applies; a config
with the lock on, the headers untrusted and a quota of `0` is rejected at
start. With nginx in front of `tls.mode = "off"` and
`[http] bind = "127.0.0.1:8080"`:

```nginx
location /relay {
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_set_header X-Forwarded-For $remote_addr;
    proxy_set_header X-TCP-RTT $tcpinfo_rtt;
    proxy_read_timeout 1h;
}
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_set_header X-Forwarded-For $remote_addr;
}
```

## Metrics

`[metrics] bind` serves OpenMetrics text: iroh-relay's `relayserver_*`
counters (a second copy labelled `component="qad"` for the QUIC socket) and
`pkarr_*` (`puts`, `put_rejected`, `put_rate_limited`, `gets`,
`get_missing`, `evicted`). Keep it on a private address.

## Pointing a Tcode machine at it

Desktop: **Settings → Remote → Traverse → Add self-hosted Traverse**, then
the base URL, `https://traverse.example.org`; switch **Official Traverse**
off to publish to your instance alone. Headless:
`tcode-headless serve --traverse https://traverse.example.org`, adding
`--traverse official` (or further `--traverse <url>`) to publish to several
instances at once. Devices take the URL from the machine's invitation, so
nothing is configured on a phone.
The base URL may be `http` for development; the relay and pkarr URLs in the
manifest must be `https`. A `--dev` instance is for the tests in this crate,
not for a machine.

## Docker

Releases publish `ghcr.io/tryanks/tcode-traverse:<version>` and `:latest` for
Linux amd64 and arm64, using the same binaries as the release archives.
The runtime is `debian:trixie-slim`, exposing 80, 443 and 7842/udp. Mount the config at
`/etc/tcode-traverse/traverse.toml` and the data directory at
`/var/lib/tcode-traverse`:

```sh
docker run -p 80:80 -p 443:443 -p 7842:7842/udp \
  -v "$PWD/traverse.toml:/etc/tcode-traverse/traverse.toml:ro" \
  -v tcode-traverse-data:/var/lib/tcode-traverse ghcr.io/tryanks/tcode-traverse:latest
```

[`docker-compose.yml`](docker-compose.yml) is the same deployment as a
Compose project, with a health check on `/healthz`.

For a local source build, run
`docker build -f crates/traverse-server/Dockerfile -t tcode-traverse .`.
Release builds pass `--build-arg BINARY_SOURCE=release`, with the binary
staged at `dist/<amd64|arm64>/tcode-traverse`.
