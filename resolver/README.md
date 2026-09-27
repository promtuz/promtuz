# pzresolver

Promtuz resolver — a stateless relay discovery service. Relays register here so
clients can find them; it keeps only an in-memory map of which relays are online
and owns no message data. Part of [promtuz](../README.md).

Resolvers are run by the operator (you), not volunteers, so deployment is
deliberate and manual.

## Install (Debian / Ubuntu)

Add the Promtuz apt repo (the same one relays use), then install:

```sh
sudo install -d -m 0755 /etc/apt/keyrings
sudo curl -fsSL https://apt.promtuz.dev/promtuz-archive-keyring.asc \
     -o /etc/apt/keyrings/promtuz.asc

echo "deb [signed-by=/etc/apt/keyrings/promtuz.asc] https://apt.promtuz.dev edge main" \
  | sudo tee /etc/apt/sources.list.d/promtuz.list

sudo apt update && sudo apt install pzresolver
```

## Enroll + run

Same self-service flow as the relay, and the private key never leaves the box.
On first boot the resolver generates its single Ed25519 key
(`/etc/promtuz/keys/resolver.key` — its pubkey is the resolver IPK relays seed),
writes `/etc/promtuz/resolver.csr`, and waits.

```sh
sudo systemctl enable --now pzresolver                  # generates key, writes CSR, waits
# on your CA box:
certgen sign resolver.csr
# back on the resolver box:
sudo cp resolver.crt /etc/promtuz/certs/resolver.crt    # it starts automatically
journalctl -u pzresolver -f
```

Check the bind address in `/etc/promtuz/resolver.toml` (a dpkg conffile).

To let current phones discover relays when UDP is blocked, enable
`tcp_fallback = true` under `[network]`, open TCP on the same configured port
(normally 40433), and restart `pzresolver`. The existing certificate and key
serve the TLS listener. Relay and gateway registration still uses UDP.
Existing configs default to disabled; the packaged example enables it.
See the [relay fallback guide](../relay/README.md#tcp-fallback-for-phones)
for the corresponding relay and gateway settings and compatibility limits.

## Update

```sh
sudo apt update && sudo apt upgrade     # config preserved
```

## Paths

| What | Where |
|------|-------|
| binary | `/usr/bin/pzresolver` |
| config | `/etc/promtuz/resolver.toml` (conffile) |
| RootCA | `/etc/promtuz/ca.pem` |
| key | `/etc/promtuz/keys/resolver.key` (identity + TLS, 0600) |
| cert | `/etc/promtuz/certs/resolver.crt` |
| logs | `journalctl -u pzresolver` |
| version | `pzresolver --version` |

Stateless — no database, no `/var/lib`. Runs as the unprivileged `pzresolver`
user under a hardened systemd unit.

## Build from source

```sh
./tools/scripts/build-deb.sh resolver
# → target/x86_64-unknown-linux-gnu/debian/pzresolver_<version>_amd64.deb
```
