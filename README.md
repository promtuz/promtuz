# Promtuz

A decentralized, end-to-end encrypted messenger built in Rust on QUIC, with an Android client in Kotlin/Compose. Your identity is an Ed25519 keypair; you don't need a phone number.

This is a personal project, not an attempt to compete with Signal or Telegram. I just wanted to understand what it takes to build one of these from the ground up. It's still under active development.

## What works

- Direct and group chats support replies, edits, deletion, reactions, delivered/read receipts, typing and presence. Groups have owners, admins and configurable member permissions.
- Exchange contacts through QR codes, invite links or contact cards, with message requests for new contacts.
- Send still and animated AVIF images, videos, voice notes with waveforms and sticker packs. You can also turn videos into stickers. Photos, avatars and stickers go through the same processing in core; Android's codec adapter processes video.
- Choosing File sends the original bytes, even for photos and videos, without compression or metadata stripping.
- Larger attachments download in parallel, hash-verified 256 KiB chunks. Verified download progress and sender transfer history survive app restarts. Transfer details show each recipient's activity, speed, estimated time remaining and a map of file parts.
- Relays queue encrypted messages for offline recipients within storage and expiry limits. Optional push wakeups let the app drain those queues in the background. Transfers retry when connectivity returns or the sender comes online.
- Code-locked chats encrypt their local history, stay out of ordinary chat lists and silence notifications. Enter the code in home search to open them.
- Recover your identity through a 24-word phrase or Android Block Store, and restore history from an encrypted backup.
- Updates have signed manifests and release notes, manual download/install, a choice of channels and release history you can read offline.

## How it works

The network has three infrastructure roles. It works with a single relay; adding relays distributes routing and replication. Infrastructure nodes need certificates issued by the network's CA to join.

A resolver keeps an in-memory directory of available relays and gateways. Clients find nodes through configured resolver seeds. Resolvers hold no chat history, and each keeps its own directory without synchronizing with other resolvers.

Relays authenticate clients, publish presence and route encrypted messages. They form a Kademlia-style DHT that replicates presence, MLS KeyPackages and Welcomes. Queued ciphertext is stored on disk for offline delivery, with expiry and storage limits. Relays can also help peers connect when direct UDP traversal fails. They don't keep a permanent archive of chat history.

A gateway handles FCM push wakeups and encrypted sticker storage. For push, the relay keeps an `identity → pseudonym` mapping and the gateway keeps a separate `pseudonym → device token` mapping. A wakeup asks the app to fetch queued messages without carrying their contents.

The client library, libcore, handles identity, MLS state, delivery, media policy, local storage and attachment transfers. Android calls it through UniFFI and supplies the UI, secure key storage, notifications and video codecs.

A client discovers a relay, then authenticates with an Ed25519 challenge-response bound to its TLS session. Messages use MLS for both direct and group conversations. Relays route ciphertext to recipients or queue it while they are offline. Small images and voice notes travel inside the encrypted message; larger attachments are fetched separately from available peers.

Peer connections use QUIC with hole punching and optional relay forwarding. For networks that block UDP, node operators can enable a TLS/TCP fallback that also supports attachment forwarding through a relay. This needs corresponding node configuration; it is not an HTTP proxy. See the [operator guide](relay/README.md#tcp-fallback-for-phones).

## Security and recovery

- Identity uses Ed25519, with `NodeId = BLAKE3(pubkey)`. Android wraps the private key using an AES-256-GCM key in Android Keystore, preferring StrongBox when available. During use, Rust caches the unwrapped secret in zeroizing memory, so the secret also exists outside secure hardware.
- Messages use [MLS (RFC 9420)](https://www.rfc-editor.org/rfc/rfc9420) through [OpenMLS](https://github.com/openmls/openmls). Leaf signing keys are bound to identity keys, and receivers check group membership and permissions. Periodic MLS key updates aren't scheduled yet.
- Connections use QUIC/TLS 1.3 with rustls and aws-lc-rs. Infrastructure connections validate CA-issued certificates and capabilities; peer connections pin self-signed keys to the expected node identity. Infrastructure that handles the traffic can still see routing identities, IP addresses and timing.
- The 24-word BIP39 phrase restores your identity key. To restore history, you also need an encrypted backup, sealed with XChaCha20-Poly1305 under a key derived from that identity. Code-locked history needs its lock code too. The phrase alone cannot bring back missing messages, contacts or local attachment files.

Photos and videos are resized, recompressed and stripped of privacy-sensitive metadata before sending. JPEG, PNG, GIF and WebP decoders are bundled. AVIF uses libavif, with dav1d for decoding and rav1e for encoding; other supported inputs use platform adapters. HDR images have an SDR fallback. Outgoing video currently targets SDR H.264/AAC MP4. Choosing File preserves the original metadata along with the bytes.

## Project structure

```
common/     Shared crypto, wire types, identity and transport configuration
libcore/    Client state, MLS, messaging, media policy, storage and transfers; UniFFI API
media/      AVIF wrapper and native libavif/dav1d build; rav1e encoder integration
android/    Working client: Kotlin, Jetpack Compose, Material 3 and platform adapters
ios/        SwiftUI scaffold, not a working client
relay/      Routing, DHT, offline queues and peer connection assistance
resolver/   Relay and gateway discovery
gateway/    Push registration, FCM dispatch and encrypted sticker storage
web/        Invite-link assets, not a web messenger
tools/      Bindings generator, media benchmark, licenses, packaging and release scripts
```

## Current limits

- Android is the only working client: Android 8.0+ on `arm64-v8a` and `x86_64`. No working iOS, desktop or web client yet.
- You can use one active install per identity. Restoring a backup doesn't keep two devices in sync.
- Audio/video calling is disabled while the existing calling code is being repaired.
- Media processing must finish before sending. You can't resume an interrupted encode or download a video that's still being processed. The HD image option is disabled.
- An attachment can only be downloaded while a peer holding it is available. Relay message queues don't replace local media storage or backups.

## Building

### Android

Gradle cross-compiles libcore and generates the Kotlin bindings. You'll need:

- Rust with `cargo-ndk` and the `aarch64-linux-android` / `x86_64-linux-android` targets.
- Android SDK and NDK versions declared in [app/build.gradle.kts](android/app/build.gradle.kts), plus the Gradle daemon's [JDK toolchain](android/gradle/gradle-daemon-jvm.properties).
- A C/C++ build toolchain, CMake, Meson and Ninja for the bundled codecs. NASM enables dav1d assembly on x86_64.
- Python 3.11+ for the bundled [open-source license notices](tools/licenses/README.md).

Set the SDK path in `android/local.properties` (`sdk.dir`) or `ANDROID_HOME`. Put the network's public CA certificate at `.tls/RootCA.pem`; it gets compiled into libcore.

You'll also need your Firebase `android/app/google-services.json`. Set `RESOLVER_SEEDS=<resolver-public-key-hex>::<host:port>` in the ignored `android/secrets.properties`, separating multiple seeds with `\n` in that value.

```sh
cargo install cargo-ndk
rustup target add aarch64-linux-android x86_64-linux-android
cd android
./gradlew :app:assembleDebug
```

APKs are split by ABI under `android/app/build/outputs/apk/`. For signing, release builds and publication, see [the release tools](tools/scripts/README.md).

### Infrastructure

The relay, resolver and gateway are Rust binaries (`cargo run -p relay`, `-p resolver`, `-p gateway`). They need node configuration and CA-issued certificates; `common` provides the `certgen` tool. See the [relay](relay/README.md) and [resolver](resolver/README.md) operator guides.

Debian packages use `tools/scripts/build-deb.sh <crate>`, which builds through cargo-zigbuild against an older glibc. Packaging, signing and announcements are documented in [the release tools](tools/scripts/README.md).

## License

[AGPL-3.0-or-later](https://www.gnu.org/licenses/agpl-3.0.en.html).
