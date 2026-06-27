# pzrelay

Promtuz relay node — a Kademlia DHT peer that authenticates clients, replicates
presence + MLS handshake material (KeyPackages, Welcomes) across the network,
and store-and-forwards ciphertext. Part of [promtuz](../README.md); relays see
only ciphertext, never message contents.

## Install (Debian / Ubuntu)

One line — adds the apt repo and installs `pzrelay`:

```sh
curl -fsSL https://apt.promtuz.dev/install.sh | sudo sh
```

### Manual (if you'd rather not pipe a script to a shell)

```sh
sudo install -d -m 0755 /etc/apt/keyrings
sudo curl -fsSL https://apt.promtuz.dev/promtuz-archive-keyring.asc \
     -o /etc/apt/keyrings/promtuz.asc

echo "deb [signed-by=/etc/apt/keyrings/promtuz.asc] https://apt.promtuz.dev edge main" \
  | sudo tee /etc/apt/sources.list.d/promtuz.list

sudo apt update && sudo apt install pzrelay
```

`signed-by` pins this repo to our key only — no other key can authorize these
packages on your system.

## Configure

Edit `/etc/promtuz/relay.toml` — set the resolver seed key and your box's
public address. Edits survive `apt upgrade` (it's a dpkg conffile). Log
verbosity is `[log] level` (or the `PZ_LOG` env): `trace|debug|info|warn|error`.

## Enroll (mint the cert)

A relay is permissioned: it needs a cert signed by the Promtuz RootCA before it
can serve. The flow is self-service and the private key never leaves the box:

1. Start the relay (below). On first boot it generates its single Ed25519 key
   (`/etc/promtuz/keys/relay.key` — identity **and** TLS), writes a CSR to
   `/etc/promtuz/relay.csr`, logs what to do, and **waits** (no crash-loop).
2. Copy `relay.csr` to your CA box and sign it: `certgen sign relay.csr` →
   `relay.crt`.
3. Drop the signed cert at `/etc/promtuz/certs/relay.crt`. The relay is watching
   — it picks the cert up, deletes the CSR, and starts serving.

The cert certifies the relay's identity key, so `CN = relay_id`.

## Run

```sh
sudo systemctl enable --now pzrelay
systemctl status pzrelay
journalctl -u pzrelay -f
```

## Update

```sh
sudo apt update && sudo apt upgrade     # config preserved
```

## Channel

Single `edge` channel for now — the project is pre-production. A vetted
`stable` channel gets added once releases settle down.

## Paths

| What | Where |
|------|-------|
| binary | `/usr/bin/pzrelay` |
| config | `/etc/promtuz/relay.toml` (conffile) |
| RootCA | `/etc/promtuz/ca.pem` |
| key | `/etc/promtuz/keys/relay.key` (identity + TLS, 0600) |
| cert | `/etc/promtuz/certs/relay.crt` |
| database | `/var/lib/pzrelay/` (RocksDB) |
| logs | `journalctl -u pzrelay` |
| version | `pzrelay --version` |

Runs as the unprivileged `pzrelay` system user under a hardened systemd unit.

## Build from source

Requires Rust, [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild),
`cargo-deb`, and `zig` on `PATH`. The `.deb` is built against an old glibc so it
runs on Debian 10+ / Ubuntu 18.04+:

```sh
./scripts/build-deb.sh relay
# → target/x86_64-unknown-linux-gnu/debian/pzrelay_<version>_amd64.deb
```

Plain `cargo deb` is **not** supported — it rebuilds against the host glibc and
the package's pinned `libc6` dependency would be wrong. Always use the script.
