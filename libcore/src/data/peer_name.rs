//! Names people assert about themselves inside the groups they share with us.

use anyhow::Result;
use common::utils::now_secs;

use crate::state::core;

const MAX_NAME: usize = 32;

pub fn put(who: &[u8; 32], name: &str) -> Result<()> {
    let name: String = name.trim().chars().take(MAX_NAME).collect();
    if name.is_empty() {
        return Ok(());
    }
    let conn = core().db.messages().lock();
    conn.execute(
        "INSERT INTO peer_names (ipk, name, updated_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(ipk) DO UPDATE SET name = excluded.name, updated_at = excluded.updated_at",
        (who.as_slice(), &name, now_secs()),
    )?;
    Ok(())
}

pub fn get(who: &[u8; 32]) -> Option<String> {
    let conn = core().db.messages().lock();
    conn.query_row("SELECT name FROM peer_names WHERE ipk = ?1", [who.as_slice()], |r| r.get(0))
        .ok()
}

/// The name to show, and whether it is only their own claim. Precedence: local nickname, then
/// [`named`], then the key's head.
pub fn resolve_claimed(who: &[u8; 32]) -> (String, bool) {
    let nickname = crate::data::app_prefs::get(&format!("nickname:{}", hex::encode(who)));
    if let Some(nickname) = nickname.filter(|s| !s.is_empty()) {
        return (nickname, false);
    }
    named(who).unwrap_or_else(|| (hex::encode(&who[..4]), false))
}

/// Their profile name, the address book, then a group-asserted name, and whether it is only their
/// own claim.
pub fn named(who: &[u8; 32]) -> Option<(String, bool)> {
    if let Some(profile) = crate::data::peer_profile::get(who) {
        return Some((profile.name, true));
    }
    if let Some(c) = crate::data::contact::Contact::get(who).filter(|c| !c.inner.name.is_empty()) {
        return Some((c.inner.name.clone(), false));
    }
    get(who).map(|name| (name, true))
}

pub fn resolve(who: &[u8; 32]) -> String {
    resolve_claimed(who).0
}
