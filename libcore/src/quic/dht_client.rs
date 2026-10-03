//! The DHT RPCs libcore issues through its home relay: KeyPackage publish and
//! fetch, Welcome delivery, and the Welcome queue drain. The production
//! implementation is [`super::relay_dht_client::RelayDhtClient`]; callers are
//! generic over [`DhtClient`] so an in-process stand-in needs no network.

use std::future::Future;

use common::proto::mls_wire::KeyPackageRecord;
use common::proto::mls_wire::WelcomeEntry;
use common::proto::mls_wire::WelcomeEnvelopeP;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DhtClientError {
    /// Fewer homes acknowledged than the write quorum needs.
    #[error("dht_client: quorum not met ({succeeded}/{wanted} homes)")]
    QuorumNotMet { succeeded: usize, wanted: usize },

    #[error("dht_client: target has no published KeyPackage")]
    NoStash,

    #[error("dht_client: transport: {0}")]
    Transport(String),

    /// The relay replied with an unexpected packet.
    #[error("dht_client: protocol mismatch: {0}")]
    Protocol(String),
}

pub type DhtClientResult<T> = std::result::Result<T, DhtClientError>;

pub trait DhtClient: Send + Sync + 'static {
    /// Publish a batch of KeyPackages, each carrying its `owner_sig`, to the
    /// homes of our own IPK.
    fn publish_keypackages(
        &self, records: &[KeyPackageRecord],
    ) -> impl Future<Output = DhtClientResult<()>> + Send;

    /// Fetch one of `target_ipk`'s published KeyPackages.
    fn fetch_keypackage_for(
        &self, target_ipk: &[u8; 32],
    ) -> impl Future<Output = DhtClientResult<KeyPackageRecord>> + Send;

    /// Push a Welcome to its recipient over the dispatch path: live when they
    /// are online, queued otherwise.
    fn deliver_welcome(
        &self, envelope: &WelcomeEnvelopeP,
    ) -> impl Future<Output = DhtClientResult<()>> + Send;

    /// Drain the Welcomes queued for us at our homes.
    fn fetch_welcomes(&self) -> impl Future<Output = DhtClientResult<Vec<WelcomeEntry>>> + Send;

    /// Let the homes drop the named Welcomes.
    fn ack_welcomes(
        &self, welcome_ids: &[[u8; 8]],
    ) -> impl Future<Output = DhtClientResult<()>> + Send;
}
