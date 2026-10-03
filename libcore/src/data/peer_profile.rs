//! Owner-asserted names and bios, revisioned across all shared chats.
use crate::state::core;
use anyhow::Result;
use rusqlite::Connection;

#[derive(Clone, Debug)]
pub struct ProfileUpdate {
    pub revision: u64,
    pub name: String,
    pub bio: String,
    pub card: Vec<u8>,
}
crate::db::from_row!(ProfileUpdate { revision, name, bio, card });
impl ProfileUpdate {
    pub fn into_payload(self) -> common::proto::mls_wire::AppPayload {
        common::proto::mls_wire::AppPayload::ProfileDetails {
            revision: self.revision,
            name: self.name,
            bio: self.bio,
            card: self.card,
        }
    }
}
pub fn notify_changed() {
    if let Some(events) = core().events.get() {
        events.on_db_changed(vec![
            "peer_profiles".into(),
            "contacts".into(),
            "conversation_members".into(),
        ]);
    }
}
pub fn apply_tx(conn: &Connection, who: &[u8; 32], update: &ProfileUpdate) -> Result<bool> {
    anyhow::ensure!(
        !update.name.trim().is_empty()
            && update.name.chars().count() <= 32
            && update.bio.chars().count() <= 160,
        "invalid profile"
    );
    if !update.card.is_empty() {
        let card = crate::contact_card::verify_card(&update.card)?;
        anyhow::ensure!(card.ipk == *who && card.name == update.name, "profile card mismatch");
    }
    let revision = i64::try_from(update.revision)?;
    Ok(conn.execute("INSERT INTO peer_profiles(ipk, name, bio, revision, card) VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(ipk) DO UPDATE SET name=excluded.name, bio=excluded.bio, revision=excluded.revision, card=excluded.card
        WHERE excluded.revision > peer_profiles.revision",
        (who.as_slice(), &update.name, &update.bio, revision, &update.card))? != 0)
}
pub fn get(who: &[u8; 32]) -> Option<ProfileUpdate> {
    core().db.messages()
        .lock()
        .query_row(
            "SELECT revision, name, bio, card FROM peer_profiles WHERE ipk = ?1",
            [who.as_slice()],
            ProfileUpdate::from_row,
        )
        .ok()
}
