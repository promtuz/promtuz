pub mod data;
pub mod mls;
pub mod net;
pub mod transfer;

use std::sync::Arc;

use parking_lot::Mutex;
use tempfile::TempDir;

use crate::events::connection::ConnectionState;
use crate::platform::CallEvent;
use crate::platform::CoreError;
use crate::platform::CoreEvents;
use crate::platform::MessageEvent;
use crate::platform::Presence;
use crate::platform::SecureStore;
use crate::state::Core;

/// Until this drops, `state::core()` on this thread is `core`, over in-memory databases with
/// `events` as its sink. A current-thread test runs its spawned tasks here, so they see it too.
pub(crate) struct ScopedCore {
    pub core:   &'static Core,
    pub events: Arc<Events>,
    previous:   Option<&'static Core>,
    _files:     TempDir,
}

impl ScopedCore {
    pub fn new() -> Self {
        let (db, files) = data::stores();
        let core = Box::leak(Box::new(Core::new(db, tokio::runtime::Handle::current())));
        let events = Arc::new(Events::default());
        let _ = core.events.set(events.clone());
        let _ = core.secure_store.set(Arc::new(Plain));
        let previous = crate::state::SCOPED.replace(Some(core));
        Self { core, events, previous, _files: files }
    }
}

impl Drop for ScopedCore {
    fn drop(&mut self) {
        crate::state::SCOPED.set(self.previous);
    }
}

/// Every table change core announced, with the picture generation the app would read then.
#[derive(Default)]
pub(crate) struct Events(pub Mutex<Vec<(Vec<String>, u64)>>);

impl CoreEvents for Events {
    fn on_connection(&self, _: ConnectionState) {}
    fn on_message(&self, _: MessageEvent) {}
    fn on_activity(&self, _: Vec<u8>, _: Vec<u8>, _: u16) {}
    fn on_presence(&self, _: Vec<u8>, _: Presence) {}
    fn on_reaction(&self, _: Vec<u8>, _: Vec<u8>, _: Vec<u8>, _: String, _: bool) {}
    fn on_db_changed(&self, tables: Vec<String>) {
        self.0.lock().push((tables, crate::data::peer_avatar::generation()));
    }
    fn on_call(&self, _: CallEvent) {}
    fn on_call_video(&self, _: Vec<u8>, _: bool) {}
    fn on_call_video_keyframe(&self) {}
    fn on_call_video_bitrate(&self, _: u32) {}
}

struct Plain;

impl SecureStore for Plain {
    fn seal(&self, plaintext: Vec<u8>) -> Result<Vec<u8>, CoreError> {
        Ok(plaintext)
    }

    fn open(&self, ciphertext: Vec<u8>) -> Result<Vec<u8>, CoreError> {
        Ok(ciphertext)
    }
}
