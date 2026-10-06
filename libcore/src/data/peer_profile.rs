//! Displayed names, bios and contact cards from the encrypted profile store.
use crate::state::core;

#[derive(Clone, Debug)]
pub struct Profile {
    pub name: String,
    pub bio: String,
    pub card: Vec<u8>,
}
crate::db::from_row!(Profile { name, bio, card });
pub fn notify_changed() {
    if let Some(events) = core().events.get() {
        events.on_db_changed(vec![
            "peer_profiles".into(),
            "contacts".into(),
            "conversation_members".into(),
        ]);
    }
}
pub fn get(who: &[u8; 32]) -> Option<Profile> {
    core()
        .db
        .messages()
        .lock()
        .query_row(
            "SELECT name, bio, card FROM peer_profiles WHERE ipk = ?1",
            [who.as_slice()],
            Profile::from_row,
        )
        .ok()
}
