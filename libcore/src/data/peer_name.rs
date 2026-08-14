//! Names people assert about themselves, learned inside a shared group.
//!
//! A group makes it ordinary to share a room with someone you never paired
//! with, and nothing else can tell you who they are: no one introduced you, and
//! putting names beside KeyPackages would turn the DHT into a directory that
//! resolves any IPK to a person. So each member says their own name to the
//! groups they are in, and only those members hear it.
//!
//! This is what they *claim*. [`Contact`](crate::data::contact::Contact) holds
//! what the local user *chose*, and that always wins — see [`resolve`].

use anyhow::Result;

use crate::db::messages::MESSAGES_DB;
use crate::utils::systime;

/// Longer than this and a name stops being a name and starts being a payload.
const MAX_NAME: usize = 32;

/// Record what `who` calls themselves. Last assertion wins — a rename is just
/// the same person saying something new.
pub fn put(who: &[u8; 32], name: &str) -> Result<()> {
    let name: String = name.trim().chars().take(MAX_NAME).collect();
    if name.is_empty() {
        return Ok(());
    }
    let conn = MESSAGES_DB.lock();
    conn.execute(
        "INSERT INTO peer_names (ipk, name, updated_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(ipk) DO UPDATE SET name = excluded.name, updated_at = excluded.updated_at",
        (who.as_slice(), &name, systime().as_secs()),
    )?;
    Ok(())
}

pub fn get(who: &[u8; 32]) -> Option<String> {
    let conn = MESSAGES_DB.lock();
    conn.query_row("SELECT name FROM peer_names WHERE ipk = ?1", [who.as_slice()], |r| r.get(0))
        .ok()
}

/// What to call `who` on screen, in order of who is entitled to name them:
/// the local user's own address book, then the person's own assertion, then
/// the head of their key — which names nobody but at least identifies.
///
/// Resolved here rather than in each client so every surface agrees, and so
/// the precedence is stated once instead of re-derived per screen.
pub fn resolve(who: &[u8; 32]) -> String {
    if let Some(c) = crate::data::contact::Contact::get(who) {
        if !c.inner.name.is_empty() {
            return c.inner.name.clone();
        }
    }
    get(who).unwrap_or_else(|| hex::encode(&who[..4]))
}

/// True when the name came from the person rather than the address book — the
/// client marks those, the way a messenger marks a name it cannot vouch for.
pub fn is_self_asserted(who: &[u8; 32]) -> bool {
    crate::data::contact::Contact::get(who).is_none_or(|c| c.inner.name.is_empty())
        && get(who).is_some()
}
