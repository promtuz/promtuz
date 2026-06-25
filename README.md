# Promtuz

A decentralized, end-to-end encrypted messenger built from scratch in Rust and QUIC. Android client in Kotlin/Compose.

This is a personal project — not trying to compete with Signal or Telegram, just wanted to understand what it takes to build one of these from the ground up. No central server owns your messages, no phone number required, identity is just a keypair.

## How it works

There are no "servers" in the traditional sense. The network has two lightweight infrastructure roles, and everything that carries your messages is replicated, ephemeral, and replaceable.

**Resolver** — A stateless directory service. Relays register themselves here so clients can discover them. No database, just an in-memory map of which relays are currently online. If it dies, relays reconnect to another one.

**Relay** — A node in a Kademlia-style DHT. When a client connects and authenticates, its relay publishes a *presence record* ("this user is reachable through me") and replicates it across the DHT. Relays also store-and-forward the MLS handshake material (KeyPackages, Welcomes) and queued ciphertext for users who are briefly offline. Relays are stateless by design — they can crash, move hosts, or get replaced, and the DHT heals around them.

**Client (libcore)** — The core library, written in Rust, compiled to a native `.so` for Android via JNI. Handles identity, the MLS group state, relay discovery, and message delivery. The Android app is a thin UI layer on top of this.

The general flow: a client asks a resolver for available relays, connects to one, and authenticates with its Ed25519 identity key via challenge-response. To message someone, you look them up in the DHT to find which relay they're homed on, fetch their published KeyPackage, and drive an MLS group; ciphertext is routed to the recipient's home relay and delivered (or queued until they reconnect).

## Crypto

- **Identity** — an Ed25519 keypair, nothing more. On Android the private key is wrapped by the Android Keystore (AES-256-GCM) and only unwrapped momentarily for signing, then zeroized. A node's address (`NodeId`) is `BLAKE3(pubkey)`.
- **Messaging** — [MLS (RFC 9420)](https://www.rfc-editor.org/rfc/rfc9420) via [openmls](https://github.com/openmls/openmls). This gives forward secrecy and post-compromise security per epoch, and is group-native rather than bolted on. Relays only ever see ciphertext and signed handshake objects.
- **Transport** — QUIC with TLS 1.3 (rustls + aws-lc-rs), split across two trust domains:
  - *CA-hierarchical* (`client` / `relay` / `resolver` ALPNs): leaf certs signed by a root CA, verified the usual way with the `NodeId` as the SNI hostname.
  - *Key-as-identity* (`peer` ALPN): self-signed Ed25519 certs with no CA, pinned by SPKI to the `NodeId` the dialer expected. This is how relays dial each other for DHT RPC — trust is the key itself, not an issuing authority.
- **Misc** — HKDF-SHA256 for signature/transport domain separation, BLAKE3 for hashing and the DHT XOR metric, Postcard with length-prefixed framing on the wire, CBOR for Rust↔Kotlin events.

## Project structure

```
common/     Shared crate — crypto, wire protocol, QUIC config, identity, DHT/MLS message types
relay/      Relay node — DHT, client auth, presence + KeyPackage/Welcome replication, store-and-forward
resolver/   Resolver — relay discovery service
libcore/    Client library — MLS engine + networking, compiled to .so for Android via JNI
jni_macro/  Proc-macro helpers for the JNI bindings
testnet/    End-to-end harness — spins up a real resolver + N relays + clients as subprocesses
android/    Android app — Kotlin, Jetpack Compose, Material 3
```

## What works

- Identity generation with hardware-backed key storage
- Resolver discovery and relay connection with auto-reconnect
- Challenge-response authentication against relays
- A Kademlia DHT between relays: routing table, iterative `FindNode`/`FindValue`, presence publication, and Merkle-tree anti-entropy replication so records survive relay churn
- MLS group messaging: KeyPackage publication, Welcome delivery, and application messages
- **End-to-end message delivery** — a 1:1 message crossing two independent relays, exercised by the `testnet` harness over real QUIC/TLS and validated cross-continent over the public internet
- P2P identity exchange via QR codes (custom binary format)
- Event system for async Rust-to-Kotlin communication

## What doesn't (yet)

- **Resolver mesh** — there's still only one resolver; multiple resolvers don't sync with each other
- **NAT traversal** — a relay behind NAT can dial out but can't be dialed back, so cross-relay links only form to publicly-reachable peers (no STUN/TURN/hole-punching yet)
- Full Android integration of the new message path — the protocol is proven end-to-end via the harness; wiring it through the app UI is ongoing
- Multi-device / message history sync

The hard parts — networking, identity, the DHT, MLS, real end-to-end delivery — are in place and tested. What remains is mostly hardening and surfacing it in the app.

## Building

The relay and resolver are standard Rust binaries (`cargo run -p relay` / `-p resolver`). The client library cross-compiles to Android targets with `cargo-ndk`, and the Android app builds it automatically via a Gradle task.

The relay/resolver infrastructure needs a root CA and node certificates — the `common` crate ships a `certgen` binary for that.

To see the whole thing run on one machine, use the `testnet` harness: it stands up a resolver and several relays as real subprocesses on loopback, signs them under a CA, and drives clients through the full MLS stack to assert a message crosses the network.

## License

[AGPL-3.0-or-later](https://www.gnu.org/licenses/agpl-3.0.en.html).
