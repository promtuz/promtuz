//! One ciphertext upload with the same recipient-bound signatures used by ordinary delivery.
//! The audience is fixed by the sender's authenticated MLS state, never today's relay roster.
use super::{
    client_rel::{DispatchP, Wake},
    mls_wire::{MLS_ENVELOPE_VERSION, MlsApplicationEnvelopeP, MlsEnvelopeP},
    pack::Packer,
};
use crate::types::bytes::{ByteVec, Bytes};
use serde::{Deserialize, Serialize};

pub const MAX_RECIPIENTS: usize = 255;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipient {
    pub to: Bytes<32>,
    pub envelope_sig: Bytes<64>,
    pub dispatch_sig: Bytes<64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publication {
    pub from: Bytes<32>,
    pub id: Bytes<16>,
    pub group: Bytes<32>,
    pub branch: Bytes<32>,
    pub epoch: u64,
    pub message: ByteVec,
    pub proof: Option<ByteVec>,
    pub wake: Wake,
    pub ttl_ms: u64,
    #[serde(deserialize_with = "super::pack::bounded_vec::<_, _, MAX_RECIPIENTS>")]
    pub recipients: Vec<Recipient>,
}

impl Publication {
    pub fn dispatch(
        &self, recipient: &Recipient, accepted_at_ms: u64,
    ) -> Result<DispatchP, super::pack::PackError> {
        let payload = MlsEnvelopeP::GroupApplication {
            branch: self.branch,
            message: MlsApplicationEnvelopeP {
                version: MLS_ENVELOPE_VERSION,
                group_id: self.group,
                epoch: self.epoch,
                mls_message: self.message.clone(),
                sender_sig: recipient.envelope_sig,
            },
            proof: self.proof.clone(),
        }
        .ser()?;
        Ok(DispatchP {
            to: recipient.to,
            from: self.from,
            id: self.id,
            payload: payload.into(),
            sig: recipient.dispatch_sig,
            accepted_at_ms,
            wake: self.wake,
            ttl_ms: self.ttl_ms,
        })
    }
}
