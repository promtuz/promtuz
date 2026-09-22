//! The call surface the platform calls. Control is small and synchronous;
//! the audio device pushes and pulls PCM per 20 ms tick. Everything else
//! about a call arrives on `CoreEvents::on_call`.

use crate::platform::CoreError;

/// Start a call to `peer` (a 32-byte IPK). Returns the 16-byte call id; the
/// ringing screen and the rest come as events.
#[uniffi::export]
pub fn call_start(peer: Vec<u8>) -> Result<Vec<u8>, CoreError> {
    let peer = crate::api::messaging::to_ipk32(&peer)?;
    Ok(super::start(peer).map_err(anyhow_to_core)?.to_vec())
}

/// Pick up the ringing call.
#[uniffi::export]
pub fn call_accept() -> Result<(), CoreError> {
    super::accept().map_err(anyhow_to_core)
}

/// Refuse the ringing call.
#[uniffi::export]
pub fn call_reject() {
    super::reject();
}

/// Hang up, cancel, or refuse, whatever the current call is doing.
#[uniffi::export]
pub fn call_hangup() {
    super::hangup();
}

/// Mute or unmute our microphone.
#[uniffi::export]
pub fn call_set_muted(muted: bool) {
    super::set_muted(muted);
}

/// Tell the engine the default network moved, so it restarts ICE at once.
#[uniffi::export]
pub fn call_network_changed() {
    super::network_changed();
}

/// The current call for the UI, or `None` when there is none.
#[uniffi::export]
pub fn call_current() -> Option<CallState> {
    super::current().map(Into::into)
}

/// One captured frame: 20 ms of 48 kHz mono PCM, 960 little-endian i16
/// samples (1920 bytes). Called from the audio capture thread.
#[uniffi::export]
pub fn call_push_audio(pcm: Vec<u8>) {
    super::audio_capture(&pcm);
}

/// The next `frames` of playback as little-endian PCM, silence outside a
/// call. Called from the audio playback thread; one frame is 20 ms.
#[uniffi::export]
pub fn call_pull_audio(frames: u32) -> Vec<u8> {
    super::audio_playback(frames as usize)
}

/// A call as the UI reads it.
#[derive(uniffi::Record)]
pub struct CallState {
    pub call:         Vec<u8>,
    pub peer:         Vec<u8>,
    pub conversation: Vec<u8>,
    pub outgoing:     bool,
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
