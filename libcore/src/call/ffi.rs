//! The platform's call surface. Call events arrive on `CoreEvents::on_call`.

use common::types::bytes::fixed;

use crate::platform::CoreError;

/// Returns the 16-byte call id.
#[uniffi::export]
pub fn call_start(peer: Vec<u8>, video: bool) -> Result<Vec<u8>, CoreError> {
    let peer = fixed::<32>(&peer, "ipk")?;
    Ok(super::start(peer, video).map_err(anyhow_to_core)?.to_vec())
}

/// Refused unless `call` is the one ringing, so a stale Answer cannot pick up its replacement.
#[uniffi::export]
pub fn call_accept(call: Vec<u8>) -> Result<(), CoreError> {
    let call = fixed::<16>(&call, "call id")?;
    super::accept(call).map_err(anyhow_to_core)
}

/// Declines `call` if it is the one ringing; a stale id declines nothing.
#[uniffi::export]
pub fn call_reject(call: Vec<u8>) {
    if let Ok(call) = fixed::<16>(&call, "call id") {
        super::reject(call);
    }
}

/// Hangs up, cancels or refuses `call`, whatever it is doing; a stale id ends nothing.
#[uniffi::export]
pub fn call_hangup(call: Vec<u8>) {
    if let Ok(call) = fixed::<16>(&call, "call id") {
        super::hangup(call);
    }
}

#[uniffi::export]
pub fn call_set_muted(muted: bool) {
    super::set_muted(muted);
}

/// Only tells the peer to show our video or our avatar; the platform owns the camera.
#[uniffi::export]
pub fn call_set_camera(on: bool) {
    super::set_camera(on);
}

/// One encoded H.264 access unit in Annex-B format.
#[uniffi::export]
pub fn call_push_video(frame: Vec<u8>, keyframe: bool) {
    super::video_capture(frame, keyframe);
}

/// Restarts ICE at once when the default network moves.
#[uniffi::export]
pub fn call_network_changed() {
    super::network_changed();
}

#[uniffi::export]
pub fn contact_name(ipk: Vec<u8>) -> String {
    match fixed::<32>(&ipk, "ipk") {
        Ok(ipk) => crate::data::peer_name::resolve(&ipk),
        Err(_) => String::new(),
    }
}

#[uniffi::export]
pub fn call_current() -> Option<CallState> {
    super::current().map(Into::into)
}

/// One captured frame: 20 ms of 48 kHz mono PCM as 960 little-endian i16 samples.
#[uniffi::export]
pub fn call_push_audio(pcm: Vec<u8>) {
    super::audio_capture(&pcm);
}

/// The next `frames` 20 ms frames of little-endian playback PCM, silence outside a call.
#[uniffi::export]
pub fn call_pull_audio(frames: u32) -> Vec<u8> {
    super::audio_playback(frames as usize)
}

#[derive(uniffi::Record)]
pub struct CallState {
    pub call:         Vec<u8>,
    pub peer:         Vec<u8>,
    pub conversation: Vec<u8>,
    pub outgoing:     bool,
    pub video:        bool,
    pub phase:        CallPhase,
    pub muted:        bool,
    pub peer_muted:   bool,
    /// Milliseconds since the call connected, zero before it did.
    pub connected_ms: u64,
}

#[derive(uniffi::Enum)]
pub enum CallPhase {
    Offering,
    Ringing,
    Connecting,
    Connected,
    Reconnecting,
}

impl From<super::Snapshot> for CallState {
    fn from(s: super::Snapshot) -> Self {
        CallState {
            call:         s.id.to_vec(),
            peer:         s.peer.to_vec(),
            conversation: s.conversation.to_vec(),
            outgoing:     s.outgoing,
            video:        s.video,
            phase:        s.phase.into(),
            muted:        s.muted,
            peer_muted:   s.peer_muted,
            connected_ms: s.connected_ms,
        }
    }
}

impl From<super::Phase> for CallPhase {
    fn from(p: super::Phase) -> Self {
        match p {
            super::Phase::Offering => CallPhase::Offering,
            super::Phase::Ringing => CallPhase::Ringing,
            super::Phase::Connecting => CallPhase::Connecting,
            super::Phase::Connected => CallPhase::Connected,
            super::Phase::Reconnecting => CallPhase::Reconnecting,
        }
    }
}

fn anyhow_to_core(e: anyhow::Error) -> CoreError {
    CoreError::Refused { msg: format!("{e}") }
}
