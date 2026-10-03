//! The originating side of the KeyPackage RPCs. A fetch consumes a one-shot KeyPackage at every
//! home it reaches, so it tries the homes one at a time and stops at the first `Found`.

use std::sync::Arc;

use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::mls_wire::KeyPackageFetchOutcome;
use common::proto::mls_wire::KeyPackageFetchReq;
use common::proto::mls_wire::KeyPackageFetchResp;
use common::proto::mls_wire::KeyPackagePublishOutcome;
use common::proto::mls_wire::KeyPackagePublishReq;
use common::proto::mls_wire::KeyPackagePublishResp;
use common::proto::mls_wire::KeyPackageRecord;
use common::proto::mls_wire::key_package_stash_prefix;
use common::quic::id::NodeId;

use crate::dht::Dht;
use crate::dht::config::FORWARD_TIMEOUT_MS;
use crate::dht::config::write_quorum;
use crate::dht::handler::handle_dht_request;
use crate::dht::rpc::ask_homes;
use crate::dht::rpc::rpc;

pub(crate) struct KpPublishQuorum {
    pub homes_succeeded: u8,
    pub quorum_met:      bool,
}

pub(crate) struct KpFetchResult {
    pub unavailable: bool,
    pub record: Option<KeyPackageRecord>,
    pub remaining: u32,
    pub static_hash: [u8; 32],
}

pub(crate) async fn originate_publish(
    dht: &Arc<Dht>, ipk: [u8; 32], records: Vec<KeyPackageRecord>, timestamp: u64, sig: [u8; 64],
) -> KpPublishQuorum {
    let req = DhtRequest::KeyPackagePublish(KeyPackagePublishReq {
        ipk: ipk.into(),
        records,
        timestamp,
        sig: sig.into(),
    });
    let target = NodeId::from_bytes(key_package_stash_prefix(&ipk));
    let (asked, replies) = ask_homes(dht, &target, req).await;
    let succeeded = replies
        .iter()
        .filter(|(_, reply)| {
            matches!(
                reply,
                DhtResponse::KeyPackagePublish(KeyPackagePublishResp {
                    outcome: KeyPackagePublishOutcome::Stored,
                })
            )
        })
        .count();
    KpPublishQuorum {
        homes_succeeded: succeeded as u8,
        quorum_met:      succeeded >= write_quorum(asked.len()),
    }
}

pub(crate) async fn originate_fetch(
    dht: &Arc<Dht>, target_ipk: [u8; 32], now_ms: u64,
) -> KpFetchResult {
    let target = NodeId::from_bytes(key_package_stash_prefix(&target_ipk));
    let (peers, is_home) = crate::dht::routing::homes(dht, &target);
    let req = DhtRequest::KeyPackageFetch(KeyPackageFetchReq {
        target_ipk: target_ipk.into(),
        requester_relay_id: dht.node_id,
        timestamp: now_ms,
    });

    let mut unavailable = false;
    // `None` is this relay, asked first through its own dispatcher.
    for home in is_home.then_some(None).into_iter().chain(peers.iter().map(Some)) {
        let reply = match home {
            None => Some(handle_dht_request(dht, req.clone(), dht.node_id).await),
            Some(peer) => rpc(dht, peer, &req, FORWARD_TIMEOUT_MS).await,
        };
        match reply {
            Some(DhtResponse::KeyPackageFetch(KeyPackageFetchResp {
                outcome: KeyPackageFetchOutcome::Found(f),
            })) => {
                return KpFetchResult {
                    unavailable: false,
                    record: Some(f.record),
                    remaining: f.remaining,
                    static_hash: f.static_hash.0,
                };
            },
            Some(DhtResponse::KeyPackageFetch(KeyPackageFetchResp {
                outcome: KeyPackageFetchOutcome::NoStash | KeyPackageFetchOutcome::NotOwner,
            })) => {},
            _ => unavailable = true,
        }
    }
    KpFetchResult { unavailable, record: None, remaining: 0, static_hash: [0; 32] }
}
