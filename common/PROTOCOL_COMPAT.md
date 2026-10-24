# Temporary protocol fallbacks

Protocol: **12**. The build fails on a protocol change until this inventory has
been handled. Remove the listed shims and their compatibility-only tests before
retiring the guard in `src/lib.rs`; do not just change its expected version.

Mark each temporary branch `COMPAT(v12): <id>` so its callers can be found with
`rg 'COMPAT\(v12\)'`. Add new fallbacks here when adding their implementation.
Reserved wire ordinals are not fallback branches and must not be deleted casually.
Local database migrations and backup readers are independent of protocol versions.

## `media-completion` — receiver implemented, sender pending

**Purpose:** let older v12 clients receive an ordinary completed attachment when
the sender supports preparation and partial delivery. No global protocol bump.

- Older recipients receive the existing attachment format after preparation.
- Partial-capable recipients retain one logical message through preparation,
  partial delivery and completion. The fallback must not create a second message,
  restart a verified download or mix data from different encoding attempts.
- This applies to direct chats and mixed-version groups, including delayed,
  duplicate and reordered delivery and reopening either app.
- Receiver: `libcore/src/messaging/preparation.rs::complete_fallback` supplements
  presentation metadata when the ordinary Post arrives first. New-style completion
  is attempt-bound; a fallback Post cannot finalize an active preparation.
- Wire-format proof: `common/src/proto/media.rs::tests` pins the old Post bytes.
- Lifecycle/order proofs: `libcore/src/messaging/preparation.rs::tests`.
- Sender emission, partial delivery and processing recovery are not implemented.
  Add their shim paths here when they land.

**Remove at the next protocol bump:** the old-recipient completed-attachment
fallback and any negotiation/state used only by it. Keep authenticated attempt
identity, integrity checks, idempotent completion, persisted recovery and deletion
protection; those are correctness requirements, not compatibility code.

**Required proof before shipping:** older recipients see one finished attachment;
new recipients complete the existing partial message exactly once; finalization
cannot resurrect deleted media or accept another author's data; direct and
mixed-version group delivery survive retry/reopen.
