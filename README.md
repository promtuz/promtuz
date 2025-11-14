
# Resolver

Small QUIC service that keeps track of online relay nodes.  
Relays connect to the resolver, announce themselves, and send periodic
heartbeats. The resolver stores the active set in memory and can return
it to clients or other components.

This repo contains only the resolver side of the system.

---

## Running

```

cargo run --release

````

The resolver binds to the address defined in `config.toml` and expects a
TLS certificate + key to exist at the configured paths.

---

## Configuration

Everything is controlled through `config.toml` next to the binary.

Example:

```toml
[network]
address = "0.0.0.0:4433"
cert_path = "cert/server.crt"
key_path = "cert/server.key"

[resolver]
seed = []      # optional: list of other resolvers for gossip
````

* `address` – QUIC bind address
* `cert_path` / `key_path` – TLS certificate (Rustls)
* `seed` – optional list of other resolvers; if present, this instance
  will try to link with them at startup

---

## What It Does

* Accepts QUIC connections
* Determines the peer type via ALPN
* Handles the `RelayHello → HelloAck` handshake
* Tracks connected relays in memory
* Receives heartbeats from relays
* Optionally syncs with other resolvers
* Exposes the current relay list to clients (WIP)

This repo does **not** deal with message queues, user data, or storage.
It is strictly presence + discovery.

---

## Development Notes

* QUIC streams are all unidirectional for control messages
* Handshake and heartbeat are intentionally minimal
* Registry is in-memory; persistence may be added later
* This repo is for internal tooling only, not distribution

---

## TLS

The resolver expects real certificate files.
For local testing:

```
cert/server.crt
cert/server.key
```

You can self-sign; the relays use certificate pinning.

---

## Status

The resolver is functional enough for relay discovery and liveness
tracking. Additional features (mesh gossip, client querying, metrics)
are added as needed.