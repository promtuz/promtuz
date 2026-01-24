use serde::Deserialize;
use serde::Serialize;

use crate::events::Emittable;
use crate::events::InternalEvent;

/// For InternalEvents
#[allow(unused)]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum IdentityEv {
    AddMe {
        #[serde(with = "serde_bytes")]
        ipk: [u8; 32],
        name: String,
    },
}

impl Emittable for IdentityEv {
    fn emit(self) {
        InternalEvent::emit("IDENTITY", &self);
    }
}
