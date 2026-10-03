//! Message requests: a stranger's first message opens a direct chat we have not agreed to. Until
//! it is accepted nothing of ours reaches them: no receipts, typing, profile, reactions or replies.
use anyhow::{Result, anyhow, ensure};
use common::types::bytes::fixed;

use crate::data::app_prefs;
use crate::data::contact::{Contact, PAIR_STATUS_REQUEST};
use crate::data::conversation::{Conversation, KIND_DIRECT};
use crate::platform::CoreError;

const MAX_PENDING: u32 = 100;
const SETTING: &str = "message_requests";

fn blocked_key(ipk: &[u8; 32]) -> String {
    format!("blocked:{}", hex::encode(ipk))
}

pub(crate) fn is_blocked(ipk: &[u8; 32]) -> bool {
    app_prefs::get(&blocked_key(ipk)).is_some()
}

pub(crate) fn is_request(ipk: &[u8; 32]) -> bool {
    Contact::status(ipk) == Some(PAIR_STATUS_REQUEST)
}

pub(crate) fn is_request_chat(conversation: &[u8; 16]) -> bool {
    Conversation::get(conversation).is_some_and(|c| c.kind == KIND_DIRECT)
        && Conversation::peer_of(conversation).is_some_and(|p| is_request(&p))
}

pub(crate) fn admits_stranger() -> bool {
    message_requests_enabled() && Contact::count_requests() < MAX_PENDING
}

/// Off means only contacts can message us; strangers' Welcomes are dropped.
#[uniffi::export]
pub fn message_requests_enabled() -> bool {
    app_prefs::get(SETTING).as_deref() != Some("off")
}

#[uniffi::export]
pub fn set_message_requests_enabled(enabled: bool) -> Result<(), CoreError> {
    Ok(app_prefs::set(SETTING, if enabled { "on" } else { "off" })?)
}

/// The pair ack confirms it on their side, then our profile and the held-back receipts follow.
#[uniffi::export]
pub fn accept_message_request(ipk: Vec<u8>) -> Result<(), CoreError> {
    let peer = fixed::<32>(&ipk, "ipk")?;
    if !Contact::accept_request(&peer)? {
        return Err(anyhow!("This request is no longer available").into());
    }
    crate::messaging::welcome::confirm_pair(peer);
    crate::data::receipts::schedule();
    Ok(())
}

/// Remove the request and its chat. The requester is not told.
#[uniffi::export(async_runtime = "tokio")]
pub async fn delete_message_request(ipk: Vec<u8>) -> Result<(), CoreError> {
    let peer = fixed::<32>(&ipk, "ipk")?;
    ensure_request(&peer)?;
    crate::api::messaging::forget_contact(ipk).await
}

/// Delete the request and drop anything they send us directly from now on.
#[uniffi::export(async_runtime = "tokio")]
pub async fn block_message_request(ipk: Vec<u8>) -> Result<(), CoreError> {
    let peer = fixed::<32>(&ipk, "ipk")?;
    ensure_request(&peer)?;
    app_prefs::set(&blocked_key(&peer), &crate::data::peer_name::resolve(&peer))?;
    crate::api::messaging::forget_contact(ipk).await
}

#[uniffi::export]
pub fn unblock(ipk: Vec<u8>) -> Result<(), CoreError> {
    Ok(app_prefs::remove(&blocked_key(&fixed::<32>(&ipk, "ipk")?))?)
}

#[derive(uniffi::Record)]
pub struct BlockedPerson {
    pub ipk: Vec<u8>,
    /// What they were called when blocked.
    pub name: String,
}

#[uniffi::export]
pub fn blocked_people() -> Vec<BlockedPerson> {
    app_prefs::with_prefix("blocked:")
        .into_iter()
        .filter_map(|(key, name)| {
            let ipk = hex::decode(key.strip_prefix("blocked:")?).ok().filter(|k| k.len() == 32)?;
            Some(BlockedPerson { ipk, name })
        })
        .collect()
}

fn ensure_request(peer: &[u8; 32]) -> Result<()> {
    ensure!(is_request(peer), "This request is no longer available");
    Ok(())
}
