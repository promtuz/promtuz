# Contact cards and encrypted requests

A shared card discovers an identity. It does not grant pairing, group membership, or permission to transfer files. The recipient reviews a request and explicitly accepts it before the normal MLS pairing flow runs.

## Card

The postcard-encoded card contains the owner's Ed25519 identity public key, display name (1–32 characters), X25519 request public key, and Ed25519 signature. The signature covers `promtuz-contact-card-v1` followed by the postcard tuple `(identity, name, request_key)`. A forwarding contact cannot change these fields. Cards have no expiry or revocation mechanism; a copied card may show an old name. Possessing a card permits sending requests, never accepting them.

The request key is separate from the signing scalar. HKDF-SHA256 derives 32 bytes from the identity signing seed with info `promtuz-contact-request-key-v1`; RFC 9180 DHKEM(X25519, HKDF-SHA256) DeriveKeyPair produces the request key pair. Identity recovery reproduces this key. The public key is bound by the card signature.

Updated clients send their signed card, name and bio inside authenticated MLS `ProfileDetails` controls. Only verified owner cards can be forwarded from contact info. A personal nickname is local and never appears in the forwarded card.

## Request

`MlsEnvelopeP::ContactRequest` appends a new envelope variant, preserving previous discriminants. It carries sender and recipient identity keys, a random 16-byte ID, seven-day expiry in Unix milliseconds, HPKE encapsulation, ciphertext, and an outer Ed25519 signature.

Encryption uses RFC 9180 base-mode HPKE with DHKEM(X25519, HKDF-SHA256), HKDF-SHA256 and ChaCha20Poly1305. HPKE info is `promtuz-contact-request-v1`. Associated data is that domain followed by the fixed-width sender key, recipient key, request ID, and big-endian expiry. The encrypted plaintext is the requester's signed card. The outer signature covers associated data, encapsulation and ciphertext.

The receiver checks routing identity, recipient, signature, size, expiry, decryption and the inner card's owner. The requester name is revealed only after successful decryption. Relays still see routing identities, timing, size and expiry. Static recipient request keys mean archived requests do **not** have forward secrecy against later compromise of the recipient's identity seed; established conversations use the existing MLS lifecycle.

## Consent and delivery

Receiving a valid request creates no contact or conversation. Pending incoming requests are capped at 100. Duplicate or renewed requests cannot undo a decline until the retained request expires. Expired requests are hidden and pruned on subsequent requests. Five minutes of future clock tolerance is allowed in addition to the seven-day lifetime.

Outgoing consent and the sealed envelope are stored in one SQLite transaction. The request has its own durable outbox, retried on reconnect and periodically while online. Cancellation removes that work and revokes local consent. It cannot retract a request already delivered to the other device. A later Welcome is refused once consent is revoked or expired. Explicit resend creates fresh consent.

Acceptance runs in a core-owned task and is serialized against another acceptance or incoming dismissal. It sends a normal signed MLS Welcome without a bearer invite. The requester accepts it only while outgoing consent remains valid. Initial pairing must contain exactly the two expected identities and no group metadata. Contact creation happens after successful Welcome validation. Ordinary invitation pairing remains available.

## Compatibility and recovery

New profile controls and request envelopes are appended to their enums. Older clients do not gain contact-card support; the UI hides forwarding until an owner-signed card exists. Existing invitations and name-only profile introductions remain supported. This is not a replacement for the broader protocol-version compatibility work in TODOS.md.

Bios are included in identity backups. Admin-owned group photos and removals use an optional appended backup suffix; older backups still decode. Peer profile details are repaired through revision/ack reconciliation. Incoming/outgoing requests are device-local and are not restored as pairing consent from a backup.

Android links use `https://promtuz.dev/contact#<base64url-card>` with a `promtuz://contact#...` fallback. Deploy `web/contact/index.html` at `/contact` for browsers without verified App Links. The fragment is not sent in the HTTP request.

## Verification

Local tests cover signed-card tampering, encrypted name previews, recipient and sender binding, expiry, ciphertext corruption, embedded identity substitution, decline replay protection, group-photo authorization and removal ordering, profile transaction failures and repair, and backup suffix compatibility. Live two-account request delivery and notification behavior still require a known test recipient.
