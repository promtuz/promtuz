# Promtuz

A decentralized, end-to-end encrypted messaging app built from scratch with Rust and QUIC. Android client in Kotlin/Compose.

This is a personal project — not trying to compete with Signal or Telegram, just wanted to understand what it takes to build one of these from the ground up. No central server owns your messages, no phone number required, identity is just a keypair.

## How it works

There are no "servers" in the traditional sense. The network has two lightweight infrastructure roles and everything else is peer-to-peer.

**Resolver** — A stateless directory service. Relays register themselves here so clients can discover them. No database, just an in-memory map of who's currently online. If it dies, relays reconnect to another one.

**Relay** — A stateless worker node in a Kademlia-style DHT. When a client connects and authenticates, the relay publishes a presence record ("this user is reachable through me") and replicates it across the DHT. Relays are ephemeral by design — they can crash, move hosts, or get replaced without breaking anything.

**Client (libcore)** — The core library, written in Rust, compiled to a native `.so` for Android via JNI. Handles identity, encryption, relay discovery, and peer-to-peer connections. The Android app is a thin UI layer on top of this.

The general flow: client asks a resolver for available relays, connects to one, authenticates with its Ed25519 identity key via challenge-response, and gets registered in the DHT. To reach another user, you look them up in the DHT to find which relay they're connected through, then communicate.

## Crypto

- **Identity**: Ed25519 keypair. Private key encrypted with Android Keystore (AES-256-GCM), only decrypted momentarily during signing operations, then zeroized.
- **Transport**: QUIC with TLS 1.3. Relay connections use P256 certificates signed by a root CA. Peer-to-peer connections use certificates derived from identity keys.
- **Encryption**: ChaCha20-Poly1305 for message encryption, X25519 for key agreement, HKDF for key derivation, BLAKE3 for hashing.
- **Serialization**: Postcard with length-prefixed framing for wire protocol, CBOR for Rust-to-Kotlin events.

## Project structure

```
common/     Shared crate — crypto, protocol definitions, QUIC config, identity system
relay/      Relay server — DHT node, client auth, presence replication
resolver/   Resolver server — relay discovery service
libcore/    Client library — compiled to .so for Android via JNI
android/    Android app — Kotlin, Jetpack Compose, Material 3
```

## What works

- Identity generation with hardware-backed key storage
- Resolver discovery and relay connection with auto-reconnect
- Challenge-response authentication against relays
- P2P identity exchange via QR codes (custom binary format)
- Kademlia routing table with XOR distance metric
- Network statistics and connection state monitoring in the app
- Event system for async Rust-to-Kotlin communication

## What doesn't (yet)

- Actually sending and receiving messages — the foundation is there but the message protocol isn't implemented
- DHT peer-to-peer RPC is partially stubbed out
- Contact storage and message persistence
- Resolver mesh (multiple resolvers don't sync with each other)

The project got about 60% of the way to a working MVP. The hard parts (networking, identity, crypto, P2P connectivity) are largely done. The "easy" part (actual messaging on top of all that) is what's missing.

## Building

The relay and resolver are standard Rust binaries. The client library cross-compiles to Android targets using `cargo-ndk`. The Android app builds the Rust library automatically via a Gradle task before compilation.

Requires a root CA and node certificates for the relay/resolver infrastructure (the `common` crate includes a `certgen` binary for this).

## License

None specified yet.
