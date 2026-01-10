use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
#[allow(unused)]
pub enum IdentityEv {
    AddMe { ipk: [u8; 32], name: String }
}