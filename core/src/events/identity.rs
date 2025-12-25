use serde::Serialize;

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[allow(unused)]
pub enum Identity {
    AddMe { ipk: [u8; 32], nickname: String }
}