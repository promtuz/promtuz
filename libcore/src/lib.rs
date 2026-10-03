pub mod api;
pub mod call;
pub mod data;
pub mod db;
pub mod delivery;
pub mod events;
pub mod media;
pub mod groups;
pub mod messaging;
pub mod mls;
pub mod p2p;
pub mod platform;
pub mod presence;
mod contact_card;
mod requests;
mod profile_sync;
pub mod push;
pub mod quic;
pub mod staging;
pub mod state;
pub mod stickers;
pub mod transfer;
pub mod utils;

#[cfg(test)]
mod test_support;

uniffi::setup_scaffolding!();
