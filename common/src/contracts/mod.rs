//! Relay service contracts advertised over an authenticated connection.
use std::collections::BTreeMap;

pub mod services;

const FORMAT: u16 = 1;
const MAX_CONTRACTS: usize = 32;
const MAX_VERSIONS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContractError {
    #[error("malformed contract data")]
    Malformed,
    #[error("contract data exceeds its bound")]
    Limit,
    #[error("unsupported {document} format {version}")]
    DocumentVersion { document: &'static str, version: u16 },
    #[error("unsupported negotiation format {0}")]
    NegotiationFormat(u16),
}
pub type Result<T> = std::result::Result<T, ContractError>;

/// Sorted, bounded, explicit version sets. A range would accidentally imply
/// support for removed versions between its endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Support(BTreeMap<u16, Vec<u16>>);

impl Support {
    pub fn new(contracts: impl IntoIterator<Item = (u16, Vec<u16>)>) -> Result<Self> {
        let mut support = BTreeMap::new();
        for (id, versions) in contracts {
            if id == 0
                || versions.is_empty()
                || versions[0] == 0
                || versions.windows(2).any(|v| v[0] >= v[1])
            {
                return Err(ContractError::Malformed);
            }
            if versions.len() > MAX_VERSIONS || support.len() >= MAX_CONTRACTS {
                return Err(ContractError::Limit);
            }
            if support.insert(id, versions).is_some() {
                return Err(ContractError::Malformed);
            }
        }
        Ok(Self(support))
    }

    pub fn supports(&self, id: u16, version: u16) -> bool {
        self.0.get(&id).is_some_and(|versions| versions.binary_search(&version).is_ok())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        put(&mut bytes, FORMAT);
        put(&mut bytes, self.0.len() as u16);
        for (id, versions) in &self.0 {
            put(&mut bytes, *id);
            put(&mut bytes, versions.len() as u16);
            for version in versions {
                put(&mut bytes, *version);
            }
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut input = Input(bytes);
        input.format()?;
        let count = input.bounded_count(MAX_CONTRACTS)?;
        let mut offers = Vec::with_capacity(count);
        let mut previous = 0;
        for _ in 0..count {
            let id = input.u16()?;
            if id <= previous {
                return Err(ContractError::Malformed);
            }
            previous = id;
            let count = input.bounded_count(MAX_VERSIONS)?;
            let versions = (0..count).map(|_| input.u16()).collect::<Result<Vec<_>>>()?;
            offers.push((id, versions));
        }
        input.finish()?;
        Self::new(offers)
    }
}

fn put(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_be_bytes());
}
struct Input<'a>(&'a [u8]);
impl Input<'_> {
    fn u16(&mut self) -> Result<u16> {
        let (value, tail) = self.0.split_first_chunk::<2>().ok_or(ContractError::Malformed)?;
        self.0 = tail;
        Ok(u16::from_be_bytes(*value))
    }
    fn format(&mut self) -> Result<()> {
        let version = self.u16()?;
        if version != FORMAT {
            return Err(ContractError::NegotiationFormat(version));
        }
        Ok(())
    }
    fn bounded_count(&mut self, max: usize) -> Result<usize> {
        let count = self.u16()? as usize;
        if count > max {
            return Err(ContractError::Limit);
        }
        Ok(count)
    }
    fn finish(self) -> Result<()> {
        if !self.0.is_empty() {
            return Err(ContractError::Malformed);
        }
        Ok(())
    }
}
