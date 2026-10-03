//! Owner-authorized, read-only KeyPackage availability. A snapshot covers one home and reserves
//! nothing; missing home evidence never means empty.
use crate::contracts::{ContractError, Result};

pub const VERSION: u16 = 1;
pub const MAX_HOMES: usize = 4;
pub const MAX_REFERENCES: usize = 100;
pub const MAX_SKEW_MS: u64 = 60_000;
const HEADER: &[u8; 6] = b"PZKI\0\x01";
const REQUEST_BYTES: usize = 6 + 32 + 32 + 8 + 64;
const MAX_RESPONSE_BYTES: usize =
    6 + 32 + 8 + 1 + MAX_HOMES * (32 + 1 + 8 + 1 + MAX_REFERENCES * 32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub owner: [u8; 32],
    pub delegate: [u8; 32],
    pub timestamp: u64,
    pub signature: [u8; 64],
}
impl Request {
    pub fn signing_input(&self) -> Vec<u8> {
        let domain: &[u8] = b"promtuz-key-package-inventory";
        [domain, HEADER, &self.owner, &self.delegate, &self.timestamp.to_be_bytes()].concat()
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(REQUEST_BYTES);
        bytes.extend_from_slice(HEADER);
        bytes.extend_from_slice(&self.owner);
        bytes.extend_from_slice(&self.delegate);
        bytes.extend_from_slice(&self.timestamp.to_be_bytes());
        bytes.extend_from_slice(&self.signature);
        bytes
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != REQUEST_BYTES {
            return Err(ContractError::Malformed);
        }
        let mut input = Input::new(bytes)?;
        Ok(Self {
            owner: input.take()?,
            delegate: input.take()?,
            timestamp: u64::from_be_bytes(input.take()?),
            signature: input.take()?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub observed_at_ms: u64,
    pub references: Vec<[u8; 32]>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Home {
    pub node: [u8; 32],
    /// None includes unavailable, unsupported, rejected, and no-longer-owner.
    /// Some with no references is the only positive evidence of an empty home.
    pub snapshot: Option<Snapshot>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inventory {
    pub owner: [u8; 32],
    pub request_timestamp: u64,
    /// Every selected home appears once, including failed requests. Sorted by ID.
    pub homes: Vec<Home>,
}
impl Inventory {
    pub fn complete(&self) -> bool {
        !self.homes.is_empty() && self.homes.iter().all(|home| home.snapshot.is_some())
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.homes.is_empty() || self.homes.len() > MAX_HOMES {
            return Err(ContractError::Limit);
        }
        if self.homes.windows(2).any(|pair| pair[0].node >= pair[1].node) {
            return Err(ContractError::Malformed);
        }
        let mut bytes = HEADER.to_vec();
        bytes.extend_from_slice(&self.owner);
        bytes.extend_from_slice(&self.request_timestamp.to_be_bytes());
        bytes.push(self.homes.len() as u8);
        for home in &self.homes {
            bytes.extend_from_slice(&home.node);
            match &home.snapshot {
                None => bytes.push(0),
                Some(snapshot) => {
                    if snapshot.references.len() > MAX_REFERENCES {
                        return Err(ContractError::Limit);
                    }
                    if snapshot.references.windows(2).any(|pair| pair[0] >= pair[1]) {
                        return Err(ContractError::Malformed);
                    }
                    bytes.push(1);
                    bytes.extend_from_slice(&snapshot.observed_at_ms.to_be_bytes());
                    bytes.push(snapshot.references.len() as u8);
                    for reference in &snapshot.references {
                        bytes.extend_from_slice(reference);
                    }
                },
            }
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(ContractError::Limit);
        }
        let mut input = Input::new(bytes)?;
        let owner = input.take()?;
        let request_timestamp = u64::from_be_bytes(input.take()?);
        let count = input.count(MAX_HOMES)?;
        if count == 0 {
            return Err(ContractError::Malformed);
        }
        let mut homes: Vec<Home> = Vec::with_capacity(count);
        for _ in 0..count {
            let node = input.take()?;
            if homes.last().is_some_and(|home| home.node >= node) {
                return Err(ContractError::Malformed);
            }
            let snapshot = match input.take::<1>()?[0] {
                0 => None,
                1 => {
                    let observed_at_ms = u64::from_be_bytes(input.take()?);
                    let count = input.count(MAX_REFERENCES)?;
                    let mut references: Vec<[u8; 32]> = Vec::with_capacity(count);
                    for _ in 0..count {
                        let reference = input.take()?;
                        if references.last().is_some_and(|previous| *previous >= reference) {
                            return Err(ContractError::Malformed);
                        }
                        references.push(reference);
                    }
                    Some(Snapshot { observed_at_ms, references })
                },
                _ => return Err(ContractError::Malformed),
            };
            homes.push(Home { node, snapshot });
        }
        if !input.0.is_empty() {
            return Err(ContractError::Malformed);
        }
        Ok(Self { owner, request_timestamp, homes })
    }
    /// Bound a reply to the fresh query before using its evidence.
    pub fn matches(&self, request: &Request) -> bool {
        self.owner == request.owner
            && self.request_timestamp == request.timestamp
            && self.homes.iter().all(|home| {
                home.snapshot.as_ref().is_none_or(|snapshot| {
                    snapshot.observed_at_ms.abs_diff(request.timestamp) <= MAX_SKEW_MS
                })
            })
    }
}
struct Input<'a>(&'a [u8]);
impl<'a> Input<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self> {
        if bytes.get(..4) != Some(b"PZKI") {
            return Err(ContractError::Malformed);
        }
        let mut input = Self(&bytes[4..]);
        let version = u16::from_be_bytes(input.take()?);
        if version != VERSION {
            return Err(ContractError::DocumentVersion {
                document: "KeyPackage inventory",
                version,
            });
        }
        Ok(input)
    }
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        let (head, tail) = self.0.split_first_chunk::<N>().ok_or(ContractError::Malformed)?;
        self.0 = tail;
        Ok(*head)
    }
    fn count(&mut self, limit: usize) -> Result<usize> {
        let count = self.take::<1>()?[0] as usize;
        if count > limit {
            return Err(ContractError::Limit);
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcripts() {
        let request =
            Request { owner: [1; 32], delegate: [2; 32], timestamp: 0x0102, signature: [3; 64] };
        crate::proto::golden(
            &[request.signing_input()],
            "54db042ee3ea93eae52f97a362005b66bb0a3f089f491fb06effb40009a5c0cb",
        );
    }
}
