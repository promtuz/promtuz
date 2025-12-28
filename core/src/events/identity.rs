use serde::Serialize;

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[allow(unused)]
pub enum IdentityEv {
    AddMe { ipk: [u8; 32], name: String }
}