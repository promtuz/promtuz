# Promtuz

A decentralized, end-to-end encrypted messenger built from scratch in Rust and QUIC. Android client in Kotlin/Compose.

This is a personal project, not an attempt to compete with Signal or Telegram. I just wanted to understand what it takes to build one of these from the ground up. No central server owns your messages, no phone number required, identity is just a keypair.

## How it works

There are no "servers" in the traditional sense. The network has three lightweight infrastructure roles, and everything that carries your messages is replicated, ephemeral, and replaceable.

**Resolver**: a stateless directory service. Relays and gateways register themselves here so the rest of the network can find them. No database, just an in-memory map of which nodes are currently online. If it dies, they reconnect to another one.

**Relay**: a node in a Kademlia-style DHT. When a client connects and authenticates, its relay publishes a *presence record* ("this user is reachable through me") and replicates it across the DHT. Relays also store-and-forward the MLS handshake material (KeyPackages, Welcomes) and queued ciphertext for users who are briefly offline, and they can lend their already-open QUIC port to hole-punch assist (a STUN echo plus a blind TURN bridge), which each operator switches on per relay. Relays are stateless by design: they can crash, move hosts, or get replaced, and the DHT heals around them.

**Gateway**: optional push infrastructure, and the only piece that speaks to a platform vendor. A device mints a random per-install pseudonym `P`, tells its home relay `IPK → P`, and separately tells a gateway `P → device token`. When a message queues for someone offline, their relay asks the gateway to wake `P`. The relay never learns a device token, the gateway never learns an identity, and a dropped wake costs nothing because the message is already durably queued.

**Client (libcore)**: the core library, written in Rust and compiled to a native `.so` for Android via uniffi. Handles identity, the MLS group state, relay discovery, message delivery, media encoding, and the direct peer transport. The Android app is a thin UI layer on top of this.

The general flow: a client asks a resolver for available relays, connects to one, and authenticates with its Ed25519 identity key via challenge-response. To message someone, you look them up in the DHT to find which relay they're homed on, fetch their published KeyPackage, and drive an MLS group; ciphertext is routed to the recipient's home relay and delivered (or queued until they reconnect).

Paired contacts can also leave the relay behind. A direct link opens through the relay's TURN bridge first, so it is usable in about one round trip, while a hole punch runs in the background; once a direct path validates, the same QUIC connection swaps its egress to raw UDP and the bytes go device to device. That path is what carries attachments too big to inline.

## Crypto

- **Identity**: an Ed25519 keypair, nothing more. On Android the private key is wrapped by the Android Keystore (AES-256-GCM) and only unwrapped momentarily for signing, then zeroized. A node's address (`NodeId`) is `BLAKE3(pubkey)`.
- **Messaging**: [MLS (RFC 9420)](https://www.rfc-editor.org/rfc/rfc9420) via [openmls](https://github.com/openmls/openmls). This gives forward secrecy per message, and is group-native rather than bolted on. Epochs advance when a member is added; nothing issues a periodic key update, so post-compromise recovery is available in the protocol but not driven yet. Relays only ever see ciphertext and signed handshake objects.
- **Transport**: QUIC with TLS 1.3 (rustls + aws-lc-rs), split across two trust domains:
  - *CA-hierarchical* (`client` / `relay` / `resolver` ALPNs): leaf certs signed by a root CA, verified the usual way with the `NodeId` as the SNI hostname. The CA also stamps a capability bitset (push gateway, blob store, and so on) into a custom extension inside the leaf, so what a node is allowed to offer is attested by the same signature that proves who it is, and cannot be self-asserted.
  - *Key-as-identity* (`peer` ALPN): self-signed Ed25519 certs with no CA, pinned by SPKI to the `NodeId` the dialer expected. Trust is the key itself, not an issuing authority. This is how relays dial each other for DHT RPC, and how two clients talk over a direct link.
- **Recovery**: the identity key exports as a 24-word BIP39 phrase, or goes to platform escrow (Android Block Store). History, contacts, and profile name are sealed separately under XChaCha20-Poly1305, keyed by HKDF-SHA256 from the identity key, so recovering the identity through either channel unlocks the backup with no second password to remember.
- **Misc**: HKDF-SHA256 for signature/transport domain separation, BLAKE3 for hashing and the DHT XOR metric, Postcard with length-prefixed framing on the wire, CBOR for Rust↔Kotlin events.

## Project structure

```
common/     Shared crate: crypto, wire protocol, QUIC config, identity, DHT/MLS/push message types
relay/      Relay node: DHT, client auth, presence + KeyPackage/Welcome replication, store-and-forward, punch assist
resolver/   Resolver: relay and gateway discovery service
gateway/    Push gateway: pseudonym registry and FCM dispatch
libcore/    Client library: MLS engine, networking, media, direct transport, exposed via uniffi (Kotlin/Swift bindings)
android/    Android app: Kotlin, Jetpack Compose, Material 3
ios/        iOS app: Swift, SwiftUI (scaffold)
web/        Deeplink assets for promtuz.dev invite links
tools/      Dev tooling: uniffi-bindgen, packaging and release scripts
```

## What works

- Identity generation with hardware-backed key storage, recoverable by 24-word phrase, platform escrow, or an encrypted backup blob
- Resolver discovery and relay connection with auto-reconnect
- Challenge-response authentication against relays
- A Kademlia DHT between relays: routing table with liveness eviction and bucket refresh, iterative `FindNode`, presence publication, and K-closest fan-out replication with a drift sweep that re-homes records as the closest set moves
- MLS group messaging: KeyPackage publication, Welcome delivery, and application messages
- **End-to-end message delivery** across two independent relays over real QUIC/TLS, validated cross-continent over the public internet
- **Direct peer links**: reflexive-address probing, UDP hole punching, and a relay-side TURN bridge, opt-in per relay, for the pairs that cannot punch, gated to paired contacts only
- A real 1:1 chat: replies, edits, deletes (for me or for everyone), emoji reactions, delivered and read receipts, typing activity, and presence
- Attachments: images encoded to AVIF in libcore and inlined below 256KB, larger files pulled over the direct link with a chunked manifest
- Offline delivery: queued at the home relay, woken through the gateway under a pseudonym, drained in the background on the device
- Contact exchange by QR code, or by an `https://promtuz.dev/pair` invite link whose code rides the URL fragment and never reaches a server log
- In-app updates against a manifest signed with a pinned Ed25519 key
- Debian packages for relay, resolver, and gateway, served from an apt repo

## What doesn't (yet)

- **Resolver mesh**: there's still only one resolver; multiple resolvers don't sync with each other
- **Group chats**: MLS is group-native and the stack carries the group state, but the client API and the UI are 1:1 only
- **Multi-device**: one install per identity. A restore rebuilds history on a new device; two live devices don't stay in sync
- **iOS**: libcore already emits Swift bindings, the app itself is still a scaffold
- **Voice and video**: the call-relay capability bit is reserved, nothing is built behind it

The hard parts are in place and running: networking, identity, the DHT, MLS, NAT traversal, and real delivery over the public internet. What's left is mostly breadth rather than foundations.

## Building

The relay, resolver, and gateway are standard Rust binaries (`cargo run -p relay`, `-p resolver`, `-p gateway`). The client library cross-compiles to Android targets with `cargo-ndk`, and the Android app builds it automatically via a Gradle task.

The infrastructure needs a root CA and node certificates. The `common` crate ships a `certgen` binary that mints them and stamps in the capability bits a node is entitled to.

Deployment packages are built with `tools/scripts/build-deb.sh <crate>`, which links against an old glibc through cargo-zigbuild so the result runs on Debian 10+ and Ubuntu 18.04+. Operator docs live with each node: [relay](relay/README.md), [resolver](resolver/README.md).

## License

[AGPL-3.0-or-later](https://www.gnu.org/licenses/agpl-3.0.en.html).
