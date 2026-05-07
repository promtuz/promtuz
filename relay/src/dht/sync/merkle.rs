//! Per-slice radix-16 Merkle trie covering 16-bit leaves of `user_ipk`.
//!
//! ## Geometry (§6.1)
//!
//! Each `SliceTree` covers all `user_ipk`s whose top
//! `MERKLE_SLICE_BITS == 8` bits equal `slice_id` (so 256 slices over the
//! full keyspace). Within a slice we walk the next
//! `MERKLE_LEAF_BITS == 16` bits as 4 nibbles
//! (`MERKLE_FANOUT == 16` → 4 bits per level → depth 4) to reach a leaf.
//! A leaf holds the actual `(user_ipk, value_hash)` records that share the
//! same `(slice_id, nibble0, nibble1, nibble2, nibble3)` prefix.
//!
//! The hashing scheme:
//! - `leaf_hash` = `BLAKE3("dht-merkle-leaf-v1" || sorted (ipk || value_hash) entries)`.
//! - `internal_hash` = `BLAKE3("dht-merkle-node-v1" || child_0 || child_1 || ... || child_15)`.
//! - Empty leaf / empty subtree hashes to all-zeros so an empty slice's
//!   root is `[0; 32]` and is trivially distinguishable from any
//!   populated state in a [`MerkleSummary`] reply.
//!
//! ## Sparse storage
//!
//! Most slices touch only a handful of leaves; rather than allocating
//! `16^4 = 65 536` slots per slice, we keep a `HashMap<NodePath, Bytes32>`
//! and a `HashMap<NodePath, Vec<LeafEntry>>` and recompute the root from
//! the affected leaf-to-root path on each `insert` / `remove`. This is
//! exactly the lazy-population pattern §6.1 calls for.
//!
//! ## Recovery from process restart
//!
//! Per the dispatch's "no persistence" decision, the on-disk
//! `dht_merkle` CF is **not** populated by this implementation. On
//! relay restart the tree is rebuilt from `cf_dht_presence` records via
//! [`MerkleState::rebuild_from_records`] — at design-doc §6.4 scale (~300
//! records per relay) this is a few-millisecond walk and avoids a second
//! source of truth that could diverge.
//!
//! design-doc: §6.1 (per-slice Merkle tree), §6.3 (sync sequence).

use std::collections::HashMap;

use common::quic::id::NodeId;

// Re-export the module-level fanout under this module's namespace so
// siblings (`sync/mod.rs`, tests) can refer to it alongside the
// depth-derived helpers below without a second `use config::`.
pub(crate) use crate::dht::config::MERKLE_FANOUT;
use crate::dht::config::MERKLE_LEAF_BITS;
use crate::dht::config::MERKLE_SLICE_BITS;

// ---------------------------------------------------------------------------
// Domain-separation tags
// ---------------------------------------------------------------------------

/// Domain prefix mixed into a leaf's BLAKE3 hash so two trees with
/// identical entry sets can never coincide with an internal-node hash.
const MERKLE_LEAF_DOMAIN: &[u8] = b"dht-merkle-leaf-v1";

/// Domain prefix for internal-node hashes. Distinct from
/// [`MERKLE_LEAF_DOMAIN`] so a captured leaf hash can never be replayed
/// as an internal node.
const MERKLE_NODE_DOMAIN: &[u8] = b"dht-merkle-node-v1";

/// Domain prefix for the per-record value-hash. The presence of this
/// tag in the input guarantees a tombstone's leaf-hash and a record's
/// leaf-hash for the *same* (user_ipk, generation) pair are distinct,
/// so anti-entropy converges deletions, not just insertions.
pub(crate) const MERKLE_RECORD_DOMAIN: &[u8] = b"dht-merkle-record-v1";

/// Domain prefix for tombstone value-hashes. Symmetric to
/// [`MERKLE_RECORD_DOMAIN`].
pub(crate) const MERKLE_TOMBSTONE_DOMAIN: &[u8] = b"dht-merkle-tombstone-v1";

/// Tree depth: `MERKLE_LEAF_BITS / 4` nibbles to reach a leaf.
pub(crate) const TREE_DEPTH: usize = (MERKLE_LEAF_BITS as usize) / 4;

// ---------------------------------------------------------------------------
// NodePath
// ---------------------------------------------------------------------------

/// Path of nibble indices from the slice root.
///
/// Length 0 = the slice root; length [`TREE_DEPTH`] (4) = a leaf node.
/// Stored as a `Vec<u8>` rather than a `[u8; 4]` so tests / RPC
/// handlers can reuse the same type for partial-depth paths during
/// bisect (§6.3).
pub(crate) type NodePath = Vec<u8>;

/// Compute the full `(slice_id, nibble_path)` path for a `user_ipk`. The
/// returned vec has length [`TREE_DEPTH`].
///
/// Layout:
/// - Slice id is `user_ipk[0]` (top 8 bits).
/// - Nibble 0 is `user_ipk[1] >> 4` (next 4 bits).
/// - Nibble 1 is `user_ipk[1] & 0x0F`.
/// - Nibble 2 is `user_ipk[2] >> 4`.
/// - Nibble 3 is `user_ipk[2] & 0x0F`.
pub(crate) fn nibble_path_for(user_ipk: &[u8; 32]) -> (u8, NodePath) {
    let slice_id = user_ipk[0];
    let mut path = Vec::with_capacity(TREE_DEPTH);
    // Walk MERKLE_LEAF_BITS bits past the slice prefix, in 4-bit chunks.
    // For LEAF_BITS=16 this is bytes [1..3] split into hi/lo nibbles.
    let leaf_bytes = (MERKLE_LEAF_BITS as usize) / 8;
    for byte in &user_ipk[1..1 + leaf_bytes] {
        path.push((*byte >> 4) & 0x0F);
        path.push(*byte & 0x0F);
    }
    debug_assert_eq!(path.len(), TREE_DEPTH);
    (slice_id, path)
}

// ---------------------------------------------------------------------------
// Hashing helpers
// ---------------------------------------------------------------------------

/// Hash a sorted list of `(user_ipk, value_hash)` entries into a single
/// 32-byte leaf hash. The empty list hashes to `[0u8; 32]` so an empty
/// leaf is trivially distinguishable from any populated leaf.
///
/// **Caller responsibility:** entries must be sorted by `user_ipk`
/// ascending before calling. The hash is order-sensitive but the
/// canonical order ensures every replica with the same set arrives at
/// the same hash.
fn hash_leaf(sorted_entries: &[([u8; 32], [u8; 32])]) -> [u8; 32] {
    if sorted_entries.is_empty() {
        return [0u8; 32];
    }
    let mut buf =
        Vec::with_capacity(MERKLE_LEAF_DOMAIN.len() + sorted_entries.len() * 64);
    buf.extend_from_slice(MERKLE_LEAF_DOMAIN);
    for (ipk, vh) in sorted_entries {
        buf.extend_from_slice(ipk);
        buf.extend_from_slice(vh);
    }
    *NodeId::new(&buf).as_bytes()
}

/// Hash an internal node from its [`MERKLE_FANOUT`] children.
fn hash_internal(children: &[[u8; 32]; MERKLE_FANOUT]) -> [u8; 32] {
    // Optimisation: an all-zero subtree hashes back to zero. Lets a
    // sparse tree skip work for empty branches without a recursive
    // descent.
    if children.iter().all(|c| c == &[0u8; 32]) {
        return [0u8; 32];
    }
    let mut buf = Vec::with_capacity(MERKLE_NODE_DOMAIN.len() + 32 * MERKLE_FANOUT);
    buf.extend_from_slice(MERKLE_NODE_DOMAIN);
    for c in children {
        buf.extend_from_slice(c);
    }
    *NodeId::new(&buf).as_bytes()
}

// ---------------------------------------------------------------------------
// Leaf storage
// ---------------------------------------------------------------------------

/// Single leaf-entry: the user IPK and its value-hash. `value_hash` is
/// caller-supplied (see [`MERKLE_RECORD_DOMAIN`] /
/// [`MERKLE_TOMBSTONE_DOMAIN`] for the canonical recipe).
type LeafEntry = ([u8; 32], [u8; 32]);

// ---------------------------------------------------------------------------
// SliceTree
// ---------------------------------------------------------------------------

/// One slice's worth of Merkle state.
///
/// The tree is stored sparsely:
/// - `leaves` keeps full `(user_ipk → value_hash)` records, keyed by the
///   nibble-path that leads to the leaf.
/// - `nodes` caches the BLAKE3 hash at every populated internal-node
///   path. The slice root is at the empty path.
///
/// Re-hashing on insert/remove walks only the affected leaf-to-root
/// path, which is `TREE_DEPTH = 4` levels deep. Anything outside the
/// touched branch is left untouched.
#[derive(Debug, Default)]
pub(crate) struct SliceTree {
    /// Top byte of `user_ipk` this tree covers (0..256).
    pub slice_id: u8,

    /// Sparse leaf storage: `nibble_path` → list of `(ipk, value_hash)`.
    /// The list is kept in ipk-sorted order so [`hash_leaf`] is
    /// deterministic regardless of insertion order.
    leaves: HashMap<NodePath, Vec<LeafEntry>>,

    /// Sparse internal-node hash cache. The root is at `path = []`.
    /// Internal nodes whose entire subtree is empty are absent (they
    /// hash to all-zeros, and [`hash_internal`] short-circuits that).
    nodes: HashMap<NodePath, [u8; 32]>,
}

impl SliceTree {
    /// Empty tree for `slice_id`. No allocations beyond the struct
    /// itself; populated lazily as records arrive (§6.2).
    pub(crate) fn new(slice_id: u8) -> Self {
        Self {
            slice_id,
            leaves: HashMap::new(),
            nodes: HashMap::new(),
        }
    }

    /// Cached slice-root hash, or `[0u8; 32]` when the slice is empty.
    pub(crate) fn root(&self) -> [u8; 32] {
        self.nodes.get(&Vec::<u8>::new()).copied().unwrap_or([0u8; 32])
    }

    /// True iff the tree currently holds no entries (so its root is
    /// `[0; 32]`).
    pub(crate) fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }

    /// Insert (or update) the leaf entry for `user_ipk` to `value_hash`.
    /// Re-hashes the affected leaf-to-root path.
    ///
    /// Idempotent on the (ipk, value_hash) pair: re-inserting the same
    /// pair leaves the tree unchanged.
    pub(crate) fn insert(&mut self, user_ipk: &[u8; 32], value_hash: [u8; 32]) {
        let (sid, path) = nibble_path_for(user_ipk);
        debug_assert_eq!(sid, self.slice_id);

        let entries = self.leaves.entry(path.clone()).or_default();
        match entries.binary_search_by(|(ipk, _)| ipk.cmp(user_ipk)) {
            Ok(pos) => {
                // Already present — update if changed.
                if entries[pos].1 == value_hash {
                    return;
                }
                entries[pos].1 = value_hash;
            }
            Err(pos) => {
                entries.insert(pos, (*user_ipk, value_hash));
            }
        }
        self.recompute_path(&path);
    }

    /// Remove the leaf entry for `user_ipk`. Re-hashes the affected
    /// path. No-op if `user_ipk` is not present.
    pub(crate) fn remove(&mut self, user_ipk: &[u8; 32]) {
        let (sid, path) = nibble_path_for(user_ipk);
        debug_assert_eq!(sid, self.slice_id);

        let Some(entries) = self.leaves.get_mut(&path) else {
            return;
        };
        let pos = match entries.binary_search_by(|(ipk, _)| ipk.cmp(user_ipk)) {
            Ok(pos) => pos,
            Err(_) => return, // not present
        };
        entries.remove(pos);
        if entries.is_empty() {
            self.leaves.remove(&path);
        }
        self.recompute_path(&path);
    }

    /// Re-hash the leaf at `path`, then walk up every ancestor to the
    /// root, re-hashing in turn. Any internal node whose recomputed
    /// hash is `[0; 32]` is *removed* from `nodes` so the sparse map
    /// reflects the empty-subtree shortcut [`hash_internal`] takes.
    fn recompute_path(&mut self, path: &NodePath) {
        debug_assert_eq!(path.len(), TREE_DEPTH);
        // 1. Recompute leaf.
        let leaf_hash = match self.leaves.get(path) {
            Some(entries) => hash_leaf(entries),
            None => [0u8; 32],
        };
        if leaf_hash == [0u8; 32] {
            self.nodes.remove(path);
        } else {
            self.nodes.insert(path.clone(), leaf_hash);
        }

        // 2. Walk up to root, re-hashing each ancestor.
        for level in (0..TREE_DEPTH).rev() {
            let prefix = &path[..level];
            let mut children = [[0u8; 32]; MERKLE_FANOUT];
            for (i, child) in children.iter_mut().enumerate() {
                let mut child_path = Vec::with_capacity(level + 1);
                child_path.extend_from_slice(prefix);
                child_path.push(i as u8);
                if let Some(h) = self.nodes.get(&child_path) {
                    *child = *h;
                }
            }
            let h = hash_internal(&children);
            let prefix_owned: NodePath = prefix.to_vec();
            if h == [0u8; 32] {
                self.nodes.remove(&prefix_owned);
            } else {
                self.nodes.insert(prefix_owned, h);
            }
        }
    }

    /// Return the 16 child hashes at the internal node at `path`. If a
    /// child position has no record below it, its slot is `[0; 32]`.
    ///
    /// Caller must ensure `path.len() < TREE_DEPTH` — the leaf-level
    /// case is handled by [`Self::leaves_at`] / [`Self::diff`].
    pub(crate) fn children_at(&self, path: &NodePath) -> [[u8; 32]; MERKLE_FANOUT] {
        debug_assert!(path.len() < TREE_DEPTH);
        let mut out = [[0u8; 32]; MERKLE_FANOUT];
        for (i, slot) in out.iter_mut().enumerate() {
            let mut child_path = Vec::with_capacity(path.len() + 1);
            child_path.extend_from_slice(path);
            child_path.push(i as u8);
            if let Some(h) = self.nodes.get(&child_path) {
                *slot = *h;
            }
        }
        out
    }

    /// Return the `(user_ipk, value_hash)` pairs in the leaf at `path`.
    /// `path` must be exactly [`TREE_DEPTH`] nibbles deep. Empty if the
    /// leaf has no entries.
    pub(crate) fn leaves_at(&self, path: &NodePath) -> Vec<LeafEntry> {
        debug_assert_eq!(path.len(), TREE_DEPTH);
        self.leaves.get(path).cloned().unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// MerkleState — convenience accessors
// ---------------------------------------------------------------------------
//
// These functions live here (not on `super::MerkleState`) because they
// share the hashing helpers above. `super::mod::MerkleState` calls into
// them via thin wrappers.

/// Slot-id helper: `MERKLE_SLICE_BITS == 8` => slice_id is just byte 0.
#[inline]
pub(crate) fn slice_id_for(user_ipk: &[u8; 32]) -> u8 {
    debug_assert_eq!(MERKLE_SLICE_BITS, 8);
    user_ipk[0]
}

/// Compute the canonical record-hash for a presence record. Used as the
/// `value_hash` argument to [`SliceTree::insert`] when a record is
/// stored.
///
/// Includes the postcard-serialised record bytes prefixed by
/// [`MERKLE_RECORD_DOMAIN`] — this means two replicas holding the same
/// `(generation, user_sig, relay_sig, timestamps)` tuple produce the
/// same value-hash, while any field divergence diverges the hash and
/// triggers anti-entropy.
pub(crate) fn record_value_hash(serialized_record: &[u8]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(MERKLE_RECORD_DOMAIN.len() + serialized_record.len());
    buf.extend_from_slice(MERKLE_RECORD_DOMAIN);
    buf.extend_from_slice(serialized_record);
    *NodeId::new(&buf).as_bytes()
}

/// Compute the canonical tombstone-hash. Symmetric to
/// [`record_value_hash`] but with a distinct domain so a record-hash
/// and a tombstone-hash for the same `(user_ipk, generation)` are
/// always different.
pub(crate) fn tombstone_value_hash(serialized_tomb: &[u8]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(MERKLE_TOMBSTONE_DOMAIN.len() + serialized_tomb.len());
    buf.extend_from_slice(MERKLE_TOMBSTONE_DOMAIN);
    buf.extend_from_slice(serialized_tomb);
    *NodeId::new(&buf).as_bytes()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ipk_for(slice: u8, b1: u8, b2: u8, b3: u8) -> [u8; 32] {
        let mut k = [0u8; 32];
        k[0] = slice;
        k[1] = b1;
        k[2] = b2;
        k[3] = b3;
        k
    }

    #[test]
    fn nibble_path_for_decomposes_correctly() {
        let k = ipk_for(0xAA, 0x12, 0x34, 0xFF);
        let (sid, path) = nibble_path_for(&k);
        assert_eq!(sid, 0xAA);
        assert_eq!(path, vec![0x1, 0x2, 0x3, 0x4]);
    }

    #[test]
    fn merkle_state_empty_root_is_zero() {
        let tree = SliceTree::new(7);
        assert_eq!(tree.root(), [0u8; 32]);
        assert!(tree.is_empty());
    }

    #[test]
    fn merkle_state_insert_then_remove_returns_to_zero_root() {
        let mut tree = SliceTree::new(7);
        let k = ipk_for(7, 0x01, 0x02, 0x03);
        tree.insert(&k, [42u8; 32]);
        assert_ne!(tree.root(), [0u8; 32]);

        tree.remove(&k);
        assert_eq!(tree.root(), [0u8; 32]);
        assert!(tree.is_empty());
    }

    #[test]
    fn merkle_state_two_inserts_in_same_slice_diff_keys() {
        // Insert two records in slice 5 in opposite orders → root
        // identical (Merkle hash is over a sorted-canonical view).
        let k1 = ipk_for(5, 0x11, 0x00, 0x00);
        let k2 = ipk_for(5, 0x22, 0x00, 0x00);
        let v1 = [1u8; 32];
        let v2 = [2u8; 32];

        let mut a = SliceTree::new(5);
        a.insert(&k1, v1);
        a.insert(&k2, v2);
        let root_a = a.root();

        let mut b = SliceTree::new(5);
        b.insert(&k2, v2);
        b.insert(&k1, v1);
        let root_b = b.root();

        assert_ne!(root_a, [0u8; 32]);
        assert_eq!(root_a, root_b, "insert order must not affect the root hash");
    }

    #[test]
    fn merkle_state_value_hash_change_changes_root() {
        // Same key, different value-hash → different root. This is the
        // §6.3 "convergence on diverging value, not just absence" case.
        let k = ipk_for(5, 0x11, 0x22, 0x33);
        let mut a = SliceTree::new(5);
        a.insert(&k, [1u8; 32]);
        let root_a = a.root();

        let mut b = SliceTree::new(5);
        b.insert(&k, [2u8; 32]);
        let root_b = b.root();

        assert_ne!(root_a, root_b);
    }

    #[test]
    fn merkle_state_diff_at_root_returns_children() {
        // Build a tree with one entry and check that the slice-root's
        // 16 child hashes have exactly one non-zero slot — the one
        // matching the entry's first nibble.
        let mut tree = SliceTree::new(5);
        let k = ipk_for(5, 0xA0, 0x00, 0x00); // first nibble = 0xA
        tree.insert(&k, [1u8; 32]);

        let children = tree.children_at(&Vec::<u8>::new());
        for (i, c) in children.iter().enumerate() {
            if i == 0xA {
                assert_ne!(c, &[0u8; 32], "child index 0xA must have a non-zero hash");
            } else {
                assert_eq!(c, &[0u8; 32], "child index {i} must be zero");
            }
        }
    }

    #[test]
    fn merkle_state_diff_at_leaf_depth_returns_leaves() {
        let mut tree = SliceTree::new(5);
        let k1 = ipk_for(5, 0xA0, 0x00, 0x00);
        let k2 = ipk_for(5, 0xA0, 0x00, 0xFF); // same leaf path (top 16 bits identical)
        tree.insert(&k1, [1u8; 32]);
        tree.insert(&k2, [2u8; 32]);

        let (_, path) = nibble_path_for(&k1);
        let leaves = tree.leaves_at(&path);
        assert_eq!(leaves.len(), 2);
        // Sorted by ipk.
        assert_eq!(leaves[0].0, k1);
        assert_eq!(leaves[1].0, k2);
    }

    #[test]
    fn merkle_state_diff_at_unrelated_leaf_returns_empty() {
        let tree = SliceTree::new(5);
        let path: NodePath = vec![0, 0, 0, 0];
        assert!(tree.leaves_at(&path).is_empty());
    }

    #[test]
    fn merkle_state_root_recovers_after_partial_remove() {
        // Insert 3 leaves, remove one, verify root matches a tree
        // built directly with the surviving 2 entries.
        let k1 = ipk_for(5, 0x11, 0x00, 0x00);
        let k2 = ipk_for(5, 0x22, 0x00, 0x00);
        let k3 = ipk_for(5, 0x33, 0x00, 0x00);

        let mut a = SliceTree::new(5);
        a.insert(&k1, [1u8; 32]);
        a.insert(&k2, [2u8; 32]);
        a.insert(&k3, [3u8; 32]);
        a.remove(&k2);
        let root_a = a.root();

        let mut b = SliceTree::new(5);
        b.insert(&k1, [1u8; 32]);
        b.insert(&k3, [3u8; 32]);
        let root_b = b.root();

        assert_eq!(root_a, root_b);
    }

    #[test]
    fn record_and_tombstone_value_hashes_differ_for_same_input() {
        let bytes = b"sample-record";
        assert_ne!(record_value_hash(bytes), tombstone_value_hash(bytes));
    }
}
