//! Relay service guarantees, advertised over an authenticated connection.
//! IDs here belong to the service namespace, not MLS application contracts.

/// v1: per-replica atomic KeyPackage publish and consume, durable consumption before handoff, and
/// no resurrection by a publish retry. Delegated homes must advertise it too.
pub const KEY_PACKAGE_CUSTODY: u16 = 1;
pub const KEY_PACKAGE_CUSTODY_VERSION: u16 = 1;

/// Owner-only, bounded read of each home's available KeyPackage references. Delegation is bound to
/// the owner's signed relay ID and needs the same guarantee on every storage connection.
pub const KEY_PACKAGE_INVENTORY: u16 = 2;
pub const KEY_PACKAGE_INVENTORY_VERSION: u16 = 1;
pub mod key_inventory;

/// Local presence works without DHT. Subscription policy and observations are persisted before
/// acknowledgement. Read interests never imply consent to publish one's own state.
pub const DURABLE_PRESENCE: u16 = 3;
pub const DURABLE_PRESENCE_VERSION: u16 = 1;

/// Atomic, locally durable latest encrypted profile fields and reader grants, without DHT.
pub const PROFILE_STORE: u16 = 4;
pub const PROFILE_STORE_VERSION: u16 = 1;
