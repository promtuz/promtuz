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

Edit `/etc/pzrelay/relay.toml` — set the resolver seed key and your box's
public address. Edits survive `apt upgrade` (it's a dpkg conffile).

## Provision identity (enrollment)

A relay is permissioned: it needs a cert signed by the Promtuz RootCA before it
can serve. Today that material is obtained out-of-band and placed in
`/var/lib/pzrelay/` as `node.crt` + `node.key` (automated enrollment is in
progress). The Ed25519 identity key auto-generates on first start if absent.

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
| config | `/etc/pzrelay/relay.toml` (conffile) |
| RootCA | `/etc/pzrelay/ca.pem` |
| data + keys | `/var/lib/pzrelay/` (RocksDB, identity/TLS keys) |
| logs | `journalctl -u pzrelay` |
| version | `pzrelay --version` |

Runs as the unprivileged `pzrelay` system user under a hardened systemd unit.

## Build from source

Requires Rust, [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild),
`cargo-deb`, and `zig` on `PATH`. The `.deb` is built against an old glibc so it
runs on Debian 10+ / Ubuntu 18.04+:

```sh
./relay/pkg/build-deb.sh
# → target/x86_64-unknown-linux-gnu/debian/pzrelay_<version>_amd64.deb
```

Plain `cargo deb` is **not** supported — it rebuilds against the host glibc and
the package's pinned `libc6` dependency would be wrong. Always use the script.
