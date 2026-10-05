//! Local latest-profile service v1. Object identity is independent of its hosting relay.
//! Relay authentication still uses identity keys; these are not anonymous mailboxes.
use crate::types::bytes::{ByteVec, Bytes};
use serde::{Deserialize, Serialize};

pub const MAX_READERS: usize = 1024;
pub const MAX_CIPHERTEXT: usize = 65_600;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Field {
    Name,
    Bio,
    Avatar,
}
impl Field {
    pub const ALL: [Self; 3] = [Self::Name, Self::Bio, Self::Avatar];
    pub fn id(self) -> u8 {
        self as u8
    }
}

/// Deterministic, identity-signed HPKE public key. No profile text is in the directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaderKey {
    pub owner: Bytes<32>,
    pub key: Bytes<32>,
    pub signature: Bytes<64>,
}
impl ReaderKey {
    pub fn input(&self) -> Vec<u8> {
        [b"promtuz-profile-reader-v1".as_slice(), &self.owner.0, &self.key.0].concat()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Value {
    pub owner: Bytes<32>,
    pub object: Bytes<32>,
    pub field: Field,
    pub version: u64,
    pub ciphertext: ByteVec,
    pub signature: Bytes<64>,
}
impl Value {
    pub fn context(&self) -> Vec<u8> {
        [
            b"promtuz-profile-value-v1".as_slice(),
            &self.owner.0,
            &self.object.0,
            &[self.field.id()],
            &self.version.to_be_bytes(),
        ]
        .concat()
    }
    pub fn input(&self) -> Vec<u8> {
        [self.context(), self.ciphertext.0.clone()].concat()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub reader: Bytes<32>,
    pub encapsulated: Bytes<32>,
    pub wrapped_key: Bytes<48>,
}

/// Replaces the entire field and audience atomically, including an empty audience (withdrawal).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publication {
    pub value: Value,
    #[serde(deserialize_with = "crate::proto::pack::bounded_vec::<_, _, MAX_READERS>")]
    pub grants: Vec<Grant>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Register(ReaderKey),
    ReaderKey {
        owner: Bytes<32>,
    },
    Publish(Publication),
    /// Owner-only recovery of the current object/version, never a reader lookup.
    Head {
        field: Field,
    },
    Fetch {
        owner: Bytes<32>,
        field: Field,
        known_version: Option<u64>,
    },
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Accepted,
    Rejected,
    ReaderKey(Option<ReaderKey>),
    Value {
        value: Value,
        grant: Grant,
    },
    Unchanged,
    /// No record is not a withdrawal: a temporarily different relay may have no copy.
    Missing,
    Head(Option<(Bytes<32>, u64)>),
    /// The record exists but has no grant for this authenticated reader.
    Withdrawn {
        value: Value,
    },
}
